use crate::diff::{ChangeItem, Diff, Summary};
use crate::scan::{self, Estimate, Progress, ScanOptions};
use crate::snapshot::{self, Meta};
use crate::tree::{Entry, Kind, NO_PARENT, NodeId, SortKey, Tree, flags};
use crate::treemap::Rect;
use anyhow::Context;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
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
    Changes,
    History,
}

impl View {
    pub const ALL: [View; 6] = [
        View::Tree,
        View::Treemap,
        View::Files,
        View::Types,
        View::Changes,
        View::History,
    ];

    pub fn title(self) -> &'static str {
        match self {
            View::Tree => "Tree",
            View::Treemap => "Treemap",
            View::Files => "Largest files",
            View::Types => "File types",
            View::Changes => "Changes",
            View::History => "History",
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Scan,
    Delete,
}

enum JobResult {
    /// A rescanned subtree.
    Scan(anyhow::Result<Entry>),
    Full(anyhow::Result<FullScan>),
    Delete(Vec<String>),
}

struct FullScan {
    tree: Tree,
    /// Sort order the tree was built with.
    sort: SortKey,
    meta: Meta,
    cache_err: Option<anyhow::Error>,
}

/// Size of the viewed directory in one snapshot of the history.
pub struct HistPoint {
    pub time: u64,
    /// `None` for the tree currently loaded.
    pub file: Option<PathBuf>,
    /// `None` if the directory did not exist in that snapshot.
    pub totals: Option<(u64, u64, u64)>,
    /// Immediate children: (name, size, disk).
    pub children: Vec<(Box<[u8]>, u64, u64)>,
}

impl HistPoint {
    fn from_tree(tree: &Tree, id: NodeId, time: u64, file: Option<PathBuf>) -> HistPoint {
        let n = tree.node(id);
        HistPoint {
            time,
            file,
            totals: Some((n.size, n.disk, n.items)),
            children: n
                .children
                .iter()
                .map(|&c| {
                    let c = tree.node(c);
                    (c.name.clone(), c.size, c.disk)
                })
                .collect(),
        }
    }
}

struct ViewState {
    view_root: Vec<Box<[u8]>>,
    cursor: Option<Vec<Box<[u8]>>>,
    expanded: Vec<Vec<Box<[u8]>>>,
}

pub struct Job {
    pub kind: JobKind,
    /// Node being refreshed/deleted, or `None` for the full tree / initial scan.
    pub target: Option<NodeId>,
    pub path: PathBuf,
    pub progress: Arc<Progress>,
    pub started: Instant,
    rx: Receiver<JobResult>,
}

pub enum Mode {
    Normal,
    Help,
    SavePrompt(String),
    ConfirmDelete(NodeId),
    /// Problems from a delete that did not fully succeed.
    Errors(Vec<String>),
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
    /// Bumped when nodes are added/removed (not on re-sorts).
    pub structure_gen: u64,
    pub allow_delete: bool,
    pub diff: Option<Diff>,
    /// Baseline to compare against once the initial scan completes.
    pub pending_baseline: Option<(Tree, u64)>,
    /// Whether full scans are automatically saved to the cache.
    pub use_cache: bool,
    /// Where the stale tree came from, if not this root's own snapshot.
    pub stale_source: Option<PathBuf>,
    /// The tree changed (subtree refresh, delete) since the cache was written.
    pub cache_dirty: bool,
    cache_err: Option<anyhow::Error>,
    /// Showing a cached snapshot while the background refresh runs.
    pub stale: bool,

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

    // History view
    pub history: Vec<HistPoint>,
    pub history_pos: ListPos,
    history_key: Option<(u64, NodeId)>,
    history_rx: Option<Receiver<HistPoint>>,

    // Changes view
    pub changes: Vec<ChangeItem>,
    pub changes_sum: Summary,
    pub changes_pos: ListPos,
    changes_key: Option<(u64, NodeId, bool)>,

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
            structure_gen: 0,
            allow_delete: true,
            diff: None,
            pending_baseline: None,
            use_cache: false,
            stale_source: None,
            cache_dirty: false,
            cache_err: None,
            stale: false,
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
            history: Vec::new(),
            history_pos: ListPos::default(),
            history_key: None,
            history_rx: None,
            changes: Vec::new(),
            changes_sum: Summary::default(),
            changes_pos: ListPos::default(),
            changes_key: None,
            quit: false,
        }
    }

    pub fn set_tree(&mut self, tree: Tree) {
        self.view_root = tree.root;
        self.tree = Some(tree);
        self.generation += 1;
        self.structure_gen += 1;
        self.rows_dirty = true;
        if let Some((base, time)) = self.pending_baseline.take() {
            self.set_baseline(Diff::from_tree(base, time));
        }
    }

    pub fn set_baseline(&mut self, diff: Diff) {
        self.diff = Some(diff);
        self.changes_key = None;
        self.ensure_diff();
        if !self.diff.as_ref().unwrap().overlaps() {
            self.diff = None;
            self.flash("snapshot to compare with does not overlap the scanned path");
        }
        self.generation += 1;
    }

    pub fn ensure_diff(&mut self) {
        if let (Some(d), Some(t)) = (&mut self.diff, &self.tree) {
            d.sync(t, self.structure_gen);
        }
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
            _ => self.initial_estimate.take().or_else(|| {
                // Excluded entries would make a filesystem-wide estimate wrong.
                self.opts
                    .exclude
                    .is_empty()
                    .then(|| scan::estimate_fs(&path, &self.opts))
                    .flatten()
            }),
        };
        let progress = Arc::new(Progress::with_estimate(estimate));
        let (tx, rx) = std::sync::mpsc::channel();
        let (p, opts, scan_path) = (progress.clone(), self.opts.clone(), path.clone());
        let (sort, cache, mut meta) = (self.sort, self.use_cache, self.meta.clone());
        std::thread::Builder::new()
            .name("dut-scan".into())
            .spawn(move || {
                let t0 = Instant::now();
                let res = scan::scan(&scan_path, opts, &p);
                let msg = match (target, res) {
                    (Some(_), res) => JobResult::Scan(res),
                    (None, Err(e)) => JobResult::Full(Err(e)),
                    (None, Ok(entry)) => {
                        // Build the arena and write the cache here, off the
                        // UI thread.
                        let tree = Tree::from_entry(entry, sort);
                        meta.scanned_at = now_unix();
                        meta.scan_millis = t0.elapsed().as_millis() as u64;
                        let cancelled = p.cancel.load(std::sync::atomic::Ordering::Relaxed);
                        let cache_err = if cache && !cancelled {
                            p.saving.store(true, std::sync::atomic::Ordering::Relaxed);
                            crate::cache::save(&tree, &meta).err()
                        } else {
                            None
                        };
                        JobResult::Full(Ok(FullScan {
                            tree,
                            sort,
                            meta,
                            cache_err,
                        }))
                    }
                };
                let _ = tx.send(msg);
            })
            .expect("spawning scan thread");
        self.job = Some(Job {
            kind: JobKind::Scan,
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
            Err(TryRecvError::Disconnected) => {
                JobResult::Scan(Err(anyhow::anyhow!("background thread died")))
            }
        };
        let job = self.job.take().unwrap();
        if let JobResult::Delete(errors) = res {
            return self.finish_delete(job, errors);
        }
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
        let result = match res {
            JobResult::Scan(r) => r.map(|e| self.apply_subtree(job.target.unwrap(), e)),
            JobResult::Full(r) => r.map(|full| self.apply_full(full)),
            JobResult::Delete(_) => unreachable!(),
        };
        if let Err(e) = result {
            if self.tree.is_none() {
                // Nothing to show; surface the error and exit.
                eprintln!("dut: {}: {e:#}", job.path.display());
                self.quit = true;
            }
            self.flash(format!("scan failed: {e:#}"));
            return;
        }
        let errors = job
            .progress
            .errors
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut msg = format!(
            "scanned {} in {}",
            job.path.display(),
            crate::fmt::duration_ms(elapsed.as_millis() as u64)
        );
        if errors > 0 {
            msg += &format!(" ({errors} errors)");
        }
        if let Some(e) = self.cache_err.take() {
            msg += &format!(" · cache not saved: {e:#}");
        }
        self.flash(msg);
    }

    /// Captures zoom, expanded dirs and selection as name chains, which
    /// survive the tree being rebuilt.
    fn capture_view(&self) -> ViewState {
        let tree = self.tree.as_ref().unwrap();
        let root = tree.root;
        let chain = |id: NodeId| tree.name_chain(root, id);
        ViewState {
            view_root: chain(self.view_root),
            cursor: self.rows.get(self.tree_pos.sel).map(|r| chain(r.id)),
            expanded: self.expanded.iter().map(|&id| chain(id)).collect(),
        }
    }

    fn restore_view(&mut self, vs: ViewState) {
        let tree = self.tree.as_ref().unwrap();
        let root = tree.root;
        self.view_root = tree.resolve_chain(root, &vs.view_root);
        self.expanded = vs
            .expanded
            .iter()
            .map(|c| tree.resolve_chain(root, c))
            .collect();
        let cursor = vs.cursor.map(|c| tree.resolve_chain(root, &c));
        self.generation += 1;
        self.structure_gen += 1;
        self.rebuild_rows();
        if let Some(c) = cursor {
            self.select_id(c);
        }
    }

    /// Installs a freshly scanned full tree. The tree it replaces becomes the
    /// baseline for the changes view, unless one is already set.
    fn apply_full(&mut self, full: FullScan) {
        let FullScan {
            mut tree,
            sort,
            meta,
            cache_err,
        } = full;
        if sort != self.sort {
            tree.sort_all(self.sort);
        }
        self.cache_err = cache_err;
        self.cache_dirty = self.use_cache && self.cache_err.is_some();
        self.stale = false;
        self.stale_source = None;
        let old_time = self.meta.scanned_at;
        self.meta.scanned_at = meta.scanned_at;
        self.meta.scan_millis = meta.scan_millis;
        if self.tree.is_none() {
            self.set_tree(tree);
            return;
        }
        let vs = self.capture_view();
        let old = self.tree.replace(tree).unwrap();
        if self
            .diff
            .as_ref()
            .is_some_and(|d| d.covers(&self.meta.root))
        {
            // Freeing millions of nodes takes a moment; don't block the UI.
            std::thread::spawn(move || drop(old));
        } else {
            self.diff = Some(Diff::from_tree(old, old_time));
        }
        self.restore_view(vs);
    }

    /// Swaps in a rescanned subtree, keeping the old one as the baseline
    /// unless an existing baseline already covers it.
    fn apply_subtree(&mut self, target: NodeId, entry: Entry) {
        let vs = self.capture_view();
        let sort = self.sort;
        let time = self.meta.scanned_at;
        let tree = self.tree.as_mut().unwrap();
        let target_path = tree.path(target);
        if self.diff.as_ref().is_some_and(|d| d.covers(&target_path)) {
            tree.replace(target, entry, sort);
        } else {
            let mut old = tree.to_entry(target);
            old.name = target_path.as_os_str().as_bytes().into();
            tree.replace(target, entry, sort);
            self.diff = Some(Diff::new(old, time));
        }
        if tree.garbage > tree.nodes.len() / 2 {
            // Too much dead weight from refreshes; rebuild the arena.
            let e = tree.to_entry(tree.root);
            *tree = Tree::from_entry(e, sort);
        }
        self.cache_dirty = self.use_cache;
        self.restore_view(vs);
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
            View::Types | View::History => None,
            View::Changes => self.changes.get(self.changes_pos.sel).and_then(|c| c.cur),
        }
    }

    /// Deepest node along `path` in the current tree.
    fn find_path(&self, path: &std::path::Path) -> Option<NodeId> {
        let tree = self.tree.as_ref()?;
        let rel = path.strip_prefix(tree.path(tree.root)).ok()?;
        let chain: Vec<Box<[u8]>> = rel.iter().map(|c| c.as_bytes().into()).collect();
        Some(tree.resolve_chain(tree.root, &chain))
    }

    fn reveal_change(&mut self) {
        let Some(item) = self.changes.get(self.changes_pos.sel) else {
            return;
        };
        let id = match (item.cur, item.base, &self.diff) {
            (Some(c), _, _) => Some(c),
            (None, Some(b), Some(d)) => self.find_path(&d.base.path(b)),
            _ => None,
        };
        let removed = item.cur.is_none();
        if let Some(id) = id {
            self.reveal(id);
            if removed {
                self.flash("removed entry; showing where it was");
            }
        }
    }

    pub fn ensure_changes(&mut self) {
        self.ensure_diff();
        let key = (self.structure_gen, self.view_root, self.use_disk);
        if self.changes_key == Some(key) {
            return;
        }
        self.changes_key = Some(key);
        self.changes_pos = ListPos::default();
        let (Some(d), Some(t)) = (&self.diff, &self.tree) else {
            self.changes.clear();
            return;
        };
        (self.changes, self.changes_sum) = d.changes(t, self.view_root, self.use_disk, TOP_FILES);
    }

    // ---- history view ------------------------------------------------------

    /// Loads, in the background, the viewed directory's size from each
    /// cached snapshot of this root.
    pub fn ensure_history(&mut self) {
        let key = (self.structure_gen, self.view_root);
        if self.history_key != Some(key) {
            self.history_key = Some(key);
            self.history_pos = ListPos::default();
            let tree = self.tree.as_ref().unwrap();
            let path = tree.path(self.view_root);
            self.history = vec![HistPoint::from_tree(
                tree,
                self.view_root,
                self.meta.scanned_at,
                None,
            )];
            let files: Vec<(u64, PathBuf)> = crate::cache::Key::of(&self.meta)
                .history()
                .into_iter()
                .filter(|(t, _)| *t != self.meta.scanned_at)
                .collect();
            let (tx, rx) = std::sync::mpsc::channel();
            self.history_rx = Some(rx);
            std::thread::spawn(move || {
                for (time, file) in files {
                    let point =
                        match snapshot::load_tree(&file, SortKey::Name, Some(&path), Some(1)) {
                            Ok((t, _)) => HistPoint::from_tree(&t, t.root, time, Some(file)),
                            Err(_) => HistPoint {
                                time,
                                file: Some(file),
                                totals: None,
                                children: Vec::new(),
                            },
                        };
                    // The view moved on; stop.
                    if tx.send(point).is_err() {
                        return;
                    }
                }
            });
        }
        if let Some(rx) = &self.history_rx {
            loop {
                match rx.try_recv() {
                    Ok(p) => self.history.push(p),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.history_rx = None;
                        break;
                    }
                }
            }
        }
    }

    pub fn history_loading(&self) -> bool {
        self.history_rx.is_some()
    }

    /// Uses the selected historical snapshot as the comparison baseline.
    fn compare_with_history(&mut self) {
        let Some(file) = self
            .history
            .get(self.history_pos.sel)
            .and_then(|p| p.file.clone())
        else {
            return self.flash("pick an older snapshot to compare with");
        };
        match snapshot::load(&file, SortKey::Name) {
            Ok((tree, meta)) => {
                self.set_baseline(Diff::from_tree(tree, meta.scanned_at));
                self.view = View::Changes;
                self.flash(format!(
                    "comparing with snapshot from {}",
                    crate::fmt::ago(meta.scanned_at)
                ));
            }
            Err(e) => self.flash(format!("loading snapshot: {e:#}")),
        }
    }

    // ---- deletion ----------------------------------------------------------

    fn ask_delete(&mut self) {
        if !self.allow_delete {
            return self.flash("deleting is disabled (--no-delete)");
        }
        if self.stale {
            return self.flash("showing a cached snapshot; wait for the refresh before deleting");
        }
        let Some(id) = self.selected() else { return };
        let tree = self.tree.as_ref().unwrap();
        if id == tree.root || id == self.view_root {
            return self.flash("can't delete the directory being viewed; go up first");
        }
        if self.job.is_some() {
            return self.flash("wait for the running job to finish");
        }
        self.mode = Mode::ConfirmDelete(id);
    }

    fn start_delete(&mut self, id: NodeId) {
        let tree = self.tree.as_ref().unwrap();
        let path = tree.path(id);
        let progress = Arc::new(Progress::with_estimate(Some(Estimate {
            items: tree.node(id).items,
            disk: 0,
            source: "scan",
        })));
        let (tx, rx) = std::sync::mpsc::channel();
        let (p, del_path) = (progress.clone(), path.clone());
        std::thread::Builder::new()
            .name("dut-delete".into())
            .spawn(move || {
                let _ = tx.send(JobResult::Delete(crate::delete::delete(&del_path, &p)));
            })
            .expect("spawning delete thread");
        self.job = Some(Job {
            kind: JobKind::Delete,
            target: Some(id),
            path,
            progress,
            started: Instant::now(),
            rx,
        });
    }

    fn finish_delete(&mut self, job: Job, errors: Vec<String>) {
        let id = job.target.unwrap();
        let freed = job.progress.disk.load(std::sync::atomic::Ordering::Relaxed);
        if errors.is_empty() && !job.path.exists() {
            let sort = self.sort;
            let tree = self.tree.as_mut().unwrap();
            let parent = tree.node(id).parent;
            tree.remove(id, sort);
            self.expanded.remove(&id);
            self.cache_dirty = self.use_cache;
            self.generation += 1;
            self.structure_gen += 1;
            self.rebuild_rows();
            if self.view == View::Tree && self.rows.get(self.tree_pos.sel).is_none() {
                self.select_id(parent);
            }
            self.flash(format!(
                "deleted {} ({} freed)",
                job.path.display(),
                crate::fmt::size(freed)
            ));
        } else {
            // Partially deleted: rescan the parent so the tree matches disk.
            let parent = self.tree.as_ref().unwrap().node(id).parent;
            if errors.is_empty() {
                self.flash("delete cancelled; rescanning");
            } else {
                self.mode = Mode::Errors(errors);
            }
            self.start_scan(Some(parent));
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
            Which::Changes => (&mut self.changes_pos, self.changes.len()),
            Which::History => (&mut self.history_pos, self.history.len()),
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
            Mode::ConfirmDelete(id) => {
                let id = *id;
                self.mode = Mode::Normal;
                if key.code == KeyCode::Char('y') {
                    self.start_delete(id);
                } else {
                    self.flash("delete cancelled");
                }
                return;
            }
            Mode::Errors(_) => {
                self.mode = Mode::Normal;
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
            KeyCode::Char('5') => self.view = View::Changes,
            KeyCode::Char('6') => self.view = View::History,
            KeyCode::Char('d') | KeyCode::Delete if !ctrl => self.ask_delete(),
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
                View::Changes => match key.code {
                    KeyCode::Enter => self.reveal_change(),
                    _ => self.list_nav(key, page, Which::Changes),
                },
                View::History => match key.code {
                    KeyCode::Enter => self.compare_with_history(),
                    _ => self.list_nav(key, page, Which::History),
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
            let timeout = if self.job.is_some() || self.history_loading() {
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
        if let Some(j) = &self.job {
            j.progress
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Persist subtree refreshes / deletes so the next run starts from them.
        if self.cache_dirty && self.tree.is_some() {
            self.flash("saving cache…");
            terminal.draw(|f| crate::ui::draw(f, &mut self))?;
            let tree = self.tree.as_ref().unwrap();
            crate::cache::save(tree, &self.meta).context("saving cache")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Which {
    Tree,
    Files,
    Types,
    Changes,
    History,
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
