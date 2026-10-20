//! Sessions (File ▸ Session): open files plus their layout, filters, sort, grouping,
//! tags and conditional formatting, stored as JSON.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::expr;
use crate::format::FormatRule;
use crate::value::Col;
use crate::view::{AutoOp, Tab};

pub const EXTENSION: &str = "ferrotime";
/// Format version written to session files; newer versions are rejected on load.
pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub active: usize,
    pub tabs: Vec<TabSession>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ColumnSession {
    pub name: String,
    pub visible: bool,
    pub width: f32,
    pub auto_op: AutoOp,
    /// Column filter in filter-language syntax.
    pub filter: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TabSession {
    pub path: PathBuf,
    pub columns: Vec<ColumnSession>,
    pub first_scrollable: Option<String>,
    pub sort: Vec<(String, bool)>,
    pub group_by: Vec<(String, bool)>,
    pub advanced_filter: Option<String>,
    pub filter_enabled: bool,
    pub search_text: String,
    /// 1-based line numbers of tagged rows.
    pub tags: Vec<usize>,
    pub formats: Vec<FormatRule>,
}

impl TabSession {
    pub fn capture(tab: &Tab) -> TabSession {
        let names = |v: &[(Col, bool)]| v.iter().map(|(c, d)| (tab.col_name(*c).to_string(), *d)).collect();
        TabSession {
            path: tab.table.path.clone(),
            columns: tab
                .columns
                .iter()
                .map(|c| ColumnSession {
                    name: c.name.clone(),
                    visible: c.visible,
                    width: c.width,
                    auto_op: c.auto_op,
                    filter: c.filter.as_ref().map(ToString::to_string),
                })
                .collect(),
            first_scrollable: tab.first_scrollable.map(|c| tab.col_name(c).to_string()),
            sort: names(&tab.sort),
            group_by: names(&tab.group_by),
            advanced_filter: tab.advanced_filter.as_ref().map(ToString::to_string),
            filter_enabled: tab.filter_enabled,
            search_text: tab.search_text.clone(),
            tags: tab.tagged_lines(),
            formats: tab.formats.clone(),
        }
    }

    /// Restores state onto a freshly loaded tab. Unknown columns and unparsable filters
    /// are skipped and reported.
    pub fn apply(&self, tab: &mut Tab) -> Vec<String> {
        let mut problems = Vec::new();
        let mut parse = |text: &str| match expr::parse(text) {
            Ok(e) => Some(e),
            Err(e) => {
                problems.push(format!("Filter '{text}': {e}"));
                None
            }
        };
        let mut ordered = Vec::with_capacity(tab.columns.len());
        for cs in &self.columns {
            let Some(col) = tab.resolve(&cs.name) else { continue };
            let Some(i) = tab.columns.iter().position(|c| c.col == col) else { continue };
            let mut c = tab.columns.remove(i);
            c.visible = cs.visible;
            c.width = cs.width;
            c.auto_op = cs.auto_op;
            c.set_filter(cs.filter.as_deref().and_then(&mut parse));
            ordered.push(c);
        }
        ordered.append(&mut tab.columns);
        tab.columns = ordered;
        tab.advanced_filter = self.advanced_filter.as_deref().and_then(&mut parse);
        let cols = |t: &Tab, v: &[(String, bool)]| {
            v.iter().filter_map(|(n, d)| t.resolve(n).map(|c| (c, *d))).collect::<Vec<_>>()
        };
        tab.sort = cols(tab, &self.sort);
        tab.group_by = cols(tab, &self.group_by);
        tab.first_scrollable = self.first_scrollable.as_deref().and_then(|n| tab.resolve(n));
        tab.filter_enabled = self.filter_enabled;
        tab.search_text = self.search_text.clone();
        tab.tag_lines(&self.tags);
        tab.formats = self.formats.clone();
        tab.recompile_formats();
        problems.extend(tab.compiled_formats.errors.iter().cloned());
        tab.refresh();
        problems
    }
}

pub fn save(path: &Path, session: &Session) -> Result<(), String> {
    let json = serde_json::to_string_pretty(session).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load(path: &Path) -> Result<Session, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let session: Session = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if session.version > SESSION_VERSION {
        return Err(format!(
            "{}: session format version {} is newer than this version of Ferrotime supports ({SESSION_VERSION})",
            path.display(),
            session.version
        ));
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Table;
    use crate::format::{Preset, RuleKind};

    fn tab(headers: &[&str]) -> Tab {
        let rows = (0..4).map(|i| headers.iter().map(|h| format!("{h}{i}")).collect()).collect();
        let table =
            Table::from_rows(PathBuf::from("/cases/x.csv"), headers.iter().map(|h| h.to_string()).collect(), rows);
        Tab::new(table, "x.csv".into())
    }

    fn snapshot(t: &Tab) -> String {
        let mut s = TabSession::capture(t);
        s.columns.iter_mut().for_each(|c| c.width = c.width.round());
        serde_json::to_string(&s).unwrap()
    }

    #[test]
    fn round_trip_restores_view_state() {
        let mut a = tab(&["Time", "Level", "Rule Title"]);
        let level = a.resolve("Level").unwrap();
        a.column_mut(level).unwrap().set_filter(Some(expr::parse("[Level] In ('Level1', 'Level2')").unwrap()));
        a.advanced_filter = Some(expr::parse("Not Contains([Rule Title], 'x') Or [Tag] = 'True'").unwrap());
        a.sort = vec![(a.resolve("Time").unwrap(), true)];
        a.group_by = vec![(level, false)];
        a.first_scrollable = a.resolve("Rule Title");
        a.columns.swap(2, 4);
        a.column_mut(Col::Line).unwrap().visible = false;
        a.column_mut(level).unwrap().width = 321.0;
        a.tags[1] = true;
        a.tags[3] = true;
        a.formats.push(FormatRule {
            column: "Level".into(),
            kind: RuleKind::Criteria("[Level] = 'Level1'".into()),
            preset: Preset::RedFill,
            whole_row: true,
            enabled: true,
        });
        a.search_text = "foo +bar".into();
        a.filter_enabled = false;

        let file = serde_json::to_string(&Session {
            version: SESSION_VERSION,
            active: 0,
            tabs: vec![TabSession::capture(&a)],
        })
        .unwrap();
        let restored: Session = serde_json::from_str(&file).unwrap();
        let mut b = tab(&["Time", "Level", "Rule Title"]);
        assert_eq!(restored.tabs[0].apply(&mut b), Vec::<String>::new());
        assert_eq!(snapshot(&a), snapshot(&b));
        assert_eq!(b.tags, vec![false, true, false, true]);
        assert_eq!(
            b.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["Line", "Tag", "Rule Title", "Level", "Time"]
        );
    }

    #[test]
    fn apply_tolerates_changed_file() {
        let mut a = tab(&["Time", "Level", "Gone"]);
        let gone = a.resolve("Gone").unwrap();
        a.column_mut(gone).unwrap().set_filter(Some(expr::parse("Contains([Gone], 'x')").unwrap()));
        a.sort = vec![(gone, false)];
        a.tags[0] = true;
        let mut s = TabSession::capture(&a);
        s.advanced_filter = Some("Contains([Level], ".into());
        s.tags.push(99);

        let mut b = tab(&["Time", "Level"]);
        let problems = s.apply(&mut b);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(b.sort.is_empty());
        assert!(b.columns.iter().all(|c| c.filter.is_none()));
        assert!(b.advanced_filter.is_none());
        assert_eq!(b.tags, vec![true, false, false, false]);
    }
}
