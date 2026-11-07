//! Regressions for the engine findings in the rule core's scope. Each
//! proof of concept asserted the defect; each test here asserts the corrected
//! behaviour and names the proof of concept it inverts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{Claims, audit_leaves, books, code, eur, journal, leaf_bytes, ledger, rejected, sale};
use doubleentry::error::AuditFailureKind;
use doubleentry::store::{Header, Op, StateView, execute};
use doubleentry::{
    AccountHandle, AccountKind, AccountPath, AccountRef, AccountSpec, Amount, Balance,
    BalanceLimit, Dimensions, Entry, EntryRecord, Error, IdempotencyKey, Journal, Label, Layer,
    LedgerPolicy, PostingRef, Rejection, Side,
    currency::EUR,
    date,
    record::{EntryParts, PostingRecord, Record},
};

/// The leaf of an entry record built directly, bypassing the rule core.
fn forged_entry(key: &str, on: doubleentry::Date, legs: &[(u32, Side, &str)]) -> Vec<u8> {
    let postings = legs
        .iter()
        .map(|&(handle, side, amount)| PostingRecord {
            handle: AccountHandle::new(handle).unwrap(),
            side,
            amount: eur(amount),
            currency: EUR,
            layer: Layer::Settled,
            dimensions: Dimensions::new(),
        })
        .collect();
    let record = EntryRecord::new(EntryParts {
        key: IdempotencyKey::new(key).unwrap(),
        booking_date: on,
        value_date: on,
        captured_at: None,
        kind: None,
        memo: None,
        documents: Vec::new(),
        postings,
        reverses: None,
        hold: None,
        rates: Vec::new(),
    })
    .unwrap();
    Record::Entry(record).to_leaf_bytes()
}

fn would_reject(
    result: Result<doubleentry::AuditReport, doubleentry::error::AuditFailure>,
) -> (u64, Rejection) {
    match result {
        Err(doubleentry::error::AuditFailure {
            position: Some(position),
            kind: AuditFailureKind::WouldReject(rejection),
            ..
        }) => (position, rejection),
        other => panic!("expected a replay refusal, got {other:?}"),
    }
}

fn restore(books: &Journal, leaves: &[Vec<u8>]) -> Result<Journal, Error> {
    Journal::from_log(
        books.id(),
        leaves
            .iter()
            .enumerate()
            .map(|(position, bytes)| doubleentry::log::ReplayLeaf {
                position: position as u64,
                bytes,
                memo: None,
                fingerprint: None,
            }),
    )
}

// PoC: f1_forge_balanced_entry_via_public_digest
#[test]
fn ef1_unbalanced_draft_has_no_path_in() {
    let mut books = books();
    let unbalanced = Entry::new("forged", date!("2026-03-01"))
        .debit("Assets:Cash", eur("100.00"), EUR)
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(code(books.record(unbalanced)), 106);
    let lone = Entry::new("lone", date!("2026-03-01")).debit(
        AccountHandle::new(999).unwrap(),
        eur("5.00"),
        EUR,
    );
    assert_eq!(code(books.record(lone)), 101);
    // There is no pre-validated entry type and no way to record one (the
    // `compile_fail` examples on the `core` module prove the names are gone);
    // a forged leaf is refused by the replay.
    let mut leaves = leaf_bytes(&books);
    leaves.push(forged_entry(
        "forged",
        date!("2026-03-01"),
        &[(0, Side::Debit, "100.00"), (1, Side::Credit, "1.00")],
    ));
    let (position, rejection) = would_reject(audit_leaves(&books, &leaves, &Claims::of(&books)));
    assert_eq!(position, books.head().size);
    assert_eq!(rejection.code(), 106);
}

// PoC: f2_record_validated_books_below_watermark_and_audit_misses_it
// PoC: z_probe_stale_seal
#[test]
fn ef2_sealed_period_refused_and_audited() {
    let mut books = books();
    books
        .record(sale("e1", date!("2026-03-15"), "10.00"))
        .unwrap();
    books.seal_period("2026-01").unwrap();
    books.seal_period("2026-02").unwrap();
    books.seal_period("2026-03").unwrap();
    assert_eq!(
        rejected(books.record(sale("late", date!("2026-03-10"), "999.00"))),
        Rejection::PeriodSealed {
            period: Label::new("2026-03").unwrap(),
            date: date!("2026-03-10")
        }
    );
    // A leaf injected after the seal fails the replay at its position.
    let mut leaves = leaf_bytes(&books);
    leaves.push(forged_entry(
        "late",
        date!("2026-03-10"),
        &[(0, Side::Debit, "999.00"), (1, Side::Credit, "999.00")],
    ));
    let error = restore(&books, &leaves).unwrap_err();
    let Error::Integrity(integrity) = error else {
        panic!("expected an integrity failure");
    };
    assert_eq!(integrity.position(), Some(books.head().size));
    let source = std::error::Error::source(&integrity)
        .and_then(|source| source.downcast_ref::<doubleentry::error::AuditFailure>())
        .unwrap();
    assert!(matches!(
        &source.kind,
        AuditFailureKind::WouldReject(Rejection::PeriodSealed { .. })
    ));
}

// PoC: f3_audit_passes_with_individually_unbalanced_entries
#[test]
fn ef3_audit_replays_each_entry() {
    let books = books();
    let mut leaves = leaf_bytes(&books);
    let first = books.head().size;
    leaves.push(forged_entry(
        "a",
        date!("2026-03-01"),
        &[(0, Side::Debit, "100.00")],
    ));
    leaves.push(forged_entry(
        "b",
        date!("2026-03-02"),
        &[(1, Side::Credit, "100.00")],
    ));
    let (position, rejection) = would_reject(audit_leaves(&books, &leaves, &Claims::of(&books)));
    assert_eq!(position, first);
    assert_eq!(rejection, Rejection::TooFewPostings { count: 1 });
}

// PoC: f4_period_sealed_without_a_seal
#[test]
fn ef4_no_state_without_seal() {
    // The only way to seal is `seal_period`, which writes a seal record;
    // sealing February while January is open is refused.
    let mut books = books();
    assert_eq!(
        rejected(books.seal_period("2026-02")),
        Rejection::PredecessorNotSealed {
            predecessor: Label::new("2026-01").unwrap(),
            earliest: Label::new("2026-01").unwrap(),
        }
    );
    // A store claiming January sealed without a seal record fails the audit.
    let mut claims = Claims::of(&books);
    claims
        .rows_mut(doubleentry::store::ProjectionPart::Seals)
        .push(doubleentry::store::ProjectionRow::new(vec![
            doubleentry::store::Cell::Text("2026-01".into()),
            doubleentry::store::Cell::Int(5),
        ]));
    let failure = audit_leaves(&books, &leaf_bytes(&books), &claims).unwrap_err();
    assert!(
        matches!(
            failure.kind,
            AuditFailureKind::ProjectionMismatch {
                part: doubleentry::store::ProjectionPart::Seals,
                ..
            }
        ),
        "{failure}"
    );
}

// PoC: f5_reversal_original_booking_date_unchecked
#[test]
fn ef5_reversal_mirror_and_date() {
    let mut books = books();
    let original = books
        .record(sale("o", date!("2026-03-15"), "10.00"))
        .unwrap();
    let early = books
        .reversal(original.id, "r-early", date!("2026-03-14"))
        .unwrap();
    assert_eq!(code(books.record(early)), 402);
    let not_a_mirror = Entry::new("r-other", date!("2026-04-01"))
        .reverses(original.id)
        .credit("Assets:Cash", eur("9.00"), EUR)
        .debit("Income:Sales", eur("9.00"), EUR);
    assert_eq!(code(books.record(not_a_mirror)), 403);
    let mirror = books
        .reversal(original.id, "r", date!("2026-04-01"))
        .unwrap();
    books.record(mirror).unwrap();
}

// PoC: f9_turnover_overflow_breaks_audit_and_freezes_account
#[test]
fn ef9_i128_turnover_and_typed_overflow() {
    // At P = 6, 10¹³ units of turnover (beyond the 0.7 `i64` totals) book
    // and audit.
    let mut books = Journal::<6>::with_precision(ledger());
    books.declare_currency(EUR, 6, date!("2026-01-01")).unwrap();
    for path in ["Assets:Cash", "Assets:Other"] {
        books
            .open(path, AccountKind::Asset, date!("2026-01-01"))
            .unwrap();
    }
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026", date!("2026-01-01"), date!("2026-12-31"))
        .unwrap();
    let big: Amount<6> = "5000000000000.00".parse().unwrap();
    let transfer = |key: &str, debit: &str, credit: &str, amount: Amount<6>| {
        Entry::new(key, date!("2026-03-01"))
            .debit(debit, amount, EUR)
            .credit(credit, amount, EUR)
    };
    books
        .record(transfer("1", "Assets:Cash", "Income:Sales", big))
        .unwrap();
    books
        .record(transfer("2", "Assets:Other", "Assets:Cash", big))
        .unwrap();
    books
        .record(transfer("3", "Assets:Cash", "Assets:Other", big))
        .unwrap();
    books.audit().unwrap();
    // At the `i128` edge the overflow is a typed rejection, never a wrap.
    assert_eq!(
        rejected(books.record(transfer("over", "Assets:Cash", "Assets:Other", Amount::MAX))),
        Rejection::BalanceOverflow {
            account: Some("Assets:Cash".parse().unwrap()),
            currency: EUR,
            layer: Layer::Settled
        }
    );
    books.audit().unwrap();
}

// PoC: f12_period_deserialise_bypasses_constructor
#[test]
fn ef12_inverted_period_refused() {
    let mut books = journal();
    assert_eq!(
        rejected(books.define_period("x", date!("2026-04-10"), date!("2026-01-01"))),
        Rejection::InvalidPeriodRange {
            start: date!("2026-04-10"),
            end: date!("2026-01-01")
        }
    );
    // A period leaf whose dates were swapped does not even decode.
    books
        .define_period("x", date!("2026-01-01"), date!("2026-04-10"))
        .unwrap();
    let mut leaves = leaf_bytes(&books);
    let period = leaves.last_mut().unwrap();
    let dates = period.len() - 8;
    let (start, end) = period[dates..].split_at_mut(4);
    start.swap_with_slice(end);
    let failure = audit_leaves(&books, &leaves, &Claims::of(&books)).unwrap_err();
    assert_eq!(failure.position, Some(1));
    assert!(matches!(failure.kind, AuditFailureKind::Undecodable(_)));
}

#[test]
fn ef13_seal_commits_range_and_count() {
    let mut books = books();
    books
        .record(sale("s", date!("2026-01-10"), "1.00"))
        .unwrap();
    let receipt = books.seal_period("2026-01").unwrap();
    let parts = receipt.seal.parts();
    assert_eq!(
        (parts.start, parts.end),
        (date!("2026-01-01"), date!("2026-01-31"))
    );
    assert_eq!(parts.through, receipt.position - 1);
    assert_eq!(parts.accounts, 5);
    assert_eq!(parts.period.as_str(), "2026-01");
}

// PoC: fc2_registry_context_independent
#[test]
fn fc2_foreign_handle_refused() {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("A", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("B", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026", date!("2026-01-01"), date!("2026-12-31"))
        .unwrap();
    let foreign = Entry::new("f", date!("2026-03-20"))
        .debit(AccountHandle::new(7).unwrap(), eur("1.00"), EUR)
        .credit("A", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(foreign)),
        Rejection::UnknownAccount {
            account: AccountRef::Handle(AccountHandle::new(7).unwrap())
        }
    );
    // A posted-to leaf cannot grow a child; a parent cannot be posted to.
    books
        .record(
            Entry::new("p", date!("2026-03-20"))
                .debit("A", eur("1.00"), EUR)
                .credit("B", eur("1.00"), EUR),
        )
        .unwrap();
    assert_eq!(
        rejected(books.open("A:B", AccountKind::Asset, date!("2026-03-21"))),
        Rejection::ParentHasPostings {
            parent: "A".parse().unwrap()
        }
    );
    books
        .open("C:D", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("C", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    let to_parent = Entry::new("c", date!("2026-03-20"))
        .debit("C", eur("1.00"), EUR)
        .credit("B", eur("1.00"), EUR);
    assert_eq!(code(books.record(to_parent)), 303);
}

// PoC: qe1_entry_ids_are_not_committed_by_the_log
#[test]
fn qe1_swapped_keys_change_head() {
    let mut first = books();
    first
        .record(sale("a", date!("2026-03-10"), "10.00"))
        .unwrap();
    first
        .record(sale("b", date!("2026-03-10"), "99.00"))
        .unwrap();
    let mut swapped = books();
    swapped
        .record(sale("b", date!("2026-03-10"), "10.00"))
        .unwrap();
    swapped
        .record(sale("a", date!("2026-03-10"), "99.00"))
        .unwrap();
    assert_ne!(
        first.head(),
        swapped.head(),
        "the key, and so the id, is committed"
    );
    // A reversal names the derived id, so it reverses the same content in
    // every replica.
    let a = doubleentry::EntryId::derive(&ledger(), &IdempotencyKey::new("a").unwrap());
    let reversal = first.reversal(a, "rev", date!("2026-03-11")).unwrap();
    assert_eq!(reversal.postings()[0].amount(), eur("10.00"));
}

// PoC: qe4_from_records_accepts_duplicate_handles
#[test]
fn qe4_duplicate_registration_fails_replay() {
    let books = books();
    let mut leaves = leaf_bytes(&books);
    // Position 2 is the registration of handle 0 (Assets:Cash); repeat it.
    let duplicate = leaves[2].clone();
    leaves.push(duplicate);
    let failure = audit_leaves(&books, &leaves, &Claims::of(&books)).unwrap_err();
    assert_eq!(failure.position, Some(books.head().size));
    assert_eq!(
        failure.kind,
        AuditFailureKind::HandleMismatch {
            expected: 5,
            found: AccountHandle::new(0).unwrap()
        }
    );
}

// PoC: qe5_clearing_dates_unchecked
#[test]
fn qe5_clearing_dates_checked() {
    let mut books = books();
    let invoice = books
        .record(sale("a", date!("2026-03-10"), "10.00"))
        .unwrap();
    let payment = books
        .record(
            Entry::new("pay", date!("2026-03-11"))
                .debit("Income:Sales", eur("10.00"), EUR)
                .credit("Assets:Cash", eur("10.00"), EUR),
        )
        .unwrap();
    let debit = PostingRef::at(invoice.id, 0);
    let credit = PostingRef::at(payment.id, 1);
    assert_eq!(
        rejected(books.clear("early", debit, credit, eur("10.00"), date!("2026-03-05"))),
        Rejection::ClearingBeforePosting {
            posting: debit,
            booked: date!("2026-03-10")
        }
    );
    let applied = books
        .clear("c", debit, credit, eur("10.00"), date!("2026-06-30"))
        .unwrap()
        .position;
    assert_eq!(
        rejected(books.reset_clearing("r", applied, date!("1999-01-01"))),
        Rejection::NoPeriod {
            date: date!("1999-01-01")
        }
    );
    assert_eq!(
        rejected(books.reset_clearing("r", applied, date!("2026-06-01"))),
        Rejection::ResetBeforeClearing {
            cleared_on: date!("2026-06-30")
        }
    );
    for month in 1..=3 {
        books.seal_period(format!("2026-{month:02}")).unwrap();
    }
    assert_eq!(
        rejected(books.clear("back", debit, credit, eur("1.00"), date!("2026-03-15"))),
        Rejection::PeriodSealed {
            period: Label::new("2026-03").unwrap(),
            date: date!("2026-03-15")
        }
    );
}

// PoC: qe6_gap_entries_are_in_no_seals_coverage
#[test]
fn qe6_no_gap_can_exist() {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026-01", date!("2026-01-01"), date!("2026-01-31"))
        .unwrap();
    assert_eq!(
        rejected(books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))),
        Rejection::PeriodGap {
            expected_start: date!("2026-02-01"),
            start: date!("2026-03-01")
        }
    );
    assert_eq!(
        rejected(books.record(sale("feb", date!("2026-02-10"), "10.00"))),
        Rejection::NoPeriod {
            date: date!("2026-02-10")
        }
    );
}

// PoC: qe8_closing_refused_for_closed_income_account
#[test]
fn qe8_closing_names_closed_account() {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Old", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .open("Equity:Retained", AccountKind::Equity, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026", date!("2026-01-01"), date!("2026-12-31"))
        .unwrap();
    books
        .record(
            Entry::new("a", date!("2026-03-10"))
                .debit("Assets:Cash", eur("10.00"), EUR)
                .credit("Income:Old", eur("10.00"), EUR),
        )
        .unwrap();
    books
        .close_account("Income:Old", date!("2026-06-30"))
        .unwrap();
    assert_eq!(
        rejected(books.close_year("close", date!("2026-12-31"), "Equity:Retained")),
        Rejection::ClosingAccountNotOpen {
            account: "Income:Old".parse().unwrap(),
            date: date!("2026-12-31")
        }
    );
}

/// A view of a ledger with every handle in use.
struct Exhausted;

impl StateView<2> for Exhausted {
    fn header(&self) -> Header<2> {
        Header {
            ledger: ledger(),
            size: 1,
            accounts: u32::MAX,
            entries: 0,
            watermark: None,
            last_booking: None,
            last_period: None,
        }
    }
    fn entry_by_key(&self, _: &IdempotencyKey) -> Option<doubleentry::store::EntryMeta> {
        None
    }
    fn account_by_path(&self, _: &AccountPath) -> Option<doubleentry::store::AccountView> {
        None
    }
    fn account(&self, _: AccountHandle) -> Option<doubleentry::store::AccountView> {
        None
    }
    fn account_terms(
        &self,
        _: AccountHandle,
        _: doubleentry::Date,
    ) -> Option<doubleentry::store::Terms> {
        None
    }
    fn currency(&self, _: doubleentry::Currency, _: doubleentry::Date) -> Option<u8> {
        None
    }
    fn currency_latest(
        &self,
        _: doubleentry::Currency,
    ) -> Option<doubleentry::store::Versioned<u8>> {
        None
    }
    fn policy(&self, _: doubleentry::Date) -> LedgerPolicy {
        LedgerPolicy::default()
    }
    fn policy_latest(&self) -> Option<doubleentry::store::Versioned<LedgerPolicy>> {
        None
    }
    fn period_at(&self, _: doubleentry::Date) -> Option<doubleentry::store::PeriodView<2>> {
        None
    }
    fn period(&self, _: &doubleentry::PeriodId) -> Option<doubleentry::store::PeriodView<2>> {
        None
    }
    fn first_unsealed(&self) -> Option<doubleentry::store::PeriodView<2>> {
        None
    }
    fn entry(&self, _: doubleentry::EntryId) -> Option<doubleentry::store::EntryView<2>> {
        None
    }
    fn reversed_by(&self, _: doubleentry::EntryId) -> Option<doubleentry::EntryId> {
        None
    }
    fn hold(&self, _: doubleentry::EntryId) -> Option<doubleentry::store::HoldView<2>> {
        None
    }
    fn outstanding_hold_debits(&self, _: AccountHandle, _: doubleentry::Currency) -> Amount {
        Amount::ZERO
    }
    fn outstanding_hold_credits(&self, _: AccountHandle, _: doubleentry::Currency) -> Amount {
        Amount::ZERO
    }
    fn balance(&self, _: doubleentry::store::BalanceKey) -> Balance {
        Balance::ZERO
    }
    fn clearing_by_key(&self, _: &IdempotencyKey) -> Option<doubleentry::store::ClearingView<2>> {
        None
    }
    fn clearing_at(&self, _: u64) -> Option<doubleentry::store::ClearingView<2>> {
        None
    }
    fn applied(&self, _: PostingRef) -> Amount {
        Amount::ZERO
    }
    fn all_balances(&self) -> Vec<(doubleentry::store::BalanceKey, Balance)> {
        Vec::new()
    }
    fn postings_after(&self, _: doubleentry::Date) -> Vec<doubleentry::store::Movement<2>> {
        Vec::new()
    }
    fn pnl_slices(&self, _: doubleentry::Date) -> Vec<doubleentry::store::Slice<2>> {
        Vec::new()
    }
}

#[test]
fn qe11_handle_space_exhausted() {
    let op = Op::Register {
        spec: AccountSpec::new("Assets:New", AccountKind::Asset, date!("2026-01-01")),
        actor: "setup".into(),
    };
    assert_eq!(execute(&Exhausted, &op), Err(Rejection::TooManyAccounts));
}

#[test]
fn a2_identical_retry_replays() {
    let mut books = books();
    let captured = doubleentry::CaptureTime::new(1_767_225_600, 0).unwrap();
    let draft = sale("a2", date!("2026-01-15"), "10.00").captured_at(captured);
    let first = books.record(draft.clone()).unwrap();
    let again = books.record(draft).unwrap();
    assert!(again.replayed && again.id == first.id);
    let recaptured = doubleentry::CaptureTime::new(1_767_225_601, 0).unwrap();
    assert_eq!(
        code(books.record(sale("a2", date!("2026-01-15"), "10.00").captured_at(recaptured))),
        200
    );
}

#[test]
fn a4_backdated_entry_meets_todays_limit() {
    let mut books = books();
    books
        .open("Assets:Card", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .set_limit(
            "Assets:Card",
            BalanceLimit::NoDebitBalance,
            date!("2026-07-01"),
        )
        .unwrap();
    // Dated before the limit takes effect, recorded after: the latest limit
    // applies.
    let backdated = Entry::new("back", date!("2026-06-30"))
        .debit("Assets:Card", eur("5.00"), EUR)
        .credit("Assets:Cash", eur("5.00"), EUR);
    assert_eq!(code(books.record(backdated)), 501);
}

#[test]
fn a16_noop_is_idempotent() {
    let mut books = books();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-07-01"),
        )
        .unwrap();
    let size = books.head().size;
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-07-01"),
        )
        .unwrap();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-08-01"),
        )
        .unwrap();
    books.declare_currency(EUR, 2, date!("2026-09-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026-01", date!("2026-01-01"), date!("2026-01-31"))
        .unwrap();
    assert_eq!(books.head().size, size);
}

#[test]
fn p1_no_stale_validation() {
    // Built while March is open, recorded after March is sealed: refused.
    let mut books = books();
    let draft = sale("late", date!("2026-03-20"), "999.00");
    assert!(doubleentry::validate(&draft).is_empty());
    for month in 1..=3 {
        books.seal_period(format!("2026-{month:02}")).unwrap();
    }
    assert_eq!(code(books.record(draft)), 301);
}

// PoC: z_probe_store
#[test]
fn p4_retry_after_seal_replays() {
    let mut books = books();
    let first = books
        .record(sale("m-1", date!("2026-03-10"), "10.00"))
        .unwrap();
    for month in 1..=3 {
        books.seal_period(format!("2026-{month:02}")).unwrap();
    }
    let retry = books
        .record(sale("m-1", date!("2026-03-10"), "10.00"))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!((retry.id, retry.position), (first.id, first.position));
}

#[test]
fn p6_from_log_audit_compares_seals() {
    let mut books = books();
    books
        .record(sale("a", date!("2026-01-10"), "10.00"))
        .unwrap();
    let receipt = books.seal_period("2026-01").unwrap();
    books
        .record(sale("b", date!("2026-02-10"), "5.00"))
        .unwrap();
    let mut restored: Journal = Journal::from_log(books.id(), books.leaves()).unwrap();
    assert_eq!(restored.head(), books.head());
    assert_eq!(restored.seal_period("2026-01").unwrap(), receipt);
    // A restore missing the seal leaf is not the claimed book.
    let mut leaves = leaf_bytes(&books);
    leaves.remove(usize::try_from(receipt.position).unwrap());
    let failure = audit_leaves(&books, &leaves, &Claims::of(&books)).unwrap_err();
    assert_eq!(failure.kind, AuditFailureKind::HeadMismatch);
}

// PoC: poc_g1_define_period_reopens_sealed_and_moves_range
#[test]
fn g1_a_period_cannot_be_redefined() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        code(books.define_period("2026-01", date!("2026-01-01"), date!("2026-02-15"))),
        425
    );
    books
        .define_period("2026-01", date!("2026-01-01"), date!("2026-01-31"))
        .unwrap();
}

// PoC: poc_f2_reversal_replay_with_new_id_refused_by_sqlite_only
#[test]
fn f2_a_reversal_retry_replays() {
    let mut books = books();
    let original = books
        .record(sale("o", date!("2026-03-10"), "10.00"))
        .unwrap();
    let reversal = books
        .reversal(original.id, "r", date!("2026-03-11"))
        .unwrap();
    let first = books.record(reversal.clone()).unwrap();
    let again = books.record(reversal).unwrap();
    assert!(again.replayed);
    assert_eq!(again.position, first.position);
}
