# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-10-20

First public release, with pre-built binaries for macOS (universal: Apple Silicon and Intel) and Linux (x86_64 and aarch64).

### Added

- Timeline Explorer–style window for macOS and Linux: tabs, group-by box, search box, auto filter row, `Line` and `Tag` columns, filter bar, status bar and search options.
- CSV/TSV and other delimited text (automatic delimiter and encoding detection) and Excel/OpenDocument spreadsheets, loaded in the background with progress.
- Filtering through the auto filter row, header Values/Text Filters popups and a Filter Editor with a criteria language.
- Search with *Filter* and *Search* behaviours and *Mixed*, *Or*, *And* and *Exact* match criteria.
- Multi-column sorting, nested grouping, tagging, conditional formatting, column chooser, best fit, column reordering and pinning.
- Cell contents window, copy to clipboard, CSV export of visible or tagged rows, light and dark skins.
- Ferrotime sessions (`.ferrotime`) and reading/writing Timeline Explorer sessions (`.tle_sess`).

[Unreleased]: https://github.com/jackcsk/ferrotime/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jackcsk/ferrotime/releases/tag/v0.1.0
