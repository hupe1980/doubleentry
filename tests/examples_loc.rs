//! The canonical programs stay short: each of the nine files in
//! `examples/` within its budget of rustfmt'd lines — non-blank, not a `//`
//! comment — and all nine within 350; and none imports more than three
//! `doubleentry` names besides the prelude. `cargo fmt --check` (CI) keeps
//! the files rustfmt'd, so the count is the formatted one. Other examples
//! (holds, transparency) are not counted.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

const BUDGETS: [(&str, usize); 9] = [
    ("wallet", 40),
    ("invoice_clearing", 34),
    ("month_close", 24),
    ("multi_currency", 40),
    ("reversal", 37),
    ("sqlite_service", 46),
    ("reporting", 48),
    ("witness", 42),
    ("restore_audit", 37),
];

const TOTAL: usize = 350;

fn source(name: &str) -> String {
    let path = format!("{}/examples/{name}.rs", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The FR-030 count: lines that are neither blank nor a `//` comment.
fn lines(source: &str) -> usize {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .count()
}

/// The `doubleentry` names a file imports besides `prelude::*`.
fn imported(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in source.lines().map(str::trim) {
        let Some(path) = line
            .strip_prefix("use doubleentry::")
            .and_then(|path| path.strip_suffix(';'))
        else {
            continue;
        };
        if path == "prelude::*" {
            continue;
        }
        match path.split_once('{') {
            Some((module, list)) => names.extend(
                list.trim_end_matches('}')
                    .split(',')
                    .map(|name| format!("{module}{}", name.trim())),
            ),
            None => names.push(path.to_owned()),
        }
    }
    names
}

#[test]
fn every_canonical_example_is_within_budget() {
    let mut total = 0;
    for (name, budget) in BUDGETS {
        let count = lines(&source(name));
        assert!(count <= budget, "{name}: {count} lines, budget {budget}");
        total += count;
    }
    assert!(total <= TOTAL, "{total} lines in all, budget {TOTAL}");
}

#[test]
fn every_canonical_example_imports_at_most_three_names() {
    for (name, _) in BUDGETS {
        let names = imported(&source(name));
        assert!(names.len() <= 3, "{name} imports {names:?}");
    }
}

#[test]
fn the_count_ignores_blank_lines_and_comments() {
    assert_eq!(
        lines("//! doc\nuse x;\n\n    // note\nfn main() {} // trailing\n"),
        2
    );
    assert_eq!(
        imported("use doubleentry::prelude::*;\nuse doubleentry::store::{SqlStore, Sqlite};\n"),
        ["store::SqlStore", "store::Sqlite"]
    );
}
