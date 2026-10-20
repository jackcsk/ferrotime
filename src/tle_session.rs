//! Timeline Explorer session files (`.tle_sess`), so analysts on Windows and on
//! macOS/Linux can exchange sessions.
//!
//! The format, as written by Timeline Explorer, is JSON of the shape
//! `{"SessionFiles":{"C:\\path\\file.csv":[3,9]}}`: each open file (in tab order) mapped to
//! the `Line` numbers of its tagged rows. Older versions wrote a plain array of paths.
//! Nothing else (filters, sorting, layout) is stored in it.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

pub const EXTENSION: &str = "tle_sess";

/// One file of a Timeline Explorer session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TleFile {
    /// Path as recorded in the session (may be a Windows path).
    pub path: String,
    /// 1-based `Line` numbers of tagged rows.
    pub tagged_lines: Vec<usize>,
}

/// Files in insertion (tab) order; JSON object order is significant here.
struct OrderedFiles(Vec<TleFile>);

impl<'de> Deserialize<'de> for OrderedFiles {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OrderedFiles;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object mapping file paths to tagged line numbers")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<OrderedFiles, A::Error> {
                let mut files: Vec<TleFile> = Vec::new();
                while let Some((path, lines)) = map.next_entry::<String, Option<Vec<i64>>>()? {
                    if files.iter().any(|f| f.path == path) {
                        continue;
                    }
                    let tagged_lines = lines
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|l| usize::try_from(l).ok())
                        .filter(|&l| l > 0)
                        .collect();
                    files.push(TleFile { path, tagged_lines });
                }
                Ok(OrderedFiles(files))
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Document {
    Current {
        #[serde(rename = "SessionFiles", alias = "sessionFiles", alias = "sessionfiles")]
        files: Option<OrderedFiles>,
    },
    Legacy(Vec<String>),
}

pub fn parse(text: &str) -> Result<Vec<TleFile>, String> {
    let text = text.trim_start_matches('\u{feff}');
    match serde_json::from_str::<Document>(text) {
        Ok(Document::Current { files }) => Ok(files.map(|f| f.0).unwrap_or_default()),
        Ok(Document::Legacy(paths)) => {
            let mut files: Vec<TleFile> = Vec::new();
            for path in paths {
                if !files.iter().any(|f| f.path == path) {
                    files.push(TleFile { path, tagged_lines: Vec::new() });
                }
            }
            Ok(files)
        }
        Err(_) => Err("not a Timeline Explorer session file".into()),
    }
}

/// Serializes exactly like Timeline Explorer (compact JSON, `SessionFiles` object).
pub fn render(files: &[TleFile]) -> String {
    let entries: Vec<String> = files
        .iter()
        .map(|f| {
            let key = serde_json::to_string(&f.path).expect("strings always serialize");
            let lines = serde_json::to_string(&f.tagged_lines).expect("integers always serialize");
            format!("{key}:{lines}")
        })
        .collect();
    format!("{{\"SessionFiles\":{{{}}}}}", entries.join(","))
}

pub fn load(path: &Path) -> Result<Vec<TleFile>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn save(path: &Path, files: &[TleFile]) -> Result<(), String> {
    std::fs::write(path, render(files)).map_err(|e| format!("{}: {e}", path.display()))
}

/// Finds a session file on this machine. Sessions record absolute paths of the machine
/// that saved them (often `C:\…`), so after the exact path, try the recorded path's
/// trailing components below the session's folder, longest first: a session in
/// `~/case/` that recorded `C:\Cases\case\out\a.csv` resolves to `~/case/out/a.csv`,
/// or to `~/case/a.csv` if the files were copied flat next to the session.
pub fn resolve(recorded: &str, session_dir: &Path) -> Option<PathBuf> {
    let exact = Path::new(recorded);
    if exact.is_file() {
        return Some(exact.to_path_buf());
    }
    let parts: Vec<&str> =
        recorded.split(['\\', '/']).filter(|p| !p.is_empty() && !p.ends_with(':') && *p != "." && *p != "..").collect();
    (1..=parts.len())
        .rev()
        .map(|k| parts[parts.len() - k..].iter().fold(session_dir.to_path_buf(), |acc, p| acc.join(p)))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timeline_explorer_output() {
        let text = "\u{feff}{\"SessionFiles\":{\"C:\\\\Cases\\\\b.csv\":[5,2],\
                    \"C:\\\\Cases\\\\a.csv\":[],\"D:\\\\x.csv\":null}}";
        let files = parse(text).unwrap();
        assert_eq!(
            files,
            vec![
                TleFile { path: r"C:\Cases\b.csv".into(), tagged_lines: vec![5, 2] },
                TleFile { path: r"C:\Cases\a.csv".into(), tagged_lines: vec![] },
                TleFile { path: r"D:\x.csv".into(), tagged_lines: vec![] },
            ],
            "tab order must follow the JSON object order"
        );
    }

    #[test]
    fn parses_legacy_path_list_and_rejects_other_json() {
        let files = parse(r#"["/a.csv","/b.csv","/a.csv"]"#).unwrap();
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["/a.csv", "/b.csv"]);
        assert!(parse(r#"{"version":1,"tabs":[]}"#).unwrap().is_empty());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn render_round_trips_and_matches_timeline_explorer_layout() {
        let files = vec![
            TleFile { path: r#"C:\Cases\日誌 "q".csv"#.into(), tagged_lines: vec![1, 42] },
            TleFile { path: "/Users/a/b.csv".into(), tagged_lines: vec![] },
        ];
        let text = render(&files);
        assert!(text.starts_with(r#"{"SessionFiles":{"C:\\Cases\\日誌 \"q\".csv":[1,42],"#), "{text}");
        assert_eq!(parse(&text).unwrap(), files);
    }

    #[test]
    fn resolves_windows_paths_relative_to_the_session_folder() {
        let root = std::env::temp_dir().join(format!("ferrotime-tle-{}", std::process::id()));
        let nested = root.join("out");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.csv"), "x").unwrap();
        std::fs::write(root.join("b.csv"), "x").unwrap();

        assert_eq!(resolve(r"C:\Cases\case\out\a.csv", &root), Some(nested.join("a.csv")));
        assert_eq!(resolve(r"\\server\share\b.csv", &root), Some(root.join("b.csv")));
        let exact = nested.join("a.csv");
        assert_eq!(resolve(exact.to_str().unwrap(), Path::new("/nonexistent")), Some(exact));
        assert_eq!(resolve(r"C:\Cases\missing.csv", &root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
