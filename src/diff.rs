//! Comparison of the current tree against a baseline (an older snapshot or
//! the tree as it was before a refresh). Nodes are matched by path.

use crate::tree::{Entry, Kind, NodeId, SortKey, Tree};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// `map` value for nodes outside the compared region.
const OUTSIDE: u32 = u32::MAX;
/// `map` value for nodes with no counterpart in the baseline.
const NEW: u32 = u32::MAX - 1;

pub enum Change {
    New,
    Delta(i64),
}

/// One row of the changes list. `cur` is `None` for removed entries and
/// `base` is `None` for new ones.
pub struct ChangeItem {
    pub cur: Option<NodeId>,
    pub base: Option<NodeId>,
    pub delta: i64,
}

#[derive(Default)]
pub struct Summary {
    pub grown: u64,
    pub shrunk: u64,
    pub new: u64,
    pub removed: u64,
}

pub struct Diff {
    pub base: Tree,
    /// When the baseline was scanned (unix seconds).
    pub base_time: u64,
    /// Current node id -> baseline node id (or OUTSIDE / NEW).
    map: Vec<u32>,
    /// Topmost matched pair; everything compared lies below it.
    anchor: Option<(NodeId, NodeId)>,
    built_for: Option<u64>,
}

impl Diff {
    /// The baseline entry's name must be its full path.
    pub fn new(base: Entry, base_time: u64) -> Diff {
        Diff::from_tree(Tree::from_entry(base, SortKey::Name), base_time)
    }

    pub fn from_tree(base: Tree, base_time: u64) -> Diff {
        Diff {
            base,
            base_time,
            map: Vec::new(),
            anchor: None,
            built_for: None,
        }
    }

    pub fn base_path(&self) -> PathBuf {
        self.base.path(self.base.root)
    }

    /// Whether the baseline contains `path`.
    pub fn covers(&self, path: &Path) -> bool {
        path.starts_with(self.base_path())
    }

    pub fn overlaps(&self) -> bool {
        self.anchor.is_some()
    }

    /// Recomputes the node mapping if the current tree changed shape.
    pub fn sync(&mut self, cur: &Tree, generation: u64) {
        if self.built_for == Some(generation) {
            return;
        }
        self.built_for = Some(generation);
        self.map = vec![OUTSIDE; cur.nodes.len()];
        self.anchor = find_anchor(cur, &self.base);
        let Some((c0, b0)) = self.anchor else { return };
        self.map[c0 as usize] = b0;
        let mut stack = vec![(c0, b0)];
        while let Some((c, b)) = stack.pop() {
            let cch = cur.children(c);
            if cch.is_empty() {
                continue;
            }
            let bch = self.base.children(b);
            let lookup: Option<HashMap<&[u8], NodeId>> = (bch.len() > 16)
                .then(|| bch.iter().map(|&y| (&*self.base.node(y).name, y)).collect());
            for &x in cch {
                let name = &*cur.node(x).name;
                let found = match &lookup {
                    Some(m) => m.get(name).copied(),
                    None => bch
                        .iter()
                        .copied()
                        .find(|&y| &*self.base.node(y).name == name),
                };
                match found {
                    Some(y) => {
                        self.map[x as usize] = y;
                        stack.push((x, y));
                    }
                    None => mark_new(cur, x, &mut self.map),
                }
            }
        }
    }

    pub fn change(&self, cur: &Tree, id: NodeId, use_disk: bool) -> Option<Change> {
        match *self.map.get(id as usize)? {
            OUTSIDE => None,
            NEW => Some(Change::New),
            b => Some(Change::Delta(
                metric(cur, id, use_disk) as i64 - metric(&self.base, b, use_disk) as i64,
            )),
        }
    }

    /// The largest changes below `from`: changed files, plus new and removed
    /// entries (directories as a whole).
    pub fn changes(
        &self,
        cur: &Tree,
        from: NodeId,
        use_disk: bool,
        limit: usize,
    ) -> (Vec<ChangeItem>, Summary) {
        let mut sum = Summary::default();
        let mut heap: BinaryHeap<std::cmp::Reverse<(u64, i64, u32, u32)>> = BinaryHeap::new();
        let mut push = |item: ChangeItem, sum: &mut Summary| {
            match (item.cur, item.base) {
                (_, None) => sum.new += 1,
                (None, _) => sum.removed += 1,
                _ => {}
            }
            if item.delta > 0 {
                sum.grown += item.delta as u64;
            } else {
                sum.shrunk += item.delta.unsigned_abs();
            }
            let key = (
                item.delta.unsigned_abs(),
                item.delta,
                item.cur.unwrap_or(u32::MAX),
                item.base.unwrap_or(u32::MAX),
            );
            if heap.len() < limit {
                heap.push(std::cmp::Reverse(key));
            } else if heap.peek().is_some_and(|m| m.0 < key) {
                heap.pop();
                heap.push(std::cmp::Reverse(key));
            }
        };

        // Start at `from` if it was matched, else at the anchor if it lies below.
        let start = match self.map.get(from as usize).copied() {
            Some(NEW) => {
                let d = metric(cur, from, use_disk) as i64;
                push(
                    ChangeItem {
                        cur: Some(from),
                        base: None,
                        delta: d,
                    },
                    &mut sum,
                );
                None
            }
            Some(OUTSIDE) | None => self.anchor.filter(|&(c, _)| cur.is_ancestor(from, c)),
            Some(b) => Some((from, b)),
        };

        let mut stack: Vec<(NodeId, NodeId)> = start.into_iter().collect();
        while let Some((c, b)) = stack.pop() {
            let (cn, bn) = (cur.node(c), self.base.node(b));
            if cn.kind != Kind::Dir || bn.kind != Kind::Dir {
                let d = metric(cur, c, use_disk) as i64 - metric(&self.base, b, use_disk) as i64;
                if d != 0 {
                    push(
                        ChangeItem {
                            cur: Some(c),
                            base: Some(b),
                            delta: d,
                        },
                        &mut sum,
                    );
                }
                continue;
            }
            let mut matched: HashSet<NodeId> = HashSet::new();
            for &x in cur.children(c) {
                match self.map[x as usize] {
                    NEW => {
                        let d = metric(cur, x, use_disk) as i64;
                        push(
                            ChangeItem {
                                cur: Some(x),
                                base: None,
                                delta: d,
                            },
                            &mut sum,
                        );
                    }
                    OUTSIDE => {}
                    y => {
                        matched.insert(y);
                        // Only descend where something changed.
                        let (xn, yn) = (cur.node(x), self.base.node(y));
                        if xn.size != yn.size || xn.disk != yn.disk || xn.items != yn.items {
                            stack.push((x, y));
                        }
                    }
                }
            }
            for &y in self.base.children(b) {
                if !matched.contains(&y) {
                    let d = -(metric(&self.base, y, use_disk) as i64);
                    push(
                        ChangeItem {
                            cur: None,
                            base: Some(y),
                            delta: d,
                        },
                        &mut sum,
                    );
                }
            }
        }

        let mut v: Vec<_> = heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        let items = v
            .into_iter()
            .map(|(_, delta, c, b)| ChangeItem {
                cur: (c != u32::MAX).then_some(c),
                base: (b != u32::MAX).then_some(b),
                delta,
            })
            .collect();
        (items, sum)
    }
}

fn metric(t: &Tree, id: NodeId, use_disk: bool) -> u64 {
    let n = t.node(id);
    if use_disk { n.disk } else { n.size }
}

fn mark_new(cur: &Tree, id: NodeId, map: &mut [u32]) {
    let mut stack = vec![id];
    while let Some(x) = stack.pop() {
        map[x as usize] = NEW;
        stack.extend_from_slice(cur.children(x));
    }
}

/// Walks `t` from its root along the components of `rel`.
fn resolve(t: &Tree, rel: &Path) -> Option<NodeId> {
    let mut id = t.root;
    for c in rel.components() {
        match c {
            Component::Normal(name) => id = t.find_child(id, name.as_bytes())?,
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(id)
}

/// Finds the deepest common point of the two trees by root path.
fn find_anchor(cur: &Tree, base: &Tree) -> Option<(NodeId, NodeId)> {
    let cp = PathBuf::from(OsStr::from_bytes(&cur.root_node().name));
    let bp = PathBuf::from(OsStr::from_bytes(&base.root_node().name));
    if let Ok(rel) = bp.strip_prefix(&cp) {
        Some((resolve(cur, rel)?, base.root))
    } else if let Ok(rel) = cp.strip_prefix(&bp) {
        Some((cur.root, resolve(base, rel)?))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.as_bytes().into(),
            kind: Kind::File,
            flags: 0,
            size,
            disk: size,
            items: 1,
            children: vec![],
        }
    }

    fn dir(name: &str, children: Vec<Entry>) -> Entry {
        let mut d = file(name, 0);
        d.kind = Kind::Dir;
        for c in &children {
            d.size += c.size;
            d.disk += c.disk;
            d.items += c.items;
        }
        d.children = children;
        d
    }

    #[test]
    fn detects_changes() {
        let old = dir(
            "/r",
            vec![
                dir("a", vec![file("x", 10), file("y", 5)]),
                dir("gone", vec![file("z", 7)]),
                file("same", 3),
            ],
        );
        let new = dir(
            "/r",
            vec![
                dir("a", vec![file("x", 30), file("w", 1)]),
                dir("fresh", vec![file("q", 4)]),
                file("same", 3),
            ],
        );
        let cur = Tree::from_entry(new, SortKey::Disk);
        let mut d = Diff::new(old, 0);
        d.sync(&cur, 1);
        let (items, sum) = d.changes(&cur, cur.root, true, 100);
        let mut got: Vec<(String, i64)> = items
            .iter()
            .map(|i| {
                let name = match i.cur {
                    Some(c) => cur.node(c).name_lossy().into_owned(),
                    None => d.base.node(i.base.unwrap()).name_lossy().into_owned(),
                };
                (name, i.delta)
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("fresh".into(), 4),
                ("gone".into(), -7),
                ("w".into(), 1),
                ("x".into(), 20),
                ("y".into(), -5),
            ]
        );
        assert_eq!(
            (sum.new, sum.removed, sum.grown, sum.shrunk),
            (2, 2, 25, 12)
        );
        let a = cur.find_child(cur.root, b"a").unwrap();
        assert!(matches!(d.change(&cur, a, true), Some(Change::Delta(16))));
    }

    #[test]
    fn baseline_is_subtree() {
        let old = dir("/r/a", vec![file("x", 10)]);
        let new = dir("/r", vec![dir("a", vec![file("x", 12)]), file("b", 1)]);
        let cur = Tree::from_entry(new, SortKey::Disk);
        let mut d = Diff::new(old, 0);
        d.sync(&cur, 1);
        let b = cur.find_child(cur.root, b"b").unwrap();
        assert!(d.change(&cur, b, true).is_none());
        let (items, _) = d.changes(&cur, cur.root, true, 100);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].delta, 2);
    }
}
