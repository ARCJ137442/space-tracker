use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::fs::{self, Metadata};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::model::{
    Entry, EntryKind, SNAPSHOT_FORMAT_VERSION, ScanError, Snapshot, absolute_path, root_key,
};

#[derive(Clone, Debug)]
pub struct ScanProgress {
    pub current_path: String,
    pub elapsed_ms: u64,
    pub total_bytes: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub reparse_point_count: u64,
    pub error_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ScanEstimate {
    pub total_entries: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub total_bytes: u64,
    pub error_count: usize,
}

struct ScanState {
    entries: Vec<Entry>,
    errors: Vec<ScanError>,
    file_count: u64,
    directory_count: u64,
    reparse_point_count: u64,
    total_bytes: u64,
    progress_events: u64,
    last_progress_at: Option<Instant>,
    started_at: Instant,
    progress: Option<Box<dyn FnMut(ScanProgress) + Send>>,
}

pub fn scan(root: &Path) -> Result<Snapshot> {
    scan_internal(root, None)
}

/// Performs a lightweight, non-following walk used by the interactive UI to
/// establish a progress denominator. The normal CLI scan does not call this,
/// so Agent-oriented scans do not pay for a second traversal.
pub fn estimate(root: &Path) -> Result<ScanEstimate> {
    let root = absolute_path(root).context("failed to resolve scan root")?;
    let root_metadata = fs::symlink_metadata(&root)
        .with_context(|| format!("cannot inspect scan root: {}", root.display()))?;
    let root_is_reparse = is_reparse_point(&root_metadata);
    if !root_metadata.is_dir() && !root_is_reparse {
        bail!("scan root is not a directory: {}", root.display());
    }

    let mut estimate = ScanEstimate {
        total_entries: 1,
        file_count: 0,
        directory_count: 1,
        total_bytes: 0,
        error_count: 0,
    };
    if !root_is_reparse {
        estimate_directory(&root, &mut estimate);
    }
    Ok(estimate)
}

pub fn scan_with_progress(
    root: &Path,
    progress: impl FnMut(ScanProgress) + Send + 'static,
) -> Result<Snapshot> {
    scan_internal(root, Some(Box::new(progress)))
}

fn scan_internal(
    root: &Path,
    progress: Option<Box<dyn FnMut(ScanProgress) + Send>>,
) -> Result<Snapshot> {
    let root = absolute_path(root).context("failed to resolve scan root")?;
    let root_metadata = fs::symlink_metadata(&root)
        .with_context(|| format!("cannot inspect scan root: {}", root.display()))?;
    let root_is_reparse = is_reparse_point(&root_metadata);
    if !root_metadata.is_dir() && !root_is_reparse {
        bail!("scan root is not a directory: {}", root.display());
    }

    let started = Instant::now();
    let captured_at_unix_ms = unix_millis();
    let mut state = ScanState {
        entries: Vec::new(),
        errors: Vec::new(),
        file_count: 0,
        directory_count: 0,
        reparse_point_count: 0,
        total_bytes: 0,
        progress_events: 0,
        last_progress_at: None,
        started_at: started,
        progress,
    };
    let total_bytes = if root_is_reparse {
        state.reparse_point_count += 1;
        state.directory_count += 1;
        state.entries.push(make_entry_with_target(
            "",
            EntryKind::Directory,
            0,
            &root_metadata,
            resolve_link_target(&root),
        ));
        0
    } else {
        state.directory_count += 1;
        let total = if state.progress.is_some() {
            scan_directory(&root, "", &mut state)
        } else {
            scan_directory_parallel(&root, "", &mut state)
        };
        state
            .entries
            .push(make_entry("", EntryKind::Directory, total, &root_metadata));
        total
    };
    state.total_bytes = total_bytes;
    state.report(&root);

    Ok(Snapshot {
        format_version: SNAPSHOT_FORMAT_VERSION,
        snapshot_id: String::new(),
        root: path_string(&root),
        root_key: root_key(&root),
        captured_at_unix_ms,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        total_bytes,
        file_count: state.file_count,
        directory_count: state.directory_count,
        reparse_point_count: state.reparse_point_count,
        errors: state.errors,
        entries: state.entries,
    })
}

fn scan_directory(path: &Path, relative: &str, state: &mut ScanState) -> u64 {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            record_error(state, path, error);
            return 0;
        }
    };

    let mut total_bytes: u64 = 0;
    for child in entries {
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                record_error(state, path, error);
                continue;
            }
        };
        let child_path = child.path();
        let child_relative = append_relative(relative, &child.file_name());
        total_bytes = total_bytes.saturating_add(scan_child(&child_path, &child_relative, state));
    }
    total_bytes
}

fn scan_directory_parallel(path: &Path, relative: &str, state: &mut ScanState) -> u64 {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            record_error(state, path, error);
            return 0;
        }
    };
    let mut jobs = Vec::new();
    for child in entries {
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                record_error(state, path, error);
                continue;
            }
        };
        let child_path = child.path();
        let child_relative = append_relative(relative, &child.file_name());
        jobs.push((child_path, child_relative));
    }
    if jobs.len() < 2 {
        return jobs
            .into_iter()
            .map(|(child_path, child_relative)| scan_child(&child_path, &child_relative, state))
            .sum();
    }

    let results = jobs
        .into_par_iter()
        .map(|(child_path, child_relative)| {
            let mut child_state = empty_scan_state();
            let total_bytes = scan_child(&child_path, &child_relative, &mut child_state);
            (child_state, total_bytes)
        })
        .collect::<Vec<_>>();
    let mut total_bytes: u64 = 0;
    for (child_state, child_total) in results {
        merge_scan_state(state, child_state);
        total_bytes = total_bytes.saturating_add(child_total);
    }
    total_bytes
}

fn scan_child(path: &Path, relative: &str, state: &mut ScanState) -> u64 {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            record_error(state, path, error);
            state.report_if_due(path);
            return 0;
        }
    };

    if is_reparse_point(&metadata) {
        let target = resolve_link_target(path);
        let kind = if is_directory_link(&metadata, target.as_deref()) {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        state.reparse_point_count += 1;
        match kind {
            EntryKind::Directory => state.directory_count += 1,
            EntryKind::File => state.file_count += 1,
        }
        state
            .entries
            .push(make_entry_with_target(relative, kind, 0, &metadata, target));
        state.report_if_due(path);
        return 0;
    }

    let size = if metadata.is_dir() {
        state.directory_count += 1;
        let child_size = scan_directory(path, relative, state);
        state.entries.push(make_entry(
            relative,
            EntryKind::Directory,
            child_size,
            &metadata,
        ));
        child_size
    } else {
        let size = metadata.len();
        state.file_count += 1;
        state
            .entries
            .push(make_entry(relative, EntryKind::File, size, &metadata));
        state.total_bytes = state.total_bytes.saturating_add(size);
        size
    };
    state.report_if_due(path);
    size
}

fn empty_scan_state() -> ScanState {
    ScanState {
        entries: Vec::new(),
        errors: Vec::new(),
        file_count: 0,
        directory_count: 0,
        reparse_point_count: 0,
        total_bytes: 0,
        progress_events: 0,
        last_progress_at: None,
        started_at: Instant::now(),
        progress: None,
    }
}

fn merge_scan_state(target: &mut ScanState, mut source: ScanState) {
    target.entries.append(&mut source.entries);
    target.errors.append(&mut source.errors);
    target.file_count = target.file_count.saturating_add(source.file_count);
    target.directory_count = target
        .directory_count
        .saturating_add(source.directory_count);
    target.reparse_point_count = target
        .reparse_point_count
        .saturating_add(source.reparse_point_count);
    target.total_bytes = target.total_bytes.saturating_add(source.total_bytes);
}

fn estimate_directory(path: &Path, estimate: &mut ScanEstimate) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            estimate.error_count = estimate.error_count.saturating_add(1);
            return;
        }
    };

    for child in entries {
        let child = match child {
            Ok(child) => child,
            Err(_) => {
                estimate.error_count = estimate.error_count.saturating_add(1);
                continue;
            }
        };
        let child_path = child.path();
        let metadata = match fs::symlink_metadata(&child_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                estimate.error_count = estimate.error_count.saturating_add(1);
                continue;
            }
        };

        estimate.total_entries = estimate.total_entries.saturating_add(1);
        if is_reparse_point(&metadata) {
            let target = resolve_link_target(&child_path);
            if is_directory_link(&metadata, target.as_deref()) {
                estimate.directory_count = estimate.directory_count.saturating_add(1);
            } else {
                estimate.file_count = estimate.file_count.saturating_add(1);
            }
            continue;
        }
        if metadata.is_dir() {
            estimate.directory_count = estimate.directory_count.saturating_add(1);
            estimate_directory(&child_path, estimate);
        } else {
            estimate.file_count = estimate.file_count.saturating_add(1);
            estimate.total_bytes = estimate.total_bytes.saturating_add(metadata.len());
        }
    }
}

impl ScanState {
    fn report_if_due(&mut self, path: &Path) {
        if self.progress.is_none() {
            return;
        }
        self.progress_events = self.progress_events.saturating_add(1);
        let due_to_count = self.progress_events.is_multiple_of(2048);
        if !due_to_count {
            let now = Instant::now();
            if self
                .last_progress_at
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(250))
            {
                return;
            }
            self.report_at(path, now);
        } else {
            self.report_at(path, Instant::now());
        }
    }

    fn report(&mut self, path: &Path) {
        if self.progress.is_some() {
            self.report_at(path, Instant::now());
        }
    }

    fn report_at(&mut self, path: &Path, now: Instant) {
        self.last_progress_at = Some(now);
        let progress = ScanProgress {
            current_path: path_string(path),
            elapsed_ms: now
                .duration_since(self.started_at)
                .as_millis()
                .min(u64::MAX as u128) as u64,
            total_bytes: self.total_bytes,
            file_count: self.file_count,
            directory_count: self.directory_count,
            reparse_point_count: self.reparse_point_count,
            error_count: self.errors.len(),
        };
        if let Some(callback) = self.progress.as_mut() {
            callback(progress);
        }
    }
}

fn make_entry(relative: &str, kind: EntryKind, size_bytes: u64, metadata: &Metadata) -> Entry {
    Entry {
        relative_path: relative.to_string(),
        kind,
        size_bytes,
        modified_unix_ms: modified_unix_ms(metadata),
        is_reparse_point: false,
        link_target: None,
        followed: false,
    }
}

fn make_entry_with_target(
    relative: &str,
    kind: EntryKind,
    size_bytes: u64,
    metadata: &Metadata,
    link_target: Option<String>,
) -> Entry {
    Entry {
        relative_path: relative.to_string(),
        kind,
        size_bytes,
        modified_unix_ms: modified_unix_ms(metadata),
        is_reparse_point: true,
        link_target,
        followed: false,
    }
}

fn is_directory_link(metadata: &Metadata, target: Option<&str>) -> bool {
    if metadata.is_dir() {
        return true;
    }
    target
        .map(Path::new)
        .and_then(|path| fs::metadata(path).ok())
        .is_some_and(|target_metadata| target_metadata.is_dir())
}

fn resolve_link_target(path: &Path) -> Option<String> {
    fs::canonicalize(path)
        .ok()
        .map(|target| path_string(&target))
}

fn modified_unix_ms(metadata: &Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn record_error(state: &mut ScanState, path: &Path, error: impl std::fmt::Display) {
    state.errors.push(ScanError {
        path: path_string(path),
        message: error.to_string(),
    });
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

fn append_relative(parent: &str, name: &std::ffi::OsStr) -> String {
    let name = name.to_string_lossy();
    let mut relative = String::with_capacity(parent.len() + name.len() + 1);
    if !parent.is_empty() {
        relative.push_str(parent);
        relative.push('/');
    }
    for character in name.chars() {
        relative.push(if character == '\\' { '/' } else { character });
    }
    relative
}

fn is_reparse_point(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::{estimate, scan, scan_with_progress};
    use crate::model::EntryKind;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn parallel_and_progress_scans_match() {
        let root = std::env::temp_dir().join(format!(
            "space-tracker-parallel-scan-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("nested")).expect("create nested fixture");
        fs::write(root.join("one.txt"), b"one").expect("write first file");
        fs::write(root.join("nested").join("two.txt"), b"two").expect("write second file");

        let mut parallel = scan(&root).expect("parallel scan");
        let mut with_progress = scan_with_progress(&root, |_| {}).expect("progress scan");
        parallel.entries.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });
        with_progress.entries.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });

        assert_eq!(parallel.total_bytes, with_progress.total_bytes);
        assert_eq!(parallel.file_count, with_progress.file_count);
        assert_eq!(parallel.directory_count, with_progress.directory_count);
        assert_eq!(parallel.entries, with_progress.entries);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(windows)]
    #[test]
    fn junction_is_recorded_without_counting_target_twice() {
        let root = test_root();
        let target = root.join("target");
        let link = root.join("alias");
        fs::create_dir_all(&target).expect("create target");
        fs::write(target.join("payload.bin"), b"payload").expect("write payload");

        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .expect("run mklink");
        if !status.success() {
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let estimate = estimate(&root).expect("estimate fixture");
        assert_eq!(estimate.total_entries, 4);
        assert_eq!(estimate.file_count, 1);
        assert_eq!(estimate.directory_count, 3);
        assert_eq!(estimate.total_bytes, 7);

        let snapshot = scan(&root).expect("scan fixture");
        assert_eq!(snapshot.total_bytes, 7);
        assert_eq!(snapshot.reparse_point_count, 1);
        let alias = snapshot
            .entries
            .iter()
            .find(|entry| entry.relative_path == "alias")
            .expect("alias entry");
        assert_eq!(alias.kind, EntryKind::Directory);
        assert!(alias.is_reparse_point);
        assert_eq!(alias.size_bytes, 0);
        assert!(alias.link_target.is_some());

        fs::remove_dir_all(&root).expect("remove fixture");
    }

    #[cfg(windows)]
    fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("space-tracker-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }
}
