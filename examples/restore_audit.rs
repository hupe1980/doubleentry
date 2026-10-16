//! Back up a SQLite ledger, restore the copy, and prove by replay that nothing was altered.
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};
use std::{fs, path::Path};

const START: Date = date!("2026-01-01");

async fn open(file: &Path) -> Result<(SqlStore<Sqlite>, Ledger<SqlStore<Sqlite>>)> {
    let store = SqlStore::sqlite(&format!("sqlite://{}?mode=rwc", file.display())).await?;
    let ledger = Ledger::new(store.clone(), LedgerId::new("example.com/acme")?).await?;
    Ok((store, ledger))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("doubleentry-restore-audit");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let (store, live) = open(&dir.join("live.db")).await?;
    live.declare_currency(EUR, 2, START).await?;
    live.define_months(2026, 3..=4).await?;
    live.open("Assets:Cash", AccountKind::Asset, START).await?;
    live.open("Income:Sales", AccountKind::Income, START)
        .await?;
    for i in 0..10 {
        let sale = Entry::new(format!("e{i}"), date!("2026-03-05"));
        let sale = sale.debit("Assets:Cash", amount!("10.00"), EUR);
        live.record(sale.credit("Income:Sales", amount!("10.00"), EUR))
            .await?;
    }
    live.seal_period("2026-03").await?;
    store.close().await; // the file alone is now a complete copy

    fs::copy(dir.join("live.db"), dir.join("backup.db"))?;
    let (_, restored) = open(&dir.join("backup.db")).await?;
    println!("{}", restored.audit().await?); // replays every record through the rule core
    let books = restored.to_journal().await?; // an in-memory replica, seals included
    assert_eq!(books.head(), restored.head().await?);
    Ok(())
}
