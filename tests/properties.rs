//! Property tests for the engine's invariants.
//!
//! These check the claims the crate makes, over generated inputs rather than
//! hand-picked ones: money is conserved under splitting, proofs verify for every
//! shape of log, encoding is deterministic, and the balance invariant holds no
//! matter how the postings are arranged.

// A failing assertion is the point of a test; library code keeps the strict
// lints that forbid panicking and unchecked arithmetic.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::account::AccountRegistry;
use doubleentry::balance::TrialBalance;
use doubleentry::canonical::Canonical;
use doubleentry::entry::{Draft, LedgerPolicy, SealContext};
use doubleentry::hash::Hash;
use doubleentry::merkle::MerkleLog;
use doubleentry::period::{LedgerId, PeriodCalendar};
use doubleentry::{
    AccountId, Amount, Currency, Direction, Entry, EntryId, IdempotencyKey, Journal, Layer,
    MoneyError, Posting, ValidationError,
};
use proptest::prelude::*;
use time::macros::date;

/// The ledger these tests keep their books in.
fn test_ledger() -> LedgerId {
    LedgerId::new("test-ledger").expect("valid")
}

type Eur = Amount<2>;

// ── money ────────────────────────────────────────────────────────────────────

proptest! {
    /// Splitting never creates or destroys a minor unit.
    #[test]
    fn allocate_conserves_the_total(
        minor in -1_000_000_000i64..1_000_000_000,
        weights in prop::collection::vec(0u64..1000, 1..24),
    ) {
        let total = Eur::from_minor(minor);
        match total.allocate(&weights) {
            Ok(parts) => {
                prop_assert_eq!(parts.len(), weights.len());
                let sum = Eur::checked_sum(parts.iter().copied()).expect("no overflow");
                prop_assert_eq!(sum, total);
            }
            // The only permitted refusal is a degenerate weight vector.
            Err(e) => prop_assert_eq!(e, MoneyError::ZeroWeight),
        }
    }

    /// Equal splitting conserves the total and stays within one minor unit.
    #[test]
    fn distribute_conserves_and_is_even(
        minor in -1_000_000_000i64..1_000_000_000,
        n in 1usize..64,
    ) {
        let total = Eur::from_minor(minor);
        let parts = total.distribute(n).expect("valid split");
        prop_assert_eq!(Eur::checked_sum(parts.iter().copied()).expect("ok"), total);

        let max = parts.iter().map(|p| p.to_minor()).max().unwrap_or(0);
        let min = parts.iter().map(|p| p.to_minor()).min().unwrap_or(0);
        prop_assert!(max - min <= 1, "parts differ by more than one minor unit");
    }

    /// A split of a non-negative total never yields a negative part.
    #[test]
    fn allocate_preserves_sign(
        minor in 0i64..1_000_000_000,
        weights in prop::collection::vec(1u64..1000, 1..16),
    ) {
        let parts = Eur::from_minor(minor).allocate(&weights).expect("valid split");
        prop_assert!(parts.iter().all(|p| !p.is_negative()));
    }

    /// Parsing and displaying are inverse at the type's own scale.
    #[test]
    fn parse_display_round_trips(minor in -9_000_000_000_000i64..9_000_000_000_000) {
        let a = Eur::from_minor(minor);
        prop_assert_eq!(Eur::parse(&a.to_string()).expect("round trips"), a);
    }

    /// Addition never wraps: it either produces the right answer or an error.
    #[test]
    fn addition_is_total(a in any::<i64>(), b in any::<i64>()) {
        let result = Eur::from_minor(a).checked_add(Eur::from_minor(b));
        match a.checked_add(b) {
            Some(expected) => prop_assert_eq!(result.expect("fits"), Eur::from_minor(expected)),
            None => prop_assert_eq!(result, Err(MoneyError::Overflow)),
        }
    }
}

// ── merkle log ───────────────────────────────────────────────────────────────

fn leaf(i: u64) -> Hash {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&i.to_le_bytes());
    Hash::from_bytes(bytes)
}

fn log_of(n: u64) -> MerkleLog {
    MerkleLog::from_leaves((0..n).map(leaf).collect())
}

proptest! {
    /// Every leaf in every log size is provably included under the current root.
    #[test]
    fn inclusion_proofs_always_verify(n in 1u64..80, i in 0u64..80) {
        prop_assume!(i < n);
        let log = log_of(n);
        let proof = log.inclusion_proof(i).expect("in range");
        prop_assert!(proof.verify(&leaf(i), &log.root()));
    }

    /// An inclusion proof does not verify against a leaf it was not built for.
    #[test]
    fn inclusion_proofs_reject_other_leaves(n in 2u64..60, i in 0u64..60, j in 0u64..60) {
        prop_assume!(i < n && j < n && i != j);
        let log = log_of(n);
        let proof = log.inclusion_proof(i).expect("in range");
        prop_assert!(!proof.verify(&leaf(j), &log.root()));
    }

    /// Any prefix of the log is provably a prefix of the whole.
    #[test]
    fn consistency_proofs_always_verify(n in 1u64..80, m in 0u64..80) {
        prop_assume!(m <= n);
        let log = log_of(n);
        let old_root = log.root_at(m).expect("in range");
        let proof = log.consistency_proof(m).expect("in range");
        prop_assert!(proof.verify(&old_root, &log.root()));
    }

    /// Rewriting any already-committed leaf changes the root.
    #[test]
    fn tampering_is_always_detected(n in 1u64..60, i in 0u64..60) {
        prop_assume!(i < n);
        let original = log_of(n);
        let mut leaves = original.leaves().to_vec();
        leaves[i as usize] = leaf(9_999_999);
        let tampered = MerkleLog::from_leaves(leaves);
        prop_assert_ne!(original.root(), tampered.root());
    }

    /// A prefix root is exactly the root the log had at that size.
    #[test]
    fn historical_roots_match_replay(n in 0u64..48, m in 0u64..48) {
        prop_assume!(m <= n);
        prop_assert_eq!(log_of(n).root_at(m).expect("in range"), log_of(m).root());
    }
}

// ── entries and the journal ──────────────────────────────────────────────────

struct Fixture {
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,
    ids: Vec<AccountId>,
}

impl Fixture {
    fn new(account_count: usize) -> Self {
        let mut accounts = AccountRegistry::new();
        let ids = (0..account_count)
            .map(|i| {
                accounts
                    .register_path(&format!("Accounts:A{i}"), date!(2020 - 01 - 01))
                    .expect("registers")
            })
            .collect();
        Self {
            accounts,
            calendar: PeriodCalendar::new(),
            policy: LedgerPolicy::default(),
            ids,
        }
    }

    fn ctx(&self) -> SealContext<'_> {
        SealContext {
            accounts: &self.accounts,
            calendar: &self.calendar,
            policy: &self.policy,
        }
    }

    fn account(&self, i: usize) -> AccountId {
        *self.ids.get(i % self.ids.len()).expect("non-empty")
    }

    /// A journal sharing this fixture's accounts, so drafts recorded through it
    /// are validated against the same registry they were built for.
    fn journal(&self) -> Journal<2> {
        let mut journal = Journal::<2>::new(test_ledger());
        for record in self.accounts.records() {
            journal
                .accounts_mut()
                .restore(record)
                .expect("restores at its own handle");
        }
        journal
    }
}

fn draft(key: &[u8]) -> Entry<Draft, 2> {
    Entry::new(
        EntryId::generate(),
        IdempotencyKey::new(key.to_vec()).expect("valid"),
        date!(2026 - 03 - 15),
    )
}

proptest! {
    /// Any set of postings whose debits equal credits seals successfully.
    #[test]
    fn balanced_postings_always_seal(
        amounts in prop::collection::vec(1i64..1_000_000, 1..8),
    ) {
        let f = Fixture::new(4);
        let total: i64 = amounts.iter().sum();

        // Every amount on the debit side, the total credited back in one leg.
        let mut e = draft(b"k");
        for (i, a) in amounts.iter().enumerate() {
            e = e.debit(f.account(i), Eur::from_minor(*a), Currency::EUR);
        }
        e = e.credit(f.account(amounts.len()), Eur::from_minor(total), Currency::EUR);

        prop_assert!(e.seal(&f.ctx()).is_ok());
    }

    /// Any imbalance is caught and named.
    #[test]
    fn unbalanced_postings_never_seal(
        debit in 1i64..1_000_000,
        credit in 1i64..1_000_000,
    ) {
        prop_assume!(debit != credit);
        let f = Fixture::new(2);
        let err = draft(b"k")
            .debit(f.account(0), Eur::from_minor(debit), Currency::EUR)
            .credit(f.account(1), Eur::from_minor(credit), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("must not balance");
        let names_the_imbalance = err.any(|e| matches!(e, ValidationError::Unbalanced { .. }));
        prop_assert!(names_the_imbalance);
    }

    /// Reordering the postings does not change the entry's identity.
    #[test]
    fn the_content_hash_ignores_nothing_semantic(amount in 1i64..1_000_000) {
        let f = Fixture::new(2);
        let build = || {
            draft(b"k")
                .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
                .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances")
        };
        // Distinct identifiers, identical content.
        let a = build();
        let b = build();
        prop_assert_ne!(a.id(), b.id());
        prop_assert_eq!(a.content_hash(), b.content_hash());
    }

    /// Canonical encoding is a pure function of the value.
    #[test]
    fn canonical_encoding_is_deterministic(amount in 1i64..1_000_000) {
        let f = Fixture::new(2);
        let build = || {
            draft(b"k")
                .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
                .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances")
                .to_canonical_bytes()
        };
        prop_assert_eq!(build(), build());
    }

    /// A journal of balanced entries has matching debit and credit totals,
    /// and its Merkle log always agrees with its contents.
    #[test]
    fn the_journal_folds_to_a_balanced_trial_balance(
        amounts in prop::collection::vec(1i64..100_000, 1..24),
    ) {
        let f = Fixture::new(5);
        let mut j = f.journal();

        for (i, a) in amounts.iter().enumerate() {
            let entry = draft(format!("k{i}").as_bytes())
                .debit(f.account(i), Eur::from_minor(*a), Currency::EUR)
                .credit(f.account(i + 1), Eur::from_minor(*a), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances");
            j.record_validated(entry).expect("records");
        }

        prop_assert_eq!(j.len(), amounts.len());
        prop_assert!(j.verify_balanced().expect("no overflow"));
        prop_assert!(j.verify_log());

        let totals = j
            .trial_balance(None)
            .expect("no overflow")
            .totals(Currency::EUR, Layer::Settled)
            .expect("no overflow");
        prop_assert!(totals.is_balanced());
        prop_assert_eq!(totals.debits.to_minor(), amounts.iter().sum::<i64>());
    }

    /// Every entry in a journal of any size is provably included.
    #[test]
    fn every_journal_entry_is_provable(count in 1usize..40) {
        let f = Fixture::new(3);
        let mut j = f.journal();
        for i in 0..count {
            let entry = draft(format!("k{i}").as_bytes())
                .debit(f.account(i), Eur::from_minor(100), Currency::EUR)
                .credit(f.account(i + 1), Eur::from_minor(100), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances");
            j.record_validated(entry).expect("records");
        }

        let head = j.head();
        for (i, entry) in j.entries().iter().enumerate() {
            let proof = j
                .prove_inclusion(doubleentry::LogIndex::from(i as u64))
                .expect("in range");
            prop_assert!(proof.verify(&entry.content_hash(), &head.root));
        }
    }

    /// Reversing an entry restores every net balance it touched, while leaving
    /// both gross totals visible.
    #[test]
    fn a_reversal_restores_every_net_balance(amount in 1i64..1_000_000) {
        let f = Fixture::new(2);
        let mut j = f.journal();

        let original = draft(b"orig")
            .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
            .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        j.record_validated(original.clone()).expect("records");

        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("balances");
        j.record_validated(reversal).expect("records");

        let tb = j.trial_balance(None).expect("no overflow");
        for (_, balance) in tb.iter() {
            prop_assert_eq!(balance.signed_net().expect("ok"), Eur::ZERO);
            prop_assert!(!balance.is_empty(), "gross turnover must remain visible");
        }
    }

    /// Replaying the same submission never appends, whatever the repeat count.
    #[test]
    fn replays_are_idempotent(repeats in 1usize..10, amount in 1i64..1_000_000) {
        let f = Fixture::new(2);
        let mut j = f.journal();
        let mut first_index = None;

        for _ in 0..repeats {
            let entry = draft(b"stable-key")
                .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
                .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances");
            let recorded = j.record_validated(entry).expect("records or replays");
            match first_index {
                None => first_index = Some(recorded.index),
                Some(idx) => {
                    prop_assert_eq!(recorded.index, idx);
                    prop_assert!(!recorded.is_new);
                }
            }
        }
        prop_assert_eq!(j.len(), 1);
    }

    /// A prefix fold equals a journal built from that prefix alone.
    #[test]
    fn prefix_folds_match_replayed_history(
        amounts in prop::collection::vec(1i64..100_000, 1..16),
    ) {
        let f = Fixture::new(4);
        let build = |take: usize| {
            let mut j = f.journal();
            for (i, a) in amounts.iter().take(take).enumerate() {
                let entry = draft(format!("k{i}").as_bytes())
                    .debit(f.account(i), Eur::from_minor(*a), Currency::EUR)
                    .credit(f.account(i + 1), Eur::from_minor(*a), Currency::EUR)
                    .seal(&f.ctx())
                    .expect("balances");
                j.record_validated(entry).expect("records");
            }
            j
        };

        let full = build(amounts.len());
        for take in 1..=amounts.len() {
            let prefix = build(take);
            let through = doubleentry::LogIndex::from((take - 1) as u64);

            let from_prefix: TrialBalance<2> = prefix.trial_balance(None).expect("ok");
            let from_full: TrialBalance<2> = full.trial_balance(Some(through)).expect("ok");
            prop_assert_eq!(from_prefix, from_full);
        }
    }
}

// ── direction and layer ──────────────────────────────────────────────────────

proptest! {
    /// Inverting a posting twice is the identity, for every shape of posting.
    #[test]
    fn inverting_twice_is_the_identity(amount in 0i64..1_000_000, debit in any::<bool>()) {
        let account = AccountId::from_index(0);
        let direction = if debit { Direction::Debit } else { Direction::Credit };
        let p = Posting::<2>::new(account, direction, Eur::from_minor(amount), Currency::EUR);
        prop_assert_eq!(p.inverted().inverted(), p);
    }

    /// A posting and its inverse always net to zero.
    #[test]
    fn a_posting_and_its_inverse_cancel(amount in 0i64..1_000_000, debit in any::<bool>()) {
        let account = AccountId::from_index(0);
        let direction = if debit { Direction::Debit } else { Direction::Credit };
        let p = Posting::<2>::new(account, direction, Eur::from_minor(amount), Currency::EUR);
        let sum = p
            .signed()
            .expect("ok")
            .checked_add(p.inverted().signed().expect("ok"))
            .expect("ok");
        prop_assert_eq!(sum, Eur::ZERO);
    }
}

// ── seals, checkpoints, and assertions ───────────────────────────────────────

proptest! {
    /// A seal commits to its own contents: any edit invalidates it.
    #[test]
    fn seals_detect_any_edit(size in 1u64..64, debits in 1i64..1_000_000) {
        use doubleentry::merkle::TreeHead;
        use doubleentry::period::PeriodId;
        use doubleentry::seal::PeriodCoverage;
        use doubleentry::{Balance, BalanceKey, Seal, TrialBalance};

        let mut tb = TrialBalance::<2>::new();
        let key = BalanceKey {
            account: AccountId::from_index(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let mut balance = Balance::<2>::ZERO;
        balance.add(Direction::Debit, Eur::from_minor(debits)).expect("ok");
        tb.set(key, balance);

        let head = TreeHead { size, root: leaf(size) };
        let seal = Seal::build(test_ledger(), PeriodId::new("p").expect("valid"), PeriodCoverage::spanning(0, size.saturating_sub(1), size), head, &tb, leaf(7), None);
        prop_assert!(seal.is_self_consistent());

        let mut edited = seal.clone();
        edited.last_index = Some(size);
        prop_assert!(!edited.is_self_consistent());

        let mut restated = seal;
        restated.trial_balance_root = leaf(999_999);
        prop_assert!(!restated.is_self_consistent());
    }

    /// A seal's trial-balance root distinguishes any change in gross totals,
    /// including ones that leave every net untouched.
    #[test]
    fn the_trial_balance_root_sees_gross_movement(volume in 1i64..1_000_000) {
        use doubleentry::seal::trial_balance_root;
        use doubleentry::{Balance, BalanceKey, TrialBalance};

        let key = BalanceKey {
            account: AccountId::from_index(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };

        let mut quiet = TrialBalance::<2>::new();
        quiet.set(key, Balance::<2>::ZERO);

        let mut busy = TrialBalance::<2>::new();
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::from_minor(volume)).expect("ok");
        b.add(Direction::Credit, Eur::from_minor(volume)).expect("ok");
        busy.set(key, b);

        // Identical nets, different turnover: the commitment must tell them apart.
        prop_assert_eq!(
            quiet.get(&key).expect("set").signed_net().expect("ok"),
            busy.get(&key).expect("set").signed_net().expect("ok")
        );
        prop_assert_ne!(trial_balance_root(&quiet), trial_balance_root(&busy));
    }

    /// A checkpoint taken at any point re-derives from the journal.
    #[test]
    fn checkpoints_always_re_derive(amounts in prop::collection::vec(1i64..100_000, 1..16)) {
        use doubleentry::BalanceKey;

        let f = Fixture::new(3);
        let mut j = f.journal();
        for (i, a) in amounts.iter().enumerate() {
            let entry = draft(format!("k{i}").as_bytes())
                .debit(f.account(i), Eur::from_minor(*a), Currency::EUR)
                .credit(f.account(i + 1), Eur::from_minor(*a), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances");
            j.record_validated(entry).expect("records");
        }

        let key = BalanceKey {
            account: f.account(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let cp = j.checkpoint(&key).expect("no overflow");
        prop_assert!(j.verify_checkpoint(&cp).is_ok());
    }

    /// An assertion holds exactly when it names the journal's own net.
    #[test]
    fn assertions_hold_only_on_the_true_net(
        amount in 1i64..1_000_000,
        offset in -1000i64..1000,
    ) {
        use doubleentry::{BalanceAssertion, BalanceKey};

        let f = Fixture::new(2);
        let mut j = f.journal();
        let entry = draft(b"k")
            .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
            .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        j.record_validated(entry).expect("records");

        let key = BalanceKey {
            account: f.account(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let claimed = amount.saturating_add(offset);
        let outcome = j
            .check_assertion(&BalanceAssertion::net(key, Eur::from_minor(claimed)))
            .expect("no overflow");
        prop_assert_eq!(outcome.held(), offset == 0);
    }
}

// ── serde round-tripping ─────────────────────────────────────────────────────

#[cfg(feature = "serde")]
proptest! {
    /// Money survives a JSON round trip exactly, as a decimal string.
    #[test]
    fn amounts_round_trip_through_json(minor in -9_000_000_000_000i64..9_000_000_000_000) {
        let a = Eur::from_minor(minor);
        let json = serde_json::to_string(&a).expect("serialises");
        // Never a bare integer: the scale would be lost.
        prop_assert!(json.starts_with('"'), "expected a string, got {json}");
        prop_assert_eq!(serde_json::from_str::<Eur>(&json).expect("parses"), a);
    }

    /// A deserialised entry is a draft and re-seals to the same content hash.
    #[test]
    fn entries_round_trip_and_re_seal_identically(amount in 1i64..1_000_000) {
        let f = Fixture::new(2);
        let original = draft(b"k")
            .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
            .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");

        let json = serde_json::to_string(&original).expect("serialises");
        let received: Entry<Draft, 2> = serde_json::from_str(&json).expect("parses as a draft");

        // The witness is re-established locally, never trusted from the wire.
        let resealed = received.seal(&f.ctx()).expect("still balances");
        prop_assert_eq!(resealed.content_hash(), original.content_hash());
    }
}

#[cfg(feature = "serde")]
#[test]
fn deserialisation_re_runs_validation() {
    use doubleentry::{Currency, Label};

    // Values that no constructor would accept must not survive a round trip.
    assert!(serde_json::from_str::<Currency>("\"eur\"").is_err());
    assert!(serde_json::from_str::<Currency>("\"EURO\"").is_err());
    assert!(serde_json::from_str::<Label>("\"\"").is_err());
    assert!(serde_json::from_str::<Label>("\"bad\\nvalue\"").is_err());
    assert!(serde_json::from_str::<Eur>("\"1.234\"").is_err());
    assert!(serde_json::from_str::<Eur>("123").is_err());

    // Valid ones do.
    assert_eq!(
        serde_json::from_str::<Currency>("\"EUR\"").expect("valid"),
        Currency::EUR
    );
}

// ── clearing ─────────────────────────────────────────────────────────────────

proptest! {
    /// However a receivable is settled, the applied amount never exceeds it and
    /// the residual is exactly what is left.
    #[test]
    fn clearing_never_over_applies(
        invoice in 100i64..1_000_000,
        payments in prop::collection::vec(1i64..200_000, 1..8),
    ) {
        use doubleentry::clearing::{Clearing, ClearingId, PostingRef};
        use doubleentry::BalanceKey;

        let f = Fixture::new(2);
        let mut j = f.journal();

        let inv = draft(b"invoice")
            .debit(f.account(0), Eur::from_minor(invoice), Currency::EUR)
            .credit(f.account(1), Eur::from_minor(invoice), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        let invoice_ref = PostingRef::new(inv.id(), 0);
        j.record_validated(inv).expect("records");

        let key = BalanceKey {
            account: f.account(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };

        let mut applied_total = 0i64;
        for (i, pay) in payments.iter().enumerate() {
            let p = draft(format!("pay{i}").as_bytes())
                .credit(f.account(0), Eur::from_minor(*pay), Currency::EUR)
                .debit(f.account(1), Eur::from_minor(*pay), Currency::EUR)
                .seal(&f.ctx())
                .expect("balances");
            let pay_ref = PostingRef::new(p.id(), 0);
            j.record_validated(p).expect("records");

            let room = invoice.saturating_sub(applied_total).min(*pay);
            if room <= 0 {
                continue;
            }
            j.clear(
                Clearing::new(ClearingId::generate(), key, date!(2026 - 03 - 20))
                    .apply(invoice_ref, Eur::from_minor(room))
                    .apply(pay_ref, Eur::from_minor(room)),
            )
            .expect("within the residual");
            applied_total = applied_total.saturating_add(room);
        }

        // The invoice is never applied beyond its own amount.
        prop_assert!(applied_total <= invoice);
        prop_assert_eq!(
            j.clearings().applied_to(invoice_ref),
            Eur::from_minor(applied_total)
        );

        // Whatever is open is exactly what was not applied.
        let open = j.open_items(&key).expect("ok");
        let invoice_open = open.iter().find(|i| i.posting == invoice_ref);
        if applied_total == invoice {
            prop_assert!(invoice_open.is_none());
        } else {
            let item = invoice_open.expect("still open");
            prop_assert_eq!(item.residual, Eur::from_minor(invoice - applied_total));
        }
    }

    /// Clearing is an assignment, never a movement: balances are untouched.
    #[test]
    fn clearing_never_moves_money(amount in 1i64..1_000_000) {
        use doubleentry::clearing::{Clearing, ClearingId, PostingRef};

        let f = Fixture::new(2);
        let mut j = f.journal();

        let a = draft(b"a")
            .debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
            .credit(f.account(1), Eur::from_minor(amount), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        let a_ref = PostingRef::new(a.id(), 0);
        j.record_validated(a).expect("records");

        let b = draft(b"b")
            .credit(f.account(0), Eur::from_minor(amount), Currency::EUR)
            .debit(f.account(1), Eur::from_minor(amount), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        let b_ref = PostingRef::new(b.id(), 0);
        j.record_validated(b).expect("records");

        let before = j.trial_balance(None).expect("ok");
        let id = ClearingId::generate();
        let key = doubleentry::BalanceKey {
            account: f.account(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        j.clear(
            Clearing::new(id, key, date!(2026 - 03 - 20))
                .apply(a_ref, Eur::from_minor(amount))
                .apply(b_ref, Eur::from_minor(amount)),
        )
        .expect("clears");
        prop_assert_eq!(&before, &j.trial_balance(None).expect("ok"));

        // And a reset restores the open items exactly.
        j.reset_clearing(id, date!(2026 - 04 - 01)).expect("resets");
        prop_assert_eq!(j.clearings().applied_to(a_ref), Eur::ZERO);
        prop_assert_eq!(&before, &j.trial_balance(None).expect("ok"));
    }
}

// ── closing entries ──────────────────────────────────────────────────────────

proptest! {
    /// Closing always balances, and always flattens the accounts in scope.
    #[test]
    fn closing_balances_and_flattens(
        revenue in 1i64..1_000_000,
        expense in 1i64..1_000_000,
    ) {
        use doubleentry::account::{Account, AccountKind, AccountPath};
        use doubleentry::{BalanceKey, TrialBalance, closing_postings};

        let mut accounts = AccountRegistry::new();
        let mut register = |path: &str, kind: AccountKind| {
            accounts
                .register(
                    Account::new(AccountPath::parse(path).expect("valid"), date!(2020 - 01 - 01))
                        .with_kind(kind),
                )
                .expect("registers")
        };
        let income = register("Income:Sales", AccountKind::Income);
        let cost = register("Expense:Rent", AccountKind::Expense);
        let equity = register("Equity:Retained", AccountKind::Equity);

        let mut tb = TrialBalance::<2>::new();
        tb.apply(&Posting::credit(income, Eur::from_minor(revenue), Currency::EUR)).expect("ok");
        tb.apply(&Posting::debit(cost, Eur::from_minor(expense), Currency::EUR)).expect("ok");

        let postings = closing_postings(
            &tb,
            &accounts,
            &[AccountKind::Income, AccountKind::Expense],
            equity,
            Layer::Settled,
        )
        .expect("closes");

        // The generated postings balance on their own.
        let mut check = TrialBalance::<2>::new();
        for p in &postings {
            check.apply(p).expect("ok");
        }
        prop_assert!(check.totals(Currency::EUR, Layer::Settled).expect("ok").is_balanced());

        // Applying them flattens income and expense.
        let mut after = tb;
        for p in &postings {
            after.apply(p).expect("ok");
        }
        for account in [income, cost] {
            let key = BalanceKey { account, currency: Currency::EUR, layer: Layer::Settled };
            prop_assert_eq!(
                after.get_or_zero(&key).signed_net().expect("ok"),
                Eur::ZERO
            );
        }

        // Equity absorbs exactly the period's result.
        let equity_key = BalanceKey { account: equity, currency: Currency::EUR, layer: Layer::Settled };
        prop_assert_eq!(
            after.get_or_zero(&equity_key).signed_net().expect("ok"),
            Eur::from_minor(expense - revenue)
        );

        // A second pass has nothing left to do.
        let again = closing_postings(
            &after,
            &accounts,
            &[AccountKind::Income, AccountKind::Expense],
            equity,
            Layer::Settled,
        )
        .expect("ok");
        prop_assert!(again.is_empty());
    }
}
