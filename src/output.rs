use anyhow::Result;
use serde::Serialize;

use crate::model::{
    DeltaItem, DeltaReport, Entry, Snapshot, SnapshotSummary, StoredSnapshotSummary,
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
        println!(
            "{} | {} | {}{}",
            match entry.kind {
                crate::model::EntryKind::Directory => "dir",
                crate::model::EntryKind::File => "file",
            },
            format_bytes(entry.size_bytes),
            path,
            link
        );
    }
    Ok(())
}
