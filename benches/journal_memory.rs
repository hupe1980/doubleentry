//! `Journal` memory, recorded and not gated: the
//! net heap (bytes allocated − bytes deallocated, reallocations included)
//! that a `Journal` holds after loading 10⁵ and 10⁶ entries, total and per
//! entry, for two representative shapes:
//!
//! | id | shape |
//! |---|---|
//! | M1 | 2-posting entry (debit, credit), no dimensions, no memo |
//! | M2 | 6-posting entry (three debits, three credits), every posting with two dimensions (`region`, `channel`) |
//!
//! Counted by `stats_alloc`'s instrumented global allocator (a
//! dev-dependency, so the crate's `unsafe_code = "forbid"` holds here too).
//! The figures cover the whole `Journal` — log, tree, entries, balances,
//! indexes — and grow linearly: `Journal` keeps everything and evicts
//! nothing. Deterministic: fixed keys, dates, amounts and memo salt.
//!
//! `just bench journal_memory`, i.e.
//! `cargo bench --locked --all-features --bench journal_memory`;
//! `DOUBLEENTRY_BENCH_ENTRIES=<n>` replaces the sizes with `n` (smoke runs).
//! The bench runs when no filter is given or the filter names it
//! (`journal`), not for the store filters `memory`, `sqlite`, `postgres`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::alloc::System;
use std::time::Instant;

use doubleentry::{
    AccountKind, Amount, Entry, Journal, LedgerId, MemoSalt, Posting, currency::EUR, date,
};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// The drafts per `record_batch` call while loading.
const BATCH: u64 = 1_000;

#[derive(Clone, Copy)]
enum Shape {
    /// M1: 2-posting.
    Two,
    /// M2: 6-posting, two dimensions per posting.
    Six,
}

impl Shape {
    fn id(self) -> &'static str {
        match self {
            Self::Two => "M1 2-posting",
            Self::Six => "M2 6-posting + 2 dimensions",
        }
    }

    fn entry(self, n: u64) -> Entry {
        let amount: Amount = Amount::from_minor(1_00).unwrap();
        let entry = Entry::new(format!("e-{n}"), date!("2026-06-15"));
        match self {
            Self::Two => entry
                .debit(
                    if n.is_multiple_of(2) {
                        "Assets:Cash"
                    } else {
                        "Assets:Bank"
                    },
                    amount,
                    EUR,
                )
                .credit("Income:Sales", amount, EUR),
            Self::Six => {
                let regions = ["eu", "us", "apac"];
                let channels = ["web", "store"];
                let mut entry = entry;
                for (i, (debit, credit)) in [
                    ("Assets:Cash", "Income:Sales"),
                    ("Assets:Bank", "Income:Fees"),
                    ("Expenses:Rent", "Income:Other"),
                ]
                .into_iter()
                .enumerate()
                {
                    let region = regions[(n as usize + i) % regions.len()];
                    let channel = channels[(n as usize + i) % channels.len()];
                    entry = entry
                        .posting(
                            Posting::debit(debit, amount, EUR)
                                .dimension("region", region)
                                .dimension("channel", channel),
                        )
                        .posting(
                            Posting::credit(credit, amount, EUR)
                                .dimension("region", region)
                                .dimension("channel", channel),
                        );
                }
                entry
            }
        }
    }
}

/// A `Journal` with EUR, six accounts, the months of 2026 and `entries`
/// entries of `shape`.
fn load(shape: Shape, entries: u64) -> Journal {
    let mut books = Journal::new(LedgerId::new("example.com/bench").unwrap())
        .with_salt_source(|| MemoSalt([1; 16]));
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Assets:Bank", AccountKind::Asset),
        ("Expenses:Rent", AccountKind::Expense),
        ("Income:Sales", AccountKind::Income),
        ("Income:Fees", AccountKind::Income),
        ("Income:Other", AccountKind::Income),
    ] {
        books.open(path, kind, on).unwrap();
    }
    books.define_months(2026, 1..=12).unwrap();
    let mut n = 0;
    while n < entries {
        let size = BATCH.min(entries - n);
        books
            .record_batch((n..n + size).map(|n| shape.entry(n)))
            .unwrap();
        n += size;
    }
    books
}

/// The first argument that is not a flag: the filter `cargo bench` passes
/// through (`--bench`, criterion's own flags and their values start with
/// `-` or follow one, and none of them is given to this bench).
fn filter() -> Option<String> {
    std::env::args().skip(1).find(|arg| !arg.starts_with('-'))
}

fn sizes() -> Vec<u64> {
    match std::env::var("DOUBLEENTRY_BENCH_ENTRIES") {
        Ok(n) => vec![n.parse().expect("DOUBLEENTRY_BENCH_ENTRIES: a count")],
        Err(_) => vec![100_000, 1_000_000],
    }
}

fn main() {
    if let Some(filter) = filter()
        && !filter.contains("journal")
    {
        return;
    }
    println!("Journal net heap after loading (bytes allocated − deallocated):");
    println!(
        "{:<30} {:>10} {:>16} {:>12} {:>10}",
        "shape", "entries", "total bytes", "bytes/entry", "load time"
    );
    for shape in [Shape::Two, Shape::Six] {
        for entries in sizes() {
            let region = Region::new(GLOBAL);
            let started = Instant::now();
            let books = load(shape, entries);
            let elapsed = started.elapsed();
            let change = region.change();
            let net = change.bytes_allocated as i128 - change.bytes_deallocated as i128;
            // Genesis, EUR, six accounts and twelve months precede the entries.
            assert_eq!(books.head().size, entries + 20, "every entry recorded");
            println!(
                "{:<30} {:>10} {:>16} {:>12.0} {:>10.1?}",
                shape.id(),
                entries,
                net,
                net as f64 / entries as f64,
                elapsed
            );
            drop(books);
        }
    }
}
