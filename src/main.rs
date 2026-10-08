mod app;
mod cache;
mod delete;
mod diff;
mod fmt;
#[cfg(target_os = "macos")]
mod macos;
mod mounts;
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
    /// Directory to scan [default: .]
    #[arg(conflicts_with = "load")]
    path: Option<PathBuf>,

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

    /// Also descend into virtual (/proc, /sys, /dev) and network (NFS, SMB,
    /// sshfs) filesystems, which are skipped by default
    #[arg(long, conflicts_with = "one_file_system")]
    all_mounts: bool,

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

    /// Don't read or write automatic snapshots (in ~/.cache/dut, or
    /// ~/Library/Caches/dut on macOS)
    #[arg(long)]
    no_cache: bool,

    /// Open the automatic snapshot without rescanning
    #[arg(short = 'c', long, conflicts_with_all = ["no_cache", "load"])]
    cached: bool,

    /// List the automatic snapshots and exit
    #[arg(long)]
    cache_list: bool,

    /// Delete the automatic snapshots of PATH (or all of them) and exit
    #[arg(long)]
    clear_cache: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.cache_list {
        return cache_list();
    }
    if cli.clear_cache {
        let root = cli.path.as_deref().map(std::fs::canonicalize).transpose()?;
        let (n, bytes) = cache::clear(root.as_deref());
        eprintln!("removed {n} snapshots ({})", fmt::size(bytes));
        return Ok(());
    }

    let sort = if cli.apparent {
        tree::SortKey::Size
    } else {
        tree::SortKey::Disk
    };
    let mut one_file_system = cli.one_file_system;
    let mut all_mounts = cli.all_mounts;
    let mut exclude = cli.exclude.clone();

    // What to show first: an explicit snapshot, a cached one, or a fresh scan.
    let mut loaded: Option<tree::Tree> = None;
    let mut baseline = None;
    let mut meta = match &cli.load {
        Some(file) => {
            let (tree, meta) = snapshot::load(file, sort)?;
            one_file_system |= meta.one_file_system;
            all_mounts |= meta.all_mounts;
            for p in &meta.exclude {
                if !exclude.contains(p) {
                    exclude.push(p.clone());
                }
            }
            if cli.refresh {
                // The old tree is what the rescan gets compared against.
                baseline = Some((tree, meta.scanned_at));
            } else {
                loaded = Some(tree);
            }
            meta
        }
        None => {
            let path = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
            Meta {
                root: std::fs::canonicalize(&path)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
                one_file_system,
                all_mounts,
                scanned_at: app::now_unix(),
                scan_millis: 0,
                exclude: Vec::new(),
            }
        }
    };
    if let Some(file) = &cli.diff {
        let (tree, m) = snapshot::load(file, tree::SortKey::Name)?;
        baseline = Some((tree, m.scanned_at));
    }
    // -x is stricter, and wins over a snapshot's --all-mounts.
    all_mounts &= !one_file_system;
    meta.one_file_system = one_file_system;
    meta.all_mounts = all_mounts;
    meta.exclude = exclude.clone();
    let opts = ScanOptions {
        one_file_system,
        all_mounts,
        threads: cli.threads.unwrap_or_else(scan::default_threads),
        exclude: std::sync::Arc::new(scan::Excludes::new(exclude.clone())?),
        aliases: std::sync::Arc::new(scan::aliases(&meta.root)),
    };
    let use_cache = !cli.no_cache && cache::dir().is_some();

    if cli.no_ui {
        if cli.output.is_none() && !use_cache {
            bail!("--no-ui needs --output, or the cache enabled");
        }
        let estimate = baseline.as_ref().map(|(t, _)| estimate_from(t));
        return headless(meta, loaded, estimate, opts, cli.output.clone(), use_cache);
    }

    // Without an explicit snapshot, start from the cached one if there is
    // one: show it right away, refresh in the background, then compare.
    // Failing that, a cached ancestor's subtree will do.
    let mut stale_source = None;
    if cli.load.is_none() && use_cache {
        let own = cache::Key::of(&meta)
            .latest()
            .and_then(|p| snapshot::load(&p, sort).ok());
        let cached = own.or_else(|| {
            let (file, m) =
                cache::find_ancestor(&meta.root, one_file_system, all_mounts, &exclude)?;
            let sub = snapshot::load_tree(&file, sort, Some(&meta.root), None).ok()?;
            stale_source = Some(m.root);
            Some(sub)
        });
        match cached {
            Some((tree, m)) => {
                meta.scanned_at = m.scanned_at;
                meta.scan_millis = m.scan_millis;
                loaded = Some(tree);
            }
            None if cli.cached => bail!("no cached snapshot for {}", meta.root.display()),
            None => {}
        }
    }
    let stale = cli.load.is_none() && loaded.is_some() && !cli.cached;

    let save_path = cli.output.clone().or_else(|| cli.load.clone());
    let mut app = App::new(meta, opts, !cli.apparent, save_path);
    app.allow_delete = !cli.no_delete;
    app.use_cache = use_cache;
    if let Some((t, time)) = baseline {
        if loaded.is_none() {
            app.initial_estimate = Some(estimate_from(&t));
        }
        app.pending_baseline = Some((t, time));
    }
    match loaded {
        Some(tree) => {
            app.set_tree(tree);
            if stale {
                app.stale = true;
                app.stale_source = stale_source;
                app.start_scan(None);
            }
        }
        None => app.start_scan(None),
    }
    let mut terminal = ratatui::init();
    let res = app.run(&mut terminal);
    ratatui::restore();
    res?;
    app.finish()
}

fn estimate_from(t: &tree::Tree) -> scan::Estimate {
    let r = t.root_node();
    scan::Estimate {
        items: r.items,
        disk: r.disk,
        source: "previous scan",
    }
}

fn cache_list() -> anyhow::Result<()> {
    let list = cache::list();
    if list.is_empty() {
        eprintln!(
            "no cached snapshots in {}",
            cache::dir().unwrap_or_default().display()
        );
        return Ok(());
    }
    let mut total = 0;
    for l in &list {
        total += l.bytes;
        let mut opts = String::new();
        if l.meta.one_file_system {
            opts += " -x";
        }
        if l.meta.all_mounts {
            opts += " --all-mounts";
        }
        for e in &l.meta.exclude {
            opts += &format!(" -e {e}");
        }
        println!(
            "{:>9}  {:>9}  {}{}",
            fmt::ago(l.meta.scanned_at),
            fmt::size(l.bytes),
            l.meta.root.display(),
            opts
        );
    }
    println!(
        "{} snapshots, {} in {}",
        list.len(),
        fmt::size(total),
        cache::dir().unwrap_or_default().display()
    );
    Ok(())
}

fn headless(
    mut meta: Meta,
    loaded: Option<tree::Tree>,
    estimate: Option<scan::Estimate>,
    opts: ScanOptions,
    output: Option<PathBuf>,
    use_cache: bool,
) -> anyhow::Result<()> {
    let tree = match loaded {
        Some(t) => t,
        None => {
            let estimate = estimate
                .or_else(|| {
                    let own = cache::Key::of(&meta).latest()?;
                    let (t, _) =
                        snapshot::load_tree(&own, tree::SortKey::Name, None, Some(0)).ok()?;
                    Some(estimate_from(&t))
                })
                .or_else(|| {
                    // Excluded entries would make a filesystem-wide estimate wrong.
                    opts.exclude
                        .is_empty()
                        .then(|| scan::estimate_fs(&meta.root, &opts))
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
            tree::Tree::from_entry(e, tree::SortKey::Disk)
        }
    };
    if let Some(out) = output {
        snapshot::save(&out, &tree, &meta)?;
        eprintln!("saved {}", out.display());
    }
    if use_cache {
        let path = cache::save(&tree, &meta)?;
        eprintln!("cached {}", path.display());
    }
    Ok(())
}
