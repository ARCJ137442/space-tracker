mod delta;
mod interactive;
mod model;
mod output;
mod scanner;
mod store;
mod threshold;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::model::{StoredSnapshotSummary, absolute_path, root_key, target_relative_path};
use crate::threshold::GrowthThreshold;

#[derive(Debug, Parser)]
#[command(
    name = "space_tracker",
    version,
    about = "Read-only disk snapshots and growth analysis"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Scan {
        root: PathBuf,
        #[arg(long, default_value = ".space-tracker")]
        store: PathBuf,
        #[arg(long, help = "write compact JSON snapshots to reduce disk usage")]
        compact: bool,
        #[arg(long)]
        json: bool,
    },
    History {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, default_value = ".space-tracker")]
        store: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Delta {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        current: Option<PathBuf>,
        #[arg(long, default_value = ".space-tracker")]
        store: PathBuf,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(
            long,
            default_value = "0",
            value_name = "BYTES_OR_RATE",
            help = "minimum absolute growth or rate, for example 100M/d or 1M/s"
        )]
        min_bytes: String,
        #[arg(long)]
        json: bool,
    },
    Children {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value = ".space-tracker")]
        store: PathBuf,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Query {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value = ".space-tracker")]
        store: PathBuf,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Links {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long, default_value = ".space-tracker")]
        store: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    install_ctrl_c_handler();
    let interactive_mode = std::env::args_os().nth(1).is_none();
    let result = if interactive_mode {
        interactive::run()
    } else {
        run()
    };
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        if interactive_mode {
            interactive::pause_before_exit();
        }
        std::process::exit(1);
    }
    if interactive_mode {
        interactive::pause_before_exit();
    }
}

#[cfg(windows)]
fn install_ctrl_c_handler() {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::ExitProcess;

    unsafe extern "system" fn handler(ctrl_type: u32) -> windows_sys::core::BOOL {
        if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
            unsafe { ExitProcess(130) };
        }
        0
    }

    // ExitProcess is intentional here: Ctrl+C is an explicit request to
    // cancel a potentially long-running scan immediately.
    let _ = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}

#[cfg(not(windows))]
fn install_ctrl_c_handler() {}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Scan {
            root,
            store,
            compact,
            json,
        } => run_scan(&root, &store, compact, json),
        Commands::History { root, store, json } => run_history(root.as_deref(), &store, json),
        Commands::Delta {
            root,
            current,
            store,
            limit,
            min_bytes,
            json,
        } => run_delta(
            root.as_deref(),
            current.as_deref(),
            &store,
            limit,
            &min_bytes,
            json,
        ),
        Commands::Children {
            root,
            snapshot,
            path,
            store,
            limit,
            json,
        } => run_children(
            root.as_deref(),
            snapshot.as_deref(),
            &path,
            &store,
            limit,
            json,
        ),
        Commands::Query {
            root,
            snapshot,
            path,
            store,
            limit,
            json,
        } => run_children(
            root.as_deref(),
            snapshot.as_deref(),
            &path,
            &store,
            limit,
            json,
        ),
        Commands::Links {
            root,
            snapshot,
            store,
            json,
        } => run_links(root.as_deref(), snapshot.as_deref(), &store, json),
    }
}

fn run_scan(root: &Path, store_path: &Path, compact: bool, json: bool) -> Result<()> {
    let mut snapshot = scanner::scan(root)?;
    let store = store::SnapshotStore::new(absolute_path(store_path)?);
    let saved_path = store.save(&mut snapshot, compact)?;
    output::print_scan(&saved_path.to_string_lossy(), &snapshot, json)
}

fn run_history(root: Option<&Path>, store_path: &Path, json: bool) -> Result<()> {
    let store = store::SnapshotStore::new(absolute_path(store_path)?);
    let root_key_filter = root
        .map(|path| absolute_path(path).map(|path| root_key(&path)))
        .transpose()?;
    let items = store
        .list_summaries()?
        .into_iter()
        .filter(|item| {
            root_key_filter
                .as_deref()
                .is_none_or(|key| key == root_key(Path::new(&item.summary.root)))
        })
        .collect::<Vec<StoredSnapshotSummary>>();
    output::print_history(&items, json)
}

fn run_delta(
    root: Option<&Path>,
    current_path: Option<&Path>,
    store_path: &Path,
    limit: usize,
    minimum_threshold: &str,
    json: bool,
) -> Result<()> {
    let store = store::SnapshotStore::new(absolute_path(store_path)?);
    let (current, previous) = if let Some(current_path) = current_path {
        let current = store::StoredSnapshot {
            path: current_path.to_path_buf(),
            snapshot: store.load(current_path)?,
        };
        let previous = store
            .previous_for(&current.snapshot)?
            .context("no previous snapshot found for current snapshot")?;
        (current, previous)
    } else {
        let root = root.context("provide --root or --current")?;
        let root = absolute_path(root)?;
        let (current, previous) = store.require_previous_pair(&root)?;
        (current, previous)
    };
    let threshold = GrowthThreshold::parse(minimum_threshold)
        .map_err(|error| anyhow::anyhow!("无法解析 --min-bytes `{minimum_threshold}`：{error}"))?;
    let elapsed_ms = current
        .snapshot
        .captured_at_unix_ms
        .saturating_sub(previous.snapshot.captured_at_unix_ms);
    let minimum_bytes = threshold.effective_bytes(elapsed_ms);
    let report = delta::compare(&current.snapshot, &previous.snapshot, limit, minimum_bytes)?;
    output::print_delta(&report, json)
}

fn run_children(
    root: Option<&Path>,
    snapshot_path: Option<&Path>,
    requested_path: &Path,
    store_path: &Path,
    limit: usize,
    json: bool,
) -> Result<()> {
    let snapshot = load_snapshot(root, snapshot_path, store_path)?;
    let requested_relative = target_relative_path(&snapshot, requested_path)?;
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|entry| {
            !entry.relative_path.is_empty()
                && parent_relative(&entry.relative_path) == requested_relative
        })
        .cloned()
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    entries.truncate(limit);
    output::print_entries(&entries, &snapshot.root, json)
}

fn run_links(
    root: Option<&Path>,
    snapshot_path: Option<&Path>,
    store_path: &Path,
    json: bool,
) -> Result<()> {
    let snapshot = load_snapshot(root, snapshot_path, store_path)?;
    let entries = snapshot
        .entries
        .iter()
        .filter(|entry| entry.is_reparse_point)
        .cloned()
        .collect::<Vec<_>>();
    output::print_entries(&entries, &snapshot.root, json)
}

fn load_snapshot(
    root: Option<&Path>,
    snapshot_path: Option<&Path>,
    store_path: &Path,
) -> Result<model::Snapshot> {
    let store = store::SnapshotStore::new(absolute_path(store_path)?);
    if let Some(snapshot_path) = snapshot_path {
        return store.load(snapshot_path);
    }
    let root = root.context("provide --root or --snapshot")?;
    Ok(store.require_latest(&absolute_path(root)?)?.snapshot)
}

fn parent_relative(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(parent, _)| parent.to_string())
}
