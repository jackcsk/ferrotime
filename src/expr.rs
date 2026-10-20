//! Filter criteria language modelled on the DevExpress criteria syntax used by the
//! filter editor and filter bar of Eric Zimmerman's Timeline Explorer, e.g.
//! `Not Contains([Rule Title], 'Proc Exec') And [Level] In ('high', 'crit')`.
//!
//! Text is parsed into an [`Expr`] tree, which prints back to canonical text via
//! `Display` and compiles against a concrete table into a [`Compiled`] predicate.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;
use std::ops::Range;

use regex::Regex;

use crate::value::{Col, cmp_ci, parse_datetime, parse_number};

/// A filter expression tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Cond { column: String, op: Op },
}

/// A condition applied to a single column's cell text.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Equals(String),
    NotEquals(String),
    Less(String),
    LessOrEqual(String),
    Greater(String),
    GreaterOrEqual(String),
    Between(String, String),
    Contains(String),
    StartsWith(String),
    EndsWith(String),
    Like(String),
    Regex(String),
    In(Vec<String>),
    IsEmpty,
}

/// Lexical category of a span, for syntax colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Function,
    Column,
    String,
    Number,
    Operator,
    Punct,
    Error,
}

/// Combines filters with `And`, flattening nested conjunctions.
/// Returns `None` for no filters and the filter itself when there is only one.
pub fn and_all(exprs: Vec<Expr>) -> Option<Expr> {
    let mut items = Vec::with_capacity(exprs.len());
    for e in exprs {
        match e {
            Expr::And(inner) => items.extend(inner),
            other => items.push(other),
        }
    }
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(Expr::And(items)),
    }
}

// ---------------------------------------------------------------------------
// Display

fn is_compound(e: &Expr) -> bool {
    matches!(e, Expr::And(v) | Expr::Or(v) if v.len() >= 2)
}

fn write_column(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    f.write_str("[")?;
    for c in name.chars() {
        match c {
            ']' => f.write_str("\\]")?,
            '\\' => f.write_str("\\\\")?,
            _ => fmt::Write::write_char(f, c)?,
        }
    }
    f.write_str("]")
}

fn write_value(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("'")?;
    let mut parts = value.split('\'');
    if let Some(first) = parts.next() {
        f.write_str(first)?;
    }
    for part in parts {
        f.write_str("''")?;
        f.write_str(part)?;
    }
    f.write_str("'")
}

fn write_joined(
    f: &mut fmt::Formatter<'_>,
    items: &[Expr],
    sep: &str,
    needs_parens: impl Fn(&Expr) -> bool,
) -> fmt::Result {
    for (i, e) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(sep)?;
        }
        if needs_parens(e) {
            write!(f, "({e})")?;
        } else {
            write!(f, "{e}")?;
        }
    }
    Ok(())
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Same-kind children are parenthesized so the tree shape survives a round trip.
            Expr::And(v) if v.len() >= 2 => write_joined(f, v, " And ", is_compound),
            Expr::Or(v) if v.len() >= 2 => write_joined(f, v, " Or ", |e| matches!(e, Expr::Or(x) if x.len() >= 2)),
            // Degenerate conjunctions/disjunctions use the function form `And(...)`.
            Expr::And(v) | Expr::Or(v) => {
                f.write_str(if matches!(self, Expr::And(_)) { "And(" } else { "Or(" })?;
                write_joined(f, v, ", ", |_| false)?;
                f.write_str(")")
            }
            Expr::Not(inner) if is_compound(inner) => write!(f, "Not ({inner})"),
            Expr::Not(inner) => write!(f, "Not {inner}"),
            Expr::Cond { column, op } => write_cond(f, column, op),
        }
    }
}

fn write_cond(f: &mut fmt::Formatter<'_>, column: &str, op: &Op) -> fmt::Result {
    let binary = |f: &mut fmt::Formatter<'_>, sym: &str, v: &str| {
        write_column(f, column)?;
        write!(f, " {sym} ")?;
        write_value(f, v)
    };
    let func = |f: &mut fmt::Formatter<'_>, name: &str, v: &str| {
        write!(f, "{name}(")?;
        write_column(f, column)?;
        f.write_str(", ")?;
        write_value(f, v)?;
        f.write_str(")")
    };
    match op {
        Op::Equals(v) => binary(f, "=", v),
        Op::NotEquals(v) => binary(f, "<>", v),
        Op::Less(v) => binary(f, "<", v),
        Op::LessOrEqual(v) => binary(f, "<=", v),
        Op::Greater(v) => binary(f, ">", v),
        Op::GreaterOrEqual(v) => binary(f, ">=", v),
        Op::Like(v) => binary(f, "Like", v),
        Op::Between(lo, hi) => {
            write_column(f, column)?;
            f.write_str(" Between(")?;
            write_value(f, lo)?;
            f.write_str(", ")?;
            write_value(f, hi)?;
            f.write_str(")")
        }
        Op::Contains(v) => func(f, "Contains", v),
        Op::StartsWith(v) => func(f, "StartsWith", v),
        Op::EndsWith(v) => func(f, "EndsWith", v),
        Op::Regex(v) => func(f, "Regex", v),
        Op::In(values) => {
            write_column(f, column)?;
            f.write_str(" In (")?;
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_value(f, v)?;
            }
            f.write_str(")")
        }
        Op::IsEmpty => {
            f.write_str("IsNullOrEmpty(")?;
            write_column(f, column)?;
            f.write_str(")")
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident,
    Column { name: String, closed: bool },
    Str { value: String, closed: bool },
    Number,
    Op(&'static str),
    LParen,
    RParen,
    Comma,
    Bad,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    span: Range<usize>,
}

const KEYWORDS: &[&str] = &["and", "or", "not", "between", "in", "like", "true", "false"];
const FUNCTIONS: &[&str] = &["contains", "startswith", "endswith", "regex", "isnullorempty"];

/// Length in bytes of a decimal number literal at the start of `b`, or 0.
fn number_len(b: &[u8]) -> usize {
    let mut j = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int_start = j;
    while b.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    let mut digits = j - int_start;
    if b.get(j) == Some(&b'.') {
        let frac = j + 1;
        let mut k = frac;
        while b.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
        if digits > 0 || k > frac {
            digits += k - frac;
            j = k;
        }
    }
    if digits == 0 {
        return 0;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        let exp = k;
        while b.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
        if k > exp {
            j = k;
        }
    }
    j
}

/// Tokenizes leniently: never fails, unterminated literals run to the end of input
/// and unknown characters become `Tok::Bad`.
fn lex(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = src[i..].chars().next() {
        let start = i;
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let tok = match c {
            '[' => {
                i += 1;
                let mut name = String::new();
                let mut closed = false;
                while let Some(ch) = src[i..].chars().next() {
                    i += ch.len_utf8();
                    match ch {
                        ']' => {
                            closed = true;
                            break;
                        }
                        '\\' if matches!(b.get(i), Some(b']' | b'\\')) => {
                            name.push(b[i] as char);
                            i += 1;
                        }
                        _ => name.push(ch),
                    }
                }
                Tok::Column { name, closed }
            }
            '\'' => {
                i += 1;
                let mut value = String::new();
                let mut closed = false;
                loop {
                    match src[i..].find('\'') {
                        None => {
                            value.push_str(&src[i..]);
                            i = src.len();
                            break;
                        }
                        Some(p) => {
                            value.push_str(&src[i..i + p]);
                            i += p + 1;
                            if b.get(i) == Some(&b'\'') {
                                value.push('\'');
                                i += 1;
                            } else {
                                closed = true;
                                break;
                            }
                        }
                    }
                }
                Tok::Str { value, closed }
            }
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            _ if c.is_ascii_digit() || matches!(c, '+' | '-' | '.') => {
                let n = number_len(&b[i..]);
                if n > 0 {
                    i += n;
                    Tok::Number
                } else {
                    i += 1;
                    Tok::Bad
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                i += src[i..].find(|ch: char| !(ch.is_alphanumeric() || ch == '_')).unwrap_or(src.len() - i);
                Tok::Ident
            }
            _ => {
                let two = b.get(i..i + 2).unwrap_or(&[]);
                let op = match two {
                    b"==" => Some("=="),
                    b"<>" => Some("<>"),
                    b"!=" => Some("!="),
                    b"<=" => Some("<="),
                    b">=" => Some(">="),
                    b"&&" => Some("&&"),
                    b"||" => Some("||"),
                    _ => None,
                };
                if let Some(op) = op {
                    i += 2;
                    Tok::Op(op)
                } else {
                    i += c.len_utf8();
                    match c {
                        '=' => Tok::Op("="),
                        '<' => Tok::Op("<"),
                        '>' => Tok::Op(">"),
                        '!' => Tok::Op("!"),
                        _ => Tok::Bad,
                    }
                }
            }
        };
        out.push(Token { tok, span: start..i });
    }
    out
}

/// Lexical spans of `text` for syntax colouring; tolerant of partial or invalid input.
pub fn highlight(text: &str) -> Vec<(Range<usize>, TokenKind)> {
    lex(text)
        .into_iter()
        .map(|t| {
            let kind = match t.tok {
                Tok::Ident => {
                    let word = &text[t.span.clone()];
                    if KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k)) {
                        TokenKind::Keyword
                    } else if FUNCTIONS.iter().any(|k| word.eq_ignore_ascii_case(k)) {
                        TokenKind::Function
                    } else {
                        TokenKind::Error
                    }
                }
                Tok::Column { .. } => TokenKind::Column,
                Tok::Str { .. } => TokenKind::String,
                Tok::Number => TokenKind::Number,
                Tok::Op(_) => TokenKind::Operator,
                Tok::LParen | Tok::RParen | Tok::Comma => TokenKind::Punct,
                Tok::Bad => TokenKind::Error,
            };
            (t.span, kind)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Parser

/// Parses filter text into an expression. Errors name the offending position (1-based characters).
pub fn parse(text: &str) -> Result<Expr, String> {
    let mut p = Parser { src: text, toks: lex(text), pos: 0 };
    if p.toks.is_empty() {
        return Err("empty filter expression".to_owned());
    }
    let e = p.or_expr()?;
    if p.pos < p.toks.len() {
        return Err(p.error("expected 'And', 'Or' or end of expression"));
    }
    Ok(e)
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn position(&self, byte: usize) -> usize {
        self.src[..byte].chars().count() + 1
    }

    /// Error describing the current token (or end of input).
    fn error(&self, expected: &str) -> String {
        match self.peek() {
            None => format!("{expected}, but the expression ended"),
            Some(t) => {
                let at = self.position(t.span.start);
                match &t.tok {
                    Tok::Column { closed: false, .. } => {
                        format!("unterminated column name (missing ']') starting at position {at}")
                    }
                    Tok::Str { closed: false, .. } => {
                        format!("unterminated string (missing ') starting at position {at}")
                    }
                    _ => format!("{expected}, but found '{}' at position {at}", &self.src[t.span.clone()]),
                }
            }
        }
    }

    fn at_kw(&self, kw: &str) -> bool {
        self.peek().is_some_and(|t| t.tok == Tok::Ident && self.src[t.span.clone()].eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let hit = self.at_kw(kw);
        self.pos += usize::from(hit);
        hit
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        let hit = self.peek().is_some_and(|t| &t.tok == tok);
        self.pos += usize::from(hit);
        hit
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> Result<(), String> {
        if self.eat(tok) { Ok(()) } else { Err(self.error(&format!("expected {what}"))) }
    }

    fn or_expr(&mut self) -> Result<Expr, String> {
        let mut items = vec![self.and_expr()?];
        while self.eat_kw("or") || self.eat(&Tok::Op("||")) {
            items.push(self.and_expr()?);
        }
        Ok(if items.len() == 1 { items.pop().expect("one item") } else { Expr::Or(items) })
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut items = vec![self.not_expr()?];
        while self.eat_kw("and") || self.eat(&Tok::Op("&&")) {
            items.push(self.not_expr()?);
        }
        Ok(if items.len() == 1 { items.pop().expect("one item") } else { Expr::And(items) })
    }

    fn not_expr(&mut self) -> Result<Expr, String> {
        if self.eat_kw("not") || self.eat(&Tok::Op("!")) {
            Ok(Expr::Not(Box::new(self.not_expr()?)))
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        const EXPECTED: &str = "expected a condition";
        let Some(t) = self.peek() else {
            return Err(self.error(EXPECTED));
        };
        match &t.tok {
            Tok::LParen => {
                self.pos += 1;
                let e = self.or_expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::Column { closed: true, .. } => {
                let column = self.column()?;
                self.predicate(column)
            }
            Tok::Ident => {
                let name = self.src[t.span.clone()].to_ascii_lowercase();
                let next_is_paren = self.toks.get(self.pos + 1).is_some_and(|n| n.tok == Tok::LParen);
                match name.as_str() {
                    _ if FUNCTIONS.contains(&name.as_str()) => {
                        self.pos += 1;
                        self.expect(&Tok::LParen, "'('")?;
                        let column = self.column()?;
                        let op = if name == "isnullorempty" {
                            Op::IsEmpty
                        } else {
                            self.expect(&Tok::Comma, "','")?;
                            let v = self.value()?;
                            match name.as_str() {
                                "contains" => Op::Contains(v),
                                "startswith" => Op::StartsWith(v),
                                "endswith" => Op::EndsWith(v),
                                _ => Op::Regex(v),
                            }
                        };
                        self.expect(&Tok::RParen, "')'")?;
                        Ok(Expr::Cond { column, op })
                    }
                    "and" | "or" if next_is_paren => {
                        self.pos += 2;
                        let mut items = Vec::new();
                        if !self.eat(&Tok::RParen) {
                            loop {
                                items.push(self.or_expr()?);
                                if self.eat(&Tok::RParen) {
                                    break;
                                }
                                self.expect(&Tok::Comma, "',' or ')'")?;
                            }
                        }
                        Ok(if name == "and" { Expr::And(items) } else { Expr::Or(items) })
                    }
                    _ => Err(self.error(EXPECTED)),
                }
            }
            _ => Err(self.error(EXPECTED)),
        }
    }

    fn column(&mut self) -> Result<String, String> {
        match self.peek() {
            Some(Token { tok: Tok::Column { name, closed: true }, .. }) => {
                let name = name.clone();
                self.pos += 1;
                Ok(name)
            }
            _ => Err(self.error("expected a column reference like [Name]")),
        }
    }

    fn value(&mut self) -> Result<String, String> {
        let Some(t) = self.peek() else {
            return Err(self.error("expected a value"));
        };
        let v = match &t.tok {
            Tok::Str { value, closed: true } => value.clone(),
            Tok::Number => self.src[t.span.clone()].to_owned(),
            Tok::Ident if self.at_kw("true") => "True".to_owned(),
            Tok::Ident if self.at_kw("false") => "False".to_owned(),
            _ => return Err(self.error("expected a value")),
        };
        self.pos += 1;
        Ok(v)
    }

    fn value_list(&mut self) -> Result<Vec<String>, String> {
        self.expect(&Tok::LParen, "'('")?;
        let mut values = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(values);
        }
        loop {
            values.push(self.value()?);
            if self.eat(&Tok::RParen) {
                return Ok(values);
            }
            self.expect(&Tok::Comma, "',' or ')'")?;
        }
    }

    fn predicate(&mut self, column: String) -> Result<Expr, String> {
        if self.eat_kw("not") {
            let inner = self
                .keyword_predicate()?
                .ok_or_else(|| self.error("expected 'Like', 'In' or 'Between' after 'Not'"))?;
            return Ok(Expr::Not(Box::new(Expr::Cond { column, op: inner })));
        }
        if let Some(op) = self.keyword_predicate()? {
            return Ok(Expr::Cond { column, op });
        }
        let ctor: fn(String) -> Op = match self.peek().map(|t| &t.tok) {
            Some(Tok::Op("=" | "==")) => Op::Equals,
            Some(Tok::Op("<>" | "!=")) => Op::NotEquals,
            Some(Tok::Op("<")) => Op::Less,
            Some(Tok::Op("<=")) => Op::LessOrEqual,
            Some(Tok::Op(">")) => Op::Greater,
            Some(Tok::Op(">=")) => Op::GreaterOrEqual,
            _ => return Err(self.error("expected an operator after the column")),
        };
        self.pos += 1;
        Ok(Expr::Cond { column, op: ctor(self.value()?) })
    }

    /// `Like v`, `In (...)` or `Between(a, b)`; `None` if none of these keywords follows.
    fn keyword_predicate(&mut self) -> Result<Option<Op>, String> {
        if self.eat_kw("like") {
            Ok(Some(Op::Like(self.value()?)))
        } else if self.eat_kw("in") {
            Ok(Some(Op::In(self.value_list()?)))
        } else if self.eat_kw("between") {
            self.expect(&Tok::LParen, "'('")?;
            let lo = self.value()?;
            self.expect(&Tok::Comma, "','")?;
            let hi = self.value()?;
            self.expect(&Tok::RParen, "')'")?;
            Ok(Some(Op::Between(lo, hi)))
        } else {
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation and evaluation

/// An expression bound to concrete columns, ready for per-row evaluation.
pub struct Compiled {
    root: Node,
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Test { col: Col, test: Test },
}

enum Test {
    Cmp { lit: Literal, pred: Pred },
    Between(Literal, Literal),
    Pattern(Regex),
    In(HashSet<String>),
    IsEmpty,
}

#[derive(Clone, Copy)]
enum Pred {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A comparison operand with its typed interpretations precomputed.
struct Literal {
    text: String,
    num: Option<f64>,
    time: Option<i64>,
}

impl Literal {
    fn new(text: &str) -> Self {
        Literal { text: text.to_owned(), num: parse_number(text), time: parse_datetime(text) }
    }

    /// Same ordering as `value::cmp_values(cell, literal)`, parsing the cell only when needed.
    fn cmp_cell(&self, cell: &str) -> Ordering {
        if let Some(n) = self.num
            && let Some(c) = parse_number(cell)
        {
            return c.total_cmp(&n);
        }
        if let Some(t) = self.time
            && let Some(c) = parse_datetime(cell)
        {
            return c.cmp(&t);
        }
        cmp_ci(cell, &self.text)
    }
}

fn build_regex(pattern: &str, source: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|e| format!("invalid regular expression '{source}': {e}"))
}

/// Translates a `LIKE` pattern into an anchored, case-insensitive (dot-matches-newline) regex
/// source; `wildcard` maps a wildcard character to its regex, other characters are literal.
pub fn like_pattern(pattern: &str, wildcard: impl Fn(char) -> Option<&'static str>) -> String {
    let mut re = String::with_capacity(pattern.len() + 16);
    re.push_str(r"(?is)\A");
    let mut literal_start = 0;
    for (i, c) in pattern.char_indices() {
        let Some(w) = wildcard(c) else { continue };
        re.push_str(&regex::escape(&pattern[literal_start..i]));
        re.push_str(w);
        literal_start = i + c.len_utf8();
    }
    re.push_str(&regex::escape(&pattern[literal_start..]));
    re.push_str(r"\z");
    re
}

/// Translates a SQL `LIKE` pattern (`%`, `_`) into a regex.
fn like_regex(pattern: &str) -> Result<Regex, String> {
    let re = like_pattern(pattern, |c| match c {
        '%' => Some(".*"),
        '_' => Some("."),
        _ => None,
    });
    build_regex(&re, pattern)
}

impl Expr {
    /// Binds column names via `resolve` and precompiles patterns and literals.
    pub fn compile(&self, resolve: &dyn Fn(&str) -> Option<Col>) -> Result<Compiled, String> {
        Ok(Compiled { root: compile_node(self, resolve)? })
    }
}

fn compile_node(e: &Expr, resolve: &dyn Fn(&str) -> Option<Col>) -> Result<Node, String> {
    let all = |v: &[Expr]| v.iter().map(|x| compile_node(x, resolve)).collect::<Result<Vec<_>, _>>();
    Ok(match e {
        Expr::And(v) => Node::And(all(v)?),
        Expr::Or(v) => Node::Or(all(v)?),
        Expr::Not(x) => Node::Not(Box::new(compile_node(x, resolve)?)),
        Expr::Cond { column, op } => {
            let col = resolve(column).ok_or_else(|| format!("unknown column [{column}]"))?;
            let cmp = |v: &str, pred| Test::Cmp { lit: Literal::new(v), pred };
            let test = match op {
                Op::Equals(v) => cmp(v, Pred::Eq),
                Op::NotEquals(v) => cmp(v, Pred::Ne),
                Op::Less(v) => cmp(v, Pred::Lt),
                Op::LessOrEqual(v) => cmp(v, Pred::Le),
                Op::Greater(v) => cmp(v, Pred::Gt),
                Op::GreaterOrEqual(v) => cmp(v, Pred::Ge),
                Op::Between(lo, hi) => Test::Between(Literal::new(lo), Literal::new(hi)),
                Op::Contains(v) => Test::Pattern(build_regex(&format!("(?i){}", regex::escape(v)), v)?),
                Op::StartsWith(v) => Test::Pattern(build_regex(&format!(r"(?i)\A{}", regex::escape(v)), v)?),
                Op::EndsWith(v) => Test::Pattern(build_regex(&format!(r"(?i){}\z", regex::escape(v)), v)?),
                Op::Like(v) => Test::Pattern(like_regex(v)?),
                Op::Regex(v) => Test::Pattern(build_regex(&format!("(?i){v}"), v)?),
                Op::In(values) => Test::In(values.iter().cloned().collect()),
                Op::IsEmpty => Test::IsEmpty,
            };
            Node::Test { col, test }
        }
    })
}

impl Test {
    fn eval(&self, cell: &str) -> bool {
        match self {
            Test::Cmp { lit, pred } => {
                let o = lit.cmp_cell(cell);
                match pred {
                    Pred::Eq => o.is_eq(),
                    Pred::Ne => o.is_ne(),
                    Pred::Lt => o.is_lt(),
                    Pred::Le => o.is_le(),
                    Pred::Gt => o.is_gt(),
                    Pred::Ge => o.is_ge(),
                }
            }
            Test::Between(lo, hi) => lo.cmp_cell(cell).is_ge() && hi.cmp_cell(cell).is_le(),
            Test::Pattern(re) => re.is_match(cell),
            Test::In(set) => set.contains(cell),
            Test::IsEmpty => cell.trim().is_empty(),
        }
    }
}

impl Node {
    fn eval<'a, F: Fn(Col) -> Cow<'a, str>>(&self, cell: &F) -> bool {
        match self {
            Node::And(v) => v.iter().all(|n| n.eval(cell)),
            Node::Or(v) => v.iter().any(|n| n.eval(cell)),
            Node::Not(n) => !n.eval(cell),
            Node::Test { col, test } => test.eval(&cell(*col)),
        }
    }

    fn uses(&self, col: Col) -> bool {
        match self {
            Node::And(v) | Node::Or(v) => v.iter().any(|n| n.uses(col)),
            Node::Not(n) => n.uses(col),
            Node::Test { col: c, .. } => *c == col,
        }
    }
}

impl Compiled {
    /// Evaluates the filter for one row; `cell` yields the text of a column in that row.
    pub fn eval<'a, F: Fn(Col) -> Cow<'a, str>>(&self, cell: &F) -> bool {
        self.root.eval(cell)
    }

    /// Whether any condition references `col`.
    pub fn uses(&self, col: Col) -> bool {
        self.root.uses(col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(column: &str, op: Op) -> Expr {
        Expr::Cond { column: column.to_owned(), op }
    }

    fn s(v: &str) -> String {
        v.to_owned()
    }

    const HEADERS: &[&str] = &["EventID", "Rule Title", "Level", "Time", "Payload", "Empty"];

    fn resolve(name: &str) -> Option<Col> {
        match name {
            "Line" => Some(Col::Line),
            "Tag" => Some(Col::Tag),
            _ => HEADERS.iter().position(|h| h.eq_ignore_ascii_case(name)).map(Col::Data),
        }
    }

    fn eval_row(text: &str, row: &[&str]) -> bool {
        let compiled = parse(text).unwrap().compile(&resolve).unwrap();
        compiled.eval(&|c: Col| match c {
            Col::Line => Cow::Borrowed("7"),
            Col::Tag => Cow::Borrowed("False"),
            Col::Data(i) => Cow::Borrowed(row.get(i).copied().unwrap_or("")),
        })
    }

    #[test]
    fn parses_example() {
        let e = parse("Not Contains([Rule Title], 'Proc Exec') And [Level] In ('high', 'crit')").unwrap();
        assert_eq!(
            e,
            Expr::And(vec![
                Expr::Not(Box::new(cond("Rule Title", Op::Contains(s("Proc Exec"))))),
                cond("Level", Op::In(vec![s("high"), s("crit")])),
            ])
        );
        assert_eq!(e.to_string(), "Not Contains([Rule Title], 'Proc Exec') And [Level] In ('high', 'crit')");
    }

    #[test]
    fn case_insensitive_keywords_and_aliases() {
        let e = parse("not contains([A], 'x') AND [B] == 1 || [C] != true && ![D] like 'a%'").unwrap();
        assert_eq!(
            e,
            Expr::Or(vec![
                Expr::And(vec![Expr::Not(Box::new(cond("A", Op::Contains(s("x"))))), cond("B", Op::Equals(s("1"))),]),
                Expr::And(
                    vec![cond("C", Op::NotEquals(s("True"))), Expr::Not(Box::new(cond("D", Op::Like(s("a%"))))),]
                ),
            ])
        );
    }

    #[test]
    fn precedence() {
        let a = || cond("A", Op::Equals(s("1")));
        let b = || cond("B", Op::Equals(s("2")));
        let c = || cond("C", Op::Equals(s("3")));
        assert_eq!(parse("[A] = 1 Or [B] = 2 And [C] = 3").unwrap(), Expr::Or(vec![a(), Expr::And(vec![b(), c()])]));
        assert_eq!(parse("Not [A] = 1 And [B] = 2").unwrap(), Expr::And(vec![Expr::Not(Box::new(a())), b()]));
        assert_eq!(parse("([A] = 1 Or [B] = 2) And [C] = 3").unwrap(), Expr::And(vec![Expr::Or(vec![a(), b()]), c()]));
        assert_eq!(parse("Not ([A] = 1 Or [B] = 2)").unwrap().to_string(), "Not ([A] = '1' Or [B] = '2')");
    }

    #[test]
    fn all_ops_and_sugar() {
        let e = parse(
            "[A] = 'x' And [A] <> 'x' And [A] < -1.5 And [A] <= 2 And [A] > 3 And [A] >= 4 And \
             [A] Between(1, 10) And StartsWith([A], 'p') And EndsWith([A], 's') And Regex([A], '^a+$') \
             And IsNullOrEmpty([A]) And [A] Not In ('a') And [A] Not Between('a', 'b') And [A] Not Like 'z'",
        )
        .unwrap();
        let Expr::And(items) = &e else { panic!("expected And") };
        assert_eq!(items[2], cond("A", Op::Less(s("-1.5"))));
        assert_eq!(items[6], cond("A", Op::Between(s("1"), s("10"))));
        assert_eq!(items[10], cond("A", Op::IsEmpty));
        assert_eq!(items[11], Expr::Not(Box::new(cond("A", Op::In(vec![s("a")])))));
        assert_eq!(items[13], Expr::Not(Box::new(cond("A", Op::Like(s("z"))))));
        assert_eq!(parse(&e.to_string()).unwrap(), e);
    }

    #[test]
    fn escapes() {
        let e = parse(r"[a\]b\\c] = 'it''s'").unwrap();
        assert_eq!(e, cond(r"a]b\c", Op::Equals(s("it's"))));
        assert_eq!(e.to_string(), r"[a\]b\\c] = 'it''s'");
        // A backslash before any other character is literal.
        assert_eq!(parse(r"[C:\x] = ''").unwrap(), cond(r"C:\x", Op::Equals(s(""))));
    }

    #[test]
    fn round_trip() {
        let a = cond("A", Op::Equals(s("")));
        let cases = vec![
            a.clone(),
            cond("we]ird\\ [col]", Op::Contains(s("'quoted' ''"))),
            cond("", Op::In(vec![])),
            cond("x", Op::In(vec![s(""), s("a,b"), s("Ünïcode")])),
            cond("t", Op::Between(s("2024-01-01 00:00:00"), s(")"))),
            cond("n", Op::Regex(s(r"\d+\s'"))),
            cond("n", Op::IsEmpty),
            Expr::And(vec![]),
            Expr::Or(vec![]),
            Expr::And(vec![a.clone()]),
            Expr::Or(vec![Expr::Or(vec![a.clone()])]),
            Expr::And(vec![Expr::And(vec![a.clone(), a.clone()]), a.clone()]),
            Expr::Or(vec![a.clone(), Expr::Or(vec![a.clone(), a.clone()])]),
            Expr::And(vec![Expr::Or(vec![a.clone(), a.clone()]), Expr::Not(Box::new(a.clone()))]),
            Expr::Not(Box::new(Expr::Not(Box::new(Expr::And(vec![a.clone(), a.clone()]))))),
            Expr::Not(Box::new(Expr::Or(vec![]))),
            Expr::Or(vec![Expr::And(vec![a.clone(), a.clone()]), Expr::And(vec![])]),
        ];
        for e in cases {
            let text = e.to_string();
            assert_eq!(parse(&text), Ok(e), "round trip of {text}");
        }
    }

    #[test]
    fn and_all_flattens() {
        let a = cond("A", Op::IsEmpty);
        assert_eq!(and_all(vec![]), None);
        assert_eq!(and_all(vec![a.clone()]), Some(a.clone()));
        assert_eq!(
            and_all(vec![Expr::And(vec![a.clone(), a.clone()]), a.clone()]),
            Some(Expr::And(vec![a.clone(), a.clone(), a]))
        );
    }

    #[test]
    fn parse_errors() {
        for bad in [
            "",
            "   ",
            "[A]",
            "[A] =",
            "[A] = 'x",
            "[A = 'x'",
            "Contains([A] 'x')",
            "[A] = 1 And",
            "([A] = 1",
            "[A] = 1)",
            "Foo([A])",
            "[A] In ('a',)",
            "[A] Not = 1",
            "[A] = bare",
            "[A] = 1 & [B] = 2",
        ] {
            assert!(parse(bad).is_err(), "should fail: {bad:?}");
        }
        let err = parse("[A] = 1 Xor [B] = 2").unwrap_err();
        assert!(err.contains("position 9"), "{err}");
        assert!(parse("[A] = 'x").unwrap_err().contains("unterminated string"));
    }

    #[test]
    fn unknown_column_and_bad_regex() {
        let err = parse("[Nope] = 1").unwrap().compile(&resolve).err().unwrap();
        assert!(err.contains("[Nope]"), "{err}");
        assert!(parse("Regex([Payload], '(')").unwrap().compile(&resolve).is_err());
    }

    #[test]
    fn typed_comparisons() {
        assert!(eval_row("[EventID] > 999", &["4624"]));
        assert!(!eval_row("[EventID] < 999", &["4624"]));
        // Text vs number falls back to case-insensitive text: "abc" > "999".
        assert!(eval_row("[EventID] > 999", &["abc"]));
        assert!(eval_row("[EventID] = 4624.0", &["4624"]));
        assert!(eval_row("[Level] = 'HIGH'", &["", "", "high"]));
        assert!(!eval_row("[Level] <> 'HIGH'", &["", "", "high"]));
        assert!(eval_row("[Time] >= '2024-01-02'", &["", "", "", "2024-01-02 10:00:00"]));
        assert!(eval_row("[Time] < '2024-01-02 10:00:01'", &["", "", "", "2024-01-02 10:00:00"]));
        assert!(eval_row("[EventID] Between(4600, 4700)", &["4624"]));
        assert!(eval_row("[EventID] Between(4624, 4624)", &["4624"]));
        assert!(!eval_row("[EventID] Between(10, 999)", &["4624"]));
        assert!(eval_row("[Line] = 7 And [Tag] = false", &[]));
    }

    #[test]
    fn string_functions() {
        let row = ["", "Proc Exec ÄBC", "", "", "Mimikatz.EXE"];
        assert!(eval_row("Contains([Rule Title], 'proc exec')", &row));
        assert!(eval_row("Contains([Rule Title], 'äbc')", &row));
        assert!(eval_row("StartsWith([Rule Title], 'PROC')", &row));
        assert!(!eval_row("StartsWith([Rule Title], 'Exec')", &row));
        assert!(eval_row("EndsWith([Payload], '.exe')", &row));
        assert!(eval_row("Contains([Payload], '')", &row));
        assert!(eval_row("Contains([Payload], 'z.e')", &row));
        assert!(!eval_row("Contains([Payload], 'z?e')", &row));
    }

    #[test]
    fn like_and_regex() {
        let row = ["", "", "", "", "C:\\Windows\\System32\\cmd.exe"];
        assert!(eval_row(r"[Payload] Like 'c:\windows\%.EXE'", &row));
        assert!(eval_row(r"[Payload] Like '%cmd.ex_'", &row));
        assert!(!eval_row("[Payload] Like 'cmd%'", &row));
        assert!(!eval_row("[Payload] Like '%cmd.e_'", &row));
        assert!(eval_row(r"Regex([Payload], 'SYSTEM\d+')", &row));
        assert!(!eval_row(r"Regex([Payload], '(?-i)SYSTEM\d+')", &row));
        assert!(eval_row(r"Regex([Payload], 'cmd\.exe$')", &row));
    }

    #[test]
    fn in_is_exact_and_is_empty() {
        assert!(eval_row("[Level] In ('high', 'crit')", &["", "", "high"]));
        assert!(!eval_row("[Level] In ('high', 'crit')", &["", "", "High"]));
        assert!(!eval_row("[Level] In ('high')", &["", "", "high "]));
        assert!(eval_row("[Level] Not In ('high')", &["", "", "low"]));
        assert!(eval_row("IsNullOrEmpty([Empty])", &["", "", "", "", "", " \t"]));
        assert!(!eval_row("IsNullOrEmpty([Level])", &["", "", "x"]));
    }

    #[test]
    fn empty_and_or_semantics() {
        assert!(eval_row("And()", &[]));
        assert!(!eval_row("Or()", &[]));
        assert!(!eval_row("Not And()", &[]));
    }

    #[test]
    fn uses_reports_columns() {
        let c = parse("[Level] = 1 Or Not Contains([Payload], 'x')").unwrap().compile(&resolve).unwrap();
        assert!(c.uses(Col::Data(2)));
        assert!(c.uses(Col::Data(4)));
        assert!(!c.uses(Col::Data(0)));
        assert!(!c.uses(Col::Tag));
    }

    #[test]
    fn highlight_spans() {
        let text = "Not Contains([A], 'x') And [B] >= -1 Or Foo ( , ) & é";
        let spans = highlight(text);
        let kinds: Vec<TokenKind> = spans.iter().map(|(_, k)| *k).collect();
        use TokenKind::*;
        assert_eq!(
            kinds,
            vec![
                Keyword, Function, Punct, Column, Punct, String, Punct, Keyword, Column, Operator, Number, Keyword,
                Error, Punct, Punct, Punct, Error, Error
            ]
        );
        assert_eq!(&text[spans[3].0.clone()], "[A]");
        assert_eq!(&text[spans[10].0.clone()], "-1");
    }

    #[test]
    fn highlight_never_panics() {
        let garbage = [
            "",
            "[",
            "'",
            "[abc\\",
            "'it''",
            "\\",
            "é[ü'ß",
            "!=<>&&||&|",
            "-",
            "+.",
            "1e",
            "1.5e+",
            "[A] In ('a', ",
            "))))((((",
            "\u{1F600}'\u{1F600}",
            "[\\]",
            "''''",
            "Not Not Not",
        ];
        for g in garbage {
            for (r, _) in highlight(g) {
                assert!(g.is_char_boundary(r.start) && g.is_char_boundary(r.end) && r.start < r.end);
            }
            let _ = parse(g);
        }
        // Every prefix of a valid expression (on char boundaries) must be safe.
        let full = "Not Contains([Rülé], 'x''y') And [B] Between(1, 2) Or [C] In ('é')";
        for (i, _) in full.char_indices() {
            let _ = highlight(&full[..i]);
            let _ = parse(&full[..i]);
        }
    }
}
