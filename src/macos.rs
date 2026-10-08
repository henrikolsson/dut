//! macOS specifics: bulk directory reads, firmlinks and mount usage.
//!
//! `getattrlistbulk` returns names and sizes for a whole directory per call,
//! instead of one `lstat` per entry (std's `DirEntry::metadata` stats by full
//! path on macOS, not relative to the directory fd).
//!
//! Since Catalina the read-only system volume is mounted at `/` and user data
//! lives on a separate volume at `/System/Volumes/Data`, stitched into `/` by
//! firmlinks (`/Users` is `/System/Volumes/Data/Users`). A scan of `/` would
//! reach that data twice.

use crate::tree::Kind;
use std::collections::HashSet;
use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const DATA_VOLUME: &str = "/System/Volumes/Data";
const FIRMLINKS: &str = "/usr/share/firmlinks";

/// Data-volume directories that a scan of `root` also reaches through their
/// firmlink, so scanning them again would count them twice.
pub fn firmlink_duplicates(root: &Path) -> HashSet<PathBuf> {
    let Ok(list) = std::fs::read(FIRMLINKS) else {
        return HashSet::new();
    };
    parse_firmlinks(&list, root)
}

/// Lines are `<path on />\t<path relative to the data volume>`.
fn parse_firmlinks(list: &[u8], root: &Path) -> HashSet<PathBuf> {
    let data = Path::new(DATA_VOLUME);
    list.split(|&b| b == b'\n')
        .filter_map(|line| {
            let tab = line.iter().position(|&b| b == b'\t')?;
            let link = Path::new(OsStr::from_bytes(&line[..tab]));
            let target = data.join(OsStr::from_bytes(&line[tab + 1..]));
            (link.starts_with(root) && target.starts_with(root)).then_some(target)
        })
        .collect()
}

/// Device of the data volume, if it differs from the system volume's (older
/// macOS reports them separately; newer ones report the same id).
pub fn data_volume_dev() -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let data = std::fs::metadata(DATA_VOLUME).ok()?.dev();
    let root = std::fs::metadata("/").ok()?.dev();
    (data != root).then_some(data)
}

pub struct Mount {
    pub dir: PathBuf,
    pub fstype: String,
    pub local: bool,
    /// A mounted APFS snapshot, e.g. Time Machine's local snapshots.
    pub snapshot: bool,
}

pub fn mounts() -> Vec<Mount> {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Vec::new();
    }
    let list = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    let text = |s: &[libc::c_char]| unsafe { CStr::from_ptr(s.as_ptr()) }.to_bytes();
    list.iter()
        .map(|st| Mount {
            dir: PathBuf::from(OsStr::from_bytes(text(&st.f_mntonname))),
            fstype: String::from_utf8_lossy(text(&st.f_fstypename)).into_owned(),
            local: st.f_flags & libc::MNT_LOCAL as u32 != 0,
            snapshot: st.f_flags & libc::MNT_SNAPSHOT as u32 != 0,
        })
        .collect()
}

/// Used bytes and inodes of the volume mounted at `dir`.
pub fn usage(dir: &Path) -> Option<(u64, u64)> {
    let cpath = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // APFS volumes share their container's free space, so statfs reports
    // the whole container as used; ask for the volume's own.
    let bytes = volume_space_used(&cpath)
        .unwrap_or(st.f_blocks.saturating_sub(st.f_bfree) * st.f_bsize as u64);
    Some((bytes, st.f_files.saturating_sub(st.f_ffree)))
}

fn volume_space_used(mount: &CStr) -> Option<u64> {
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.volattr = libc::ATTR_VOL_INFO | libc::ATTR_VOL_SPACEUSED;
    // u32 length, then the off_t.
    let mut buf = [0u32; 4];
    let r = unsafe {
        libc::getattrlist(
            mount.as_ptr(),
            (&mut al as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf),
            0,
        )
    };
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), 16) };
    (r == 0 && rd_u32(bytes, 0) >= 12).then(|| rd_u64(bytes, 4))
}

pub fn open_dir(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
}

/// `lstat` of `name` relative to the open directory `dir`.
pub fn stat_at(dir: &File, name: &[u8]) -> io::Result<libc::stat> {
    let mut cname = Vec::with_capacity(name.len() + 1);
    cname.extend_from_slice(name);
    cname.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            cname.as_ptr().cast(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r == 0 {
        Ok(st)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// One directory entry as returned by `getattrlistbulk`. Sizes are only
/// meaningful for non-directories; directories need a separate stat (their
/// device is the covered one at mount points).
pub struct Attrs<'a> {
    pub name: &'a [u8],
    pub kind: Kind,
    pub error: bool,
    /// Logical size of the data fork (`st_size`).
    pub size: u64,
    /// Allocated size of all forks (`st_blocks * 512`).
    pub disk: u64,
    pub nlink: u32,
    pub dev: u64,
    pub ino: u64,
}

const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// Calls `f` for every entry of the open directory `dir`.
pub fn read_dir(dir: &File, mut f: impl FnMut(Attrs)) -> io::Result<()> {
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_FILEID
        | ATTR_CMN_ERROR;
    al.fileattr =
        libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;
    // u64 elements keep the buffer 8-byte aligned, as the kernel expects.
    let mut buf = vec![0u64; 32 * 1024];
    let len = buf.len() * 8;
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                dir.as_raw_fd(),
                (&mut al as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                len,
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(());
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) };
        let mut off = 0;
        for _ in 0..n {
            let rec_len = rd_u32(bytes, off) as usize;
            if rec_len == 0 || off + rec_len > len {
                return Err(io::Error::other("malformed getattrlistbulk record"));
            }
            if let Some(a) = parse(&bytes[off..off + rec_len]) {
                f(a);
            }
            off += rec_len;
        }
    }
}

/// Parses one record: length, returned-attribute set, then the attributes
/// present in that set, in bit order.
fn parse(r: &[u8]) -> Option<Attrs<'_>> {
    let common = rd_u32(r, 4);
    let file = rd_u32(r, 4 + 12);
    let mut p = 4 + 20;
    let mut a = Attrs {
        name: &[],
        kind: Kind::Other,
        error: false,
        size: 0,
        disk: 0,
        nlink: 1,
        dev: 0,
        ino: 0,
    };
    if common & libc::ATTR_CMN_NAME == 0 {
        return None;
    }
    let (name_off, name_len) = (rd_u32(r, p) as i32, rd_u32(r, p + 4) as usize);
    let start = (p as isize + name_off as isize) as usize;
    let name = r.get(start..start + name_len)?;
    a.name = name.strip_suffix(&[0]).unwrap_or(name);
    p += 8;
    if common & libc::ATTR_CMN_DEVID != 0 {
        a.dev = rd_u32(r, p) as i32 as u64; // dev_t is i32, like st_dev
        p += 4;
    }
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        a.kind = match rd_u32(r, p) {
            VREG => Kind::File,
            VDIR => Kind::Dir,
            VLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        p += 4;
    }
    if common & libc::ATTR_CMN_FILEID != 0 {
        a.ino = rd_u64(r, p);
        p += 8;
    }
    if common & ATTR_CMN_ERROR != 0 {
        a.error = rd_u32(r, p) != 0;
        p += 4;
    }
    if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        a.nlink = rd_u32(r, p);
        p += 4;
    }
    if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        a.disk = rd_u64(r, p);
        p += 8;
    }
    if file & libc::ATTR_FILE_DATALENGTH != 0 {
        a.size = rd_u64(r, p);
    }
    Some(a)
}

fn rd_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_ne_bytes(s.try_into().unwrap()))
}

fn rd_u64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .map_or(0, |s| u64::from_ne_bytes(s.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn firmlinks_relative_to_root() {
        let list = b"/Users\tUsers\n/System/Library/Caches\tSystem/Library/Caches\n";
        let all = parse_firmlinks(list, Path::new("/"));
        assert!(all.contains(Path::new("/System/Volumes/Data/Users")));
        assert_eq!(all.len(), 2);
        // Only the caches firmlink is inside /System, but its data-volume
        // side isn't, so nothing is reached twice.
        assert!(parse_firmlinks(list, Path::new("/System/Library")).is_empty());
        assert!(parse_firmlinks(list, Path::new(DATA_VOLUME)).is_empty());
        assert_eq!(parse_firmlinks(list, Path::new("/System")).len(), 1);
    }

    /// Bulk attributes must agree with lstat, including for compressed
    /// system files and symlinks.
    #[test]
    fn bulk_matches_lstat() {
        let tmp = std::env::temp_dir().join(format!("dut-bulk-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("f"), vec![7u8; 50_000]).unwrap();
        std::fs::hard_link(tmp.join("f"), tmp.join("f2")).unwrap();
        std::os::unix::fs::symlink("f", tmp.join("link")).unwrap();
        for dir in [tmp.as_path(), Path::new("/usr/bin")] {
            let d = open_dir(dir).unwrap();
            let mut seen = 0;
            read_dir(&d, |a| {
                let md = std::fs::symlink_metadata(dir.join(OsStr::from_bytes(a.name))).unwrap();
                assert!(!a.error);
                assert_eq!(a.ino, md.ino());
                assert_eq!(a.dev, md.dev());
                if a.kind != Kind::Dir {
                    let name = String::from_utf8_lossy(a.name);
                    assert_eq!(a.size, md.len(), "{name}");
                    assert_eq!(a.disk, md.blocks() * 512, "{name}");
                    assert_eq!(a.nlink as u64, md.nlink(), "{name}");
                }
                seen += 1;
            })
            .unwrap();
            assert_eq!(seen, std::fs::read_dir(dir).unwrap().count());
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
