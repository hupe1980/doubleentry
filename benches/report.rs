//! Report timings, recorded and not gated: a current balance
//! (the maintained row), a position-bounded one (one `balance_after`), a
//! folded one, the trial balance, and the export of 100 000 entries — on
//! `Journal` and on a `Ledger` over `MemoryStore`.
//!
//! `cargo bench --bench report`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::time::{Duration, Instant};

use doubleentry::export::ExportScope;
use doubleentry::store::MemoryStore;
use doubleentry::{
    AccountKind, Amount, BalanceQuery, Entry, Journal, Ledger, LedgerId, MemoSalt, currency::EUR,
    date,
};

const ENTRIES: u32 = 100_000;

fn time<T>(what: &str, runs: u32, mut read: impl FnMut() -> T) {
    let started = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(read());
    }
    let each = started.elapsed() / runs;
    println!("{what}: {each:?}");
}

async fn time_async<T, F: Future<Output = T>>(what: &str, runs: u32, mut read: impl FnMut() -> F) {
    let started = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(read().await);
    }
    let each: Duration = started.elapsed() / runs;
    println!("{what}: {each:?}");
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let books = Ledger::new(
            MemoryStore::new(),
            LedgerId::new("example.com/bench").unwrap(),
        )
        .await
        .unwrap()
        .with_salt_source(|| MemoSalt([1; 16]));
        let on = date!("2026-01-01");
        books.declare_currency(EUR, 2, on).await.unwrap();
        books
            .open("Assets:Cash", AccountKind::Asset, on)
            .await
            .unwrap();
        books
            .open("Income:Sales", AccountKind::Income, on)
            .await
            .unwrap();
        books.define_months(2026, 1..=12).await.unwrap();
        let amount: Amount = Amount::from_minor(1_00).unwrap();
        for n in 0..ENTRIES {
            let day = date!("2026-01-01") + time::Duration::days(i64::from(n % 360));
            books
                .record(
                    Entry::new(format!("e-{n}"), day)
                        .debit("Assets:Cash", amount, EUR)
                        .credit("Income:Sales", amount, EUR),
                )
                .await
                .unwrap();
        }
        let journal: Journal = books.to_journal().await.unwrap();
        let current = BalanceQuery::all();
        let at = BalanceQuery::through_position(u64::from(ENTRIES / 2));
        let folded = BalanceQuery::through(date!("2026-06-30"));
        for (name, query, runs) in [
            ("current", &current, 1_000),
            ("position", &at, 1_000),
            ("folded", &folded, 3),
        ] {
            time(&format!("journal balance, {name}"), runs, || {
                journal.balance("Assets:Cash", EUR, query).unwrap()
            });
            time_async(&format!("ledger balance, {name}"), runs, || {
                books.balance("Assets:Cash", EUR, query)
            })
            .await;
        }
        time("journal trial balance, current", 100, || {
            journal.trial_balance(&current).unwrap()
        });
        time("journal trial balance, folded", 3, || {
            journal.trial_balance(&folded).unwrap()
        });
        time("journal export, 100 000 entries", 1, || {
            journal.export(ExportScope::All, None).unwrap().count()
        });
        time_async("ledger export, 100 000 entries", 1, || async {
            let mut export = books.export(ExportScope::All, None).await.unwrap();
            let mut rows = 0;
            while let Some(page) = export.next_page(1_000).await.unwrap() {
                rows += page.len();
            }
            rows
        })
        .await;
    });
}
