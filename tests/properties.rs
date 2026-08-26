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
    AccountId, Amount, BalanceQuery, Currency, Direction, Entry, EntryId, IdempotencyKey, Journal,
    Layer, MoneyError, Posting, Rounding, ValidationError,
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

fn any_rounding() -> impl Strategy<Value = Rounding> {
    prop_oneof![
        Just(Rounding::HalfUp),
        Just(Rounding::HalfDown),
        Just(Rounding::HalfEven),
        Just(Rounding::TowardZero),
        Just(Rounding::AwayFromZero),
        Just(Rounding::Floor),
        Just(Rounding::Ceiling),
    ]
}

proptest! {
    /// A rounded ratio never lands more than one minor unit from the exact
    /// value, and never on the wrong side of it.
    ///
    /// This is the whole contract of rounding, and it is the property a sign
    /// bug breaks: truncating toward zero where `Floor` was asked for looks
    /// right for every positive input and is wrong for every negative one.
    #[test]
    fn a_rounded_ratio_brackets_the_exact_value(
        minor in -1_000_000_000i64..1_000_000_000,
        numerator in -100_000i64..100_000,
        denominator in prop::sample::select(vec![-997i64, -100, -7, -1, 1, 3, 7, 100, 365, 1_000_000]),
        rounding in any_rounding(),
    ) {
        let exact = i128::from(minor) * i128::from(numerator);
        let got = Eur::from_minor(minor)
            .checked_mul_ratio(numerator, denominator, rounding)
            .expect("in range for these bounds");

        // `got * denominator` is the exact value rounded to a whole multiple of
        // the denominator, so it cannot differ by a whole one.
        let reconstructed = i128::from(got.to_minor()) * i128::from(denominator);
        prop_assert!(
            (reconstructed - exact).abs() < i128::from(denominator).abs(),
            "{rounding:?}: {minor} * {numerator} / {denominator} landed on {got}"
        );

        // Directional modes are not merely near — they are on a stated side.
        let scaled_up = i128::from(got.to_minor()) * i128::from(denominator).abs();
        let exact_up = if denominator < 0 { -exact } else { exact };
        match rounding {
            Rounding::Floor => prop_assert!(scaled_up <= exact_up),
            Rounding::Ceiling => prop_assert!(scaled_up >= exact_up),
            Rounding::TowardZero => prop_assert!(scaled_up.abs() <= exact_up.abs()),
            Rounding::AwayFromZero => prop_assert!(scaled_up.abs() >= exact_up.abs()),
            _ => {}
        }
    }

    /// Every sign-symmetric mode gives mirrored answers for mirrored inputs.
    #[test]
    fn sign_symmetric_rounding_does_not_favour_a_sign(
        minor in -1_000_000_000i64..1_000_000_000,
        numerator in 1i64..100_000,
        denominator in 1i64..100_000,
        rounding in prop_oneof![
            Just(Rounding::HalfUp),
            Just(Rounding::HalfDown),
            Just(Rounding::HalfEven),
            Just(Rounding::TowardZero),
            Just(Rounding::AwayFromZero),
        ],
    ) {
        let positive = Eur::from_minor(minor)
            .checked_mul_ratio(numerator, denominator, rounding)
            .expect("in range");
        let negative = Eur::from_minor(-minor)
            .checked_mul_ratio(numerator, denominator, rounding)
            .expect("in range");
        prop_assert_eq!(negative, positive.checked_neg().expect("in range"));
    }

    /// Widening a scale is lossless: narrowing it again returns the original.
    #[test]
    fn rescaling_wider_round_trips(
        minor in -100_000_000i64..100_000_000,
        rounding in any_rounding(),
    ) {
        let original = Eur::from_minor(minor);
        let wide = original.rescale::<6>(rounding).expect("in range");
        prop_assert_eq!(wide.rescale::<2>(rounding).expect("in range"), original);
    }
}

// ── merkle log ───────────────────────────────────────────────────────────────

fn leaf(i: u64) -> Hash {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&i.to_le_bytes());
    Hash::from_bytes(bytes)
}

/// A log size in `1..max`, and a leaf index inside it.
///
/// Generated **together**, never as two free ranges filtered with
/// `prop_assume!`. Independent ranges discard most of what they produce, so the
/// test explores a fraction of the cases its count suggests — and proptest
/// abandons a run once its global reject budget is spent, so raising the count
/// makes the suite stop running instead. Deriving the index is exact.
fn size_and_index(max: u64) -> impl Strategy<Value = (u64, u64)> {
    (1u64..max).prop_flat_map(|n| (Just(n), 0u64..n))
}

/// A log size of at least two, and two **distinct** indices inside it.
///
/// The second index is an offset from the first rather than a second free draw,
/// so `i != j` holds by construction instead of by rejection.
fn size_and_distinct_indices(max: u64) -> impl Strategy<Value = (u64, u64, u64)> {
    (2u64..max)
        .prop_flat_map(|n| (Just(n), 0u64..n, 1u64..n))
        .prop_map(|(n, i, step)| (n, i, (i + step) % n))
}

/// A log size and a **non-empty** prefix of it.
///
/// Non-empty because a proof from the empty tree is refused by construction.
fn size_and_nonempty_prefix(max: u64) -> impl Strategy<Value = (u64, u64)> {
    (1u64..max).prop_flat_map(|n| (Just(n), 1u64..=n))
}

/// A log size, possibly zero, and any prefix of it — the empty one included.
fn size_and_any_prefix(max: u64) -> impl Strategy<Value = (u64, u64)> {
    (0u64..max).prop_flat_map(|n| (Just(n), 0u64..=n))
}

fn log_of(n: u64) -> MerkleLog {
    MerkleLog::from_leaves((0..n).map(leaf).collect())
}

proptest! {
    /// Every leaf in every log size is provably included under the current root.
    #[test]
    fn inclusion_proofs_always_verify((n, i) in size_and_index(80)) {
        let log = log_of(n);
        let proof = log.inclusion_proof(i).expect("in range");
        prop_assert!(proof.verify(&leaf(i), &log.head()));
    }

    /// An inclusion proof does not verify against a leaf it was not built for.
    #[test]
    fn inclusion_proofs_reject_other_leaves((n, i, j) in size_and_distinct_indices(60)) {
        let log = log_of(n);
        let proof = log.inclusion_proof(i).expect("in range");
        prop_assert!(!proof.verify(&leaf(j), &log.head()));
    }

    /// A proof path with a sibling added or removed never verifies, whatever the
    /// added hash is and wherever the cut falls.
    #[test]
    fn inclusion_proofs_reject_deformed_paths(
        (n, i) in size_and_index(60),
        at in 0usize..8,
        junk in 0u64..1000,
    ) {
        let log = log_of(n);
        let proof = log.inclusion_proof(i).expect("in range");
        let at = at % (proof.path.len() + 1);

        let mut padded = proof.clone();
        padded.path.insert(at, leaf(junk));
        prop_assert!(!padded.verify(&leaf(i), &log.head()));

        // Padding with a hash the tree genuinely contains, rather than a made-up
        // one, must fail for the same reason.
        let mut duplicated = proof.clone();
        if let Some(&real) = proof.path.get(at.min(proof.path.len().saturating_sub(1))) {
            duplicated.path.insert(at, real);
            prop_assert!(!duplicated.verify(&leaf(i), &log.head()));
        }

        let mut truncated = proof.clone();
        if at < truncated.path.len() {
            truncated.path.remove(at);
            prop_assert!(!truncated.verify(&leaf(i), &log.head()));
        }
    }

    /// Only one (index, size) labelling verifies under a given head — the true
    /// one. Verification takes the whole head for exactly this reason: against a
    /// bare root the labels would be unauthenticated.
    #[test]
    fn inclusion_proofs_are_labelled_uniquely_under_a_head(
        (n, i) in size_and_index(40),
        // The forged labels, drawn independently of the true ones and covering
        // both the sizes this log reaches and the ones it does not.
        (size, index) in (1u64..48).prop_flat_map(|s| (Just(s), 0u64..s)),
    ) {
        let log = log_of(n);
        let head = log.head();
        let mut proof = log.inclusion_proof(i).expect("in range");
        proof.leaf_index = index;
        proof.tree_size = size;
        prop_assert_eq!(
            proof.verify(&leaf(i), &head),
            index == i && size == n
        );
    }

    /// A consistency proof verifies between exactly one pair of heads.
    #[test]
    fn consistency_proofs_are_bound_to_both_heads(
        // The prefix is non-empty: a proof from the empty tree is refused, not
        // built.
        (n, m) in size_and_nonempty_prefix(40),
        old_size in 0u64..48,
        new_size in 0u64..48,
    ) {
        let log = log_of(n);
        let old_head = log.head_at(m).expect("in range");
        let new_head = log.head();
        let mut proof = log.consistency_proof(m).expect("in range");
        proof.old_size = old_size;
        proof.new_size = new_size;
        prop_assert_eq!(
            proof.verify(&old_head, &new_head),
            old_size == m && new_size == n
        );
    }

    /// A proof from the empty tree is refused at both ends, for every log.
    ///
    /// It would verify — every log really does extend the empty one — and that
    /// is exactly why it must not be produced or accepted: a `true` that
    /// examined nothing is indistinguishable from one that examined everything.
    #[test]
    fn a_proof_from_the_empty_tree_is_never_available(n in 1u64..60) {
        let log = log_of(n);
        let refused = matches!(
            log.consistency_proof(0),
            Err(doubleentry::ProofError::EmptyOldTree { .. })
        );
        prop_assert!(refused);

        // And the hand-built form is refused by the verifier, whatever path it
        // carries and whatever head it is aimed at.
        let empty = log.head_at(0).expect("in range");
        for path in [vec![], vec![leaf(1)], vec![leaf(1), leaf(2)]] {
            let forged = doubleentry::ConsistencyProof { old_size: 0, new_size: n, path };
            prop_assert!(!forged.verify(&empty, &log.head()));
        }
    }

    /// Any prefix of the log is provably a prefix of the whole.
    #[test]
    fn consistency_proofs_always_verify((n, m) in size_and_nonempty_prefix(80)) {
        let log = log_of(n);
        let old_head = log.head_at(m).expect("in range");
        let proof = log.consistency_proof(m).expect("in range");
        prop_assert!(proof.verify(&old_head, &log.head()));
    }

    /// Rewriting any already-committed leaf changes the root.
    #[test]
    fn tampering_is_always_detected((n, i) in size_and_index(60)) {
        let original = log_of(n);
        let mut leaves: Vec<Hash> = (0..n).map(leaf).collect();
        leaves[i as usize] = leaf(9_999_999);
        let tampered = MerkleLog::from_leaves(leaves);
        prop_assert_ne!(original.root(), tampered.root());
        // And the damage is visible in the stored tree, not only in the root.
        prop_assert_ne!(original.nodes(), tampered.nodes());
    }

    /// A prefix root is exactly the root the log had at that size.
    #[test]
    fn historical_roots_match_replay((n, m) in size_and_any_prefix(48)) {
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
                .restore_account(record)
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

    /// An entry that only balances by netting a layer against the other never
    /// seals — however the postings are arranged.
    ///
    /// The invariant behind it: `verify_balanced` totals every currency and
    /// layer independently, so an entry that balanced only across layers would
    /// falsify it permanently, and an append-only log has no way back.
    #[test]
    fn an_entry_that_nets_across_layers_never_seals(
        amount in 1i64..1_000_000,
        pending_first in any::<bool>(),
    ) {
        let f = Fixture::new(2);
        let pending = Posting::debit(f.account(0), Eur::from_minor(amount), Currency::EUR)
            .in_layer(Layer::Pending);
        let settled = Posting::credit(f.account(1), Eur::from_minor(amount), Currency::EUR);
        let e = if pending_first {
            draft(b"k").post(pending).post(settled)
        } else {
            draft(b"k").post(settled).post(pending)
        };

        let err = e.seal(&f.ctx()).expect_err("the layers do not balance apart");
        // Both layers are named, because both are wrong.
        for want in [Layer::Settled, Layer::Pending] {
            let named = err.any(|e| match e {
                ValidationError::Unbalanced { layer, .. } => *layer == want,
                _ => false,
            });
            prop_assert!(named);
        }
    }

    /// Every layer of a recorded journal balances on its own, always.
    #[test]
    fn each_layer_of_the_journal_balances(
        amounts in prop::collection::vec(1i64..1_000_000, 1..8),
        layers in prop::collection::vec(any::<bool>(), 1..8),
    ) {
        let mut journal = Journal::<2>::new(test_ledger());
        for path in ["P:A", "P:B", "P:C"] {
            journal.register_path(path, date!(2000 - 01 - 01)).expect("registers");
        }
        for (i, amount) in amounts.iter().enumerate() {
            let layer = if *layers.get(i).unwrap_or(&false) { Layer::Pending } else { Layer::Settled };
            let key = format!("k{i}");
            let draft = Entry::<Draft, 2>::new(
                EntryId::generate(),
                IdempotencyKey::new(key.into_bytes()).expect("valid"),
                date!(2026 - 03 - 15),
            )
            .post(Posting::debit(AccountId::from_index(0), Eur::from_minor(*amount), Currency::EUR).in_layer(layer))
            .post(Posting::credit(AccountId::from_index(1), Eur::from_minor(*amount), Currency::EUR).in_layer(layer));
            journal.record(draft).expect("records");
        }
        prop_assert!(journal.verify_balanced().expect("no overflow"));
        prop_assert!(journal.verify_balances().expect("no overflow"));
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
            .trial_balance(BalanceQuery::all())
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
            prop_assert!(proof.verify(&entry.content_hash(), &head));
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

        let tb = j.trial_balance(BalanceQuery::all()).expect("no overflow");
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

            let from_prefix: TrialBalance<2> = prefix.trial_balance(BalanceQuery::all()).expect("ok");
            let from_full: TrialBalance<2> = full.trial_balance(BalanceQuery::over_prefix(take as u64)).expect("ok");
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

        let seal = Seal::build(
            test_ledger(),
            PeriodId::new("p").expect("valid"),
            PeriodCoverage::spanning(0, size.saturating_sub(1), size),
            &log_of(size),
            &tb,
            TreeHead { size: 1, root: leaf(7) },
            None,
        ).expect("builds");
        prop_assert!(seal.is_self_consistent());

        let mut edited = seal.clone();
        edited.last_index = Some(size);
        prop_assert!(!edited.is_self_consistent());

        let mut restated = seal;
        restated.trial_balance.root = leaf(999_999);
        prop_assert!(!restated.is_self_consistent());
    }

    /// A seal's trial-balance root distinguishes any change in gross totals,
    /// including ones that leave every net untouched.
    #[test]
    fn the_trial_balance_head_sees_gross_movement(volume in 1i64..1_000_000) {
        use doubleentry::seal::trial_balance_head;
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
        prop_assert_ne!(trial_balance_head(&quiet), trial_balance_head(&busy));
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

        let before = j.trial_balance(BalanceQuery::all()).expect("ok");
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
        prop_assert_eq!(&before, &j.trial_balance(BalanceQuery::all()).expect("ok"));

        // And a reset restores the open items exactly.
        j.reset_clearing(id, date!(2026 - 04 - 01)).expect("resets");
        prop_assert_eq!(j.clearings().applied_to(a_ref), Eur::ZERO);
        prop_assert_eq!(&before, &j.trial_balance(BalanceQuery::all()).expect("ok"));
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

// ── statements ───────────────────────────────────────────────────────────────

proptest! {
    /// A statement's opening balance plus its movements is its closing balance,
    /// on either date basis, whatever order the entries were recorded in.
    ///
    /// The identity that makes a statement a statement. It breaks if the carried
    /// balance is bounded by log position rather than by date, and no fixture
    /// written in date order can see that — so these are generated in neither
    /// order.
    #[test]
    fn a_statement_opens_where_the_period_before_it_closed(
        // (booking-day offset, value-day offset, minor amount), in recording order.
        rows in prop::collection::vec(
            (0i64..90, 0i64..90, 1i64..10_000),
            0..24,
        ),
        window in (0i64..90, 0i64..90),
        by_value in any::<bool>(),
    ) {
        let f = Fixture::new(2);
        let (left, right) = (f.account(0), f.account(1));
        let mut journal = f.journal();

        let epoch = date!(2026 - 01 - 01);
        let day = |offset: i64| epoch.saturating_add(time::Duration::days(offset));

        for (i, (booked, valued, minor)) in rows.iter().enumerate() {
            let amount = Eur::from_minor(*minor);
            let entry = Entry::<Draft, 2>::new(
                EntryId::generate(),
                IdempotencyKey::new(format!("s{i}").into_bytes()).expect("valid"),
                day(*booked),
            )
            .with_value_date(day(*valued))
            .debit(left, amount, Currency::EUR)
            .credit(right, amount, Currency::EUR);
            journal.record(entry).expect("records");
        }

        let (a, b) = window;
        let (start, end) = (day(a.min(b)), day(a.max(b)));
        let query = if by_value {
            BalanceQuery::between(start, end).by_value_date()
        } else {
            BalanceQuery::between(start, end)
        };

        let key = doubleentry::BalanceKey {
            account: left,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let lines = journal.statement(&key, query).expect("no overflow");
        let opening = journal.statement_opening(&key, query, None).expect("no overflow");

        // Opening plus the page's movements is the last line's running balance.
        let mut folded = opening;
        for line in &lines {
            folded.add(line.direction, line.amount).expect("no overflow");
            prop_assert_eq!(folded, line.running);
        }

        // The closing figure is the same fold the balance reader gives for
        // everything up to the end of the window, on the same basis.
        let through = if by_value {
            BalanceQuery::through(end).by_value_date()
        } else {
            BalanceQuery::through(end)
        };
        prop_assert_eq!(folded, journal.balance(&key, through).expect("no overflow"));

        // And resuming after any line reproduces exactly that line's total, so a
        // page opens where the previous one closed.
        for line in &lines {
            prop_assert_eq!(
                journal
                    .statement_opening(&key, query, Some(line.position()))
                    .expect("no overflow"),
                line.running
            );
        }
    }
}

// ── hierarchical reporting ───────────────────────────────────────────────────

proptest! {
    /// A rollup redistributes a trial balance and never changes it.
    ///
    /// Every own-balance lands under exactly one root, so the roots sum to the
    /// trial balance total. Double counting a node under two ancestors, or
    /// dropping one whose parent was never registered, breaks that sum and
    /// nothing else notices.
    #[test]
    fn a_rollup_redistributes_the_trial_balance_without_changing_it(
        // (path shape, minor amount) — deliberately overlapping prefixes, and
        // never registering the grouping nodes, which is the default a caller
        // falls into.
        rows in prop::collection::vec(
            ((0usize..3, 0usize..3, 0usize..3), 1i64..100_000),
            1..20,
        ),
        depth in 0usize..5,
    ) {
        let mut accounts = AccountRegistry::new();
        let mut balances = TrialBalance::<2>::new();

        for ((a, b, c), minor) in &rows {
            let path = format!("Root{a}:Mid{b}:Leaf{c}");
            let id = match accounts.id_of(&path.parse().expect("valid")) {
                Some(id) => id,
                None => accounts
                    .register_path(&path, date!(2020 - 01 - 01))
                    .expect("registers"),
            };
            // Both sides on the same tree, so the whole report balances.
            balances
                .apply(&Posting::debit(id, Eur::from_minor(*minor), Currency::EUR))
                .expect("no overflow");
            balances
                .apply(&Posting::credit(id, Eur::from_minor(*minor), Currency::EUR))
                .expect("no overflow");
        }

        let report = doubleentry::Rollup::of(
            &balances,
            &accounts,
            Currency::EUR,
            Layer::Settled,
        )
        .expect("no overflow");

        // The roots reproduce the trial balance exactly.
        prop_assert_eq!(
            report.total().expect("no overflow"),
            balances.totals(Currency::EUR, Layer::Settled).expect("no overflow")
        );

        // Every node is its own balance plus its children's subtrees — the
        // definition, checked against the report rather than assumed from it.
        for node in report.nodes() {
            let mut folded = node.own;
            for child in report.nodes() {
                if child.path.parent().as_ref() == Some(&node.path) {
                    folded = folded.checked_add(&child.subtree).expect("no overflow");
                }
            }
            prop_assert_eq!(folded, node.subtree);
        }

        // Parent before children, so printing the nodes in order is the tree.
        for pair in report.nodes().windows(2) {
            prop_assert!(pair[0].path < pair[1].path);
        }

        // Truncating shortens the report without changing what it says.
        let short = report.to_depth(depth);
        prop_assert!(short.nodes().iter().all(|n| n.depth() <= depth));
        if depth >= 1 {
            prop_assert_eq!(
                short.total().expect("no overflow"),
                report.total().expect("no overflow")
            );
        }
    }
}
