//! Per-tab view state: column layout, filters, sorting, grouping, tags, selection,
//! and the background filter/sort pipeline that produces the visible row order.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::data::Table;
use crate::expr::{self, Compiled, Expr, Op};
use crate::format::{CompiledRules, FormatRule};
use crate::search::{Behavior, Query, SearchOptions};
use crate::value::{Col, ColKind, cmp_ci, parse_datetime, parse_number};

pub const MIN_COL_WIDTH: f32 = 24.0;
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Operators offered by the auto filter row (the `ABC` button under each header).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoOp {
    Equals,
    NotEquals,
    Contains,
    NotContains,
    Like,
    NotLike,
    BeginsWith,
    EndsWith,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
}

impl AutoOp {
    pub const ALL: [AutoOp; 12] = [
        AutoOp::Equals,
        AutoOp::NotEquals,
        AutoOp::Contains,
        AutoOp::NotContains,
        AutoOp::Like,
        AutoOp::NotLike,
        AutoOp::BeginsWith,
        AutoOp::EndsWith,
        AutoOp::Greater,
        AutoOp::GreaterOrEqual,
        AutoOp::Less,
        AutoOp::LessOrEqual,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AutoOp::Equals => "Equals",
            AutoOp::NotEquals => "Does not equal",
            AutoOp::Contains => "Contains",
            AutoOp::NotContains => "Does not contain",
            AutoOp::Like => "Is like",
            AutoOp::NotLike => "Is not like",
            AutoOp::BeginsWith => "Begins with",
            AutoOp::EndsWith => "Ends with",
            AutoOp::Greater => "Is greater than",
            AutoOp::GreaterOrEqual => "Is greater than or equal to",
            AutoOp::Less => "Is less than",
            AutoOp::LessOrEqual => "Is less than or equal to",
        }
    }

    /// Short glyph drawn in the auto filter row button.
    pub fn glyph(self) -> &'static str {
        match self {
            AutoOp::Equals => "=",
            AutoOp::NotEquals => "≠",
            AutoOp::Contains | AutoOp::NotContains => "ABC",
            AutoOp::Like | AutoOp::NotLike => "A%C",
            AutoOp::BeginsWith => "AB·",
            AutoOp::EndsWith => "·BC",
            AutoOp::Greater => ">",
            AutoOp::GreaterOrEqual => "≥",
            AutoOp::Less => "<",
            AutoOp::LessOrEqual => "≤",
        }
    }

    pub fn is_negative(self) -> bool {
        matches!(self, AutoOp::NotContains | AutoOp::NotLike | AutoOp::NotEquals)
    }

    pub fn to_expr(self, column: &str, text: &str) -> Expr {
        let v = text.to_string();
        let cond = |op| Expr::Cond { column: column.to_string(), op };
        let not = |e| Expr::Not(Box::new(e));
        match self {
            AutoOp::Equals => cond(Op::Equals(v)),
            AutoOp::NotEquals => cond(Op::NotEquals(v)),
            AutoOp::Contains => cond(Op::Contains(v)),
            AutoOp::NotContains => not(cond(Op::Contains(v))),
            AutoOp::Like => cond(Op::Like(v)),
            AutoOp::NotLike => not(cond(Op::Like(v))),
            AutoOp::BeginsWith => cond(Op::StartsWith(v)),
            AutoOp::EndsWith => cond(Op::EndsWith(v)),
            AutoOp::Greater => cond(Op::Greater(v)),
            AutoOp::GreaterOrEqual => cond(Op::GreaterOrEqual(v)),
            AutoOp::Less => cond(Op::Less(v)),
            AutoOp::LessOrEqual => cond(Op::LessOrEqual(v)),
        }
    }

    /// Inverse of [`AutoOp::to_expr`] so the auto filter row can display a column filter
    /// that was created elsewhere (popup, session).
    pub fn from_expr(e: &Expr) -> Option<(AutoOp, String)> {
        match e {
            Expr::Not(inner) => match inner.as_ref() {
                Expr::Cond { op: Op::Contains(v), .. } => Some((AutoOp::NotContains, v.clone())),
                Expr::Cond { op: Op::Like(v), .. } => Some((AutoOp::NotLike, v.clone())),
                _ => None,
            },
            Expr::Cond { op, .. } => Some(match op {
                Op::Equals(v) => (AutoOp::Equals, v.clone()),
                Op::NotEquals(v) => (AutoOp::NotEquals, v.clone()),
                Op::Contains(v) => (AutoOp::Contains, v.clone()),
                Op::Like(v) => (AutoOp::Like, v.clone()),
                Op::StartsWith(v) => (AutoOp::BeginsWith, v.clone()),
                Op::EndsWith(v) => (AutoOp::EndsWith, v.clone()),
                Op::Greater(v) => (AutoOp::Greater, v.clone()),
                Op::GreaterOrEqual(v) => (AutoOp::GreaterOrEqual, v.clone()),
                Op::Less(v) => (AutoOp::Less, v.clone()),
                Op::LessOrEqual(v) => (AutoOp::LessOrEqual, v.clone()),
                _ => return None,
            }),
            _ => None,
        }
    }
}

pub struct ColumnState {
    pub col: Col,
    pub name: String,
    pub kind: ColKind,
    pub visible: bool,
    pub width: f32,
    /// The column's own filter (auto filter row, header popup). Combined with `And`.
    pub filter: Option<Expr>,
    pub auto_op: AutoOp,
    pub auto_text: String,
}

impl ColumnState {
    /// Sets the column filter and mirrors it into the auto filter row when representable.
    pub fn set_filter(&mut self, filter: Option<Expr>) {
        match filter.as_ref().and_then(AutoOp::from_expr) {
            Some((op, text)) => {
                self.auto_op = op;
                self.auto_text = text;
            }
            None => self.auto_text.clear(),
        }
        self.filter = filter;
    }
}

/// One line of the grid body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Row(u32),
    /// Group header; `start..start+len` indexes into the filtered+sorted row list.
    Group {
        level: u16,
        start: u32,
        len: u32,
    },
}

enum SortKeys {
    Num(Vec<f64>),
    Time(Vec<i64>),
}

type KeyCache = Arc<Mutex<HashMap<usize, Arc<SortKeys>>>>;

struct SortSpec {
    col: Col,
    desc: bool,
    keys: Option<Arc<SortKeys>>,
}

struct JobResult {
    rows: Vec<u32>,
    search_hits: usize,
    group_by: Vec<(Col, bool)>,
}

/// Everything needed to compare rows: shared by the worker pipeline and grouping.
struct Comparer<'a> {
    table: &'a Table,
    tags: &'a [bool],
    specs: &'a [SortSpec],
}

impl Comparer<'_> {
    fn cmp_col(&self, spec: &SortSpec, a: u32, b: u32) -> Ordering {
        let (ra, rb) = (a as usize, b as usize);
        match spec.col {
            Col::Line => a.cmp(&b),
            Col::Tag => self.tags[ra].cmp(&self.tags[rb]),
            Col::Data(c) => {
                let by_text = || cmp_ci(self.table.cell(ra, c), self.table.cell(rb, c));
                match spec.keys.as_deref() {
                    Some(SortKeys::Num(k)) => match (k[ra].is_nan(), k[rb].is_nan()) {
                        (false, false) => k[ra].total_cmp(&k[rb]),
                        (true, true) => by_text(),
                        (x, _) => x.cmp(&!x),
                    },
                    Some(SortKeys::Time(k)) => match (k[ra] == i64::MIN, k[rb] == i64::MIN) {
                        (false, false) => k[ra].cmp(&k[rb]),
                        (true, true) => by_text(),
                        (x, _) => x.cmp(&!x),
                    },
                    None => by_text(),
                }
            }
        }
    }

    fn cmp(&self, a: u32, b: u32) -> Ordering {
        for spec in self.specs {
            let o = self.cmp_col(spec, a, b);
            if o != Ordering::Equal {
                return if spec.desc { o.reverse() } else { o };
            }
        }
        a.cmp(&b)
    }
}

fn sort_specs(table: &Table, cols: &[(Col, bool)], cache: &KeyCache) -> Vec<SortSpec> {
    cols.iter()
        .map(|&(col, desc)| SortSpec {
            col,
            desc,
            keys: match col {
                Col::Data(c) => sort_keys(table, c, cache),
                _ => None,
            },
        })
        .collect()
}

fn sort_keys(table: &Table, col: usize, cache: &KeyCache) -> Option<Arc<SortKeys>> {
    let kind = table.kinds[col];
    if kind == ColKind::Text {
        return None;
    }
    if let Some(k) = cache.lock().unwrap().get(&col) {
        return Some(k.clone());
    }
    let n = table.row_count();
    let keys = Arc::new(match kind {
        ColKind::Number => SortKeys::Num(
            (0..n).into_par_iter().map(|r| parse_number(table.cell(r, col)).unwrap_or(f64::NAN)).collect(),
        ),
        _ => SortKeys::Time(
            (0..n).into_par_iter().map(|r| parse_datetime(table.cell(r, col)).unwrap_or(i64::MIN)).collect(),
        ),
    });
    cache.lock().unwrap().insert(col, keys.clone());
    Some(keys)
}

/// Text of any column for a row, including the synthetic Line/Tag columns.
pub fn cell_text<'a>(table: &'a Table, tags: &[bool], row: usize, col: Col) -> Cow<'a, str> {
    match col {
        Col::Line => Cow::Owned((row + 1).to_string()),
        Col::Tag => Cow::Borrowed(if tags[row] { "True" } else { "False" }),
        Col::Data(c) => Cow::Borrowed(table.cell(row, c)),
    }
}

pub struct Tab {
    pub table: Arc<Table>,
    pub title: String,
    pub columns: Vec<ColumnState>,
    /// Columns before this one (in display order) stay pinned while scrolling horizontally.
    pub first_scrollable: Option<Col>,
    pub sort: Vec<(Col, bool)>,
    pub group_by: Vec<(Col, bool)>,
    pub groups_expanded_default: bool,
    pub group_toggled: HashSet<String>,
    /// Filter built in the Filter Editor; combined with column filters.
    pub advanced_filter: Option<Expr>,
    pub filter_enabled: bool,
    pub filter_error: Option<String>,
    pub search_text: String,
    pub search_error: Option<String>,
    pub search: Option<Arc<Query>>,
    pub tags: Vec<bool>,
    pub selected: Vec<bool>,
    pub formats: Vec<FormatRule>,
    pub compiled_formats: CompiledRules,

    // Visible order.
    pub rows: Arc<Vec<u32>>,
    pub items: Option<Vec<Item>>,
    /// Grouping that `rows`/`items` were built with; `group_by` may be newer while a job runs.
    pub shown_group_by: Vec<(Col, bool)>,
    pub search_hits: usize,

    // Cursor and scroll position (display indices).
    pub focus: usize,
    pub focus_col: usize,
    pub anchor: usize,
    pub scroll_row: f64,
    pub scroll_x: f32,
    pub scroll_to_focus: bool,

    key_cache: KeyCache,
    generation: Arc<AtomicU64>,
    job: Option<Receiver<JobResult>>,
    pending_since: Option<Instant>,
    pub computing: bool,
    filter_uses_tag: bool,
}

impl Tab {
    pub fn new(table: Table, title: String) -> Tab {
        let table = Arc::new(table);
        let n = table.row_count();
        let mut columns = vec![
            ColumnState {
                col: Col::Line,
                name: "Line".into(),
                kind: ColKind::Number,
                visible: true,
                width: 60.0,
                filter: None,
                auto_op: AutoOp::Equals,
                auto_text: String::new(),
            },
            ColumnState {
                col: Col::Tag,
                name: "Tag".into(),
                kind: ColKind::Text,
                visible: true,
                width: 44.0,
                filter: None,
                auto_op: AutoOp::Equals,
                auto_text: String::new(),
            },
        ];
        for (i, h) in table.headers.iter().enumerate() {
            columns.push(ColumnState {
                col: Col::Data(i),
                name: h.clone(),
                kind: table.kinds[i],
                visible: true,
                width: 100.0,
                filter: None,
                auto_op: AutoOp::Contains,
                auto_text: String::new(),
            });
        }
        let first_scrollable = table.headers.first().map(|_| Col::Data(0));
        Tab {
            title,
            columns,
            first_scrollable,
            sort: Vec::new(),
            group_by: Vec::new(),
            shown_group_by: Vec::new(),
            groups_expanded_default: false,
            group_toggled: HashSet::new(),
            advanced_filter: None,
            filter_enabled: true,
            filter_error: None,
            search_text: String::new(),
            search_error: None,
            search: None,
            tags: vec![false; n],
            selected: vec![false; n],
            formats: Vec::new(),
            compiled_formats: CompiledRules::default(),
            rows: Arc::new((0..n as u32).collect()),
            items: None,
            search_hits: 0,
            focus: 0,
            focus_col: 0,
            anchor: 0,
            scroll_row: 0.0,
            scroll_x: 0.0,
            scroll_to_focus: false,
            key_cache: Arc::default(),
            generation: Arc::default(),
            job: None,
            pending_since: None,
            computing: false,
            filter_uses_tag: false,
            table,
        }
    }

    pub fn col_name(&self, col: Col) -> &str {
        match col {
            Col::Line => "Line",
            Col::Tag => "Tag",
            Col::Data(i) => &self.table.headers[i],
        }
    }

    pub fn column(&self, col: Col) -> Option<&ColumnState> {
        self.columns.iter().find(|c| c.col == col)
    }

    pub fn column_mut(&mut self, col: Col) -> Option<&mut ColumnState> {
        self.columns.iter_mut().find(|c| c.col == col)
    }

    /// Resolves a column name from a filter expression.
    pub fn resolve(&self, name: &str) -> Option<Col> {
        if name.eq_ignore_ascii_case("Line") {
            return Some(Col::Line);
        }
        if name.eq_ignore_ascii_case("Tag") {
            return Some(Col::Tag);
        }
        let h = &self.table.headers;
        h.iter().position(|x| x == name).or_else(|| h.iter().position(|x| x.eq_ignore_ascii_case(name))).map(Col::Data)
    }

    pub fn visible_columns(&self) -> impl Iterator<Item = &ColumnState> {
        self.columns.iter().filter(|c| c.visible)
    }

    pub fn row_count(&self) -> usize {
        self.table.row_count()
    }

    pub fn text(&self, row: usize, col: Col) -> Cow<'_, str> {
        cell_text(&self.table, &self.tags, row, col)
    }

    // ----- display list --------------------------------------------------------------

    pub fn item_count(&self) -> usize {
        self.items.as_ref().map_or(self.rows.len(), Vec::len)
    }

    pub fn item(&self, i: usize) -> Option<Item> {
        match &self.items {
            Some(items) => items.get(i).copied(),
            None => self.rows.get(i).map(|&r| Item::Row(r)),
        }
    }

    pub fn item_row(&self, i: usize) -> Option<usize> {
        match self.item(i)? {
            Item::Row(r) => Some(r as usize),
            Item::Group { .. } => None,
        }
    }

    fn group_path(&self, level: usize, row: usize) -> String {
        let mut path = String::new();
        for (col, _) in &self.shown_group_by[..=level] {
            path.push_str(&self.text(row, *col));
            path.push('\u{1f}');
        }
        path
    }

    pub fn group_is_expanded(&self, level: usize, row: usize) -> bool {
        self.groups_expanded_default != self.group_toggled.contains(&self.group_path(level, row))
    }

    pub fn toggle_group(&mut self, item: usize) {
        if let Some(Item::Group { level, start, .. }) = self.item(item) {
            let row = self.rows[start as usize] as usize;
            let path = self.group_path(level as usize, row);
            if !self.group_toggled.remove(&path) {
                self.group_toggled.insert(path);
            }
            self.rebuild_items();
        }
    }

    pub fn set_all_groups_expanded(&mut self, expanded: bool) {
        self.groups_expanded_default = expanded;
        self.group_toggled.clear();
        self.rebuild_items();
    }

    fn specs(&self, cols: &[(Col, bool)]) -> Vec<SortSpec> {
        sort_specs(&self.table, cols, &self.key_cache)
    }

    /// Groups by `col` (keeping its sort direction, if sorted) unless already grouped.
    pub fn group_by_column(&mut self, col: Col) {
        if self.group_by.iter().any(|(c, _)| *c == col) {
            return;
        }
        let desc = self.sort.iter().find(|(c, _)| *c == col).is_some_and(|(_, d)| *d);
        self.sort.retain(|(c, _)| *c != col);
        self.group_by.push((col, desc));
        self.refresh();
    }

    /// Rebuilds the item list (group headers + expanded rows) from the sorted rows.
    pub fn rebuild_items(&mut self) {
        let focus_item = self.item(self.focus);
        if self.shown_group_by.is_empty() {
            self.items = None;
        } else {
            let specs = self.specs(&self.shown_group_by);
            let cmp = Comparer { table: &self.table, tags: &self.tags, specs: &specs };
            let mut out = Vec::new();
            self.build_groups(&cmp, 0, 0..self.rows.len(), &mut out);
            self.items = Some(out);
        }
        self.restore_focus(focus_item);
    }

    fn build_groups(&self, cmp: &Comparer<'_>, level: usize, range: std::ops::Range<usize>, out: &mut Vec<Item>) {
        let spec = &cmp.specs[level];
        let rows = &self.rows;
        let mut i = range.start;
        while i < range.end {
            let mut j = i + 1;
            while j < range.end && cmp.cmp_col(spec, rows[i], rows[j]) == Ordering::Equal {
                j += 1;
            }
            out.push(Item::Group { level: level as u16, start: i as u32, len: (j - i) as u32 });
            if self.group_is_expanded(level, rows[i] as usize) {
                if level + 1 < cmp.specs.len() {
                    self.build_groups(cmp, level + 1, i..j, out);
                } else {
                    out.extend(rows[i..j].iter().map(|&r| Item::Row(r)));
                }
            }
            i = j;
        }
    }

    fn restore_focus(&mut self, previous: Option<Item>) {
        let n = self.item_count();
        let found = previous.and_then(|p| {
            let row_of = |it: Item| match it {
                Item::Row(r) => Some(r),
                Item::Group { start, .. } => self.rows.get(start as usize).copied(),
            };
            let want = row_of(p)?;
            let is_group = matches!(p, Item::Group { .. });
            (0..n).find(|&i| {
                let it = self.item(i).unwrap();
                matches!(it, Item::Group { .. }) == is_group && row_of(it) == Some(want)
            })
        });
        self.focus = found.unwrap_or(0).min(n.saturating_sub(1));
        self.anchor = self.focus;
        self.scroll_to_focus = true;
    }

    // ----- filtering pipeline ----------------------------------------------------------

    pub fn combined_filter(&self) -> Option<Expr> {
        let mut parts: Vec<Expr> = self.columns.iter().filter_map(|c| c.filter.clone()).collect();
        parts.extend(self.advanced_filter.clone());
        expr::and_all(parts)
    }

    pub fn has_filter(&self) -> bool {
        self.advanced_filter.is_some() || self.columns.iter().any(|c| c.filter.is_some())
    }

    pub fn clear_filters(&mut self) {
        for c in &mut self.columns {
            c.set_filter(None);
        }
        self.advanced_filter = None;
        self.refresh();
    }

    /// Replaces all filters with `expr` (Filter Editor apply): column filters that are
    /// top-level conjuncts on a single column move back into their columns.
    pub fn set_filter_expr(&mut self, expr: Option<Expr>) {
        for c in &mut self.columns {
            c.set_filter(None);
        }
        let parts = match expr {
            None => Vec::new(),
            Some(Expr::And(parts)) => parts,
            Some(e) => vec![e],
        };
        let mut rest = Vec::new();
        for part in parts {
            let target = single_column(&part).and_then(|name| self.resolve(name));
            match target.and_then(|col| self.columns.iter().position(|c| c.col == col)) {
                Some(i) if self.columns[i].filter.is_none() => self.columns[i].set_filter(Some(part)),
                _ => rest.push(part),
            }
        }
        self.advanced_filter = expr::and_all(rest);
        self.filter_enabled = true;
        self.refresh();
    }

    pub fn search_columns(&self) -> Vec<(usize, &str)> {
        self.visible_columns()
            .filter_map(|c| match c.col {
                Col::Data(i) => Some((i, c.name.as_str())),
                _ => None,
            })
            .collect()
    }

    pub fn recompile_search(&mut self, opts: &SearchOptions) {
        let cols = self.search_columns();
        let compiled = Query::compile(&self.search_text, opts.criteria, opts.condition, &cols);
        match compiled {
            Ok(q) => {
                self.search = q.map(Arc::new);
                self.search_error = None;
            }
            Err(e) => {
                self.search = None;
                self.search_error = Some(e);
            }
        }
    }

    pub fn recompile_formats(&mut self) {
        let table = self.table.clone();
        let resolver = |n: &str| self.resolve(n);
        self.compiled_formats = CompiledRules::compile(&self.formats, &table, &resolver);
    }

    /// Schedules a recompute after a short pause (typing in filter/search boxes).
    pub fn refresh_debounced(&mut self) {
        self.pending_since = Some(Instant::now());
    }

    /// Schedules an immediate recompute.
    pub fn refresh(&mut self) {
        self.pending_since = Some(Instant::now() - DEBOUNCE);
    }

    /// Call every frame: starts due jobs and collects finished ones.
    pub fn poll(&mut self, ctx: &egui::Context, opts: &SearchOptions) {
        if let Some(t) = self.pending_since {
            let due = t + DEBOUNCE;
            let now = Instant::now();
            if now >= due {
                self.pending_since = None;
                self.start_job(ctx, opts);
            } else {
                ctx.request_repaint_after(due - now);
            }
        }
        if let Some(rx) = &self.job
            && let Ok(res) = rx.try_recv()
        {
            self.job = None;
            self.computing = false;
            self.rows = Arc::new(res.rows);
            self.search_hits = res.search_hits;
            self.shown_group_by = res.group_by;
            self.rebuild_items();
        }
    }

    fn start_job(&mut self, ctx: &egui::Context, opts: &SearchOptions) {
        self.recompile_search(opts);
        let resolver = |n: &str| self.resolve(n);
        let filter = if self.filter_enabled { self.combined_filter() } else { None };
        let compiled: Option<Compiled> = match filter.map(|f| f.compile(&resolver)).transpose() {
            Ok(c) => {
                self.filter_error = None;
                c
            }
            Err(e) => {
                self.filter_error = Some(e);
                None
            }
        };
        self.filter_uses_tag = compiled.as_ref().is_some_and(|c| c.uses(Col::Tag));
        let generation = self.generation.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let gen_ref = self.generation.clone();
        let table = self.table.clone();
        let tags = self.tags.clone();
        let search = self.search.clone();
        let search_filters = opts.behavior == Behavior::Filter;
        let order: Vec<(Col, bool)> = self.group_by.iter().chain(&self.sort).copied().collect();
        let group_by = self.group_by.clone();
        let key_cache = self.key_cache.clone();
        let (tx, rx) = channel();
        self.job = Some(rx);
        self.computing = true;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let stale = || gen_ref.load(AtomicOrdering::Relaxed) != generation;
            let n = table.row_count();
            let row_matches_search = |r: usize| search.as_ref().is_none_or(|q| q.matches(&|c| table.cell(r, c)));
            let mut rows: Vec<u32> = (0..n as u32)
                .into_par_iter()
                .filter(|&r| {
                    if stale() {
                        return false;
                    }
                    let r = r as usize;
                    compiled.as_ref().is_none_or(|f| f.eval(&|c| cell_text(&table, &tags, r, c)))
                        && (!search_filters || row_matches_search(r))
                })
                .collect();
            if stale() {
                return;
            }
            let search_hits = match &search {
                Some(_) if search_filters => rows.len(),
                Some(_) => rows.par_iter().filter(|&&r| row_matches_search(r as usize)).count(),
                None => 0,
            };
            if !order.is_empty() {
                let specs = sort_specs(&table, &order, &key_cache);
                let cmp = Comparer { table: &table, tags: &tags, specs: &specs };
                rows.par_sort_unstable_by(|&a, &b| cmp.cmp(a, b));
            }
            if stale() {
                return;
            }
            let _ = tx.send(JobResult { rows, search_hits, group_by });
            ctx.request_repaint();
        });
    }

    // ----- selection / tags ----------------------------------------------------------

    pub fn clear_selection(&mut self) {
        self.selected.iter_mut().for_each(|s| *s = false);
    }

    /// Selects display items in `a..=b` (rows only).
    pub fn select_range(&mut self, a: usize, b: usize, value: bool) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        for i in lo..=hi.min(self.item_count().saturating_sub(1)) {
            if let Some(r) = self.item_row(i) {
                self.selected[r] = value;
            }
        }
    }

    pub fn select_all(&mut self) {
        let n = self.item_count();
        if n > 0 {
            self.select_range(0, n - 1, true);
        }
    }

    /// Rows that are selected, in display order; falls back to the focused row.
    pub fn selected_rows(&self) -> Vec<usize> {
        let mut out: Vec<usize> =
            (0..self.item_count()).filter_map(|i| self.item_row(i)).filter(|&r| self.selected[r]).collect();
        if out.is_empty()
            && let Some(r) = self.item_row(self.focus)
        {
            out.push(r);
        }
        out
    }

    pub fn tagged_count(&self) -> usize {
        self.tags.iter().filter(|&&t| t).count()
    }

    /// 1-based line numbers of tagged rows, ascending (the `Line` column values).
    pub fn tagged_lines(&self) -> Vec<usize> {
        self.tags.iter().enumerate().filter(|(_, t)| **t).map(|(i, _)| i + 1).collect()
    }

    /// Tags rows by 1-based line number; numbers beyond the file are ignored.
    pub fn tag_lines(&mut self, lines: &[usize]) {
        for &line in lines {
            if let Some(t) = self.tags.get_mut(line.wrapping_sub(1)) {
                *t = true;
            }
        }
    }

    fn tags_affect_view(&self) -> bool {
        self.filter_uses_tag || self.sort.iter().chain(&self.group_by).any(|(c, _)| *c == Col::Tag)
    }

    pub fn set_tags(&mut self, rows: &[usize], value: bool) {
        for &r in rows {
            self.tags[r] = value;
        }
        if self.tags_affect_view() {
            self.refresh();
        }
    }

    /// Toggles tags on the selection: if any selected row is untagged, tag all; else untag.
    pub fn toggle_tags(&mut self, rows: &[usize]) {
        let value = rows.iter().any(|&r| !self.tags[r]);
        self.set_tags(rows, value);
    }

    // ----- navigation ----------------------------------------------------------------

    pub fn set_focus(&mut self, item: usize) {
        let n = self.item_count();
        if n == 0 {
            return;
        }
        self.focus = item.min(n - 1);
        self.scroll_to_focus = true;
    }

    /// Finds the next display item (wrapping) after/before the focus satisfying `pred`.
    pub fn find_item(&self, forward: bool, pred: impl Fn(usize) -> bool) -> Option<usize> {
        let n = self.item_count();
        (1..=n)
            .map(|k| if forward { (self.focus + k) % n } else { (self.focus + n - k % n) % n })
            .find(|&i| self.item_row(i).is_some_and(&pred))
    }

    /// Makes display item `i` the focused, sole selection and scrolls it into view.
    pub fn select_item(&mut self, i: usize) {
        self.set_focus(i);
        self.anchor = self.focus;
        self.clear_selection();
        self.select_range(self.focus, self.focus, true);
    }

    pub fn goto_line(&mut self, line: usize) -> bool {
        let target = line.wrapping_sub(1);
        let found = (0..self.item_count()).find(|&i| self.item_row(i) == Some(target));
        if let Some(i) = found {
            self.select_item(i);
        }
        found.is_some()
    }
}

/// If `e` only references one column, returns its name.
fn single_column(e: &Expr) -> Option<&str> {
    match e {
        Expr::Cond { column, .. } => Some(column),
        Expr::Not(inner) => single_column(inner),
        Expr::And(v) | Expr::Or(v) => {
            let first = single_column(v.first()?)?;
            v.iter().all(|x| single_column(x).is_some_and(|n| n.eq_ignore_ascii_case(first))).then_some(first)
        }
    }
}
