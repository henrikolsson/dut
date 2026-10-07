use crate::scan::{self, Estimate, Progress, ScanOptions};
use crate::snapshot::{self, Meta};
use crate::tree::{Entry, Kind, NO_PARENT, NodeId, SortKey, Tree, flags};
use crate::treemap::Rect;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOP_FILES: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Tree,
    Treemap,
    Files,
    Types,
}

impl View {
    pub const ALL: [View; 4] = [View::Tree, View::Treemap, View::Files, View::Types];

    pub fn title(self) -> &'static str {
        match self {
            View::Tree => "Tree",
            View::Treemap => "Treemap",
            View::Files => "Largest files",
            View::Types => "File types",
        }
    }
}

pub struct Row {
    pub id: NodeId,
    pub prefix: String,
}

/// Selection + scroll offset for a list.
#[derive(Default, Clone, Copy)]
pub struct ListPos {
    pub sel: usize,
    pub offset: usize,
}

impl ListPos {
    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn clamp(&mut self, len: usize) {
        self.sel = self.sel.min(len.saturating_sub(1));
    }

    /// Adjusts `offset` so `sel` is visible in a viewport of `height` rows.
    pub fn scroll(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.sel < self.offset {
            self.offset = self.sel;
        } else if self.sel >= self.offset + height {
            self.offset = self.sel + 1 - height;
        }
    }
}

pub struct TypeStat {
    pub ext: String,
    pub bytes: u64,
    pub count: u64,
}

pub struct Job {
    /// Node being refreshed, or `None` for the full tree / initial scan.
    pub target: Option<NodeId>,
    pub path: PathBuf,
    pub progress: Arc<Progress>,
    pub started: Instant,
    rx: Receiver<anyhow::Result<Entry>>,
}

pub enum Mode {
    Normal,
    Help,
    SavePrompt(String),
}

pub struct App {
    pub tree: Option<Tree>,
    pub meta: Meta,
    pub opts: ScanOptions,
    pub sort: SortKey,
    pub use_disk: bool,
    pub view: View,
    pub view_root: NodeId,
    pub mode: Mode,
    pub job: Option<Job>,
    pub status: Option<(String, Instant)>,
    pub save_path: Option<PathBuf>,
    /// Bumped whenever the tree changes, to invalidate caches.
    pub generation: u64,
    /// Totals from a loaded snapshot, used as the estimate for its rescan.
    pub initial_estimate: Option<Estimate>,

    // Tree view
    pub expanded: HashSet<NodeId>,
    pub rows: Vec<Row>,
    rows_dirty: bool,
    pub tree_pos: ListPos,

    // Treemap view
    pub tm_sel: usize,
    /// Node to select once the treemap is next laid out.
    pub tm_focus: Option<NodeId>,
    /// Rects of the top-level treemap cells as last drawn, for spatial navigation.
    pub tm_rects: Vec<(NodeId, Rect)>,

    // Files / types views
    pub files: Vec<NodeId>,
    pub files_pos: ListPos,
    pub files_filter: Option<String>,
    files_key: Option<(u64, NodeId, bool, Option<String>)>,
    pub types: Vec<TypeStat>,
    pub types_pos: ListPos,
    types_key: Option<(u64, NodeId, bool)>,

    quit: bool,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl App {
    pub fn new(meta: Meta, opts: ScanOptions, use_disk: bool, save_path: Option<PathBuf>) -> App {
        App {
            tree: None,
            meta,
            opts,
            sort: if use_disk {
                SortKey::Disk
            } else {
                SortKey::Size
            },
            use_disk,
            view: View::Tree,
            view_root: 0,
            mode: Mode::Normal,
            job: None,
            status: None,
            save_path,
            generation: 0,
            initial_estimate: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            tree_pos: ListPos::default(),
            tm_sel: 0,
            tm_focus: None,
            tm_rects: Vec::new(),
            files: Vec::new(),
            files_pos: ListPos::default(),
            files_filter: None,
            files_key: None,
            types: Vec::new(),
            types_pos: ListPos::default(),
            types_key: None,
            quit: false,
        }
    }

    pub fn set_tree(&mut self, entry: Entry) {
        let tree = Tree::from_entry(entry, self.sort);
        self.view_root = tree.root;
        self.tree = Some(tree);
        self.generation += 1;
        self.rows_dirty = true;
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    // ---- background scanning -------------------------------------------

    pub fn start_scan(&mut self, target: Option<NodeId>) {
        if self.job.is_some() {
            self.flash("a scan is already running");
            return;
        }
        let path = match (target, &self.tree) {
            (Some(id), Some(t)) => t.path(id),
            _ => self.meta.root.clone(),
        };
        let estimate = match (target, &self.tree) {
            (_, Some(t)) => {
                let n = t.node(target.unwrap_or(t.root));
                Some(Estimate {
                    items: n.items,
                    disk: n.disk,
                    source: "previous scan",
                })
            }
            _ => self
                .initial_estimate
                .take()
                .or_else(|| scan::estimate_fs(&path, self.opts.one_file_system)),
        };
        let progress = Arc::new(Progress::with_estimate(estimate));
        let (tx, rx) = std::sync::mpsc::channel();
        let (p, opts, scan_path) = (progress.clone(), self.opts, path.clone());
        std::thread::Builder::new()
            .name("dut-scan".into())
            .spawn(move || {
                let _ = tx.send(scan::scan(&scan_path, opts, &p));
            })
            .expect("spawning scan thread");
        self.job = Some(Job {
            target,
            path,
            progress,
            started: Instant::now(),
            rx,
        });
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let res = match job.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("scan thread died")),
        };
        let job = self.job.take().unwrap();
        if job
            .progress
            .cancel
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            if self.tree.is_none() {
                self.quit = true;
            }
            self.flash("scan cancelled");
            return;
        }
        let elapsed = job.started.elapsed();
        let entry = match res {
            Ok(e) => e,
            Err(e) => {
                if self.tree.is_none() {
                    // Nothing to show; surface the error and exit.
                    eprintln!("dut: {}: {e:#}", job.path.display());
                    self.quit = true;
                }
                self.flash(format!("scan failed: {e:#}"));
                return;
            }
        };
        let errors = job
            .progress
            .errors
            .load(std::sync::atomic::Ordering::Relaxed);
        if self.tree.is_none() {
            self.set_tree(entry);
        } else {
            self.apply_refresh(job.target, entry);
        }
        if job.target.is_none() {
            self.meta.scanned_at = now_unix();
            self.meta.scan_millis = elapsed.as_millis() as u64;
        }
        let mut msg = format!(
            "scanned {} in {}",
            job.path.display(),
            crate::fmt::duration_ms(elapsed.as_millis() as u64)
        );
        if errors > 0 {
            msg += &format!(" ({errors} errors)");
        }
        self.flash(msg);
    }

    /// Swaps in a rescanned subtree while preserving the view state (zoom,
    /// expanded dirs, selection) by re-resolving it via names.
    fn apply_refresh(&mut self, target: Option<NodeId>, entry: Entry) {
        let sort = self.sort;
        let tree = self.tree.as_mut().unwrap();
        let root = tree.root;
        let cursor = self.rows.get(self.tree_pos.sel).map(|r| r.id);
        let chain = |id: NodeId| tree.name_chain(root, id);
        let view_chain = chain(self.view_root);
        let cursor_chain = cursor.map(chain);
        let expanded: Vec<_> = self.expanded.iter().map(|&id| chain(id)).collect();

        tree.replace(target.unwrap_or(root), entry, sort);
        if tree.garbage > tree.nodes.len() / 2 {
            // Too much dead weight from refreshes; rebuild the arena.
            let e = tree.to_entry(tree.root);
            *tree = Tree::from_entry(e, sort);
        }

        let root = tree.root;
        self.view_root = tree.resolve_chain(root, &view_chain);
        self.expanded = expanded
            .iter()
            .map(|c| tree.resolve_chain(root, c))
            .collect();
        let cursor = cursor_chain.map(|c| tree.resolve_chain(root, &c));
        self.generation += 1;
        self.rebuild_rows();
        if let Some(c) = cursor {
            self.select_id(c);
        }
    }

    // ---- tree view -------------------------------------------------------

    pub fn ensure_rows(&mut self) {
        if self.rows_dirty {
            self.rebuild_rows();
        }
    }

    fn rebuild_rows(&mut self) {
        let Some(tree) = &self.tree else { return };
        let prev = self.rows.get(self.tree_pos.sel).map(|r| r.id);
        self.rows.clear();
        let mut prefix = String::new();
        push_rows(
            tree,
            &self.expanded,
            self.view_root,
            0,
            &mut prefix,
            &mut self.rows,
        );
        self.rows_dirty = false;
        match prev {
            Some(id) => self.select_id(id),
            None => self.tree_pos.clamp(self.rows.len()),
        }
    }

    fn select_id(&mut self, id: NodeId) {
        if let Some(i) = self.rows.iter().position(|r| r.id == id) {
            self.tree_pos.sel = i;
        } else {
            self.tree_pos.clamp(self.rows.len());
        }
    }

    pub fn selected(&self) -> Option<NodeId> {
        match self.view {
            View::Tree => self.rows.get(self.tree_pos.sel).map(|r| r.id),
            View::Treemap => self.tm_rects.get(self.tm_sel).map(|r| r.0),
            View::Files => self.files.get(self.files_pos.sel).copied(),
            View::Types => None,
        }
    }

    fn zoom_into(&mut self, id: NodeId) {
        let tree = self.tree.as_ref().unwrap();
        if !tree.node(id).is_dir() {
            return;
        }
        self.view_root = id;
        self.tree_pos = ListPos::default();
        self.tm_sel = 0;
        self.rows_dirty = true;
    }

    fn zoom_out(&mut self) {
        let tree = self.tree.as_ref().unwrap();
        let parent = tree.node(self.view_root).parent;
        if parent == NO_PARENT {
            return;
        }
        let old = self.view_root;
        self.view_root = parent;
        self.tm_focus = Some(old);
        self.rebuild_rows();
        self.select_id(old);
    }

    /// Shows `id` in the tree view: zooms out if needed and expands ancestors.
    fn reveal(&mut self, id: NodeId) {
        let tree = self.tree.as_ref().unwrap();
        if !tree.is_ancestor(self.view_root, id) {
            self.view_root = tree.root;
        }
        let mut p = tree.node(id).parent;
        while p != NO_PARENT && p != self.view_root {
            self.expanded.insert(p);
            p = tree.node(p).parent;
        }
        self.view = View::Tree;
        self.rebuild_rows();
        self.select_id(id);
    }

    fn tree_key(&mut self, key: KeyEvent, page: isize) {
        let len = self.rows.len();
        let cur = self.rows.get(self.tree_pos.sel).map(|r| r.id);
        let tree = self.tree.as_ref().unwrap();
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(id) = cur.filter(|&id| !tree.children(id).is_empty()) {
                    if self.expanded.insert(id) {
                        self.rows_dirty = true;
                    } else {
                        self.tree_pos.move_by(1, len);
                    }
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let Some(id) = cur else {
                    return self.zoom_out();
                };
                if self.expanded.remove(&id) {
                    self.rows_dirty = true;
                } else {
                    let parent = tree.node(id).parent;
                    if parent == self.view_root || parent == NO_PARENT {
                        self.zoom_out();
                    } else {
                        self.select_id(parent);
                    }
                }
            }
            KeyCode::Char(' ') => {
                if let Some(id) = cur.filter(|&id| !tree.children(id).is_empty()) {
                    if !self.expanded.remove(&id) {
                        self.expanded.insert(id);
                    }
                    self.rows_dirty = true;
                }
            }
            KeyCode::Char('*') => {
                // Expand everything below the cursor (bounded, to stay responsive).
                if let Some(id) = cur {
                    let mut stack = vec![id];
                    let mut budget = 10_000;
                    while let Some(n) = stack.pop() {
                        if budget == 0 {
                            break;
                        }
                        if !tree.children(n).is_empty() {
                            self.expanded.insert(n);
                            budget -= 1;
                            stack.extend_from_slice(tree.children(n));
                        }
                    }
                    self.rows_dirty = true;
                }
            }
            KeyCode::Char('-') => {
                self.expanded.clear();
                self.rows_dirty = true;
            }
            KeyCode::Enter => {
                if let Some(id) = cur {
                    self.zoom_into(id);
                }
            }
            _ => self.list_nav(key, page, Which::Tree),
        }
    }

    // ---- treemap view ----------------------------------------------------

    fn treemap_key(&mut self, key: KeyEvent) {
        let n = self.tm_rects.len();
        let cur = self.tm_sel;
        match key.code {
            KeyCode::Char('j') | KeyCode::Tab => self.tm_sel = (cur + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::BackTab => self.tm_sel = cur.saturating_sub(1),
            KeyCode::Left | KeyCode::Char('h') => self.tm_move(-1.0, 0.0),
            KeyCode::Right | KeyCode::Char('l') => self.tm_move(1.0, 0.0),
            KeyCode::Up => self.tm_move(0.0, -1.0),
            KeyCode::Down => self.tm_move(0.0, 1.0),
            KeyCode::Home | KeyCode::Char('g') => self.tm_sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.tm_sel = n.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(&(id, _)) = self.tm_rects.get(cur) {
                    self.zoom_into(id);
                }
            }
            _ => {}
        }
    }

    /// Moves the treemap selection to the nearest cell in direction (dx, dy).
    fn tm_move(&mut self, dx: f64, dy: f64) {
        let Some(&(_, cur)) = self.tm_rects.get(self.tm_sel) else {
            return;
        };
        let (cx, cy) = (cur.x + cur.w / 2.0, cur.y + cur.h / 2.0);
        let mut best: Option<(usize, f64)> = None;
        for (i, (_, r)) in self.tm_rects.iter().enumerate() {
            if i == self.tm_sel {
                continue;
            }
            // Must lie beyond the current cell's edge in the requested direction.
            let ahead = match (dx as i8, dy as i8) {
                (1, _) => r.x >= cur.x + cur.w - 0.5,
                (-1, _) => r.x + r.w <= cur.x + 0.5,
                (_, 1) => r.y >= cur.y + cur.h - 0.5,
                _ => r.y + r.h <= cur.y + 0.5,
            };
            if !ahead {
                continue;
            }
            // Clamp our center into the candidate to measure edge distance.
            let px = cx.clamp(r.x, r.x + r.w);
            let py = cy.clamp(r.y, r.y + r.h);
            let (ex, ey) = (px - cx, py - cy);
            // Penalise off-axis distance so movement feels straight.
            let d = if dx != 0.0 {
                ex.abs() + 3.0 * ey.abs()
            } else {
                3.0 * ex.abs() + ey.abs()
            };
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        if let Some((i, _)) = best {
            self.tm_sel = i;
        }
    }

    // ---- files / types views ----------------------------------------------

    pub fn ensure_files(&mut self) {
        let key = (
            self.generation,
            self.view_root,
            self.use_disk,
            self.files_filter.clone(),
        );
        if self.files_key.as_ref() == Some(&key) {
            return;
        }
        let tree = self.tree.as_ref().unwrap();
        let use_disk = self.use_disk;
        let filter = self.files_filter.as_deref();
        let mut heap: BinaryHeap<std::cmp::Reverse<(u64, NodeId)>> = BinaryHeap::new();
        let mut stack = vec![self.view_root];
        while let Some(id) = stack.pop() {
            let n = tree.node(id);
            if n.kind == Kind::Dir {
                stack.extend_from_slice(&n.children);
                continue;
            }
            if filter.is_some_and(|f| extension(&n.name) != f) {
                continue;
            }
            let s = if use_disk { n.disk } else { n.size };
            if heap.len() < TOP_FILES {
                heap.push(std::cmp::Reverse((s, id)));
            } else if heap.peek().is_some_and(|m| m.0.0 < s) {
                heap.pop();
                heap.push(std::cmp::Reverse((s, id)));
            }
        }
        let mut v: Vec<_> = heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        self.files = v.into_iter().map(|(_, id)| id).collect();
        self.files_pos = ListPos::default();
        self.files_key = Some(key);
    }

    pub fn ensure_types(&mut self) {
        let key = (self.generation, self.view_root, self.use_disk);
        if self.types_key == Some(key) {
            return;
        }
        let tree = self.tree.as_ref().unwrap();
        let mut map: HashMap<String, (u64, u64)> = HashMap::new();
        let mut stack = vec![self.view_root];
        while let Some(id) = stack.pop() {
            let n = tree.node(id);
            if n.kind == Kind::Dir {
                stack.extend_from_slice(&n.children);
                continue;
            }
            let s = if self.use_disk { n.disk } else { n.size };
            let ext = extension(&n.name);
            let e = match map.get_mut(ext.as_str()) {
                Some(e) => e,
                None => map.entry(ext).or_default(),
            };
            e.0 += s;
            e.1 += 1;
        }
        let mut v: Vec<TypeStat> = map
            .into_iter()
            .map(|(ext, (bytes, count))| TypeStat { ext, bytes, count })
            .collect();
        v.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.ext.cmp(&b.ext)));
        self.types = v;
        self.types_pos = ListPos::default();
        self.types_key = Some(key);
    }

    fn list_nav(&mut self, key: KeyEvent, page: isize, which: Which) {
        let (pos, len) = match which {
            Which::Tree => (&mut self.tree_pos, self.rows.len()),
            Which::Files => (&mut self.files_pos, self.files.len()),
            Which::Types => (&mut self.types_pos, self.types.len()),
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => pos.move_by(1, len),
            KeyCode::Up | KeyCode::Char('k') => pos.move_by(-1, len),
            KeyCode::PageDown => pos.move_by(page, len),
            KeyCode::PageUp => pos.move_by(-page, len),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                pos.move_by(page / 2, len)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                pos.move_by(-page / 2, len)
            }
            KeyCode::Home | KeyCode::Char('g') => pos.sel = 0,
            KeyCode::End | KeyCode::Char('G') => pos.sel = len.saturating_sub(1),
            _ => {}
        }
    }

    // ---- input -------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent, page: isize) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if let Some(j) = &self.job {
                j.progress
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            self.quit = true;
            return;
        }
        match &mut self.mode {
            Mode::Help => {
                self.mode = Mode::Normal;
                return;
            }
            Mode::SavePrompt(buf) => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let path = PathBuf::from(std::mem::take(buf));
                        self.mode = Mode::Normal;
                        self.save(path);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                return;
            }
            Mode::Normal => {}
        }

        if self.tree.is_none() {
            // Initial scan in progress.
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                if let Some(j) = &self.job {
                    j.progress
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                self.quit = true;
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => {
                self.quit = true;
            }
            KeyCode::Esc => {
                if let Some(j) = &self.job {
                    j.progress
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                } else if self.view == View::Files && self.files_filter.is_some() {
                    self.files_filter = None;
                } else if self.view != View::Tree {
                    self.view = View::Tree;
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
            }
            KeyCode::Char('1') => self.view = View::Tree,
            KeyCode::Char('2') => self.view = View::Treemap,
            KeyCode::Char('3') => self.view = View::Files,
            KeyCode::Char('4') => self.view = View::Types,
            KeyCode::Char('v') => {
                let i = View::ALL.iter().position(|&v| v == self.view).unwrap();
                self.view = View::ALL[(i + 1) % View::ALL.len()];
            }
            KeyCode::Char('a') => {
                self.use_disk = !self.use_disk;
                if matches!(self.sort, SortKey::Disk | SortKey::Size) {
                    self.set_sort(if self.use_disk {
                        SortKey::Disk
                    } else {
                        SortKey::Size
                    });
                }
                self.flash(if self.use_disk {
                    "showing disk usage"
                } else {
                    "showing apparent size"
                });
            }
            KeyCode::Char('s') if ctrl => self.open_save_prompt(),
            KeyCode::Char('s') => {
                let next = match self.sort {
                    SortKey::Disk | SortKey::Size => SortKey::Items,
                    SortKey::Items => SortKey::Name,
                    SortKey::Name if self.use_disk => SortKey::Disk,
                    SortKey::Name => SortKey::Size,
                };
                self.set_sort(next);
            }
            KeyCode::Char('w') => self.open_save_prompt(),
            KeyCode::Char('r') => {
                let tree = self.tree.as_ref().unwrap();
                let target = self.selected().map(|id| {
                    if tree.node(id).is_dir() {
                        id
                    } else {
                        tree.node(id).parent
                    }
                });
                match target {
                    Some(id) if id != NO_PARENT => self.start_scan(Some(id)),
                    _ => self.start_scan(Some(self.view_root)),
                }
            }
            KeyCode::Char('R') => self.start_scan(None),
            KeyCode::Backspace | KeyCode::Char('u') if !ctrl => self.zoom_out(),
            _ => match self.view {
                View::Tree => self.tree_key(key, page),
                View::Treemap => self.treemap_key(key),
                View::Files => match key.code {
                    KeyCode::Enter => {
                        if let Some(id) = self.selected() {
                            self.reveal(id);
                        }
                    }
                    _ => self.list_nav(key, page, Which::Files),
                },
                View::Types => match key.code {
                    KeyCode::Enter => {
                        if let Some(t) = self.types.get(self.types_pos.sel) {
                            self.files_filter = Some(t.ext.clone());
                            self.view = View::Files;
                        }
                    }
                    _ => self.list_nav(key, page, Which::Types),
                },
            },
        }
    }

    fn set_sort(&mut self, sort: SortKey) {
        self.sort = sort;
        if let Some(t) = &mut self.tree {
            t.sort_all(sort);
        }
        self.tm_sel = 0;
        self.rows_dirty = true;
        self.generation += 1;
    }

    fn open_save_prompt(&mut self) {
        let default = self.save_path.clone().unwrap_or_else(|| {
            let base = self
                .meta
                .root
                .file_name()
                .map_or("root".into(), |n| n.to_string_lossy().into_owned());
            PathBuf::from(format!("{base}.dut"))
        });
        self.mode = Mode::SavePrompt(default.to_string_lossy().into_owned());
    }

    pub fn save(&mut self, path: PathBuf) {
        let Some(tree) = &self.tree else { return };
        let t0 = Instant::now();
        match snapshot::save(&path, tree, &self.meta) {
            Ok(()) => {
                let size = std::fs::metadata(&path).map_or(0, |m| m.len());
                self.flash(format!(
                    "saved {} ({}) in {}",
                    path.display(),
                    crate::fmt::size(size),
                    crate::fmt::duration_ms(t0.elapsed().as_millis() as u64)
                ));
                self.save_path = Some(path);
            }
            Err(e) => self.flash(format!("save failed: {e:#}")),
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let mut page = 20isize;
        while !self.quit {
            self.poll_job();
            if self.quit {
                break;
            }
            terminal.draw(|f| {
                page = (f.area().height as isize - 4).max(1);
                crate::ui::draw(f, &mut self)
            })?;
            let timeout = if self.job.is_some() {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(1000)
            };
            if event::poll(timeout)? {
                // Drain all pending events before redrawing, so key repeat
                // never queues up behind slow frames.
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k, page),
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Which {
    Tree,
    Files,
    Types,
}

fn push_rows(
    tree: &Tree,
    expanded: &HashSet<NodeId>,
    id: NodeId,
    depth: usize,
    prefix: &mut String,
    rows: &mut Vec<Row>,
) {
    let children = tree.children(id);
    for (i, &c) in children.iter().enumerate() {
        let last = i + 1 == children.len();
        let guide = match (depth, last) {
            (0, _) => "",
            (_, true) => "└─",
            (_, false) => "├─",
        };
        rows.push(Row {
            id: c,
            prefix: format!("{prefix}{guide}"),
        });
        if expanded.contains(&c) {
            let len = prefix.len();
            prefix.push_str(match (depth, last) {
                (0, _) => "",
                (_, true) => "  ",
                (_, false) => "│ ",
            });
            push_rows(tree, expanded, c, depth + 1, prefix, rows);
            prefix.truncate(len);
        }
    }
}

pub fn extension(name: &[u8]) -> String {
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            String::from_utf8_lossy(&name[i + 1..]).to_lowercase()
        }
        _ => "(none)".into(),
    }
}

pub fn has_problem(flags_: u8) -> bool {
    flags_ & (flags::ERROR | flags::SUB_ERROR) != 0
}
