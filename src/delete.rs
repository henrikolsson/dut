//! Recursive deletion that never follows symlinks and never crosses into
//! another filesystem (unlike a naive `rm -r` on a dir containing a mount).

use crate::scan::Progress;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

/// Deletes `path`. Returns the problems encountered; an empty list means
/// everything was removed.
pub fn delete(path: &Path, progress: &Progress) -> Vec<String> {
    let mut errors = Vec::new();
    match fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => remove_dir(path, md.dev(), progress, &mut errors),
        Ok(md) => remove_file(path, md.blocks() * 512, progress, &mut errors),
        Err(e) => errors.push(format!("{}: {e}", path.display())),
    }
    errors
}

fn remove_file(path: &Path, disk: u64, progress: &Progress, errors: &mut Vec<String>) {
    match fs::remove_file(path) {
        Ok(()) => {
            progress.items.fetch_add(1, Relaxed);
            progress.disk.fetch_add(disk, Relaxed);
        }
        Err(e) => errors.push(format!("{}: {e}", path.display())),
    }
}

fn remove_dir(path: &Path, dev: u64, progress: &Progress, errors: &mut Vec<String>) {
    if progress.cancel.load(Relaxed) {
        return;
    }
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return errors.push(format!("{}: {e}", path.display())),
    };
    for de in rd {
        let de = match de {
            Ok(de) => de,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let child = de.path();
        match de.metadata() {
            Ok(md) if md.is_dir() => {
                if md.dev() != dev {
                    errors.push(format!(
                        "{}: on another filesystem, skipped",
                        child.display()
                    ));
                } else {
                    remove_dir(&child, dev, progress, errors);
                }
            }
            Ok(md) => remove_file(&child, md.blocks() * 512, progress, errors),
            Err(e) => errors.push(format!("{}: {e}", child.display())),
        }
        if progress.cancel.load(Relaxed) {
            return;
        }
    }
    match fs::remove_dir(path) {
        Ok(()) => {
            progress.items.fetch_add(1, Relaxed);
        }
        // A non-empty dir here is a consequence of an error already reported.
        Err(e) if errors.is_empty() => errors.push(format!("{}: {e}", path.display())),
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn deletes_tree_but_not_symlink_targets() {
        let base = std::env::temp_dir().join(format!("dut-del-{}", std::process::id()));
        let keep = base.join("keep");
        let gone = base.join("gone");
        std::fs::create_dir_all(keep.join("inner")).unwrap();
        std::fs::write(keep.join("inner/f"), b"x").unwrap();
        std::fs::create_dir_all(gone.join("a/b")).unwrap();
        std::fs::write(gone.join("a/b/f"), b"x").unwrap();
        std::os::unix::fs::symlink(&keep, gone.join("link")).unwrap();
        let errs = super::delete(&gone, &Default::default());
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!gone.exists());
        assert!(keep.join("inner/f").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
