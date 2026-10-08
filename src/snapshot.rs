//! Snapshot files: an lz4-framed, varint-encoded preorder dump of the tree.
//!
//! Only each node's *own* size is stored; aggregates are recomputed on load,
//! which keeps files small and guarantees they are self-consistent.

use crate::tree::{Kind, NO_PARENT, Node, NodeId, SortKey, Tree, sort_node_ids};
use anyhow::{Context, bail};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"DUTSNAP\0";
const VERSION: u32 = 2;

#[derive(Clone)]
pub struct Meta {
    pub root: PathBuf,
    pub one_file_system: bool,
    /// Scanned with `--all-mounts`.
    pub all_mounts: bool,
    /// Unix timestamp (seconds) of when the scan finished.
    pub scanned_at: u64,
    pub scan_millis: u64,
    /// Exclude patterns the scan was made with.
    pub exclude: Vec<String>,
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
    // Snapshots list every file name, so keep them private.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;

    let mut z = lz4_flex::frame::FrameEncoder::new(w);
    put_bytes(&mut z, meta.root.as_os_str().as_bytes())?;
    z.write_all(&[meta.one_file_system as u8 | (meta.all_mounts as u8) << 1])?;
    put_varint(&mut z, meta.scanned_at)?;
    put_varint(&mut z, meta.scan_millis)?;
    put_varint(&mut z, meta.exclude.len() as u64)?;
    for p in &meta.exclude {
        put_bytes(&mut z, p.as_bytes())?;
    }
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

/// Opens a snapshot and reads its header, leaving the reader at the node count.
fn open(path: &Path) -> anyhow::Result<(impl Read, Meta)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)
        .with_context(|| format!("{} is not a dut snapshot", path.display()))?;
    if &magic != MAGIC {
        bail!("{} is not a dut snapshot", path.display());
    }
    let mut ver = [0u8; 4];
    r.read_exact(&mut ver)?;
    let ver = u32::from_le_bytes(ver);
    if !(1..=VERSION).contains(&ver) {
        bail!("unsupported snapshot version {ver}");
    }
    let mut z = BufReader::with_capacity(1 << 16, lz4_flex::frame::FrameDecoder::new(r));
    let root = get_bytes(&mut z)?;
    let root = PathBuf::from(std::ffi::OsStr::from_bytes(&root));
    let mut b = [0u8];
    z.read_exact(&mut b)?;
    let mut meta = Meta {
        root,
        one_file_system: b[0] & 1 != 0,
        all_mounts: b[0] & 2 != 0,
        scanned_at: get_varint(&mut z)?,
        scan_millis: get_varint(&mut z)?,
        exclude: Vec::new(),
    };
    if ver >= 2 {
        for _ in 0..get_varint(&mut z)? {
            let p = get_bytes(&mut z)?;
            meta.exclude.push(String::from_utf8_lossy(&p).into_owned());
        }
    }
    Ok((z, meta))
}

/// Reads only a snapshot's header.
pub fn read_meta(path: &Path) -> anyhow::Result<Meta> {
    Ok(open(path)?.1)
}

pub fn load(path: &Path, sort: SortKey) -> anyhow::Result<(Tree, Meta)> {
    load_tree(path, sort, None, None)
}

#[derive(Clone, Copy)]
enum Place {
    /// On the way down to the requested subtree, `n` components matched.
    Path(usize),
    /// Inside the requested subtree, at this depth below its root.
    Inside(usize),
    /// Somewhere irrelevant: parsed, not kept.
    Skip,
}

struct Frame {
    id: Option<NodeId>,
    place: Place,
    remaining: u64,
    children: Vec<NodeId>,
    size: u64,
    disk: u64,
    items: u64,
}

/// Loads a snapshot straight into an arena tree.
///
/// With `sub`, only the subtree at that path (which must lie below the
/// snapshot's root) is kept; reading stops as soon as it is complete. With
/// `max_depth`, nodes deeper than that below the kept root are aggregated
/// into their ancestors but not materialized.
pub fn load_tree(
    path: &Path,
    sort: SortKey,
    sub: Option<&Path>,
    max_depth: Option<usize>,
) -> anyhow::Result<(Tree, Meta)> {
    let (mut z, mut meta) = open(path)?;
    let target: Vec<Box<[u8]>> = match sub {
        Some(p) if p != meta.root => {
            let rel = p.strip_prefix(&meta.root).with_context(|| {
                format!("{} is not inside {}", p.display(), meta.root.display())
            })?;
            rel.iter().map(|c| c.as_bytes().into()).collect()
        }
        _ => Vec::new(),
    };
    // Note `join("")` would add a trailing slash.
    let mut target_path = meta.root.clone();
    for c in &target {
        target_path.push(std::ffi::OsStr::from_bytes(c));
    }
    let count = get_varint(&mut z)?;
    let mut nodes: Vec<Node> = Vec::new();
    if target.is_empty() && max_depth.is_none() {
        nodes.reserve(count.min(1 << 28) as usize);
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut root = None;

    'records: loop {
        let mut hdr = [0u8; 2];
        z.read_exact(&mut hdr).context("truncated snapshot")?;
        let kind = Kind::from_u8(hdr[0]).context("corrupt snapshot")?;
        let name = get_bytes(&mut z)?;
        let (size, disk) = (get_varint(&mut z)?, get_varint(&mut z)?);
        let nchildren = if kind == Kind::Dir {
            get_varint(&mut z)?
        } else {
            0
        };

        let place = match stack.last().map(|f| f.place) {
            None if target.is_empty() => Place::Inside(0),
            None => Place::Path(0),
            Some(Place::Path(k)) if name == target[k] => {
                if k + 1 == target.len() {
                    Place::Inside(0)
                } else {
                    Place::Path(k + 1)
                }
            }
            Some(Place::Inside(d)) => Place::Inside(d + 1),
            Some(_) => Place::Skip,
        };
        let id = match place {
            Place::Inside(d) if max_depth.is_none_or(|m| d <= m) => {
                let (parent, name) = match d {
                    0 => (NO_PARENT, target_path.as_os_str().as_bytes().into()),
                    _ => (stack.last().unwrap().id.unwrap(), name),
                };
                nodes.push(Node {
                    name,
                    children: Box::new([]),
                    size,
                    disk,
                    items: 1,
                    parent,
                    kind,
                    flags: hdr[1],
                });
                Some((nodes.len() - 1) as NodeId)
            }
            _ => None,
        };
        let mut done = Frame {
            id,
            place,
            remaining: nchildren,
            children: Vec::new(),
            size,
            disk,
            items: 1,
        };
        if nchildren > 0 {
            stack.push(done);
            continue;
        }
        // `done` is complete; close it and any directories it completes.
        loop {
            if let Some(id) = done.id {
                sort_node_ids(&nodes, &mut done.children, sort);
                let n = &mut nodes[id as usize];
                n.size = done.size;
                n.disk = done.disk;
                n.items = done.items;
                n.children = std::mem::take(&mut done.children).into_boxed_slice();
                if matches!(done.place, Place::Inside(0)) {
                    root = Some(id);
                    // Everything we need has been read.
                    break 'records;
                }
            }
            let Some(p) = stack.last_mut() else {
                break 'records;
            };
            p.size += done.size;
            p.disk += done.disk;
            p.items += done.items;
            if let (Some(id), Some(_)) = (done.id, p.id) {
                p.children.push(id);
            }
            p.remaining -= 1;
            if p.remaining > 0 {
                break;
            }
            done = stack.pop().unwrap();
        }
    }
    let root = root.with_context(|| format!("{} is not in the snapshot", target_path.display()))?;
    meta.root = target_path;
    Ok((
        Tree {
            nodes,
            root,
            garbage: 0,
        },
        meta,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Excludes, Progress, ScanOptions, scan};
    use std::sync::Arc;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("dut-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::create_dir_all(dir.join("a/skipdir")).unwrap();
        std::fs::write(dir.join("a/skipdir/big"), vec![1u8; 100_000]).unwrap();
        std::fs::write(dir.join("a/b/f"), vec![1u8; 10_000]).unwrap();
        std::fs::write(dir.join("a/b/f.tmp"), vec![1u8; 10_000]).unwrap();
        std::fs::write(dir.join("g"), b"hi").unwrap();
        std::fs::hard_link(dir.join("g"), dir.join("a/g2")).unwrap();
        let exclude = vec!["*.tmp".to_string(), "**/a/skipdir".to_string()];
        let opts = ScanOptions {
            one_file_system: true,
            threads: 2,
            exclude: Arc::new(Excludes::new(exclude.clone()).unwrap()),
            all_mounts: false,
            aliases: Default::default(),
        };
        let e = scan(&dir, opts, &Progress::default()).unwrap();
        let (items, size, disk) = (e.items, e.size, e.disk);
        // root, a, a/b, a/b/f, a/g2, g
        assert_eq!(items, 6);
        let tree = Tree::from_entry(e, SortKey::Disk);
        let meta = Meta {
            root: dir.clone(),
            one_file_system: true,
            all_mounts: false,
            scanned_at: 42,
            scan_millis: 7,
            exclude: exclude.clone(),
        };
        let file = dir.with_extension("dut");
        save(&file, &tree, &meta).unwrap();
        let (t2, m2) = load(&file, SortKey::Disk).unwrap();
        let r = t2.root_node();
        assert_eq!(t2.path(t2.root), dir);
        assert_eq!((r.items, r.size, r.disk), (items, size, disk));
        let (sub, sm) = load_tree(&file, SortKey::Disk, Some(&dir.join("a/b")), None).unwrap();
        assert_eq!(sub.root_node().items, 2);
        let a = t2.find_child(t2.root, b"a").unwrap();
        let ab = t2.find_child(a, b"b").unwrap();
        assert_eq!(sub.root_node().size, t2.node(ab).size);
        assert!(sub.root_node().size >= 10_000);
        assert_eq!(sm.root, dir.join("a/b"));
        assert_eq!(sub.path(sub.root), dir.join("a/b"));
        let (shallow, _) = load_tree(&file, SortKey::Disk, None, Some(1)).unwrap();
        assert_eq!(shallow.root_node().items, items);
        assert_eq!(shallow.nodes.len(), 3); // root, a, g; a/* aggregated only
        assert!(load_tree(&file, SortKey::Disk, Some(&dir.join("nope")), None).is_err());
        assert_eq!(
            (m2.root, m2.scanned_at, m2.scan_millis),
            (dir.clone(), 42, 7)
        );
        assert_eq!(m2.exclude, exclude);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&file).unwrap();
    }
}
