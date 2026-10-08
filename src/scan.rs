//! Parallel directory scanner.
//!
//! Each directory is read on a rayon worker; subdirectories are recursed into
//! in parallel via work stealing. Entries are stat-ed relative to the open
//! directory fd (std's `DirEntry::metadata` uses `fstatat` on Linux), so the
//! kernel never re-walks full paths per file. On macOS, `getattrlistbulk`
//! fetches a whole directory's sizes at once.

use crate::mounts;
use crate::tree::{Entry, Kind, flags};
use anyhow::Context;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, DirEntry, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Directories with more entries than this get their stat calls spread
/// across workers instead of being handled by a single task.
const PAR_STAT_THRESHOLD: usize = 4096;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub one_file_system: bool,
    pub threads: usize,
    pub exclude: Arc<Excludes>,
    /// Also enter virtual and network filesystems (see [`crate::mounts`]).
    pub all_mounts: bool,
    /// Directories that are also reachable under another path of the scanned
    /// tree (macOS firmlinks). They're skipped so nothing is counted twice.
    pub aliases: Arc<HashSet<PathBuf>>,
}

/// See [`ScanOptions::aliases`]; `root` is the root of the whole tree, even
/// when only part of it is being rescanned.
pub fn aliases(root: &Path) -> HashSet<PathBuf> {
    #[cfg(target_os = "macos")]
    return crate::macos::firmlink_duplicates(root);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        HashSet::new()
    }
}

/// Glob patterns for entries to skip entirely. Patterns without a `/` match
/// the entry's name; patterns with one match its full path.
#[derive(Debug, Default)]
pub struct Excludes {
    pub patterns: Vec<String>,
    names: GlobSet,
    paths: GlobSet,
}

impl Excludes {
    pub fn new(patterns: Vec<String>) -> anyhow::Result<Excludes> {
        let (mut names, mut paths) = (GlobSetBuilder::new(), GlobSetBuilder::new());
        for p in &patterns {
            let glob = GlobBuilder::new(p.trim_end_matches('/'))
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .with_context(|| format!("bad exclude pattern {p:?}"))?;
            if p.trim_end_matches('/').contains('/') {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }
        Ok(Excludes {
            patterns,
            names: names.build()?,
            paths: paths.build()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn matches(&self, dir: &Path, name: &OsStr) -> bool {
        (!self.names.is_empty() && self.names.is_match(Path::new(name)))
            || (!self.paths.is_empty() && self.paths.is_match(dir.join(name)))
    }
}

#[derive(Default)]
pub struct Progress {
    pub items: AtomicU64,
    pub disk: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
    /// Scan finished; the snapshot is being written.
    pub saving: AtomicBool,
    /// Expected totals, if known, for percentage/ETA display.
    pub estimate: Option<Estimate>,
}

#[derive(Clone, Copy, Debug)]
pub struct Estimate {
    pub items: u64,
    pub disk: u64,
    pub source: &'static str,
}

impl Progress {
    pub fn with_estimate(estimate: Option<Estimate>) -> Progress {
        Progress {
            estimate,
            ..Default::default()
        }
    }

    /// Fraction done (0..1) and estimated time remaining, if an estimate exists.
    pub fn eta(&self, elapsed: Duration) -> Option<(f64, Option<Duration>)> {
        let est = self.estimate?;
        // Scan time tracks entry count much better than bytes (one stat per
        // entry), so prefer items when the estimate has them.
        let frac = if est.items > 0 {
            self.items.load(Relaxed) as f64 / est.items as f64
        } else if est.disk > 0 {
            self.disk.load(Relaxed) as f64 / est.disk as f64
        } else {
            return None;
        };
        let frac = frac.clamp(0.0, 0.999);
        let secs = elapsed.as_secs_f64();
        let eta = (frac > 0.01 && secs > 0.5)
            .then(|| Duration::from_secs_f64(secs * (1.0 - frac) / frac));
        Some((frac, eta))
    }
}

/// Estimates scan totals from filesystem usage: that of the mount at `path`
/// plus every mount below it the scan will enter. Only when `path` is a
/// mount point, and each of those mounts shows its whole filesystem (not a
/// bind mount or subvolume).
pub fn estimate_fs(path: &Path, opts: &ScanOptions) -> Option<Estimate> {
    let path = fs::canonicalize(path).ok()?;
    let all = mounts::list();
    let own = all.iter().find(|m| m.dir == path)?;
    let mut below: Vec<&mounts::Mount> = all
        .iter()
        .filter(|m| m.dir != path && m.dir.starts_with(&path))
        .collect();
    below.sort_by_key(|m| m.dir.components().count());
    let mut entered = vec![own];
    let mut not_entered: Vec<&Path> = Vec::new();
    for m in below {
        if not_entered.iter().any(|d| m.dir.starts_with(d)) {
            continue;
        }
        if enters_mount(&path, m, opts) {
            entered.push(m);
        } else {
            not_entered.push(&m.dir);
        }
    }
    let (mut items, mut disk, mut have_items) = (0, 0, true);
    for m in entered {
        if !m.whole {
            return None;
        }
        let (bytes, inodes) = mounts::usage(&m.dir)?;
        disk += bytes;
        items += inodes;
        // Some filesystems (btrfs, zfs) report no meaningful inode counts.
        have_items &= inodes > 0;
    }
    (disk > 0).then_some(Estimate {
        items: if have_items { items } else { 0 },
        disk,
        source: "filesystem usage",
    })
}

/// Whether a scan of `root` descends into mount `m` below it.
fn enters_mount(root: &Path, m: &mounts::Mount, opts: &ScanOptions) -> bool {
    // The macOS data volume is firmlinked into / and scanned with it.
    #[cfg(target_os = "macos")]
    if root == Path::new("/") && m.dir == Path::new(crate::macos::DATA_VOLUME) {
        return true;
    }
    let _ = root;
    !opts.one_file_system && (opts.all_mounts || !m.skip)
}

struct Ctx<'a> {
    root_dev: u64,
    /// Another device that counts as the root's filesystem for `-x`: the
    /// macOS data volume, firmlinked into the system volume.
    paired_dev: Option<u64>,
    /// Virtual and network mount points to skip.
    skip_mounts: HashSet<PathBuf>,
    opts: ScanOptions,
    progress: &'a Progress,
    /// (dev, ino) of multiply-linked files already counted.
    hardlinks: Mutex<HashSet<(u64, u64)>>,
}

pub fn default_threads() -> usize {
    // Scanning is dominated by syscall latency rather than CPU, so a few
    // more threads than cores keeps the I/O queue fuller on SSDs and network
    // filesystems.
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores * 2).clamp(4, 64)
}

/// Scans `root`, returning the entry tree. The root entry's name is the
/// absolute path that was scanned.
pub fn scan(root: &Path, opts: ScanOptions, progress: &Progress) -> anyhow::Result<Entry> {
    let root = fs::canonicalize(root)?;
    let md = fs::symlink_metadata(&root)?;
    let threads = opts.threads.max(1);
    #[cfg(target_os = "macos")]
    let paired_dev = crate::macos::data_volume_dev()
        .filter(|_| fs::symlink_metadata("/").is_ok_and(|r| r.dev() == md.dev()));
    #[cfg(not(target_os = "macos"))]
    let paired_dev = None;
    let skip_mounts = if opts.one_file_system || opts.all_mounts {
        HashSet::new()
    } else {
        mounts::skipped_below(&root)
    };
    let ctx = Ctx {
        root_dev: md.dev(),
        paired_dev,
        skip_mounts,
        opts,
        progress,
        hardlinks: Mutex::new(HashSet::new()),
    };
    let mut entry = entry_from_meta(root.as_os_str().as_bytes().into(), &md);
    progress.items.fetch_add(1, Relaxed);
    progress.disk.fetch_add(entry.disk, Relaxed);
    if entry.kind == Kind::Dir {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .stack_size(16 << 20)
            .thread_name(|i| format!("dut-scan-{i}"))
            .build()?;
        pool.install(|| scan_dir(&ctx, &root, &mut entry, md.dev()));
    }
    Ok(entry)
}

fn kind_of(md: &Metadata) -> Kind {
    let ft = md.file_type();
    if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else if ft.is_symlink() {
        Kind::Symlink
    } else {
        Kind::Other
    }
}

fn entry_from_meta(name: Box<[u8]>, md: &Metadata) -> Entry {
    Entry {
        name,
        kind: kind_of(md),
        flags: 0,
        size: md.len(),
        disk: md.blocks() * 512,
        items: 1,
        children: Vec::new(),
    }
}

/// Stats a single directory entry. Returns the entry and its device id.
fn stat_entry(ctx: &Ctx, de: &DirEntry) -> (Entry, u64) {
    let name: Box<[u8]> = de.file_name().as_bytes().into();
    match de.metadata() {
        Ok(md) => {
            let mut e = entry_from_meta(name, &md);
            count_hardlink(ctx, &mut e, md.nlink(), md.dev(), md.ino());
            (e, md.dev())
        }
        Err(_) => {
            let kind = match de.file_type() {
                Ok(ft) if ft.is_dir() => Kind::Dir,
                Ok(ft) if ft.is_symlink() => Kind::Symlink,
                Ok(ft) if ft.is_file() => Kind::File,
                _ => Kind::Other,
            };
            (error_entry(name, kind), ctx.root_dev)
        }
    }
}

fn error_entry(name: Box<[u8]>, kind: Kind) -> Entry {
    Entry {
        name,
        kind,
        flags: flags::ERROR,
        size: 0,
        disk: 0,
        items: 1,
        children: Vec::new(),
    }
}

/// Zeroes the sizes of a file whose inode was already counted.
fn count_hardlink(ctx: &Ctx, e: &mut Entry, nlink: u64, dev: u64, ino: u64) {
    if e.kind != Kind::Dir && nlink > 1 {
        let fresh = ctx.hardlinks.lock().unwrap().insert((dev, ino));
        if !fresh {
            e.flags |= flags::HARDLINK;
            e.size = 0;
            e.disk = 0;
        }
    }
}

/// Reads and stats the entries of `path`, minus excluded ones. Returns the
/// entries with their device ids and whether reading stopped early, or
/// `None` if the directory couldn't be opened.
#[cfg(not(target_os = "macos"))]
fn read_entries(ctx: &Ctx, path: &Path) -> Option<(Vec<(Entry, u64)>, bool)> {
    read_entries_std(ctx, path)
}

fn read_entries_std(ctx: &Ctx, path: &Path) -> Option<(Vec<(Entry, u64)>, bool)> {
    let rd = fs::read_dir(path).ok()?;
    let mut read_error = false;
    let exclude = &ctx.opts.exclude;
    let des: Vec<DirEntry> = rd
        .filter_map(|r| r.map_err(|_| read_error = true).ok())
        .filter(|de| exclude.is_empty() || !exclude.matches(path, &de.file_name()))
        .collect();
    let stat = |de: &DirEntry| stat_entry(ctx, de);
    let stated = if des.len() > PAR_STAT_THRESHOLD {
        des.par_iter().with_min_len(512).map(stat).collect()
    } else {
        des.iter().map(stat).collect()
    };
    Some((stated, read_error))
}

/// macOS: one `getattrlistbulk` call per batch of entries instead of an
/// `lstat` each. Directories still get an `fstatat`, for their real device.
#[cfg(target_os = "macos")]
fn read_entries(ctx: &Ctx, path: &Path) -> Option<(Vec<(Entry, u64)>, bool)> {
    use crate::macos;
    let dir = macos::open_dir(path).ok()?;
    let exclude = &ctx.opts.exclude;
    let mut out = Vec::new();
    let res = macos::read_dir(&dir, |a| {
        if !exclude.is_empty() && exclude.matches(path, OsStr::from_bytes(a.name)) {
            return;
        }
        let name: Box<[u8]> = a.name.into();
        if a.error {
            return out.push((error_entry(name, a.kind), ctx.root_dev));
        }
        if a.kind == Kind::Dir {
            return out.push(match macos::stat_at(&dir, a.name) {
                Ok(st) => {
                    let e = Entry {
                        name,
                        kind: Kind::Dir,
                        flags: 0,
                        size: st.st_size as u64,
                        disk: st.st_blocks as u64 * 512,
                        items: 1,
                        children: Vec::new(),
                    };
                    (e, st.st_dev as u64)
                }
                Err(_) => (error_entry(name, Kind::Dir), ctx.root_dev),
            });
        }
        let mut e = Entry {
            name,
            kind: a.kind,
            flags: 0,
            size: a.size,
            disk: a.disk,
            items: 1,
            children: Vec::new(),
        };
        count_hardlink(ctx, &mut e, a.nlink as u64, a.dev, a.ino);
        out.push((e, a.dev));
    });
    match res {
        Ok(()) => Some((out, false)),
        // Not supported here (shouldn't happen: the kernel emulates it for
        // filesystems without native support).
        Err(e)
            if out.is_empty() && matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP)) =>
        {
            read_entries_std(ctx, path)
        }
        Err(_) => Some((out, true)),
    }
}

/// Scans the directory at `path`, which is on device `dir_dev`.
fn scan_dir(ctx: &Ctx, path: &Path, dir: &mut Entry, dir_dev: u64) {
    if ctx.progress.cancel.load(Relaxed) {
        return;
    }
    let Some((stated, read_error)) = read_entries(ctx, path) else {
        dir.flags |= flags::ERROR;
        ctx.progress.errors.fetch_add(1, Relaxed);
        return;
    };
    if read_error {
        dir.flags |= flags::ERROR;
        ctx.progress.errors.fetch_add(1, Relaxed);
    }

    let mut children = Vec::with_capacity(stated.len());
    let mut subdirs = Vec::new();
    let mut disk = 0u64;
    let mut errors = 0u64;
    for (mut e, dev) in stated {
        disk += e.disk;
        if e.flags & flags::ERROR != 0 {
            errors += 1;
        }
        if e.kind == Kind::Dir && e.flags & flags::ERROR == 0 {
            let child = || path.join(OsStr::from_bytes(&e.name));
            let aliases = &ctx.opts.aliases;
            let skip =
                if ctx.opts.one_file_system && dev != ctx.root_dev && Some(dev) != ctx.paired_dev {
                    flags::OTHER_FS
                } else if dev != dir_dev
                    && !ctx.skip_mounts.is_empty()
                    && ctx.skip_mounts.contains(&child())
                {
                    flags::SKIPPED_FS
                } else if !aliases.is_empty() && aliases.contains(&child()) {
                    flags::ALIAS
                } else {
                    0
                };
            if skip != 0 {
                e.flags |= skip;
                disk -= e.disk;
                e.size = 0;
                e.disk = 0;
                children.push(e);
            } else {
                subdirs.push((e, dev));
            }
        } else {
            children.push(e);
        }
    }
    let p = ctx.progress;
    p.items
        .fetch_add((children.len() + subdirs.len()) as u64, Relaxed);
    p.disk.fetch_add(disk, Relaxed);
    if errors > 0 {
        p.errors.fetch_add(errors, Relaxed);
    }

    subdirs.par_iter_mut().for_each(|(sub, dev)| {
        let child_path: PathBuf = path.join(OsStr::from_bytes(&sub.name));
        scan_dir(ctx, &child_path, sub, *dev);
    });

    children.extend(subdirs.into_iter().map(|(e, _)| e));
    for c in &children {
        dir.size += c.size;
        dir.disk += c.disk;
        dir.items += c.items;
        if c.flags & (flags::ERROR | flags::SUB_ERROR) != 0 {
            dir.flags |= flags::SUB_ERROR;
        }
    }
    dir.children = children;
}
