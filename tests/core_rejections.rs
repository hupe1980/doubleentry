//! The exact-rejection table: every code the rule core owns, asserted with its
//! variant and fields on `Journal`, plus precedence between and within stages,
//! retries and reversals.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{books, code, eur, journal, ledger, rejected, sale};
use doubleentry::{
    AccountHandle, AccountKind, AccountPath, AccountRef, AccountSpec, Amount, BalanceLimit,
    CaptureTime, Currency, Document, Entry, EntryId, Error, IdempotencyKey, Journal, Label, Layer,
    LedgerPolicy, MemoSalt, Posting, PostingRef, Rate, Recorded, Rejection, Side,
    currency::{EUR, JPY, USD},
    date,
    error::{LabelField, ScaledAmount, Stage, ValueError},
    record::MAX_LEAF_BYTES,
    store::BalanceKey,
    validate,
};

fn scaled(text: &str) -> ScaledAmount {
    ScaledAmount::from(eur(text))
}

fn path(text: &str) -> AccountPath {
    text.parse().unwrap()
}

fn label(text: &str) -> Label {
    Label::new(text).unwrap()
}

/// One instance of every variant the rule core owns.
fn every_rejection() -> Vec<Rejection> {
    let id = EntryId::from_bytes([1; 16]);
    let posting = PostingRef::at(id, 0);
    let key = BalanceKey {
        handle: AccountHandle::new(0).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
    };
    let day = date!("2026-03-15");
    vec![
        Rejection::InvalidKey {
            reason: ValueError::Empty,
        },
        Rejection::TooFewPostings { count: 1 },
        Rejection::TooManyPostings {
            count: 1025,
            max: 1024,
        },
        Rejection::InvalidPath {
            posting: Some(0),
            reason: ValueError::Empty,
        },
        Rejection::NonPositiveAmount { posting: 0 },
        Rejection::InvalidDimensions {
            posting: 0,
            reason: ValueError::TooManyDimensions,
        },
        Rejection::Unbalanced {
            currency: EUR,
            layer: Layer::Settled,
            debit: scaled("10.00"),
            credit: scaled("9.00"),
        },
        Rejection::InvalidLabel {
            field: LabelField::Kind,
            reason: ValueError::Empty,
        },
        Rejection::InvalidMemo {
            reason: ValueError::Empty,
        },
        Rejection::TooManyDocuments { count: 17, max: 16 },
        Rejection::TooManyRates { count: 17, max: 16 },
        Rejection::RecordTooLarge {
            bytes: 70_000,
            max: MAX_LEAF_BYTES,
        },
        Rejection::TotalOutOfRange {
            currency: EUR,
            layer: Layer::Settled,
        },
        Rejection::MemoSaltMissing,
        Rejection::NonPositiveClearingAmount,
        Rejection::InvalidPeriodRange {
            start: day,
            end: day,
        },
        Rejection::InvalidOpenWindow {
            open_from: day,
            open_to: day,
        },
        Rejection::ExponentTooLarge {
            exponent: 3,
            max: 2,
        },
        Rejection::KeyConflict { position: 1 },
        Rejection::NoPeriod { date: day },
        Rejection::PeriodSealed {
            period: label("2026-03"),
            date: day,
        },
        Rejection::UnknownAccount {
            account: AccountRef::Path(path("Assets:Nope")),
        },
        Rejection::NotALeaf {
            account: path("Assets"),
        },
        Rejection::AccountNotOpen {
            account: path("Assets:Cash"),
            date: day,
        },
        Rejection::UndeclaredCurrency {
            currency: USD,
            date: day,
        },
        Rejection::ExcessPrecision {
            posting: 0,
            currency: JPY,
            exponent: 0,
        },
        Rejection::MissingDimension {
            posting: 0,
            axis: label("segment"),
        },
        Rejection::ValueDateDrift { days: 10, max: 5 },
        Rejection::AtOrBelowWatermark {
            effective: day,
            watermark: day,
        },
        Rejection::EffectiveDateRegresses {
            effective: day,
            latest: day,
        },
        Rejection::UnknownPeriod {
            period: label("2027-01"),
        },
        Rejection::ReversalTargetUnknown { target: id },
        Rejection::AlreadyReversed { target: id, by: id },
        Rejection::ReversalBeforeTarget {
            target: id,
            booked: day,
        },
        Rejection::NotAMirror {
            target: id,
            posting: None,
        },
        Rejection::ClearingTargetUnknown { posting },
        Rejection::ClearingSideMismatch {
            posting,
            expected: Side::Debit,
        },
        Rejection::ClearingKeyMismatch {
            debit: key,
            credit: key,
        },
        Rejection::ClearingPrecision {
            currency: EUR,
            exponent: 2,
        },
        Rejection::OverApplied {
            posting,
            open: scaled("0.00"),
            requested: scaled("1.00"),
        },
        Rejection::ClearingBeforePosting {
            posting,
            booked: day,
        },
        Rejection::ClearingUnknown { clearing: 3 },
        Rejection::ClearingAlreadyReset { clearing: 3, by: 4 },
        Rejection::ResetBeforeClearing { cleared_on: day },
        Rejection::ClearingNotSettled { posting },
        Rejection::AccountExists {
            path: path("Assets:Cash"),
            handle: AccountHandle::new(0).unwrap(),
        },
        Rejection::ParentHasPostings {
            parent: path("Assets:Cash"),
        },
        Rejection::ClassFrozen {
            account: path("Assets:Cash"),
        },
        Rejection::PostingsAfterClose {
            account: path("Assets:Cash"),
            last_booked: day,
        },
        Rejection::ExponentLowered {
            currency: EUR,
            current: 2,
            requested: 1,
        },
        Rejection::PeriodExists {
            period: label("2026-03"),
            start: day,
            end: day,
        },
        Rejection::PeriodGap {
            expected_start: day,
            start: day,
        },
        Rejection::PredecessorNotSealed {
            predecessor: label("2026-02"),
            earliest: label("2026-01"),
        },
        Rejection::TooManyAccounts,
        Rejection::ClosingEquityInScope {
            account: path("Income:Sales"),
        },
        Rejection::ClosingAccountNotOpen {
            account: path("Income:Old"),
            date: day,
        },
        Rejection::PolicyBeforeBookings {
            effective: day,
            last_booking: day,
        },
        Rejection::BalanceOverflow {
            account: None,
            currency: EUR,
            layer: Layer::Settled,
        },
        Rejection::LimitExceeded {
            account: path("Liabilities:Wallet"),
            currency: EUR,
            limit: BalanceLimit::NoDebitBalance,
            shortfall: scaled("30.00"),
        },
    ]
}

// ── The code catalogue ────────────────────────────────────────────────────────

#[test]
fn codes_are_unique_ranged_and_ordered() {
    let rejections = every_rejection();
    assert_eq!(rejections.len(), 59, "every variant the core owns");
    let mut codes: Vec<u16> = rejections.iter().map(Rejection::code).collect();
    for rejection in &rejections {
        let stage = match rejection.code() / 100 {
            1 => Stage::Structural,
            2 => Stage::Idempotency,
            3 => Stage::Context,
            4 => Stage::References,
            5 => Stage::Limits,
            other => panic!("code outside every stage: {other}"),
        };
        assert_eq!(rejection.stage(), stage, "{rejection:?}");
        assert!(
            !(140..150).contains(&rejection.code()) && !(440..460).contains(&rejection.code()),
            "the hold ranges are reserved: {rejection:?}"
        );
        let text = rejection.to_string();
        assert!(
            text.starts_with(&format!("[{}] ", rejection.code())),
            "{text}"
        );
    }
    let listed = codes.clone();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), 59, "codes are unique");
    assert_eq!(listed, codes, "listed in precedence order");
    assert!(!codes.contains(&112), "112 is unassigned");
    assert!(Stage::Structural < Stage::Idempotency && Stage::References < Stage::Limits);
}

#[test]
fn scaled_amount_display() {
    let thirty = ScaledAmount {
        minor: 3000,
        scale: 2,
    };
    assert_eq!(thirty.to_string(), "30.00");
    assert_eq!(
        ScaledAmount {
            minor: -5,
            scale: 3
        }
        .to_string(),
        "-0.005"
    );
    assert_eq!(ScaledAmount { minor: 7, scale: 0 }.to_string(), "7");
    assert_eq!(
        Rejection::LimitExceeded {
            account: path("Liabilities:Wallets:alice"),
            currency: EUR,
            limit: BalanceLimit::NoDebitBalance,
            shortfall: thirty,
        }
        .to_string(),
        "[501] limit exceeded on Liabilities:Wallets:alice in EUR: short by 30.00"
    );
}

// ── US2: recording ────────────────────────────────────────────────────────────

#[test]
fn us2_a_valid_draft_is_recorded_with_its_derived_id() {
    let mut books = books();
    let position = books.head().size;
    let recorded = books
        .record(sale("inv-1", date!("2026-01-15"), "1190.00"))
        .unwrap();
    let key = IdempotencyKey::new("inv-1").unwrap();
    assert_eq!(recorded.id, EntryId::derive(&ledger(), &key));
    assert_eq!(recorded.position, position);
    assert!(!recorded.replayed);
    assert_eq!(books.head().size, position + 1);
}

#[test]
fn us2_an_identical_retry_after_the_seal_replays() {
    let mut books = books();
    let first = books
        .record(sale("inv-1", date!("2026-01-15"), "1190.00"))
        .unwrap();
    books.seal_period("2026-01").unwrap();
    let size = books.head().size;
    let again = books
        .record(sale("inv-1", date!("2026-01-15"), "1190.00"))
        .unwrap();
    assert_eq!((again.id, again.position), (first.id, first.position));
    assert!(again.replayed);
    assert_eq!(books.head().size, size, "a replay appends nothing");
}

#[test]
fn us2_other_content_under_a_used_key_is_a_key_conflict() {
    let mut books = books();
    let first = books
        .record(sale("inv-1", date!("2026-01-15"), "1190.00"))
        .unwrap();
    assert_eq!(
        rejected(books.record(sale("inv-1", date!("2026-01-15"), "1190.01"))),
        Rejection::KeyConflict {
            position: first.position
        }
    );
}

#[test]
fn us2_structural_precedes_context() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    let unbalanced_in_sealed_january = Entry::new("bad", date!("2026-01-15"))
        .debit("Assets:Cash", eur("10.00"), EUR)
        .credit("Income:Sales", eur("9.00"), EUR);
    assert_eq!(
        rejected(books.record(unbalanced_in_sealed_january)),
        Rejection::Unbalanced {
            currency: EUR,
            layer: Layer::Settled,
            debit: scaled("10.00"),
            credit: scaled("9.00"),
        }
    );
}

#[test]
fn us2_a_wallet_limit_refuses_the_overdraw_and_accepts_the_exact_draw() {
    let mut books = books();
    let fund = Entry::new("fund", date!("2026-02-01"))
        .debit("Assets:Cash", eur("100.00"), EUR)
        .credit("Liabilities:Wallet", eur("100.00"), EUR);
    books.record(fund).unwrap();
    let draw = |key: &str, amount: &str| {
        Entry::new(key, date!("2026-02-02"))
            .debit("Liabilities:Wallet", eur(amount), EUR)
            .credit("Assets:Cash", eur(amount), EUR)
    };
    assert_eq!(
        rejected(books.record(draw("draw-130", "130.00"))),
        Rejection::LimitExceeded {
            account: path("Liabilities:Wallet"),
            currency: EUR,
            limit: BalanceLimit::NoDebitBalance,
            shortfall: scaled("30.00"),
        }
    );
    books.record(draw("draw-100", "100.00")).unwrap();
}

#[test]
fn us2_a_repayment_into_an_account_beyond_a_tightened_limit_passes() {
    let mut books = books();
    books
        .open("Assets:Overdraft", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    let lend = |key: &str, amount: &str| {
        Entry::new(key, date!("2026-02-10"))
            .debit("Assets:Overdraft", eur(amount), EUR)
            .credit("Assets:Cash", eur(amount), EUR)
    };
    let repay = |key: &str, amount: &str| {
        Entry::new(key, date!("2026-02-11"))
            .debit("Assets:Cash", eur(amount), EUR)
            .credit("Assets:Overdraft", eur(amount), EUR)
    };
    books.record(lend("lend", "50.00")).unwrap();
    books
        .set_limit(
            "Assets:Overdraft",
            BalanceLimit::NoDebitBalance,
            date!("2026-02-10"),
        )
        .unwrap();
    books.record(repay("repay", "20.00")).unwrap();
    assert_eq!(
        rejected(books.record(lend("lend-more", "1.00"))),
        Rejection::LimitExceeded {
            account: path("Assets:Overdraft"),
            currency: EUR,
            limit: BalanceLimit::NoDebitBalance,
            shortfall: scaled("31.00"),
        }
    );
}

#[test]
fn us2_validate_lists_every_structural_violation_in_order() {
    let draft: Entry = Entry::new("", date!("2026-01-15"))
        .debit("Assets::Cash", eur("0.00"), EUR)
        .kind("")
        .memo("");
    let codes: Vec<u16> = validate(&draft).iter().map(Rejection::code).collect();
    // No 114: the façade salts the memo when it records the draft.
    assert_eq!(codes, [100, 101, 103, 104, 107, 108]);
}

// ── The table: structural (1xx) ───────────────────────────────────────────────

fn balanced(key: &str) -> Entry {
    sale(key, date!("2026-01-15"), "10.00")
}

#[test]
fn row_100_invalid_key() {
    let mut books = books();
    let draft = Entry::new("", date!("2026-01-15"))
        .debit("Assets:Cash", eur("1.00"), EUR)
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::InvalidKey {
            reason: ValueError::Empty
        }
    );
    let long = "k".repeat(129);
    assert_eq!(
        rejected(books.record(sale(&long, date!("2026-01-15"), "1.00"))),
        Rejection::InvalidKey {
            reason: ValueError::TooLong {
                max: 128,
                found: 129
            }
        }
    );
}

#[test]
fn row_101_too_few_postings() {
    let mut books = books();
    let lone = Entry::new("lone", date!("2026-01-15")).debit("Assets:Cash", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(lone)),
        Rejection::TooFewPostings { count: 1 }
    );
    assert_eq!(
        rejected(books.record(Entry::new("none", date!("2026-01-15")))),
        Rejection::TooFewPostings { count: 0 }
    );
}

#[test]
fn row_102_too_many_postings() {
    let mut books = books();
    let draft = (0..1024).fold(
        Entry::new("many", date!("2026-01-15")).credit("Income:Sales", eur("10.24"), EUR),
        |draft, _| draft.debit("Assets:Cash", eur("0.01"), EUR),
    );
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::TooManyPostings {
            count: 1025,
            max: 1024
        }
    );
}

#[test]
fn row_103_invalid_path() {
    let mut books = books();
    let draft = Entry::new("bad-path", date!("2026-01-15"))
        .debit("Assets:Cash", eur("1.00"), EUR)
        .credit("Income::Sales", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::InvalidPath {
            posting: Some(1),
            reason: ValueError::Empty
        }
    );
}

#[test]
fn row_104_non_positive_amount() {
    let mut books = books();
    let draft = Entry::new("zero", date!("2026-01-15"))
        .debit("Assets:Cash", eur("0.00"), EUR)
        .credit("Income:Sales", eur("0.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::NonPositiveAmount { posting: 0 }
    );
    let negative = Entry::new("negative", date!("2026-01-15"))
        .debit("Assets:Cash", eur("1.00"), EUR)
        .credit("Income:Sales", eur("-1.00"), EUR);
    assert_eq!(
        rejected(books.record(negative)),
        Rejection::NonPositiveAmount { posting: 1 }
    );
}

#[test]
fn row_105_invalid_dimensions() {
    let mut books = books();
    let ninth = (0..9).fold(
        Posting::debit("Assets:Cash", eur("1.00"), EUR),
        |posting, axis| posting.dimension(format!("axis{axis}"), "x"),
    );
    let draft = Entry::new("dims", date!("2026-01-15"))
        .posting(ninth)
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::InvalidDimensions {
            posting: 0,
            reason: ValueError::TooManyDimensions
        }
    );
}

#[test]
fn row_106_unbalanced_reports_the_first_pair() {
    let mut books = books();
    books.declare_currency(USD, 2, date!("2026-01-01")).unwrap();
    let draft = Entry::new("two-pairs", date!("2026-01-15"))
        .debit("Assets:Cash", eur("5.00"), USD)
        .credit("Income:Sales", eur("4.00"), USD)
        .debit("Assets:Cash", eur("10.00"), EUR)
        .credit("Income:Sales", eur("9.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::Unbalanced {
            currency: EUR,
            layer: Layer::Settled,
            debit: scaled("10.00"),
            credit: scaled("9.00"),
        }
    );
}

#[test]
fn row_107_invalid_kind() {
    let mut books = books();
    assert_eq!(
        rejected(books.record(balanced("kind").kind(" padded"))),
        Rejection::InvalidLabel {
            field: LabelField::Kind,
            reason: ValueError::EdgeWhitespace
        }
    );
}

#[test]
fn row_108_invalid_memo() {
    let mut books = books();
    assert_eq!(
        rejected(books.record(balanced("memo").memo("x".repeat(4097)))),
        Rejection::InvalidMemo {
            reason: ValueError::TooLong {
                max: 4096,
                found: 4097
            }
        }
    );
}

#[test]
fn row_109_too_many_documents() {
    let mut books = books();
    let draft = (0..17).fold(balanced("docs"), |draft, n| {
        draft.document(Document::new("invoice", format!("RE-{n}")).unwrap())
    });
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::TooManyDocuments { count: 17, max: 16 }
    );
}

#[test]
fn row_110_too_many_rates() {
    let mut books = books();
    let quotes = [
        "AUD", "BHD", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "GBP", "HKD", "HUF", "INR", "JPY",
        "KRW", "KWD", "MXN", "NOK",
    ];
    let draft = quotes.iter().fold(balanced("rates"), |draft, quote| {
        draft.rate(Rate::new(EUR, Currency::new(quote).unwrap(), 11, -1).unwrap())
    });
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::TooManyRates { count: 17, max: 16 }
    );
}

#[test]
fn row_111_record_too_large() {
    let mut books = books();
    let wide = |posting: Posting| {
        (0..8).fold(posting, |posting, axis| {
            posting.dimension(format!("{axis}{}", "a".repeat(63)), "v".repeat(64))
        })
    };
    let draft = (0..512).fold(Entry::new("wide", date!("2026-01-15")), |draft, _| {
        draft
            .posting(wide(Posting::debit("Assets:Cash", eur("1.00"), EUR)))
            .posting(wide(Posting::credit("Income:Sales", eur("1.00"), EUR)))
    });
    let bytes = draft.encoded_len();
    assert!(bytes > MAX_LEAF_BYTES);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::RecordTooLarge {
            bytes,
            max: MAX_LEAF_BYTES
        }
    );
}

#[test]
fn row_113_total_out_of_range() {
    let mut books = books();
    let draft = Entry::new("huge", date!("2026-01-15"))
        .debit("Assets:Cash", Amount::MAX, EUR)
        .debit("Expenses:Rent", Amount::MAX, EUR)
        .credit("Income:Sales", Amount::MAX, EUR)
        .credit("Equity:Retained", Amount::MAX, EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::TotalOutOfRange {
            currency: EUR,
            layer: Layer::Settled
        }
    );
}

#[test]
fn row_114_a_memo_without_salt_never_reaches_the_core_from_a_facade() {
    // Façades salt every memo; only a store driving the core protocol
    // directly can submit one without a salt (114). `validate` lists what a
    // form must fix, so it does not report a salt the façade will supply.
    let draft = balanced("memo-salt").memo("Rechnung 4711");
    assert!(validate(&draft).is_empty());
    assert!(validate(&draft.memo_salted("Rechnung 4711", MemoSalt([1; 16]))).is_empty());
    let mut books = books();
    books
        .record(balanced("memo-salt").memo("Rechnung 4711"))
        .unwrap();
}

#[test]
fn rows_120_to_123_structural_master_data_and_clearing() {
    let mut books = books();
    let paid = books.record(balanced("paid")).unwrap();
    let posting = PostingRef::at(paid.id, 0);
    assert_eq!(
        rejected(books.clear("clr", posting, posting, Amount::ZERO, date!("2026-01-20"))),
        Rejection::NonPositiveClearingAmount
    );
    assert_eq!(
        rejected(books.define_period("2027-01", date!("2027-01-31"), date!("2027-01-01"))),
        Rejection::InvalidPeriodRange {
            start: date!("2027-01-31"),
            end: date!("2027-01-01")
        }
    );
    assert_eq!(
        rejected(
            books.register(
                AccountSpec::new("Assets:Short", AccountKind::Asset, date!("2026-05-01"))
                    .open_to(date!("2026-04-30"))
            )
        ),
        Rejection::InvalidOpenWindow {
            open_from: date!("2026-05-01"),
            open_to: date!("2026-04-30")
        }
    );
    assert_eq!(
        rejected(books.declare_currency(USD, 3, date!("2026-01-01"))),
        Rejection::ExponentTooLarge {
            exponent: 3,
            max: 2
        }
    );
}

// ── Idempotency (2xx) and context (3xx) ──────────────────────────────────────

#[test]
fn row_300_no_period() {
    let mut books = books();
    assert_eq!(
        rejected(books.record(sale("next-year", date!("2027-01-05"), "1.00"))),
        Rejection::NoPeriod {
            date: date!("2027-01-05")
        }
    );
}

#[test]
fn row_301_period_sealed() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.record(sale("late", date!("2026-01-31"), "1.00"))),
        Rejection::PeriodSealed {
            period: label("2026-01"),
            date: date!("2026-01-31")
        }
    );
}

#[test]
fn row_302_unknown_account() {
    let mut books = books();
    let draft = Entry::new("unknown", date!("2026-01-15"))
        .debit("Assets:Nope", eur("1.00"), EUR)
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::UnknownAccount {
            account: AccountRef::Path(path("Assets:Nope"))
        }
    );
}

#[test]
fn row_303_not_a_leaf() {
    let mut books = books();
    books
        .open("Assets:Cash:Petty", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    assert_eq!(
        rejected(books.record(balanced("parent"))),
        Rejection::NotALeaf {
            account: path("Assets:Cash")
        }
    );
}

#[test]
fn row_304_account_not_open() {
    let mut books = books();
    books
        .open("Assets:Bank", AccountKind::Asset, date!("2026-06-01"))
        .unwrap();
    let early = Entry::new("early", date!("2026-03-15"))
        .debit("Assets:Bank", eur("1.00"), EUR)
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(early)),
        Rejection::AccountNotOpen {
            account: path("Assets:Bank"),
            date: date!("2026-03-15")
        }
    );
}

#[test]
fn row_305_undeclared_currency() {
    let mut books = books();
    let draft = Entry::new("usd", date!("2026-01-15"))
        .debit("Assets:Cash", eur("1.00"), USD)
        .credit("Income:Sales", eur("1.00"), USD);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::UndeclaredCurrency {
            currency: USD,
            date: date!("2026-01-15")
        }
    );
}

#[test]
fn row_306_excess_precision_is_refused_never_rounded() {
    let mut books = books();
    books.declare_currency(JPY, 0, date!("2026-01-01")).unwrap();
    let draft = Entry::new("yen", date!("2026-01-15"))
        .debit("Assets:Cash", eur("1.50"), JPY)
        .credit("Income:Sales", eur("1.50"), JPY);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::ExcessPrecision {
            posting: 0,
            currency: JPY,
            exponent: 0
        }
    );
    let mut fine = Journal::<3>::with_precision(ledger());
    fine.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    fine.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    fine.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    fine.define_period("2026", date!("2026-01-01"), date!("2026-12-31"))
        .unwrap();
    let tenth_of_a_cent: Amount<3> = "0.001".parse().unwrap();
    let draft = Entry::new("milli", date!("2026-01-15"))
        .debit("Assets:Cash", tenth_of_a_cent, EUR)
        .credit("Income:Sales", tenth_of_a_cent, EUR);
    assert_eq!(
        rejected(fine.record(draft)),
        Rejection::ExcessPrecision {
            posting: 0,
            currency: EUR,
            exponent: 2
        }
    );
}

#[test]
fn row_307_missing_dimension() {
    let mut books = books();
    books
        .set_policy(
            LedgerPolicy::new([label("segment")], None).unwrap(),
            date!("2026-01-01"),
        )
        .unwrap();
    let draft = Entry::new("no-segment", date!("2026-01-15"))
        .posting(Posting::debit("Assets:Cash", eur("1.00"), EUR).dimension("segment", "Retail"))
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::MissingDimension {
            posting: 1,
            axis: label("segment")
        }
    );
}

#[test]
fn row_308_value_date_drift() {
    let mut books = books();
    books
        .set_policy(LedgerPolicy::new([], Some(5)).unwrap(), date!("2026-01-01"))
        .unwrap();
    let draft = balanced("drift").value_date(date!("2026-01-25"));
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::ValueDateDrift { days: 10, max: 5 }
    );
    books
        .record(balanced("near").value_date(date!("2026-01-20")))
        .unwrap();
}

#[test]
fn rows_309_to_311_watermark_regression_and_unknown_period() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.declare_currency(USD, 2, date!("2026-01-31"))),
        Rejection::AtOrBelowWatermark {
            effective: date!("2026-01-31"),
            watermark: date!("2026-01-31")
        }
    );
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-03-01"),
        )
        .unwrap();
    assert_eq!(
        rejected(books.set_limit("Assets:Cash", BalanceLimit::Unlimited, date!("2026-02-15"))),
        Rejection::EffectiveDateRegresses {
            effective: date!("2026-02-15"),
            latest: date!("2026-03-01")
        }
    );
    assert_eq!(
        rejected(books.seal_period("2027-01")),
        Rejection::UnknownPeriod {
            period: label("2027-01")
        }
    );
}

// ── Limits (5xx) ──────────────────────────────────────────────────────────────

#[test]
fn row_500_balance_overflow_never_wraps() {
    let mut books = books();
    books
        .record(
            Entry::new("max", date!("2026-01-15"))
                .debit("Assets:Cash", Amount::MAX, EUR)
                .credit("Income:Sales", Amount::MAX, EUR),
        )
        .unwrap();
    assert_eq!(
        rejected(books.record(balanced("one-more"))),
        Rejection::BalanceOverflow {
            account: Some(path("Assets:Cash")),
            currency: EUR,
            layer: Layer::Settled
        }
    );
}

// ── US6: reversals (4xx) ──────────────────────────────────────────────────────

fn rent(books: &mut Journal) -> Recorded {
    books
        .record(
            Entry::new("rent-03", date!("2026-03-05"))
                .debit("Expenses:Rent", eur("800.00"), EUR)
                .credit("Assets:Cash", eur("800.00"), EUR),
        )
        .unwrap()
}

#[test]
fn us6_a_reversal_mirrors_its_target_once() {
    let mut books = books();
    let original = rent(&mut books);
    books.seal_period("2026-01").unwrap();
    let reversal = books
        .reversal(original.id, "rev-1", date!("2026-04-01"))
        .unwrap();
    assert_eq!(reversal.get_reverses(), Some(original.id));
    let sides: Vec<Side> = reversal.postings().iter().map(Posting::side).collect();
    assert_eq!(sides, [Side::Credit, Side::Debit]);
    let reversed = books.record(reversal).unwrap();
    let again = books
        .reversal(original.id, "rev-2", date!("2026-04-02"))
        .unwrap();
    assert_eq!(
        rejected(books.record(again)),
        Rejection::AlreadyReversed {
            target: original.id,
            by: reversed.id
        }
    );
    // A reversal of a reversal is allowed.
    let back = books
        .reversal(reversed.id, "rev-of-rev", date!("2026-04-03"))
        .unwrap();
    books.record(back).unwrap();
}

#[test]
fn row_400_reversal_target_unknown() {
    let mut books = books();
    let ghost = EntryId::from_bytes([9; 16]);
    let draft = balanced("rev-ghost").reverses(ghost);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::ReversalTargetUnknown { target: ghost }
    );
    assert!(matches!(
        books.reversal(ghost, "rev", date!("2026-01-15")),
        Err(Error::Query(_))
    ));
}

#[test]
fn row_402_reversal_before_target() {
    let mut books = books();
    let original = rent(&mut books);
    let early = books
        .reversal(original.id, "rev-early", date!("2026-03-04"))
        .unwrap();
    assert_eq!(
        rejected(books.record(early)),
        Rejection::ReversalBeforeTarget {
            target: original.id,
            booked: date!("2026-03-05")
        }
    );
}

#[test]
fn row_403_not_a_mirror() {
    let mut books = books();
    let original = rent(&mut books);
    let changed = Entry::new("rev-changed", date!("2026-03-06"))
        .reverses(original.id)
        .debit("Assets:Cash", eur("800.00"), EUR)
        .credit("Expenses:Rent", eur("800.00"), EUR);
    // Same amounts, but the legs are in another order: posting 0 differs.
    assert_eq!(
        rejected(books.record(changed)),
        Rejection::NotAMirror {
            target: original.id,
            posting: Some(0)
        }
    );
    let fewer = Entry::new("rev-three", date!("2026-03-06"))
        .reverses(original.id)
        .credit("Expenses:Rent", eur("800.00"), EUR)
        .debit("Assets:Cash", eur("400.00"), EUR)
        .debit("Assets:Cash", eur("400.00"), EUR);
    assert_eq!(
        rejected(books.record(fewer)),
        Rejection::NotAMirror {
            target: original.id,
            posting: None
        }
    );
}

// ── Precedence (Q-E9: one order for every façade and store) ──────────────────

#[test]
fn qe9_one_precedence() {
    let mut books = books();
    let first = books.record(balanced("used")).unwrap();
    books.seal_period("2026-01").unwrap();

    // Structural before idempotency: a malformed draft under a used key.
    let malformed = Entry::new("used", date!("2026-01-15")).debit("Assets:Cash", eur("1.00"), EUR);
    assert_eq!(code(books.record(malformed)), 101);
    // Idempotency before context: other content under a used key, dated in a
    // sealed period, is a key conflict…
    assert_eq!(
        rejected(books.record(sale("used", date!("2026-01-15"), "11.00"))),
        Rejection::KeyConflict {
            position: first.position
        }
    );
    // …and the same content a replay.
    assert!(books.record(balanced("used")).unwrap().replayed);
    // Context before references: a reversal of an unknown entry dated in no
    // period.
    let ghost =
        sale("ghost-rev", date!("2027-02-01"), "1.00").reverses(EntryId::from_bytes([3; 16]));
    assert_eq!(code(books.record(ghost)), 300);
    // References before limits: a non-mirror reversal that would overdraw the
    // limited wallet.
    let overdraw = Entry::new("rev-wallet", date!("2026-02-01"))
        .reverses(first.id)
        .debit("Liabilities:Wallet", eur("99.00"), EUR)
        .credit("Assets:Cash", eur("99.00"), EUR);
    assert_eq!(code(books.record(overdraw)), 403);

    // Inside the structural stage: one posting is also unbalanced → 101.
    let lone = Entry::new("lone", date!("2026-02-01")).debit("Assets:Cash", eur("1.00"), EUR);
    assert_eq!(code(books.record(lone)), 101);
    // Inside the context stage: no period beats an unknown account.
    let nowhere = Entry::new("nowhere", date!("2027-02-01"))
        .debit("Assets:Nope", eur("1.00"), EUR)
        .credit("Income:Sales", eur("1.00"), EUR);
    assert_eq!(code(books.record(nowhere)), 300);
    // Inside the references stage: already reversed beats dated too early.
    let target = books
        .record(sale("target", date!("2026-03-10"), "5.00"))
        .unwrap();
    let reversed = books
        .record(
            books
                .reversal(target.id, "rev-a", date!("2026-03-10"))
                .unwrap(),
        )
        .unwrap();
    let early_again = books
        .reversal(target.id, "rev-b", date!("2026-03-05"))
        .unwrap();
    assert_eq!(
        rejected(books.record(early_again)),
        Rejection::AlreadyReversed {
            target: target.id,
            by: reversed.id
        }
    );
}

#[test]
fn qe9_overflow_precedes_the_limit() {
    let mut books = books();
    let fund = Entry::new("fund", date!("2026-02-01"))
        .debit("Expenses:Rent", Amount::MAX, EUR)
        .credit("Liabilities:Wallet", Amount::MAX, EUR);
    books.record(fund).unwrap();
    let draw = |key: &str, amount: Amount| {
        Entry::new(key, date!("2026-02-02"))
            .debit("Liabilities:Wallet", amount, EUR)
            .credit("Assets:Cash", amount, EUR)
    };
    books.record(draw("draw-all", Amount::MAX)).unwrap();
    // One more cent both overflows the wallet's debits and breaches its limit.
    assert_eq!(
        rejected(books.record(draw("draw-more", eur("0.01")))),
        Rejection::BalanceOverflow {
            account: Some(path("Liabilities:Wallet")),
            currency: EUR,
            layer: Layer::Settled
        }
    );
}

// ── Retries (SC-007, A2, P4) ──────────────────────────────────────────────────

#[test]
fn retry_after_seal_close_and_limit_tightening_replays() {
    let mut books = books();
    let draw = Entry::new("draw", date!("2026-01-20"))
        .debit("Liabilities:Wallet", eur("10.00"), EUR)
        .credit("Assets:Cash", eur("10.00"), EUR);
    let fund = Entry::new("fund", date!("2026-01-10"))
        .debit("Assets:Cash", eur("50.00"), EUR)
        .credit("Liabilities:Wallet", eur("50.00"), EUR);
    let funded = books.record(fund.clone()).unwrap();
    let drawn = books.record(draw.clone()).unwrap();
    books.seal_period("2026-01").unwrap();
    books
        .close_account("Liabilities:Wallet", date!("2026-02-28"))
        .unwrap();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoDebitBalance,
            date!("2026-03-01"),
        )
        .unwrap();
    books.declare_currency(EUR, 2, date!("2026-03-01")).unwrap();
    let mut raised = Journal::<3>::with_precision(ledger());
    raised
        .declare_currency(EUR, 2, date!("2026-01-01"))
        .unwrap();
    let size = books.head().size;
    for (draft, original) in [(fund, funded), (draw, drawn)] {
        let again = books.record(draft).unwrap();
        assert_eq!((again.id, again.position), (original.id, original.position));
        assert!(again.replayed);
    }
    assert_eq!(books.head().size, size);
}

#[test]
fn retry_after_an_exponent_raise_replays() {
    let mut books = Journal::<3>::with_precision(ledger())
        .with_salt_source(|| MemoSalt([7; 16]))
        .as_actor("setup");
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
    let amount: Amount<3> = "1.25".parse().unwrap();
    let draft = Entry::new("inv", date!("2026-01-15"))
        .debit("Assets:Cash", amount, EUR)
        .credit("Income:Sales", amount, EUR);
    let first = books.record(draft.clone()).unwrap();
    books.declare_currency(EUR, 3, date!("2026-02-01")).unwrap();
    let again = books.record(draft).unwrap();
    assert!(again.replayed);
    assert_eq!(again.position, first.position);
}

#[test]
fn retry_with_another_capture_time_is_a_key_conflict() {
    let mut books = books();
    let captured =
        |seconds| balanced("captured").captured_at(CaptureTime::new(seconds, 60).unwrap());
    let first = books.record(captured(1_767_225_600)).unwrap();
    assert!(books.record(captured(1_767_225_600)).unwrap().replayed);
    assert_eq!(
        rejected(books.record(captured(1_767_225_601))),
        Rejection::KeyConflict {
            position: first.position
        }
    );
}

/// Amended 2026-10-16 (round-3 fixes): the fingerprint a ledger keeps names
/// every account by its handle — an audit recomputes it from the leaf — so a
/// retry naming an account by handle instead of path is the same draft and
/// replays; another account is still another draft.
#[test]
fn retry_naming_an_account_by_handle_is_the_same_draft() {
    let mut books = books();
    let first = books.record(balanced("by-path")).unwrap();
    let by_handle = Entry::new("by-path", date!("2026-01-15"))
        .debit(AccountHandle::new(0).unwrap(), eur("10.00"), EUR)
        .credit("Income:Sales", eur("10.00"), EUR);
    let retry = books.record(by_handle).unwrap();
    assert!(retry.replayed && retry.id == first.id);
    let other_account = Entry::new("by-path", date!("2026-01-15"))
        .debit(AccountHandle::new(1).unwrap(), eur("10.00"), EUR)
        .credit("Income:Sales", eur("10.00"), EUR);
    assert_eq!(
        rejected(books.record(other_account)),
        Rejection::KeyConflict {
            position: first.position
        }
    );
    let unknown_path = Entry::new("by-path", date!("2026-01-15"))
        .debit("Assets:Nowhere", eur("10.00"), EUR)
        .credit("Income:Sales", eur("10.00"), EUR);
    assert_eq!(
        rejected(books.record(unknown_path)),
        Rejection::KeyConflict {
            position: first.position
        }
    );
}

#[test]
fn a_memo_retry_replays_whatever_salt_the_facade_draws() {
    let salts = std::sync::atomic::AtomicU8::new(0);
    let mut books = books().with_salt_source(move || {
        MemoSalt([salts.fetch_add(1, std::sync::atomic::Ordering::Relaxed); 16])
    });
    let draft = balanced("memo").memo("Rechnung 4711");
    let first = books.record(draft.clone()).unwrap();
    let again = books.record(draft).unwrap();
    assert!(again.replayed);
    assert_eq!(again.position, first.position);
}

#[test]
fn a_rejected_draft_leaves_the_journal_unchanged() {
    let mut books = books();
    books.record(balanced("ok")).unwrap();
    let before: Vec<Vec<u8>> = books.leaves().map(|leaf| leaf.bytes.to_vec()).collect();
    let head = books.head();
    for draft in [
        sale("ok", date!("2026-01-15"), "99.00"),
        sale("x", date!("2027-01-15"), "1.00"),
        Entry::new("y", date!("2026-01-15")).debit("Assets:Cash", eur("1.00"), EUR),
    ] {
        books.record(draft).unwrap_err();
    }
    let after: Vec<Vec<u8>> = books.leaves().map(|leaf| leaf.bytes.to_vec()).collect();
    assert_eq!(books.head(), head);
    assert_eq!(after, before);
    books.audit().unwrap();
}

#[test]
fn journal_is_send_sync_and_clone() {
    fn assert_traits<T: Send + Sync + Clone>() {}
    assert_traits::<Journal>();
    assert_traits::<Error>();
    let _ = journal().clone();
}

// ── Memo erasure (FR-041, 020:T055) ──────────────────────────────────────────

#[test]
fn erasing_a_memo_turns_its_retry_into_a_key_conflict() {
    let mut books = books();
    let draft = balanced("erased").memo("Max Mustermann");
    let recorded = books.record(draft.clone()).unwrap();
    assert!(books.record(draft.clone()).unwrap().replayed);
    let head = books.head();
    books.erase_memo(recorded.id).unwrap();
    books.erase_memo(recorded.id).unwrap(); // again: nothing left to erase
    assert_eq!(books.head(), head);
    assert_eq!(
        rejected(books.record(draft)),
        Rejection::KeyConflict {
            position: recorded.position
        }
    );
    let leaf = books
        .leaves()
        .find(|leaf| leaf.position == recorded.position)
        .unwrap();
    assert!(leaf.memo.is_none() && leaf.fingerprint.is_none());
    assert!(matches!(
        books.erase_memo(EntryId::from_bytes([9; 16])),
        Err(Error::Query(_))
    ));
    books.audit().unwrap();
}

#[test]
fn a_replayed_journal_keeps_replays_and_erasures() {
    let mut books = books();
    let kept = balanced("kept").memo("kept");
    let erased = balanced("gone").memo("gone");
    books.record(kept.clone()).unwrap();
    let gone = books.record(erased.clone()).unwrap();
    books.erase_memo(gone.id).unwrap();
    let mut restored: Journal = Journal::from_log(&ledger(), books.leaves()).unwrap();
    assert_eq!(restored.head(), books.head());
    assert!(restored.record(kept).unwrap().replayed);
    assert_eq!(code(restored.record(erased)), 200);
}
