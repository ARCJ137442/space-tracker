use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::model::{
    Entry, EntryKind, SNAPSHOT_FORMAT_VERSION, Snapshot, SnapshotSummary, StoredSnapshotSummary,
    root_key,
};

const STORE_FORMAT_VERSION: u32 = 1;
const STORAGE_CHILD_NAME: &str = "space-tracker";
const MAX_DELTA_CHAIN: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotFileKind {
    Base,
    Delta,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SnapshotRecord {
    snapshot_id: String,
    file_name: String,
    kind: SnapshotFileKind,
    /// Snapshot format the payload was written in. A record written by an
    /// older build is not comparable with a newer one field for field, so
    /// the store starts a fresh base instead of layering deltas on it.
    #[serde(default)]
    format_version: u32,
    captured_at_unix_ms: u64,
    duration_ms: u64,
    total_bytes: u64,
    #[serde(default)]
    total_allocated_bytes: u64,
    #[serde(default)]
    total_reclaimable_bytes: u64,
    file_count: u64,
    directory_count: u64,
    reparse_point_count: u64,
    error_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoreManifest {
    format_version: u32,
    root: String,
    root_key: String,
    records: Vec<SnapshotRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DeltaSnapshot {
    format_version: u32,
    snapshot_id: String,
    root: String,
    root_key: String,
    captured_at_unix_ms: u64,
    duration_ms: u64,
    total_bytes: u64,
    #[serde(default)]
    total_allocated_bytes: u64,
    #[serde(default)]
    total_reclaimable_bytes: u64,
    file_count: u64,
    directory_count: u64,
    reparse_point_count: u64,
    errors: Vec<crate::model::ScanError>,
    changes: Vec<EntryChange>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", content = "entry", rename_all = "snake_case")]
enum EntryChange {
    Upsert(Entry),
    Delete {
        relative_path: String,
        kind: EntryKind,
    },
}

#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub path: PathBuf,
    pub snapshot: Snapshot,
}

#[derive(Clone, Debug)]
pub struct SnapshotStore {
    data_directory: PathBuf,
    directory: PathBuf,
}

impl SnapshotStore {
    /// `data_directory` is the user-facing location. New files live below a
    /// `space-tracker` child directory, unless the caller already passed a
    /// directory with that name for compatibility with the old CLI default.
    pub fn new(data_directory: PathBuf) -> Self {
        let directory = if is_storage_directory(&data_directory) {
            data_directory.clone()
        } else {
            data_directory.join(STORAGE_CHILD_NAME)
        };
        Self {
            data_directory,
            directory,
        }
    }

    pub fn save(&self, snapshot: &mut Snapshot, _compact: bool) -> Result<PathBuf> {
        let previous = self.latest_for_root_key(&snapshot.root_key)?;
        let use_base = previous.as_ref().is_none_or(|stored| {
            !is_new_format_path(&stored.path)
                || stored.snapshot.format_version != SNAPSHOT_FORMAT_VERSION
                || self.should_rebase(&snapshot.root_key)
        });
        let kind = if use_base {
            SnapshotFileKind::Base
        } else {
            SnapshotFileKind::Delta
        };

        let root_directory = self.root_directory(&snapshot.root_key);
        fs::create_dir_all(&root_directory).with_context(|| {
            format!(
                "cannot create snapshot directory: {}",
                root_directory.display()
            )
        })?;

        let (snapshot_id, path) = allocate_snapshot_path(&root_directory, kind, snapshot);
        snapshot.snapshot_id = snapshot_id;
        if kind == SnapshotFileKind::Base {
            write_compressed_json(&path, snapshot)?;
        } else {
            let previous =
                previous.context("internal error: delta snapshot has no previous snapshot")?;
            let delta = make_delta(&previous.snapshot, snapshot)?;
            write_compressed_json(&path, &delta)?;
        }

        self.append_manifest(&root_directory, snapshot, kind, &path)?;
        Ok(path)
    }

    pub fn load(&self, path: &Path) -> Result<Snapshot> {
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            return load_json(path);
        }
        if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.contains(".base."))
        {
            return load_compressed_json(path);
        }
        if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.contains(".delta."))
        {
            return self.load_delta_chain(path);
        }
        bail!("unsupported snapshot file: {}", path.display());
    }

    pub fn list_summaries(&self) -> Result<Vec<StoredSnapshotSummary>> {
        let mut summaries = self.list_new_summaries()?;
        summaries.extend(self.list_legacy_summaries()?);
        summaries.sort_by_key(|item| item.summary.captured_at_unix_ms);
        Ok(summaries)
    }

    pub fn latest_two_for_root(
        &self,
        root: &Path,
    ) -> Result<Option<(StoredSnapshot, StoredSnapshot)>> {
        let key = root_key(root);
        let mut matching: Vec<_> = self
            .list_summaries()?
            .into_iter()
            .filter(|item| summary_root_key(item) == key)
            .collect();
        matching.sort_by_key(|item| std::cmp::Reverse(item.summary.captured_at_unix_ms));
        if matching.len() < 2 {
            return Ok(None);
        }
        let current = self.load_stored_summary(&matching[0])?;
        let previous = self.load_stored_summary(&matching[1])?;
        Ok(Some((current, previous)))
    }

    pub fn previous_for(&self, current: &Snapshot) -> Result<Option<StoredSnapshot>> {
        let mut matching: Vec<_> = self
            .list_summaries()?
            .into_iter()
            .filter(|item| {
                summary_root_key(item) == current.root_key
                    && item.summary.snapshot_id != current.snapshot_id
                    && item.summary.captured_at_unix_ms < current.captured_at_unix_ms
            })
            .collect();
        matching.sort_by_key(|item| std::cmp::Reverse(item.summary.captured_at_unix_ms));
        matching
            .first()
            .map(|item| self.load_stored_summary(item))
            .transpose()
    }

    pub fn require_latest(&self, root: &Path) -> Result<StoredSnapshot> {
        let key = root_key(root);
        let latest = self
            .list_summaries()?
            .into_iter()
            .filter(|item| summary_root_key(item) == key)
            .max_by_key(|item| item.summary.captured_at_unix_ms)
            .ok_or_else(|| anyhow::anyhow!("no snapshot found for root: {}", root.display()))?;
        self.load_stored_summary(&latest)
    }

    pub fn require_previous_pair(&self, root: &Path) -> Result<(StoredSnapshot, StoredSnapshot)> {
        self.latest_two_for_root(root)?.ok_or_else(|| {
            anyhow::anyhow!("need at least two snapshots for root: {}", root.display())
        })
    }

    fn latest_for_root_key(&self, key: &str) -> Result<Option<StoredSnapshot>> {
        let latest = self
            .list_summaries()?
            .into_iter()
            .filter(|item| summary_root_key(item) == key)
            .max_by_key(|item| item.summary.captured_at_unix_ms);
        latest
            .map(|item| self.load_stored_summary(&item))
            .transpose()
    }

    fn load_stored_summary(&self, item: &StoredSnapshotSummary) -> Result<StoredSnapshot> {
        let path = PathBuf::from(&item.snapshot_path);
        Ok(StoredSnapshot {
            snapshot: self.load(&path)?,
            path,
        })
    }

    fn list_new_summaries(&self) -> Result<Vec<StoredSnapshotSummary>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut summaries = Vec::new();
        for item in fs::read_dir(&self.directory)
            .with_context(|| format!("cannot read store: {}", self.directory.display()))?
        {
            let item = item.context("cannot read store entry")?;
            if !item
                .file_type()
                .context("cannot inspect store entry")?
                .is_dir()
            {
                continue;
            }
            let manifest_path = item.path().join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest: StoreManifest = load_json(&manifest_path)?;
            for record in manifest.records {
                summaries.push(StoredSnapshotSummary {
                    snapshot_path: item
                        .path()
                        .join(&record.file_name)
                        .to_string_lossy()
                        .to_string(),
                    summary: record_summary(&record, &manifest.root),
                });
            }
        }
        Ok(summaries)
    }

    fn list_legacy_summaries(&self) -> Result<Vec<StoredSnapshotSummary>> {
        let mut directories = vec![self.data_directory.clone()];
        if self.data_directory != self.directory {
            directories.push(self.directory.clone());
        }
        let mut summaries = Vec::new();
        for directory in directories {
            if !directory.exists() {
                continue;
            }
            for item in fs::read_dir(&directory)
                .with_context(|| format!("cannot read store: {}", directory.display()))?
            {
                let item = item.context("cannot read store entry")?;
                let path = item.path();
                let is_json = path.extension().and_then(|value| value.to_str()) == Some("json");
                if !is_json
                    || !path.is_file()
                    || path.file_name().and_then(|v| v.to_str()) == Some("manifest.json")
                {
                    continue;
                }
                let snapshot: Snapshot = load_json(&path)?;
                summaries.push(StoredSnapshotSummary {
                    snapshot_path: path.to_string_lossy().to_string(),
                    summary: SnapshotSummary::from_snapshot(&snapshot),
                });
            }
        }
        Ok(summaries)
    }

    fn root_directory(&self, key: &str) -> PathBuf {
        self.directory.join(root_id(key))
    }

    fn should_rebase(&self, key: &str) -> bool {
        let manifest_path = self.root_directory(key).join("manifest.json");
        let Ok(manifest) = load_json::<StoreManifest>(&manifest_path) else {
            return false;
        };
        let Some(base_index) = manifest
            .records
            .iter()
            .rposition(|record| record.kind == SnapshotFileKind::Base)
        else {
            return false;
        };
        manifest.records[base_index + 1..]
            .iter()
            .filter(|record| record.kind == SnapshotFileKind::Delta)
            .count()
            >= MAX_DELTA_CHAIN
    }

    fn append_manifest(
        &self,
        root_directory: &Path,
        snapshot: &Snapshot,
        kind: SnapshotFileKind,
        path: &Path,
    ) -> Result<()> {
        let manifest_path = root_directory.join("manifest.json");
        let mut manifest = if manifest_path.exists() {
            load_json(&manifest_path)?
        } else {
            StoreManifest {
                format_version: STORE_FORMAT_VERSION,
                root: snapshot.root.clone(),
                root_key: snapshot.root_key.clone(),
                records: Vec::new(),
            }
        };
        if manifest.root_key != snapshot.root_key {
            bail!(
                "snapshot root fingerprint collision: {} vs {}",
                manifest.root,
                snapshot.root
            );
        }
        manifest.records.push(SnapshotRecord {
            snapshot_id: snapshot.snapshot_id.clone(),
            file_name: path
                .file_name()
                .and_then(|value| value.to_str())
                .context("snapshot filename is not valid UTF-8")?
                .to_string(),
            kind,
            format_version: snapshot.format_version,
            captured_at_unix_ms: snapshot.captured_at_unix_ms,
            duration_ms: snapshot.duration_ms,
            total_bytes: snapshot.total_bytes,
            total_allocated_bytes: snapshot.total_allocated_bytes,
            total_reclaimable_bytes: snapshot.total_reclaimable_bytes,
            file_count: snapshot.file_count,
            directory_count: snapshot.directory_count,
            reparse_point_count: snapshot.reparse_point_count,
            error_count: snapshot.errors.len(),
        });
        manifest
            .records
            .sort_by_key(|record| record.captured_at_unix_ms);
        write_json_atomically(&manifest_path, &manifest)
    }

    fn load_delta_chain(&self, path: &Path) -> Result<Snapshot> {
        let root_directory = path
            .parent()
            .context("delta snapshot has no parent directory")?;
        let manifest_path = root_directory.join("manifest.json");
        let manifest: StoreManifest = load_json(&manifest_path).with_context(|| {
            format!(
                "cannot read manifest for delta snapshot: {}",
                path.display()
            )
        })?;
        let target_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("delta snapshot filename is not valid UTF-8")?;
        let target_index = manifest
            .records
            .iter()
            .position(|record| record.file_name == target_name)
            .context("delta snapshot is not registered in its manifest")?;
        let base_index = manifest.records[..=target_index]
            .iter()
            .enumerate()
            .filter(|(_, record)| record.kind == SnapshotFileKind::Base)
            .map(|(index, _)| index)
            .next_back()
            .context("delta snapshot has no preceding base snapshot")?;
        let base = &manifest.records[base_index];
        let base_path = root_directory.join(&base.file_name);
        let mut snapshot: Snapshot = load_compressed_json(&base_path)?;
        for record in &manifest.records[base_index + 1..=target_index] {
            if record.kind != SnapshotFileKind::Delta {
                continue;
            }
            let delta_path = root_directory.join(&record.file_name);
            let delta: DeltaSnapshot = load_compressed_json(&delta_path)?;
            apply_delta(&mut snapshot, &delta)?;
        }
        Ok(snapshot)
    }
}

fn is_storage_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            value.eq_ignore_ascii_case(STORAGE_CHILD_NAME)
                || value.eq_ignore_ascii_case(".space-tracker")
        })
}

fn is_new_format_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.contains(".base.") || value.contains(".delta."))
}

fn allocate_snapshot_path(
    root_directory: &Path,
    kind: SnapshotFileKind,
    snapshot: &Snapshot,
) -> (String, PathBuf) {
    let suffix = match kind {
        SnapshotFileKind::Base => "base.zst",
        SnapshotFileKind::Delta => "delta.zst",
    };
    let stem = format!("snapshot-{}", snapshot.captured_at_unix_ms);
    let mut attempt = 0u32;
    loop {
        let snapshot_id = if attempt == 0 {
            stem.clone()
        } else {
            format!("{stem}-{attempt}")
        };
        let path = root_directory.join(format!("{snapshot_id}.{suffix}"));
        if !path.exists() {
            return (snapshot_id, path);
        }
        attempt = attempt.saturating_add(1);
    }
}

fn record_summary(record: &SnapshotRecord, root: &str) -> SnapshotSummary {
    SnapshotSummary {
        snapshot_id: record.snapshot_id.clone(),
        root: root.to_string(),
        captured_at_unix_ms: record.captured_at_unix_ms,
        duration_ms: record.duration_ms,
        total_bytes: record.total_bytes,
        total_allocated_bytes: record.total_allocated_bytes,
        total_reclaimable_bytes: record.total_reclaimable_bytes,
        file_count: record.file_count,
        directory_count: record.directory_count,
        reparse_point_count: record.reparse_point_count,
        error_count: record.error_count,
    }
}

fn summary_root_key(item: &StoredSnapshotSummary) -> String {
    root_key(Path::new(&item.summary.root))
}

fn make_delta(previous: &Snapshot, current: &Snapshot) -> Result<DeltaSnapshot> {
    if previous.root_key != current.root_key {
        bail!(
            "snapshot roots do not match: {} vs {}",
            previous.root,
            current.root
        );
    }
    let previous_entries = index_entries(&previous.entries);
    let current_entries = index_entries(&current.entries);
    let mut changes = Vec::new();

    for (key, entry) in &current_entries {
        if previous_entries.get(key) != Some(entry) {
            changes.push(EntryChange::Upsert((*entry).clone()));
        }
    }
    for (kind, relative_path) in previous_entries.keys() {
        let key = (*kind, relative_path.clone());
        if !current_entries.contains_key(&key) {
            changes.push(EntryChange::Delete {
                relative_path: relative_path.clone(),
                kind: *kind,
            });
        }
    }
    changes.sort_by(|left, right| change_key(left).cmp(&change_key(right)));

    Ok(DeltaSnapshot {
        format_version: SNAPSHOT_FORMAT_VERSION,
        snapshot_id: current.snapshot_id.clone(),
        root: current.root.clone(),
        root_key: current.root_key.clone(),
        captured_at_unix_ms: current.captured_at_unix_ms,
        duration_ms: current.duration_ms,
        total_bytes: current.total_bytes,
        total_allocated_bytes: current.total_allocated_bytes,
        total_reclaimable_bytes: current.total_reclaimable_bytes,
        file_count: current.file_count,
        directory_count: current.directory_count,
        reparse_point_count: current.reparse_point_count,
        errors: current.errors.clone(),
        changes,
    })
}

fn change_key(change: &EntryChange) -> (EntryKind, &str, u8) {
    match change {
        EntryChange::Upsert(entry) => (entry.kind, &entry.relative_path, 0),
        EntryChange::Delete {
            relative_path,
            kind,
        } => (*kind, relative_path, 1),
    }
}

fn index_entries(entries: &[Entry]) -> HashMap<(EntryKind, String), &Entry> {
    entries
        .iter()
        .map(|entry| ((entry.kind, entry.relative_path.clone()), entry))
        .collect()
}

fn apply_delta(snapshot: &mut Snapshot, delta: &DeltaSnapshot) -> Result<()> {
    if snapshot.root_key != delta.root_key {
        bail!(
            "delta root does not match base root: {} vs {}",
            delta.root,
            snapshot.root
        );
    }
    let mut entries: HashMap<(EntryKind, String), Entry> = snapshot
        .entries
        .drain(..)
        .map(|entry| ((entry.kind, entry.relative_path.clone()), entry))
        .collect();
    for change in &delta.changes {
        match change {
            EntryChange::Upsert(entry) => {
                entries.insert((entry.kind, entry.relative_path.clone()), entry.clone());
            }
            EntryChange::Delete {
                relative_path,
                kind,
            } => {
                entries.remove(&(*kind, relative_path.clone()));
            }
        }
    }
    snapshot.format_version = delta.format_version;
    snapshot.snapshot_id = delta.snapshot_id.clone();
    snapshot.root = delta.root.clone();
    snapshot.root_key = delta.root_key.clone();
    snapshot.captured_at_unix_ms = delta.captured_at_unix_ms;
    snapshot.duration_ms = delta.duration_ms;
    snapshot.total_bytes = delta.total_bytes;
    snapshot.total_allocated_bytes = delta.total_allocated_bytes;
    snapshot.total_reclaimable_bytes = delta.total_reclaimable_bytes;
    snapshot.file_count = delta.file_count;
    snapshot.directory_count = delta.directory_count;
    snapshot.reparse_point_count = delta.reparse_point_count;
    snapshot.errors = delta.errors.clone();
    snapshot.entries = entries.into_values().collect();
    snapshot.entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    Ok(())
}

fn root_id(key: &str) -> String {
    // This is an opaque, stable directory name rather than a security hash.
    // The manifest still stores and validates the complete root_key.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn write_compressed_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .context("snapshot filename is not valid UTF-8")?
    ));
    let file = File::create(&temporary)
        .with_context(|| format!("cannot create temporary snapshot: {}", temporary.display()))?;
    let writer = BufWriter::new(file);
    let mut encoder =
        zstd::stream::write::Encoder::new(writer, 3).context("cannot initialize zstd encoder")?;
    serde_json::to_writer(&mut encoder, value).context("failed to encode compressed snapshot")?;
    let mut writer = encoder.finish().context("cannot finish zstd snapshot")?;
    writer
        .flush()
        .with_context(|| format!("cannot flush temporary snapshot: {}", temporary.display()))?;
    drop(writer);
    fs::rename(&temporary, path)
        .with_context(|| format!("cannot commit snapshot: {}", path.display()))?;
    Ok(())
}

fn load_compressed_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path)
        .with_context(|| format!("cannot read compressed snapshot: {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
        .context("cannot initialize zstd decoder")?;
    serde_json::from_reader(decoder)
        .with_context(|| format!("invalid compressed snapshot: {}", path.display()))
}

fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file =
        File::open(path).with_context(|| format!("cannot read snapshot: {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("invalid snapshot JSON: {}", path.display()))
}

fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .context("manifest filename is not valid UTF-8")?
    ));
    let file = File::create(&temporary)
        .with_context(|| format!("cannot create temporary manifest: {}", temporary.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value).context("failed to encode manifest")?;
    writer
        .flush()
        .with_context(|| format!("cannot flush temporary manifest: {}", temporary.display()))?;
    drop(writer);
    fs::rename(&temporary, path)
        .with_context(|| format!("cannot commit manifest: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SnapshotStore;
    use crate::model::{Entry, EntryKind, SNAPSHOT_FORMAT_VERSION, Snapshot, root_key};
    use std::fs;
    use std::path::PathBuf;

    fn snapshot(id: &str, captured: u64, child_size: u64) -> Snapshot {
        let root = PathBuf::from("C:\\fixture");
        Snapshot {
            format_version: SNAPSHOT_FORMAT_VERSION,
            snapshot_id: id.to_string(),
            root: root.to_string_lossy().to_string(),
            root_key: root_key(&root),
            captured_at_unix_ms: captured,
            duration_ms: 2,
            total_bytes: child_size,
            file_count: 1,
            directory_count: 2,
            reparse_point_count: 0,
            errors: Vec::new(),
            entries: vec![
                Entry {
                    relative_path: String::new(),
                    kind: EntryKind::Directory,
                    size_bytes: child_size,
                    ..Default::default()
                },
                Entry {
                    relative_path: "child".to_string(),
                    kind: EntryKind::File,
                    size_bytes: child_size,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn base_and_delta_round_trip_with_child_data_directory() {
        let data_directory = std::env::temp_dir().join(format!(
            "space-tracker-compressed-store-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&data_directory);
        let store = SnapshotStore::new(data_directory.clone());
        let mut first = snapshot("first", 1, 10);
        let base_path = store.save(&mut first, true).expect("save base");
        assert!(base_path.to_string_lossy().contains("space-tracker"));
        assert!(base_path.to_string_lossy().contains(".base.zst"));

        let mut second = snapshot("second", 2, 20);
        let delta_path = store.save(&mut second, true).expect("save delta");
        assert!(delta_path.to_string_lossy().contains(".delta.zst"));

        let loaded = store.load(&delta_path).expect("load delta chain");
        assert_eq!(loaded.snapshot_id, second.snapshot_id);
        assert_eq!(loaded.total_bytes, 20);
        assert_eq!(loaded.entries.len(), second.entries.len());
        assert_eq!(store.list_summaries().expect("list summaries").len(), 2);

        for captured in 3..=33 {
            let mut next = snapshot("next", captured, captured);
            store.save(&mut next, true).expect("save delta chain");
        }
        let mut rebased = snapshot("rebased", 34, 34);
        let rebased_path = store.save(&mut rebased, true).expect("rebase delta chain");
        assert!(rebased_path.to_string_lossy().contains(".base.zst"));

        fs::remove_dir_all(data_directory).expect("remove test store");
    }

    #[test]
    fn legacy_json_is_read_and_new_scan_starts_a_new_base() {
        let data_directory = std::env::temp_dir().join(format!(
            "space-tracker-legacy-store-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&data_directory);
        fs::create_dir_all(&data_directory).expect("create legacy store");
        let mut legacy = snapshot("legacy", 1, 10);
        let legacy_path = data_directory.join("snapshot-1.json");
        fs::write(
            &legacy_path,
            serde_json::to_vec(&legacy).expect("encode legacy"),
        )
        .expect("write legacy");

        let store = SnapshotStore::new(data_directory.clone());
        assert_eq!(store.list_summaries().expect("list legacy").len(), 1);
        legacy.captured_at_unix_ms = 2;
        let path = store.save(&mut legacy, true).expect("save migrated base");
        assert!(path.to_string_lossy().contains(".base.zst"));
        assert_eq!(store.list_summaries().expect("list migrated").len(), 2);

        fs::remove_dir_all(data_directory).expect("remove legacy store");
    }
}
