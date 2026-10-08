mod app;
mod cache;
mod delete;
mod diff;
mod fmt;
mod scan;
mod snapshot;
mod tree;
mod treemap;
mod ui;

use anyhow::bail;
use app::App;
use clap::Parser;
use scan::{Progress, ScanOptions};
use snapshot::Meta;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

/// Fast interactive disk usage analyzer.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Directory to scan
    #[arg(default_value = ".", conflicts_with = "load")]
    path: PathBuf,

    /// Open a saved snapshot instead of scanning
    #[arg(short = 'f', long, value_name = "FILE")]
    load: Option<PathBuf>,

    /// Rescan the snapshot's root right after loading it
    #[arg(short, long, requires = "load")]
    refresh: bool,

    /// Snapshot file to save to (default target for `w` in the UI)
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Stay on the filesystem of the root; don't descend into other mounts
    #[arg(short = 'x', long)]
    one_file_system: bool,

    /// Number of scanner threads [default: 2x cores]
    #[arg(short = 'j', long)]
    threads: Option<usize>,

    /// Show apparent file sizes instead of disk usage
    #[arg(short = 'A', long)]
    apparent: bool,

    /// Skip entries matching this glob (repeatable). Patterns without '/'
    /// match names (e.g. 'node_modules', '*.o'); others match full paths
    /// (e.g. '/home/*/.cache')
    #[arg(short = 'e', long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// Compare against this snapshot (shown in the Changes view and tree)
    #[arg(short = 'd', long, value_name = "FILE")]
    diff: Option<PathBuf>,

    /// Disable deleting files from the UI
    #[arg(long)]
    no_delete: bool,

    /// Don't start the UI: scan (or refresh) and save the result to the
    /// cache (and --output, if given)
    #[arg(long)]
    no_ui: bool,

    /// Don't read or write automatic snapshots (in ~/.cache/dut)
    #[arg(long)]
    no_cache: bool,

    /// Open the automatic snapshot without rescanning
    #[arg(short = 'c', long, conflicts_with_all = ["no_cache", "load"])]
    cached: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut one_file_system = cli.one_file_system;
    let mut exclude = cli.exclude.clone();

    // Work out what to show: a snapshot, or a fresh scan of some root.
    let mut estimate = None;
    let mut baseline = None;
    let (mut meta, loaded) = match &cli.load {
        Some(file) => {
            let (entry, meta) = snapshot::load(file)?;
            one_file_system |= meta.one_file_system;
            for p in &meta.exclude {
                if !exclude.contains(p) {
                    exclude.push(p.clone());
                }
            }
            let entry = if cli.refresh {
                // The old totals make a good estimate for the rescan, and
                // the old tree is what we compare the new one against.
                estimate = Some(scan::Estimate {
                    items: entry.items,
                    disk: entry.disk,
                    source: "previous scan",
                });
                baseline = Some((entry, meta.scanned_at));
                None
            } else {
                Some(entry)
            };
            (meta, entry)
        }
        None => {
            let root = std::fs::canonicalize(&cli.path)
                .map_err(|e| anyhow::anyhow!("{}: {e}", cli.path.display()))?;
            let meta = Meta {
                root,
                one_file_system,
                scanned_at: app::now_unix(),
                scan_millis: 0,
                exclude: Vec::new(),
            };
            (meta, None)
        }
    };
    if let Some(file) = &cli.diff {
        let (entry, m) = snapshot::load(file)?;
        baseline = Some((entry, m.scanned_at));
    }
    meta.one_file_system = one_file_system;
    meta.exclude = exclude.clone();
    let opts = ScanOptions {
        one_file_system,
        threads: cli.threads.unwrap_or_else(scan::default_threads),
        exclude: std::sync::Arc::new(scan::Excludes::new(exclude)?),
    };
    let save_path = cli.output.clone().or_else(|| cli.load.clone());
    let cache_path = if cli.no_cache {
        None
    } else {
        cache::path_for(&meta.root, meta.one_file_system, &meta.exclude)
    };

    if cli.no_ui {
        let outs: Vec<PathBuf> = cli.output.iter().chain(&cache_path).cloned().collect();
        if outs.is_empty() {
            bail!("--no-ui needs --output, or the cache enabled");
        }
        return headless(meta, loaded, estimate, opts, outs);
    }

    // Without an explicit snapshot, start from the automatic one if present:
    // show it right away, refresh in the background, then compare.
    let mut stale = false;
    let mut loaded = loaded;
    if cli.load.is_none() {
        let cached = cache_path
            .as_deref()
            .filter(|p| p.exists())
            .and_then(|p| snapshot::load(p).ok());
        match cached {
            Some((entry, m)) => {
                meta.scanned_at = m.scanned_at;
                meta.scan_millis = m.scan_millis;
                loaded = Some(entry);
                stale = !cli.cached;
            }
            None if cli.cached => bail!("no cached snapshot for {}", meta.root.display()),
            None => {}
        }
    }

    let mut app = App::new(meta, opts, !cli.apparent, save_path);
    app.initial_estimate = estimate;
    app.allow_delete = !cli.no_delete;
    app.pending_baseline = baseline;
    app.cache_path = cache_path;
    match loaded {
        Some(entry) => {
            app.set_tree(entry);
            if stale {
                app.stale = true;
                app.start_scan(None);
            }
        }
        None => app.start_scan(None),
    }
    let mut terminal = ratatui::init();
    let res = app.run(&mut terminal);
    ratatui::restore();
    res
}

fn headless(
    mut meta: Meta,
    loaded: Option<tree::Entry>,
    estimate: Option<scan::Estimate>,
    opts: ScanOptions,
    outs: Vec<PathBuf>,
) -> anyhow::Result<()> {
    let entry = match loaded {
        Some(e) => e,
        None => {
            let estimate = estimate.or_else(|| {
                // Excluded entries would make a filesystem-wide estimate wrong.
                opts.exclude
                    .is_empty()
                    .then(|| scan::estimate_fs(&meta.root, opts.one_file_system))
                    .flatten()
            });
            let progress = Progress::with_estimate(estimate);
            let t0 = Instant::now();
            let tty = std::io::stderr().is_terminal();
            let res = std::thread::scope(|s| {
                let h = s.spawn(|| scan::scan(&meta.root, opts.clone(), &progress));
                let mut tick = 0u64;
                while !h.is_finished() {
                    if tty && tick.is_multiple_of(10) {
                        let eta = match progress.eta(t0.elapsed()) {
                            Some((frac, eta)) => format!(
                                " · {:.0}%{}",
                                frac * 100.0,
                                eta.map_or(String::new(), |d| format!(
                                    " · ~{} left",
                                    fmt::duration_ms(d.as_millis() as u64)
                                ))
                            ),
                            None => String::new(),
                        };
                        eprint!(
                            "\r\x1b[Kscanning {}: {} items, {}{eta}",
                            meta.root.display(),
                            fmt::count(progress.items.load(Relaxed)),
                            fmt::size(progress.disk.load(Relaxed)),
                        );
                        let _ = std::io::stderr().flush();
                    }
                    tick += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                if tty {
                    eprint!("\r\x1b[K");
                }
                h.join().unwrap()
            });
            let e = res?;
            meta.scanned_at = app::now_unix();
            meta.scan_millis = t0.elapsed().as_millis() as u64;
            let errors = progress.errors.load(Relaxed);
            eprintln!(
                "scanned {}: {} items, {} on disk ({} apparent) in {}{}",
                meta.root.display(),
                fmt::count(e.items),
                fmt::size(e.disk),
                fmt::size(e.size),
                fmt::duration_ms(meta.scan_millis),
                if errors > 0 {
                    format!(", {errors} errors")
                } else {
                    String::new()
                }
            );
            e
        }
    };
    if entry.items == 0 {
        bail!("nothing scanned");
    }
    let tree = tree::Tree::from_entry(entry, tree::SortKey::Disk);
    for out in outs {
        cache::ensure_dir(&out)?;
        snapshot::save(&out, &tree, &meta)?;
        eprintln!("saved {}", out.display());
    }
    Ok(())
}
