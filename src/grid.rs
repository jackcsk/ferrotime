//! The data grid: header row, auto filter row, virtualized body with group rows,
//! pinned (frozen) columns, custom scrollbars, selection, and their menus/popups.

use std::collections::HashMap;
use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, Modifiers, PointerButton, Pos2, Rangef, Rect, Sense,
    Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2,
};

use crate::expr::{Expr, Op};
use crate::format::{RowStyle, Style};
use crate::value::{Col, ColKind, cmp_values};
use crate::view::{AutoOp, Item, MIN_COL_WIDTH, Tab};

/// Drag payload: a column header being dragged (to reorder or to the group-by box).
#[derive(Clone, Copy, Debug)]
pub struct HeaderDrag(pub Col);
/// Drag payload: a group-by chip being dragged back to the header row (to ungroup).
#[derive(Clone, Copy, Debug)]
pub struct GroupChipDrag(pub Col);

/// Conditional formatting dialogs reachable from the header menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleDialog {
    GreaterThan,
    LessThan,
    Between,
    EqualTo,
    TextContains,
    Custom,
    Unique,
    Duplicate,
}

/// Requests the grid cannot satisfy itself.
#[derive(Clone, Debug)]
pub enum GridAction {
    ShowCell { row: usize, col: Col },
    FilterEditor,
    ColumnChooser,
    ToggleGroupBox,
    ToggleAutoFilterRow,
    RuleDialog { col: Col, kind: RuleDialog },
    ClearRules(Option<Col>),
    ManageRules,
    Copy { with_header: bool },
    CopyCell,
    SetTags(bool),
}

pub struct GridSettings {
    pub font_size: f32,
    pub show_auto_filter: bool,
    pub show_group_box: bool,
    pub dark: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilterPage {
    Values,
    Text,
}

struct FilterPopup {
    col: Col,
    pos: Pos2,
    page: FilterPage,
    values: Vec<(String, u32)>,
    checked: Vec<bool>,
    search: String,
    op1: AutoOp,
    val1: String,
    join_and: bool,
    op2: AutoOp,
    val2: String,
}

/// UI state of the grid that is not part of the tab model.
#[derive(Default)]
pub struct GridState {
    filter_popup: Option<FilterPopup>,
    op_menu: Option<(Col, Pos2)>,
    context_cell: Option<(usize, Col)>,
    drag_select: bool,
}

impl GridState {
    pub fn close_popups(&mut self) {
        self.filter_popup = None;
        self.op_menu = None;
        self.context_cell = None;
    }
}

struct Palette {
    bg: Color32,
    header: Color32,
    line: Color32,
    text: Color32,
    dim: Color32,
    select: Color32,
    focus: Color32,
    group: Color32,
    hl_bg: Color32,
    hl_text: Color32,
    accent: Color32,
    contains: Color32,
    negative: Color32,
}

impl Palette {
    fn new(dark: bool) -> Palette {
        let rgb = Color32::from_rgb;
        if dark {
            Palette {
                bg: rgb(30, 30, 30),
                header: rgb(45, 45, 48),
                line: rgb(62, 62, 66),
                text: rgb(220, 220, 220),
                dim: rgb(150, 150, 150),
                select: rgb(38, 79, 120),
                focus: rgb(58, 110, 165),
                group: rgb(40, 40, 42),
                hl_bg: rgb(255, 200, 60),
                hl_text: Color32::BLACK,
                accent: rgb(80, 160, 255),
                contains: rgb(80, 190, 90),
                negative: rgb(230, 90, 80),
            }
        } else {
            Palette {
                bg: Color32::WHITE,
                header: rgb(246, 246, 246),
                line: rgb(222, 222, 222),
                text: rgb(32, 32, 32),
                dim: rgb(110, 110, 110),
                select: rgb(204, 232, 255),
                focus: rgb(153, 209, 255),
                group: rgb(240, 240, 240),
                hl_bg: rgb(255, 214, 79),
                hl_text: Color32::BLACK,
                accent: rgb(30, 110, 210),
                contains: rgb(40, 150, 60),
                negative: rgb(205, 50, 40),
            }
        }
    }
}

/// Takes a dropped payload only if it has type `P`. (`Response::dnd_release_payload`
/// discards a payload of any other type, which would swallow drops meant for other targets.)
pub fn take_drop<P: std::any::Any + Send + Sync>(resp: &egui::Response) -> Option<std::sync::Arc<P>> {
    if egui::DragAndDrop::has_payload_of_type::<P>(&resp.ctx) { resp.dnd_release_payload::<P>() } else { None }
}

/// Commands collected during rendering and applied afterwards.
enum Cmd {
    Sort { col: Col, desc: Option<bool>, add: bool },
    ClearSort(Option<Col>),
    Group(Col),
    Ungroup(Col),
    ToggleGroupDir(Col),
    Hide(Col),
    BestFit(Option<Col>),
    ClearFilter(Col),
    SetFilter(Col, Option<Expr>),
    Move { col: Col, before: Col },
    OpenFilterPopup(Col, Pos2),
    ExpandAll(bool),
    SetWidth(Col, f32),
}

pub struct Metrics {
    pub row_h: f32,
    pub char_w: f32,
    pub font: FontId,
}

pub fn metrics(ui: &Ui, font_size: f32) -> Metrics {
    let font = FontId::monospace(font_size);
    let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font, '0'));
    Metrics { row_h: (font_size * 1.6).round() + 2.0, char_w, font }
}

/// Number of displayed characters in a cell (first 256).
fn display_chars(s: &str) -> usize {
    s.chars().take(256).count()
}

pub fn best_fit(tab: &mut Tab, col: Option<Col>, char_w: f32) {
    let sample = tab.item_count().min(2000);
    let rows: Vec<usize> = (0..sample).filter_map(|i| tab.item_row(i)).collect();
    let mut widths: HashMap<Col, f32> = HashMap::new();
    for c in tab.columns.iter().filter(|c| col.is_none_or(|x| x == c.col)) {
        let mut chars = display_chars(&c.name) + 3;
        if c.col == Col::Line {
            chars = chars.max(tab.row_count().max(1).ilog10() as usize + 2);
        } else if c.col != Col::Tag {
            for &r in &rows {
                chars = chars.max(display_chars(&tab.text(r, c.col)));
            }
        }
        widths.insert(c.col, (chars as f32 * char_w + 14.0).clamp(MIN_COL_WIDTH, 640.0));
    }
    for c in &mut tab.columns {
        if let Some(w) = widths.get(&c.col) {
            c.width = *w;
        }
    }
}

/// `1234567` → `1,234,567`, as in Timeline Explorer's status bar and group rows.
pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Display-only single-line rendering of a cell: when an ASCII control byte is present, control
/// characters become spaces; very long values are cut. Search highlights are computed on the result.
fn display_text(s: &str) -> std::borrow::Cow<'_, str> {
    let mut end = s.len().min(1024);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let s = &s[..end];
    if s.bytes().any(|b| b < 0x20) {
        std::borrow::Cow::Owned(s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect())
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

fn cell_job(
    text: &str,
    font: &FontId,
    style: &Style,
    hl: &[Range<usize>],
    pal: &Palette,
    max_w: f32,
    right: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let color = style.text.unwrap_or(pal.text);
    let base = TextFormat {
        font_id: font.clone(),
        color,
        italics: style.italic,
        strikethrough: if style.strike { Stroke::new(1.0, color) } else { Stroke::NONE },
        ..Default::default()
    };
    let mut pos = 0;
    for r in hl {
        let (s, e) = (r.start.min(text.len()), r.end.min(text.len()));
        if s >= e || !text.is_char_boundary(s) || !text.is_char_boundary(e) || s < pos {
            continue;
        }
        if s > pos {
            job.append(&text[pos..s], 0.0, base.clone());
        }
        job.append(&text[s..e], 0.0, TextFormat { background: pal.hl_bg, color: pal.hl_text, ..base.clone() });
        pos = e;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, base);
    }
    job.wrap.max_width = max_w.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    if right {
        job.halign = egui::Align::RIGHT;
    }
    job
}

struct Layout {
    /// (index into tab.columns, x offset relative to its region start, width)
    frozen: Vec<(usize, f32, f32)>,
    scroll: Vec<(usize, f32, f32)>,
    frozen_w: f32,
    scroll_w: f32,
}

fn layout(tab: &Tab) -> Layout {
    let pin_at = tab.first_scrollable.and_then(|c| tab.columns.iter().position(|x| x.col == c)).unwrap_or(0);
    let mut l = Layout { frozen: vec![], scroll: vec![], frozen_w: 0.0, scroll_w: 0.0 };
    for (i, c) in tab.columns.iter().enumerate().filter(|(_, c)| c.visible) {
        if i < pin_at {
            l.frozen.push((i, l.frozen_w, c.width));
            l.frozen_w += c.width;
        } else {
            l.scroll.push((i, l.scroll_w, c.width));
            l.scroll_w += c.width;
        }
    }
    l
}

/// Screen-space rect of each visible column for the current scroll position.
struct Placed {
    idx: usize,
    rect_x: Rangef,
    clip: Rangef,
}

fn place(l: &Layout, frozen_x0: f32, scroll_x0: f32, right: f32, scroll_x: f32) -> Vec<Placed> {
    let mut out = Vec::with_capacity(l.frozen.len() + l.scroll.len());
    for &(idx, off, w) in &l.scroll {
        let x0 = scroll_x0 + off - scroll_x;
        if x0 + w < scroll_x0 || x0 > right {
            continue;
        }
        out.push(Placed {
            idx,
            rect_x: Rangef::new(x0, x0 + w),
            clip: Rangef::new(x0.max(scroll_x0), (x0 + w).min(right)),
        });
    }
    for &(idx, off, w) in &l.frozen {
        let x0 = frozen_x0 + off;
        out.push(Placed { idx, rect_x: Rangef::new(x0, x0 + w), clip: Rangef::new(x0, (x0 + w).min(right)) });
    }
    out
}

fn funnel(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let h = size / 2.0;
    let pts = vec![
        pos2(center.x - h, center.y - h * 0.8),
        pos2(center.x + h, center.y - h * 0.8),
        pos2(center.x + h * 0.2, center.y + h * 0.1),
        pos2(center.x + h * 0.2, center.y + h),
        pos2(center.x - h * 0.2, center.y + h * 0.7),
        pos2(center.x - h * 0.2, center.y + h * 0.1),
    ];
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn checkbox(painter: &egui::Painter, center: Pos2, state: Option<bool>, pal: &Palette) {
    let r = Rect::from_center_size(center, vec2(12.0, 12.0));
    painter.rect_stroke(r, 1.0, Stroke::new(1.0, pal.dim), StrokeKind::Inside);
    match state {
        Some(true) => {
            let pts = [
                pos2(r.left() + 2.5, r.center().y),
                pos2(r.left() + 5.0, r.bottom() - 3.0),
                pos2(r.right() - 2.5, r.top() + 3.0),
            ];
            painter.line_segment([pts[0], pts[1]], Stroke::new(1.8, pal.accent));
            painter.line_segment([pts[1], pts[2]], Stroke::new(1.8, pal.accent));
        }
        None => {
            painter.rect_filled(r.shrink(3.0), 0.0, pal.dim);
        }
        Some(false) => {}
    }
}

fn op_glyph(painter: &egui::Painter, rect: Rect, op: AutoOp, pal: &Palette) {
    let color = match op {
        AutoOp::Contains | AutoOp::Like | AutoOp::BeginsWith | AutoOp::EndsWith => pal.contains,
        AutoOp::NotContains | AutoOp::NotLike => pal.negative,
        _ => pal.dim,
    };
    let small = matches!(
        op,
        AutoOp::Contains | AutoOp::NotContains | AutoOp::Like | AutoOp::NotLike | AutoOp::BeginsWith | AutoOp::EndsWith
    );
    let font = FontId::proportional(if small { 8.0 } else { 13.0 });
    painter.text(rect.center(), Align2::CENTER_CENTER, op.glyph(), font, color);
    if op.is_negative() && small {
        painter.hline(rect.x_range().shrink(2.0), rect.center().y, Stroke::new(1.0, color));
    }
}

pub fn show(ui: &mut Ui, tab: &mut Tab, st: &mut GridState, set: &GridSettings) -> Vec<GridAction> {
    let pal = Palette::new(set.dark);
    let m = metrics(ui, set.font_size);
    let row_h = m.row_h;
    let header_h = row_h + 2.0;
    let filter_h = if set.show_auto_filter { row_h + 2.0 } else { 0.0 };
    let sb = 14.0;
    let ind_w = 16.0;
    let grid_id = ui.scope_id().with("grid");

    let outer = ui.available_rect_before_wrap();
    ui.allocate_rect(outer, Sense::hover());
    let area = Rect::from_min_max(outer.min, pos2(outer.right() - sb, outer.bottom() - sb));
    let header = Rect::from_min_size(area.min, vec2(area.width(), header_h));
    let filter_rect = Rect::from_min_size(pos2(area.left(), header.bottom()), vec2(area.width(), filter_h));
    let body = Rect::from_min_max(pos2(area.left(), filter_rect.bottom()), area.max);
    let painter = ui.painter_at(outer);
    painter.rect_filled(outer, 0.0, pal.bg);

    let l = layout(tab);
    let frozen_x0 = area.left() + ind_w;
    let scroll_x0 = frozen_x0 + l.frozen_w;
    let view_w = (area.right() - scroll_x0).max(0.0);
    let max_scroll_x = (l.scroll_w - view_w).max(0.0);
    let n_items = tab.item_count();
    let vis_rows = (body.height() / row_h).max(1.0) as f64;
    let max_scroll_row = (n_items as f64 - vis_rows.floor()).max(0.0);

    let mut cmds: Vec<Cmd> = Vec::new();
    let mut actions: Vec<GridAction> = Vec::new();

    // ----- keyboard ------------------------------------------------------------------
    let has_focus = ui.memory(|mem| mem.has_focus(grid_id));
    if has_focus {
        ui.memory_mut(|mem| {
            mem.set_focus_lock_filter(
                grid_id,
                egui::EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: false },
            )
        });
        handle_keys(ui, tab, &mut actions, &mut cmds, vis_rows as usize);
    }

    // ----- scroll input --------------------------------------------------------------
    if ui.rect_contains_pointer(outer) && !egui::Popup::is_any_open(ui.ctx()) {
        let d = ui.input(|i| i.smooth_scroll_delta);
        if d.y != 0.0 {
            tab.scroll_row -= f64::from(d.y / row_h);
        }
        if d.x != 0.0 {
            tab.scroll_x -= d.x;
        }
    }
    if std::mem::take(&mut tab.scroll_to_focus) && n_items > 0 {
        let f = tab.focus as f64;
        if f < tab.scroll_row {
            tab.scroll_row = f;
        } else if f + 1.0 > tab.scroll_row + vis_rows.floor() {
            tab.scroll_row = f + 1.0 - vis_rows.floor();
        }
        let vis: Vec<usize> = tab.columns.iter().enumerate().filter(|(_, c)| c.visible).map(|(i, _)| i).collect();
        if let Some(&ci) = vis.get(tab.focus_col)
            && let Some(&(_, off, w)) = l.scroll.iter().find(|s| s.0 == ci)
        {
            if off < tab.scroll_x {
                tab.scroll_x = off;
            } else if off + w > tab.scroll_x + view_w {
                tab.scroll_x = off + w - view_w;
            }
        }
    }
    tab.scroll_row = tab.scroll_row.clamp(0.0, max_scroll_row);
    tab.scroll_x = tab.scroll_x.clamp(0.0, max_scroll_x);
    let placed = place(&l, frozen_x0, scroll_x0, area.right(), tab.scroll_x);

    // ----- body ----------------------------------------------------------------------
    let body_resp = ui.interact(body, grid_id, Sense::click_and_drag());
    let hit = |pos: Pos2, tab: &Tab| -> Option<(usize, Option<Col>)> {
        if !body.contains(pos) {
            return None;
        }
        let i = (tab.scroll_row + f64::from((pos.y - body.top()) / row_h)) as usize;
        if i >= tab.item_count() {
            return None;
        }
        let col = placed.iter().find(|p| p.clip.contains(pos.x)).map(|p| tab.columns[p.idx].col);
        Some((i, col))
    };
    let mods = ui.input(|i| i.modifiers);
    if body_resp.clicked() || body_resp.secondary_clicked() || body_resp.drag_started_by(PointerButton::Primary) {
        body_resp.request_focus();
    }
    if let Some(pos) = body_resp.interact_pointer_pos()
        && let Some((i, col)) = hit(pos, tab)
    {
        let vis_idx = col.and_then(|c| tab.visible_columns().position(|x| x.col == c));
        if body_resp.drag_started_by(PointerButton::Primary) {
            st.drag_select = true;
            click_select(tab, i, mods);
            if let Some(v) = vis_idx {
                tab.focus_col = v;
            }
        } else if body_resp.clicked() {
            let on_expander = |level: u16| pos.x < body.left() + ind_w + 24.0 + f32::from(level) * 20.0;
            if let Some(Item::Group { level, .. }) = tab.item(i)
                && on_expander(level)
            {
                tab.toggle_group(i);
            } else if col == Some(Col::Tag) {
                let rows = if tab.item_row(i).is_some_and(|r| tab.selected[r]) {
                    tab.selected_rows()
                } else {
                    tab.item_row(i).into_iter().collect()
                };
                tab.toggle_tags(&rows);
                tab.focus = i;
            } else {
                click_select(tab, i, mods);
            }
            if let Some(v) = vis_idx {
                tab.focus_col = v;
            }
        } else if body_resp.secondary_clicked() {
            if tab.item_row(i).is_none_or(|r| !tab.selected[r]) {
                click_select(tab, i, Modifiers::NONE);
            }
            if let Some(v) = vis_idx {
                tab.focus_col = v;
            }
            st.context_cell = col.map(|c| (i, c));
        }
        if body_resp.double_clicked() {
            match (tab.item(i), col) {
                (Some(Item::Group { .. }), _) => tab.toggle_group(i),
                (Some(Item::Row(r)), Some(c)) if c != Col::Tag => {
                    actions.push(GridAction::ShowCell { row: r as usize, col: c })
                }
                _ => {}
            }
        }
    }
    if st.drag_select
        && body_resp.dragged_by(PointerButton::Primary)
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
    {
        if pos.y < body.top() {
            tab.scroll_row -= 1.0;
        } else if pos.y > body.bottom() {
            tab.scroll_row += 1.0;
        }
        let y = pos.y.clamp(body.top(), body.bottom() - 1.0);
        let i = ((tab.scroll_row + f64::from((y - body.top()) / row_h)) as usize).min(n_items.saturating_sub(1));
        if i != tab.focus {
            if !mods.command {
                tab.clear_selection();
            }
            tab.select_range(tab.anchor, i, true);
            tab.focus = i;
        }
        ui.ctx().request_repaint();
    }
    if body_resp.drag_stopped() {
        st.drag_select = false;
    }
    body_resp.context_menu(|ui| row_menu(ui, tab, st, &mut actions, &mut cmds));

    let pin_x = (!l.frozen.is_empty()).then_some(scroll_x0 - 0.5);
    paint_body(ui, tab, &painter, &pal, &m, body, &placed, ind_w, has_focus, pin_x);

    // ----- header --------------------------------------------------------------------
    paint_header(ui, tab, &painter, &pal, &m, header, &placed, ind_w, grid_id, &mut cmds, &mut actions, set);
    if set.show_auto_filter {
        auto_filter_row(ui, tab, st, &painter, &pal, &m, filter_rect, &placed, ind_w, grid_id, &mut cmds);
    }

    // Separators for the indicator column and pinned columns (body rows draw their own,
    // so group rows spanning the full width are not crossed).
    let heads = Rangef::new(area.top(), body.top());
    let sep = Stroke::new(1.5, pal.dim.gamma_multiply(0.6));
    if let Some(x) = pin_x {
        painter.vline(x, heads, sep);
    }
    painter.vline(area.left() + ind_w - 0.5, heads, Stroke::new(1.0, pal.line));

    // ----- scrollbars ----------------------------------------------------------------
    let vtrack = Rect::from_min_max(pos2(area.right(), body.top()), pos2(outer.right(), body.bottom()));
    if let Some(v) = scrollbar(ui, grid_id.with("vsb"), vtrack, true, n_items as f64, vis_rows, tab.scroll_row, &pal) {
        tab.scroll_row = v.clamp(0.0, max_scroll_row);
    }
    let htrack = Rect::from_min_max(pos2(scroll_x0, area.bottom()), pos2(area.right(), outer.bottom()));
    if let Some(v) = scrollbar(
        ui,
        grid_id.with("hsb"),
        htrack,
        false,
        f64::from(l.scroll_w),
        f64::from(view_w),
        f64::from(tab.scroll_x),
        &pal,
    ) {
        tab.scroll_x = (v as f32).clamp(0.0, max_scroll_x);
    }
    painter.rect_filled(Rect::from_min_max(area.max, outer.max), 0.0, pal.header);
    painter.rect_stroke(outer, 0.0, Stroke::new(1.0, pal.line), StrokeKind::Inside);

    filter_popup(ui, tab, st, &mut cmds);
    op_menu(ui, tab, st, &mut cmds);

    apply(tab, st, cmds, m.char_w);
    actions
}

fn click_select(tab: &mut Tab, i: usize, mods: Modifiers) {
    if mods.shift {
        if !mods.command {
            tab.clear_selection();
        }
        tab.select_range(tab.anchor, i, true);
    } else if mods.command {
        if let Some(r) = tab.item_row(i) {
            tab.selected[r] = !tab.selected[r];
        }
        tab.anchor = i;
    } else {
        tab.clear_selection();
        tab.select_range(i, i, true);
        tab.anchor = i;
    }
    tab.focus = i;
}

fn handle_keys(ui: &mut Ui, tab: &mut Tab, actions: &mut Vec<GridAction>, cmds: &mut Vec<Cmd>, page: usize) {
    let n = tab.item_count();
    let n_cols = tab.visible_columns().count();
    if n == 0 {
        return;
    }
    let mut moved: Option<usize> = None;
    let mut col_moved = false;
    let (shift, events) = ui.input(|i| (i.modifiers.shift, i.events.clone()));
    for ev in events {
        let (key, modifiers) = match ev {
            // The platform integration turns Cmd/Ctrl+C into a Copy event rather than a key.
            egui::Event::Copy => {
                actions.push(GridAction::CopyCell);
                continue;
            }
            egui::Event::Key { key, pressed: true, modifiers, .. } => (key, modifiers),
            _ => continue,
        };
        let cmd = modifiers.command;
        match key {
            Key::ArrowDown => moved = Some((tab.focus + 1).min(n - 1)),
            Key::ArrowUp => moved = Some(tab.focus.saturating_sub(1)),
            Key::PageDown => moved = Some((tab.focus + page.max(1)).min(n - 1)),
            Key::PageUp => moved = Some(tab.focus.saturating_sub(page.max(1))),
            Key::Home if cmd => moved = Some(0),
            Key::End if cmd => moved = Some(n - 1),
            Key::Home => {
                tab.focus_col = 0;
                col_moved = true;
            }
            Key::End => {
                tab.focus_col = n_cols.saturating_sub(1);
                col_moved = true;
            }
            Key::ArrowLeft | Key::ArrowRight => {
                col_moved = true;
                let right = key == Key::ArrowRight;
                if let Some(Item::Group { level, start, .. }) = tab.item(tab.focus) {
                    let open = tab.group_is_expanded(level as usize, tab.rows[start as usize] as usize);
                    if open != right {
                        tab.toggle_group(tab.focus);
                    }
                } else if right {
                    tab.focus_col = (tab.focus_col + 1).min(n_cols.saturating_sub(1));
                } else {
                    tab.focus_col = tab.focus_col.saturating_sub(1);
                }
            }
            Key::Enter => match tab.item(tab.focus) {
                Some(Item::Group { .. }) => tab.toggle_group(tab.focus),
                Some(Item::Row(r)) => {
                    if let Some(c) = tab.visible_columns().nth(tab.focus_col).map(|c| c.col) {
                        actions.push(GridAction::ShowCell { row: r as usize, col: c });
                    }
                }
                None => {}
            },
            Key::Space => actions.push(GridAction::SetTags(!tab.selected_rows().iter().all(|&r| tab.tags[r]))),
            Key::A if cmd => tab.select_all(),
            Key::C if cmd => {
                if modifiers.shift {
                    actions.push(GridAction::Copy { with_header: true });
                } else {
                    actions.push(GridAction::CopyCell);
                }
            }
            Key::Plus | Key::Equals if !cmd => cmds.push(Cmd::ExpandAll(true)),
            Key::Minus if !cmd => cmds.push(Cmd::ExpandAll(false)),
            _ => {}
        }
    }
    if let Some(i) = moved {
        tab.focus = i;
        tab.clear_selection();
        if shift {
            tab.select_range(tab.anchor, i, true);
        } else {
            tab.select_range(i, i, true);
            tab.anchor = i;
        }
    }
    if moved.is_some() || tab.focus_col >= n_cols {
        tab.focus_col = tab.focus_col.min(n_cols.saturating_sub(1));
    }
    tab.scroll_to_focus |= moved.is_some() || col_moved;
}

#[allow(clippy::too_many_arguments)]
fn paint_body(
    ui: &Ui,
    tab: &Tab,
    painter: &egui::Painter,
    pal: &Palette,
    m: &Metrics,
    body: Rect,
    placed: &[Placed],
    ind_w: f32,
    has_focus: bool,
    pin_x: Option<f32>,
) {
    let row_h = m.row_h;
    let first = tab.scroll_row.floor() as usize;
    let frac = (tab.scroll_row - first as f64) as f32;
    let count = (body.height() / row_h).ceil() as usize + 1;
    let p = painter.with_clip_rect(body);
    let right = body.right();
    let visible: Vec<Col> = tab.visible_columns().map(|c| c.col).collect();
    let focus_col = visible.get(tab.focus_col).copied();
    let mut hl: Vec<Range<usize>> = Vec::new();
    let empty_style = RowStyle::default();

    for k in 0..count {
        let i = first + k;
        let Some(item) = tab.item(i) else { break };
        let y = body.top() + (k as f32 - frac) * row_h;
        let row_rect = Rect::from_min_max(pos2(body.left(), y), pos2(right, y + row_h));
        let focused = i == tab.focus;
        match item {
            Item::Group { level, start, len } => {
                p.rect_filled(row_rect, 0.0, if focused { pal.select } else { pal.group });
                let r = tab.rows[start as usize] as usize;
                let (col, _) = tab.shown_group_by[level as usize];
                let open = tab.group_is_expanded(level as usize, r);
                let x = body.left() + ind_w + 6.0 + f32::from(level) * 20.0;
                let arrow = if open { "⏷" } else { "⏵" };
                p.text(
                    pos2(x, row_rect.center().y),
                    Align2::LEFT_CENTER,
                    arrow,
                    FontId::proportional(m.font.size),
                    pal.dim,
                );
                let label = format!(
                    "{}: {}  (Count: {})",
                    tab.col_name(col),
                    display_text(&tab.text(r, col)),
                    fmt_count(len as usize)
                );
                p.text(pos2(x + 18.0, row_rect.center().y), Align2::LEFT_CENTER, label, m.font.clone(), pal.text);
                if focused && has_focus {
                    p.rect_stroke(row_rect.shrink(0.5), 0.0, Stroke::new(1.0, pal.accent), StrokeKind::Inside);
                }
            }
            Item::Row(r) => {
                let r = r as usize;
                let selected = tab.selected[r];
                let styles = if tab.compiled_formats.is_empty() {
                    None
                } else {
                    Some(tab.compiled_formats.row_style(&tab.table, &tab.tags, r))
                };
                let styles = styles.as_ref().unwrap_or(&empty_style);
                if focused {
                    p.text(
                        pos2(body.left() + ind_w / 2.0, row_rect.center().y),
                        Align2::CENTER_CENTER,
                        "▸",
                        FontId::proportional(m.font.size),
                        pal.dim,
                    );
                }
                for pc in placed {
                    let c = &tab.columns[pc.idx];
                    let cell = Rect::from_x_y_ranges(pc.rect_x, row_rect.y_range());
                    let clip = Rect::from_x_y_ranges(pc.clip, row_rect.y_range()).intersect(body);
                    let cp = p.with_clip_rect(clip);
                    let style = styles.cell(c.col);
                    let fill = if selected {
                        Some(if focused && focus_col == Some(c.col) { pal.focus } else { pal.select })
                    } else {
                        style.fill
                    };
                    if let Some(f) = fill {
                        cp.rect_filled(cell, 0.0, f);
                    }
                    match c.col {
                        Col::Tag => checkbox(&cp, cell.center(), Some(tab.tags[r]), pal),
                        col => {
                            let raw = tab.text(r, col);
                            let text = display_text(&raw);
                            hl.clear();
                            if let (Some(q), Col::Data(dc)) = (&tab.search, col) {
                                q.highlight(dc, &text, &mut hl);
                            }
                            let right_align = col == Col::Line;
                            let color = if selected { pal.text } else { style.text.unwrap_or(pal.text) };
                            let st = Style { text: Some(color), ..style };
                            let job = cell_job(&text, &m.font, &st, &hl, pal, cell.width() - 8.0, right_align);
                            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
                            let x = if right_align { cell.right() - 4.0 } else { cell.left() + 4.0 };
                            let pos = pos2(x, cell.center().y - galley.size().y / 2.0);
                            if style.bold {
                                cp.galley(pos + vec2(0.6, 0.0), galley.clone(), color);
                            }
                            cp.galley(pos, galley, color);
                        }
                    }
                    if focused && has_focus && focus_col == Some(c.col) {
                        cp.rect_stroke(cell.shrink(0.5), 0.0, Stroke::new(1.0, pal.accent), StrokeKind::Inside);
                    }
                    cp.vline(cell.right() - 0.5, cell.y_range(), Stroke::new(1.0, pal.line));
                }
                p.vline(body.left() + ind_w - 0.5, row_rect.y_range(), Stroke::new(1.0, pal.line));
                if let Some(x) = pin_x {
                    p.vline(x, row_rect.y_range(), Stroke::new(1.5, pal.dim.gamma_multiply(0.6)));
                }
            }
        }
        p.hline(body.left()..=right, row_rect.bottom() - 0.5, Stroke::new(1.0, pal.line));
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_header(
    ui: &mut Ui,
    tab: &Tab,
    painter: &egui::Painter,
    pal: &Palette,
    m: &Metrics,
    header: Rect,
    placed: &[Placed],
    ind_w: f32,
    grid_id: Id,
    cmds: &mut Vec<Cmd>,
    actions: &mut Vec<GridAction>,
    set: &GridSettings,
) {
    painter.rect_filled(header, 0.0, pal.header);
    let hp = painter.with_clip_rect(header);
    // Dropping a group chip on the header row ungroups that column.
    let header_resp = ui.interact(header, grid_id.with("hdr_bg"), Sense::hover());
    if let Some(p) = take_drop::<GroupChipDrag>(&header_resp) {
        cmds.push(Cmd::Ungroup(p.0));
    }
    let pointer = ui.input(|i| i.pointer.hover_pos());
    for pc in placed {
        let c = &tab.columns[pc.idx];
        let rect = Rect::from_x_y_ranges(pc.rect_x, header.y_range());
        let clip = Rect::from_x_y_ranges(pc.clip, header.y_range());
        if clip.width() <= 0.0 {
            continue;
        }
        let cp = hp.with_clip_rect(clip);
        cp.rect_filled(rect, 0.0, pal.header);
        let resp = ui.interact(clip, grid_id.with(("hdr", c.col)), Sense::click_and_drag());
        let funnel_rect = Rect::from_min_size(pos2(rect.right() - 13.0, rect.top() + 1.0), vec2(11.0, 11.0));
        let filtered = c.filter.is_some();
        let sorted = tab.sort.iter().chain(&tab.group_by).find(|(col, _)| *col == c.col).map(|(_, d)| *d);
        let text_w = rect.width() - 8.0 - if sorted.is_some() { 26.0 } else { 12.0 };
        let galley = ui.ctx().fonts_mut(|f| {
            let mut job = LayoutJob::simple_singleline(c.name.clone(), m.font.clone(), pal.dim);
            job.wrap.max_width = text_w.max(1.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            job.wrap.overflow_character = Some('…');
            f.layout_job(job)
        });
        cp.galley(pos2(rect.left() + 5.0, rect.center().y - galley.size().y / 2.0), galley, pal.dim);
        if let Some(desc) = sorted {
            cp.text(
                pos2(rect.right() - 20.0, rect.center().y + 1.0),
                Align2::CENTER_CENTER,
                if desc { "▼" } else { "▲" },
                FontId::proportional(9.0),
                pal.dim,
            );
        }
        let show_funnel = c.col != Col::Tag && (filtered || resp.hovered());
        if show_funnel {
            funnel(&cp, funnel_rect.center(), 8.0, if filtered { pal.accent } else { pal.dim });
        }
        cp.vline(rect.right() - 0.5, header.y_range(), Stroke::new(1.0, pal.line));

        // Column resize handle.
        let grip = Rect::from_x_y_ranges(rect.right() - 4.0..=rect.right() + 3.0, header.y_range());
        let grip_resp = ui
            .interact(grip, grid_id.with(("grip", c.col)), Sense::click_and_drag())
            .on_hover_cursor(CursorIcon::ResizeHorizontal);
        if grip_resp.dragged()
            && let Some(px) = pointer
        {
            let w = (px.x - rect.left()).max(MIN_COL_WIDTH);
            cmds.push(Cmd::SetWidth(c.col, w));
        }
        if grip_resp.double_clicked() {
            cmds.push(Cmd::BestFit(Some(c.col)));
        }
        if grip_resp.dragged() || grip_resp.hovered() {
            continue;
        }

        let on_funnel = pointer.is_some_and(|p| funnel_rect.contains(p)) && c.col != Col::Tag;
        if resp.clicked() {
            if on_funnel {
                cmds.push(Cmd::OpenFilterPopup(c.col, funnel_rect.left_bottom()));
            } else if tab.group_by.iter().any(|(g, _)| *g == c.col) {
                cmds.push(Cmd::ToggleGroupDir(c.col));
            } else {
                let mods = ui.input(|i| i.modifiers);
                if mods.command {
                    cmds.push(Cmd::ClearSort(Some(c.col)));
                } else {
                    cmds.push(Cmd::Sort { col: c.col, desc: None, add: mods.shift });
                }
            }
        }
        if resp.drag_started() && !on_funnel {
            resp.dnd_set_drag_payload(HeaderDrag(c.col));
        }
        if resp.dragged()
            && let Some(p) = pointer
        {
            let layer = egui::LayerId::new(egui::Order::Tooltip, grid_id.with("drag_hdr"));
            let dp = ui.ctx().layer_painter(layer);
            let g = dp.layout_no_wrap(c.name.clone(), m.font.clone(), pal.text);
            let r = Rect::from_min_size(p + vec2(8.0, 4.0), g.size() + vec2(10.0, 6.0));
            dp.rect_filled(r, 2.0, pal.header);
            dp.rect_stroke(r, 2.0, Stroke::new(1.0, pal.accent), StrokeKind::Inside);
            dp.galley(r.min + vec2(5.0, 3.0), g, pal.text);
        }
        if let Some(p) = resp.dnd_hover_payload::<HeaderDrag>()
            && p.0 != c.col
        {
            painter.vline(rect.left(), header.y_range(), Stroke::new(2.0, pal.accent));
        }
        if let Some(p) = take_drop::<HeaderDrag>(&resp)
            && p.0 != c.col
        {
            cmds.push(Cmd::Move { col: p.0, before: c.col });
        }
        resp.context_menu(|ui| header_menu(ui, tab, c.col, cmds, actions, set));
    }
    // Indicator corner.
    let corner = Rect::from_min_size(header.min, vec2(ind_w, header.height()));
    painter.rect_filled(corner, 0.0, pal.header);
    painter.hline(header.x_range(), header.bottom() - 0.5, Stroke::new(1.0, pal.line));
}

#[allow(clippy::too_many_arguments)]
fn auto_filter_row(
    ui: &mut Ui,
    tab: &mut Tab,
    st: &mut GridState,
    painter: &egui::Painter,
    pal: &Palette,
    m: &Metrics,
    rect: Rect,
    placed: &[Placed],
    ind_w: f32,
    grid_id: Id,
    cmds: &mut Vec<Cmd>,
) {
    painter.rect_filled(rect, 0.0, pal.bg);
    funnel(painter, pos2(rect.left() + ind_w / 2.0, rect.center().y), 9.0, pal.dim);
    for pc in placed {
        let idx = pc.idx;
        let col = tab.columns[idx].col;
        let cell = Rect::from_x_y_ranges(pc.rect_x, rect.y_range());
        let clip = Rect::from_x_y_ranges(pc.clip, rect.y_range());
        if clip.width() <= 2.0 {
            continue;
        }
        let cp = painter.with_clip_rect(clip);
        cp.vline(cell.right() - 0.5, rect.y_range(), Stroke::new(1.0, pal.line));
        if col == Col::Tag {
            let state = match &tab.columns[idx].filter {
                Some(Expr::Cond { op: Op::Equals(v), .. }) => Some(v.eq_ignore_ascii_case("true")),
                _ => None,
            };
            checkbox(&cp, cell.center(), state, pal);
            let resp = ui.interact(clip, grid_id.with(("af_tag",)), Sense::click());
            if resp.clicked() {
                let next = match state {
                    None => Some(true),
                    Some(true) => Some(false),
                    Some(false) => None,
                };
                cmds.push(Cmd::SetFilter(
                    col,
                    next.map(|b| AutoOp::Equals.to_expr("Tag", if b { "True" } else { "False" })),
                ));
            }
            continue;
        }
        let glyph_rect = Rect::from_min_size(cell.min + vec2(2.0, 2.0), vec2(22.0, rect.height() - 4.0));
        let gresp = ui.interact(glyph_rect.intersect(clip), grid_id.with(("af_op", col)), Sense::click());
        if gresp.hovered() {
            cp.rect_filled(glyph_rect, 2.0, pal.select);
        }
        op_glyph(&cp, glyph_rect, tab.columns[idx].auto_op, pal);
        if gresp.clicked() {
            st.op_menu = Some((col, glyph_rect.left_bottom()));
        }
        let edit_rect = Rect::from_min_max(
            pos2(glyph_rect.right() + 2.0, cell.top() + 1.0),
            pos2(cell.right() - 2.0, cell.bottom() - 1.0),
        );
        let visible = edit_rect.intersect(clip);
        if visible.width() < 4.0 {
            continue;
        }
        let mut child = ui.new_child(UiBuilder::new().max_rect(edit_rect).id_salt(("af", col)));
        child.set_clip_rect(visible);
        let c = &mut tab.columns[idx];
        let before = c.auto_text.clone();
        child.add(
            egui::TextEdit::singleline(&mut c.auto_text)
                .id(grid_id.with(("af_text", col)))
                .frame(egui::Frame::NONE)
                .font(m.font.clone())
                .text_color(pal.text)
                .desired_width(edit_rect.width())
                .margin(vec2(2.0, 2.0)),
        );
        if c.auto_text != before {
            let f = (!c.auto_text.is_empty()).then(|| c.auto_op.to_expr(&c.name, &c.auto_text));
            c.filter = f;
            tab.refresh_debounced();
        }
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, pal.line));
}

fn header_menu(
    ui: &mut Ui,
    tab: &Tab,
    col: Col,
    cmds: &mut Vec<Cmd>,
    actions: &mut Vec<GridAction>,
    set: &GridSettings,
) {
    let sorted = tab.sort.iter().any(|(c, _)| *c == col);
    let grouped = tab.group_by.iter().any(|(c, _)| *c == col);
    let c = tab.column(col);
    if ui.button("Sort Ascending").clicked() {
        cmds.push(Cmd::Sort { col, desc: Some(false), add: false });
    }
    if ui.button("Sort Descending").clicked() {
        cmds.push(Cmd::Sort { col, desc: Some(true), add: false });
    }
    if sorted && ui.button("Clear Sorting").clicked() {
        cmds.push(Cmd::ClearSort(Some(col)));
    }
    if !tab.sort.is_empty() && ui.button("Clear All Sorting").clicked() {
        cmds.push(Cmd::ClearSort(None));
    }
    ui.separator();
    if grouped {
        if ui.button("UnGroup").clicked() {
            cmds.push(Cmd::Ungroup(col));
        }
    } else if ui.button("Group By This Column").clicked() {
        cmds.push(Cmd::Group(col));
    }
    if ui.button(if set.show_group_box { "Hide Group By Box" } else { "Show Group By Box" }).clicked() {
        actions.push(GridAction::ToggleGroupBox);
    }
    ui.separator();
    if ui.button("Hide This Column").clicked() {
        cmds.push(Cmd::Hide(col));
    }
    if ui.button("Column Chooser").clicked() {
        actions.push(GridAction::ColumnChooser);
    }
    if ui.button("Best Fit").clicked() {
        cmds.push(Cmd::BestFit(Some(col)));
    }
    if ui.button("Best Fit (all columns)").clicked() {
        cmds.push(Cmd::BestFit(None));
    }
    ui.separator();
    if c.is_some_and(|c| c.filter.is_some()) && ui.button("Clear Filter").clicked() {
        cmds.push(Cmd::ClearFilter(col));
    }
    if ui.button("Filter Editor...").clicked() {
        actions.push(GridAction::FilterEditor);
    }
    if ui.button(if set.show_auto_filter { "Hide Auto Filter Row" } else { "Show Auto Filter Row" }).clicked() {
        actions.push(GridAction::ToggleAutoFilterRow);
    }
    ui.separator();
    ui.menu_button("Conditional Formatting", |ui| {
        ui.menu_button("Highlight Cell Rules", |ui| {
            for (label, kind) in [
                ("Greater Than...", RuleDialog::GreaterThan),
                ("Less Than...", RuleDialog::LessThan),
                ("Between...", RuleDialog::Between),
                ("Equal To...", RuleDialog::EqualTo),
                ("Text that Contains...", RuleDialog::TextContains),
                ("Custom Condition...", RuleDialog::Custom),
            ] {
                if ui.button(label).clicked() {
                    actions.push(GridAction::RuleDialog { col, kind });
                }
            }
        });
        if matches!(col, Col::Data(_)) {
            ui.menu_button("Unique/Duplicate Rules", |ui| {
                if ui.button("Unique Values...").clicked() {
                    actions.push(GridAction::RuleDialog { col, kind: RuleDialog::Unique });
                }
                if ui.button("Duplicate Values...").clicked() {
                    actions.push(GridAction::RuleDialog { col, kind: RuleDialog::Duplicate });
                }
            });
        }
        ui.separator();
        if ui.button("Clear Rules from This Column").clicked() {
            actions.push(GridAction::ClearRules(Some(col)));
        }
        if ui.button("Clear Rules from All Columns").clicked() {
            actions.push(GridAction::ClearRules(None));
        }
        if ui.button("Manage Rules...").clicked() {
            actions.push(GridAction::ManageRules);
        }
    });
}

fn row_menu(ui: &mut Ui, tab: &Tab, st: &GridState, actions: &mut Vec<GridAction>, cmds: &mut Vec<Cmd>) {
    let n_sel = tab.selected_rows().len();
    if ui.button("Copy cell").clicked() {
        actions.push(GridAction::CopyCell);
    }
    if ui.button(format!("Copy selected rows ({n_sel})")).clicked() {
        actions.push(GridAction::Copy { with_header: false });
    }
    if ui.button("Copy selected rows with header").clicked() {
        actions.push(GridAction::Copy { with_header: true });
    }
    ui.separator();
    if ui.button("Tag selected rows").clicked() {
        actions.push(GridAction::SetTags(true));
    }
    if ui.button("Untag selected rows").clicked() {
        actions.push(GridAction::SetTags(false));
    }
    if let Some((i, col)) = st.context_cell
        && let Some(r) = tab.item_row(i)
        && col != Col::Tag
    {
        ui.separator();
        if ui.button("Show cell contents...").clicked() {
            actions.push(GridAction::ShowCell { row: r, col });
        }
        let value = tab.text(r, col).into_owned();
        let name = tab.col_name(col).to_string();
        let short: String = value.chars().take(40).collect();
        if ui.button(format!("Filter: {name} = '{short}'")).clicked() {
            cmds.push(Cmd::SetFilter(col, Some(AutoOp::Equals.to_expr(&name, &value))));
        }
        if ui.button(format!("Filter: {name} <> '{short}'")).clicked() {
            cmds.push(Cmd::SetFilter(col, Some(AutoOp::NotEquals.to_expr(&name, &value))));
        }
    }
    if !tab.group_by.is_empty() {
        ui.separator();
        if ui.button("Expand All Groups").clicked() {
            cmds.push(Cmd::ExpandAll(true));
        }
        if ui.button("Collapse All Groups").clicked() {
            cmds.push(Cmd::ExpandAll(false));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn scrollbar(
    ui: &Ui,
    id: Id,
    track: Rect,
    vertical: bool,
    total: f64,
    visible: f64,
    pos: f64,
    pal: &Palette,
) -> Option<f64> {
    let painter = ui.painter_at(track);
    painter.rect_filled(track, 0.0, pal.header);
    if total <= visible || track.width() < 4.0 || track.height() < 4.0 {
        return None;
    }
    let len = if vertical { track.height() } else { track.width() } as f64;
    let thumb_len = (len * visible / total).max(24.0).min(len);
    let max_pos = total - visible;
    let span = len - thumb_len;
    let thumb_off = if span > 0.0 { (pos / max_pos) * span } else { 0.0 };
    let thumb = if vertical {
        Rect::from_min_size(
            pos2(track.left() + 2.0, track.top() + thumb_off as f32),
            vec2(track.width() - 4.0, thumb_len as f32),
        )
    } else {
        Rect::from_min_size(
            pos2(track.left() + thumb_off as f32, track.top() + 2.0),
            vec2(thumb_len as f32, track.height() - 4.0),
        )
    };
    let resp = ui.interact(track, id, Sense::click_and_drag());
    let active = resp.dragged() || resp.hovered();
    painter.rect_filled(thumb, CornerRadius::same(3), if active { pal.dim } else { pal.dim.gamma_multiply(0.6) });
    let pointer = resp.interact_pointer_pos()?;
    let along = |p: Pos2| if vertical { p.y - track.top() } else { p.x - track.left() } as f64;
    if resp.drag_started() {
        let grab = if thumb.contains(pointer) { along(pointer) - thumb_off } else { thumb_len / 2.0 };
        ui.memory_mut(|m| m.data.insert_temp(id, grab));
    }
    if resp.dragged() {
        if span <= 0.0 {
            return None;
        }
        let grab: f64 = ui.memory(|m| m.data.get_temp(id)).unwrap_or(thumb_len / 2.0);
        let off = (along(pointer) - grab).clamp(0.0, span);
        return Some(off / span * max_pos);
    }
    if resp.clicked() && !thumb.contains(pointer) {
        let before = along(pointer) < thumb_off;
        let page = visible.floor().max(1.0);
        return Some(if before { pos - page } else { pos + page });
    }
    None
}

fn op_menu(ui: &mut Ui, tab: &mut Tab, st: &mut GridState, cmds: &mut Vec<Cmd>) {
    let Some((col, pos)) = st.op_menu else { return };
    let mut open = true;
    let current = tab.column(col).map(|c| c.auto_op);
    egui::Popup::new(
        Id::unique("auto_filter_op_menu"),
        ui.ctx().clone(),
        egui::PopupAnchor::Position(pos),
        ui.layer_id(),
    )
    .kind(egui::PopupKind::Menu)
    .open_bool(&mut open)
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show(|ui| {
        ui.set_min_width(220.0);
        if ui.button("Clear Filter").clicked() {
            cmds.push(Cmd::ClearFilter(col));
            ui.close();
        }
        ui.separator();
        for op in AutoOp::ALL {
            if ui.selectable_label(current == Some(op), format!("{:<3}  {}", op.glyph(), op.label())).clicked() {
                if let Some(c) = tab.column_mut(col) {
                    c.auto_op = op;
                    let f = (!c.auto_text.is_empty()).then(|| op.to_expr(&c.name, &c.auto_text));
                    cmds.push(Cmd::SetFilter(col, f));
                }
                ui.close();
            }
        }
    });
    if !open || ui.input(|i| i.key_pressed(Key::Escape)) {
        st.op_menu = None;
    }
}

/// Distinct values of a column with their counts, sorted with value semantics.
fn distinct_values(tab: &Tab, col: Col) -> Vec<(String, u32)> {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    let table = &tab.table;
    match col {
        Col::Data(c) => {
            for r in 0..table.row_count() {
                *counts.entry(table.cell(r, c)).or_default() += 1;
            }
        }
        Col::Tag => {
            let t = tab.tagged_count() as u32;
            counts.insert("True", t);
            counts.insert("False", table.row_count() as u32 - t);
        }
        Col::Line => {}
    }
    let mut v: Vec<(String, u32)> = counts.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
    v.sort_by(|a, b| cmp_values(&a.0, &b.0).then_with(|| a.0.cmp(&b.0)));
    v
}

fn open_filter_popup(tab: &Tab, col: Col, pos: Pos2) -> FilterPopup {
    let values = distinct_values(tab, col);
    let current = tab.column(col).and_then(|c| c.filter.clone());
    let (set, invert) = match &current {
        Some(Expr::Cond { op: Op::In(list), .. }) => (Some(list.clone()), false),
        Some(Expr::Not(inner)) => match inner.as_ref() {
            Expr::Cond { op: Op::In(list), .. } => (Some(list.clone()), true),
            _ => (None, false),
        },
        _ => (None, false),
    };
    let checked = match &set {
        Some(list) => {
            let s: std::collections::HashSet<&str> = list.iter().map(String::as_str).collect();
            values.iter().map(|(v, _)| s.contains(v.as_str()) != invert).collect()
        }
        None => vec![false; values.len()],
    };
    // Typed columns default to a range (e.g. a time window); text columns to Contains.
    let (default1, default2) = match tab.column(col).map(|c| c.kind).unwrap_or_default() {
        ColKind::Text => (AutoOp::Contains, AutoOp::Contains),
        ColKind::Number | ColKind::DateTime => (AutoOp::GreaterOrEqual, AutoOp::LessOrEqual),
    };
    let (op1, val1) = current.as_ref().and_then(AutoOp::from_expr).unwrap_or((default1, String::new()));
    FilterPopup {
        col,
        pos,
        page: if col == Col::Line || (set.is_none() && current.is_some()) {
            FilterPage::Text
        } else {
            FilterPage::Values
        },
        values,
        checked,
        search: String::new(),
        op1,
        val1,
        join_and: true,
        op2: default2,
        val2: String::new(),
    }
}

fn values_filter(name: &str, values: &[(String, u32)], checked: &[bool]) -> Option<Expr> {
    let n_checked = checked.iter().filter(|&&c| c).count();
    if n_checked == 0 || n_checked == values.len() {
        return None;
    }
    let pick = |want: bool| {
        values.iter().zip(checked).filter(|(_, c)| **c == want).map(|(v, _)| v.0.clone()).collect::<Vec<_>>()
    };
    let cond = |list| Expr::Cond { column: name.to_string(), op: Op::In(list) };
    Some(if n_checked * 2 > values.len() { Expr::Not(Box::new(cond(pick(false)))) } else { cond(pick(true)) })
}

fn filter_popup(ui: &mut Ui, tab: &mut Tab, st: &mut GridState, cmds: &mut Vec<Cmd>) {
    let Some(fp) = st.filter_popup.as_mut() else { return };
    let mut open = true;
    let mut close = false;
    let name = tab.col_name(fp.col).to_string();
    let kind = tab.column(fp.col).map(|c| c.kind).unwrap_or_default();
    egui::Popup::new(
        Id::unique("column_filter_popup"),
        ui.ctx().clone(),
        egui::PopupAnchor::Position(fp.pos),
        ui.layer_id(),
    )
    .open_bool(&mut open)
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show(|ui| {
        ui.set_width(300.0);
        ui.horizontal(|ui| {
            if fp.col != Col::Line {
                ui.selectable_value(&mut fp.page, FilterPage::Values, "Values");
            }
            ui.selectable_value(&mut fp.page, FilterPage::Text, "Text Filters");
        });
        ui.separator();
        match fp.page {
            FilterPage::Values => {
                ui.add(
                    egui::TextEdit::singleline(&mut fp.search)
                        .hint_text("Enter text to search...")
                        .desired_width(f32::INFINITY),
                );
                let needle = fp.search.to_lowercase();
                let shown: Vec<usize> = (0..fp.values.len())
                    .filter(|&i| needle.is_empty() || fp.values[i].0.to_lowercase().contains(&needle))
                    .collect();
                let mut changed = false;
                let all = !shown.is_empty() && shown.iter().all(|&i| fp.checked[i]);
                let mut all_box = all;
                if ui.checkbox(&mut all_box, "(All)").changed() {
                    for &i in &shown {
                        fp.checked[i] = all_box;
                    }
                    changed = true;
                }
                let row_h = ui.spacing().interact_size.y;
                egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show_rows(
                    ui,
                    row_h,
                    shown.len(),
                    |ui, range| {
                        for &i in &shown[range] {
                            let (v, n) = &fp.values[i];
                            let label = if v.is_empty() {
                                "(Blanks)".to_string()
                            } else {
                                display_text(v).chars().take(120).collect()
                            };
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut fp.checked[i], label).changed() {
                                    changed = true;
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.weak(fmt_count(*n as usize));
                                });
                            });
                        }
                    },
                );
                if changed {
                    cmds.push(Cmd::SetFilter(fp.col, values_filter(&name, &fp.values, &fp.checked)));
                }
            }
            FilterPage::Text => {
                ui.label(format!("Show rows where {name}:"));
                op_combo(ui, "op1", &mut fp.op1);
                ui.add(egui::TextEdit::singleline(&mut fp.val1).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    ui.radio_value(&mut fp.join_and, true, "And");
                    ui.radio_value(&mut fp.join_and, false, "Or");
                });
                op_combo(ui, "op2", &mut fp.op2);
                ui.add(egui::TextEdit::singleline(&mut fp.val2).desired_width(f32::INFINITY));
                if kind != ColKind::Text {
                    ui.weak("Values compare numerically / chronologically, e.g. 2022-08-05 06:00:00");
                }
                if ui.button("Apply").clicked() {
                    let mut parts = Vec::new();
                    if !fp.val1.is_empty() {
                        parts.push(fp.op1.to_expr(&name, &fp.val1));
                    }
                    if !fp.val2.is_empty() {
                        parts.push(fp.op2.to_expr(&name, &fp.val2));
                    }
                    let e = match parts.len() {
                        0 => None,
                        1 => parts.pop(),
                        _ if fp.join_and => Some(Expr::And(parts)),
                        _ => Some(Expr::Or(parts)),
                    };
                    cmds.push(Cmd::SetFilter(fp.col, e));
                }
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Clear Filter").clicked() {
                fp.checked.iter_mut().for_each(|c| *c = false);
                cmds.push(Cmd::ClearFilter(fp.col));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
    });
    if !open || close || ui.input(|i| i.key_pressed(Key::Escape)) {
        st.filter_popup = None;
    }
}

fn op_combo(ui: &mut Ui, id: &str, op: &mut AutoOp) {
    egui::ComboBox::from_id_salt(id).selected_text(op.label()).width(280.0).show_ui(ui, |ui| {
        for o in AutoOp::ALL {
            ui.selectable_value(op, o, o.label());
        }
    });
}

fn apply(tab: &mut Tab, st: &mut GridState, cmds: Vec<Cmd>, char_w: f32) {
    let mut popup: Option<(Col, Pos2)> = None;
    for cmd in cmds {
        match cmd {
            Cmd::Sort { col, desc, add } => {
                if let Some(g) = tab.group_by.iter_mut().find(|(c, _)| *c == col) {
                    g.1 = desc.unwrap_or(!g.1);
                } else {
                    let existing = tab.sort.iter().position(|(c, _)| *c == col);
                    let new_desc = desc.unwrap_or_else(|| existing.is_some_and(|i| !tab.sort[i].1));
                    if add {
                        match existing {
                            Some(i) => tab.sort[i].1 = new_desc,
                            None => tab.sort.push((col, new_desc)),
                        }
                    } else {
                        tab.sort = vec![(col, new_desc)];
                    }
                }
                tab.refresh();
            }
            Cmd::ClearSort(col) => {
                match col {
                    Some(c) => tab.sort.retain(|(x, _)| *x != c),
                    None => tab.sort.clear(),
                }
                tab.refresh();
            }
            Cmd::Group(col) => tab.group_by_column(col),
            Cmd::Ungroup(col) => {
                tab.group_by.retain(|(c, _)| *c != col);
                tab.refresh();
            }
            Cmd::ToggleGroupDir(col) => {
                if let Some(g) = tab.group_by.iter_mut().find(|(c, _)| *c == col) {
                    g.1 = !g.1;
                }
                tab.refresh();
            }
            Cmd::Hide(col) => {
                if let Some(c) = tab.column_mut(col) {
                    c.visible = false;
                }
                tab.refresh();
            }
            Cmd::SetWidth(col, w) => {
                if let Some(c) = tab.column_mut(col) {
                    c.width = w;
                }
            }
            Cmd::BestFit(col) => best_fit(tab, col, char_w),
            Cmd::ClearFilter(col) => {
                if let Some(c) = tab.column_mut(col) {
                    c.set_filter(None);
                }
                tab.refresh();
            }
            Cmd::SetFilter(col, f) => {
                if let Some(c) = tab.column_mut(col) {
                    c.set_filter(f);
                }
                tab.refresh_debounced();
            }
            Cmd::Move { col, before } => {
                if let Some(from) = tab.columns.iter().position(|c| c.col == col) {
                    let c = tab.columns.remove(from);
                    let to = tab.columns.iter().position(|c| c.col == before).unwrap_or(tab.columns.len());
                    tab.columns.insert(to, c);
                }
            }
            Cmd::OpenFilterPopup(col, pos) => popup = Some((col, pos)),
            Cmd::ExpandAll(e) => tab.set_all_groups_expanded(e),
        }
    }
    if let Some((col, pos)) = popup {
        st.op_menu = None;
        st.filter_popup = Some(open_filter_popup(tab, col, pos));
    }
}
