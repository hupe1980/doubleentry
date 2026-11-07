//! The least-privilege grants the PostgreSQL page shows are the ones the
//! crate generates (`Postgres::runtime_grants`), so the documentation cannot
//! drift from the schema. The site is not part of the published crate: the
//! check is skipped where the page is absent.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::panic)]

use doubleentry::store::Postgres;

/// The page, and the block after its `<!-- runtime-grants -->` marker.
fn documented() -> Option<String> {
    let path = format!(
        "{}/site/content/docs/operating/postgresql.md",
        env!("CARGO_MANIFEST_DIR")
    );
    let page = std::fs::read_to_string(path).ok()?;
    let after = page.split("<!-- runtime-grants -->").nth(1)?;
    let block = after.split("```sql\n").nth(1)?.split("```").next()?;
    Some(block.to_owned())
}

#[test]
fn the_documented_grants_are_the_generated_ones() {
    let Some(block) = documented() else {
        return;
    };
    let generated: String = Postgres::runtime_grants("ledger", "app")
        .unwrap()
        .iter()
        .map(|grant| format!("{grant};\n"))
        .collect();
    assert_eq!(
        block, generated,
        "update the runtime-grants block of the PostgreSQL page"
    );
}
