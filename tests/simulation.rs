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

use doubleentry::account::BalanceLimit;
use doubleentry::clearing::{Clearing, ClearingId};
use doubleentry::entry::Draft;
use doubleentry::period::{LedgerId, Period, PeriodId, PeriodState};
use doubleentry::{
    AccountId, AccountPath, Amount, BalanceKey, BalanceQuery, Currency, Description, Entry,
    EntryId, Hash, IdempotencyKey, Journal, JournalError, Label, Layer,
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
    /// Append an entry backdated into an earlier month.
    ///
    /// The operation that could otherwise restate a sealed period: a legal
    /// booking whose date falls in a month a later seal already closed over,
    /// including months the calendar never defined as periods.
    AppendBackdated { amount: i64, month: u8, day: u8 },
    /// Fund the limited account, or try to draw against it — settled, or as a
    /// reservation in the pending layer. Both draw on the same headroom.
    MoveLimited {
        amount: i64,
        into: bool,
        reserve: bool,
    },
    /// Impose or lift the limited account's balance limit.
    SetLimit { on: bool },
    /// Reserve an amount: a balanced pair in the **pending** layer.
    Hold { amount: i64 },
    /// Discharge an earlier hold: release it in the pending layer and move the
    /// money in the settled one, as one atomic entry.
    Settle { which: usize },
    /// Try to book an entry that only balances by netting one layer against the
    /// other. Must always be refused.
    NetAcrossLayers { amount: i64 },
    /// Append an entry whose value date sits away from its booking date.
    ///
    /// The straddling booking — in one month's books and another's cash. It is
    /// what separates the two date bases, so without it every value-dated
    /// assertion below would check the booking-dated answer twice.
    AppendStraddling { amount: i64, drift: i8 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1i64..1_000_000).prop_map(|amount| Op::Append { amount }),
        2 => (0usize..32).prop_map(|which| Op::Replay { which }),
        2 => (0usize..32).prop_map(|which| Op::Reverse { which }),
        3 => (0usize..32, 0usize..32, 1i64..1_000_000)
            .prop_map(|(debit, credit, amount)| Op::Clear { debit, credit, amount }),
        1 => (0usize..16).prop_map(|which| Op::ResetClearing { which }),
        // Weighted up: the interesting failures need *two* seals and then a
        // backdated write, so a sequence that seals once proves little.
        3 => Just(Op::SealPeriod),
        // Days past the 28th matter: the sealed months are defined as 1–28, so
        // 29–31 fall in a gap the calendar never covers. That gap reporting
        // `Open` forever was half the defect — a booking there restates a
        // sealed month without ever touching a defined period.
        4 => (1i64..1_000_000, 1u8..7, 1u8..32)
            .prop_map(|(amount, month, day)| Op::AppendBackdated { amount, month, day }),
        4 => (1i64..1_000_000, any::<bool>(), any::<bool>())
            .prop_map(|(amount, into, reserve)| Op::MoveLimited { amount, into, reserve }),
        1 => any::<bool>().prop_map(|on| Op::SetLimit { on }),
        // Two-phase money. The pending layer is a second set of balances over
        // the same accounts, and every rule that keys on `(account, currency,
        // layer)` has to hold in it independently.
        3 => (1i64..1_000_000).prop_map(|amount| Op::Hold { amount }),
        3 => (0usize..32).prop_map(|which| Op::Settle { which }),
        2 => (1i64..1_000_000).prop_map(|amount| Op::NetAcrossLayers { amount }),
        // Drift in both directions and across the month boundary the statement
        // window below is scoped to, so the two bases genuinely disagree.
        3 => (1i64..1_000_000, -40i8..40)
            .prop_map(|(amount, drift)| Op::AppendStraddling { amount, drift }),
    ]
}

struct World {
    left: AccountId,
    right: AccountId,
    /// An account the sequence may constrain and then try to overdraw.
    limited: AccountId,
    journal: Journal<2>,
    /// Plain appends, with the key and amount that reproduce each. Only these
    /// are replayable: a reversal is not rebuildable from a key and an amount.
    recorded: Vec<(EntryId, Vec<u8>, i64)>,
    /// Every entry identifier, appends and reversals alike — the candidates a
    /// reversal may target.
    all_ids: Vec<EntryId>,
    /// Clearings recorded so far.
    clearings: Vec<ClearingId>,
    /// Holds placed and not yet discharged, as the amounts they reserved.
    holds: Vec<i64>,
    /// Periods already sealed.
    sealed: usize,
    next_key: u64,
}

impl World {
    fn new() -> Self {
        let mut journal = Journal::<2>::new(test_ledger());
        let left = journal
            .register_path("Sim:Left", date!(2000 - 01 - 01))
            .expect("registers");
        let right = journal
            .register_path("Sim:Right", date!(2000 - 01 - 01))
            .expect("registers");
        let limited = journal
            .register_path("Sim:Limited", date!(2000 - 01 - 01))
            .expect("registers");
        Self {
            left,
            right,
            limited,
            journal,
            recorded: Vec::new(),
            all_ids: Vec::new(),
            clearings: Vec::new(),
            holds: Vec::new(),
            sealed: 0,
            next_key: 0,
        }
    }

    fn key(&mut self) -> Vec<u8> {
        self.next_key += 1;
        format!("sim-{}", self.next_key).into_bytes()
    }

    /// The limited account's settled and pending balances — both, because a
    /// limit is one question about the two together.
    fn limited_balances(&self) -> (doubleentry::Balance<2>, doubleentry::Balance<2>) {
        let at = |layer| {
            self.journal
                .balance(
                    &BalanceKey {
                        account: self.limited,
                        currency: Currency::EUR,
                        layer,
                    },
                    BalanceQuery::all(),
                )
                .expect("no overflow")
        };
        (at(Layer::Settled), at(Layer::Pending))
    }

    fn build(&self, key: &[u8], amount: i64) -> Option<Entry<Draft, 2>> {
        self.build_on(key, amount, date!(2026 - 06 - 15))
    }

    fn build_on(&self, key: &[u8], amount: i64, on: time::Date) -> Option<Entry<Draft, 2>> {
        Some(
            Entry::<Draft, 2>::new(
                EntryId::generate(),
                IdempotencyKey::new(key.to_vec()).ok()?,
                on,
            )
            .debit(self.left, Eur::from_minor(amount), Currency::EUR)
            .credit(self.right, Eur::from_minor(amount), Currency::EUR),
        )
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Append { amount } => {
                let key = self.key();
                if let Some(entry) = self.build(&key, *amount) {
                    let id = entry.id();
                    if self.journal.record(entry).is_ok() {
                        self.recorded.push((id, key, *amount));
                        self.all_ids.push(id);
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
                if self.all_ids.is_empty() {
                    return;
                }
                let id = self.all_ids[*which % self.all_ids.len()];
                let Some(original) = self.journal.get(id).cloned() else {
                    return;
                };
                let key = self.key();
                let Ok(k) = IdempotencyKey::new(key.clone()) else {
                    return;
                };
                let draft = original.reverse(EntryId::generate(), k, date!(2026 - 06 - 20));
                let reversal_id = draft.id();
                match self.journal.record(draft) {
                    Ok(_) => self.all_ids.push(reversal_id),
                    // The legitimate refusals are the correction rules, a
                    // booking date whose period has since been sealed, and a
                    // balance limit: reversing a funding entry withdraws money
                    // the account may since have committed, and a limit is a
                    // rule about the resulting balance, not about intent.
                    Err(
                        JournalError::AlreadyReversed { .. }
                        | JournalError::ReversalOfReversal { .. }
                        | JournalError::LimitBreached { .. }
                        | JournalError::Invalid(_),
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
                let outcome = self.journal.clear(
                    Clearing::new(id, key, date!(2026 - 06 - 25))
                        .apply(d.posting, Eur::from_minor(applied))
                        .apply(c.posting, Eur::from_minor(applied)),
                );
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

            Op::MoveLimited {
                amount,
                into,
                reserve,
            } => {
                let key = self.key();
                let Ok(k) = IdempotencyKey::new(key) else {
                    return;
                };
                let amount = Eur::from_minor(*amount);
                let layer = if *reserve {
                    Layer::Pending
                } else {
                    Layer::Settled
                };
                let (debit, credit) = if *into {
                    (self.limited, self.right)
                } else {
                    (self.right, self.limited)
                };
                let draft = Entry::<Draft, 2>::new(EntryId::generate(), k, date!(2026 - 06 - 15))
                    .post(doubleentry::Posting::debit(debit, amount, Currency::EUR).in_layer(layer))
                    .post(
                        doubleentry::Posting::credit(credit, amount, Currency::EUR).in_layer(layer),
                    );
                let id = draft.id();
                let limit = self.journal.accounts().limit_of(self.limited);
                match self.journal.record(draft) {
                    Ok(_) => {
                        self.all_ids.push(id);
                        // The rule the engine actually promises: an entry that
                        // is *accepted* leaves the account satisfying whatever
                        // limit was in force when it was accepted. Asserting it
                        // here rather than after every step is what makes it
                        // exact — a limit imposed later on an account already
                        // past it is legal and does not retroactively falsify
                        // anything that was booked before.
                        let (settled, pending) = self.limited_balances();
                        assert!(
                            limit.permits(&settled, &pending),
                            "an accepted entry breached a {limit} limit: \
                             settled {settled:?}, pending {pending:?}"
                        );
                    }
                    // The limit and a sealed period are the only refusals a
                    // freshly built, balanced entry can legitimately draw.
                    Err(JournalError::LimitBreached { .. } | JournalError::Invalid(_)) => {}
                    Err(e) => panic!("unexpected failure moving the limited account: {e}"),
                }
            }

            Op::SetLimit { on } => {
                let limit = if *on {
                    BalanceLimit::NoCreditBalance
                } else {
                    BalanceLimit::Unlimited
                };
                // Imposing a limit is master data and always succeeds, even on
                // an account already past it: it governs the next booking only.
                self.journal
                    .set_account_limit(self.limited, limit)
                    .expect("the account is registered");
            }

            Op::Hold { amount } => {
                let key = self.key();
                let Ok(idem) = IdempotencyKey::new(key) else {
                    return;
                };
                let draft =
                    Entry::<Draft, 2>::new(EntryId::generate(), idem, date!(2026 - 06 - 15))
                        .post(
                            doubleentry::Posting::debit(
                                self.left,
                                Eur::from_minor(*amount),
                                Currency::EUR,
                            )
                            .in_layer(Layer::Pending),
                        )
                        .post(
                            doubleentry::Posting::credit(
                                self.right,
                                Eur::from_minor(*amount),
                                Currency::EUR,
                            )
                            .in_layer(Layer::Pending),
                        );
                if self.journal.record(draft).is_ok() {
                    self.holds.push(*amount);
                }
            }

            Op::Settle { which } => {
                if self.holds.is_empty() {
                    return;
                }
                let index = *which % self.holds.len();
                let amount = self.holds[index];
                let key = self.key();
                let Ok(idem) = IdempotencyKey::new(key) else {
                    return;
                };
                // One entry, four postings: the hold comes off in the pending
                // layer and the money moves in the settled one. Each layer
                // balances on its own, which is what validation requires.
                let draft =
                    Entry::<Draft, 2>::new(EntryId::generate(), idem, date!(2026 - 06 - 15))
                        .post(
                            doubleentry::Posting::credit(
                                self.left,
                                Eur::from_minor(amount),
                                Currency::EUR,
                            )
                            .in_layer(Layer::Pending),
                        )
                        .post(
                            doubleentry::Posting::debit(
                                self.right,
                                Eur::from_minor(amount),
                                Currency::EUR,
                            )
                            .in_layer(Layer::Pending),
                        )
                        .debit(self.left, Eur::from_minor(amount), Currency::EUR)
                        .credit(self.right, Eur::from_minor(amount), Currency::EUR);
                if self.journal.record(draft).is_ok() {
                    self.holds.remove(index);
                }
            }

            Op::NetAcrossLayers { amount } => {
                let key = self.key();
                let Ok(idem) = IdempotencyKey::new(key) else {
                    return;
                };
                let before = self.journal.len();
                let draft =
                    Entry::<Draft, 2>::new(EntryId::generate(), idem, date!(2026 - 06 - 15))
                        .post(
                            doubleentry::Posting::debit(
                                self.left,
                                Eur::from_minor(*amount),
                                Currency::EUR,
                            )
                            .in_layer(Layer::Pending),
                        )
                        .credit(self.right, Eur::from_minor(*amount), Currency::EUR);

                // It nets to zero in EUR and leaves both layers out of balance.
                // Accepting it would falsify `verify_balanced` permanently, and
                // an append-only log has no way back.
                assert!(
                    matches!(self.journal.record(draft), Err(JournalError::Invalid(_))),
                    "an entry that only balances across layers was accepted"
                );
                assert_eq!(self.journal.len(), before, "a refused entry still appended");
            }

            Op::AppendStraddling { amount, drift } => {
                let booked = date!(2026 - 06 - 15);
                let Some(valued) = booked.checked_add(time::Duration::days(i64::from(*drift)))
                else {
                    return;
                };
                let key = self.key();
                let Some(entry) = self.build_on(&key, *amount, booked) else {
                    return;
                };
                let entry = entry.with_value_date(valued);
                let id = entry.id();
                if self.journal.record(entry).is_ok() {
                    self.all_ids.push(id);
                }
            }

            Op::AppendBackdated { amount, month, day } => {
                let Ok(month) = time::Month::try_from(*month) else {
                    return;
                };
                let Ok(on) = time::Date::from_calendar_date(2026, month, *day) else {
                    return;
                };
                let key = self.key();
                if let Some(entry) = self.build_on(&key, *amount, on) {
                    let id = entry.id();
                    // Refused is the expected answer once the books are sealed
                    // through that date; accepted is fine while they are not.
                    if self.journal.record(entry).is_ok() {
                        self.all_ids.push(id);
                    }
                }
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
                if self.journal.define_period(period).is_err() {
                    return;
                }
                if self
                    .journal
                    .transition_period(&id, PeriodState::Closing)
                    .is_err()
                {
                    return;
                }
                if self.journal.seal_period(&id).is_ok() {
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
        assert!(
            self.journal.verify_balances().expect("no overflow"),
            "the maintained balances drifted from the entries"
        );

        // Every entry is provable under the current head.
        let head = self.journal.head();
        assert_eq!(head.size, self.journal.len() as u64);
        for (i, entry) in self.journal.entries().iter().enumerate() {
            let proof = self
                .journal
                .prove_inclusion(doubleentry::LogIndex::new(i as u64))
                .expect("in range");
            assert!(
                proof.verify(&entry.content_hash(), &head),
                "entry {i} became unprovable"
            );
        }

        // The model's view of the pending layer must match the journal's. A
        // hold reserves without moving, and discharging it releases exactly what
        // it reserved — so the outstanding reservations are the pending net,
        // and `verify_balanced` above has already said each layer balances on
        // its own.
        let outstanding: i64 = self.holds.iter().sum();
        let pending = self
            .journal
            .balance(
                &BalanceKey {
                    account: self.left,
                    currency: Currency::EUR,
                    layer: Layer::Pending,
                },
                BalanceQuery::all(),
            )
            .expect("no overflow");
        assert_eq!(
            pending.signed_net().expect("no overflow"),
            Eur::from_minor(outstanding),
            "the pending layer disagrees with the holds still outstanding"
        );

        // A checkpoint taken now must still verify now, and one taken over the
        // empty prefix must verify however far the log has grown.
        for account in [self.left, self.limited] {
            let key = BalanceKey {
                account,
                currency: Currency::EUR,
                layer: Layer::Settled,
            };
            let checkpoint = self.journal.checkpoint(&key).expect("no overflow");
            assert_eq!(checkpoint.size(), self.journal.len() as u64);
            self.journal
                .verify_checkpoint(&checkpoint)
                .expect("a checkpoint taken from the journal must re-derive");

            let empty = doubleentry::Checkpoint::<2>::new(
                key,
                doubleentry::Balance::ZERO,
                self.journal.head_at(0).expect("size zero is in range"),
            );
            self.journal
                .verify_checkpoint(&empty)
                .expect("an empty-prefix checkpoint must stay valid");
        }

        // Every seal still describes the books. This is the invariant a seal
        // actually asserts — that its closing balances fold every entry booked
        // on or before the period's last day — and it is the one that used to
        // break silently: sealing out of order, or booking into a gap the
        // calendar never defined, restated a sealed period while the seal, its
        // proofs and the chain all went on verifying.
        for seal in self.journal.seals().seals() {
            let period = self
                .journal
                .calendar()
                .get(&seal.period)
                .expect("a sealed period stays defined");
            let recomputed = self
                .journal
                .trial_balance(BalanceQuery::through(period.end))
                .expect("no overflow");
            assert_eq!(
                doubleentry::seal::trial_balance_head(&recomputed),
                seal.trial_balance,
                "period {} was restated after it was sealed",
                seal.period
            );
        }

        // And nothing may sit at or before the watermark that was not there
        // when it moved — the rule that keeps the above true.
        if let Some(through) = self.journal.calendar().sealed_through() {
            let sealed_indices: Vec<u64> = self
                .journal
                .seals()
                .seals()
                .iter()
                .filter_map(|s| s.tree_head.size.checked_sub(1))
                .collect();
            let high_water = sealed_indices.iter().copied().max().unwrap_or(0);
            for (i, entry) in self.journal.entries().iter().enumerate() {
                if entry.booking_date() <= through {
                    assert!(
                        i as u64 <= high_water,
                        "entry {i} was booked on {} — at or before the sealed \
                         watermark of {through} — after the last seal was taken",
                        entry.booking_date()
                    );
                }
            }
        }

        // A hierarchical report redistributes the trial balance and never
        // changes it: every own-balance sits under exactly one root, so the
        // roots reproduce the totals. `Sim` is never registered, so this also
        // pins that an inferred grouping node collects its children.
        for layer in [Layer::Settled, Layer::Pending] {
            let sheet = self
                .journal
                .rollup(BalanceQuery::all(), Currency::EUR, layer)
                .expect("no overflow");
            let flat = self
                .journal
                .trial_balance(BalanceQuery::all())
                .expect("no overflow");
            assert_eq!(
                sheet.total().expect("no overflow"),
                flat.totals(Currency::EUR, layer).expect("no overflow"),
                "the rollup stopped adding up to the trial balance"
            );
            for node in sheet.nodes() {
                let mut folded = node.own;
                for child in sheet.nodes() {
                    if child.path.parent().as_ref() == Some(&node.path) {
                        folded = folded
                            .checked_add(&child.subtree)
                            .expect("a subset of the books cannot overflow");
                    }
                }
                assert_eq!(
                    folded, node.subtree,
                    "{} is not its own balance plus its children",
                    node.path
                );
            }
        }

        // A statement opens where the window before it closed, on either date
        // basis — the identity that makes a statement a statement.
        for account in [self.left, self.right, self.limited] {
            let key = BalanceKey {
                account,
                currency: Currency::EUR,
                layer: Layer::Settled,
            };
            let window = BalanceQuery::between(date!(2026 - 06 - 01), date!(2026 - 06 - 30));
            for query in [window, window.by_value_date()] {
                let lines = self.journal.statement(&key, query).expect("no overflow");
                let opening = self
                    .journal
                    .statement_opening(&key, query, None)
                    .expect("no overflow");
                let mut folded = opening;
                for line in &lines {
                    folded
                        .add(line.direction, line.amount)
                        .expect("a subset of the books cannot overflow");
                    assert_eq!(
                        folded, line.running,
                        "the running balance left the fold behind"
                    );
                    // Resuming after any line reproduces that line's total, so a
                    // page opens where the previous one closed.
                    assert_eq!(
                        self.journal
                            .statement_opening(&key, query, Some(line.position()))
                            .expect("no overflow"),
                        line.running
                    );
                }
            }
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
        prop_assert_eq!(world.journal.trial_balance(BalanceQuery::all()).expect("ok"), expected);
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
        let _ = Label::new(s.clone());
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
        let _ = serde_json::from_str::<Label>(&quoted);
        let _ = serde_json::from_str::<IdempotencyKey>(&quoted);
        let _ = serde_json::from_str::<Entry<Draft, 2>>(&s);
    }
}
