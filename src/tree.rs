//! Arena-backed file tree. Nodes reference each other by `u32` index, which
//! keeps nodes small and makes the tree cheap to traverse and serialize.

use std::path::PathBuf;

pub type NodeId = u32;
pub const NO_PARENT: NodeId = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    Symlink = 2,
    Other = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            0 => Kind::File,
            1 => Kind::Dir,
            2 => Kind::Symlink,
            3 => Kind::Other,
            _ => return None,
        })
    }
}

/// Flags describing why a node may be incomplete.
pub mod flags {
    /// Reading the directory (or stat-ing the entry) failed.
    pub const ERROR: u8 = 1;
    /// A descendant had an error.
    pub const SUB_ERROR: u8 = 2;
    /// Directory is a mount point of another filesystem and was skipped.
    pub const OTHER_FS: u8 = 4;
    /// Hard link to an inode already counted elsewhere; size counted as zero.
    pub const HARDLINK: u8 = 8;
}

/// Entry produced by the scanner (or snapshot loader) before being placed in
/// the arena. Sizes are aggregates including all descendants.
pub struct Entry {
    pub name: Box<[u8]>,
    pub kind: Kind,
    pub flags: u8,
    pub size: u64,
    pub disk: u64,
    pub items: u64,
    pub children: Vec<Entry>,
}

pub struct Node {
    pub name: Box<[u8]>,
    pub children: Box<[NodeId]>,
    /// Apparent size (sum of st_size), aggregated.
    pub size: u64,
    /// Allocated size (st_blocks * 512), aggregated.
    pub disk: u64,
    /// Number of entries in this subtree, including the node itself.
    pub items: u64,
    pub parent: NodeId,
    pub kind: Kind,
    pub flags: u8,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn name_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Disk,
    Size,
    Items,
    Name,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
    /// Nodes no longer reachable from `root` (left behind by refreshes).
    pub garbage: usize,
}

impl Tree {
    pub fn from_entry(root: Entry, sort: SortKey) -> Tree {
        let mut tree = Tree {
            nodes: Vec::with_capacity(root.items as usize),
            root: 0,
            garbage: 0,
        };
        tree.root = tree.append(root, NO_PARENT, sort);
        tree
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn root_node(&self) -> &Node {
        self.node(self.root)
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.node(id).children
    }

    /// Appends an entry subtree to the arena, returning the new node's id.
    fn append(&mut self, e: Entry, parent: NodeId, sort: SortKey) -> NodeId {
        let id = self.nodes.len() as NodeId;
        let Entry {
            name,
            kind,
            flags,
            size,
            disk,
            items,
            children,
        } = e;
        self.nodes.push(Node {
            name,
            children: Box::new([]),
            size,
            disk,
            items,
            parent,
            kind,
            flags,
        });
        if !children.is_empty() {
            let mut ids: Box<[NodeId]> = children
                .into_iter()
                .map(|c| self.append(c, id, sort))
                .collect();
            self.sort_ids(&mut ids, sort);
            self.nodes[id as usize].children = ids;
        }
        id
    }

    fn sort_ids(&self, ids: &mut [NodeId], sort: SortKey) {
        sort_node_ids(&self.nodes, ids, sort);
    }

    fn sort_children_of(&mut self, id: NodeId, sort: SortKey) {
        let mut ids = std::mem::take(&mut self.nodes[id as usize].children);
        self.sort_ids(&mut ids, sort);
        self.nodes[id as usize].children = ids;
    }

    /// Re-sorts the children of every reachable directory.
    pub fn sort_all(&mut self, sort: SortKey) {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if self.nodes[id as usize].children.is_empty() {
                continue;
            }
            self.sort_children_of(id, sort);
            stack.extend_from_slice(&self.nodes[id as usize].children);
        }
    }

    /// Replaces the subtree at `old` with a freshly scanned entry, fixing up
    /// ancestor aggregates. Returns the id of the new subtree root.
    pub fn replace(&mut self, old: NodeId, mut e: Entry, sort: SortKey) -> NodeId {
        let parent = self.node(old).parent;
        // Keep the original name: the root node's name is its full path.
        e.name = self.node(old).name.clone();
        let (old_size, old_disk, old_items) = {
            let n = self.node(old);
            (n.size, n.disk, n.items)
        };
        self.garbage += old_items as usize;
        let new = self.append(e, parent, sort);
        if parent == NO_PARENT {
            self.root = new;
            return new;
        }
        let new_has_err = self.node(new).flags & (flags::ERROR | flags::SUB_ERROR) != 0;
        for c in self.nodes[parent as usize].children.iter_mut() {
            if *c == old {
                *c = new;
            }
        }
        let (new_size, new_disk, new_items) = {
            let n = self.node(new);
            (n.size, n.disk, n.items)
        };
        let mut p = parent;
        while p != NO_PARENT {
            let n = &mut self.nodes[p as usize];
            n.size = n.size - old_size + new_size;
            n.disk = n.disk - old_disk + new_disk;
            n.items = n.items - old_items + new_items;
            if new_has_err {
                n.flags |= flags::SUB_ERROR;
            }
            let next = n.parent;
            self.sort_children_of(p, sort);
            p = next;
        }
        new
    }

    /// Detaches the subtree at `id` (after it was deleted from disk),
    /// subtracting its sizes from all ancestors.
    pub fn remove(&mut self, id: NodeId, sort: SortKey) {
        let (parent, size, disk, items) = {
            let n = self.node(id);
            (n.parent, n.size, n.disk, n.items)
        };
        if parent == NO_PARENT {
            return;
        }
        let siblings = &mut self.nodes[parent as usize].children;
        *siblings = siblings.iter().copied().filter(|&c| c != id).collect();
        self.garbage += items as usize;
        let mut p = parent;
        while p != NO_PARENT {
            let n = &mut self.nodes[p as usize];
            n.size -= size;
            n.disk -= disk;
            n.items -= items;
            let next = n.parent;
            self.sort_children_of(p, sort);
            p = next;
        }
    }

    /// Full filesystem path of a node.
    pub fn path(&self, id: NodeId) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != NO_PARENT {
            parts.push(cur);
            cur = self.node(cur).parent;
        }
        let mut p = PathBuf::new();
        for &id in parts.iter().rev() {
            p.push(std::ffi::OsStr::from_bytes(&self.node(id).name));
        }
        p
    }

    /// Path of `id` relative to `ancestor`, for display.
    pub fn rel_path(&self, ancestor: NodeId, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ancestor && cur != NO_PARENT {
            parts.push(self.node(cur).name_lossy().into_owned());
            cur = self.node(cur).parent;
        }
        parts.reverse();
        parts.join("/")
    }

    pub fn find_child(&self, id: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children(id)
            .iter()
            .copied()
            .find(|&c| &*self.node(c).name == name)
    }

    pub fn is_ancestor(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        while id != NO_PARENT {
            if id == ancestor {
                return true;
            }
            id = self.node(id).parent;
        }
        false
    }

    /// Chain of names from `ancestor` (exclusive) down to `id` (inclusive).
    pub fn name_chain(&self, ancestor: NodeId, id: NodeId) -> Vec<Box<[u8]>> {
        let mut chain = Vec::new();
        let mut cur = id;
        while cur != ancestor && cur != NO_PARENT {
            chain.push(self.node(cur).name.clone());
            cur = self.node(cur).parent;
        }
        chain.reverse();
        chain
    }

    /// Follows a name chain from `start`, stopping at the deepest match.
    pub fn resolve_chain(&self, start: NodeId, chain: &[Box<[u8]>]) -> NodeId {
        let mut cur = start;
        for name in chain {
            match self.find_child(cur, name) {
                Some(c) => cur = c,
                None => break,
            }
        }
        cur
    }

    /// Copies the subtree at `id` out of the arena (used to compact it).
    pub fn to_entry(&self, id: NodeId) -> Entry {
        let n = self.node(id);
        Entry {
            name: n.name.clone(),
            kind: n.kind,
            flags: n.flags,
            size: n.size,
            disk: n.disk,
            items: n.items,
            children: n.children.iter().map(|&c| self.to_entry(c)).collect(),
        }
    }
}

pub fn sort_node_ids(n: &[Node], ids: &mut [NodeId], sort: SortKey) {
    match sort {
        SortKey::Disk => ids.sort_unstable_by_key(|&i| std::cmp::Reverse(n[i as usize].disk)),
        SortKey::Size => ids.sort_unstable_by_key(|&i| std::cmp::Reverse(n[i as usize].size)),
        SortKey::Items => ids.sort_unstable_by_key(|&i| std::cmp::Reverse(n[i as usize].items)),
        SortKey::Name => ids.sort_unstable_by(|&a, &b| n[a as usize].name.cmp(&n[b as usize].name)),
    }
}
