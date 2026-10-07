mod app;
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

    /// Don't start the UI: scan (or refresh) and write the snapshot to --output
    #[arg(long, requires = "output")]
    no_ui: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut opts = ScanOptions {
        one_file_system: cli.one_file_system,
        threads: cli.threads.unwrap_or_else(scan::default_threads),
    };

    // Work out what to show: a snapshot, or a fresh scan of some root.
    let mut estimate = None;
    let (meta, loaded) = match &cli.load {
        Some(file) => {
            let (entry, meta) = snapshot::load(file)?;
            opts.one_file_system |= meta.one_file_system;
            let entry = if cli.refresh {
                // The old totals make a good estimate for the rescan.
                estimate = Some(scan::Estimate {
                    items: entry.items,
                    disk: entry.disk,
                    source: "previous scan",
                });
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
                one_file_system: opts.one_file_system,
                scanned_at: app::now_unix(),
                scan_millis: 0,
            };
            (meta, None)
        }
    };
    let save_path = cli.output.clone().or_else(|| cli.load.clone());

    if cli.no_ui {
        return headless(meta, loaded, estimate, opts, cli.output.unwrap());
    }

    let mut app = App::new(meta, opts, !cli.apparent, save_path);
    app.initial_estimate = estimate;
    match loaded {
        Some(entry) => app.set_tree(entry),
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
    out: PathBuf,
) -> anyhow::Result<()> {
    let entry = match loaded {
        Some(e) => e,
        None => {
            let estimate = estimate.or_else(|| scan::estimate_fs(&meta.root, opts.one_file_system));
            let progress = Progress::with_estimate(estimate);
            let t0 = Instant::now();
            let tty = std::io::stderr().is_terminal();
            let res = std::thread::scope(|s| {
                let h = s.spawn(|| scan::scan(&meta.root, opts, &progress));
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
    snapshot::save(&out, &tree, &meta)?;
    eprintln!("saved {}", out.display());
    Ok(())
}
