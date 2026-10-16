//! Rule-core timings, recorded and not gated: `audit` of 100 000
//! two-posting entries, and `record` on one hot account.
//!
//! `cargo bench --bench core`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::time::Instant;

use doubleentry::{AccountKind, Amount, Entry, Journal, LedgerId, MemoSalt, currency::EUR, date};

fn books(entries: u32) -> Journal {
    let mut books = Journal::new(LedgerId::new("example.com/bench").unwrap())
        .with_salt_source(|| MemoSalt([1; 16]));
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026", date!("2026-01-01"), date!("2026-12-31"))
        .unwrap();
    let amount: Amount = Amount::from_minor(1_00).unwrap();
    let started = Instant::now();
    for n in 0..entries {
        books
            .record(
                Entry::new(format!("e-{n}"), date!("2026-06-15"))
                    .debit("Assets:Cash", amount, EUR)
                    .credit("Income:Sales", amount, EUR),
            )
            .unwrap();
    }
    let elapsed = started.elapsed();
    println!(
        "record: {entries} entries on one hot account in {elapsed:?} ({:.0} entries/s)",
        f64::from(entries) / elapsed.as_secs_f64()
    );
    books
}

fn main() {
    let books = books(100_000);
    let started = Instant::now();
    let report = books.audit().unwrap();
    let elapsed = started.elapsed();
    println!(
        "audit: {} records replayed in {elapsed:?} ({:.0} records/s; target < 10 s)",
        report.size,
        report.size as f64 / elapsed.as_secs_f64()
    );
}
