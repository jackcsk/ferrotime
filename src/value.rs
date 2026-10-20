//! Shared column identifiers and typed interpretation of cell text.

use std::cmp::Ordering;

use chrono::Datelike;
use serde::{Deserialize, Serialize};

/// A column of a grid: the two synthetic columns (`Line`, `Tag`, as in Timeline Explorer), or a data column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Col {
    Line,
    Tag,
    Data(usize),
}

/// Inferred type of a data column; drives sorting and comparison semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColKind {
    #[default]
    Text,
    Number,
    DateTime,
}

/// Parses a plain decimal number (optionally signed, with fraction/exponent).
/// Rejects `inf`/`nan` spellings and hexadecimal so identifiers are not misread as numbers.
pub fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    let b = t.as_bytes();
    let first = *b.first()?;
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.')) {
        return None;
    }
    if !b.iter().any(u8::is_ascii_digit) || b.iter().any(|c| c.is_ascii_alphabetic() && !matches!(c, b'e' | b'E')) {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn digits(&mut self, min: usize, max: usize) -> Option<(i64, usize)> {
        let start = self.i;
        let mut v: i64 = 0;
        while self.i < self.b.len() && self.i - start < max && self.b[self.i].is_ascii_digit() {
            v = v * 10 + i64::from(self.b[self.i] - b'0');
            self.i += 1;
        }
        let n = self.i - start;
        (n >= min).then_some((v, n))
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_any(&mut self, set: &[u8]) -> Option<u8> {
        let c = *self.b.get(self.i)?;
        set.contains(&c).then(|| {
            self.i += 1;
            c
        })
    }
    fn skip_spaces(&mut self) {
        while self.b.get(self.i) == Some(&b' ') {
            self.i += 1;
        }
    }
    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

const NANOS_PER_SEC: i64 = 1_000_000_000;

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    let date = chrono::NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, u32::try_from(m).ok()?, u32::try_from(d).ok()?)?;
    Some(i64::from(date.num_days_from_ce()) - 719_163)
}

/// Parses common timestamp layouts found in forensic CSV output and returns
/// nanoseconds since the Unix epoch (UTC when an offset is present, naive otherwise).
///
/// Accepted shapes:
/// - `YYYY-MM-DD`, `YYYY/MM/DD`, optionally followed by `T` or space and
///   `HH:MM[:SS[.fraction]]`, then optional `Z`, `UTC`, or `±HH[:MM]` (space allowed before it)
/// - `M/D/YYYY` optionally followed by `H:MM[:SS[.fraction]]` and `AM`/`PM`
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 8 || !b[0].is_ascii_digit() {
        return None;
    }
    let mut c = Cursor { b, i: 0 };
    let (a, alen) = c.digits(1, 4)?;
    let sep = c.eat_any(b"-/")?;
    let (y, m, d, us_style) = if alen == 4 {
        let (m, _) = c.digits(1, 2)?;
        if !c.eat(sep) {
            return None;
        }
        let (d, _) = c.digits(1, 2)?;
        (a, m, d, false)
    } else {
        if sep != b'/' {
            return None;
        }
        let (d, _) = c.digits(1, 2)?;
        if !c.eat(b'/') {
            return None;
        }
        let (y, _) = c.digits(4, 4)?;
        (y, a, d, true)
    };
    let days = days_from_civil(y, m, d)?;
    let mut nanos = days * 86_400 * NANOS_PER_SEC;
    if c.done() {
        return Some(nanos);
    }
    if !(c.eat(b'T') || c.eat(b' ')) {
        return None;
    }
    c.skip_spaces();
    let (mut hh, _) = c.digits(1, 2)?;
    if !c.eat(b':') {
        return None;
    }
    let (mm, _) = c.digits(2, 2)?;
    let mut ss = 0;
    let mut frac_nanos = 0;
    if c.eat(b':') {
        ss = c.digits(2, 2)?.0;
        if c.eat(b'.') || c.eat(b',') {
            let start = c.i;
            let mut v: i64 = 0;
            while c.i < b.len() && b[c.i].is_ascii_digit() {
                if c.i - start < 9 {
                    v = v * 10 + i64::from(b[c.i] - b'0');
                }
                c.i += 1;
            }
            let n = c.i - start;
            if n == 0 {
                return None;
            }
            frac_nanos = v * 10_i64.pow(9 - n.min(9) as u32);
        }
    }
    c.skip_spaces();
    if us_style {
        let rest = &s[c.i..];
        let upper = rest.get(..2).map(str::to_ascii_uppercase);
        match upper.as_deref() {
            Some("AM") | Some("PM") => {
                if !(1..=12).contains(&hh) {
                    return None;
                }
                let pm = upper.as_deref() == Some("PM");
                hh = (hh % 12) + if pm { 12 } else { 0 };
                c.i += 2;
                c.skip_spaces();
            }
            _ => {}
        }
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    nanos += ((hh * 60 + mm) * 60 + ss) * NANOS_PER_SEC + frac_nanos;
    if c.done() {
        return Some(nanos);
    }
    let rest = &s[c.i..];
    if rest.eq_ignore_ascii_case("Z") || rest.eq_ignore_ascii_case("UTC") {
        return Some(nanos);
    }
    let sign = match c.eat_any(b"+-")? {
        b'+' => 1,
        _ => -1,
    };
    let (oh, _) = c.digits(2, 2)?;
    c.eat(b':');
    let om = if c.done() { 0 } else { c.digits(2, 2)?.0 };
    if !c.done() || oh > 18 || om > 59 {
        return None;
    }
    Some(nanos - sign * (oh * 60 + om) * 60 * NANOS_PER_SEC)
}

/// Case-insensitive ordering (simple Unicode lowercase folding, ASCII fast path).
pub fn cmp_ci(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        let (x, y) = (a.as_bytes(), b.as_bytes());
        for (p, q) in x.iter().zip(y) {
            match p.to_ascii_lowercase().cmp(&q.to_ascii_lowercase()) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        return x.len().cmp(&y.len());
    }
    a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
}

/// Compares two cell texts with the semantics used by sorting and relational filters:
/// numerically if both are numbers, chronologically if both are timestamps,
/// otherwise case-insensitively as text.
pub fn cmp_values(a: &str, b: &str) -> Ordering {
    if let (Some(x), Some(y)) = (parse_number(a), parse_number(b)) {
        return x.total_cmp(&y);
    }
    if let (Some(x), Some(y)) = (parse_datetime(a), parse_datetime(b)) {
        return x.cmp(&y);
    }
    cmp_ci(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_number(" 42 "), Some(42.0));
        assert_eq!(parse_number("-1.5e3"), Some(-1500.0));
        assert_eq!(parse_number("inf"), None);
        assert_eq!(parse_number("0x10"), None);
        assert_eq!(parse_number("4688a"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("-"), None);
    }

    #[test]
    fn datetimes() {
        let base = parse_datetime("2022-08-05 06:12:39").unwrap();
        assert_eq!(parse_datetime("2022-08-05T06:12:39Z"), Some(base));
        assert_eq!(parse_datetime("2022-08-05 06:12:39.252 +00:00"), Some(base + 252_000_000));
        assert_eq!(parse_datetime("2022-08-05 15:12:39 +09:00"), Some(base));
        assert_eq!(parse_datetime("8/5/2022 6:12:39 AM"), Some(base));
        assert_eq!(parse_datetime("8/5/2022 6:12:39 PM"), Some(base + 12 * 3600 * NANOS_PER_SEC));
        assert_eq!(parse_datetime("2022-08-05 06:12:39.1234567"), Some(base + 123_456_700));
        assert_eq!(parse_datetime("1970-01-01"), Some(0));
        assert_eq!(parse_datetime("2022-13-05"), None);
        assert_eq!(parse_datetime("2022-08-05 garbage"), None);
        assert_eq!(parse_datetime("12345678"), None);
    }

    #[test]
    fn ordering() {
        assert_eq!(cmp_values("9", "10"), Ordering::Less);
        assert_eq!(cmp_values("abc", "ABD"), Ordering::Less);
        assert_eq!(cmp_values("8/5/2022", "2022-08-04"), Ordering::Greater);
        assert_eq!(cmp_ci("Äb", "äa"), Ordering::Greater);
    }
}
