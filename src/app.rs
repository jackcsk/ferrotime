//! Application shell: menus, tabs, group-by box + search, filter panel, status bar,
//! search options, dialogs, sessions and keyboard shortcuts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Id, Key, KeyboardShortcut, Modifiers, RichText, Sense, Ui};
use serde::{Deserialize, Serialize};

use crate::data::{self, Table};
use crate::expr::{self, Expr, Op, TokenKind};
use crate::format::{FormatRule, Preset, RuleKind};
use crate::grid::{self, GridAction, GridSettings, GridState, GroupChipDrag, HeaderDrag, RuleDialog, fmt_count};
use crate::search::{Behavior, Condition, MatchCriteria, SearchOptions};
use crate::session::{self, Session, TabSession};
use crate::tle_session::{self, TleFile};
use crate::value::Col;
use crate::view::Tab;

pub const APP_NAME: &str = "Ferrotime";
const MAX_RECENT: usize = 10;

const fn cmd(key: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(Modifiers::COMMAND, key)
}

const fn cmd_shift(key: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), key)
}

const OPEN: KeyboardShortcut = cmd(Key::O);
const CLOSE: KeyboardShortcut = cmd(Key::W);
const FOCUS_SEARCH: KeyboardShortcut = cmd(Key::F);
const GOTO_LINE: KeyboardShortcut = cmd(Key::G);
const TOGGLE_TAG: KeyboardShortcut = cmd(Key::T);
const NEXT_TAGGED: KeyboardShortcut = cmd(Key::CloseBracket);
const PREV_TAGGED: KeyboardShortcut = cmd(Key::OpenBracket);
const FILTER_EDITOR: KeyboardShortcut = cmd_shift(Key::E);
const FIND_NEXT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F3);
const FIND_PREV: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F3);
const NEXT_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Tab);
const PREV_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Tab);
// Handled by the grid's key events; listed here for the help dialog.
const COPY: KeyboardShortcut = cmd(Key::C);
const COPY_WITH_HEADER: KeyboardShortcut = cmd_shift(Key::C);
const SELECT_ALL: KeyboardShortcut = cmd(Key::A);

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    dark: bool,
    font_size: f32,
    show_group_box: bool,
    show_auto_filter: bool,
    search: SearchOptions,
    recent: Vec<PathBuf>,
    last_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dark: false,
            font_size: 13.0,
            show_group_box: true,
            show_auto_filter: true,
            search: SearchOptions::default(),
            recent: Vec::new(),
            last_dir: None,
        }
    }
}

struct Loading {
    path: PathBuf,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<Table, String>>,
    restore: Option<Restore>,
}

/// State to apply once a file finishes loading.
enum Restore {
    Session(TabSession),
    /// 1-based line numbers to tag (Timeline Explorer sessions carry only these).
    Tags(Vec<usize>),
}

enum Slot {
    Loading(Box<Loading>),
    Ready(Box<Tab>),
}

impl Slot {
    fn title(&self) -> String {
        match self {
            Slot::Loading(l) => file_name(&l.path),
            Slot::Ready(t) => t.title.clone(),
        }
    }
    fn path(&self) -> &Path {
        match self {
            Slot::Loading(l) => &l.path,
            Slot::Ready(t) => &t.table.path,
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Replace {
    CrLf,
    Cr,
    Lf,
    Comma,
    Tab,
    Pipe,
}

impl Replace {
    const ALL: [(Replace, &'static str); 6] = [
        (Replace::CrLf, "CRLF"),
        (Replace::Cr, "CR"),
        (Replace::Lf, "LF"),
        (Replace::Comma, "Comma"),
        (Replace::Tab, "Tab"),
        (Replace::Pipe, "¦"),
    ];
    fn separator(self) -> &'static str {
        match self {
            Replace::CrLf => "\r\n",
            Replace::Cr => "\r",
            Replace::Lf => "\n",
            Replace::Comma => ",",
            Replace::Tab => "\t",
            Replace::Pipe => "¦",
        }
    }
}

struct CellViewer {
    id: u64,
    title: String,
    text: String,
    replace: Replace,
    formatted: bool,
    font_size: f32,
}

struct FilterEditor {
    text: String,
    error: Option<String>,
}

struct RuleEditor {
    col: Col,
    kind: RuleDialog,
    v1: String,
    v2: String,
    preset: Preset,
    whole_row: bool,
    error: Option<String>,
}

#[derive(Default)]
struct Dialogs {
    filter_editor: Option<FilterEditor>,
    column_chooser: bool,
    cells: Vec<CellViewer>,
    rule: Option<RuleEditor>,
    manage_rules: bool,
    /// Go-to-line input and whether the last attempt found nothing.
    goto_line: Option<(String, bool)>,
    about: bool,
    shortcuts: bool,
    errors: Vec<String>,
}

enum ChipAction {
    ToggleDir(usize),
    Ungroup(usize),
    Clear,
    Expand(bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DialogAction {
    Ok,
    Apply,
    Cancel,
}

pub struct App {
    slots: Vec<Slot>,
    active: usize,
    grid: GridState,
    settings: Settings,
    show_search_options: bool,
    dialogs: Dialogs,
    next_viewer_id: u64,
    focus_search: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> App {
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let mut app = App {
            slots: Vec::new(),
            active: 0,
            grid: GridState::default(),
            settings,
            show_search_options: false,
            dialogs: Dialogs::default(),
            next_viewer_id: 0,
            focus_search: false,
        };
        install_fonts(&cc.egui_ctx);
        app.apply_theme(&cc.egui_ctx);
        for f in files {
            app.open_path(&cc.egui_ctx, f);
        }
        app
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        ctx.set_theme(if self.settings.dark { egui::Theme::Dark } else { egui::Theme::Light });
        ctx.all_styles_mut(|s| {
            s.visuals.window_corner_radius = egui::CornerRadius::same(2);
            s.visuals.menu_corner_radius = egui::CornerRadius::same(2);
            s.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(1);
            s.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(1);
            s.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(1);
            s.visuals.widgets.active.corner_radius = egui::CornerRadius::same(1);
            s.spacing.button_padding = egui::vec2(6.0, 2.0);
        });
    }

    fn tab(&mut self) -> Option<&mut Tab> {
        match self.slots.get_mut(self.active) {
            Some(Slot::Ready(t)) => Some(t),
            _ => None,
        }
    }

    fn tab_ref(&self) -> Option<&Tab> {
        match self.slots.get(self.active) {
            Some(Slot::Ready(t)) => Some(t),
            _ => None,
        }
    }

    // ----- files ---------------------------------------------------------------------

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let mut dlg = rfd::FileDialog::new()
            .set_title("Open")
            .add_filter("Supported files", &["csv", "tsv", "tab", "txt", "log", "xlsx", "xlsm", "xlsb", "xls", "ods"])
            .add_filter("All files", &["*"]);
        if let Some(d) = &self.settings.last_dir {
            dlg = dlg.set_directory(d);
        }
        for p in dlg.pick_files().unwrap_or_default() {
            self.open_path(ctx, p);
        }
    }

    fn remember(&mut self, path: &Path) {
        self.settings.recent.retain(|p| p != path);
        self.settings.recent.insert(0, path.to_path_buf());
        self.settings.recent.truncate(MAX_RECENT);
        self.settings.last_dir = path.parent().map(Path::to_path_buf);
    }

    pub fn open_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        let path = std::path::absolute(&path).unwrap_or(path);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if ext.eq_ignore_ascii_case(tle_session::EXTENSION) {
            self.load_tle_session(ctx, &path);
            return;
        }
        if ext.eq_ignore_ascii_case(session::EXTENSION) {
            self.load_session(ctx, &path);
            return;
        }
        if let Some(i) = self.slots.iter().position(|s| s.path() == path) {
            if i != self.active {
                self.set_active(i);
            }
            return;
        }
        self.start_loading(ctx, path, None);
    }

    fn start_loading(&mut self, ctx: &egui::Context, path: PathBuf, restore: Option<Restore>) {
        self.remember(&path);
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        {
            let (path, progress, cancel, ctx) = (path.clone(), progress.clone(), cancel.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = data::load(&path, &|f| {
                    progress.store(f.to_bits(), Ordering::Relaxed);
                    !cancel.load(Ordering::Relaxed)
                });
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        }
        self.slots.push(Slot::Loading(Box::new(Loading { path, progress, cancel, rx, restore })));
        self.set_active(self.slots.len() - 1);
    }

    fn poll_loading(&mut self, ui: &Ui) {
        let char_w = grid::metrics(ui, self.settings.font_size).char_w;
        let mut i = 0;
        while i < self.slots.len() {
            if let Slot::Loading(l) = &mut self.slots[i] {
                match l.rx.try_recv() {
                    Ok(Ok(table)) => {
                        let title = file_name(&l.path);
                        let restore = l.restore.take();
                        let mut tab = Tab::new(table, title);
                        grid::best_fit(&mut tab, None, char_w);
                        match restore {
                            Some(Restore::Session(r)) => self.dialogs.errors.extend(r.apply(&mut tab)),
                            Some(Restore::Tags(lines)) => tab.tag_lines(&lines),
                            None => {}
                        }
                        tab.refresh();
                        self.slots[i] = Slot::Ready(Box::new(tab));
                    }
                    Ok(Err(e)) => {
                        if e != data::CANCELLED {
                            self.dialogs.errors.push(e);
                        }
                        self.close_slot(i);
                        continue;
                    }
                    Err(_) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(100)),
                }
            }
            i += 1;
        }
    }

    fn close_slot(&mut self, i: usize) {
        if i >= self.slots.len() {
            return;
        }
        if let Slot::Loading(l) = &self.slots[i] {
            l.cancel.store(true, Ordering::Relaxed);
        }
        self.slots.remove(i);
        let active = if self.active >= i && self.active > 0 { self.active - 1 } else { self.active };
        self.set_active(active);
    }

    /// Switches tabs, dropping popups and dialogs that refer to the previous tab's columns.
    fn set_active(&mut self, i: usize) {
        self.active = i;
        self.grid.close_popups();
        self.dialogs.rule = None;
        self.dialogs.filter_editor = None;
    }

    // ----- sessions ------------------------------------------------------------------

    fn save_session(&mut self) {
        let tabs: Vec<TabSession> = self
            .slots
            .iter()
            .filter_map(|s| match s {
                Slot::Ready(t) => Some(TabSession::capture(t)),
                Slot::Loading(_) => None,
            })
            .collect();
        if tabs.is_empty() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("Save session")
            .add_filter("Ferrotime session", &[session::EXTENSION])
            .set_file_name(format!("session.{}", session::EXTENSION))
            .save_file()
        else {
            return;
        };
        let s = Session { version: session::SESSION_VERSION, active: self.active, tabs };
        if let Err(e) = session::save(&path, &s) {
            self.dialogs.errors.push(e);
        }
    }

    /// Saves open files and their tagged lines in Timeline Explorer's format, so Windows
    /// analysts can open the session there. Filters, sorting and layout are not part of it.
    fn save_tle_session(&mut self) {
        let files: Vec<TleFile> = self
            .slots
            .iter()
            .filter_map(|s| match s {
                Slot::Ready(t) => {
                    Some(TleFile { path: t.table.path.to_string_lossy().into_owned(), tagged_lines: t.tagged_lines() })
                }
                Slot::Loading(_) => None,
            })
            .collect();
        if files.is_empty() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("Save as Timeline Explorer session")
            .add_filter("Timeline Explorer session", &[tle_session::EXTENSION])
            .set_file_name(format!("session.{}", tle_session::EXTENSION))
            .save_file()
        else {
            return;
        };
        if let Err(e) = tle_session::save(&path, &files) {
            self.dialogs.errors.push(e);
        }
    }

    fn load_session_dialog(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Load session")
            .add_filter("Sessions", &[session::EXTENSION, tle_session::EXTENSION])
            .add_filter("Ferrotime session", &[session::EXTENSION])
            .add_filter("Timeline Explorer session", &[tle_session::EXTENSION])
            .pick_file()
        {
            self.open_path(ctx, path);
        }
    }

    /// Replaces any open tab of the same file, then loads it with `restore` applied.
    fn reopen(&mut self, ctx: &egui::Context, path: PathBuf, restore: Restore) {
        if let Some(i) = self.slots.iter().position(|x| x.path() == path) {
            self.close_slot(i);
        }
        self.start_loading(ctx, path, Some(restore));
    }

    fn load_session(&mut self, ctx: &egui::Context, path: &Path) {
        match session::load(path) {
            Ok(s) => {
                // `reopen` replaces tabs of the same file, so find the active tab by path.
                let active = s.tabs.get(s.active).map(|t| t.path.clone());
                for t in s.tabs {
                    self.reopen(ctx, t.path.clone(), Restore::Session(t));
                }
                if let Some(i) = active.and_then(|p| self.slots.iter().position(|x| x.path() == p)) {
                    self.set_active(i);
                }
            }
            Err(e) => self.dialogs.errors.push(e),
        }
    }

    fn load_tle_session(&mut self, ctx: &egui::Context, path: &Path) {
        let files = match tle_session::load(path) {
            Ok(f) => f,
            Err(e) => return self.dialogs.errors.push(e),
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut missing = Vec::new();
        for f in files {
            match tle_session::resolve(&f.path, dir) {
                Some(p) => self.reopen(ctx, p, Restore::Tags(f.tagged_lines)),
                None => missing.push(f.path),
            }
        }
        if !missing.is_empty() {
            self.dialogs.errors.push(format!(
                "{}: these files were not found, either at the recorded path or in the session's folder:\n{}",
                path.display(),
                missing.join("\n")
            ));
        }
    }

    // ----- export / copy -------------------------------------------------------------

    fn export(&mut self, tagged_only: bool) {
        let Some(tab) = self.tab_ref() else { return };
        let stem = tab.table.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export")
            .add_filter("CSV", &["csv"])
            .set_file_name(format!("{stem}_{}.csv", if tagged_only { "tagged" } else { "export" }))
            .save_file()
        else {
            return;
        };
        let rows: Vec<usize> = if tagged_only {
            (0..tab.row_count()).filter(|&r| tab.tags[r]).collect()
        } else {
            tab.rows.iter().map(|&r| r as usize).collect()
        };
        if let Err(e) = export_csv(tab, &path, &rows) {
            self.dialogs.errors.push(format!("{}: {e}", path.display()));
        }
    }

    fn copy_rows(&self, ctx: &egui::Context, with_header: bool) {
        let Some(tab) = self.tab_ref() else { return };
        let cols: Vec<Col> = tab.visible_columns().map(|c| c.col).collect();
        let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
        let mut out = String::new();
        if with_header {
            out.push_str(&cols.iter().map(|c| clean(tab.col_name(*c))).collect::<Vec<_>>().join("\t"));
            out.push('\n');
        }
        for r in tab.selected_rows() {
            out.push_str(&cols.iter().map(|c| clean(&tab.text(r, *c))).collect::<Vec<_>>().join("\t"));
            out.push('\n');
        }
        ctx.copy_text(out);
    }

    fn copy_cell(&self, ctx: &egui::Context) {
        let Some(tab) = self.tab_ref() else { return };
        if tab.selected_rows().len() > 1 {
            self.copy_rows(ctx, false);
            return;
        }
        if let (Some(r), Some(c)) = (tab.item_row(tab.focus), tab.visible_columns().nth(tab.focus_col)) {
            ctx.copy_text(tab.text(r, c.col).into_owned());
        }
    }

    // ----- navigation ----------------------------------------------------------------

    fn find_match(&mut self, forward: bool) {
        let opts = self.settings.search;
        let Some(tab) = self.tab() else { return };
        // The search may have changed since the last background job compiled it.
        tab.recompile_search(&opts);
        let Some(q) = tab.search.clone() else { return };
        let table = tab.table.clone();
        if let Some(i) = tab.find_item(forward, |r| q.matches(&|c| table.cell(r, c))) {
            tab.select_item(i);
            if let Some(r) = tab.item_row(i) {
                let mut hl = Vec::new();
                let pos = tab.visible_columns().position(|c| match c.col {
                    Col::Data(dc) => {
                        q.highlight(dc, table.cell(r, dc), &mut hl);
                        !hl.is_empty()
                    }
                    _ => false,
                });
                if let Some(p) = pos {
                    tab.focus_col = p;
                }
            }
        }
    }

    fn find_tagged(&mut self, forward: bool) {
        let Some(tab) = self.tab() else { return };
        let tags = &tab.tags;
        if let Some(i) = tab.find_item(forward, |r| tags[r]) {
            tab.select_item(i);
        }
    }

    fn set_tags(&mut self, value: Option<bool>) {
        if let Some(tab) = self.tab() {
            let rows = tab.selected_rows();
            match value {
                Some(v) => tab.set_tags(&rows, v),
                None => tab.toggle_tags(&rows),
            }
        }
    }

    fn refresh_all(&mut self) {
        for s in &mut self.slots {
            if let Slot::Ready(t) = s {
                t.refresh();
            }
        }
    }

    fn open_cell(&mut self, row: usize, col: Col) {
        let Some(tab) = self.tab_ref() else { return };
        let title = format!("Cell contents — {} (line {})", tab.col_name(col), row + 1);
        let text = tab.text(row, col).into_owned();
        self.next_viewer_id += 1;
        self.dialogs.cells.push(CellViewer {
            id: self.next_viewer_id,
            title,
            text,
            replace: Replace::Comma,
            formatted: false,
            font_size: 12.0,
        });
    }

    // ----- shortcuts -----------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let pressed = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        if pressed(OPEN) {
            self.open_dialog(ctx);
        }
        if pressed(CLOSE) {
            self.close_slot(self.active);
        }
        if pressed(FOCUS_SEARCH) {
            self.focus_search = true;
        }
        if pressed(GOTO_LINE) {
            self.dialogs.goto_line = Some((String::new(), false));
        }
        if pressed(TOGGLE_TAG) {
            self.set_tags(None);
        }
        if pressed(NEXT_TAGGED) {
            self.find_tagged(true);
        }
        if pressed(PREV_TAGGED) {
            self.find_tagged(false);
        }
        // Shifted variants first: consume_shortcut ignores extra modifiers.
        if pressed(FIND_PREV) {
            self.find_match(false);
        }
        if pressed(FIND_NEXT) {
            self.find_match(true);
        }
        let n = self.slots.len();
        if pressed(PREV_TAB) && n > 1 {
            self.set_active((self.active + n - 1) % n);
        }
        if pressed(NEXT_TAB) && n > 1 {
            self.set_active((self.active + 1) % n);
        }
        if pressed(FILTER_EDITOR) {
            self.dialogs.filter_editor = self.tab_ref().map(new_filter_editor);
        }
    }

    // ----- top area ------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(menu_item(&ctx, "Open...", &OPEN)).clicked() {
                    self.open_dialog(&ctx);
                }
                ui.menu_button("Open Recent", |ui| {
                    if self.settings.recent.is_empty() {
                        ui.weak("(empty)");
                    }
                    for p in self.settings.recent.clone() {
                        if ui.button(p.display().to_string()).clicked() {
                            self.open_path(&ctx, p);
                        }
                    }
                    if !self.settings.recent.is_empty() {
                        ui.separator();
                        if ui.button("Clear list").clicked() {
                            self.settings.recent.clear();
                        }
                    }
                });
                if ui.add_enabled(!self.slots.is_empty(), menu_item(&ctx, "Close", &CLOSE)).clicked() {
                    self.close_slot(self.active);
                }
                ui.separator();
                ui.menu_button("Session", |ui| {
                    if ui.button("Load...").clicked() {
                        self.load_session_dialog(&ctx);
                    }
                    if ui.add_enabled(self.tab_ref().is_some(), egui::Button::new("Save...")).clicked() {
                        self.save_session();
                    }
                    let tle = egui::Button::new("Save as Timeline Explorer session...");
                    if ui
                        .add_enabled(self.tab_ref().is_some(), tle)
                        .on_hover_text(
                            "Saves the open files and tagged lines as a .tle_sess file for Timeline Explorer on \
                             Windows. Filters, sorting and layout are not included.",
                        )
                        .clicked()
                    {
                        self.save_tle_session();
                    }
                });
                ui.menu_button("Export", |ui| {
                    let ok = self.tab_ref().is_some();
                    if ui.add_enabled(ok, egui::Button::new("Visible rows to CSV...")).clicked() {
                        self.export(false);
                    }
                    if ui.add_enabled(ok, egui::Button::new("Tagged rows to CSV...")).clicked() {
                        self.export(true);
                    }
                });
                ui.separator();
                if ui.button("Exit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Tools", |ui| {
                let ok = self.tab_ref().is_some();
                if ui.add_enabled(ok, menu_item(&ctx, "Go to line #...", &GOTO_LINE)).clicked() {
                    self.dialogs.goto_line = Some((String::new(), false));
                }
                ui.separator();
                if ui.add_enabled(ok, menu_item(&ctx, "Tag/untag selected rows", &TOGGLE_TAG)).clicked() {
                    self.set_tags(None);
                }
                if ui.add_enabled(ok, menu_item(&ctx, "Go to next tagged row", &NEXT_TAGGED)).clicked() {
                    self.find_tagged(true);
                }
                if ui.add_enabled(ok, menu_item(&ctx, "Go to previous tagged row", &PREV_TAGGED)).clicked() {
                    self.find_tagged(false);
                }
                if ui.add_enabled(ok, egui::Button::new("Clear all tags")).clicked()
                    && let Some(t) = self.tab()
                {
                    let all: Vec<usize> = (0..t.row_count()).collect();
                    t.set_tags(&all, false);
                }
                ui.separator();
                if ui.add_enabled(ok, menu_item(&ctx, "Filter Editor...", &FILTER_EDITOR)).clicked() {
                    self.dialogs.filter_editor = self.tab_ref().map(new_filter_editor);
                }
                if ui.add_enabled(ok, egui::Button::new("Column Chooser")).clicked() {
                    self.dialogs.column_chooser = true;
                }
                if ui.add_enabled(ok, egui::Button::new("Reset column widths")).clicked() {
                    let cw = grid::metrics(ui, self.settings.font_size).char_w;
                    if let Some(t) = self.tab() {
                        grid::best_fit(t, None, cw);
                    }
                }
                ui.separator();
                ui.menu_button("Skins", |ui| {
                    let mut dark = self.settings.dark;
                    ui.radio_value(&mut dark, false, "Light");
                    ui.radio_value(&mut dark, true, "Dark");
                    if dark != self.settings.dark {
                        self.settings.dark = dark;
                        self.apply_theme(&ctx);
                    }
                });
            });
            ui.menu_button("Tabs", |ui| {
                for i in 0..self.slots.len() {
                    if ui.radio(self.active == i, self.slots[i].title()).clicked() && self.active != i {
                        self.set_active(i);
                    }
                }
                if !self.slots.is_empty() {
                    ui.separator();
                }
                if ui.add_enabled(!self.slots.is_empty(), egui::Button::new("Close current tab")).clicked() {
                    self.close_slot(self.active);
                }
                if ui.add_enabled(!self.slots.is_empty(), egui::Button::new("Close all tabs")).clicked() {
                    while !self.slots.is_empty() {
                        self.close_slot(0);
                    }
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.settings.show_group_box, "Group By Box");
                ui.checkbox(&mut self.settings.show_auto_filter, "Auto Filter Row");
                ui.checkbox(&mut self.show_search_options, "Search options");
                ui.separator();
                let grouped = self.tab_ref().is_some_and(|t| !t.group_by.is_empty());
                if ui.add_enabled(grouped, egui::Button::new("Expand all groups")).clicked()
                    && let Some(t) = self.tab()
                {
                    t.set_all_groups_expanded(true);
                }
                if ui.add_enabled(grouped, egui::Button::new("Collapse all groups")).clicked()
                    && let Some(t) = self.tab()
                {
                    t.set_all_groups_expanded(false);
                }
                if ui.add_enabled(grouped, egui::Button::new("Clear grouping")).clicked()
                    && let Some(t) = self.tab()
                {
                    t.group_by.clear();
                    t.refresh();
                }
                let sorted = self.tab_ref().is_some_and(|t| !t.sort.is_empty());
                if ui.add_enabled(sorted, egui::Button::new("Clear sorting")).clicked()
                    && let Some(t) = self.tab()
                {
                    t.sort.clear();
                    t.refresh();
                }
                let filtered = self.tab_ref().is_some_and(Tab::has_filter);
                if ui.add_enabled(filtered, egui::Button::new("Clear filter")).clicked()
                    && let Some(t) = self.tab()
                {
                    t.clear_filters();
                }
                ui.separator();
                ui.menu_button("Grid font size", |ui| {
                    if ui.button("Larger").clicked() {
                        self.settings.font_size = (self.settings.font_size + 1.0).min(28.0);
                    }
                    if ui.button("Smaller").clicked() {
                        self.settings.font_size = (self.settings.font_size - 1.0).max(8.0);
                    }
                    if ui.button("Reset").clicked() {
                        self.settings.font_size = Settings::default().font_size;
                    }
                });
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts").clicked() {
                    self.dialogs.shortcuts = true;
                }
                if ui.button(format!("About {APP_NAME}")).clicked() {
                    self.dialogs.about = true;
                }
            });
        });
    }

    fn tab_strip(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            egui::ScrollArea::horizontal().id_salt("tabs").max_width(ui.available_width() - 24.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for i in 0..self.slots.len() {
                        let title = self.slots[i].title();
                        let path = self.slots[i].path().display().to_string();
                        let resp = ui.selectable_label(self.active == i, title).on_hover_text(path);
                        if resp.clicked() && self.active != i {
                            self.set_active(i);
                        }
                        if resp.middle_clicked() {
                            self.close_slot(i);
                            break;
                        }
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.slots.is_empty() && ui.small_button("×").on_hover_text("Close").clicked() {
                    self.close_slot(self.active);
                }
            });
        });
    }

    fn group_box_and_search(&mut self, ui: &mut Ui) {
        let show_box = self.settings.show_group_box;
        let opts = self.settings.search;
        let focus_search = std::mem::take(&mut self.focus_search);
        let mut find_clicked = None;
        let mono = FontId::monospace(13.0);
        let Some(tab) = self.tab() else { return };
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            let search_w = 260.0;
            let box_w = (ui.available_width() - search_w - 120.0).max(100.0);
            if show_box {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(box_w, 26.0), Sense::hover());
                let hovering = resp.dnd_hover_payload::<HeaderDrag>().is_some();
                if hovering {
                    ui.painter().rect_stroke(
                        rect,
                        2.0,
                        egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                        egui::StrokeKind::Inside,
                    );
                }
                if let Some(p) = grid::take_drop::<HeaderDrag>(&resp) {
                    tab.group_by_column(p.0);
                }
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect.shrink2(egui::vec2(4.0, 0.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                if tab.group_by.is_empty() {
                    child.label(
                        RichText::new("Drag a column header here to group by that column").font(mono.clone()).weak(),
                    );
                } else {
                    let mut action = None;
                    for (i, (col, desc)) in tab.group_by.iter().enumerate() {
                        if i > 0 {
                            child.weak("›");
                        }
                        let label = format!("{} {}", tab.col_name(*col), if *desc { "▼" } else { "▲" });
                        let chip = child.add(
                            egui::Button::new(RichText::new(label).font(mono.clone())).sense(Sense::click_and_drag()),
                        );
                        if chip.clicked() {
                            action = Some(ChipAction::ToggleDir(i));
                        }
                        if chip.drag_started() {
                            chip.dnd_set_drag_payload(GroupChipDrag(*col));
                        }
                        chip.context_menu(|ui| {
                            if ui.button("UnGroup").clicked() {
                                action = Some(ChipAction::Ungroup(i));
                            }
                            if ui.button("Clear Grouping").clicked() {
                                action = Some(ChipAction::Clear);
                            }
                            ui.separator();
                            if ui.button("Full Expand").clicked() {
                                action = Some(ChipAction::Expand(true));
                            }
                            if ui.button("Full Collapse").clicked() {
                                action = Some(ChipAction::Expand(false));
                            }
                        });
                    }
                    match action {
                        Some(ChipAction::ToggleDir(i)) => {
                            tab.group_by[i].1 = !tab.group_by[i].1;
                            tab.refresh();
                        }
                        Some(ChipAction::Ungroup(i)) => {
                            tab.group_by.remove(i);
                            tab.refresh();
                        }
                        Some(ChipAction::Clear) => {
                            tab.group_by.clear();
                            tab.refresh();
                        }
                        Some(ChipAction::Expand(e)) => tab.set_all_groups_expanded(e),
                        None => {}
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if opts.behavior == Behavior::Search {
                    if ui.small_button("▼").on_hover_text("Find next (F3)").clicked() {
                        find_clicked = Some(true);
                    }
                    if ui.small_button("▲").on_hover_text("Find previous (Shift+F3)").clicked() {
                        find_clicked = Some(false);
                    }
                }
                if ui.button("Find").clicked() {
                    tab.refresh();
                    if opts.behavior == Behavior::Search {
                        find_clicked = Some(true);
                    }
                }
                let id = Id::unique("search_box");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut tab.search_text)
                        .id(id)
                        .hint_text("Enter text to search...")
                        .desired_width(search_w),
                );
                if focus_search {
                    resp.request_focus();
                }
                if resp.changed() {
                    tab.refresh_debounced();
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    tab.refresh();
                    if opts.behavior == Behavior::Search {
                        find_clicked = Some(true);
                    }
                }
                if let Some(e) = &tab.search_error {
                    ui.colored_label(ui.visuals().error_fg_color, "⚠").on_hover_text(e.as_str());
                } else if opts.behavior == Behavior::Search && tab.search.is_some() {
                    ui.weak(format!("{} matching rows", fmt_count(tab.search_hits)));
                }
            });
        });
        if let Some(fwd) = find_clicked {
            self.find_match(fwd);
        }
    }

    // ----- bottom area ---------------------------------------------------------------

    fn filter_panel(&mut self, ui: &mut Ui) {
        let mut open_editor = false;
        let Some(tab) = self.tab() else { return };
        ui.horizontal(|ui| {
            if ui.small_button("×").on_hover_text("Clear filter").clicked() {
                tab.clear_filters();
                return;
            }
            if ui.checkbox(&mut tab.filter_enabled, "").on_hover_text("Enable/disable filter").changed() {
                tab.refresh();
            }
            let text = tab.combined_filter().map(|e| e.to_string()).unwrap_or_default();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Edit Filter").clicked() {
                    open_editor = true;
                }
                if let Some(e) = &tab.filter_error {
                    ui.colored_label(ui.visuals().error_fg_color, format!("⚠ {e}"));
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let mut job = highlight_job(ui, &text, FontId::monospace(13.0), !tab.filter_enabled);
                    job.wrap.max_width = ui.available_width();
                    job.wrap.max_rows = 1;
                    job.wrap.overflow_character = Some('…');
                    if ui
                        .add(egui::Label::new(job).truncate().sense(Sense::click()))
                        .on_hover_text(text.as_str())
                        .clicked()
                    {
                        open_editor = true;
                    }
                });
            });
        });
        if open_editor {
            self.dialogs.filter_editor = self.tab_ref().map(new_filter_editor);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let open_files = self.slots.len();
        ui.horizontal(|ui| {
            match self.slots.get(self.active) {
                Some(Slot::Ready(t)) => {
                    ui.label(t.table.path.display().to_string());
                }
                Some(Slot::Loading(l)) => {
                    let f = f32::from_bits(l.progress.load(Ordering::Relaxed));
                    ui.label(format!("Loading {}", l.path.display()));
                    ui.add(egui::ProgressBar::new(f).desired_width(200.0).show_percentage());
                    if ui.small_button("Cancel").clicked() {
                        l.cancel.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    ui.weak("Open a CSV or Excel file with File ▸ Open, or drop files here");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.selectable_label(self.show_search_options, "⚙ Search options").clicked() {
                    self.show_search_options = !self.show_search_options;
                }
                ui.separator();
                ui.label(format!("Open files: {open_files}"));
                if let Some(Slot::Ready(t)) = self.slots.get(self.active) {
                    ui.separator();
                    ui.label(format!("Tagged lines {}", fmt_count(t.tagged_count())));
                    ui.separator();
                    ui.label(format!("Visible lines {}", fmt_count(t.rows.len())));
                    ui.separator();
                    ui.label(format!("Total lines {}", fmt_count(t.row_count())));
                    if t.computing {
                        ui.separator();
                        ui.spinner();
                    }
                }
            });
        });
    }

    fn search_options(&mut self, ui: &mut Ui) {
        let before = self.settings.search;
        ui.horizontal(|ui| {
            ui.strong("Search options");
        });
        ui.horizontal_wrapped(|ui| {
            let o = &mut self.settings.search;
            ui.label("Condition");
            ui.group(|ui| {
                ui.add_enabled_ui(o.criteria != MatchCriteria::Mixed, |ui| {
                    ui.radio_value(&mut o.condition, Condition::Contains, "Contains");
                    ui.radio_value(&mut o.condition, Condition::StartsWith, "Starts with");
                    ui.radio_value(&mut o.condition, Condition::Like, "Like");
                    ui.radio_value(&mut o.condition, Condition::Equals, "Equals");
                });
            });
            ui.label("Match criteria");
            ui.group(|ui| {
                ui.radio_value(&mut o.criteria, MatchCriteria::Mixed, "Mixed");
                ui.radio_value(&mut o.criteria, MatchCriteria::Or, "Or");
                ui.radio_value(&mut o.criteria, MatchCriteria::And, "And");
                ui.radio_value(&mut o.criteria, MatchCriteria::Exact, "Exact");
            });
            ui.label("Behavior");
            ui.group(|ui| {
                ui.radio_value(&mut o.behavior, Behavior::Filter, "Filter");
                ui.radio_value(&mut o.behavior, Behavior::Search, "Search");
            });
        });
        if let Some(tab) = self.tab() {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("×").on_hover_text("Unpin all columns").clicked() {
                        tab.first_scrollable = None;
                    }
                    let current = tab
                        .first_scrollable
                        .map(|c| tab.col_name(c).to_string())
                        .unwrap_or_else(|| "Select a column to pin".into());
                    let cols: Vec<(Col, String)> = tab.columns.iter().map(|c| (c.col, c.name.clone())).collect();
                    egui::ComboBox::from_id_salt("first_scrollable").selected_text(current).width(220.0).show_ui(
                        ui,
                        |ui| {
                            for (col, name) in cols {
                                ui.selectable_value(&mut tab.first_scrollable, Some(col), name);
                            }
                        },
                    );
                    ui.label("First scrollable column");
                });
            });
        }
        if self.settings.search != before {
            self.refresh_all();
        }
    }

    // ----- dialogs -------------------------------------------------------------------

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.filter_editor(ctx);
        self.column_chooser(ctx);
        self.cell_viewers(ctx);
        self.rule_editor(ctx);
        self.manage_rules(ctx);
        self.goto_line(ctx);
        self.about(ctx);
        self.shortcuts_help(ctx);
        if !self.dialogs.errors.is_empty() {
            let mut close = false;
            egui::Modal::new(Id::unique("errors")).show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.heading("Error");
                for e in &self.dialogs.errors {
                    ui.label(e);
                }
                if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape)) {
                    close = true;
                }
            });
            if close {
                self.dialogs.errors.clear();
            }
        }
    }

    fn filter_editor(&mut self, ctx: &egui::Context) {
        let Some(fe) = self.dialogs.filter_editor.as_mut() else { return };
        let mut open = true;
        let mut action = None;
        let window = dialog(ctx, "Filter Editor").open(&mut open).default_size([560.0, 420.0]).collapsible(false);
        window.show(ctx, |ui| {
            let parsed = if fe.text.trim().is_empty() { None } else { Some(expr::parse(&fe.text)) };
            let tree_area =
                egui::ScrollArea::vertical().id_salt("fe_tree").max_height(180.0).auto_shrink([false, true]);
            tree_area.show(ui, |ui| match &parsed {
                None => {
                    ui.weak("(no filter)");
                }
                Some(Ok(e)) => tree(ui, e, 0),
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
            });
            ui.separator();
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                let mut job = highlight_job(ui, buf.as_str(), FontId::monospace(13.0), false);
                job.wrap.max_width = wrap;
                ui.fonts_mut(|f| f.layout_job(job))
            };
            ui.add(
                egui::TextEdit::multiline(&mut fe.text)
                    .code_editor()
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter),
            );
            ui.weak(
                "Functions: Contains, StartsWith, EndsWith, Regex, IsNullOrEmpty · \
                 Operators: = <> < <= > >= Like In Between · And Or Not",
            );
            if let Some(e) = &fe.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Apply").clicked() {
                        action = Some(DialogAction::Apply);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(DialogAction::Cancel);
                    }
                    if ui.button("OK").clicked() {
                        action = Some(DialogAction::Ok);
                    }
                });
            });
        });
        if !open || action == Some(DialogAction::Cancel) {
            self.dialogs.filter_editor = None;
            return;
        }
        let Some(action) = action else { return };
        let text = fe.text.clone();
        let result = if text.trim().is_empty() { Ok(None) } else { expr::parse(&text).map(Some) };
        let Some(tab) = self.tab() else { return };
        let checked = result.and_then(|e| {
            if let Some(e) = &e {
                e.compile(&|n| tab.resolve(n))?;
            }
            Ok(e)
        });
        match checked {
            Ok(e) => {
                tab.set_filter_expr(e);
                if action == DialogAction::Ok {
                    self.dialogs.filter_editor = None;
                } else if let Some(fe) = self.dialogs.filter_editor.as_mut() {
                    fe.error = None;
                }
            }
            Err(e) => {
                if let Some(fe) = self.dialogs.filter_editor.as_mut() {
                    fe.error = Some(e);
                }
            }
        }
    }

    fn column_chooser(&mut self, ctx: &egui::Context) {
        if !self.dialogs.column_chooser {
            return;
        }
        let mut open = true;
        let Some(tab) = self.tab() else {
            self.dialogs.column_chooser = false;
            return;
        };
        let mut changed = false;
        dialog(ctx, "Column Chooser").open(&mut open).default_width(260.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Show all").clicked() {
                    tab.columns.iter_mut().for_each(|c| c.visible = true);
                    changed = true;
                }
                if ui.button("Hide all").clicked() {
                    tab.columns.iter_mut().filter(|c| matches!(c.col, Col::Data(_))).for_each(|c| c.visible = false);
                    changed = true;
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                for c in &mut tab.columns {
                    changed |= ui.checkbox(&mut c.visible, c.name.as_str()).changed();
                }
            });
        });
        if changed {
            tab.refresh();
        }
        self.dialogs.column_chooser = open;
    }

    fn cell_viewers(&mut self, ctx: &egui::Context) {
        let mut closed = Vec::new();
        for v in &mut self.dialogs.cells {
            let mut open = true;
            dialog(ctx, v.title.as_str())
                .id(Id::unique(("cell_viewer", v.id)))
                .open(&mut open)
                .default_size([640.0, 360.0])
                .resizable(true)
                .show(ctx, |ui| {
                    egui::Panel::bottom(Id::unique(("cv_bottom", v.id))).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.toggle_value(&mut v.formatted, "☰ Format");
                            ui.separator();
                            ui.label("Replace:");
                            for (r, label) in Replace::ALL {
                                ui.radio_value(&mut v.replace, r, label);
                            }
                            ui.separator();
                            ui.label("Font size");
                            ui.add(egui::DragValue::new(&mut v.font_size).range(8.0..=32.0));
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(v.text.clone());
                            }
                        });
                    });
                    let shown = if v.formatted {
                        let sep = v.replace.separator();
                        let mut s = v.text.replace(sep, "\n");
                        if v.replace != Replace::CrLf {
                            s = s.replace("\r\n", "\n");
                        }
                        s.lines().map(str::trim).collect::<Vec<_>>().join("\n")
                    } else {
                        v.text.clone()
                    };
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        let mut s = shown.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut s)
                                .font(FontId::monospace(v.font_size))
                                .desired_width(f32::INFINITY)
                                .frame(egui::Frame::NONE),
                        );
                    });
                });
            if !open {
                closed.push(v.id);
            }
        }
        self.dialogs.cells.retain(|v| !closed.contains(&v.id));
    }

    fn rule_editor(&mut self, ctx: &egui::Context) {
        let Some(re) = self.dialogs.rule.as_mut() else { return };
        let Some(Slot::Ready(tab)) = self.slots.get_mut(self.active) else {
            self.dialogs.rule = None;
            return;
        };
        let name = tab.col_name(re.col).to_string();
        let (title, prompt) = match re.kind {
            RuleDialog::GreaterThan => ("Greater Than", "Format cells that are GREATER THAN:"),
            RuleDialog::LessThan => ("Less Than", "Format cells that are LESS THAN:"),
            RuleDialog::Between => ("Between", "Format cells that are BETWEEN:"),
            RuleDialog::EqualTo => ("Equal To", "Format cells that are EQUAL TO:"),
            RuleDialog::TextContains => ("Text that Contains", "Format cells that contain the text:"),
            RuleDialog::Custom => ("Custom Condition", "Format cells that match the condition:"),
            RuleDialog::Unique => ("Unique Values", "Format cells that contain UNIQUE values"),
            RuleDialog::Duplicate => ("Duplicate Values", "Format cells that contain DUPLICATE values"),
        };
        let mut open = true;
        let mut ok = false;
        let mut cancel = false;
        dialog(ctx, title).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(format!("{prompt}  [{name}]"));
            ui.horizontal(|ui| {
                match re.kind {
                    RuleDialog::Unique | RuleDialog::Duplicate => {}
                    RuleDialog::Between => {
                        ui.add(egui::TextEdit::singleline(&mut re.v1).desired_width(140.0));
                        ui.label("and");
                        ui.add(egui::TextEdit::singleline(&mut re.v2).desired_width(140.0));
                    }
                    RuleDialog::Custom => {
                        ui.add(
                            egui::TextEdit::singleline(&mut re.v1).font(FontId::monospace(13.0)).desired_width(360.0),
                        );
                    }
                    _ => {
                        ui.add(egui::TextEdit::singleline(&mut re.v1).desired_width(200.0));
                    }
                }
                ui.label("with");
                egui::ComboBox::from_id_salt("rule_preset").selected_text(re.preset.label()).width(240.0).show_ui(
                    ui,
                    |ui| {
                        for p in Preset::ALL {
                            ui.selectable_value(&mut re.preset, p, p.label());
                        }
                    },
                );
            });
            ui.checkbox(&mut re.whole_row, "Apply formatting to an entire row");
            if let Some(e) = &re.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if ui.button("OK").clicked() {
                        ok = true;
                    }
                });
            });
        });
        if !open || cancel {
            self.dialogs.rule = None;
            return;
        }
        if !ok {
            return;
        }
        let cond = |op| Expr::Cond { column: name.clone(), op }.to_string();
        let kind = match re.kind {
            RuleDialog::GreaterThan => RuleKind::Criteria(cond(Op::Greater(re.v1.clone()))),
            RuleDialog::LessThan => RuleKind::Criteria(cond(Op::Less(re.v1.clone()))),
            RuleDialog::Between => RuleKind::Criteria(cond(Op::Between(re.v1.clone(), re.v2.clone()))),
            RuleDialog::EqualTo => RuleKind::Criteria(cond(Op::Equals(re.v1.clone()))),
            RuleDialog::TextContains => RuleKind::Criteria(cond(Op::Contains(re.v1.clone()))),
            RuleDialog::Custom => RuleKind::Criteria(re.v1.clone()),
            RuleDialog::Unique => RuleKind::Unique,
            RuleDialog::Duplicate => RuleKind::Duplicate,
        };
        if let RuleKind::Criteria(text) = &kind
            && let Err(e) = expr::parse(text).and_then(|e| e.compile(&|n| tab.resolve(n)))
        {
            re.error = Some(e);
            return;
        }
        tab.formats.push(FormatRule { column: name, kind, preset: re.preset, whole_row: re.whole_row, enabled: true });
        tab.recompile_formats();
        self.dialogs.rule = None;
    }

    fn manage_rules(&mut self, ctx: &egui::Context) {
        if !self.dialogs.manage_rules {
            return;
        }
        let mut open = true;
        let Some(Slot::Ready(tab)) = self.slots.get_mut(self.active) else {
            self.dialogs.manage_rules = false;
            return;
        };
        let before = tab.formats.clone();
        dialog(ctx, "Conditional Formatting Rules Manager").open(&mut open).default_width(720.0).show(ctx, |ui| {
            if tab.formats.is_empty() {
                ui.weak("No rules. Right-click a column header ▸ Conditional Formatting to add one.");
            }
            let mut remove = None;
            let mut swap = None;
            let n = tab.formats.len();
            egui::Grid::new("rules").striped(true).num_columns(6).show(ui, |ui| {
                ui.strong("On");
                ui.strong("Column");
                ui.strong("Rule");
                ui.strong("Format");
                ui.strong("Entire row");
                ui.label("");
                ui.end_row();
                for (i, r) in tab.formats.iter_mut().enumerate() {
                    ui.checkbox(&mut r.enabled, "");
                    ui.label(&r.column);
                    ui.label(RichText::new(r.describe()).monospace());
                    egui::ComboBox::from_id_salt(("preset", i)).selected_text(r.preset.label()).width(220.0).show_ui(
                        ui,
                        |ui| {
                            for p in Preset::ALL {
                                ui.selectable_value(&mut r.preset, p, p.label());
                            }
                        },
                    );
                    ui.checkbox(&mut r.whole_row, "");
                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("⏶")).clicked() {
                            swap = Some(i - 1);
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("⏷")).clicked() {
                            swap = Some(i);
                        }
                        if ui.button("Delete").clicked() {
                            remove = Some(i);
                        }
                    });
                    ui.end_row();
                }
            });
            if let Some(i) = remove {
                tab.formats.remove(i);
            }
            if let Some(i) = swap {
                tab.formats.swap(i, i + 1);
            }
            for e in &tab.compiled_formats.errors {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        });
        if tab.formats != before {
            tab.recompile_formats();
        }
        self.dialogs.manage_rules = open;
    }

    fn goto_line(&mut self, ctx: &egui::Context) {
        let Some((text, not_found)) = self.dialogs.goto_line.as_mut() else { return };
        let mut open = true;
        let mut go = false;
        dialog(ctx, "Go to line").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Line #");
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(120.0));
                if text.is_empty() && !r.has_focus() {
                    r.request_focus();
                }
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    go = true;
                }
                if ui.button("Go").clicked() {
                    go = true;
                }
            });
            if *not_found {
                ui.colored_label(ui.visuals().error_fg_color, "Line not found among visible rows.");
            }
        });
        if go {
            let line = text.trim().parse::<usize>().ok();
            let found = match (line, self.tab()) {
                (Some(l), Some(t)) => t.goto_line(l),
                _ => false,
            };
            match self.dialogs.goto_line.as_mut() {
                Some(_) if found => open = false,
                Some((_, nf)) => *nf = true,
                None => {}
            }
        }
        if !open {
            self.dialogs.goto_line = None;
        }
    }

    fn about(&mut self, ctx: &egui::Context) {
        let mut open = self.dialogs.about;
        dialog(ctx, format!("About {APP_NAME}")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading(format!("{APP_NAME} v{}", env!("CARGO_PKG_VERSION")));
            ui.label(
                "Cross-platform timeline analysis for DFIR, written in Rust. Views, filters, sorts, groups and tags \
                 CSV and Excel timelines on macOS and Linux.",
            );
            ui.separator();
            ui.label(
                "Inspired by Timeline Explorer, a Windows tool by Eric Zimmerman. Ferrotime follows its layout \
                 and workflow so that Timeline Explorer users feel at home.",
            );
            ui.label(
                RichText::new(
                    "Ferrotime is an independent project. It is not affiliated with, endorsed by, or supported by \
                     Eric Zimmerman, and it contains no Timeline Explorer code. Please do not send Ferrotime \
                     questions or bug reports to Eric Zimmerman.",
                )
                .strong(),
            );
            ui.label(
                "No guarantee of accuracy: Ferrotime has not been validated for forensic or evidentiary use. Verify \
                 important findings against the original evidence or with another tool. Provided \"as is\", without \
                 warranty of any kind, under the MIT License.",
            );
            ui.hyperlink_to("Eric Zimmerman's Timeline Explorer", "https://ericzimmerman.github.io/");
        });
        self.dialogs.about = open;
    }

    fn shortcuts_help(&mut self, ctx: &egui::Context) {
        let mut open = self.dialogs.shortcuts;
        let f = |s: &KeyboardShortcut| ctx.format_shortcut(s);
        let pair = |a: &KeyboardShortcut, b: &KeyboardShortcut| format!("{} / {}", f(a), f(b));
        let rows = [
            (f(&OPEN), "Open file"),
            (f(&CLOSE), "Close tab"),
            (f(&FOCUS_SEARCH), "Focus search box"),
            (pair(&FIND_NEXT, &FIND_PREV), "Next / previous search match (Search behavior)"),
            (f(&TOGGLE_TAG), "Tag/untag selected rows (also Space in the grid)"),
            (pair(&NEXT_TAGGED, &PREV_TAGGED), "Next / previous tagged row"),
            (f(&GOTO_LINE), "Go to line"),
            (f(&FILTER_EDITOR), "Filter Editor"),
            (f(&COPY), "Copy cell (rows when several are selected)"),
            (f(&COPY_WITH_HEADER), "Copy selected rows with header"),
            (f(&SELECT_ALL), "Select all rows"),
            ("Enter / double-click".into(), "Show cell contents; toggle group"),
            ("← / →".into(), "Move between cells; collapse/expand group"),
            ("+ / -".into(), "Expand / collapse all groups"),
            ("Shift+click header".into(), "Add column to multi-column sort"),
            (
                (if cfg!(target_os = "macos") { "⌘+click header" } else { "Ctrl+click header" }).into(),
                "Clear sorting on that column",
            ),
            (pair(&NEXT_TAB, &PREV_TAB), "Next / previous tab"),
        ];
        dialog(ctx, "Keyboard shortcuts").open(&mut open).collapsible(false).show(ctx, |ui| {
            egui::Grid::new("shortcuts").striped(true).show(ui, |ui| {
                for (k, d) in &rows {
                    ui.monospace(k);
                    ui.label(*d);
                    ui.end_row();
                }
            });
        });
        self.dialogs.shortcuts = open;
    }

    fn handle_grid_actions(&mut self, ctx: &egui::Context, actions: Vec<GridAction>) {
        for a in actions {
            match a {
                GridAction::ShowCell { row, col } => self.open_cell(row, col),
                GridAction::FilterEditor => self.dialogs.filter_editor = self.tab_ref().map(new_filter_editor),
                GridAction::ColumnChooser => self.dialogs.column_chooser = true,
                GridAction::ToggleGroupBox => self.settings.show_group_box = !self.settings.show_group_box,
                GridAction::ToggleAutoFilterRow => self.settings.show_auto_filter = !self.settings.show_auto_filter,
                GridAction::RuleDialog { col, kind } => {
                    let Some(tab) = self.tab_ref() else { continue };
                    let v1 = match kind {
                        RuleDialog::Custom => {
                            Expr::Cond { column: tab.col_name(col).to_string(), op: Op::Equals(String::new()) }
                                .to_string()
                        }
                        _ => tab.item_row(tab.focus).map(|r| tab.text(r, col).into_owned()).unwrap_or_default(),
                    };
                    self.dialogs.rule = Some(RuleEditor {
                        col,
                        kind,
                        v1,
                        v2: String::new(),
                        preset: Preset::LightRedFillDarkRedText,
                        whole_row: false,
                        error: None,
                    });
                }
                GridAction::ClearRules(col) => {
                    if let Some(tab) = self.tab() {
                        let name = col.map(|c| tab.col_name(c).to_string());
                        tab.formats.retain(|r| name.as_ref().is_some_and(|n| *n != r.column));
                        tab.recompile_formats();
                    }
                }
                GridAction::ManageRules => self.dialogs.manage_rules = true,
                GridAction::Copy { with_header } => self.copy_rows(ctx, with_header),
                GridAction::CopyCell => self.copy_cell(ctx),
                GridAction::SetTags(v) => self.set_tags(Some(v)),
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for p in dropped {
            self.open_path(&ctx, p);
        }
        self.poll_loading(ui);
        let opts = self.settings.search;
        for s in &mut self.slots {
            if let Slot::Ready(t) = s {
                t.poll(&ctx, &opts);
            }
        }
        self.shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        if !self.slots.is_empty() {
            egui::Panel::top("tabs").show(ui, |ui| self.tab_strip(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.show_search_options {
            egui::Panel::bottom("search_options").show(ui, |ui| self.search_options(ui));
        }
        if self.tab_ref().is_some_and(Tab::has_filter) {
            egui::Panel::bottom("filter_panel").show(ui, |ui| self.filter_panel(ui));
        }
        if self.tab_ref().is_some() {
            egui::Panel::top("group_box").show(ui, |ui| self.group_box_and_search(ui));
        }

        let set = GridSettings {
            font_size: self.settings.font_size,
            show_auto_filter: self.settings.show_auto_filter,
            show_group_box: self.settings.show_group_box,
            dark: self.settings.dark,
        };
        let mut actions = Vec::new();
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let ui_ref = ui;
            match self.slots.get_mut(self.active) {
                Some(Slot::Ready(tab)) => actions = grid::show(ui_ref, tab, &mut self.grid, &set),
                Some(Slot::Loading(l)) => {
                    let f = f32::from_bits(l.progress.load(Ordering::Relaxed));
                    ui_ref.centered_and_justified(|ui| {
                        ui.add(egui::ProgressBar::new(f).desired_width(400.0).text(format!(
                            "Loading {} ({:.0}%)",
                            file_name(&l.path),
                            f * 100.0
                        )));
                    });
                }
                None => {
                    ui_ref.centered_and_justified(|ui| {
                        ui.label(RichText::new("File ▸ Open… or drop CSV / Excel files here").size(18.0).weak());
                    });
                }
            }
        });
        self.handle_grid_actions(&ctx, actions);
        self.dialogs(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    /// Window background behind unpainted areas (no file open, loading). eframe's default is a
    /// fixed near-black, which showed as a black pane in the Light skin.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

/// Menu button showing `shortcut` on the right.
fn menu_item<'a>(ctx: &egui::Context, text: &'a str, shortcut: &KeyboardShortcut) -> egui::Button<'a> {
    egui::Button::new(text).shortcut_text(ctx.format_shortcut(shortcut))
}

/// Dialog window centred in the main window, like the modal dialogs of Timeline Explorer.
fn dialog<'a>(ctx: &egui::Context, title: impl Into<egui::WidgetText>) -> egui::Window<'a> {
    egui::Window::new(title).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center())
}

/// Lets proportional text fall back to the monospace font for symbols such as `¦`
/// that forensic tools use as field separators.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mono: Vec<String> = fonts.families.get(&egui::FontFamily::Monospace).cloned().unwrap_or_default();
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        for f in mono {
            if !prop.contains(&f) {
                prop.push(f);
            }
        }
    }
    ctx.set_fonts(fonts);
}

fn new_filter_editor(tab: &Tab) -> FilterEditor {
    FilterEditor { text: tab.combined_filter().map(|e| e.to_string()).unwrap_or_default(), error: None }
}

fn export_csv(tab: &Tab, path: &Path, rows: &[usize]) -> Result<(), String> {
    let cols: Vec<Col> = tab.visible_columns().map(|c| c.col).filter(|c| *c != Col::Tag).collect();
    let mut w = csv::Writer::from_path(path).map_err(|e| e.to_string())?;
    w.write_record(cols.iter().map(|c| tab.col_name(*c))).map_err(|e| e.to_string())?;
    for &r in rows {
        w.write_record(cols.iter().map(|c| tab.text(r, *c).into_owned())).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())
}

fn token_color(ui: &Ui, kind: TokenKind) -> Color32 {
    let dark = ui.visuals().dark_mode;
    let rgb = Color32::from_rgb;
    match kind {
        TokenKind::Keyword => {
            if dark {
                rgb(230, 120, 120)
            } else {
                rgb(180, 40, 40)
            }
        }
        TokenKind::Function => {
            if dark {
                rgb(110, 170, 255)
            } else {
                rgb(30, 90, 200)
            }
        }
        TokenKind::Column => {
            if dark {
                rgb(240, 170, 90)
            } else {
                rgb(200, 110, 20)
            }
        }
        TokenKind::String | TokenKind::Number => {
            if dark {
                rgb(120, 200, 120)
            } else {
                rgb(30, 130, 50)
            }
        }
        TokenKind::Operator | TokenKind::Punct => ui.visuals().text_color(),
        TokenKind::Error => ui.visuals().error_fg_color,
    }
}

fn highlight_job(ui: &Ui, text: &str, font: FontId, dim: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = ui.visuals().text_color();
    let fmt = |c: Color32| TextFormat {
        font_id: font.clone(),
        color: if dim { c.gamma_multiply(0.45) } else { c },
        ..Default::default()
    };
    let mut pos = 0;
    for (range, kind) in expr::highlight(text) {
        if range.start < pos || range.end > text.len() {
            continue;
        }
        if range.start > pos {
            job.append(&text[pos..range.start], 0.0, fmt(plain));
        }
        job.append(&text[range.clone()], 0.0, fmt(token_color(ui, kind)));
        pos = range.end;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, fmt(plain));
    }
    job
}

fn chip(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.25))
        .corner_radius(2.0)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(text);
        });
}

fn op_label(op: &Op) -> (&'static str, Vec<String>) {
    match op {
        Op::Equals(v) => ("Equals", vec![v.clone()]),
        Op::NotEquals(v) => ("Does not equal", vec![v.clone()]),
        Op::Less(v) => ("Is less than", vec![v.clone()]),
        Op::LessOrEqual(v) => ("Is less than or equal to", vec![v.clone()]),
        Op::Greater(v) => ("Is greater than", vec![v.clone()]),
        Op::GreaterOrEqual(v) => ("Is greater than or equal to", vec![v.clone()]),
        Op::Between(a, b) => ("Is between", vec![a.clone(), b.clone()]),
        Op::Contains(v) => ("Contains", vec![v.clone()]),
        Op::StartsWith(v) => ("Begins with", vec![v.clone()]),
        Op::EndsWith(v) => ("Ends with", vec![v.clone()]),
        Op::Like(v) => ("Is like", vec![v.clone()]),
        Op::Regex(v) => ("Matches regex", vec![v.clone()]),
        Op::In(list) => ("Is any of", list.clone()),
        Op::IsEmpty => ("Is blank", vec![]),
    }
}

/// Read-only structural view of a filter, styled like Timeline Explorer's filter tree.
fn tree(ui: &mut Ui, e: &Expr, depth: usize) {
    let red = Color32::from_rgb(220, 60, 60);
    let orange = Color32::from_rgb(230, 140, 40);
    let blue = Color32::from_rgb(60, 120, 220);
    let green = Color32::from_rgb(50, 160, 70);
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 18.0);
        match e {
            Expr::And(_) => chip(ui, "And", red),
            Expr::Or(_) => chip(ui, "Or", red),
            Expr::Not(_) => chip(ui, "Not", red),
            Expr::Cond { column, op } => {
                chip(ui, column, orange);
                let (label, values) = op_label(op);
                chip(ui, label, blue);
                for v in values {
                    chip(ui, &v, green);
                }
            }
        }
    });
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().for_each(|x| tree(ui, x, depth + 1)),
        Expr::Not(inner) => tree(ui, inner, depth + 1),
        Expr::Cond { .. } => {}
    }
}
