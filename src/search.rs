//! Find-panel search: the search box above the grid, with DevExpress-style
//! match criteria (`Mixed` syntax supports `+required`, `-excluded`,
//! `"quoted phrases"` and `Column:value` scoping).

use std::ops::Range;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Whether non-matching rows are hidden or merely matches are highlighted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Behavior {
    #[default]
    Filter,
    Search,
}

/// How the search text is split into terms and how the terms combine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MatchCriteria {
    #[default]
    Mixed,
    Or,
    And,
    Exact,
}

/// How a single term is matched against a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Condition {
    #[default]
    Contains,
    StartsWith,
    Like,
    Equals,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchOptions {
    pub behavior: Behavior,
    pub criteria: MatchCriteria,
    pub condition: Condition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Required,
    Excluded,
    Optional,
}

struct Term {
    re: Regex,
    /// Data column the term is restricted to; `None` searches every searchable column.
    scope: Option<usize>,
    role: Role,
}

/// A compiled search.
pub struct Query {
    terms: Vec<Term>,
    columns: Vec<usize>,
    has_optional: bool,
}

/// A whitespace-delimited token with quotes removed.
struct RawToken {
    prefix: Option<char>,
    text: String,
    /// Byte offset in `text` of the first colon outside quotes, when a non-empty
    /// unquoted name precedes it (candidate `Column:value`).
    colon_at: Option<usize>,
}

/// Splits on whitespace outside double quotes; quotes are removed and may span spaces.
/// With `mixed`, a leading `+`/`-` is taken as a prefix and `Name:` positions are recorded.
/// An unmatched quote runs to the end of the text.
fn tokenize(text: &str, mixed: bool) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let prefix = if mixed { chars.next_if(|c| matches!(c, '+' | '-')) } else { None };
        let mut buf = String::new();
        let mut in_quote = false;
        let mut quote_seen = false;
        let mut colon_at = None;
        while let Some(c) = chars.next_if(|&c| in_quote || !c.is_whitespace()) {
            match c {
                '"' => {
                    in_quote = !in_quote;
                    quote_seen = true;
                }
                ':' if mixed && !quote_seen && colon_at.is_none() && !buf.is_empty() => {
                    colon_at = Some(buf.len());
                    buf.push(c);
                }
                _ => buf.push(c),
            }
        }
        tokens.push(RawToken { prefix, text: buf, colon_at });
    }
    tokens
}

/// Exact case-insensitive header match first, otherwise the first header starting with `name`.
fn find_column(columns: &[(usize, &str)], name: &str) -> Option<usize> {
    let name = name.to_lowercase();
    let lowered: Vec<(usize, String)> = columns.iter().map(|&(i, h)| (i, h.to_lowercase())).collect();
    lowered
        .iter()
        .find(|(_, h)| *h == name)
        .or_else(|| lowered.iter().find(|(_, h)| h.starts_with(&name)))
        .map(|&(i, _)| i)
}

fn term_regex(term: &str, condition: Condition) -> Result<Regex, String> {
    let pattern = match condition {
        Condition::Contains => format!("(?i){}", regex::escape(term)),
        Condition::StartsWith => format!(r"(?i)\A{}", regex::escape(term)),
        Condition::Equals => format!(r"(?i)\A{}\z", regex::escape(term)),
        Condition::Like => crate::expr::like_pattern(term, |c| match c {
            '%' | '*' => Some(".*"),
            '_' | '?' => Some("."),
            _ => None,
        }),
    };
    Regex::new(&pattern).map_err(|e| format!("cannot search for '{term}': {e}"))
}

impl Query {
    /// `columns`: searchable data columns as (data column index, header name). Ok(None) for blank text.
    pub fn compile(
        text: &str,
        criteria: MatchCriteria,
        condition: Condition,
        columns: &[(usize, &str)],
    ) -> Result<Option<Query>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let mut terms = Vec::new();
        let mut push = |needle: &str, condition, scope, role| -> Result<(), String> {
            if !needle.is_empty() {
                terms.push(Term { re: term_regex(needle, condition)?, scope, role });
            }
            Ok(())
        };
        match criteria {
            MatchCriteria::Exact => push(text, condition, None, Role::Optional)?,
            MatchCriteria::Or | MatchCriteria::And => {
                let role = if criteria == MatchCriteria::Or { Role::Optional } else { Role::Required };
                for tok in tokenize(text, false) {
                    push(&tok.text, condition, None, role)?;
                }
            }
            MatchCriteria::Mixed => {
                for tok in tokenize(text, true) {
                    let scoped = tok.colon_at.and_then(|at| find_column(columns, &tok.text[..at]).map(|col| (col, at)));
                    let (scope, needle) = match scoped {
                        Some((col, at)) => (Some(col), &tok.text[at + 1..]),
                        None => (None, tok.text.as_str()),
                    };
                    let role = match tok.prefix {
                        Some('+') => Role::Required,
                        Some(_) => Role::Excluded,
                        None if scope.is_some() => Role::Required,
                        None => Role::Optional,
                    };
                    push(needle, Condition::Contains, scope, role)?;
                }
            }
        }
        if terms.is_empty() {
            return Ok(None);
        }
        let has_optional = terms.iter().any(|t| t.role == Role::Optional);
        Ok(Some(Query { terms, columns: columns.iter().map(|&(i, _)| i).collect(), has_optional }))
    }

    fn term_hits<'a, F: Fn(usize) -> &'a str>(&self, term: &Term, cell: &F) -> bool {
        match term.scope {
            Some(col) => term.re.is_match(cell(col)),
            None => self.columns.iter().any(|&col| term.re.is_match(cell(col))),
        }
    }

    /// `cell(data_col)` returns that column's text for the row being tested.
    pub fn matches<'a, F: Fn(usize) -> &'a str>(&self, cell: &F) -> bool {
        let mut optional_hit = false;
        for term in &self.terms {
            match term.role {
                Role::Required if !self.term_hits(term, cell) => return false,
                Role::Excluded if self.term_hits(term, cell) => return false,
                Role::Optional if !optional_hit => optional_hit = self.term_hits(term, cell),
                _ => {}
            }
        }
        optional_hit || !self.has_optional
    }

    /// Byte ranges in `text` (cell of data column `col`) that should be highlighted; sorted, merged,
    /// non-overlapping. Excluded (-) terms never highlight. `out` is cleared first.
    pub fn highlight(&self, col: usize, text: &str, out: &mut Vec<Range<usize>>) {
        out.clear();
        let searchable = self.columns.contains(&col);
        for term in &self.terms {
            let applies = match term.scope {
                Some(c) => c == col,
                None => searchable,
            };
            if term.role != Role::Excluded && applies {
                out.extend(term.re.find_iter(text).filter(|m| !m.is_empty()).map(|m| m.range()));
            }
        }
        out.sort_unstable_by_key(|r| r.start);
        let mut merged = 0;
        for i in 0..out.len() {
            let r = out[i].clone();
            if merged > 0 && r.start <= out[merged - 1].end {
                let last = &mut out[merged - 1];
                last.end = last.end.max(r.end);
            } else {
                out[merged] = r;
                merged += 1;
            }
        }
        out.truncate(merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMNS: &[(usize, &str)] = &[(0, "EventID"), (1, "From"), (2, "Subject"), (3, "Payload")];

    fn query(text: &str, criteria: MatchCriteria, condition: Condition) -> Query {
        Query::compile(text, criteria, condition, COLUMNS).unwrap().unwrap()
    }

    fn mixed(text: &str) -> Query {
        query(text, MatchCriteria::Mixed, Condition::Contains)
    }

    fn hit(q: &Query, row: [&str; 4]) -> bool {
        q.matches(&|c| row[c])
    }

    #[test]
    fn blank_text_is_none() {
        for t in ["", "   ", "\t\n"] {
            assert!(Query::compile(t, MatchCriteria::Mixed, Condition::Contains, COLUMNS).unwrap().is_none());
        }
        for t in ["+", "-", "\"\"", "+ -", "\"", "EventID:"] {
            assert!(Query::compile(t, MatchCriteria::Mixed, Condition::Contains, COLUMNS).unwrap().is_none(), "{t}");
        }
    }

    #[test]
    fn mixed_single_term() {
        let q = mixed("mimikatz");
        assert!(hit(&q, ["", "", "", "C:\\MimiKatz.exe"]));
        assert!(!hit(&q, ["", "", "", "cmd.exe"]));
    }

    #[test]
    fn mixed_plain_terms_are_or() {
        let q = mixed("one two three");
        assert!(hit(&q, ["", "TWO", "", ""]));
        assert!(hit(&q, ["three", "", "", ""]));
        assert!(!hit(&q, ["four", "", "", ""]));
    }

    #[test]
    fn mixed_quoted_phrase() {
        let q = mixed("\"hoge hoge\"");
        assert!(hit(&q, ["", "", "x hoge hoge y", ""]));
        assert!(!hit(&q, ["", "", "hoge x hoge", ""]));
    }

    #[test]
    fn mixed_required_phrase() {
        let q = mixed("mimikatz +\"Bad Guy\"");
        assert!(hit(&q, ["", "bad guy", "", "mimikatz"]));
        assert!(!hit(&q, ["", "bad", "", "mimikatz"]));
        assert!(!hit(&q, ["", "bad guy", "", "procdump"]));
    }

    #[test]
    fn mixed_column_scoped() {
        let q = mixed("EventID:4624 kali");
        assert!(hit(&q, ["4624", "", "", "user kali logged in"]));
        assert!(!hit(&q, ["4625", "", "", "kali"]));
        assert!(!hit(&q, ["1", "", "", "4624 kali"]));
        assert!(!hit(&q, ["4624", "", "", "windows"]));
        // Prefix match on the header name, case-insensitively.
        let q = mixed("event:4624");
        assert!(hit(&q, ["4624", "", "", ""]));
        assert!(!hit(&q, ["", "", "", "4624"]));
    }

    #[test]
    fn mixed_required_and_excluded() {
        let q = mixed("data +entry -mark");
        assert!(hit(&q, ["", "data entry", "", ""]));
        assert!(!hit(&q, ["", "data", "", ""]));
        assert!(!hit(&q, ["", "data entry", "", "mark"]));
        assert!(!hit(&q, ["", "entry", "", ""]));
    }

    #[test]
    fn mixed_two_scoped_terms() {
        let q = mixed("From:Roller Subj:\"currency mask\"");
        assert!(hit(&q, ["", "Ken Roller", "New currency mask editor", ""]));
        assert!(!hit(&q, ["", "Ken Roller", "currency", ""]));
        assert!(!hit(&q, ["", "Steve", "currency mask", "Roller"]));
    }

    #[test]
    fn mixed_excluded_scoped() {
        let q = mixed("import -From:Steve");
        assert!(hit(&q, ["", "Ken", "Import data", ""]));
        assert!(!hit(&q, ["", "Steve Smith", "Import data", ""]));
        assert!(hit(&q, ["", "Ken", "Import", "steve"]));
        assert!(!hit(&q, ["", "Ken", "export", ""]));
    }

    #[test]
    fn mixed_unknown_column_is_literal() {
        let q = mixed("http://evil");
        assert!(hit(&q, ["", "", "", "GET http://evil.com"]));
        assert!(!hit(&q, ["", "", "", "evil"]));
        // Quoted colon is never a scope.
        let q = mixed("\"EventID:4624\"");
        assert!(hit(&q, ["", "", "", "EventID:4624"]));
        assert!(!hit(&q, ["4624", "", "", ""]));
    }

    #[test]
    fn mixed_exact_header_preferred() {
        let cols = [(0, "Subject Line"), (1, "Subj")];
        let q = Query::compile("subj:x", MatchCriteria::Mixed, Condition::Contains, &cols).unwrap().unwrap();
        assert!(q.matches(&|c| ["", "x"][c]));
        assert!(!q.matches(&|c| ["x", ""][c]));
    }

    #[test]
    fn mixed_ignores_condition_and_handles_garbage() {
        let q = query("ent", MatchCriteria::Mixed, Condition::Equals);
        assert!(hit(&q, ["", "", "", "data entry"]));
        let q = mixed("\"unterminated phrase");
        assert!(hit(&q, ["", "", "", "an unterminated phrase here"]));
        let q = mixed("+ - x");
        assert!(hit(&q, ["x", "", "", ""]));
        assert!(!hit(&q, ["y", "", "", ""]));
        let q = mixed("-x");
        assert!(hit(&q, ["y", "", "", ""]));
        assert!(!hit(&q, ["x", "", "", ""]));
        let q = mixed(":: (a) [b] \\");
        assert!(hit(&q, ["(a)", "", "", ""]));
    }

    #[test]
    fn or_and_exact_criteria() {
        let q = query("alpha \"beta gamma\"", MatchCriteria::Or, Condition::Contains);
        assert!(hit(&q, ["", "beta gamma", "", ""]));
        assert!(!hit(&q, ["", "beta", "", ""]));
        let q = query("alpha beta", MatchCriteria::And, Condition::Contains);
        assert!(hit(&q, ["alpha", "beta", "", ""]));
        assert!(!hit(&q, ["alpha", "", "", ""]));
        // Mixed syntax is literal outside Mixed mode.
        let q = query("-alpha", MatchCriteria::Or, Condition::Contains);
        assert!(hit(&q, ["x-alpha", "", "", ""]));
        let q = query("  \"a b\"  ", MatchCriteria::Exact, Condition::Contains);
        assert!(hit(&q, ["x\"a b\"y", "", "", ""]));
        assert!(!hit(&q, ["a b", "", "", ""]));
    }

    #[test]
    fn conditions() {
        let q = query("cmd", MatchCriteria::Exact, Condition::StartsWith);
        assert!(hit(&q, ["CMD.exe", "", "", ""]));
        assert!(!hit(&q, ["x cmd", "", "", ""]));
        let q = query("cmd.exe", MatchCriteria::Exact, Condition::Equals);
        assert!(hit(&q, ["CMD.EXE", "", "", ""]));
        assert!(!hit(&q, ["cmd.exe ", "", "", ""]));
        let q = query("c?d*.EXE", MatchCriteria::Exact, Condition::Like);
        assert!(hit(&q, ["cmd_x.exe", "", "", ""]));
        assert!(!hit(&q, ["xcmd.exe", "", "", ""]));
        let q = query("%sys_em32%", MatchCriteria::Exact, Condition::Like);
        assert!(hit(&q, ["C:\\Windows\\System32\\x", "", "", ""]));
        let q = query("ÄBC", MatchCriteria::Exact, Condition::Equals);
        assert!(hit(&q, ["äbc", "", "", ""]));
    }

    #[test]
    fn highlight_ranges() {
        let q = mixed("ab abc -ab_x EventID:46");
        let mut out = vec![7..9, 0..1];
        q.highlight(3, "xABCx ab ab_x", &mut out);
        assert_eq!(out, vec![1..4, 6..8, 9..11]);
        // Scoped term highlights only its column.
        q.highlight(0, "4646", &mut out);
        assert_eq!(out, vec![0..4]);
        // Unsearchable column gets nothing.
        q.highlight(9, "ab", &mut out);
        assert!(out.is_empty());
        // Excluded terms never highlight.
        let q = mixed("-mark data");
        q.highlight(1, "mark data", &mut out);
        assert_eq!(out, vec![5..9]);
        // Unicode text keeps char boundaries.
        let q = mixed("é");
        let text = "ÉtÉ";
        q.highlight(1, text, &mut out);
        assert_eq!(out, vec![0..2, 3..5]);
        assert!(out.iter().all(|r| text.is_char_boundary(r.start) && text.is_char_boundary(r.end)));
    }
}
