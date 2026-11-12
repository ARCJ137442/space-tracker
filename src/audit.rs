//! A check on how a snapshot's bytes were counted.
//!
//! A scan reports three numbers that can drift apart: the surface size (the
//! apparent lengths of every path added up), the allocation (each file record
//! counted once) and the reclaimable space (what deleting everything would
//! really free). When a volume holds many hard links, those three differ a
//! lot, and it is worth seeing where the difference comes from before any of
//! them is trusted.
//!
//! The audit also looks for *suspected* duplicates: files that carry the same
//! size and the same write time under different paths and different record
//! identities. Hard links share an identity, so such a pair is not a hard
//! link — it is the same bytes counted twice because the walk reached them
//! by two different routes.

use std::collections::HashMap;

use serde::Serialize;

use crate::model::{EntryKind, FileId, Snapshot};

/// One file record seen under several paths.
#[derive(Debug, Serialize)]
pub struct SharedRecord {
    pub paths: u64,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub link_count: u32,
    pub sample_paths: Vec<String>,
}

/// Files that look like the same bytes reached by two different routes.
#[derive(Debug, Serialize)]
pub struct SuspectedDuplicate {
    pub copies: u64,
    pub size_bytes: u64,
    pub modified_unix_ms: Option<i64>,
    pub sample_paths: Vec<String>,
}

/// Everything the audit found.
#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub root: String,
    pub snapshot_id: String,
    pub captured_at_unix_ms: u64,
    pub file_count: u64,
    /// Files whose platform reported no record identity.
    pub files_without_identity: u64,
    /// Distinct file records behind those files.
    pub distinct_records: u64,
    /// Records that more than one path points at.
    pub records_with_multiple_paths: u64,
    /// Sum of the apparent lengths of every path.
    pub apparent_bytes_all_paths: u64,
    /// Sum of the apparent lengths of every distinct record.
    pub apparent_bytes_distinct_records: u64,
    /// Sum of the allocation of every distinct record.
    pub allocated_bytes_distinct_records: u64,
    /// Allocation of the records that no other path links to.
    pub allocated_bytes_single_link_records: u64,
    /// How many files report each link count.
    pub link_count_histogram: Vec<(u32, u64)>,
    /// Volume serials seen among the file records. A scan of one volume
    /// should only ever see one.
    pub volume_serials: Vec<(u32, u64)>,
    /// Files whose data is not on this volume: cloud placeholders and
    /// sparse files. Their logical size counts towards the surface size but
    /// not towards the space the volume actually gives up.
    pub placeholder_files: u64,
    pub placeholder_apparent_bytes: u64,
    pub sparse_files: u64,
    pub sparse_apparent_bytes: u64,
    /// Files that could not be opened, so their size came from the directory
    /// listing and their allocation is an estimate.
    pub estimated_files: u64,
    pub estimated_apparent_bytes: u64,
    /// Directories the walk could not read.
    pub unreadable_directories: Vec<String>,
    /// How many times each kind of read failure happened.
    pub error_kinds: Vec<(String, u64)>,
    pub largest_shared_records: Vec<SharedRecord>,
    pub largest_files: Vec<LargestFile>,
    pub suspected_duplicates: Vec<SuspectedDuplicate>,
}

/// The biggest files in the tree, by the space they occupy on the volume.
#[derive(Debug, Serialize)]
pub struct LargestFile {
    pub relative_path: String,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
    pub link_count: u32,
}

const SAMPLE_LIMIT: usize = 3;
const LIST_LIMIT: usize = 10;

pub fn run(snapshot: &Snapshot) -> AuditReport {
    let mut records: HashMap<FileId, (u64, u64, u32, Vec<String>)> = HashMap::new();
    let mut without_identity = 0u64;
    let mut apparent_all_paths = 0u64;
    let mut histogram: HashMap<u32, u64> = HashMap::new();
    let mut serials: HashMap<u32, u64> = HashMap::new();
    let mut placeholder_files = 0u64;
    let mut placeholder_apparent_bytes = 0u64;
    let mut sparse_files = 0u64;
    let mut sparse_apparent_bytes = 0u64;
    let mut estimated_files = 0u64;
    let mut estimated_apparent_bytes = 0u64;
    // Files the platform could not identify, counted once each.
    let mut unidentified_allocated = 0u64;
    let mut unidentified_apparent = 0u64;
    let mut unidentified_single_link = 0u64;
    // Files grouped by size and write time, to catch the same bytes counted
    // under two different record identities.
    let mut by_shape: HashMap<(u64, Option<i64>), Vec<String>> = HashMap::new();

    for entry in snapshot.entries.iter() {
        if entry.kind != EntryKind::File {
            continue;
        }
        apparent_all_paths = apparent_all_paths.saturating_add(entry.size_bytes);
        *histogram.entry(entry.link_count).or_default() += 1;
        if entry.is_placeholder {
            placeholder_files += 1;
            placeholder_apparent_bytes =
                placeholder_apparent_bytes.saturating_add(entry.size_bytes);
        } else if entry.is_sparse {
            sparse_files += 1;
            sparse_apparent_bytes = sparse_apparent_bytes.saturating_add(entry.size_bytes);
        }
        if entry.allocation_estimated {
            estimated_files += 1;
            estimated_apparent_bytes =
                estimated_apparent_bytes.saturating_add(entry.size_bytes);
        }
        by_shape
            .entry((entry.size_bytes, entry.modified_unix_ms))
            .or_default()
            .push(entry.relative_path.clone());

        match entry.file_id {
            Some(id) => {
                *serials.entry(id.volume_serial).or_default() += 1;
                let slot = records.entry(id).or_insert((0, 0, 0, Vec::new()));
                slot.0 = slot.0.max(entry.allocated_bytes);
                slot.1 = slot.1.max(entry.size_bytes);
                slot.2 = slot.2.max(entry.link_count);
                if slot.3.len() < SAMPLE_LIMIT {
                    slot.3.push(entry.relative_path.clone());
                }
            }
            None => {
                without_identity += 1;
                // A file the platform could not identify is its own record:
                // no other path can point at it, so it is counted once.
                unidentified_allocated = unidentified_allocated.saturating_add(entry.allocated_bytes);
                unidentified_apparent = unidentified_apparent.saturating_add(entry.size_bytes);
                if entry.link_count <= 1 {
                    unidentified_single_link = unidentified_single_link
                        .saturating_add(entry.allocated_bytes);
                }
            }
        }
    }

    let mut allocated_distinct = 0u64;
    let mut apparent_distinct = 0u64;
    let mut allocated_single_link = 0u64;
    let mut multi_path_records = 0u64;
    let mut largest_shared: Vec<SharedRecord> = Vec::new();
    for (allocated, apparent, link_count, sample_paths) in records.values() {
        allocated_distinct = allocated_distinct.saturating_add(*allocated);
        apparent_distinct = apparent_distinct.saturating_add(*apparent);
        if *link_count <= 1 {
            allocated_single_link = allocated_single_link.saturating_add(*allocated);
        }
        if sample_paths.len() > 1 {
            multi_path_records += 1;
        }
    }

    // The files the platform could not identify are counted once each, so
    // the audit's allocation matches the snapshot's own total.
    allocated_distinct = allocated_distinct.saturating_add(unidentified_allocated);
    apparent_distinct = apparent_distinct.saturating_add(unidentified_apparent);
    allocated_single_link = allocated_single_link.saturating_add(unidentified_single_link);

    // A record's path count is how many entries carry its identity.
    let mut path_counts: HashMap<FileId, u64> = HashMap::new();
    for entry in snapshot.entries.iter() {
        if entry.kind == EntryKind::File && let Some(id) = entry.file_id {
            *path_counts.entry(id).or_default() += 1;
        }
    }
    let distinct_records = records.len() as u64;    for (id, (allocated, apparent, link_count, sample_paths)) in records {
        let paths = path_counts.get(&id).copied().unwrap_or(0);
        if paths > 1 {
            largest_shared.push(SharedRecord {
                paths,
                allocated_bytes: allocated,
                apparent_bytes: apparent,
                link_count,
                sample_paths,
            });
        }
    }
    largest_shared.sort_by(|left, right| {
        right
            .allocated_bytes
            .cmp(&left.allocated_bytes)
            .then_with(|| left.paths.cmp(&right.paths))
    });
    largest_shared.truncate(LIST_LIMIT);

    let mut suspected_duplicates: Vec<SuspectedDuplicate> = by_shape
        .into_iter()
        .filter(|((_, _), paths)| paths.len() > 1)
        .map(|((size_bytes, modified_unix_ms), paths)| SuspectedDuplicate {
            copies: paths.len() as u64,
            size_bytes,
            modified_unix_ms,
            sample_paths: paths
                .into_iter()
                .take(SAMPLE_LIMIT)
                .collect::<Vec<String>>(),
        })
        .collect();
    suspected_duplicates.sort_by(|left, right| {
        let left_weight = left.copies.saturating_mul(left.size_bytes);
        let right_weight = right.copies.saturating_mul(right.size_bytes);
        right_weight
            .cmp(&left_weight)
            .then_with(|| right.copies.cmp(&left.copies))
    });
    suspected_duplicates.truncate(LIST_LIMIT);

    let mut largest_files: Vec<LargestFile> = snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .map(|entry| LargestFile {
            relative_path: entry.relative_path.clone(),
            apparent_bytes: entry.size_bytes,
            allocated_bytes: entry.allocated_bytes,
            link_count: entry.link_count,
        })
        .collect();
    largest_files.sort_by(|left, right| {
        right
            .allocated_bytes
            .cmp(&left.allocated_bytes)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    largest_files.truncate(LIST_LIMIT);

    let mut link_count_histogram: Vec<(u32, u64)> = histogram.into_iter().collect();
    link_count_histogram.sort_unstable();

    let mut volume_serials: Vec<(u32, u64)> = serials.into_iter().collect();
    volume_serials.sort_unstable();

    let mut unreadable_directories: Vec<String> = snapshot
        .errors
        .iter()
        .map(|error| format!("{} | {}", error.path, error.message))
        .collect();
    unreadable_directories.sort();
    unreadable_directories.truncate(LIST_LIMIT);

    let mut error_kinds: HashMap<String, u64> = HashMap::new();
    for error in snapshot.errors.iter() {
        *error_kinds.entry(error.message.clone()).or_default() += 1;
    }
    let mut error_kinds: Vec<(String, u64)> = error_kinds.into_iter().collect();
    error_kinds.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    AuditReport {
        root: snapshot.root.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
        captured_at_unix_ms: snapshot.captured_at_unix_ms,
        file_count: snapshot.file_count,
        files_without_identity: without_identity,
        distinct_records,
        records_with_multiple_paths: multi_path_records,
        apparent_bytes_all_paths: apparent_all_paths,
        apparent_bytes_distinct_records: apparent_distinct,
        allocated_bytes_distinct_records: allocated_distinct,
        allocated_bytes_single_link_records: allocated_single_link,
        link_count_histogram,
        volume_serials,
        placeholder_files,
        placeholder_apparent_bytes,
        sparse_files,
        sparse_apparent_bytes,
        estimated_files,
        estimated_apparent_bytes,
        unreadable_directories,
        error_kinds,
        largest_shared_records: largest_shared,
        largest_files,
        suspected_duplicates,
    }
}
