//! Conditional formatting ("Highlight Cell Rules", "Unique/Duplicate Rules").

use std::collections::HashMap;

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::data::Table;
use crate::expr::{self, Compiled};
use crate::value::Col;
use crate::view::cell_text;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preset {
    LightRedFillDarkRedText,
    YellowFillDarkYellowText,
    GreenFillDarkGreenText,
    LightRedFill,
    RedFill,
    YellowFill,
    GreenFill,
    RedText,
    RedBoldText,
    GreenText,
    BoldText,
    ItalicText,
    StrikeoutText,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Style {
    pub fill: Option<Color32>,
    pub text: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
}

impl Style {
    /// Fills unset attributes of `self` from `other` (earlier rules take precedence).
    fn merge(&mut self, other: &Style) {
        self.fill = self.fill.or(other.fill);
        self.text = self.text.or(other.text);
        self.bold |= other.bold;
        self.italic |= other.italic;
        self.strike |= other.strike;
    }
}

impl Preset {
    pub const ALL: [Preset; 13] = [
        Preset::LightRedFillDarkRedText,
        Preset::YellowFillDarkYellowText,
        Preset::GreenFillDarkGreenText,
        Preset::LightRedFill,
        Preset::RedFill,
        Preset::YellowFill,
        Preset::GreenFill,
        Preset::RedText,
        Preset::RedBoldText,
        Preset::GreenText,
        Preset::BoldText,
        Preset::ItalicText,
        Preset::StrikeoutText,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::LightRedFillDarkRedText => "Light Red Fill with Dark Red Text",
            Preset::YellowFillDarkYellowText => "Yellow Fill with Dark Yellow Text",
            Preset::GreenFillDarkGreenText => "Green Fill with Dark Green Text",
            Preset::LightRedFill => "Light Red Fill",
            Preset::RedFill => "Red Fill",
            Preset::YellowFill => "Yellow Fill",
            Preset::GreenFill => "Green Fill",
            Preset::RedText => "Red Text",
            Preset::RedBoldText => "Red Bold Text",
            Preset::GreenText => "Green Text",
            Preset::BoldText => "Bold Text",
            Preset::ItalicText => "Italic Text",
            Preset::StrikeoutText => "Strikeout Text",
        }
    }

    pub fn style(self) -> Style {
        let rgb = Color32::from_rgb;
        let s = Style::default();
        match self {
            Preset::LightRedFillDarkRedText => {
                Style { fill: Some(rgb(255, 199, 206)), text: Some(rgb(156, 0, 6)), ..s }
            }
            Preset::YellowFillDarkYellowText => {
                Style { fill: Some(rgb(255, 235, 156)), text: Some(rgb(156, 101, 0)), ..s }
            }
            Preset::GreenFillDarkGreenText => Style { fill: Some(rgb(198, 239, 206)), text: Some(rgb(0, 97, 0)), ..s },
            Preset::LightRedFill => Style { fill: Some(rgb(255, 199, 206)), text: Some(Color32::BLACK), ..s },
            Preset::RedFill => Style { fill: Some(rgb(230, 50, 50)), text: Some(Color32::WHITE), ..s },
            Preset::YellowFill => Style { fill: Some(rgb(255, 230, 80)), text: Some(Color32::BLACK), ..s },
            Preset::GreenFill => Style { fill: Some(rgb(120, 200, 120)), text: Some(Color32::BLACK), ..s },
            Preset::RedText => Style { text: Some(rgb(220, 30, 30)), ..s },
            Preset::RedBoldText => Style { text: Some(rgb(220, 30, 30)), bold: true, ..s },
            Preset::GreenText => Style { text: Some(rgb(20, 160, 40)), ..s },
            Preset::BoldText => Style { bold: true, ..s },
            Preset::ItalicText => Style { italic: true, ..s },
            Preset::StrikeoutText => Style { strike: true, ..s },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RuleKind {
    /// A filter-language criteria (e.g. `[Level] = 'crit'`).
    Criteria(String),
    Unique,
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormatRule {
    /// Column the rule was created on; its cells are formatted unless `whole_row`.
    pub column: String,
    pub kind: RuleKind,
    pub preset: Preset,
    pub whole_row: bool,
    pub enabled: bool,
}

impl FormatRule {
    pub fn describe(&self) -> String {
        match &self.kind {
            RuleKind::Criteria(c) => c.clone(),
            RuleKind::Unique => format!("Unique values of [{}]", self.column),
            RuleKind::Duplicate => format!("Duplicate values of [{}]", self.column),
        }
    }
}

enum Test {
    Expr(Compiled),
    Count { counts: HashMap<String, u32>, unique: bool },
}

struct CompiledRule {
    col: Col,
    test: Test,
    style: Style,
    whole_row: bool,
}

#[derive(Default)]
pub struct CompiledRules {
    rules: Vec<CompiledRule>,
    pub errors: Vec<String>,
}

/// Rules matching one row, in rule order: `None` targets every cell (whole-row rule).
#[derive(Default)]
pub struct RowStyle {
    hits: Vec<(Option<Col>, Style)>,
}

impl RowStyle {
    /// Style of one cell: matching rules merged in rule order, so earlier rules win per attribute.
    pub fn cell(&self, col: Col) -> Style {
        let mut s = Style::default();
        for (_, style) in self.hits.iter().filter(|(c, _)| c.is_none_or(|c| c == col)) {
            s.merge(style);
        }
        s
    }
}

impl CompiledRules {
    pub fn compile(rules: &[FormatRule], table: &Table, resolve: &dyn Fn(&str) -> Option<Col>) -> CompiledRules {
        let mut out = CompiledRules::default();
        for rule in rules.iter().filter(|r| r.enabled) {
            let Some(col) = resolve(&rule.column) else {
                out.errors.push(format!("unknown column [{}]", rule.column));
                continue;
            };
            let test = match &rule.kind {
                RuleKind::Criteria(text) => match expr::parse(text).and_then(|e| e.compile(resolve)) {
                    Ok(c) => Test::Expr(c),
                    Err(e) => {
                        out.errors.push(e);
                        continue;
                    }
                },
                RuleKind::Unique | RuleKind::Duplicate => {
                    let Col::Data(c) = col else {
                        out.errors.push(format!("unique/duplicate rules need a data column, not [{}]", rule.column));
                        continue;
                    };
                    let mut counts: HashMap<String, u32> = HashMap::new();
                    for r in 0..table.row_count() {
                        let v = table.cell(r, c);
                        match counts.get_mut(v) {
                            Some(n) => *n += 1,
                            None => {
                                counts.insert(v.to_string(), 1);
                            }
                        }
                    }
                    Test::Count { counts, unique: rule.kind == RuleKind::Unique }
                }
            };
            out.rules.push(CompiledRule { col, test, style: rule.preset.style(), whole_row: rule.whole_row });
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn row_style(&self, table: &Table, tags: &[bool], row: usize) -> RowStyle {
        let mut out = RowStyle::default();
        for rule in &self.rules {
            let hit = match &rule.test {
                Test::Expr(c) => c.eval(&|col| cell_text(table, tags, row, col)),
                Test::Count { counts, unique } => {
                    let n = counts.get(cell_text(table, tags, row, rule.col).as_ref()).copied().unwrap_or(0);
                    (n == 1) == *unique
                }
            };
            if hit {
                out.hits.push(((!rule.whole_row).then_some(rule.col), rule.style));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn rule(column: &str, preset: Preset, whole_row: bool) -> FormatRule {
        FormatRule {
            column: column.into(),
            kind: RuleKind::Criteria("[a] = 'x'".into()),
            preset,
            whole_row,
            enabled: true,
        }
    }

    #[test]
    fn rules_apply_in_order() {
        let table =
            Table::from_rows(PathBuf::from("mem"), vec!["a".into(), "b".into()], vec![vec!["x".into(), "y".into()]]);
        let resolve = |n: &str| table.headers.iter().position(|h| h == n).map(Col::Data);
        let fill = |rules: &[FormatRule], col: usize| {
            CompiledRules::compile(rules, &table, &resolve).row_style(&table, &[false], 0).cell(Col::Data(col)).fill
        };
        let row = rule("a", Preset::YellowFill, true);
        let cell = rule("b", Preset::RedFill, false);
        let (yellow, red) = (Preset::YellowFill.style().fill, Preset::RedFill.style().fill);
        assert_eq!(fill(&[row.clone(), cell.clone()], 1), yellow);
        assert_eq!(fill(&[cell.clone(), row.clone()], 1), red);
        assert_eq!(fill(&[cell, row], 0), yellow);
    }
}
