//! Randomised operation sequences, with the engine's invariants checked after
//! every step.
//!
//! Unit tests check the cases someone thought of. This drives long, arbitrary
//! sequences of appends, replays, reversals, clearings, resets and seals through
//! a store, and after *each* step asserts everything the engine claims: the log
//! agrees with its entries, every entry is provable, debits equal credits,
//! residuals never go negative, and clearing never moves money.
//!
//! Every sequence is generated from a seed, so a failure reproduces exactly —
//! which is what the deterministic kernel is for.
//!
//! Also included: no input, however malformed, may panic a parser. The engine
//! reads bytes it did not write, and a panic on hostile input is a denial of
//! service in something that is supposed to be a system of record.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::account::AccountRegistry;
use doubleentry::clearing::{ClearedItem, Clearing, ClearingId};
use doubleentry::entry::{Draft, LedgerPolicy, SealContext};
use doubleentry::period::{LedgerId, Period, PeriodCalendar, PeriodId, PeriodState};
use doubleentry::{
    AccountId, AccountPath, ActivityId, Amount, BalanceKey, Currency, Description, Entry, EntryId,
    Hash, IdempotencyKey, Journal, JournalError, Layer,
};
use proptest::prelude::*;
use time::macros::date;

/// The ledger these tests keep their books in.
fn test_ledger() -> LedgerId {
    LedgerId::new("test-ledger").expect("valid")
}

type Eur = Amount<2>;

// ── the model ────────────────────────────────────────────────────────────────

/// One step in a generated sequence.
#[derive(Debug, Clone)]
enum Op {
    /// Append a fresh balanced entry.
    Append { amount: i64 },
    /// Re-append an earlier entry verbatim. Must be a no-op.
    Replay { which: usize },
    /// Reverse an earlier entry.
    Reverse { which: usize },
    /// Clear two postings against each other.
    Clear {
        debit: usize,
        credit: usize,
        amount: i64,
    },
    /// Release an earlier clearing.
    ResetClearing { which: usize },
    /// Close and seal the current period.
    SealPeriod,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1i64..1_000_000).prop_map(|amount| Op::Append { amount }),
        2 => (0usize..32).prop_map(|which| Op::Replay { which }),
        2 => (0usize..32).prop_map(|which| Op::Reverse { which }),
        3 => (0usize..32, 0usize..32, 1i64..1_000_000)
            .prop_map(|(debit, credit, amount)| Op::Clear { debit, credit, amount }),
        1 => (0usize..16).prop_map(|which| Op::ResetClearing { which }),
        1 => Just(Op::SealPeriod),
    ]
}

struct World {
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,
    left: AccountId,
    right: AccountId,
    journal: Journal<2>,
    /// Entries recorded so far, in order, with the key used for each.
    recorded: Vec<(EntryId, Vec<u8>, i64)>,
    /// Clearings recorded so far.
    clearings: Vec<ClearingId>,
    /// Periods already sealed.
    sealed: usize,
    next_key: u64,
}

impl World {
    fn new() -> Self {
        let mut accounts = AccountRegistry::new();
        let left = accounts
            .register_path("Sim:Left", date!(2000 - 01 - 01))
            .expect("registers");
        let right = accounts
            .register_path("Sim:Right", date!(2000 - 01 - 01))
            .expect("registers");
        Self {
            accounts,
            calendar: PeriodCalendar::new(),
            policy: LedgerPolicy::default(),
            left,
            right,
            journal: Journal::new(test_ledger()),
            recorded: Vec::new(),
            clearings: Vec::new(),
            sealed: 0,
            next_key: 0,
        }
    }

    fn ctx(&self) -> SealContext<'_> {
        SealContext {
            accounts: &self.accounts,
            calendar: &self.calendar,
            policy: &self.policy,
        }
    }

    fn key(&mut self) -> Vec<u8> {
        self.next_key += 1;
        format!("sim-{}", self.next_key).into_bytes()
    }

    fn build(&self, key: &[u8], amount: i64) -> Option<Entry<doubleentry::Balanced, 2>> {
        Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).ok()?,
            date!(2026 - 06 - 15),
        )
        .debit(self.left, Eur::from_minor(amount), Currency::EUR)
        .credit(self.right, Eur::from_minor(amount), Currency::EUR)
        .seal(&self.ctx())
        .ok()
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Append { amount } => {
                let key = self.key();
                if let Some(entry) = self.build(&key, *amount) {
                    let id = entry.id();
                    if self.journal.record(entry).is_ok() {
                        self.recorded.push((id, key, *amount));
                    }
                }
            }

            Op::Replay { which } => {
                if self.recorded.is_empty() {
                    return;
                }
                let (_, key, amount) = self.recorded[*which % self.recorded.len()].clone();
                let before = self.journal.len();
                if let Some(entry) = self.build(&key, amount) {
                    let outcome = self.journal.record(entry);
                    // A verbatim replay is always safe and never appends.
                    assert!(outcome.is_ok(), "a verbatim replay was rejected");
                    assert!(
                        !outcome.expect("checked").is_new,
                        "a replay was treated as new"
                    );
                }
                assert_eq!(
                    self.journal.len(),
                    before,
                    "a replay changed the log length"
                );
            }

            Op::Reverse { which } => {
                if self.recorded.is_empty() {
                    return;
                }
                let (id, _, _) = self.recorded[*which % self.recorded.len()];
                let Some(original) = self.journal.get(id).cloned() else {
                    return;
                };
                let key = self.key();
                let Ok(k) = IdempotencyKey::new(key.clone()) else {
                    return;
                };
                let draft = original.reverse(EntryId::generate(), k, date!(2026 - 06 - 20));
                let Ok(reversal) = draft.seal(&self.ctx()) else {
                    return;
                };
                let reversal_id = reversal.id();
                match self.journal.record(reversal) {
                    Ok(_) => self.recorded.push((reversal_id, key, 0)),
                    // The only legitimate refusals are the correction rules.
                    Err(
                        JournalError::AlreadyReversed { .. }
                        | JournalError::ReversalOfReversal { .. },
                    ) => {}
                    Err(e) => panic!("unexpected reversal failure: {e}"),
                }
            }

            Op::Clear {
                debit,
                credit,
                amount,
            } => {
                let key = BalanceKey {
                    account: self.left,
                    currency: Currency::EUR,
                    layer: Layer::Settled,
                };
                let open = self.journal.open_items(&key).expect("no overflow");
                let debits: Vec<_> = open
                    .iter()
                    .filter(|i| i.direction.is_debit())
                    .copied()
                    .collect();
                let credits: Vec<_> = open
                    .iter()
                    .filter(|i| i.direction.is_credit())
                    .copied()
                    .collect();
                if debits.is_empty() || credits.is_empty() {
                    return;
                }
                let d = debits[*debit % debits.len()];
                let c = credits[*credit % credits.len()];
                let applied = (*amount)
                    .min(d.residual.to_minor())
                    .min(c.residual.to_minor());
                if applied <= 0 {
                    return;
                }
                let id = ClearingId::generate();
                let outcome = self.journal.clear(Clearing {
                    id,
                    account: self.left,
                    currency: Currency::EUR,
                    cleared_on: date!(2026 - 06 - 25),
                    items: vec![
                        ClearedItem {
                            posting: d.posting,
                            applied: Eur::from_minor(applied),
                        },
                        ClearedItem {
                            posting: c.posting,
                            applied: Eur::from_minor(applied),
                        },
                    ],
                });
                assert!(
                    outcome.is_ok(),
                    "a clearing within both residuals was refused: {outcome:?}"
                );
                self.clearings.push(id);
            }

            Op::ResetClearing { which } => {
                if self.clearings.is_empty() {
                    return;
                }
                let id = self.clearings[*which % self.clearings.len()];
                // Already-reset is the only legitimate refusal.
                let _ = self.journal.reset_clearing(id, date!(2026 - 07 - 01));
            }

            Op::SealPeriod => {
                let name = format!("2026-{:02}", self.sealed + 1);
                let Ok(id) = PeriodId::new(name) else {
                    return;
                };
                // Seal historic months so live postings stay in an open window.
                let month = u8::try_from(self.sealed + 1).unwrap_or(1).clamp(1, 5);
                let Ok(start) = time::Date::from_calendar_date(
                    2026,
                    time::Month::try_from(month).unwrap_or(time::Month::January),
                    1,
                ) else {
                    return;
                };
                let Ok(end) = time::Date::from_calendar_date(
                    2026,
                    time::Month::try_from(month).unwrap_or(time::Month::January),
                    28,
                ) else {
                    return;
                };
                let Ok(period) = Period::new(id.clone(), start, end) else {
                    return;
                };
                if self.calendar.define(period).is_err() {
                    return;
                }
                if self.calendar.transition(&id, PeriodState::Closing).is_err() {
                    return;
                }
                if self.journal.seal_period(&id, &mut self.calendar).is_ok() {
                    self.sealed += 1;
                }
            }
        }
    }

    /// Everything the engine claims, checked after every step.
    fn check_invariants(&self) {
        assert!(
            self.journal.verify_balanced().expect("no overflow"),
            "debits and credits diverged"
        );
        assert!(
            self.journal.verify_log(),
            "the Merkle log stopped agreeing with the entries"
        );
        assert!(self.journal.verify_seals().is_ok(), "the seal chain broke");

        // Every entry is provable under the current head.
        let head = self.journal.head();
        assert_eq!(head.size, self.journal.len() as u64);
        for (i, entry) in self.journal.entries().iter().enumerate() {
            let proof = self
                .journal
                .prove_inclusion(doubleentry::LogIndex::new(i as u64))
                .expect("in range");
            assert!(
                proof.verify(&entry.content_hash(), &head.root),
                "entry {i} became unprovable"
            );
        }

        // Residuals are bounded by the postings they belong to.
        for account in [self.left, self.right] {
            let key = BalanceKey {
                account,
                currency: Currency::EUR,
                layer: Layer::Settled,
            };
            for item in self.journal.open_items(&key).expect("no overflow") {
                assert!(
                    !item.residual.is_negative(),
                    "a residual went negative: {item:?}"
                );
                assert!(
                    item.residual <= item.original,
                    "a residual exceeded its posting: {item:?}"
                );
                assert_eq!(
                    item.applied.checked_add(item.residual).expect("fits"),
                    item.original,
                    "applied plus residual must equal the posting"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// No sequence of operations can break an invariant.
    #[test]
    fn invariants_hold_under_arbitrary_operation_sequences(
        ops in prop::collection::vec(op_strategy(), 1..60),
    ) {
        let mut world = World::new();
        world.check_invariants();
        for op in &ops {
            world.apply(op);
            world.check_invariants();
        }

        // Clearing is an assignment, so it never changes a balance: the trial
        // balance must equal one folded from the entries alone.
        let mut expected = doubleentry::TrialBalance::<2>::new();
        for entry in world.journal.entries() {
            for posting in entry.postings() {
                expected.apply(posting).expect("no overflow");
            }
        }
        prop_assert_eq!(world.journal.trial_balance(None).expect("ok"), expected);
    }
}

// ── parser robustness ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// No string parses into a panic.
    #[test]
    fn parsers_never_panic_on_arbitrary_text(s in ".{0,80}") {
        let _ = Eur::parse(&s);
        let _ = Amount::<0>::parse(&s);
        let _ = Amount::<5>::parse(&s);
        let _ = Hash::parse_hex(&s);
        let _ = IdempotencyKey::parse_hex(&s);
        let _ = AccountPath::parse(&s);
        let _ = Currency::new(&s);
        let _ = ActivityId::new(s.clone());
        let _ = Description::new(s.clone());
        let _ = PeriodId::new(s);
    }

    /// Nor does any byte string that happens to be valid UTF-8.
    #[test]
    fn parsers_never_panic_on_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..80)) {
        let _ = IdempotencyKey::new(bytes.clone());
        if let Ok(s) = std::str::from_utf8(&bytes) {
            let _ = Eur::parse(s);
            let _ = Hash::parse_hex(s);
            let _ = AccountPath::parse(s);
        }
    }

    /// A hex round trip is exact for any key within the length bound.
    #[test]
    fn idempotency_keys_round_trip_through_hex(
        bytes in prop::collection::vec(any::<u8>(), 1..128),
    ) {
        let key = IdempotencyKey::new(bytes).expect("within bounds");
        let restored = IdempotencyKey::parse_hex(&key.to_hex()).expect("round trips");
        prop_assert_eq!(restored, key);
    }
}

#[cfg(feature = "serde")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Deserialising arbitrary JSON fails cleanly rather than panicking.
    #[test]
    fn deserialisation_never_panics(s in ".{0,60}") {
        let quoted = format!("\"{}\"", s.replace(['\\', '"'], ""));
        let _ = serde_json::from_str::<Eur>(&quoted);
        let _ = serde_json::from_str::<Currency>(&quoted);
        let _ = serde_json::from_str::<Hash>(&quoted);
        let _ = serde_json::from_str::<AccountPath>(&quoted);
        let _ = serde_json::from_str::<ActivityId>(&quoted);
        let _ = serde_json::from_str::<IdempotencyKey>(&quoted);
        let _ = serde_json::from_str::<Entry<Draft, 2>>(&s);
    }
}
