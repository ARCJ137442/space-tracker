use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::model::{Snapshot, root_key};

#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub path: PathBuf,
    pub snapshot: Snapshot,
}

#[derive(Clone, Debug)]
pub struct SnapshotStore {
    pub directory: PathBuf,
}

impl SnapshotStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn save(&self, snapshot: &mut Snapshot, compact: bool) -> Result<PathBuf> {
        fs::create_dir_all(&self.directory)
            .with_context(|| format!("cannot create store: {}", self.directory.display()))?;
        let base = format!("snapshot-{}.json", snapshot.captured_at_unix_ms);
        let mut path = self.directory.join(base);
        let mut suffix = 1u32;
        while path.exists() {
            path = self.directory.join(format!(
                "snapshot-{}-{}.json",
                snapshot.captured_at_unix_ms, suffix
            ));
            suffix = suffix.saturating_add(1);
        }
        snapshot.snapshot_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("snapshot")
            .to_string();
        let temporary = path.with_extension("json.tmp");
        let file = File::create(&temporary).with_context(|| {
            format!("cannot create temporary snapshot: {}", temporary.display())
        })?;
        let mut writer = BufWriter::new(file);
        if compact {
            serde_json::to_writer(&mut writer, snapshot).context("failed to encode snapshot")?;
        } else {
            serde_json::to_writer_pretty(&mut writer, snapshot)
                .context("failed to encode snapshot")?;
        }
        writer
            .flush()
            .with_context(|| format!("cannot flush temporary snapshot: {}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("cannot commit snapshot: {}", path.display()))?;
        Ok(path)
    }

    pub fn load(&self, path: &Path) -> Result<Snapshot> {
        let file = File::open(path)
            .with_context(|| format!("cannot read snapshot: {}", path.display()))?;
        serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("invalid snapshot JSON: {}", path.display()))
    }

    pub fn list(&self) -> Result<Vec<StoredSnapshot>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut snapshots = Vec::new();
        for item in fs::read_dir(&self.directory)
            .with_context(|| format!("cannot read store: {}", self.directory.display()))?
        {
            let item = item.context("cannot read store entry")?;
            let path = item.path();
            let is_json = path.extension().and_then(|value| value.to_str()) == Some("json");
            if !is_json || !path.is_file() {
                continue;
            }
            snapshots.push(StoredSnapshot {
                snapshot: self.load(&path)?,
                path,
            });
        }
        snapshots.sort_by_key(|item| item.snapshot.captured_at_unix_ms);
        Ok(snapshots)
    }

    pub fn latest_two_for_root(
        &self,
        root: &Path,
    ) -> Result<Option<(StoredSnapshot, StoredSnapshot)>> {
        let key = root_key(root);
        let mut matching: Vec<_> = self
            .list()?
            .into_iter()
            .filter(|item| item.snapshot.root_key == key)
            .collect();
        matching.sort_by_key(|item| std::cmp::Reverse(item.snapshot.captured_at_unix_ms));
        if matching.len() < 2 {
            return Ok(None);
        }
        Ok(Some((matching.remove(0), matching.remove(0))))
    }

    pub fn previous_for(&self, current: &Snapshot) -> Result<Option<StoredSnapshot>> {
        let mut matching: Vec<_> = self
            .list()?
            .into_iter()
            .filter(|item| {
                item.snapshot.root_key == current.root_key
                    && item.snapshot.snapshot_id != current.snapshot_id
                    && item.snapshot.captured_at_unix_ms < current.captured_at_unix_ms
            })
            .collect();
        matching.sort_by_key(|item| std::cmp::Reverse(item.snapshot.captured_at_unix_ms));
        Ok(matching.into_iter().next())
    }

    pub fn require_latest(&self, root: &Path) -> Result<StoredSnapshot> {
        let key = root_key(root);
        self.list()?
            .into_iter()
            .filter(|item| item.snapshot.root_key == key)
            .max_by_key(|item| item.snapshot.captured_at_unix_ms)
            .ok_or_else(|| anyhow::anyhow!("no snapshot found for root: {}", root.display()))
    }

    pub fn require_previous_pair(&self, root: &Path) -> Result<(StoredSnapshot, StoredSnapshot)> {
        self.latest_two_for_root(root)?.ok_or_else(|| {
            anyhow::anyhow!("need at least two snapshots for root: {}", root.display())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotStore;
    use crate::model::{SNAPSHOT_FORMAT_VERSION, Snapshot, root_key};
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn compact_snapshot_round_trips_through_streaming_store() {
        let directory =
            std::env::temp_dir().join(format!("space-tracker-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let store = SnapshotStore::new(directory.clone());
        let root = PathBuf::from("C:\\fixture");
        let mut snapshot = Snapshot {
            format_version: SNAPSHOT_FORMAT_VERSION,
            snapshot_id: String::new(),
            root: root.to_string_lossy().to_string(),
            root_key: root_key(&root),
            captured_at_unix_ms: 1,
            duration_ms: 2,
            total_bytes: 3,
            file_count: 1,
            directory_count: 1,
            reparse_point_count: 0,
            errors: Vec::new(),
            entries: Vec::new(),
        };

        let path = store
            .save(&mut snapshot, true)
            .expect("save compact snapshot");
        let text = fs::read_to_string(&path).expect("read compact snapshot");
        assert!(!text.contains('\n'));
        let loaded = store.load(&path).expect("load compact snapshot");
        assert_eq!(loaded.total_bytes, snapshot.total_bytes);
        assert_eq!(loaded.root_key, snapshot.root_key);

        fs::remove_dir_all(directory).expect("remove test store");
    }
}
