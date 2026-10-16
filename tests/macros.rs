//! The literal macros: `amount!` and `date!` are checked while compiling —
//! a malformed literal, a day the month does not have or more decimals than
//! the scale is a compile error — and work in `const` items of a crate that
//! does not depend on `time`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

#[test]
fn literals_are_checked_at_compile_time() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/literals_in_consts.rs");
    cases.compile_fail("tests/ui/amount_*.rs");
    cases.compile_fail("tests/ui/date_*.rs");
}

/// SC-006: a crate depending on `doubleentry` alone builds with both macros
/// in `const` items.
#[test]
fn a_crate_without_time_uses_both_macros() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(format!("{root}/tests/no_time_dep/Cargo.toml"))
        .env("CARGO_TARGET_DIR", format!("{root}/target/no_time_dep"))
        .status()
        .unwrap();
    assert!(status.success());
    let manifest = std::fs::read_to_string(format!("{root}/tests/no_time_dep/Cargo.toml")).unwrap();
    assert!(
        !manifest.contains("time ="),
        "the crate must not depend on time"
    );
}
