//! Cost guards for the in-memory path.
//!
//! The journal maintains balances and per-account posting lists as entries
//! arrive, so appending is amortised constant and reading a balance or a
//! statement costs what the answer costs. Those are claims in the module docs,
//! and the way they break is by someone reintroducing a fold or a clone over
//! something whose size the caller controls — which produces no test failure
//! anywhere else, only a ledger that gets slower the longer it is used.
//!
//! A ledger grows along two axes, so both are guarded:
//!
//! - **More entries.** The classic quadratic append.
//! - **More accounts.** Recording touches two accounts whether the chart holds
//!   ten or ten thousand, so a booking's cost must not move when the chart does.
//!   A per-entry clone of the maintained trial balance is invisible on a
//!   two-account fixture and ruinous on a real one.
//!
//! Both measure a *ratio* rather than a duration, so they say nothing about the
//! machine they run on and everything about the shape of the curve.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use doubleentry::period::LedgerId;
use doubleentry::{
    Amount, BalanceKey, BalanceQuery, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer,
};
use time::macros::date;

type Eur = Amount<2>;

/// Times `entries` appends onto a journal whose chart holds `accounts` accounts,
/// every one of which already carries a balance.
///
/// Seeding every account matters: it is the populated balance rows a per-entry
/// clone would copy, not the registry.
fn append_cost(accounts: usize, entries: usize) -> std::time::Duration {
    let mut journal = Journal::<2>::new(LedgerId::new("scale").unwrap());
    let ids: Vec<_> = (0..accounts)
        .map(|i| {
            journal
                .register_path(&format!("A:{i:06}"), date!(2020 - 01 - 01))
                .unwrap()
        })
        .collect();
    for i in 0..accounts - 1 {
        journal
            .record(
                Entry::new(
                    EntryId::generate(),
                    IdempotencyKey::new(format!("seed{i}").into_bytes()).unwrap(),
                    date!(2026 - 03 - 15),
                )
                .debit(ids[i], Eur::from_minor(1), Currency::EUR)
                .credit(ids[i + 1], Eur::from_minor(1), Currency::EUR),
            )
            .unwrap();
    }

    let start = std::time::Instant::now();
    for i in 0..entries {
        journal
            .record(
                Entry::new(
                    EntryId::generate(),
                    IdempotencyKey::new(format!("k{i}").into_bytes()).unwrap(),
                    date!(2026 - 03 - 15),
                )
                .debit(ids[0], Eur::from_minor(100), Currency::EUR)
                .credit(ids[1], Eur::from_minor(100), Currency::EUR),
            )
            .unwrap();
    }
    let elapsed = start.elapsed();

    let key = BalanceKey {
        account: ids[0],
        currency: Currency::EUR,
        layer: Layer::Settled,
    };
    assert_eq!(
        journal.balance(&key, BalanceQuery::all()).unwrap().debits,
        Eur::from_minor(100 * entries as i64 + 1)
    );
    assert_eq!(
        journal.statement(&key, BalanceQuery::all()).unwrap().len(),
        entries + 1
    );
    elapsed
}

fn ratio(a: std::time::Duration, b: std::time::Duration) -> f64 {
    b.as_secs_f64() / a.as_secs_f64().max(1e-6)
}

#[test]
fn appending_does_not_get_quadratically_slower() {
    let small = append_cost(2, 2_000);
    let large = append_cost(2, 8_000);
    // Four times the entries. Quadratic would be ~16x; allow generous headroom
    // for a debug build and a noisy machine, but not 16x.
    let r = ratio(small, large);
    assert!(
        r < 9.0,
        "appending 4x the entries took {r:.1}x the time ({small:?} -> {large:?})"
    );
}

#[test]
fn appending_does_not_get_slower_as_the_chart_of_accounts_grows() {
    let narrow = append_cost(50, 2_000);
    let wide = append_cost(5_000, 2_000);
    // A hundred times the accounts, the same entries, touching the same two
    // accounts. A booking costs the postings it carries, so this ratio is about
    // one; the headroom below is for scheduler noise.
    let r = ratio(narrow, wide);
    assert!(
        r < 4.0,
        "100x the accounts made an append {r:.1}x more expensive ({narrow:?} -> {wide:?})"
    );
}
