//! Parallel directory scanner.
//!
//! Each directory is read on a rayon worker; subdirectories are recursed into
//! in parallel via work stealing. Entries are stat-ed relative to the open
//! directory fd (std's `DirEntry::metadata` uses `fstatat`), so the kernel
//! never re-walks full paths per file.

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

/// Estimates scan totals from filesystem usage. Only valid when `path` is the
/// root of a mount whose whole filesystem is visible from there (not a bind
/// mount or subvolume), and only when the scan won't wander into other
/// mounts below it.
pub fn estimate_fs(path: &Path, one_file_system: bool) -> Option<Estimate> {
    let path = fs::canonicalize(path).ok()?;
    let mountinfo = fs::read("/proc/self/mountinfo").ok()?;
    let target = path.as_os_str().as_bytes();
    // Fields: id parent major:minor root mount_point ...
    let mut prefix = target.to_vec();
    if prefix.last() != Some(&b'/') {
        prefix.push(b'/');
    }
    let (mut full_fs_mount, mut has_submounts) = (false, false);
    for line in mountinfo.split(|&b| b == b'\n') {
        let mut f = line.split(|&b| b == b' ');
        let (Some(root), Some(mnt)) = (f.nth(3), f.next()) else {
            continue;
        };
        let mnt = unescape_mount(mnt);
        if mnt == target {
            full_fs_mount = root == b"/";
        } else if mnt.starts_with(&prefix) {
            has_submounts = true;
        }
    }
    if !full_fs_mount || (has_submounts && !one_file_system) {
        return None;
    }
    let cpath = std::ffi::CString::new(target).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let used_bytes = (st.f_blocks - st.f_bfree) as u64 * st.f_frsize as u64;
    // Some filesystems (btrfs, zfs) report no meaningful inode counts.
    let used_inodes = (st.f_files as u64).saturating_sub(st.f_ffree as u64);
    (used_bytes > 0).then_some(Estimate {
        items: used_inodes,
        disk: used_bytes,
        source: "filesystem usage",
    })
}

/// Mount points in mountinfo escape space, tab, newline and backslash as octal.
fn unescape_mount(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\'
            && i + 3 < s.len()
            && s[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((s[i + 1] - b'0') * 64 + (s[i + 2] - b'0') * 8 + (s[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

struct Ctx<'a> {
    root_dev: u64,
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
    let ctx = Ctx {
        root_dev: md.dev(),
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
        pool.install(|| scan_dir(&ctx, &root, &mut entry));
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
            if e.kind != Kind::Dir && md.nlink() > 1 {
                let fresh = ctx.hardlinks.lock().unwrap().insert((md.dev(), md.ino()));
                if !fresh {
                    e.flags |= flags::HARDLINK;
                    e.size = 0;
                    e.disk = 0;
                }
            }
            (e, md.dev())
        }
        Err(_) => {
            let kind = match de.file_type() {
                Ok(ft) if ft.is_dir() => Kind::Dir,
                Ok(ft) if ft.is_symlink() => Kind::Symlink,
                Ok(ft) if ft.is_file() => Kind::File,
                _ => Kind::Other,
            };
            let e = Entry {
                name,
                kind,
                flags: flags::ERROR,
                size: 0,
                disk: 0,
                items: 1,
                children: Vec::new(),
            };
            (e, ctx.root_dev)
        }
    }
}

fn scan_dir(ctx: &Ctx, path: &Path, dir: &mut Entry) {
    if ctx.progress.cancel.load(Relaxed) {
        return;
    }
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            dir.flags |= flags::ERROR;
            ctx.progress.errors.fetch_add(1, Relaxed);
            return;
        }
    };
    let mut read_error = false;
    let exclude = &ctx.opts.exclude;
    let des: Vec<DirEntry> = rd
        .filter_map(|r| r.map_err(|_| read_error = true).ok())
        .filter(|de| exclude.is_empty() || !exclude.matches(path, &de.file_name()))
        .collect();
    if read_error {
        dir.flags |= flags::ERROR;
        ctx.progress.errors.fetch_add(1, Relaxed);
    }

    let stat = |de: &DirEntry| stat_entry(ctx, de);
    let stated: Vec<(Entry, u64)> = if des.len() > PAR_STAT_THRESHOLD {
        des.par_iter().with_min_len(512).map(stat).collect()
    } else {
        des.iter().map(stat).collect()
    };
    drop(des);

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
            if ctx.opts.one_file_system && dev != ctx.root_dev {
                e.flags |= flags::OTHER_FS;
                disk -= e.disk;
                e.size = 0;
                e.disk = 0;
                children.push(e);
            } else {
                subdirs.push(e);
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

    subdirs.par_iter_mut().for_each(|sub| {
        let child_path: PathBuf = path.join(OsStr::from_bytes(&sub.name));
        scan_dir(ctx, &child_path, sub);
    });

    children.append(&mut subdirs);
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
