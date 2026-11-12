//! Accounting for file records that appear under several paths.
//!
//! A directory's surface size is the sum of the apparent lengths of the files
//! below it. On NTFS two directory entries can be hard links to the same file
//! record, and the same bytes are then counted twice — or many times. A
//! volume that holds one large set of hard links can report a surface size
//! well past its physical capacity, and a directory can look far larger than
//! it is.
//!
//! This module settles the numbers so that they mean what they should mean:
//!
//! * `allocated_bytes` counts each file record once, under the path that owns
//!   it. The owner is the lexicographically smallest path among its links,
//!   which does not depend on the order the scan visited them, so two scans
//!   of the same tree agree. A parent's number is the sum of its children's.
//! * `shared_bytes` is the part of a directory's surface size that belongs to
//!   records owned elsewhere — the part that deleting it would not return.
//! * `reclaimable_bytes` is what deleting the directory would really give
//!   back to the volume. A record only counts for a directory that holds
//!   *every* link to it, because a record survives as long as one link does.
//!   That makes this number the closest common ancestor of the links rather
//!   than the sum of the children: a file linked from two sibling folders
//!   cannot be freed by either of them alone, only by their parent.
//!
//! A record with links outside the scanned tree — the same file also present
//! in the recycle bin, for instance — is never counted as reclaimable,
//! because no deletion inside the tree can remove it.

use std::collections::HashMap;

use crate::model::{Entry, EntryKind, FileId};

/// The totals a snapshot reports about the whole tree.
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    /// Allocation of every distinct file record below the root.
    pub allocated_bytes: u64,
    /// Space that deleting every file below the root would really free.
    pub reclaimable_bytes: u64,
}

/// What one file record looks like through the paths that reach it.
#[derive(Clone, Debug)]
struct Record {
    /// Space the record occupies on the volume.
    allocated: u64,
    /// Length of the record's contents.
    apparent: u64,
    /// Number of links the filesystem reports.
    link_count: u32,
    /// The paths that reach this record.
    paths: Vec<String>,
}

/// Rewrites the per-entry numbers of one snapshot in place.
pub fn apply(entries: &mut [Entry]) -> Totals {
    let mut records: HashMap<FileId, Record> = HashMap::new();
    let mut own_records: Vec<Record> = Vec::new();
    let mut directories: HashMap<String, usize> = HashMap::new();

    for (index, entry) in entries.iter().enumerate() {
        if entry.kind == EntryKind::Directory {
            directories.insert(entry.relative_path.clone(), index);
        }
    }

    for entry in entries.iter() {
        if entry.kind != EntryKind::File {
            continue;
        }
        let Some(id) = entry.file_id else {
            own_records.push(Record {
                allocated: entry.allocated_bytes,
                apparent: entry.size_bytes,
                link_count: entry.link_count,
                paths: vec![entry.relative_path.clone()],
            });
            continue;
        };
        let record = records.entry(id).or_insert_with(|| Record {
            allocated: entry.allocated_bytes,
            apparent: entry.size_bytes,
            link_count: entry.link_count,
            paths: Vec::new(),
        });
        record.allocated = record.allocated.max(entry.allocated_bytes);
        record.apparent = record.apparent.max(entry.size_bytes);
        record.link_count = record.link_count.max(entry.link_count);
        record.paths.push(entry.relative_path.clone());
    }

    let mut totals = Totals::default();
    let mut direct_allocated: HashMap<usize, u64> = HashMap::new();
    let mut direct_reclaimable: HashMap<usize, u64> = HashMap::new();
    let mut direct_shared: HashMap<usize, u64> = HashMap::new();

    for record in records.values().chain(own_records.iter()) {
        let owner = record.paths.iter().min().expect("record has a path");
        let owner_parent = parent_of(owner);
        let owner_index = directories.get(&owner_parent).copied();
        if let Some(index) = owner_index {
            *direct_allocated.entry(index).or_default() += record.allocated;
        }

        // The record can only be freed by removing every link to it, so it
        // counts for the deepest directory that holds all of them.
        let all_links_inside = record.link_count as usize <= record.paths.len();
        if all_links_inside {
            let common = common_ancestor(&record.paths);
            let index = common
                .as_ref()
                .and_then(|common| directories.get(common).copied());
            if let Some(index) = index {
                *direct_reclaimable.entry(index).or_default() += record.allocated;
            }
        }

        for path in &record.paths {
            if path == owner {
                continue;
            }
            let parent = parent_of(path);
            if let Some(index) = directories.get(&parent).copied() {
                *direct_shared.entry(index).or_default() += record.apparent;
            }
        }

        totals.allocated_bytes = totals.allocated_bytes.saturating_add(record.allocated);
        if all_links_inside {
            totals.reclaimable_bytes = totals.reclaimable_bytes.saturating_add(record.allocated);
        }
    }

    for (index, entry) in entries.iter_mut().enumerate() {
        if entry.kind != EntryKind::File {
            continue;
        }
        // A file with other links frees nothing on its own: the record
        // survives under its other names.
        entry.reclaimable_bytes = if entry.link_count <= 1 {
            entry.allocated_bytes
        } else {
            0
        };
        let owner_parent = parent_of(&entry.relative_path);
        let owned_elsewhere = entry
            .file_id
            .and_then(|id| records.get(&id))
            .and_then(|record| record.paths.iter().min())
            .is_some_and(|owner| parent_of(owner) != owner_parent);
        entry.shared_bytes = if owned_elsewhere {
            entry.size_bytes
        } else {
            0
        };
        let _ = index;
    }

    for (index, entry) in entries.iter_mut().enumerate() {
        if entry.kind != EntryKind::Directory {
            continue;
        }
        entry.allocated_bytes = direct_allocated.get(&index).copied().unwrap_or(0);
        entry.reclaimable_bytes = direct_reclaimable.get(&index).copied().unwrap_or(0);
        entry.shared_bytes = direct_shared.get(&index).copied().unwrap_or(0);
    }

    // A child's path is always longer than its parent's, so walking the
    // directories from the longest path to the shortest lets every child
    // hand its totals to its parent before the parent is handed onwards.
    let mut order: Vec<usize> = directories.values().copied().collect();
    order.sort_by(|left, right| {
        entries[*right]
            .relative_path
            .len()
            .cmp(&entries[*left].relative_path.len())
    });
    for index in order {
        let path = entries[index].relative_path.clone();
        let parent = parent_of(&path);
        if parent == path {
            continue;
        }
        let Some(&parent_index) = directories.get(&parent) else {
            continue;
        };
        let (allocated, reclaimable, shared) = {
            let entry = &entries[index];
            (entry.allocated_bytes, entry.reclaimable_bytes, entry.shared_bytes)
        };
        entries[parent_index].allocated_bytes =
            entries[parent_index].allocated_bytes.saturating_add(allocated);
        entries[parent_index].reclaimable_bytes =
            entries[parent_index].reclaimable_bytes.saturating_add(reclaimable);
        entries[parent_index].shared_bytes =
            entries[parent_index].shared_bytes.saturating_add(shared);
    }

    totals
}

/// The deepest directory that contains every one of the paths.
fn common_ancestor(paths: &[String]) -> Option<String> {
    let first = paths.first()?;
    let mut candidates = ancestor_chain(&parent_of(first));
    for path in paths.iter().skip(1) {
        candidates.retain(|candidate| is_ancestor(candidate, path));
        if candidates.is_empty() {
            break;
        }
    }
    candidates.into_iter().next()
}

/// The directories that hold a path, deepest first.
fn ancestor_chain(directory: &str) -> Vec<String> {
    let mut chain = vec![directory.to_string()];
    let mut current = directory;
    while let Some(index) = current.rfind('/') {
        current = &current[..index];
        chain.push(current.to_string());
    }
    if chain.last().map(String::as_str) != Some("") {
        chain.push(String::new());
    }
    chain
}

/// Whether a directory holds a path.
fn is_ancestor(directory: &str, path: &str) -> bool {
    if directory.is_empty() {
        return true;
    }
    path.len() > directory.len() && path.starts_with(directory) && path.as_bytes()[directory.len()] == b'/'
}

fn parent_of(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(parent, _)| parent.to_string())
}

#[cfg(test)]
mod tests {
    use super::apply;
    use crate::model::{Entry, EntryKind, FileId};

    fn file(path: &str, apparent: u64, allocated: u64, links: u32, id: Option<u64>) -> Entry {
        Entry {
            relative_path: path.to_string(),
            kind: EntryKind::File,
            size_bytes: apparent,
            modified_unix_ms: None,
            is_reparse_point: false,
            link_target: None,
            followed: false,
            allocated_bytes: allocated,
            reclaimable_bytes: 0,
            shared_bytes: 0,
            link_count: links,
            file_id: id.map(|file_index| FileId {
                volume_serial: 7,
                file_index,
            }),
            is_sparse: false,
            is_placeholder: false,
            allocation_estimated: false,
        }
    }

    fn directory(path: &str) -> Entry {
        Entry {
            relative_path: path.to_string(),
            kind: EntryKind::Directory,
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
            allocation_estimated: false,
        }
    }

    #[test]
    fn hard_linked_file_is_counted_once_under_its_smallest_path() {
        let mut entries = vec![
            directory(""),
            directory("a"),
            directory("b"),
            file("a/shared.bin", 1000, 4096, 2, Some(1)),
            file("b/shared.bin", 1000, 4096, 2, Some(1)),
            file("a/own.bin", 500, 512, 1, Some(2)),
        ];
        let totals = apply(&mut entries);

        // One record of 4096 bytes and one of 512 bytes.
        assert_eq!(totals.allocated_bytes, 4608);
        // The shared record is freed by deleting the tree that holds both
        // links; the own record is freed by deleting "a".
        assert_eq!(totals.reclaimable_bytes, 4608);

        let find = |path: &str| {
            entries
                .iter()
                .find(|entry| entry.relative_path == path)
                .expect("entry")
        };

        // "a" owns the shared record because its path sorts first.
        assert_eq!(find("a").allocated_bytes, 4096 + 512);
        assert_eq!(find("a").shared_bytes, 0);

        // "b" only links to the shared record, so it owns nothing and shares
        // its whole surface size.
        assert_eq!(find("b").allocated_bytes, 0);
        assert_eq!(find("b").shared_bytes, 1000);

        // The root is the sum of its children.
        assert_eq!(find("").allocated_bytes, 4608);
        assert_eq!(find("").shared_bytes, 1000);
    }

    #[test]
    fn deleting_one_of_two_linked_folders_frees_nothing() {
        let mut entries = vec![
            directory(""),
            directory("a"),
            directory("b"),
            file("a/shared.bin", 1000, 4096, 2, Some(1)),
            file("b/shared.bin", 1000, 4096, 2, Some(1)),
        ];
        apply(&mut entries);

        let find = |path: &str| {
            entries
                .iter()
                .find(|entry| entry.relative_path == path)
                .expect("entry")
        };

        // Each folder holds one link; removing one of them leaves the record
        // alive in the other.
        assert_eq!(find("a").reclaimable_bytes, 0);
        assert_eq!(find("b").reclaimable_bytes, 0);
        // Their parent holds both links, so deleting it does free the record.
        assert_eq!(find("").reclaimable_bytes, 4096);
        // A single file that is linked elsewhere frees nothing on its own.
        assert_eq!(find("a/shared.bin").reclaimable_bytes, 0);
    }

    #[test]
    fn a_record_with_links_outside_the_tree_is_never_reclaimable() {
        let mut entries = vec![
            directory(""),
            directory("a"),
            // The filesystem reports three links, but only two are visible.
            file("a/shared.bin", 1000, 4096, 3, Some(1)),
            file("b/shared.bin", 1000, 4096, 3, Some(1)),
            directory("b"),
        ];
        let totals = apply(&mut entries);
        assert_eq!(totals.allocated_bytes, 4096);
        assert_eq!(totals.reclaimable_bytes, 0);
    }

    #[test]
    fn directory_totals_are_the_sum_of_their_children() {
        let mut entries = vec![
            directory(""),
            directory("a"),
            directory("a/b"),
            file("a/b/one.bin", 200, 4096, 1, Some(1)),
            file("a/two.bin", 300, 4096, 1, Some(2)),
        ];
        apply(&mut entries);

        let find = |path: &str| {
            entries
                .iter()
                .find(|entry| entry.relative_path == path)
                .expect("entry")
        };
        assert_eq!(find("a/b").allocated_bytes, 4096);
        assert_eq!(find("a").allocated_bytes, 8192);
        assert_eq!(find("").allocated_bytes, 8192);
        assert_eq!(find("a/b").reclaimable_bytes, 4096);
        assert_eq!(find("a").reclaimable_bytes, 8192);
        assert_eq!(find("").reclaimable_bytes, 8192);
    }
}
