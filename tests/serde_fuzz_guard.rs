//! The serde fuzz target (`fuzz/fuzz_targets/serde.rs`) covers every public
//! type that implements `Deserialize`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The identifier at the start of `s`.
fn ident(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect()
}

/// The names of the types in `text` that derive `Deserialize` (directly or
/// under `cfg_attr`) as a `pub struct`/`pub enum`, or that have a
/// hand-written `Deserialize<'de> for` impl. An impl inside a
/// `macro_rules!` (`for $ty`) stands for the arguments of that macro's
/// invocations in the same file.
fn deserialize_types(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut found = Vec::new();
    let mut in_macro: Option<String> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("//") {
            continue;
        }
        if let Some(name) = line.strip_prefix("macro_rules! ") {
            in_macro = Some(ident(name));
        }
        // A derive list naming `Deserialize`, possibly spread over the
        // lines of a `#[cfg_attr(...)]`.
        let derive = line.contains("Deserialize")
            && !line.contains("Deserialize<")
            && lines[i.saturating_sub(3)..=i]
                .iter()
                .any(|l| l.contains("derive("));
        if derive {
            let item = lines[i + 1..]
                .iter()
                .find(|l| l.contains("struct ") || l.contains("enum "));
            let name = item.and_then(|l| {
                l.strip_prefix("pub struct ")
                    .or_else(|| l.strip_prefix("pub enum "))
            });
            found.extend(name.map(ident));
        }
        if line.starts_with("impl")
            && let Some((_, rest)) = line.split_once("Deserialize<'de> for ")
        {
            let name = ident(rest);
            match (name.strip_prefix('$'), &in_macro) {
                (Some(_), Some(mac)) => {
                    let call = format!("{mac}!(");
                    for l in &lines {
                        if let Some(args) = l.strip_prefix(&call) {
                            let args = args.split(')').next().unwrap_or_default();
                            found.extend(args.split(',').map(|a| ident(a.trim())));
                        }
                    }
                }
                (Some(_), None) => panic!("an impl for {name} outside a macro"),
                (None, _) => found.push(name),
            }
        }
    }
    found
}

#[test]
fn serde_fuzz_target_lists_every_deserialize_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Ok(fuzz) = fs::read_to_string(root.join("fuzz/fuzz_targets/serde.rs")) else {
        // A packaged crate leaves out the nested fuzz package.
        eprintln!("fuzz/ not present; skipped");
        return;
    };
    // Public means: in the reviewed API surface (kept current by
    // tests/api_surface.rs), as a struct or enum of this crate.
    let api = fs::read_to_string(root.join("tests/public-api.txt")).unwrap();
    let public = |name: &str| {
        api.lines().any(|l| {
            let item = l
                .split_once("pub struct doubleentry::")
                .or_else(|| l.split_once("pub enum doubleentry::"))
                .map(|(_, path)| path);
            item.is_some_and(|path| {
                let path = path.split(['<', '(', ' ']).next().unwrap_or_default();
                path.rsplit("::").next() == Some(name)
            })
        })
    };
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    let mut found: Vec<String> = files
        .iter()
        .flat_map(|file| deserialize_types(&fs::read_to_string(file).unwrap()))
        .filter(|name| public(name))
        .collect();
    found.sort();
    found.dedup();
    assert!(found.len() >= 50, "the scan found too few types: {found:?}");
    let missing: Vec<_> = found
        .iter()
        .filter(|name| {
            !fuzz.contains(&format!("roundtrip::<{name}>"))
                && !fuzz.contains(&format!("roundtrip::<{name}<"))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "public Deserialize types missing from fuzz/fuzz_targets/serde.rs: {missing:?}"
    );
}
