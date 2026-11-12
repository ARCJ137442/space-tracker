use anyhow::{Result, bail};
use std::collections::HashMap;

use crate::model::{DeltaItem, DeltaReport, Entry, EntryKind, Snapshot};

pub fn compare(
    current: &Snapshot,
    previous: &Snapshot,
    limit: usize,
    minimum_bytes: u64,
) -> Result<DeltaReport> {
    if current.root_key != previous.root_key {
        bail!(
            "snapshot roots do not match: {} vs {}",
            previous.root,
            current.root
        );
    }
    let elapsed_ms = current
        .captured_at_unix_ms
        .saturating_sub(previous.captured_at_unix_ms);
    let previous_entries = index_entries(&previous.entries);
    let current_entries = index_entries(&current.entries);

    let mut growing_directories = Vec::new();
    let mut shrinking_directories = Vec::new();
    for entry in current
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
    {
        let previous_bytes = previous_entries
            .get(&(EntryKind::Directory, entry.relative_path.clone()))
            .map_or(0, |old| old.size_bytes);
        let delta_bytes = signed_delta(entry.size_bytes, previous_bytes);
        if delta_bytes.unsigned_abs() < minimum_bytes || delta_bytes == 0 {
            continue;
        }
        let item = make_item(
            entry,
            entry.size_bytes,
            previous_bytes,
            delta_bytes,
            elapsed_ms,
        );
        if delta_bytes > 0 {
            growing_directories.push(item);
        } else {
            shrinking_directories.push(item);
        }
    }

    let mut new_files = Vec::new();
    for entry in current
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
    {
        let key = (EntryKind::File, entry.relative_path.clone());
        if !previous_entries.contains_key(&key) && entry.size_bytes >= minimum_bytes {
            new_files.push(make_item(
                entry,
                entry.size_bytes,
                0,
                entry.size_bytes as i64,
                elapsed_ms,
            ));
        }
    }

    let mut removed_files = Vec::new();
    for entry in previous
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
    {
        let key = (EntryKind::File, entry.relative_path.clone());
        if !current_entries.contains_key(&key) && entry.size_bytes >= minimum_bytes {
            let delta_bytes = -(entry.size_bytes.min(i64::MAX as u64) as i64);
            removed_files.push(make_item(
                entry,
                0,
                entry.size_bytes,
                delta_bytes,
                elapsed_ms,
            ));
        }
    }

    sort_and_limit(&mut growing_directories, limit);
    sort_and_limit(&mut shrinking_directories, limit);
    sort_and_limit(&mut new_files, limit);
    sort_and_limit(&mut removed_files, limit);

    Ok(DeltaReport {
        root: current.root.clone(),
        previous_snapshot: previous.snapshot_id.clone(),
        current_snapshot: current.snapshot_id.clone(),
        elapsed_ms,
        growing_directories,
        shrinking_directories,
        new_files,
        removed_files,
    })
}

fn index_entries(entries: &[Entry]) -> HashMap<(EntryKind, String), &Entry> {
    entries
        .iter()
        .map(|entry| ((entry.kind, entry.relative_path.clone()), entry))
        .collect()
}

fn make_item(
    entry: &Entry,
    current_bytes: u64,
    previous_bytes: u64,
    delta_bytes: i64,
    elapsed_ms: u64,
) -> DeltaItem {
    let delta_percent =
        (previous_bytes > 0).then(|| delta_bytes as f64 / previous_bytes as f64 * 100.0);
    let growth_per_day_bytes =
        (elapsed_ms > 0).then(|| delta_bytes as f64 * 86_400_000.0 / elapsed_ms as f64);
    DeltaItem {
        relative_path: entry.relative_path.clone(),
        current_bytes,
        previous_bytes,
        delta_bytes,
        delta_percent,
        growth_per_day_bytes,
    }
}

fn signed_delta(current: u64, previous: u64) -> i64 {
    if current >= previous {
        (current - previous).min(i64::MAX as u64) as i64
    } else {
        -((previous - current).min(i64::MAX as u64) as i64)
    }
}

fn sort_and_limit(items: &mut Vec<DeltaItem>, limit: usize) {
    items.sort_by(|left, right| {
        right
            .delta_bytes
            .unsigned_abs()
            .cmp(&left.delta_bytes.unsigned_abs())
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    items.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::compare;
    use crate::model::{Entry, EntryKind, Snapshot};

    fn snapshot(id: &str, captured: u64, size: u64) -> Snapshot {
        Snapshot {
            format_version: 1,
            snapshot_id: id.to_string(),
            root: "C:\\Root".to_string(),
            root_key: "c:\\root".to_string(),
            captured_at_unix_ms: captured,
            duration_ms: 1,
            total_bytes: size,
            file_count: 1,
            directory_count: 2,
            reparse_point_count: 0,
            errors: Vec::new(),
            entries: vec![
                Entry {
                    relative_path: "".to_string(),
                    kind: EntryKind::Directory,
                    size_bytes: size,
                    ..Default::default()
                },
                Entry {
                    relative_path: "child".to_string(),
                    kind: EntryKind::Directory,
                    size_bytes: size,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn reports_nested_directory_growth_and_daily_rate() {
        let old = snapshot("old", 0, 100);
        let new = snapshot("new", 86_400_000, 250);
        let report = compare(&new, &old, 10, 1).expect("delta should work");
        assert_eq!(report.growing_directories[0].relative_path, "");
        assert_eq!(report.growing_directories[0].delta_bytes, 150);
        assert_eq!(
            report.growing_directories[0].growth_per_day_bytes,
            Some(150.0)
        );
        assert!(
            report
                .growing_directories
                .iter()
                .any(|item| item.relative_path == "child")
        );
    }
}
