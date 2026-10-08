//! Automatic snapshots: every full scan is saved to the user's cache dir and
//! reopened on the next run. A short history is kept per root (and options)
//! so growth over time can be shown.
//!
//! Files are named `<name>-<hash>.<unix time>.dut`, where the hash covers the
//! root path and the scan options that change results.

use crate::snapshot::{self, Meta};
use std::collections::{BTreeMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Always keep this many of the newest snapshots per root.
const KEEP_RECENT: usize = 3;
/// Beyond those, keep the newest snapshot of each of this many days...
const KEEP_DAYS: usize = 7;
/// ...and of each of this many weeks.
const KEEP_WEEKS: usize = 8;
/// Roots not scanned for this long are forgotten.
const MAX_AGE_DAYS: u64 = 90;
/// Total cache size above which the oldest history (never the latest
/// snapshot of a root) is removed.
const MAX_TOTAL_BYTES: u64 = 2 << 30;

pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DUT_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ if cfg!(target_os = "macos") => {
            PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches")
        }
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join("dut"))
}

/// Identifies the snapshots of one root scanned with one set of options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key(String);

impl Key {
    pub fn new(root: &Path, one_file_system: bool, all_mounts: bool, exclude: &[String]) -> Key {
        let mut h = Fnv::default();
        h.write(root.as_os_str().as_bytes());
        h.write(&[0, one_file_system as u8]);
        // Only hashed when set, so keys from before the option still match.
        if all_mounts {
            h.write(b"\0all-mounts");
        }
        for e in sorted(exclude) {
            h.write(&[0]);
            h.write(e.as_bytes());
        }
        let base: String = root
            .file_name()
            .map_or("root".into(), |n| n.to_string_lossy().into_owned())
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
        Key(format!("{base}-{:016x}", h.0))
    }

    pub fn of(meta: &Meta) -> Key {
        Key::new(
            &meta.root,
            meta.one_file_system,
            meta.all_mounts,
            &meta.exclude,
        )
    }

    /// Where a snapshot taken at `scanned_at` is stored.
    pub fn path(&self, scanned_at: u64) -> Option<PathBuf> {
        Some(dir()?.join(format!("{}.{scanned_at}.dut", self.0)))
    }

    /// This key's snapshots, newest first.
    pub fn history(&self) -> Vec<(u64, PathBuf)> {
        let mut v: Vec<_> = files()
            .into_iter()
            .filter(|f| f.key == *self)
            .map(|f| (f.time, f.path))
            .collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.0));
        v
    }

    pub fn latest(&self) -> Option<PathBuf> {
        self.history().into_iter().next().map(|(_, p)| p)
    }
}

fn sorted(v: &[String]) -> Vec<&String> {
    let mut v: Vec<&String> = v.iter().collect();
    v.sort();
    v.dedup();
    v
}

struct CacheFile {
    key: Key,
    time: u64,
    path: PathBuf,
    bytes: u64,
}

/// All snapshot files in the cache dir (temp files and strays ignored).
fn files() -> Vec<CacheFile> {
    let Some(rd) = dir().and_then(|d| std::fs::read_dir(d).ok()) else {
        return Vec::new();
    };
    rd.filter_map(|de| {
        let de = de.ok()?;
        let name = de.file_name().into_string().ok()?;
        let stem = name.strip_suffix(".dut")?;
        let (key, time) = stem.rsplit_once('.')?;
        Some(CacheFile {
            key: Key(key.to_string()),
            time: time.parse().ok()?,
            bytes: de.metadata().ok()?.len(),
            path: de.path(),
        })
    })
    .collect()
}

/// Creates the cache dir (private to the user) if needed.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(p) => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p),
        None => Ok(()),
    }
}

/// Saves a snapshot into the cache and applies the retention policy.
pub fn save(tree: &crate::tree::Tree, meta: &Meta) -> anyhow::Result<PathBuf> {
    let key = Key::of(meta);
    let path = key
        .path(meta.scanned_at)
        .ok_or_else(|| anyhow::anyhow!("no cache directory (HOME unset?)"))?;
    ensure_dir(&path)?;
    snapshot::save(&path, tree, meta)?;
    gc(crate::app::now_unix());
    Ok(path)
}

/// Which of a key's snapshot times (newest first) to keep.
fn retain(times: &[u64]) -> HashSet<u64> {
    let mut keep: HashSet<u64> = times.iter().take(KEEP_RECENT).copied().collect();
    let mut days = HashSet::new();
    let mut weeks = HashSet::new();
    for &t in times {
        let (day, week) = (t / 86400, t / (7 * 86400));
        if days.len() < KEEP_DAYS && days.insert(day) {
            keep.insert(t);
        }
        if weeks.len() < KEEP_WEEKS && weeks.insert(week) {
            keep.insert(t);
        }
    }
    keep
}

/// Applies retention per root, forgets stale roots, and enforces the size cap.
pub fn gc(now: u64) {
    let mut by_key: BTreeMap<String, Vec<CacheFile>> = BTreeMap::new();
    for f in files() {
        by_key.entry(f.key.0.clone()).or_default().push(f);
    }
    let mut history: Vec<CacheFile> = Vec::new(); // deletable when over the cap
    let mut total = 0u64;
    for (_, mut fs) in by_key {
        fs.sort_by_key(|f| std::cmp::Reverse(f.time));
        if now.saturating_sub(fs[0].time) > MAX_AGE_DAYS * 86400 {
            for f in fs {
                let _ = std::fs::remove_file(&f.path);
            }
            continue;
        }
        let times: Vec<u64> = fs.iter().map(|f| f.time).collect();
        let keep = retain(&times);
        for (i, f) in fs.into_iter().enumerate() {
            if !keep.contains(&f.time) {
                let _ = std::fs::remove_file(&f.path);
                continue;
            }
            total += f.bytes;
            if i > 0 {
                history.push(f);
            }
        }
    }
    history.sort_by_key(|f| f.time);
    for f in history {
        if total <= MAX_TOTAL_BYTES {
            break;
        }
        if std::fs::remove_file(&f.path).is_ok() {
            total -= f.bytes;
        }
    }
}

pub struct Listed {
    pub path: PathBuf,
    pub meta: Meta,
    pub bytes: u64,
}

/// Every cached snapshot with its header, newest first.
pub fn list() -> Vec<Listed> {
    let mut v: Vec<Listed> = files()
        .into_iter()
        .filter_map(|f| {
            Some(Listed {
                meta: snapshot::read_meta(&f.path).ok()?,
                path: f.path,
                bytes: f.bytes,
            })
        })
        .collect();
    v.sort_by(|a, b| (&a.meta.root, b.meta.scanned_at).cmp(&(&b.meta.root, a.meta.scanned_at)));
    v
}

/// Removes cached snapshots: all of them, or those of one root.
pub fn clear(root: Option<&Path>) -> (usize, u64) {
    let (mut n, mut bytes) = (0, 0);
    for l in list() {
        if root.is_none_or(|r| l.meta.root == r) && std::fs::remove_file(&l.path).is_ok() {
            n += 1;
            bytes += l.bytes;
        }
    }
    (n, bytes)
}

/// The newest snapshot of the closest ancestor of `root` scanned with the
/// same options, whose subtree can stand in until `root` itself is scanned.
pub fn find_ancestor(
    root: &Path,
    one_file_system: bool,
    all_mounts: bool,
    exclude: &[String],
) -> Option<(PathBuf, Meta)> {
    // Only the latest snapshot of each key matters.
    let mut latest: BTreeMap<String, CacheFile> = BTreeMap::new();
    for f in files() {
        match latest.get(&f.key.0) {
            Some(cur) if cur.time >= f.time => {}
            _ => {
                latest.insert(f.key.0.clone(), f);
            }
        }
    }
    let want = sorted(exclude);
    latest
        .into_values()
        .filter_map(|f| Some((snapshot::read_meta(&f.path).ok()?, f.path)))
        .filter(|(m, _)| {
            root != m.root
                && root.starts_with(&m.root)
                && m.one_file_system == one_file_system
                && m.all_mounts == all_mounts
                && sorted(&m.exclude) == want
        })
        .max_by_key(|(m, _)| (m.root.components().count(), m.scanned_at))
        .map(|(m, p)| (p, m))
}

struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf29ce484222325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_depends_on_options() {
        let p = Path::new("/home/me/My Stuff");
        let a = Key::new(p, false, false, &[]);
        let b = Key::new(p, true, false, &[]);
        let c = Key::new(p, false, false, &["x".into(), "y".into()]);
        let d = Key::new(p, false, false, &["y".into(), "x".into()]);
        let e = Key::new(p, false, true, &[]);
        assert_ne!(a, b);
        assert_ne!(a, e);
        assert_ne!(a, c);
        assert_eq!(c, d);
        assert!(a.0.starts_with("My_Stuff-"));
    }

    #[test]
    fn retention() {
        let day = 86400;
        let now = 1000 * day;
        // Hourly snapshots over 60 days, newest first.
        let times: Vec<u64> = (0..60 * 24).map(|h| now - h * 3600).collect();
        let keep = retain(&times);
        assert!(keep.contains(&now));
        assert!(keep.contains(&(now - 2 * 3600)));
        assert!(keep.len() <= KEEP_RECENT + KEEP_DAYS + KEEP_WEEKS);
        // Something from roughly 7 weeks ago survives.
        assert!(keep.iter().any(|&t| now - t > 45 * day));
    }
}
