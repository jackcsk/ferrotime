# Contributing to Ferrotime

Thanks for helping! Bug reports, feature requests and pull requests are all welcome.

## Reporting bugs

Open an issue with:

- your OS and version (e.g. macOS 15.4, Ubuntu 24.04 on Wayland) and the Ferrotime version (**Help ▸ About**);
- what you did, what you expected, and what happened;
- if the problem depends on the data, a small sample file that reproduces it. **Never attach real case data**; redact or synthesize it.

Security issues should not be reported in public issues; see [SECURITY.md](SECURITY.md).

Ferrotime is not affiliated with Timeline Explorer. If Timeline Explorer itself misbehaves, that's for its author, not this project; if Ferrotime behaves differently from Timeline Explorer in a way that gets in your way, please do report that here.

## Development setup

You need Rust 1.95 or newer and, on Linux, the packages listed in the [README](README.md#linux).

```sh
cargo run -- path/to/file.csv     # debug build
cargo run --release -- file.csv   # use release builds for multi-million-row files
```

Before sending a pull request, run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Code layout

| File | Responsibility |
|---|---|
| `src/main.rs` | Entry point and window setup |
| `src/app.rs` | Application shell: menus, tabs, group-by box and search, filter bar, status bar, dialogs, shortcuts |
| `src/grid.rs` | The virtualized grid: headers, auto filter row, rows and groups, pinned columns, scrollbars, header/row menus, filter popups |
| `src/view.rs` | Per-tab state and the background filter/sort/group pipeline |
| `src/data.rs` | Compact in-memory table and the CSV/TSV/spreadsheet loaders |
| `src/expr.rs` | The filter language: parser, printer, evaluator, syntax highlighting |
| `src/search.rs` | Search box queries (*Mixed*, *Or*, *And*, *Exact*) and match highlighting |
| `src/format.rs` | Conditional formatting rules and presets |
| `src/session.rs` | `.ferrotime` sessions |
| `src/tle_session.rs` | Timeline Explorer `.tle_sess` sessions |
| `src/value.rs` | Shared column types and number/timestamp parsing |

## Guidelines

- Keep behaviour close to Timeline Explorer where it exists there; that familiarity is the point of the project. Note deliberate differences in the pull request.
- Performance matters: tables can have millions of rows. Avoid per-row allocation in filter, sort and paint paths, and keep heavy work off the UI thread.
- Add tests for parsing and matching logic (`expr`, `search`, `value`, `data`, sessions). UI changes should include a screenshot in the pull request.
- Don't add Timeline Explorer code, assets or screenshots of it to this repository.

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
