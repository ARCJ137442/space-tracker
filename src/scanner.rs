use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::fs::{self, Metadata};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::fsinfo::{self, FileFacts};
use crate::model::{
    Entry, EntryKind, SNAPSHOT_FORMAT_VERSION, ScanError, Snapshot, absolute_path, root_key,
};

const PARALLEL_MIN_JOBS: usize = 8;
const DEFAULT_MAX_SCAN_THREADS: usize = 8;

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
    let root_facts = fsinfo::read(&root)
        .with_context(|| format!("cannot inspect scan root: {}", root.display()))?;
    let root_is_reparse = root_facts.is_reparse_point;
    if !root_facts.is_dir && !root_is_reparse {
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
        let thread_pool = build_scan_thread_pool()?;
        thread_pool.install(|| estimate_directory_parallel(&root, &mut estimate));
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
    let root_facts = fsinfo::read(&root)
        .with_context(|| format!("cannot inspect scan root: {}", root.display()))?;
    let root_is_reparse = root_facts.is_reparse_point;
    if !root_facts.is_dir && !root_is_reparse {
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
            &root_facts,
            resolve_link_target(&root),
        ));
        0
    } else {
        state.directory_count += 1;
        let total = if state.progress.is_some() {
            scan_directory(&root, "", &mut state)
        } else {
            let thread_pool = build_scan_thread_pool()?;
            thread_pool.install(|| scan_directory_parallel(&root, "", &mut state))
        };
        state
            .entries
            .push(make_entry("", EntryKind::Directory, total, &root_facts));
        total
    };
    state.total_bytes = total_bytes;
    state.report(&root);

    // The tree is walked in parallel and the order of the walk is not fixed,
    // so the numbers that must not depend on it are settled here, once, over
    // the finished tree.
    let totals = crate::accounting::apply(&mut state.entries);

    Ok(Snapshot {
        format_version: SNAPSHOT_FORMAT_VERSION,
        snapshot_id: String::new(),
        root: path_string(&root),
        root_key: root_key(&root),
        captured_at_unix_ms,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        total_bytes,
        total_allocated_bytes: totals.allocated_bytes,
        total_reclaimable_bytes: totals.reclaimable_bytes,
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
    if jobs.len() < PARALLEL_MIN_JOBS {
        return jobs
            .into_iter()
            .map(|(child_path, child_relative)| {
                scan_child_parallel(&child_path, &child_relative, state)
            })
            .sum();
    }

    // Keep each Rayon worker's mutable scan state local, then merge the
    // independent results. This avoids a shared lock on every filesystem
    // entry and also keeps the number of result buffers bounded by Rayon
    // partitions instead of by the number of files in this directory.
    let merged = jobs
        .into_par_iter()
        .fold(
            empty_scan_state,
            |mut local_state, (child_path, child_relative)| {
                scan_child_parallel(&child_path, &child_relative, &mut local_state);
                local_state
            },
        )
        .reduce(empty_scan_state, |mut left, right| {
            merge_scan_state(&mut left, right);
            left
        });
    let total_bytes = merged.total_bytes;
    merge_scan_state(state, merged);
    total_bytes
}

fn scan_child_parallel(path: &Path, relative: &str, state: &mut ScanState) -> u64 {
    let facts = match fsinfo::read(path) {
        Ok(facts) => facts,
        Err(error) => {
            record_error(state, path, error);
            return listed_child(path, relative, state);
        }
    };

    if facts.is_reparse_point {
        let target = resolve_link_target(path);
        let kind = if is_directory_link(facts.is_dir, target.as_deref()) {
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
            .push(make_entry_with_target(relative, kind, 0, &facts, target));
        return 0;
    }

    if facts.is_dir {
        state.directory_count += 1;
        let child_size = scan_directory_parallel(path, relative, state);
        state.entries.push(make_entry(
            relative,
            EntryKind::Directory,
            child_size,
            &facts,
        ));
        child_size
    } else {
        let size = facts.apparent_size;
        state.file_count += 1;
        state
            .entries
            .push(make_entry(relative, EntryKind::File, size, &facts));
        state.total_bytes = state.total_bytes.saturating_add(size);
        size
    }
}

fn scan_thread_count() -> usize {
    std::env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|parallelism| parallelism.get())
                .unwrap_or(1)
                .clamp(1, DEFAULT_MAX_SCAN_THREADS)
        })
}

fn build_scan_thread_pool() -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(scan_thread_count())
        .build()
        .context("failed to create scanner thread pool")
}

fn scan_child(path: &Path, relative: &str, state: &mut ScanState) -> u64 {
    let facts = match fsinfo::read(path) {
        Ok(facts) => facts,
        Err(error) => {
            record_error(state, path, error);
            let size = listed_child(path, relative, state);
            state.report_if_due(path);
            return size;
        }
    };

    if facts.is_reparse_point {
        let target = resolve_link_target(path);
        let kind = if is_directory_link(facts.is_dir, target.as_deref()) {
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
            .push(make_entry_with_target(relative, kind, 0, &facts, target));
        state.report_if_due(path);
        return 0;
    }

    let size = if facts.is_dir {
        state.directory_count += 1;
        let child_size = scan_directory(path, relative, state);
        state.entries.push(make_entry(
            relative,
            EntryKind::Directory,
            child_size,
            &facts,
        ));
        child_size
    } else {
        let size = facts.apparent_size;
        state.file_count += 1;
        state
            .entries
            .push(make_entry(relative, EntryKind::File, size, &facts));
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

fn estimate_directory_parallel(path: &Path, estimate: &mut ScanEstimate) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            estimate.error_count = estimate.error_count.saturating_add(1);
            return;
        }
    };

    let mut jobs = Vec::new();
    for child in entries {
        let child = match child {
            Ok(child) => child,
            Err(_) => {
                estimate.error_count = estimate.error_count.saturating_add(1);
                continue;
            }
        };
        jobs.push(child.path());
    }

    if jobs.len() < PARALLEL_MIN_JOBS {
        for child_path in jobs {
            estimate_child(&child_path, estimate);
        }
        return;
    }

    let merged = jobs
        .into_par_iter()
        .fold(ScanEstimate::default, |mut local, child_path| {
            estimate_child(&child_path, &mut local);
            local
        })
        .reduce(ScanEstimate::default, |mut left, right| {
            merge_estimate(&mut left, right);
            left
        });
    merge_estimate(estimate, merged);
}

fn estimate_child(path: &Path, estimate: &mut ScanEstimate) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            estimate.error_count = estimate.error_count.saturating_add(1);
            return;
        }
    };

    estimate.total_entries = estimate.total_entries.saturating_add(1);
    if is_reparse_point(&metadata) {
        let target = resolve_link_target(path);
        if is_directory_link(metadata.is_dir(), target.as_deref()) {
            estimate.directory_count = estimate.directory_count.saturating_add(1);
        } else {
            estimate.file_count = estimate.file_count.saturating_add(1);
        }
    } else if metadata.is_dir() {
        estimate.directory_count = estimate.directory_count.saturating_add(1);
        estimate_directory_parallel(path, estimate);
    } else {
        estimate.file_count = estimate.file_count.saturating_add(1);
        estimate.total_bytes = estimate.total_bytes.saturating_add(metadata.len());
    }
}

fn merge_estimate(target: &mut ScanEstimate, source: ScanEstimate) {
    target.total_entries = target.total_entries.saturating_add(source.total_entries);
    target.file_count = target.file_count.saturating_add(source.file_count);
    target.directory_count = target
        .directory_count
        .saturating_add(source.directory_count);
    target.total_bytes = target.total_bytes.saturating_add(source.total_bytes);
    target.error_count = target.error_count.saturating_add(source.error_count);
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

fn make_entry(relative: &str, kind: EntryKind, size_bytes: u64, facts: &FileFacts) -> Entry {
    let is_file = kind == EntryKind::File;
    Entry {
        relative_path: relative.to_string(),
        kind,
        size_bytes,
        modified_unix_ms: facts.modified_unix_ms,
        is_reparse_point: false,
        link_target: None,
        followed: false,
        // A directory's allocation is settled by the accounting pass, which
        // knows which of the file records below it belong to it.
        allocated_bytes: if is_file { facts.allocated_size } else { 0 },
        reclaimable_bytes: 0,
        shared_bytes: 0,
        link_count: if is_file { facts.link_count } else { 0 },
        file_id: if is_file { facts.file_id } else { None },
        is_sparse: if is_file { facts.is_sparse } else { false },
        is_placeholder: if is_file { facts.is_placeholder } else { false },
        allocation_estimated: false,
    }
}

fn make_entry_with_target(
    relative: &str,
    kind: EntryKind,
    size_bytes: u64,
    facts: &FileFacts,
    link_target: Option<String>,
) -> Entry {
    Entry {
        relative_path: relative.to_string(),
        kind,
        size_bytes,
        modified_unix_ms: facts.modified_unix_ms,
        is_reparse_point: true,
        link_target,
        followed: false,
        // A reparse point stands for something else. Its own record is not
        // counted, and the target behind it is not walked into.
        allocated_bytes: 0,
        reclaimable_bytes: 0,
        shared_bytes: 0,
        link_count: 0,
        file_id: None,
        is_sparse: false,
        is_placeholder: false,
        allocation_estimated: false,
    }
}

/// Records a file that could not be opened, using the size its directory
/// listing reports.
///
/// The paging file, the hibernation file and files inside protected folders
/// refuse to be opened by anyone. Leaving them out would hide tens of
/// gigabytes from the measurement, so their size is taken from the listing
/// instead. Their allocation is then an estimate: the filesystem never said
/// how many clusters they own.
fn listed_child(path: &Path, relative: &str, state: &mut ScanState) -> u64 {
    let Some(listed) = fsinfo::listed(path) else {
        return 0;
    };
    if listed.is_dir {
        // A directory that cannot be opened cannot be walked either.
        return 0;
    }
    state.file_count += 1;
    let size = listed.apparent_size;
    state.entries.push(make_listed_entry(relative, &listed));
    state.total_bytes = state.total_bytes.saturating_add(size);
    size
}

/// Builds the entry for a file that was measured from a directory listing.
fn make_listed_entry(relative: &str, listed: &fsinfo::ListedFacts) -> Entry {
    // A sparse file, a compressed file and a placeholder all own fewer
    // clusters than their length says, and the listing does not say by how
    // much. Guessing the length for those would overstate the volume, so
    // they are counted as owning nothing and marked as estimates.
    let owns_clusters =
        !listed.is_sparse && !listed.is_compressed && !listed.is_reparse_point;
    Entry {
        relative_path: relative.to_string(),
        kind: EntryKind::File,
        size_bytes: listed.apparent_size,
        modified_unix_ms: listed.modified_unix_ms,
        is_reparse_point: false,
        link_target: None,
        followed: false,
        allocated_bytes: if owns_clusters {
            listed.apparent_size
        } else {
            0
        },
        reclaimable_bytes: 0,
        shared_bytes: 0,
        link_count: 1,
        file_id: None,
        is_sparse: listed.is_sparse,
        is_placeholder: listed.is_reparse_point,
        allocation_estimated: true,
    }
}

fn is_directory_link(is_dir: bool, target: Option<&str>) -> bool {
    if is_dir {
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
        for index in 0..12 {
            fs::write(root.join(format!("wide-{index}.txt")), index.to_string())
                .expect("write wide root file");
            fs::write(
                root.join("nested").join(format!("wide-{index}.txt")),
                index.to_string(),
            )
            .expect("write wide nested file");
        }

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
    #[test]
    fn hard_linked_file_is_counted_once_and_frees_nothing_on_its_own() {
        let root = test_root_named("hardlink");
        fs::create_dir_all(&root).expect("create fixture");
        let target = root.join("target.bin");
        let alias = root.join("alias.bin");
        fs::write(&target, vec![7u8; 8192]).expect("write target");

        let status = Command::new("cmd")
            .args(["/C", "mklink", "/H"])
            .arg(&alias)
            .arg(&target)
            .status()
            .expect("run mklink");
        if !status.success() {
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let snapshot = scan(&root).expect("scan fixture");
        assert_eq!(snapshot.file_count, 2);
        // The surface size counts the bytes through both names.
        assert_eq!(snapshot.total_bytes, 16_384);
        // The volume holds one record of 8192 bytes.
        assert_eq!(snapshot.total_allocated_bytes, 8192);
        // Deleting one of the names frees nothing: the record survives.
        assert_eq!(snapshot.total_reclaimable_bytes, 8192);

        let find = |path: &str| {
            snapshot
                .entries
                .iter()
                .find(|entry| entry.relative_path == path)
                .expect("entry")
        };
        let root_entry = find("");
        assert_eq!(root_entry.allocated_bytes, 8192);
        // Both links sit in the same folder, so that folder holds the whole
        // record and deleting it gives the bytes back.
        assert_eq!(root_entry.reclaimable_bytes, 8192);
        assert_eq!(root_entry.shared_bytes, 8192);
        // Removing a single name frees nothing.
        assert_eq!(find("alias.bin").reclaimable_bytes, 0);
        assert_eq!(find("target.bin").reclaimable_bytes, 0);

        // Both names report the same record and the same link count.
        assert_eq!(find("alias.bin").file_id, find("target.bin").file_id);
        assert_eq!(find("alias.bin").link_count, 2);
        assert_eq!(find("target.bin").link_count, 2);
        assert_ne!(find("alias.bin").file_id, None);

        fs::remove_dir_all(&root).expect("remove fixture");
    }

    #[cfg(windows)]
    #[test]
    fn sparse_file_reports_no_allocation_instead_of_its_logical_size() {
        let root = test_root_named("sparse");
        fs::create_dir_all(&root).expect("create fixture");
        let file = root.join("hole.bin");
        fs::File::create(&file).expect("create file");

        let status = Command::new("cmd")
            .args(["/C", "fsutil", "sparse", "setflag"])
            .arg(&file)
            .status()
            .expect("run fsutil");
        if !status.success() {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        // Growing a sparse file leaves a hole: the logical size grows, the
        // space the file owns does not.
        let handle = fs::File::options()
            .write(true)
            .open(&file)
            .expect("open sparse file");
        handle.set_len(1024 * 1024 * 1024).expect("grow sparse file");
        drop(handle);

        let snapshot = scan(&root).expect("scan fixture");
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.relative_path == "hole.bin")
            .expect("file entry");

        assert_eq!(entry.size_bytes, 1024 * 1024 * 1024);
        assert!(entry.is_sparse);
        // The hole occupies nothing, so the volume gives up nothing for it.
        assert_eq!(entry.allocated_bytes, 0);
        assert_eq!(snapshot.total_allocated_bytes, 0);
        assert_eq!(snapshot.total_reclaimable_bytes, 0);

        fs::remove_dir_all(&root).expect("remove fixture");
    }

    #[cfg(windows)]
    #[test]
    fn a_file_measured_from_a_listing_counts_its_size_as_an_estimate() {
        use crate::fsinfo::ListedFacts;

        let listed = ListedFacts {
            apparent_size: 26 * 1024 * 1024 * 1024,
            is_dir: false,
            is_sparse: false,
            is_compressed: false,
            is_reparse_point: false,
            modified_unix_ms: None,
        };
        let entry = super::make_listed_entry("pagefile.sys", &listed);
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(entry.size_bytes, 26 * 1024 * 1024 * 1024);
        assert!(entry.allocation_estimated);
        // The filesystem never said how many clusters it owns, so the length
        // stands in for the allocation.
        assert_eq!(entry.allocated_bytes, 26 * 1024 * 1024 * 1024);
        assert_eq!(entry.file_id, None);

        // A file that the listing says owns fewer clusters than its length is
        // not counted at its length.
        let sparse = ListedFacts {
            is_sparse: true,
            ..listed
        };
        let entry = super::make_listed_entry("hole.bin", &sparse);
        assert_eq!(entry.allocated_bytes, 0);
        assert!(entry.is_sparse);
    }

    #[cfg(windows)]
    fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("space-tracker-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// Tests run in parallel and share one process, so every fixture needs
    /// its own directory.
    #[cfg(windows)]
    fn test_root_named(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "space-tracker-{tag}-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }
}
