//! The mount table: which mounts a scan skips by default, and how much the
//! mounts it will enter use (for the progress estimate).
//!
//! Crossing into other mounts is the default, like `du` and `ncdu`, except
//! for virtual filesystems (`/proc`, `/sys`, `/dev`), whose sizes are
//! meaningless, network ones, which can be slow or hang, and (on macOS)
//! mounted snapshots.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Mount {
    pub dir: PathBuf,
    /// Virtual or network filesystem, skipped unless `--all-mounts`.
    pub skip: bool,
    /// The whole filesystem is visible here: not a bind mount or a btrfs
    /// subvolume, whose usage would cover more than what's below it.
    pub whole: bool,
}

/// Filesystems without real disk usage of their own. squashfs images (snaps)
/// are counted already as the image file, and report uncompressed sizes.
#[cfg(target_os = "linux")]
const VIRTUAL: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "proc",
    "pstore",
    "rpc_pipefs",
    "securityfs",
    "selinuxfs",
    "squashfs",
    "sysfs",
    "tracefs",
];

/// Network filesystems (9p is also how WSL mounts Windows drives).
#[cfg(target_os = "linux")]
const NETWORK: &[&str] = &[
    "9p",
    "afs",
    "ceph",
    "cifs",
    "davfs",
    "fuse.gcsfuse",
    "fuse.gvfsd-fuse",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.sshfs",
    "glusterfs",
    "lustre",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb3",
    "smbfs",
];

/// Mounts in mount order. A later mount on the same directory hides the
/// earlier one, so only the last is kept.
pub fn list() -> Vec<Mount> {
    let mut out: Vec<Mount> = Vec::new();
    let mut at: HashMap<PathBuf, usize> = HashMap::new();
    for m in raw_list() {
        match at.get(&m.dir) {
            Some(&i) => out[i] = m,
            None => {
                at.insert(m.dir.clone(), out.len());
                out.push(m);
            }
        }
    }
    out
}

/// Mount points strictly below `root` that a default scan skips.
pub fn skipped_below(root: &Path) -> HashSet<PathBuf> {
    list()
        .into_iter()
        .filter(|m| m.skip && m.dir != root && m.dir.starts_with(root))
        .map(|m| m.dir)
        .collect()
}

#[cfg(target_os = "linux")]
fn raw_list() -> Vec<Mount> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let Ok(mountinfo) = std::fs::read("/proc/self/mountinfo") else {
        return Vec::new();
    };
    // id parent major:minor root mount_point options [optional...] - fstype ...
    mountinfo
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let f: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
            let sep = f.iter().position(|&x| x == b"-")?;
            let (root, dir) = (*f.get(3)?, *f.get(4)?);
            let fstype = String::from_utf8_lossy(f.get(sep + 1)?).into_owned();
            Some(Mount {
                dir: PathBuf::from(OsStr::from_bytes(&unescape(dir))),
                skip: VIRTUAL.contains(&fstype.as_str()) || NETWORK.contains(&fstype.as_str()),
                whole: root == b"/",
            })
        })
        .collect()
}

/// Mount points in mountinfo escape space, tab, newline and backslash as octal.
#[cfg(target_os = "linux")]
fn unescape(s: &[u8]) -> Vec<u8> {
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

/// Used bytes and inodes (0 if the filesystem doesn't track them).
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)] // the statvfs fields are 32-bit on some targets
pub fn usage(dir: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let used_bytes = (st.f_blocks - st.f_bfree) as u64 * st.f_frsize as u64;
    // Some filesystems (btrfs, zfs) report no meaningful inode counts.
    let used_inodes = (st.f_files as u64).saturating_sub(st.f_ffree as u64);
    Some((used_bytes, used_inodes))
}

#[cfg(target_os = "macos")]
fn raw_list() -> Vec<Mount> {
    crate::macos::mounts()
        .into_iter()
        .map(|m| Mount {
            // Snapshots below the root are copies of what's scanned anyway.
            skip: !m.local || m.snapshot || m.fstype == "devfs" || m.fstype == "autofs",
            dir: m.dir,
            whole: true,
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub fn usage(dir: &Path) -> Option<(u64, u64)> {
    crate::macos::usage(dir)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn raw_list() -> Vec<Mount> {
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn usage(_dir: &Path) -> Option<(u64, u64)> {
    None
}
