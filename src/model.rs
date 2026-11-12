use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_FORMAT_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScanError {
    pub path: String,
    pub message: String,
}

/// The identity of one file record on a volume. Two directory entries that
/// share a `FileId` are hard links to the same bytes on disk.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct FileId {
    pub volume_serial: u32,
    pub file_index: u64,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            relative_path: String::new(),
            kind: EntryKind::File,
            size_bytes: 0,
            modified_unix_ms: None,
            is_reparse_point: false,
            link_target: None,
            followed: false,
            allocated_bytes: 0,
            reclaimable_bytes: 0,
            shared_bytes: 0,
            link_count: 0,
            file_id: None,
            is_sparse: false,
            is_placeholder: false,
        }
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            format_version: SNAPSHOT_FORMAT_VERSION,
            snapshot_id: String::new(),
            root: String::new(),
            root_key: String::new(),
            captured_at_unix_ms: 0,
            duration_ms: 0,
            total_bytes: 0,
            total_allocated_bytes: 0,
            total_reclaimable_bytes: 0,
            file_count: 0,
            directory_count: 0,
            reparse_point_count: 0,
            errors: Vec::new(),
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Entry {
    pub relative_path: String,
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub modified_unix_ms: Option<i64>,
    pub is_reparse_point: bool,
    pub link_target: Option<String>,
    pub followed: bool,
    /// Space the entry occupies on the volume, as the filesystem reports it:
    /// whole clusters, less for a compressed or sparse file. For a directory
    /// this is the allocation attributed to the subtree rather than the sum of
    /// its children's own allocation, because a file record that appears under
    /// several paths is counted once, under the path that owns it.
    #[serde(default)]
    pub allocated_bytes: u64,
    /// Space that deleting this entry would really free. A file with other
    /// hard links frees nothing: the record survives under its other names.
    #[serde(default)]
    pub reclaimable_bytes: u64,
    /// Apparent bytes of this entry that belong to a file record owned by
    /// another path. This is the part of the surface size that deleting the
    /// entry would not return to the volume.
    #[serde(default)]
    pub shared_bytes: u64,
    /// Number of hard links to the file record. Zero when the platform does
    /// not report one.
    #[serde(default)]
    pub link_count: u32,
    /// Identity of the file record, when the platform can report one.
    #[serde(default)]
    pub file_id: Option<FileId>,
    /// The file keeps its data in holes: its logical length is larger than
    /// the clusters it owns.
    #[serde(default)]
    pub is_sparse: bool,
    /// The file is a placeholder whose data lives elsewhere — a cloud file
    /// the filesystem recalls on access. It occupies no space on this volume.
    #[serde(default)]
    pub is_placeholder: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub format_version: u32,
    pub snapshot_id: String,
    pub root: String,
    pub root_key: String,
    pub captured_at_unix_ms: u64,
    pub duration_ms: u64,
    pub total_bytes: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub reparse_point_count: u64,
    /// Allocation of every distinct file record under the root. This is the
    /// number that tracks the volume's real occupancy, unlike `total_bytes`,
    /// which adds up apparent lengths and counts hard links twice.
    #[serde(default)]
    pub total_allocated_bytes: u64,
    /// Space that deleting every file under the root would really free.
    #[serde(default)]
    pub total_reclaimable_bytes: u64,
    pub errors: Vec<ScanError>,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub root: String,
    pub captured_at_unix_ms: u64,
    pub duration_ms: u64,
    pub total_bytes: u64,
    /// Allocation of every distinct file record under the root.
    pub total_allocated_bytes: u64,
    /// Space that deleting everything under the root would really free.
    pub total_reclaimable_bytes: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub reparse_point_count: u64,
    pub error_count: usize,
}

impl SnapshotSummary {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            snapshot_id: snapshot.snapshot_id.clone(),
            root: snapshot.root.clone(),
            captured_at_unix_ms: snapshot.captured_at_unix_ms,
            duration_ms: snapshot.duration_ms,
            total_bytes: snapshot.total_bytes,
            total_allocated_bytes: snapshot.total_allocated_bytes,
            total_reclaimable_bytes: snapshot.total_reclaimable_bytes,
            file_count: snapshot.file_count,
            directory_count: snapshot.directory_count,
            reparse_point_count: snapshot.reparse_point_count,
            error_count: snapshot.errors.len(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StoredSnapshotSummary {
    pub snapshot_path: String,
    pub summary: SnapshotSummary,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeltaItem {
    pub relative_path: String,
    pub current_bytes: u64,
    pub previous_bytes: u64,
    pub delta_bytes: i64,
    pub delta_percent: Option<f64>,
    pub growth_per_day_bytes: Option<f64>,
}

/// What deleting one path would really give back to the volume.
#[derive(Clone, Debug, Serialize)]
pub struct ReclaimReport {
    pub root: String,
    pub relative_path: String,
    pub path: String,
    pub snapshot_id: String,
    pub captured_at_unix_ms: u64,
    /// Sum of the apparent lengths below the path.
    pub apparent_bytes: u64,
    /// Space the path occupies on the volume, counting each file record once.
    pub allocated_bytes: u64,
    /// Space that deleting the path would really free.
    pub reclaimable_bytes: u64,
    /// Part of the apparent size that belongs to file records owned elsewhere.
    pub shared_bytes: u64,
    /// Hard links to the file record, when the path is a single file.
    pub link_count: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeltaReport {
    pub root: String,
    pub previous_snapshot: String,
    pub current_snapshot: String,
    pub elapsed_ms: u64,
    pub growing_directories: Vec<DeltaItem>,
    pub shrinking_directories: Vec<DeltaItem>,
    pub new_files: Vec<DeltaItem>,
    pub removed_files: Vec<DeltaItem>,
}

pub fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    // "C:" names the current directory of that drive, which is not what a
    // scan root means. Treat a drive without a root as the drive's root.
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        return Ok(PathBuf::from(format!(
            "{}\\",
            prefix.as_os_str().to_string_lossy()
        )));
    }
    Ok(std::env::current_dir()?.join(path))
}

pub fn root_key(path: &Path) -> String {
    let mut value = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    while value.len() > 3 && value.ends_with('\\') {
        value.pop();
    }
    value
}

pub fn relative_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    let value = value.trim_matches('/');
    if value == "." {
        String::new()
    } else {
        value.to_string()
    }
}

pub fn target_relative_path(snapshot: &Snapshot, requested: &Path) -> anyhow::Result<String> {
    if requested.is_absolute() {
        let root = Path::new(&snapshot.root);
        let relative = requested.strip_prefix(root).map_err(|_| {
            anyhow::anyhow!("path is outside snapshot root: {}", requested.display())
        })?;
        Ok(relative_key(relative))
    } else {
        Ok(relative_key(requested))
    }
}

#[cfg(test)]
mod tests {
    use super::{absolute_path, relative_key, root_key};
    use std::path::{Path, PathBuf};

    #[test]
    fn normalizes_root_case_and_trailing_separator() {
        assert_eq!(root_key(Path::new("C:/Users/")), "c:\\users");
        assert_eq!(root_key(Path::new("C:/")), "c:\\");
    }

    #[test]
    fn normalizes_relative_paths_for_json() {
        assert_eq!(relative_key(Path::new("foo\\bar")), "foo/bar");
        assert_eq!(relative_key(Path::new(".")), "");
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_drive_letter_means_the_drive_root() {
        assert_eq!(
            absolute_path(Path::new("C:")).expect("path"),
            PathBuf::from("C:\\")
        );
        assert_eq!(
            absolute_path(Path::new("C:\\Users")).expect("path"),
            PathBuf::from("C:\\Users")
        );
    }
}
