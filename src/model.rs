use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Entry {
    pub relative_path: String,
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub modified_unix_ms: Option<i64>,
    pub is_reparse_point: bool,
    pub link_target: Option<String>,
    pub followed: bool,
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
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
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
    use super::{relative_key, root_key};
    use std::path::Path;

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
}
