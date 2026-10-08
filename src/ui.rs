use crate::app::{App, JobKind, ListPos, Mode, View, has_problem};
use crate::diff::Change;
use crate::fmt;
use crate::tree::{Kind, NodeId, SortKey, Tree, flags};
use crate::treemap::{self, Rect as FRect};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

const SEL_BG: Color = Color::Indexed(236);
const DIM: Color = Color::DarkGray;
const DIR: Color = Color::LightBlue;
const ACCENT: Color = Color::Cyan;
const PALETTE: [Color; 8] = [
    Color::Blue,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Cyan,
    Color::Red,
    Color::LightGreen,
    Color::LightMagenta,
];

pub fn draw(f: &mut Frame, app: &mut App) {
    if app.tree.is_none() {
        draw_initial_scan(f, app);
        return;
    }
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, header, app);
    draw_tabs(f, tabs, app);
    match app.view {
        View::Tree => draw_tree(f, body, app),
        View::Treemap => draw_treemap(f, body, app),
        View::Files => draw_files(f, body, app),
        View::Types => draw_types(f, body, app),
        View::Changes => draw_changes(f, body, app),
    }
    draw_footer(f, footer, app);

    match &app.mode {
        Mode::Normal => {}
        Mode::Help => draw_help(f),
        Mode::SavePrompt(buf) => draw_prompt(f, buf),
        Mode::ConfirmDelete(id) => draw_confirm_delete(f, app, *id),
        Mode::Errors(errors) => draw_errors(f, errors),
    }
}

fn metric(app: &App, tree: &Tree, id: NodeId) -> u64 {
    let n = tree.node(id);
    if app.use_disk { n.disk } else { n.size }
}

fn draw_initial_scan(f: &mut Frame, app: &App) {
    let Some(job) = &app.job else { return };
    let p = &job.progress;
    let el = job.started.elapsed();
    let items = p.items.load(Relaxed);
    let rate = items as f64 / el.as_secs_f64().max(0.001);
    let spinner =
        ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(el.as_millis() / 80) as usize % 10];
    let label = |s: &'static str| Span::styled(format!("{s:>9}  "), Style::new().fg(DIM));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                if p.saving.load(Relaxed) {
                    format!("{spinner} saving snapshot of ")
                } else {
                    format!("{spinner} scanning ")
                },
                Style::new().fg(ACCENT),
            ),
            Span::raw(job.path.display().to_string()).bold(),
        ]),
        Line::raw(""),
        Line::from(vec![label("items"), Span::raw(fmt::count(items))]),
        Line::from(vec![
            label("size"),
            Span::raw(fmt::size(p.disk.load(Relaxed))),
        ]),
        Line::from(vec![
            label("elapsed"),
            Span::raw(fmt::duration_ms(el.as_millis() as u64)),
        ]),
        Line::from(vec![
            label("rate"),
            Span::raw(format!("{}/s", fmt::count(rate as u64))),
        ]),
        Line::from(vec![
            label("errors"),
            Span::raw(fmt::count(p.errors.load(Relaxed))),
        ]),
    ];
    match p.eta(el) {
        Some((fr, eta)) => {
            let est = p.estimate.unwrap();
            lines.push(Line::from(vec![
                label("eta"),
                Span::raw(eta.map_or("estimating…".into(), |d| {
                    format!("~{}", fmt::duration_ms(d.as_millis() as u64))
                })),
                Span::styled(format!("  (from {})", est.source), Style::new().fg(DIM)),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(bar(fr, 50), Style::new().fg(ACCENT).bg(Color::Indexed(235))),
                Span::raw(format!(" {:>3.0}%", fr * 100.0)),
            ]));
        }
        None => {
            lines.push(Line::from(vec![
                label("eta"),
                Span::styled(
                    "unknown (no previous scan of this path)",
                    Style::new().fg(DIM),
                ),
            ]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("esc to cancel", Style::new().fg(DIM)));
    let area = centered(f.area(), 60, lines.len() as u16 + 2);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(" dut ".bold().fg(ACCENT));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let tree = app.tree.as_ref().unwrap();
    let vr = tree.node(app.view_root);
    let path = tree.path(app.view_root).display().to_string();
    let mut left = vec![
        Span::styled(" dut ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw(" "),
        Span::styled(path, Style::new().bold()),
    ];
    if app.stale {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            format!(" cached {} · refreshing… ", fmt::ago(app.meta.scanned_at)),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        ));
    }
    if has_problem(vr.flags) {
        left.push(Span::styled(
            "  ! some entries unreadable",
            Style::new().fg(Color::Red),
        ));
    }
    let right = format!(
        "{}  {} items  scanned {} in {} ",
        fmt::size(metric(app, tree, app.view_root)),
        fmt::count(vr.items),
        fmt::ago(app.meta.scanned_at),
        fmt::duration_ms(app.meta.scan_millis),
    );
    render_lr(
        f.buffer_mut(),
        area,
        Line::from(left),
        Line::styled(right, Style::new().fg(DIM)),
    );
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::raw(" ")];
    for (i, v) in View::ALL.iter().enumerate() {
        let style = if *v == app.view {
            Style::new().fg(ACCENT).bold().underlined()
        } else {
            Style::new().fg(DIM)
        };
        spans.push(Span::styled(format!("{} {}", i + 1, v.title()), style));
        spans.push(Span::raw("   "));
    }
    if let (View::Files, Some(ext)) = (app.view, &app.files_filter) {
        spans.push(Span::styled(
            format!("filter: .{ext} "),
            Style::new().fg(Color::Yellow),
        ));
    }
    let sort = match app.sort {
        SortKey::Disk | SortKey::Size => "size",
        SortKey::Items => "items",
        SortKey::Name => "name",
    };
    let mut right = format!(
        "{} · sort {}",
        if app.use_disk {
            "disk usage"
        } else {
            "apparent size"
        },
        sort
    );
    if app.meta.one_file_system {
        right += " · one fs";
    }
    if !app.meta.exclude.is_empty() {
        right += &format!(" · {} excludes", app.meta.exclude.len());
    }
    if let Some(d) = &app.diff {
        right += &format!(" · vs {}", fmt::ago(d.base_time));
    }
    right.push(' ');
    render_lr(
        f.buffer_mut(),
        area,
        Line::from(spans),
        Line::styled(right, Style::new().fg(DIM)),
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(job) = &app.job {
        let p = &job.progress;
        Line::from(vec![
            Span::styled(
                if job.kind == JobKind::Delete {
                    " ✗ deleting "
                } else if p.saving.load(Relaxed) {
                    " ⟳ saving snapshot "
                } else {
                    " ⟳ refreshing "
                },
                Style::new().fg(Color::Black).bg(Color::Yellow),
            ),
            Span::raw(format!(
                " {}  {} items  {}  {}{}   esc to cancel",
                job.path.display(),
                fmt::count(p.items.load(Relaxed)),
                fmt::size(p.disk.load(Relaxed)),
                fmt::duration_ms(job.started.elapsed().as_millis() as u64),
                match p.eta(job.started.elapsed()) {
                    Some((fr, eta)) => format!(
                        "  {:.0}%{}",
                        fr * 100.0,
                        eta.map_or(String::new(), |d| format!(
                            " ~{} left",
                            fmt::duration_ms(d.as_millis() as u64)
                        ))
                    ),
                    None => String::new(),
                },
            )),
        ])
    } else if let Some((msg, _)) = app
        .status
        .as_ref()
        .filter(|(_, t)| t.elapsed() < Duration::from_secs(5))
    {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(msg.clone(), Style::new().fg(Color::Yellow)),
        ])
    } else {
        let hints: &[(&str, &str)] = match app.view {
            View::Tree => &[
                ("←→", "fold"),
                ("⏎", "zoom"),
                ("⌫", "up"),
                ("r/R", "refresh"),
                ("w", "save"),
                ("a", "size"),
                ("s", "sort"),
                ("?", "help"),
            ],
            View::Treemap => &[
                ("arrows", "move"),
                ("⏎", "zoom"),
                ("⌫", "up"),
                ("r/R", "refresh"),
                ("w", "save"),
                ("?", "help"),
            ],
            View::Files => &[
                ("⏎", "show in tree"),
                ("⌫", "up"),
                ("esc", "clear filter"),
                ("?", "help"),
            ],
            View::Types => &[("⏎", "list files of type"), ("⌫", "up"), ("?", "help")],
            View::Changes => &[
                ("⏎", "show in tree"),
                ("⌫", "up"),
                ("R", "rescan & compare"),
                ("?", "help"),
            ],
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, d) in hints {
            spans.push(Span::styled(*k, Style::new().fg(ACCENT)));
            spans.push(Span::styled(format!(" {d}  "), Style::new().fg(DIM)));
        }
        Line::from(spans)
    };
    f.render_widget(line, area);
}

fn render_lr(buf: &mut Buffer, area: Rect, left: Line, right: Line) {
    let rw = (right.width() as u16).min(area.width);
    let lw = left.width() as u16;
    left.render(area, buf);
    if lw + rw < area.width {
        let r = Rect {
            x: area.right() - rw,
            width: rw,
            ..area
        };
        right.render(r, buf);
    }
}

// ---- bars & rows -----------------------------------------------------------

fn bar(frac: f64, width: usize) -> String {
    const EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let frac = frac.clamp(0.0, 1.0);
    let total = (frac * width as f64 * 8.0).round() as usize;
    let mut s = String::with_capacity(width * 3);
    for i in 0..width {
        let fill = total.saturating_sub(i * 8).min(8);
        s.push(EIGHTHS[fill]);
    }
    s
}

fn heat(frac: f64) -> Color {
    if frac >= 0.5 {
        Color::Red
    } else if frac >= 0.25 {
        Color::LightRed
    } else if frac >= 0.1 {
        Color::Yellow
    } else if frac >= 0.02 {
        Color::Green
    } else {
        Color::DarkGray
    }
}

fn frac(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

fn name_spans(tree: &Tree, id: NodeId) -> Vec<Span<'static>> {
    let n = tree.node(id);
    let name = n.name_lossy().into_owned();
    let mut v = match n.kind {
        Kind::Dir => vec![Span::styled(name + "/", Style::new().fg(DIR).bold())],
        Kind::Symlink => vec![Span::styled(name, Style::new().fg(Color::Cyan).italic())],
        Kind::Other => vec![Span::styled(name, Style::new().fg(Color::Magenta))],
        Kind::File => vec![Span::raw(name)],
    };
    if n.flags & flags::ERROR != 0 {
        v.push(Span::styled("  ! unreadable", Style::new().fg(Color::Red)));
    } else if n.flags & flags::SUB_ERROR != 0 {
        v.push(Span::styled("  !", Style::new().fg(Color::Red)));
    }
    if n.flags & flags::OTHER_FS != 0 {
        v.push(Span::styled("  (other filesystem)", Style::new().fg(DIM)));
    }
    if n.kind == Kind::File && n.size > (1 << 20) && n.disk < n.size / 2 {
        v.push(Span::styled(
            format!("  (sparse, {} apparent)", fmt::size(n.size)),
            Style::new().fg(DIM),
        ));
    }
    if n.flags & flags::HARDLINK != 0 {
        v.push(Span::styled(
            "  (hardlink, counted elsewhere)",
            Style::new().fg(DIM),
        ));
    }
    v
}

fn list_block(title: String) -> Block<'static> {
    Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(DIM))
        .title(Span::styled(title, Style::new().fg(DIM)))
}

fn draw_tree(f: &mut Frame, area: Rect, app: &mut App) {
    app.ensure_rows();
    app.ensure_diff();
    let block = list_block(String::new());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let tree = app.tree.as_ref().unwrap();
    if app.rows.is_empty() {
        f.render_widget(Line::styled("  (empty)", Style::new().fg(DIM)), inner);
        return;
    }
    let height = inner.height as usize;
    app.tree_pos.scroll(height);
    let wide = inner.width >= 80;
    let bar_w = if inner.width >= 100 { 20 } else { 10 };
    let mut lines = Vec::with_capacity(height);
    for (i, row) in app
        .rows
        .iter()
        .enumerate()
        .skip(app.tree_pos.offset)
        .take(height)
    {
        let n = tree.node(row.id);
        let s = metric(app, tree, row.id);
        let parent = metric(app, tree, n.parent);
        let fr = frac(s, parent);
        let marker = if n.children.is_empty() {
            "  "
        } else if app.expanded.contains(&row.id) {
            "▾ "
        } else {
            "▸ "
        };
        let mut spans = vec![
            Span::raw(format!(" {:>9} ", fmt::size(s))),
            Span::styled(format!("{:>5.1}% ", fr * 100.0), Style::new().fg(DIM)),
            Span::styled(
                bar(fr, bar_w),
                Style::new().fg(heat(fr)).bg(Color::Indexed(235)),
            ),
        ];
        if let Some(d) = &app.diff {
            spans.push(delta_span(d.change(tree, row.id, app.use_disk)));
        }
        if wide {
            let items = if n.is_dir() {
                fmt::count(n.items - 1)
            } else {
                String::new()
            };
            spans.push(Span::styled(format!(" {items:>10} "), Style::new().fg(DIM)));
        } else {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(row.prefix.clone(), Style::new().fg(DIM)));
        spans.push(Span::styled(marker, Style::new().fg(DIM)));
        spans.extend(name_spans(tree, row.id));
        let mut line = Line::from(spans);
        if i == app.tree_pos.sel {
            line = line.style(Style::new().bg(SEL_BG));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
    draw_scrollbar(f.buffer_mut(), inner, app.tree_pos, app.rows.len());
}

fn draw_scrollbar(buf: &mut Buffer, area: Rect, pos: ListPos, len: usize) {
    let h = area.height as usize;
    if len <= h || h == 0 {
        return;
    }
    let thumb = (h * h / len).max(1);
    let top = (pos.offset * (h - thumb)) / (len - h).max(1);
    let x = area.right() - 1;
    for i in 0..h {
        let (ch, fg) = if i >= top && i < top + thumb {
            ("┃", Color::Gray)
        } else {
            ("│", Color::Indexed(238))
        };
        buf[(x, area.y + i as u16)].set_symbol(ch).set_fg(fg);
    }
}

// ---- treemap ---------------------------------------------------------------

/// Children of `id` with non-zero size, largest first, capped for sanity.
fn tm_items(app: &App, tree: &Tree, id: NodeId, cap: usize) -> Vec<(NodeId, f64)> {
    let mut v: Vec<(NodeId, f64)> = tree
        .children(id)
        .iter()
        .map(|&c| (c, metric(app, tree, c) as f64))
        .filter(|&(_, w)| w > 0.0)
        .collect();
    v.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    v.truncate(cap);
    v
}

/// Converts a layout rect (in half-cell vertical units) to terminal cells.
fn to_cells(r: FRect, origin: Rect) -> Rect {
    let x0 = r.x.round() as u16;
    let x1 = (r.x + r.w).round() as u16;
    let y0 = (r.y / 2.0).round() as u16;
    let y1 = ((r.y + r.h) / 2.0).round() as u16;
    Rect {
        x: origin.x + x0,
        y: origin.y + y0,
        width: x1.saturating_sub(x0),
        height: y1.saturating_sub(y0),
    }
}

fn layout_in(items: &[(NodeId, f64)], area: Rect) -> Vec<(NodeId, Rect)> {
    let weights: Vec<f64> = items.iter().map(|i| i.1).collect();
    // Terminal cells are about twice as tall as wide; lay out in square units.
    let fr = FRect {
        x: 0.0,
        y: 0.0,
        w: area.width as f64,
        h: area.height as f64 * 2.0,
    };
    treemap::squarify(&weights, fr)
        .into_iter()
        .zip(items)
        .map(|(r, &(id, _))| (id, to_cells(r, area)))
        .collect()
}

fn draw_treemap(f: &mut Frame, area: Rect, app: &mut App) {
    let tree = app.tree.as_ref().unwrap();
    let items = tm_items(app, tree, app.view_root, 400);
    if items.is_empty() {
        f.render_widget(
            Line::styled("  (nothing to show)", Style::new().fg(DIM)),
            area,
        );
        app.tm_rects.clear();
        return;
    }
    let cells = layout_in(&items, area);
    app.tm_rects = cells
        .iter()
        .map(|&(id, r)| {
            (
                id,
                FRect {
                    x: r.x as f64,
                    y: r.y as f64 * 2.0,
                    w: r.width as f64,
                    h: r.height as f64 * 2.0,
                },
            )
        })
        .collect();
    if let Some(focus) = app.tm_focus.take() {
        app.tm_sel = cells.iter().position(|c| c.0 == focus).unwrap_or(0);
    }
    app.tm_sel = app.tm_sel.min(cells.len() - 1);

    let total = metric(app, tree, app.view_root);
    let buf = f.buffer_mut();
    for (i, &(id, r)) in cells.iter().enumerate() {
        let color = PALETTE[i % PALETTE.len()];
        draw_cell(buf, app, tree, id, r, color, 0, i == app.tm_sel, total);
    }
    // Draw the selected cell last so its heavy border wins at shared edges.
    if let Some(&(id, r)) = cells.get(app.tm_sel) {
        let color = PALETTE[app.tm_sel % PALETTE.len()];
        draw_cell(buf, app, tree, id, r, color, 0, true, total);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_cell(
    buf: &mut Buffer,
    app: &App,
    tree: &Tree,
    id: NodeId,
    r: Rect,
    color: Color,
    depth: usize,
    selected: bool,
    total: u64,
) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    let n = tree.node(id);
    let s = metric(app, tree, id);
    if r.width < 3 || r.height < 2 {
        let style = Style::new().fg(color);
        for y in r.top()..r.bottom() {
            for x in r.left()..r.right() {
                buf[(x, y)].set_symbol("▒").set_style(style);
            }
        }
        return;
    }
    let mut style = Style::new().fg(color);
    if depth > 0 {
        style = style.add_modifier(Modifier::DIM);
    }
    let (bt, title_style) = if selected {
        (
            BorderType::Thick,
            Style::new().fg(Color::Black).bg(color).bold(),
        )
    } else {
        (BorderType::Rounded, Style::new().fg(color).bold())
    };
    let mut title = n.name_lossy().into_owned();
    if n.is_dir() {
        title.push('/');
    }
    let label = if depth == 0 {
        format!(" {title} {} {:.0}% ", fmt::size(s), frac(s, total) * 100.0)
    } else {
        format!(" {title} {} ", fmt::size(s))
    };
    let block = Block::bordered()
        .border_type(bt)
        .border_style(style)
        .title(Span::styled(label, title_style));
    let inner = block.inner(r);
    block.render(r, buf);
    if n.is_dir() && depth < 1 && inner.width >= 6 && inner.height >= 3 {
        let items = tm_items(app, tree, id, 200);
        for (cid, cr) in layout_in(&items, inner) {
            draw_cell(buf, app, tree, cid, cr, color, depth + 1, false, total);
        }
    } else if !n.is_dir() && inner.height >= 1 && inner.width >= 1 {
        // Leaf: a light fill makes files read as solid blocks.
        let fill = Style::new().fg(color).add_modifier(Modifier::DIM);
        for y in inner.top()..inner.bottom() {
            for x in inner.left()..inner.right() {
                buf[(x, y)].set_symbol("░").set_style(fill);
            }
        }
    }
}

// ---- files & types -----------------------------------------------------------

fn draw_files(f: &mut Frame, area: Rect, app: &mut App) {
    app.ensure_files();
    let tree = app.tree.as_ref().unwrap();
    let total = metric(app, tree, app.view_root);
    let title = format!(" {} largest files ", app.files.len());
    let block = list_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let height = inner.height as usize;
    app.files_pos.scroll(height);
    let mut lines = Vec::with_capacity(height);
    for (i, &id) in app
        .files
        .iter()
        .enumerate()
        .skip(app.files_pos.offset)
        .take(height)
    {
        let s = metric(app, tree, id);
        let fr = frac(s, total);
        let rel = tree.rel_path(app.view_root, id);
        let (dir, name) = match rel.rfind('/') {
            Some(p) => (rel[..=p].to_string(), rel[p + 1..].to_string()),
            None => (String::new(), rel),
        };
        let mut line = Line::from(vec![
            Span::raw(format!(" {:>9} ", fmt::size(s))),
            Span::styled(format!("{:>5.1}% ", fr * 100.0), Style::new().fg(DIM)),
            Span::styled(
                bar(fr, 10),
                Style::new().fg(heat(fr)).bg(Color::Indexed(235)),
            ),
            Span::raw("  "),
            Span::styled(dir, Style::new().fg(DIM)),
            Span::raw(name),
        ]);
        if i == app.files_pos.sel {
            line = line.style(Style::new().bg(SEL_BG));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
    draw_scrollbar(f.buffer_mut(), inner, app.files_pos, app.files.len());
}

fn draw_types(f: &mut Frame, area: Rect, app: &mut App) {
    app.ensure_types();
    let tree = app.tree.as_ref().unwrap();
    let total = metric(app, tree, app.view_root);
    let block = list_block(format!(" {} file types ", app.types.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let height = inner.height as usize;
    app.types_pos.scroll(height);
    let mut lines = Vec::with_capacity(height);
    for (i, t) in app
        .types
        .iter()
        .enumerate()
        .skip(app.types_pos.offset)
        .take(height)
    {
        let fr = frac(t.bytes, total);
        let mut line = Line::from(vec![
            Span::raw(format!(" {:>9} ", fmt::size(t.bytes))),
            Span::styled(format!("{:>5.1}% ", fr * 100.0), Style::new().fg(DIM)),
            Span::styled(
                bar(fr, 20),
                Style::new().fg(heat(fr)).bg(Color::Indexed(235)),
            ),
            Span::styled(
                format!(" {:>10} files  ", fmt::count(t.count)),
                Style::new().fg(DIM),
            ),
            Span::styled(t.ext.clone(), Style::new().bold()),
        ]);
        if i == app.types_pos.sel {
            line = line.style(Style::new().bg(SEL_BG));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
    draw_scrollbar(f.buffer_mut(), inner, app.types_pos, app.types.len());
}

// ---- overlays ----------------------------------------------------------------

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_help(f: &mut Frame) {
    let keys: &[(&str, &str)] = &[
        ("1 2 3 4 / v", "tree · treemap · largest files · file types"),
        ("j k ↑ ↓", "move"),
        ("PgUp PgDn ^u ^d", "page / half page"),
        ("g G", "top / bottom"),
        ("l → / h ←", "expand / collapse (or go to parent)"),
        ("space", "toggle expand"),
        ("*  -", "expand subtree / collapse all"),
        ("enter", "zoom into directory"),
        ("backspace u", "zoom out"),
        ("a", "toggle disk usage / apparent size"),
        ("s", "cycle sort: size · items · name"),
        ("r", "rescan selected directory"),
        ("R", "rescan everything"),
        ("w ^s", "save snapshot"),
        ("esc", "cancel scan / back / quit"),
        ("d del", "delete selected (asks first)"),
        ("5", "changes since baseline snapshot / previous scan"),
        ("q", "quit"),
    ];
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("  {k:>16}  "), Style::new().fg(ACCENT)),
                Span::raw(*d),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ! marks unreadable entries (or ones below)",
        Style::new().fg(DIM),
    ));
    let area = centered(f.area(), 72, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" keys ".bold());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_prompt(f: &mut Frame, buf: &str) {
    let area = centered(f.area(), 70, 3);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" save snapshot to ".bold())
        .title_bottom(
            Line::styled(" enter save · esc cancel ", Style::new().fg(DIM)).right_aligned(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Show the tail of long paths so the cursor end stays visible.
    let max = inner.width.saturating_sub(2) as usize;
    let chars: Vec<char> = buf.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(max)..].iter().collect();
    f.render_widget(
        Line::from(vec![Span::raw(" "), Span::raw(shown.clone())]),
        inner,
    );
    f.set_cursor_position((inner.x + 1 + shown.chars().count() as u16, inner.y));
}

fn delta_span(c: Option<Change>) -> Span<'static> {
    match c {
        Some(Change::New) => Span::styled(format!(" {:>9}", "new"), Style::new().fg(Color::Yellow)),
        Some(Change::Delta(d)) if d > 0 => Span::styled(
            format!(" {:>9}", fmt::delta(d)),
            Style::new().fg(Color::LightRed),
        ),
        Some(Change::Delta(d)) if d < 0 => Span::styled(
            format!(" {:>9}", fmt::delta(d)),
            Style::new().fg(Color::LightGreen),
        ),
        _ => Span::raw(" ".repeat(10)),
    }
}

fn draw_changes(f: &mut Frame, area: Rect, app: &mut App) {
    app.ensure_changes();
    let Some(diff) = &app.diff else {
        let lines = vec![
            Line::raw(""),
            Line::raw("  Nothing to compare against yet."),
            Line::raw(""),
            Line::styled(
                "  Press R (or r on a directory) to rescan and compare with the current state,",
                Style::new().fg(DIM),
            ),
            Line::styled(
                "  or start with --diff old.dut, or --load old.dut --refresh.",
                Style::new().fg(DIM),
            ),
        ];
        f.render_widget(Paragraph::new(lines).block(list_block(String::new())), area);
        return;
    };
    let tree = app.tree.as_ref().unwrap();
    let sum = &app.changes_sum;
    let title = format!(
        " +{} grown · -{} shrunk · {} new · {} removed · vs {} ",
        fmt::size(sum.grown),
        fmt::size(sum.shrunk),
        fmt::count(sum.new),
        fmt::count(sum.removed),
        fmt::ago(diff.base_time),
    );
    let block = list_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.changes.is_empty() {
        f.render_widget(Line::styled("  (no changes)", Style::new().fg(DIM)), inner);
        return;
    }
    let height = inner.height as usize;
    app.changes_pos.scroll(height);
    let root_path = tree.path(app.view_root);
    let mut lines = Vec::with_capacity(height);
    for (i, c) in app
        .changes
        .iter()
        .enumerate()
        .skip(app.changes_pos.offset)
        .take(height)
    {
        let (status, color, path, is_dir) = match (c.cur, c.base) {
            (Some(id), None) => ("new", Color::Yellow, tree.path(id), tree.node(id).is_dir()),
            (None, Some(b)) => (
                "removed",
                Color::Magenta,
                diff.base.path(b),
                diff.base.node(b).is_dir(),
            ),
            (Some(id), Some(_)) => (
                if c.delta > 0 { "grew" } else { "shrank" },
                if c.delta > 0 {
                    Color::LightRed
                } else {
                    Color::LightGreen
                },
                tree.path(id),
                false,
            ),
            (None, None) => continue,
        };
        let now = c
            .cur
            .map_or(String::new(), |id| fmt::size(metric(app, tree, id)));
        let mut rel = path
            .strip_prefix(&root_path)
            .unwrap_or(&path)
            .display()
            .to_string();
        if is_dir {
            rel.push('/');
        }
        let mut line = Line::from(vec![
            Span::styled(
                format!(" {:>9}", fmt::delta(c.delta)),
                Style::new().fg(color),
            ),
            Span::styled(format!(" {:>9} ", now), Style::new().fg(DIM)),
            Span::styled(format!("{status:<8}"), Style::new().fg(color)),
            Span::raw(rel),
        ]);
        if i == app.changes_pos.sel {
            line = line.style(Style::new().bg(SEL_BG));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
    draw_scrollbar(f.buffer_mut(), inner, app.changes_pos, app.changes.len());
}

fn draw_confirm_delete(f: &mut Frame, app: &App, id: NodeId) {
    let tree = app.tree.as_ref().unwrap();
    let n = tree.node(id);
    let what = if n.is_dir() {
        let k = n.items - 1;
        format!(
            "this directory and its {} {}",
            fmt::count(k),
            if k == 1 { "entry" } else { "entries" }
        )
    } else {
        "this file".to_string()
    };
    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::raw(tree.path(id).display().to_string()).bold(),
        ]),
        Line::raw(""),
        Line::raw(format!(
            "  Permanently delete {what} ({})?",
            fmt::size(n.disk)
        )),
        Line::styled(
            "  Symlinks are removed, not followed; other filesystems are left alone.",
            Style::new().fg(DIM),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Red).bold()),
            Span::raw(" delete    "),
            Span::styled("any other key", Style::new().fg(ACCENT)),
            Span::raw(" cancel"),
        ]),
    ];
    let path_rows = (tree.path(id).as_os_str().len() as u16 + 2) / 76;
    let area = centered(f.area(), 80, lines.len() as u16 + 2 + path_rows);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Red))
        .title(" delete ".bold());
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_errors(f: &mut Frame, errors: &[String]) {
    let mut lines: Vec<Line> = vec![
        Line::raw("  Some entries could not be deleted:"),
        Line::raw(""),
    ];
    for e in errors.iter().take(15) {
        lines.push(Line::styled(format!("  {e}"), Style::new().fg(Color::Red)));
    }
    if errors.len() > 15 {
        lines.push(Line::styled(
            format!("  … and {} more", errors.len() - 15),
            Style::new().fg(DIM),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  The parent directory is being rescanned. Press any key.",
        Style::new().fg(DIM),
    ));
    let area = centered(f.area(), 100, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Red))
        .title(" delete incomplete ".bold());
    f.render_widget(Paragraph::new(lines).block(block), area);
}
