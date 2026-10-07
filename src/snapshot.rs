//! Snapshot files: an lz4-framed, varint-encoded preorder dump of the tree.
//!
//! Only each node's *own* size is stored; aggregates are recomputed on load,
//! which keeps files small and guarantees they are self-consistent.

use crate::tree::{Entry, Kind, NodeId, Tree};
use anyhow::{Context, bail};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"DUTSNAP\0";
const VERSION: u32 = 1;

pub struct Meta {
    pub root: PathBuf,
    pub one_file_system: bool,
    /// Unix timestamp (seconds) of when the scan finished.
    pub scanned_at: u64,
    pub scan_millis: u64,
}

fn put_varint(w: &mut impl Write, mut v: u64) -> std::io::Result<()> {
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v >= 0x80 {
        buf[i] = (v as u8) | 0x80;
        v >>= 7;
        i += 1;
    }
    buf[i] = v as u8;
    w.write_all(&buf[..=i])
}

fn get_varint(r: &mut impl Read) -> std::io::Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let mut b = [0u8];
        r.read_exact(&mut b)?;
        v |= u64::from(b[0] & 0x7f) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(std::io::Error::other("varint overflow"));
        }
    }
}

fn put_bytes(w: &mut impl Write, b: &[u8]) -> std::io::Result<()> {
    put_varint(w, b.len() as u64)?;
    w.write_all(b)
}

fn get_bytes(r: &mut impl Read) -> std::io::Result<Box<[u8]>> {
    let len = get_varint(r)? as usize;
    if len > 1 << 20 {
        return Err(std::io::Error::other("name too long"));
    }
    let mut b = vec![0u8; len];
    r.read_exact(&mut b)?;
    Ok(b.into_boxed_slice())
}

pub fn save(path: &Path, tree: &Tree, meta: &Meta) -> anyhow::Result<()> {
    // Write to a temp file and rename so a crash never leaves a torn snapshot.
    let tmp = path.with_extension("dut.tmp");
    let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;

    let mut z = lz4_flex::frame::FrameEncoder::new(w);
    put_bytes(&mut z, meta.root.as_os_str().as_bytes())?;
    z.write_all(&[meta.one_file_system as u8])?;
    put_varint(&mut z, meta.scanned_at)?;
    put_varint(&mut z, meta.scan_millis)?;
    put_varint(&mut z, tree.root_node().items)?;

    let mut stack: Vec<NodeId> = vec![tree.root];
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        let (mut own_size, mut own_disk) = (n.size, n.disk);
        for &c in n.children.iter() {
            own_size -= tree.node(c).size;
            own_disk -= tree.node(c).disk;
        }
        z.write_all(&[n.kind as u8, n.flags])?;
        put_bytes(&mut z, &n.name)?;
        put_varint(&mut z, own_size)?;
        put_varint(&mut z, own_disk)?;
        if n.kind == Kind::Dir {
            put_varint(&mut z, n.children.len() as u64)?;
        }
        // Reverse so children come out in their sorted order.
        stack.extend(n.children.iter().rev());
    }
    let mut w = z.finish()?;
    w.flush()?;
    w.into_inner()?.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> anyhow::Result<(Entry, Meta)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("{} is not a dut snapshot", path.display());
    }
    let mut ver = [0u8; 4];
    r.read_exact(&mut ver)?;
    let ver = u32::from_le_bytes(ver);
    if ver != VERSION {
        bail!("unsupported snapshot version {ver}");
    }
    let mut z = BufReader::with_capacity(1 << 16, lz4_flex::frame::FrameDecoder::new(r));
    let root = get_bytes(&mut z)?;
    let root = PathBuf::from(std::ffi::OsStr::from_bytes(&root));
    let mut b = [0u8];
    z.read_exact(&mut b)?;
    let meta = Meta {
        root,
        one_file_system: b[0] != 0,
        scanned_at: get_varint(&mut z)?,
        scan_millis: get_varint(&mut z)?,
    };
    let _count = get_varint(&mut z)?;

    // Preorder rebuild: a stack of open directories with remaining child counts.
    let mut stack: Vec<(Entry, u64)> = Vec::new();
    loop {
        let mut hdr = [0u8; 2];
        z.read_exact(&mut hdr).context("truncated snapshot")?;
        let kind = Kind::from_u8(hdr[0]).context("corrupt snapshot")?;
        let mut e = Entry {
            name: get_bytes(&mut z)?,
            kind,
            flags: hdr[1],
            size: get_varint(&mut z)?,
            disk: get_varint(&mut z)?,
            items: 1,
            children: Vec::new(),
        };
        let nchildren = if kind == Kind::Dir {
            get_varint(&mut z)?
        } else {
            0
        };
        if nchildren > 0 {
            e.children.reserve(nchildren.min(1 << 16) as usize);
            stack.push((e, nchildren));
            continue;
        }
        // `e` is complete; attach it and close any directories it completes.
        loop {
            match stack.last_mut() {
                None => return Ok((e, meta)),
                Some((parent, remaining)) => {
                    parent.size += e.size;
                    parent.disk += e.disk;
                    parent.items += e.items;
                    parent.children.push(e);
                    *remaining -= 1;
                    if *remaining > 0 {
                        break;
                    }
                    e = stack.pop().unwrap().0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Progress, ScanOptions, scan};
    use crate::tree::SortKey;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("dut-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/f"), vec![1u8; 10_000]).unwrap();
        std::fs::write(dir.join("g"), b"hi").unwrap();
        std::fs::hard_link(dir.join("g"), dir.join("a/g2")).unwrap();
        let opts = ScanOptions {
            one_file_system: true,
            threads: 2,
        };
        let e = scan(&dir, opts, &Progress::default()).unwrap();
        let (items, size, disk) = (e.items, e.size, e.disk);
        assert_eq!(items, 6);
        let tree = Tree::from_entry(e, SortKey::Disk);
        let meta = Meta {
            root: dir.clone(),
            one_file_system: true,
            scanned_at: 42,
            scan_millis: 7,
        };
        let file = dir.with_extension("dut");
        save(&file, &tree, &meta).unwrap();
        let (e2, m2) = load(&file).unwrap();
        assert_eq!((e2.items, e2.size, e2.disk), (items, size, disk));
        assert_eq!(
            (m2.root, m2.scanned_at, m2.scan_millis),
            (dir.clone(), 42, 7)
        );
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&file).unwrap();
    }
}
