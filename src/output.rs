use anyhow::Result;
use serde::Serialize;

use crate::model::{
    DeltaItem, DeltaReport, Entry, ReclaimReport, Snapshot, SnapshotSummary,
    StoredSnapshotSummary,
};

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{} {}", bytes, UNITS[index])
    } else {
        format!("{value:.2} {}", UNITS[index])
    }
}

pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn print_scan(snapshot_path: &str, snapshot: &Snapshot, json: bool) -> Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "snapshot_path": snapshot_path,
            "summary": SnapshotSummary::from_snapshot(snapshot),
        }));
    }
    println!("snapshot: {snapshot_path}");
    print_summary(snapshot);
    Ok(())
}

pub fn print_summary(snapshot: &Snapshot) {
    println!("root: {}", snapshot.root);
    println!("size: {}", format_bytes(snapshot.total_bytes));
    println!("allocated: {}", format_bytes(snapshot.total_allocated_bytes));
    println!(
        "reclaimable: {}",
        format_bytes(snapshot.total_reclaimable_bytes)
    );
    println!("files: {}", snapshot.file_count);
    println!("directories: {}", snapshot.directory_count);
    println!(
        "reparse points not followed: {}",
        snapshot.reparse_point_count
    );
    println!("errors: {}", snapshot.errors.len());
}

pub fn print_history(items: &[StoredSnapshotSummary], json: bool) -> Result<()> {
    if json {
        return print_json(items);
    }
    if items.is_empty() {
        println!("no snapshots found");
        return Ok(());
    }
    for item in items {
        let summary = &item.summary;
        println!(
            "{} | {} | {} | files={} dirs={} errors={}",
            summary.snapshot_id,
            summary.root,
            format_bytes(summary.total_bytes),
            summary.file_count,
            summary.directory_count,
            summary.error_count
        );
        println!("  {}", item.snapshot_path);
    }
    Ok(())
}

pub fn print_delta(report: &DeltaReport, json: bool) -> Result<()> {
    if json {
        return print_json(report);
    }
    println!("root: {}", report.root);
    println!("previous: {}", report.previous_snapshot);
    println!("current: {}", report.current_snapshot);
    println!("elapsed: {} ms", report.elapsed_ms);
    print_delta_section("growing directories", &report.growing_directories);
    print_delta_section("shrinking directories", &report.shrinking_directories);
    print_delta_section("new files", &report.new_files);
    print_delta_section("removed files", &report.removed_files);
    Ok(())
}

fn print_delta_section(title: &str, items: &[DeltaItem]) {
    println!("{title}:");
    if items.is_empty() {
        println!("  (none)");
        return;
    }
    for item in items {
        println!(
            "  {:+} | {} | {} | {}",
            item.delta_bytes,
            format_bytes(item.current_bytes),
            item.relative_path,
            item.growth_per_day_bytes
                .map(|value| format!("{value:.0} B/day"))
                .unwrap_or_else(|| "n/a".to_string())
        );
    }
}

pub fn print_entries(entries: &[Entry], root: &str, json: bool) -> Result<()> {
    if json {
        return print_json(entries);
    }
    for entry in entries {
        let path = if entry.relative_path.is_empty() {
            root.to_string()
        } else {
            format!(
                "{}\\{}",
                root.trim_end_matches(['\\', '/']),
                entry.relative_path.replace('/', "\\")
            )
        };
        let link = entry
            .link_target
            .as_deref()
            .map(|target| format!(" -> {target}"))
            .unwrap_or_default();
        let kind_note = match (entry.is_placeholder, entry.is_sparse) {
            (true, _) => " [placeholder: data lives elsewhere]",
            (false, true) => " [sparse]",
            (false, false) => "",
        };
        println!(
            "{} | {} | alloc={} | free={} | shared={} | links={} | {}{}{}",
            match entry.kind {
                crate::model::EntryKind::Directory => "dir",
                crate::model::EntryKind::File => "file",
            },
            format_bytes(entry.size_bytes),
            format_bytes(entry.allocated_bytes),
            format_bytes(entry.reclaimable_bytes),
            format_bytes(entry.shared_bytes),
            entry.link_count,
            path,
            link,
            kind_note
        );
    }
    Ok(())
}

pub fn print_reclaim(report: &ReclaimReport, json: bool) -> Result<()> {
    if json {
        return print_json(report);
    }
    println!("path: {}", report.path);
    println!("apparent: {}", format_bytes(report.apparent_bytes));
    println!("allocated: {}", format_bytes(report.allocated_bytes));
    println!("reclaimable: {}", format_bytes(report.reclaimable_bytes));
    println!("shared: {}", format_bytes(report.shared_bytes));
    if report.link_count > 0 {
        println!("hard links: {}", report.link_count);
    }
    println!("snapshot: {}", report.snapshot_id);
    Ok(())
}

pub fn print_audit(report: &crate::audit::AuditReport, json: bool) -> Result<()> {
    if json {
        return print_json(report);
    }
    println!("root: {}", report.root);
    println!("snapshot: {}", report.snapshot_id);
    println!("files: {}", report.file_count);
    println!("distinct records: {}", report.distinct_records);
    println!("files without identity: {}", report.files_without_identity);
    println!("records under several paths: {}", report.records_with_multiple_paths);
    println!(
        "apparent (all paths): {}",
        format_bytes(report.apparent_bytes_all_paths)
    );
    println!(
        "apparent (one per record): {}",
        format_bytes(report.apparent_bytes_distinct_records)
    );
    println!(
        "allocated (one per record): {}",
        format_bytes(report.allocated_bytes_distinct_records)
    );
    println!(
        "allocated (single-link records): {}",
        format_bytes(report.allocated_bytes_single_link_records)
    );
    println!("link count histogram:");
    for (links, count) in &report.link_count_histogram {
        println!("  {links} links | {count} files");
    }
    println!("volume serials seen:");
    for (serial, count) in &report.volume_serials {
        println!("  {serial} | {count} files");
    }
    println!(
        "placeholders (data not on this volume): {} files | {} of surface size",
        report.placeholder_files,
        format_bytes(report.placeholder_apparent_bytes)
    );
    println!(
        "sparse files: {} files | {} of surface size",
        report.sparse_files,
        format_bytes(report.sparse_apparent_bytes)
    );
    if !report.unreadable_directories.is_empty() {
        println!("unreadable paths:");
        for path in &report.unreadable_directories {
            println!("  {path}");
        }
    }
    if !report.largest_shared_records.is_empty() {
        println!("largest records under several paths:");
        for record in &report.largest_shared_records {
            println!(
                "  {} | {} paths | links={} | {}",
                format_bytes(record.allocated_bytes),
                record.paths,
                record.link_count,
                record.sample_paths.join(" | ")
            );
        }
    }
    if !report.largest_files.is_empty() {
        println!("largest files:");
        for file in &report.largest_files {
            println!(
                "  {} | apparent {} | links={} | {}",
                format_bytes(file.allocated_bytes),
                format_bytes(file.apparent_bytes),
                file.link_count,
                file.relative_path
            );
        }
    }
    if !report.suspected_duplicates.is_empty() {
        println!("suspected duplicates (same size and write time, different records):");
        for duplicate in &report.suspected_duplicates {
            println!(
                "  {} copies | {} | {}",
                duplicate.copies,
                format_bytes(duplicate.size_bytes),
                duplicate.sample_paths.join(" | ")
            );
        }
    }
    Ok(())
}
