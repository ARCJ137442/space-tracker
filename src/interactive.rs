use anyhow::{Context, Result, bail};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::delta;
use crate::model::{DeltaItem, DeltaReport, EntryKind, Snapshot, absolute_path};
use crate::output;
use crate::scanner::{self, ScanProgress};
use crate::store::SnapshotStore;
use crate::threshold::GrowthThreshold;

const DEFAULT_GROWTH_THRESHOLD: GrowthThreshold = GrowthThreshold::Absolute(100 * 1024 * 1024);
const ATTENTION_LIMIT: usize = 12;

pub fn pause_before_exit() {
    println!();
    println!("按任意键退出……");
    let _ = io::stdout().flush();
    wait_for_any_key();
}

#[cfg(windows)]
fn wait_for_any_key() {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, FlushConsoleInputBuffer,
        GetConsoleMode, GetStdHandle, INPUT_RECORD, KEY_EVENT, LEFT_CTRL_PRESSED,
        RIGHT_CTRL_PRESSED, ReadConsoleInputW, STD_INPUT_HANDLE, SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::ExitProcess;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        wait_for_enter();
        return;
    }

    let mut original_mode = 0;
    if unsafe { GetConsoleMode(handle, &mut original_mode) } == 0 {
        wait_for_enter();
        return;
    }

    let raw_mode =
        original_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
    if unsafe { SetConsoleMode(handle, raw_mode) } == 0 {
        wait_for_enter();
        return;
    }
    // The Enter key used for the final prompt can still be queued as a
    // console event. Discard all pre-existing events so only a new key exits.
    if unsafe { FlushConsoleInputBuffer(handle) } == 0 {
        let _ = unsafe { SetConsoleMode(handle, original_mode) };
        wait_for_enter();
        return;
    }

    loop {
        let mut record = MaybeUninit::<INPUT_RECORD>::uninit();
        let mut records_read = 0;
        if unsafe { ReadConsoleInputW(handle, record.as_mut_ptr(), 1, &mut records_read) } == 0 {
            break;
        }
        if records_read == 0 {
            continue;
        }

        let record = unsafe { record.assume_init() };
        if u32::from(record.EventType) == KEY_EVENT {
            let key_event = unsafe { record.Event.KeyEvent };
            let ctrl_pressed =
                key_event.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
            if ctrl_pressed && key_event.wVirtualKeyCode == 0x43 {
                unsafe { ExitProcess(130) };
            }
            if key_event.bKeyDown != 0 {
                break;
            }
        }
    }

    let _ = unsafe { SetConsoleMode(handle, original_mode) };
}

#[cfg(not(windows))]
fn wait_for_any_key() {
    wait_for_enter();
}

fn wait_for_enter() {
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
}

pub fn run() -> Result<()> {
    println!("space_tracker - 人类友好的磁盘空间扫描");
    println!("直接回车使用默认值；扫描只读磁盘，不会删除或移动文件。\n");

    let default_root = PathBuf::from(r"C:\");
    let default_store = default_store_path()?;
    let root = prompt_existing_directory("扫描路径", &default_root)?;
    let store_path = prompt_store_directory("数据存放路径", &default_store)?;
    let threshold = prompt_threshold(DEFAULT_GROWTH_THRESHOLD)?;

    println!();
    println!("扫描路径：{}", root.display());
    println!("数据存放：{}", store_path.display());
    println!(
        "增长提醒：大于 {} 的目录",
        format_threshold_for_prompt(threshold)
    );
    println!("正在建立进度基线……");
    let estimate_started = Instant::now();
    let estimate = scanner::estimate(&root)?;
    println!(
        "进度基线：{} 个条目（{} 个文件，{} 个目录），逻辑大小约 {}，耗时 {}",
        estimate.total_entries,
        estimate.file_count,
        estimate.directory_count,
        output::format_bytes(estimate.total_bytes),
        format_duration(estimate_started.elapsed().as_millis().min(u64::MAX as u128) as u64,),
    );
    if estimate.error_count > 0 {
        println!(
            "进度基线有 {} 个路径无法读取，最终结果以正式扫描为准。",
            estimate.error_count
        );
    }
    println!("开始扫描……");

    let total_entries = estimate.total_entries;
    let mut snapshot = scanner::scan_with_progress(&root, move |progress| {
        render_progress(progress, total_entries)
    })?;
    println!();
    println!("正在保存快照……");
    let store = SnapshotStore::new(absolute_path(&store_path)?);
    let saved_path = store.save(&mut snapshot, true)?;
    println!();
    print_result(&snapshot, &saved_path, &store, threshold)?;
    Ok(())
}

fn default_store_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("无法确定 exe 所在目录")?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("无法确定 exe 所在目录"))
}

fn prompt_existing_directory(label: &str, default: &Path) -> Result<PathBuf> {
    loop {
        let value = prompt(label, &default.display().to_string())?;
        let path = absolute_path(Path::new(&value))?;
        if path.is_dir() {
            return Ok(path);
        }
        eprintln!("路径不存在或不是目录，请重新输入：{}", path.display());
    }
}

fn prompt_store_directory(label: &str, default: &Path) -> Result<PathBuf> {
    loop {
        let value = prompt(label, &default.display().to_string())?;
        let path = absolute_path(Path::new(&value))?;
        if path.exists() && !path.is_dir() {
            eprintln!("数据存放路径已经是文件，请重新输入：{}", path.display());
            continue;
        }
        return Ok(path);
    }
}

fn prompt_threshold(default: GrowthThreshold) -> Result<GrowthThreshold> {
    loop {
        let value = prompt(
            "增长提示阈值（例如 100M、1G、100M/d、1M/s）",
            &format_threshold_for_prompt(default),
        )?;
        match GrowthThreshold::parse(&value) {
            Ok(threshold) => return Ok(threshold),
            Err(error) => eprintln!("{}，请重新输入。", error),
        }
    }
}

fn prompt(label: &str, default: &str) -> Result<String> {
    print!("{} [{}]: ", label, default);
    io::stdout().flush().context("无法刷新命令行提示")?;
    let mut input = String::new();
    let read = io::stdin()
        .read_line(&mut input)
        .context("无法读取命令行输入")?;
    if read == 0 {
        bail!("输入已结束，交互式扫描取消");
    }
    let value = input.trim();
    if value.is_empty() {
        return Ok(default.to_string());
    }
    Ok(value.trim_matches(['"', '\'']).to_string())
}

fn render_progress(progress: ScanProgress, total_entries: u64) {
    let current = shorten(&progress.current_path, 86);
    let processed_entries = progress.file_count.saturating_add(progress.directory_count);
    let percent = if total_entries == 0 {
        100.0
    } else {
        (processed_entries as f64 * 100.0 / total_entries as f64).min(100.0)
    };
    print!(
        "\r扫描中 | {:>6.2}% ({:>8}/{:<8}) | 用时 {:>8} | 文件 {:>8} | 目录 {:>7} | 数据 {:>10} | 联接 {:>4} | 错误 {:>4} | {}",
        percent,
        processed_entries,
        total_entries,
        format_duration(progress.elapsed_ms),
        progress.file_count,
        progress.directory_count,
        output::format_bytes(progress.total_bytes),
        progress.reparse_point_count,
        progress.error_count,
        current
    );
    let _ = io::stdout().flush();
}

fn print_result(
    snapshot: &Snapshot,
    saved_path: &Path,
    store: &SnapshotStore,
    threshold: GrowthThreshold,
) -> Result<()> {
    println!("扫描完成：{}", snapshot.root);
    output::print_summary(snapshot);
    println!("duration: {} ms", snapshot.duration_ms);
    println!("snapshot: {}", saved_path.display());

    let report = if let Some(previous) = store.previous_for(snapshot)? {
        let elapsed_ms = snapshot
            .captured_at_unix_ms
            .saturating_sub(previous.snapshot.captured_at_unix_ms);
        let effective_threshold = threshold.effective_bytes(elapsed_ms);
        Some((
            delta::compare(
                snapshot,
                &previous.snapshot,
                usize::MAX,
                effective_threshold.saturating_add(1),
            )?,
            effective_threshold,
        ))
    } else {
        None
    };

    print_attention_paths(snapshot);
    if !snapshot.errors.is_empty() {
        println!();
        println!(
            "注意：有 {} 个路径读取失败，详情已写入快照。",
            snapshot.errors.len()
        );
    }
    match report {
        Some((report, effective_threshold)) => {
            print_growth_alerts(snapshot, &report, threshold, effective_threshold)
        }
        None => {
            println!();
            println!("增长提醒：这是该路径的第一次扫描；再次扫描后将显示超过阈值的增长目录。",);
        }
    }
    Ok(())
}

fn print_growth_alerts(
    snapshot: &Snapshot,
    report: &DeltaReport,
    threshold: GrowthThreshold,
    effective_threshold: u64,
) {
    println!();
    println!(
        "增长提醒（过去 {}，{}）：",
        format_duration(report.elapsed_ms),
        format_threshold_description(threshold, effective_threshold)
    );
    let mut alerts = focus_growth_alerts(&report.growing_directories)
        .into_iter()
        .filter(|item| is_actionable_path(&item.relative_path))
        .collect::<Vec<_>>();
    if alerts.is_empty() {
        println!("  没有发现超过阈值的可关注目录（系统目录已隐藏）。");
        return;
    }
    alerts.sort_by(|left, right| {
        left.delta_bytes
            .cmp(&right.delta_bytes)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    alerts.truncate(ATTENTION_LIMIT);
    for item in alerts {
        println!(
            "  +{:>10} | 当前 {:>10} | {}",
            output::format_bytes(item.delta_bytes as u64),
            output::format_bytes(item.current_bytes),
            display_path(snapshot, &item.relative_path)
        );
    }
}

fn focus_growth_alerts(items: &[DeltaItem]) -> Vec<&DeltaItem> {
    items
        .iter()
        .filter(|item| {
            !item.relative_path.is_empty()
                && !items.iter().any(|candidate| {
                    candidate.relative_path != item.relative_path
                        && is_descendant_path(&candidate.relative_path, &item.relative_path)
                })
        })
        .collect()
}

fn is_descendant_path(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn print_attention_paths(snapshot: &Snapshot) {
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == EntryKind::Directory
                && !entry.relative_path.is_empty()
                && !entry.is_reparse_point
                && is_actionable_path(&entry.relative_path)
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    entries.truncate(ATTENTION_LIMIT);

    println!();
    println!("值得关注的当前占用路径（已隐藏 Windows/程序安装目录）：");
    if entries.is_empty() {
        println!("  没有可展示的目录。");
        return;
    }
    for entry in entries {
        println!(
            "  {:>10} | {}",
            output::format_bytes(entry.size_bytes),
            display_path(snapshot, &entry.relative_path)
        );
    }
}

fn is_actionable_path(relative: &str) -> bool {
    let first = relative.split('/').next().unwrap_or_default();
    !matches!(
        first.to_ascii_lowercase().as_str(),
        "windows"
            | "program files"
            | "program files (x86)"
            | "recovery"
            | "system volume information"
    )
}

fn display_path(snapshot: &Snapshot, relative: &str) -> String {
    if relative.is_empty() {
        snapshot.root.clone()
    } else {
        format!(
            "{}\\{}",
            snapshot.root.trim_end_matches(['\\', '/']),
            relative.replace('/', "\\")
        )
    }
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1} 秒");
    }
    let total_seconds = milliseconds / 1000;
    let minutes = total_seconds / 60;
    let remaining = total_seconds % 60;
    format!("{minutes} 分 {remaining} 秒")
}

fn format_bytes_for_prompt(bytes: u64) -> String {
    if bytes.is_multiple_of(1024 * 1024) {
        format!("{}M", bytes / (1024 * 1024))
    } else {
        bytes.to_string()
    }
}

fn format_threshold_for_prompt(threshold: GrowthThreshold) -> String {
    match threshold {
        GrowthThreshold::Absolute(bytes) => format_bytes_for_prompt(bytes),
        GrowthThreshold::Rate {
            bytes_per_unit,
            unit,
        } => format!(
            "{}/{}",
            format_bytes_for_prompt(bytes_per_unit),
            unit.suffix()
        ),
    }
}

fn format_threshold_description(threshold: GrowthThreshold, effective_bytes: u64) -> String {
    match threshold {
        GrowthThreshold::Absolute(_) => {
            format!("阈值 > {}", output::format_bytes(effective_bytes))
        }
        GrowthThreshold::Rate {
            bytes_per_unit,
            unit,
        } => format!(
            "速度阈值 > {}/{}，本次等效阈值 > {}",
            output::format_bytes(bytes_per_unit),
            unit.suffix(),
            output::format_bytes(effective_bytes)
        ),
    }
}

fn shorten(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    let tail = chars[chars.len() - max_chars.saturating_sub(3)..]
        .iter()
        .collect::<String>();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::{focus_growth_alerts, format_threshold_for_prompt, is_actionable_path};
    use crate::model::DeltaItem;
    use crate::threshold::{GrowthThreshold, parse_size_bytes};

    #[test]
    fn parses_human_size_suffixes() {
        assert_eq!(parse_size_bytes("100M"), Ok(100 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1.5GiB"), Ok(1_610_612_736));
        assert_eq!(parse_size_bytes("42"), Ok(42));
    }

    #[test]
    fn formats_rate_threshold_for_prompt() {
        let threshold = GrowthThreshold::parse("100M/d").expect("parse rate threshold");
        assert_eq!(format_threshold_for_prompt(threshold), "100M/d");
    }

    #[test]
    fn hides_system_roots_but_keeps_user_data() {
        assert!(!is_actionable_path("Windows/Temp"));
        assert!(!is_actionable_path("Program Files/Tool"));
        assert!(is_actionable_path("Users/alice/AppData"));
        assert!(is_actionable_path("ProgramData/cache"));
    }

    #[test]
    fn focuses_on_deepest_growth_directories() {
        let items = vec![
            delta_item("Users", 400),
            delta_item("Users/alice", 300),
            delta_item("Users/alice/AppData", 200),
            delta_item("ProgramData", 180),
            delta_item("Users/alice2", 70),
        ];
        let focused = focus_growth_alerts(&items)
            .into_iter()
            .map(|item| item.relative_path.as_str())
            .collect::<Vec<_>>();

        assert!(focused.contains(&"Users/alice/AppData"));
        assert!(focused.contains(&"ProgramData"));
        assert!(focused.contains(&"Users/alice2"));
        assert!(!focused.contains(&"Users"));
        assert!(!focused.contains(&"Users/alice"));
    }

    #[test]
    fn path_prefix_without_separator_is_not_a_descendant() {
        let items = vec![delta_item("cache", 200), delta_item("cache-old", 150)];
        let focused = focus_growth_alerts(&items);
        assert_eq!(focused.len(), 2);
    }

    fn delta_item(relative_path: &str, delta_bytes: i64) -> DeltaItem {
        DeltaItem {
            relative_path: relative_path.to_string(),
            current_bytes: delta_bytes.max(0) as u64,
            previous_bytes: 0,
            delta_bytes,
            delta_percent: None,
            growth_per_day_bytes: None,
        }
    }
}
