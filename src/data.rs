//! In-memory table storage and file loaders (delimited text and spreadsheets).

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use calamine::{Data, Reader as _};
use encoding_rs::{CoderResult, Decoder, Encoding, UTF_16BE, UTF_16LE, WINDOWS_1252};
use rayon::prelude::*;

use crate::value::{self, ColKind};

/// Arena offsets marking where a row begins: its text in `Table::arena` and its
/// first cell end in `Table::ends`. A sentinel entry follows the last row.
#[derive(Clone, Copy)]
struct RowIndex {
    text: u64,
    cells: u64,
}

/// A read-only table of text cells stored compactly: all cell text lives in one
/// arena, and each cell costs four bytes of index (its end offset relative to the
/// start of its row). Rows may be ragged; missing trailing cells read as `""`.
pub struct Table {
    pub path: PathBuf,
    pub headers: Vec<String>,
    pub kinds: Vec<ColKind>,
    arena: String,
    rows: Vec<RowIndex>,
    ends: Vec<u32>,
}

impl Table {
    /// Builds a table from owned rows. Headers are normalized (trimmed, empty names
    /// replaced by `Column N`, duplicates suffixed) and extended to the widest row;
    /// column kinds are inferred from the data.
    #[cfg(test)]
    pub fn from_rows(path: PathBuf, headers: Vec<String>, rows: Vec<Vec<String>>) -> Table {
        let mut b = Builder::new();
        for row in &rows {
            for field in row {
                b.push_str(field);
            }
            // Rows of in-memory strings never approach the 4 GiB per-row limit.
            b.end_row(&path).expect("row exceeds 4 GiB");
        }
        b.finish(path, headers)
    }

    pub fn row_count(&self) -> usize {
        self.rows.len() - 1
    }

    pub fn col_count(&self) -> usize {
        self.headers.len()
    }

    /// Text of a cell; `""` when the row has fewer fields than `col + 1`.
    /// Panics if `row >= row_count()`.
    #[inline]
    pub fn cell(&self, row: usize, col: usize) -> &str {
        let (a, b) = (self.rows[row], self.rows[row + 1]);
        let first = a.cells as usize;
        if col >= (b.cells - a.cells) as usize {
            return "";
        }
        let start = if col == 0 { 0 } else { self.ends[first + col - 1] as usize };
        let end = self.ends[first + col] as usize;
        let base = a.text as usize;
        &self.arena[base + start..base + end]
    }
}

/// Accumulates rows into the compact representation used by [`Table`].
struct Builder {
    arena: String,
    rows: Vec<RowIndex>,
    ends: Vec<u32>,
    row_start: usize,
    max_fields: usize,
}

impl Builder {
    fn new() -> Self {
        Builder {
            arena: String::new(),
            rows: vec![RowIndex { text: 0, cells: 0 }],
            ends: Vec::new(),
            row_start: 0,
            max_fields: 0,
        }
    }

    fn row_count(&self) -> usize {
        self.rows.len() - 1
    }

    /// Grows capacity towards an estimated final size so large loads avoid repeated doubling.
    fn reserve_total(&mut self, rows: usize, cells: usize) {
        self.rows.reserve((rows + 1).saturating_sub(self.rows.len()));
        self.ends.reserve(cells.saturating_sub(self.ends.len()));
    }

    #[inline]
    fn push_str(&mut self, s: &str) {
        self.arena.push_str(s);
        self.ends.push((self.arena.len() - self.row_start) as u32);
    }

    /// Appends a field that is UTF-8 when valid, otherwise Windows-1252.
    #[inline]
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.push_str(&decode_field(bytes));
    }

    /// Appends a whole CSV record. Valid UTF-8 records are validated and copied in a
    /// single pass; others fall back to per-field decoding.
    fn push_record(&mut self, rec: &csv::ByteRecord) {
        // Every field boundary must also be a char boundary for each field to be valid alone.
        let valid = std::str::from_utf8(rec.as_slice()).ok().filter(|s| {
            let mut end = 0;
            rec.iter().all(|f| {
                end += f.len();
                s.is_char_boundary(end)
            })
        });
        if let Some(s) = valid {
            let mut end = self.arena.len() - self.row_start;
            self.arena.push_str(s);
            for f in rec.iter() {
                end += f.len();
                self.ends.push(end as u32);
            }
        } else {
            for f in rec.iter() {
                self.push_bytes(f);
            }
        }
    }

    /// Closes the current row. Fails if the row's text exceeds the 32-bit relative offset range.
    fn end_row(&mut self, path: &Path) -> Result<(), String> {
        if self.arena.len() - self.row_start > u32::MAX as usize {
            return Err(format!("Cannot load {}: row {} is larger than 4 GiB", path.display(), self.row_count() + 1));
        }
        let cells = self.ends.len() as u64;
        let prev = self.rows[self.rows.len() - 1].cells;
        self.max_fields = self.max_fields.max((cells - prev) as usize);
        self.row_start = self.arena.len();
        self.rows.push(RowIndex { text: self.arena.len() as u64, cells });
        Ok(())
    }

    fn finish(mut self, path: PathBuf, headers: Vec<String>) -> Table {
        self.arena.shrink_to_fit();
        self.rows.shrink_to_fit();
        self.ends.shrink_to_fit();
        let width = headers.len().max(self.max_fields);
        let mut table = Table {
            path,
            headers: normalize_headers(headers, width),
            kinds: Vec::new(),
            arena: self.arena,
            rows: self.rows,
            ends: self.ends,
        };
        table.kinds = infer_kinds(&table);
        table
    }
}

/// Decodes a field that is UTF-8 when valid, otherwise Windows-1252.
fn decode_field(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => WINDOWS_1252.decode_without_bom_handling(bytes).0,
    }
}

/// Trims names, names empty or missing columns `Column N` (1-based), makes duplicates
/// unique by appending ` (2)`, ` (3)`, …, and renames headers that clash with the
/// synthetic `Line`/`Tag` columns (which own those names in filters) the same way.
fn normalize_headers(raw: Vec<String>, width: usize) -> Vec<String> {
    let mut used = HashSet::with_capacity(width);
    let mut out = Vec::with_capacity(width);
    let mut raw = raw.into_iter();
    for i in 0..width {
        let name = raw.next().map(|h| h.trim().to_owned()).unwrap_or_default();
        let name = if name.is_empty() { format!("Column {}", i + 1) } else { name };
        let unique = if used.contains(&name) {
            (2..).map(|k| format!("{name} ({k})")).find(|n| !used.contains(n)).expect("unbounded range")
        } else {
            name
        };
        used.insert(unique.clone());
        out.push(unique);
    }
    for i in 0..out.len() {
        if RESERVED_HEADERS.iter().any(|r| out[i].eq_ignore_ascii_case(r)) {
            let name = (2..)
                .map(|k| format!("{} ({k})", out[i]))
                .find(|n| !out.iter().any(|h| h.eq_ignore_ascii_case(n)))
                .expect("unbounded range");
            out[i] = name;
        }
    }
    out
}

/// Names of the synthetic columns every table view adds.
const RESERVED_HEADERS: [&str; 2] = ["Line", "Tag"];

const KIND_SAMPLES_HEAD: usize = 500;
const KIND_SAMPLES_SPREAD: usize = 500;

fn infer_kinds(table: &Table) -> Vec<ColKind> {
    (0..table.col_count()).into_par_iter().map(|c| infer_kind(table, c)).collect()
}

/// Samples up to 1000 non-empty values: the first 500 from the top of the table
/// and up to 500 more from evenly spaced positions in the remainder.
fn infer_kind(table: &Table, col: usize) -> ColKind {
    let n = table.row_count();
    let mut samples: Vec<&str> = Vec::with_capacity(KIND_SAMPLES_HEAD + KIND_SAMPLES_SPREAD);
    let mut row = 0;
    while row < n && samples.len() < KIND_SAMPLES_HEAD {
        let v = table.cell(row, col).trim();
        if !v.is_empty() {
            samples.push(v);
        }
        row += 1;
    }
    let rest = n - row;
    if rest > 0 {
        let picks = KIND_SAMPLES_SPREAD.min(rest);
        for k in 0..picks {
            let from = row + rest * k / picks;
            let to = row + rest * (k + 1) / picks;
            if let Some(v) = (from..to).map(|r| table.cell(r, col).trim()).find(|v| !v.is_empty()) {
                samples.push(v);
            }
        }
    }
    if samples.is_empty() {
        ColKind::Text
    } else if samples.iter().all(|s| value::parse_number(s).is_some()) {
        ColKind::Number
    } else if samples.iter().all(|s| value::parse_datetime(s).is_some()) {
        ColKind::DateTime
    } else {
        ColKind::Text
    }
}

/// Loads a file. `progress(fraction in 0..=1)` is called periodically (roughly every 1 MB
/// or 50k rows); if it returns false, loading aborts with `Err(CANCELLED)`.
pub fn load(path: &Path, progress: &dyn Fn(f32) -> bool) -> Result<Table, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => load_spreadsheet(path, progress),
        "tsv" | "tab" => load_delimited(path, Some(b'\t'), progress),
        _ => load_delimited(path, None, progress),
    }
}

pub const CANCELLED: &str = "Cancelled";
const PROGRESS_BYTES: u64 = 1 << 20;
const PROGRESS_ROWS: usize = 50_000;
const SNIFF_BYTES: usize = 64 * 1024;
const SNIFF_LINES: usize = 20;

/// Counts bytes read from the underlying file for progress reporting.
struct CountingReader<R> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

/// Streams UTF-8 out of a reader in another encoding (BOM already removed).
struct DecodingReader<R> {
    inner: R,
    decoder: Decoder,
    input: Box<[u8]>,
    in_pos: usize,
    in_len: usize,
    eof: bool,
    output: Box<[u8]>,
    out_pos: usize,
    out_len: usize,
    finished: bool,
}

impl<R: Read> DecodingReader<R> {
    fn new(inner: R, encoding: &'static Encoding) -> Self {
        DecodingReader {
            inner,
            decoder: encoding.new_decoder_without_bom_handling(),
            input: vec![0; 64 * 1024].into_boxed_slice(),
            in_pos: 0,
            in_len: 0,
            eof: false,
            output: vec![0; 128 * 1024].into_boxed_slice(),
            out_pos: 0,
            out_len: 0,
            finished: false,
        }
    }
}

impl<R: Read> Read for DecodingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.out_pos < self.out_len {
                let n = buf.len().min(self.out_len - self.out_pos);
                buf[..n].copy_from_slice(&self.output[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            if self.finished {
                return Ok(0);
            }
            if self.in_pos == self.in_len && !self.eof {
                let n = self.inner.read(&mut self.input)?;
                self.in_pos = 0;
                self.in_len = n;
                self.eof = n == 0;
            }
            let (result, read, written, _) =
                self.decoder.decode_to_utf8(&self.input[self.in_pos..self.in_len], &mut self.output, self.eof);
            self.in_pos += read;
            self.out_pos = 0;
            self.out_len = written;
            if self.eof && result == CoderResult::InputEmpty {
                self.finished = true;
            }
        }
    }
}

fn read_prefix(r: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit);
    r.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Picks the delimiter among `,` `\t` `;` `|` whose count outside quotes is the same
/// nonzero value on every sampled line (largest such count wins); failing that, the
/// one present on the most lines. Defaults to `,`.
fn sniff_delimiter(sample: &[u8], complete: bool) -> u8 {
    const CANDIDATES: [u8; 4] = *b",\t;|";
    let mut lines: Vec<[u32; 4]> = Vec::new();
    let mut cur = [0u32; 4];
    let mut line_len = 0usize;
    let mut in_quotes = false;
    for &b in sample {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                if line_len > 0 {
                    lines.push(cur);
                    if lines.len() == SNIFF_LINES {
                        break;
                    }
                }
                cur = [0; 4];
                line_len = 0;
                continue;
            }
            b'\r' if !in_quotes => continue,
            _ if !in_quotes => {
                if let Some(i) = CANDIDATES.iter().position(|&c| c == b) {
                    cur[i] += 1;
                }
            }
            _ => {}
        }
        line_len += 1;
    }
    if complete && line_len > 0 && lines.len() < SNIFF_LINES {
        lines.push(cur);
    }

    let mut best = (b',', (0u32, 0u32, 0usize, 0u32));
    for (i, &delim) in CANDIDATES.iter().enumerate() {
        let first = lines.first().map_or(0, |l| l[i]);
        let consistent = first > 0 && lines.iter().all(|l| l[i] == first);
        let score = (
            u32::from(consistent),
            if consistent { first } else { 0 },
            lines.iter().filter(|l| l[i] > 0).count(),
            lines.iter().map(|l| l[i]).sum(),
        );
        if score > best.1 {
            best = (delim, score);
        }
    }
    best.0
}

fn load_delimited(path: &Path, forced_delim: Option<u8>, progress: &dyn Fn(f32) -> bool) -> Result<Table, String> {
    let open_err = |e: io::Error| format!("Cannot read {}: {e}", path.display());
    let file = File::open(path).map_err(open_err)?;
    let file_len = file.metadata().map_err(open_err)?.len();
    let count = Rc::new(Cell::new(0u64));
    let mut counted = CountingReader { inner: file, count: Rc::clone(&count) };
    let raw_prefix = read_prefix(&mut counted, SNIFF_BYTES).map_err(open_err)?;
    let complete = (raw_prefix.len() as u64) == file_len;

    let (encoding, bom_len): (Option<&'static Encoding>, usize) = match raw_prefix.as_slice() {
        [0xEF, 0xBB, 0xBF, ..] => (None, 3),
        [0xFF, 0xFE, ..] => (Some(UTF_16LE), 2),
        [0xFE, 0xFF, ..] => (Some(UTF_16BE), 2),
        _ => (None, 0),
    };

    let delimiter = forced_delim.unwrap_or_else(|| match encoding {
        None => sniff_delimiter(&raw_prefix[bom_len..], complete),
        Some(enc) => {
            let usable = (raw_prefix.len() - bom_len) & !1;
            let decoded = enc.decode_without_bom_handling(&raw_prefix[bom_len..bom_len + usable]).0;
            sniff_delimiter(decoded.as_bytes(), complete)
        }
    });

    let body_len = file_len.saturating_sub(bom_len as u64);
    // Estimated UTF-8 stream length (UTF-16 text is assumed mostly ASCII).
    let stream_len = if encoding.is_some() { body_len / 2 } else { body_len };
    let mut raw_prefix = raw_prefix;
    raw_prefix.drain(..bom_len);
    let stream = io::Cursor::new(raw_prefix).chain(counted);
    let stream: Box<dyn Read> = match encoding {
        None => Box::new(stream),
        Some(enc) => Box::new(DecodingReader::new(stream, enc)),
    };

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .buffer_capacity(1 << 20)
        .from_reader(stream);

    let mut builder = Builder::new();
    builder.arena.reserve(stream_len as usize);
    let mut headers: Option<Vec<String>> = None;
    let mut rec = csv::ByteRecord::new();
    let mut next_mark = PROGRESS_BYTES;
    let mut reserved = false;
    let report = |done: u64| {
        let fraction = if file_len == 0 { 1.0 } else { (done as f64 / file_len as f64).min(1.0) as f32 };
        progress(fraction)
    };

    loop {
        match rdr.read_byte_record(&mut rec) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => return Err(format!("Cannot parse {}: {e}", path.display())),
        }
        if rec.len() == 1 && rec[0].is_empty() {
            continue;
        }
        if headers.is_none() {
            headers = Some(rec.iter().map(|f| decode_field(f).into_owned()).collect());
            continue;
        }
        builder.push_record(&rec);
        builder.end_row(path)?;

        let rows = builder.row_count();
        if !reserved && rows == PROGRESS_ROWS {
            reserved = true;
            let pos = rec.position().map_or(0, |p| p.byte()).max(1);
            let scale = stream_len as f64 / pos as f64 * 1.02;
            builder.reserve_total((rows as f64 * scale) as usize, (builder.ends.len() as f64 * scale) as usize);
        }
        if count.get() >= next_mark || rows.is_multiple_of(PROGRESS_ROWS) {
            next_mark = count.get() + PROGRESS_BYTES;
            if !report(count.get()) {
                return Err(CANCELLED.into());
            }
        }
    }
    if !report(file_len) {
        return Err(CANCELLED.into());
    }
    Ok(builder.finish(path.to_path_buf(), headers.unwrap_or_default()))
}

fn load_spreadsheet(path: &Path, progress: &dyn Fn(f32) -> bool) -> Result<Table, String> {
    let err = |e: calamine::Error| format!("Cannot read {}: {e}", path.display());
    let mut workbook = calamine::open_workbook_auto(path).map_err(err)?;
    if !progress(0.0) {
        return Err(CANCELLED.into());
    }
    let mut range = None;
    for name in workbook.sheet_names() {
        let r = workbook.worksheet_range(&name).map_err(err)?;
        if r.used_cells().any(|(_, _, d)| !matches!(d, Data::Empty)) {
            range = Some(r);
            break;
        }
        if !progress(0.0) {
            return Err(CANCELLED.into());
        }
    }
    let Some(range) = range else {
        return Ok(Builder::new().finish(path.to_path_buf(), Vec::new()));
    };
    if !progress(0.5) {
        return Err(CANCELLED.into());
    }

    let height = range.height();
    let mut rows = range.rows();
    let mut scratch = String::new();
    let headers: Vec<String> = rows
        .next()
        .map(|r| {
            r.iter()
                .map(|d| {
                    scratch.clear();
                    format_cell(d, &mut scratch);
                    scratch.clone()
                })
                .collect()
        })
        .unwrap_or_default();

    let mut builder = Builder::new();
    for (i, row) in rows.enumerate() {
        let used = row.iter().rposition(|d| !matches!(d, Data::Empty)).map_or(0, |p| p + 1);
        for d in &row[..used] {
            scratch.clear();
            format_cell(d, &mut scratch);
            builder.push_str(&scratch);
        }
        builder.end_row(path)?;
        if (i + 1).is_multiple_of(PROGRESS_ROWS) && !progress(0.5 + 0.5 * (i + 2) as f32 / height as f32) {
            return Err(CANCELLED.into());
        }
    }
    if !progress(1.0) {
        return Err(CANCELLED.into());
    }
    Ok(builder.finish(path.to_path_buf(), headers))
}

/// Renders a spreadsheet cell as display text into `out`.
fn format_cell(d: &Data, out: &mut String) {
    match d {
        Data::Empty => {}
        Data::String(s) | Data::DurationIso(s) => out.push_str(s),
        Data::DateTimeIso(s) => {
            // ISO `YYYY-MM-DDTHH:MM:SS` → `YYYY-MM-DD HH:MM:SS`, matching Excel serial dates below.
            out.push_str(s);
            if out.as_bytes().get(10) == Some(&b'T') {
                out.replace_range(10..11, " ");
            }
        }
        Data::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Data::Float(f) => {
            let _ = write!(out, "{f}");
        }
        Data::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Data::Error(e) => {
            let _ = write!(out, "{e}");
        }
        Data::DateTime(dt) if dt.is_duration() => {
            let ms = (dt.as_f64() * 86_400_000.0).round() as i64;
            let (sign, ms) = if ms < 0 { ("-", -ms) } else { ("", ms) };
            let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
            let _ = write!(out, "{sign}{h}:{m:02}:{s:02}");
            if frac != 0 {
                let _ = write!(out, ".{frac:03}");
            }
        }
        Data::DateTime(dt) => {
            let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
            let _ = write!(out, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
            if ms != 0 {
                let _ = write!(out, ".{ms:03}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{CellErrorType, ExcelDateTime, ExcelDateTimeType};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A uniquely named temp file removed on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(ext: &str, bytes: &[u8]) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let name = format!("ferrotime-data-{}-{}.{ext}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, bytes).unwrap();
            TempFile(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn load_bytes(ext: &str, bytes: &[u8]) -> Table {
        let f = TempFile::new(ext, bytes);
        load(&f.0, &|_| true).unwrap()
    }

    fn row(t: &Table, r: usize) -> Vec<&str> {
        (0..t.col_count()).map(|c| t.cell(r, c)).collect()
    }

    #[test]
    fn quoted_fields() {
        let t = load_bytes("csv", b"a,b,c\r\n\"x, y\",\"line1\nline2\",\"say \"\"hi\"\"\"\r\n1,2,3\r\n\r\n");
        assert_eq!(t.headers, ["a", "b", "c"]);
        assert_eq!(t.row_count(), 2);
        assert_eq!(row(&t, 0), ["x, y", "line1\nline2", "say \"hi\""]);
        assert_eq!(row(&t, 1), ["1", "2", "3"]);
    }

    #[test]
    fn ragged_rows() {
        let t = load_bytes("csv", b"a,b,c\n1\n1,2,3,4,5\n\n1,2\n");
        assert_eq!(t.headers, ["a", "b", "c", "Column 4", "Column 5"]);
        assert_eq!(t.kinds.len(), 5);
        assert_eq!(t.row_count(), 3);
        assert_eq!(row(&t, 0), ["1", "", "", "", ""]);
        assert_eq!(row(&t, 1), ["1", "2", "3", "4", "5"]);
        assert_eq!(row(&t, 2), ["1", "2", "", "", ""]);
        assert_eq!(t.cell(0, 99), "");
    }

    #[test]
    fn header_normalization() {
        let t = load_bytes("csv", b" Name ,,Name,Name (2),name, \nv,w,x,y,z,q\n");
        assert_eq!(t.headers, ["Name", "Column 2", "Name (2)", "Name (2) (2)", "name", "Column 6"]);
    }

    #[test]
    fn utf8_bom() {
        let t = load_bytes("csv", b"\xEF\xBB\xBFid,name\n1,caf\xC3\xA9\n");
        assert_eq!(t.headers, ["id", "name"]);
        assert_eq!(row(&t, 0), ["1", "café"]);
    }

    #[test]
    fn utf16le_bom() {
        let text = "id;name\r\n1;caf\u{e9}\r\n2;\"a;b\"\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let t = load_bytes("csv", &bytes);
        assert_eq!(t.headers, ["id", "name"]);
        assert_eq!(row(&t, 0), ["1", "café"]);
        assert_eq!(row(&t, 1), ["2", "a;b"]);
    }

    #[test]
    fn utf16be_bom() {
        let text = "x,y\n\u{4e2d},2\n";
        let mut bytes = vec![0xFE, 0xFF];
        bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        let t = load_bytes("txt", &bytes);
        assert_eq!(t.headers, ["x", "y"]);
        assert_eq!(row(&t, 0), ["\u{4e2d}", "2"]);
    }

    #[test]
    fn invalid_utf8_falls_back_to_windows_1252() {
        let t = load_bytes("csv", b"a,b\ncaf\xE9,ok \xC3\xA9\n");
        assert_eq!(row(&t, 0), ["café", "ok é"]);
    }

    #[test]
    fn sniffs_tab_and_honours_tsv_extension() {
        let t = load_bytes("txt", b"a\tb\tc\n1,5\t2\t3\n4\t5\t6\n");
        assert_eq!(t.headers, ["a", "b", "c"]);
        assert_eq!(row(&t, 0), ["1,5", "2", "3"]);

        // Commas dominate, but the extension forces tabs.
        let t = load_bytes("tsv", b"a,b\tc,d\n1,2\t3,4\n");
        assert_eq!(t.headers, ["a,b", "c,d"]);
        assert_eq!(row(&t, 0), ["1,2", "3,4"]);
    }

    #[test]
    fn sniffs_semicolon() {
        let t = load_bytes("csv", b"a;b;c\n1,5;\"x;y\";3\n2,5;z;4\n");
        assert_eq!(t.headers, ["a", "b", "c"]);
        assert_eq!(row(&t, 0), ["1,5", "x;y", "3"]);
    }

    #[test]
    fn sniff_prefers_consistent_counts() {
        assert_eq!(sniff_delimiter(b"a|b|c\nx,y|1|2\np|q|r\n", true), b'|');
        assert_eq!(sniff_delimiter(b"no delimiters here\n", true), b',');
        assert_eq!(sniff_delimiter(b"", true), b',');
        assert_eq!(sniff_delimiter(b"a\tb\n\"1\n2,3,4\"\tx\n", true), b'\t');
    }

    #[test]
    fn kind_inference() {
        let mut csv = String::from("num,when,text,empty,mixed\n");
        for i in 0..3000 {
            let mixed = if i == 2500 { "oops" } else { "7" };
            csv.push_str(&format!("{i}.5, 2022-08-05 06:12:{:02} ,x{i},,{mixed}\n", i % 60));
        }
        let t = load_bytes("csv", csv.as_bytes());
        assert_eq!(t.kinds, [ColKind::Number, ColKind::DateTime, ColKind::Text, ColKind::Text, ColKind::Text]);
        assert_eq!(t.cell(0, 1), " 2022-08-05 06:12:00 ");
    }

    #[test]
    fn from_rows_extends_and_infers() {
        let t = Table::from_rows(
            PathBuf::from("mem"),
            vec!["a".into()],
            vec![vec!["1".into(), "2022-01-01".into()], vec![], vec!["3".into()]],
        );
        assert_eq!(t.headers, ["a", "Column 2"]);
        assert_eq!(t.kinds, [ColKind::Number, ColKind::DateTime]);
        assert_eq!(t.row_count(), 3);
        assert_eq!(row(&t, 1), ["", ""]);
        assert_eq!(row(&t, 2), ["3", ""]);
    }

    #[test]
    fn reserved_headers_renamed() {
        let headers = ["line", "Tag", "Tag (2)", "x"].map(String::from).to_vec();
        let t = Table::from_rows(PathBuf::from("mem"), headers, vec![]);
        assert_eq!(t.headers, ["line (2)", "Tag (3)", "Tag (2)", "x"]);
    }

    #[test]
    fn cancellation() {
        let mut csv = String::from("a,b\n");
        for i in 0..200_000 {
            csv.push_str(&format!("{i},value {i}\n"));
        }
        let f = TempFile::new("csv", csv.as_bytes());
        let calls = Cell::new(0);
        let res = load(&f.0, &|p| {
            assert!((0.0..=1.0).contains(&p));
            calls.set(calls.get() + 1);
            false
        });
        assert_eq!(res.err().as_deref(), Some(CANCELLED));
        assert_eq!(calls.get(), 1);

        let calls = Cell::new(0);
        let t = load(&f.0, &|_| {
            calls.set(calls.get() + 1);
            true
        })
        .unwrap();
        assert_eq!(t.row_count(), 200_000);
        assert_eq!(t.cell(199_999, 1), "value 199999");
        assert!(calls.get() > 1 && calls.get() < 100, "{} progress calls", calls.get());
    }

    #[test]
    fn empty_and_header_only_files() {
        let t = load_bytes("csv", b"");
        assert_eq!((t.col_count(), t.row_count()), (0, 0));
        assert!(t.kinds.is_empty());
        let t = load_bytes("csv", b"a,b\n");
        assert_eq!(t.headers, ["a", "b"]);
        assert_eq!(t.row_count(), 0);
        assert_eq!(t.kinds, [ColKind::Text, ColKind::Text]);
    }

    #[test]
    fn missing_file_errors() {
        assert!(load(Path::new("/nonexistent/ferrotime-data.csv"), &|_| true).is_err());
    }

    #[test]
    fn spreadsheet_cell_formatting() {
        let fmt = |d: Data| {
            let mut s = String::new();
            format_cell(&d, &mut s);
            s
        };
        assert_eq!(fmt(Data::Empty), "");
        assert_eq!(fmt(Data::Int(-42)), "-42");
        assert_eq!(fmt(Data::Float(3.0)), "3");
        assert_eq!(fmt(Data::Float(0.1 + 0.2)), "0.30000000000000004");
        assert_eq!(fmt(Data::Float(1.5)), "1.5");
        assert_eq!(fmt(Data::Bool(true)), "True");
        assert_eq!(fmt(Data::Bool(false)), "False");
        assert_eq!(fmt(Data::String("hi".into())), "hi");
        assert_eq!(fmt(Data::Error(CellErrorType::Div0)), "#DIV/0!");
        assert_eq!(fmt(Data::DateTimeIso("2022-08-05T06:12:39".into())), "2022-08-05 06:12:39");
        let dt = |v, k| Data::DateTime(ExcelDateTime::new(v, k, false));
        assert_eq!(fmt(dt(45943.541, ExcelDateTimeType::DateTime)), "2025-10-13 12:59:02.400");
        assert_eq!(fmt(dt(45943.5, ExcelDateTimeType::DateTime)), "2025-10-13 12:00:00");
        assert_eq!(fmt(dt(1.5, ExcelDateTimeType::TimeDelta)), "36:00:00");
    }

    #[test]
    fn table_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<Table>();
    }
}
