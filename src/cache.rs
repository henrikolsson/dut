//! Automatic snapshots: every full scan is saved to the user's cache dir,
//! keyed by root path and scan options, and reopened on the next run.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DUT_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join("dut"))
}

/// Snapshot path for a scan of `root` with the given options. Different
/// options give different results, so they get separate files.
pub fn path_for(root: &Path, one_file_system: bool, exclude: &[String]) -> Option<PathBuf> {
    let mut h = Fnv::default();
    h.write(root.as_os_str().as_bytes());
    h.write(&[0, one_file_system as u8]);
    let mut ex: Vec<&String> = exclude.iter().collect();
    ex.sort();
    for e in ex {
        h.write(&[0]);
        h.write(e.as_bytes());
    }
    let base: String = root
        .file_name()
        .map_or("root".into(), |n| n.to_string_lossy().into_owned())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    Some(dir()?.join(format!("{base}-{:016x}.dut", h.0)))
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
        let a = path_for(p, false, &[]).unwrap();
        let b = path_for(p, true, &[]).unwrap();
        let c = path_for(p, false, &["x".into(), "y".into()]).unwrap();
        let d = path_for(p, false, &["y".into(), "x".into()]).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(c, d);
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("My_Stuff-")
        );
    }
}
