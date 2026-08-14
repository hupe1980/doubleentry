//! Cost guards for the in-memory path.
//!
//! The journal maintains balances and per-account posting lists as entries
//! arrive, so appending is amortised constant and reading a balance or a
//! statement costs what the answer costs. Those are claims in the module docs,
//! and the way they break is by someone reintroducing a fold or a clone over the
//! whole journal — which produces no test failure anywhere else, only a ledger
//! that gets slower the longer it is used.
//!
//! This measures a ratio rather than a duration, so it says nothing about the
//! machine it runs on and everything about the shape of the curve.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]

use doubleentry::period::LedgerId;
use doubleentry::{Amount, BalanceKey, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
use time::macros::date;

type Eur = Amount<2>;

fn build(n: usize) -> std::time::Duration {
    let mut j = Journal::<2>::new(LedgerId::new("scale").unwrap());
    let a = j
        .accounts_mut()
        .register_path("A:One", date!(2020 - 01 - 01))
        .unwrap();
    let b = j
        .accounts_mut()
        .register_path("B:Two", date!(2020 - 01 - 01))
        .unwrap();
    let start = std::time::Instant::now();
    for i in 0..n {
        j.record(
            Entry::new(
                EntryId::generate(),
                IdempotencyKey::new(format!("k{i}").into_bytes()).unwrap(),
                date!(2026 - 03 - 15),
            )
            .debit(a, Eur::from_minor(100), Currency::EUR)
            .credit(b, Eur::from_minor(100), Currency::EUR),
        )
        .unwrap();
    }
    let elapsed = start.elapsed();
    let key = BalanceKey {
        account: a,
        currency: Currency::EUR,
        layer: Layer::Settled,
    };
    assert_eq!(
        j.balance(&key, None).unwrap().debits,
        Eur::from_minor(100 * n as i64)
    );
    assert_eq!(j.statement(&key).unwrap().len(), n);
    elapsed
}

#[test]
fn appending_does_not_get_quadratically_slower() {
    let small = build(2_000);
    let large = build(8_000);
    // Four times the entries. Quadratic would be ~16x; allow generous headroom
    // for a debug build and a noisy machine, but not 16x.
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 9.0,
        "appending 4x the entries took {ratio:.1}x the time ({small:?} -> {large:?})"
    );
}
