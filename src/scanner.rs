use anyhow::{Context, Result, bail};
use std::fs::{self, Metadata};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::model::{
    Entry, EntryKind, SNAPSHOT_FORMAT_VERSION, ScanError, Snapshot, absolute_path, relative_key,
    root_key,
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
    progress: Option<Box<dyn FnMut(ScanProgress)>>,
}

pub fn scan(root: &Path) -> Result<Snapshot> {
    scan_internal(root, None)
}

pub fn scan_with_progress(
    root: &Path,
    progress: impl FnMut(ScanProgress) + 'static,
) -> Result<Snapshot> {
    scan_internal(root, Some(Box::new(progress)))
}

fn scan_internal(root: &Path, progress: Option<Box<dyn FnMut(ScanProgress)>>) -> Result<Snapshot> {
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
        state.entries.push(make_entry(
            &root,
            Path::new(""),
            EntryKind::Directory,
            0,
            &root_metadata,
            true,
        ));
        0
    } else {
        state.directory_count += 1;
        let total = scan_directory(&root, Path::new(""), &mut state);
        state.entries.push(make_entry(
            &root,
            Path::new(""),
            EntryKind::Directory,
            total,
            &root_metadata,
            false,
        ));
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

fn scan_directory(path: &Path, relative: &Path, state: &mut ScanState) -> u64 {
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
        let child_relative = relative.join(child.file_name());
        let metadata = match fs::symlink_metadata(&child_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                record_error(state, &child_path, error);
                state.report_if_due(&child_path);
                continue;
            }
        };

        if is_reparse_point(&metadata) {
            let target = resolve_link_target(&child_path);
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
            state.entries.push(make_entry_with_target(
                &child_relative,
                kind,
                0,
                &metadata,
                target,
            ));
            state.report_if_due(&child_path);
            continue;
        }

        if metadata.is_dir() {
            state.directory_count += 1;
            let child_size = scan_directory(&child_path, &child_relative, state);
            state.entries.push(make_entry(
                &child_path,
                &child_relative,
                EntryKind::Directory,
                child_size,
                &metadata,
                false,
            ));
            total_bytes = total_bytes.saturating_add(child_size);
        } else {
            let size = metadata.len();
            state.file_count += 1;
            state.entries.push(make_entry(
                &child_path,
                &child_relative,
                EntryKind::File,
                size,
                &metadata,
                false,
            ));
            total_bytes = total_bytes.saturating_add(size);
            state.total_bytes = state.total_bytes.saturating_add(size);
        }
        state.report_if_due(&child_path);
    }
    total_bytes
}

impl ScanState {
    fn report_if_due(&mut self, path: &Path) {
        self.progress_events = self.progress_events.saturating_add(1);
        let now = Instant::now();
        let due_to_count = self.progress_events.is_multiple_of(2048);
        let due_to_time = self
            .last_progress_at
            .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(250));
        if self.progress.is_some() && (due_to_count || due_to_time) {
            self.report_at(path, now);
        }
    }

    fn report(&mut self, path: &Path) {
        self.report_at(path, Instant::now());
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

fn make_entry(
    path: &Path,
    relative: &Path,
    kind: EntryKind,
    size_bytes: u64,
    metadata: &Metadata,
    is_reparse_point: bool,
) -> Entry {
    Entry {
        relative_path: relative_key(relative),
        kind,
        size_bytes,
        modified_unix_ms: modified_unix_ms(metadata),
        is_reparse_point,
        link_target: if is_reparse_point {
            resolve_link_target(path)
        } else {
            None
        },
        followed: false,
    }
}

fn make_entry_with_target(
    relative: &Path,
    kind: EntryKind,
    size_bytes: u64,
    metadata: &Metadata,
    link_target: Option<String>,
) -> Entry {
    Entry {
        relative_path: relative_key(relative),
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
    use super::scan;
    use crate::model::EntryKind;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

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
