//! Clearings: open items matched by apply and reset records, with
//! checked sides, balances, amounts and dates.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{books, eur, journal, ledger, rejected};
use doubleentry::{
    AccountHandle, AccountKind, Amount, Entry, EntryId, IdempotencyKey, Journal, Label, Layer,
    MemoSalt, PostingRef, Rejection, Side,
    currency::EUR,
    date,
    error::{ScaledAmount, ValueError},
    store::BalanceKey,
};

/// Books with an AR account, an invoice (debit AR 1190.00, 03-01) and a
/// payment (credit AR 500.00, 03-10).
struct Receivables {
    books: Journal,
    /// The invoice's AR debit posting.
    invoice: PostingRef,
    /// The invoice's income credit posting.
    income: PostingRef,
    /// The invoice entry's position.
    invoice_position: u64,
    /// The payment's AR credit posting.
    payment: PostingRef,
}

fn receivables() -> Receivables {
    let mut books = books();
    books
        .open(
            "Assets:Receivables",
            AccountKind::Asset,
            date!("2026-01-01"),
        )
        .unwrap();
    let invoice = books
        .record(
            Entry::new("inv-1", date!("2026-03-01"))
                .debit("Assets:Receivables", eur("1190.00"), EUR)
                .credit("Income:Sales", eur("1190.00"), EUR),
        )
        .unwrap();
    let payment = books
        .record(
            Entry::new("pay-1", date!("2026-03-10"))
                .debit("Assets:Cash", eur("500.00"), EUR)
                .credit("Assets:Receivables", eur("500.00"), EUR),
        )
        .unwrap();
    Receivables {
        books,
        invoice: PostingRef::at(invoice.id, 0),
        income: PostingRef::at(invoice.id, 1),
        invoice_position: invoice.position,
        payment: PostingRef::at(payment.id, 1),
    }
}

fn handle(index: u32) -> AccountHandle {
    AccountHandle::new(index).unwrap()
}

// ── US7 acceptance scenarios ─────────────────────────────────────────────────

#[test]
fn us7_s1_a_clearing_apply_is_recorded() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let before = books.head().size;
    let position = books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    assert_eq!(position, before);
    assert_eq!(books.head().size, before + 1);
}

#[test]
fn us7_s2_clearing_more_than_is_open_is_over_applied() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    // A second invoice with enough open, so the payment is what runs out.
    let second = books
        .record(
            Entry::new("inv-2", date!("2026-03-02"))
                .debit("Assets:Receivables", eur("1000.00"), EUR)
                .credit("Income:Sales", eur("1000.00"), EUR),
        )
        .unwrap();
    let before = books.head();
    assert_eq!(
        rejected(books.clear(
            "clr-2",
            PostingRef::at(second.id, 0),
            payment,
            eur("700.00"),
            date!("2026-03-10")
        )),
        Rejection::OverApplied {
            posting: payment,
            open: ScaledAmount { minor: 0, scale: 2 },
            requested: ScaledAmount {
                minor: 70000,
                scale: 2
            },
        }
    );
    assert_eq!(books.head(), before);
}

#[test]
fn us7_s3_clearing_before_a_posting_is_clearing_before_posting() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-05")
        )),
        Rejection::ClearingBeforePosting {
            posting: payment,
            booked: date!("2026-03-10"),
        }
    );
}

#[test]
fn us7_s4_reset_before_clearing_then_reset_then_reset_again() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let applied = books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    assert_eq!(
        rejected(books.reset_clearing("rst-1", applied, date!("2026-03-09"))),
        Rejection::ResetBeforeClearing {
            cleared_on: date!("2026-03-10"),
        }
    );
    let before = books.head().size;
    let reset = books
        .reset_clearing("rst-1", applied, date!("2026-03-12"))
        .unwrap();
    assert_eq!(reset, before);
    assert_eq!(books.head().size, before + 1);
    // The amounts are open again: the full 500.00 clears once more.
    books
        .clear(
            "clr-2",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-12"),
        )
        .unwrap();
    assert_eq!(
        rejected(books.reset_clearing("rst-2", applied, date!("2026-03-13"))),
        Rejection::ClearingAlreadyReset {
            clearing: applied,
            by: reset,
        }
    );
}

// ── Rejection rows ───────────────────────────────────────────────────────────

#[test]
fn clearing_with_an_empty_key_is_invalid_key() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let refusal = rejected(books.clear("", invoice, payment, eur("500.00"), date!("2026-03-10")));
    assert!(
        matches!(
            refusal,
            Rejection::InvalidKey {
                reason: ValueError::Empty
            }
        ),
        "{refusal:?}"
    );
    let refusal = rejected(books.reset_clearing("", 1, date!("2026-03-10")));
    assert!(
        matches!(
            refusal,
            Rejection::InvalidKey {
                reason: ValueError::Empty
            }
        ),
        "{refusal:?}"
    );
}

#[test]
fn clearing_a_zero_or_negative_amount_is_non_positive_clearing_amount() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    for amount in ["0.00", "-1.00"] {
        assert_eq!(
            rejected(books.clear("clr-1", invoice, payment, eur(amount), date!("2026-03-10"))),
            Rejection::NonPositiveClearingAmount,
            "{amount}"
        );
    }
}

#[test]
fn a_clearing_key_reused_for_other_content_is_key_conflict() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let applied = books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            invoice,
            payment,
            eur("400.00"),
            date!("2026-03-10")
        )),
        Rejection::KeyConflict { position: applied }
    );
    // The namespace is shared with resets.
    assert_eq!(
        rejected(books.reset_clearing("clr-1", applied, date!("2026-03-12"))),
        Rejection::KeyConflict { position: applied }
    );
}

#[test]
fn an_identical_apply_replays_its_position_even_after_the_seal() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let applied = books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    let head = books.head();
    assert_eq!(
        books
            .clear(
                "clr-1",
                invoice,
                payment,
                eur("500.00"),
                date!("2026-03-10")
            )
            .unwrap(),
        applied
    );
    assert_eq!(books.head(), head);

    for period in ["2026-01", "2026-02", "2026-03"] {
        books.seal_period(period).unwrap();
    }
    let head = books.head();
    assert_eq!(
        books
            .clear(
                "clr-1",
                invoice,
                payment,
                eur("500.00"),
                date!("2026-03-10")
            )
            .unwrap(),
        applied
    );
    assert_eq!(books.head(), head);
}

#[test]
fn an_identical_reset_replays_its_position() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let applied = books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    let reset = books
        .reset_clearing("rst-1", applied, date!("2026-03-12"))
        .unwrap();
    let head = books.head();
    assert_eq!(
        books
            .reset_clearing("rst-1", applied, date!("2026-03-12"))
            .unwrap(),
        reset
    );
    assert_eq!(books.head(), head);
}

#[test]
fn clearing_dated_in_no_period_is_no_period() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2027-01-05")
        )),
        Rejection::NoPeriod {
            date: date!("2027-01-05"),
        }
    );
    assert_eq!(
        rejected(books.reset_clearing("rst-1", 1, date!("2027-01-05"))),
        Rejection::NoPeriod {
            date: date!("2027-01-05"),
        }
    );
}

#[test]
fn clearing_dated_in_a_sealed_period_is_period_sealed() {
    let mut books = books();
    books
        .open(
            "Assets:Receivables",
            AccountKind::Asset,
            date!("2026-01-01"),
        )
        .unwrap();
    let invoice = books
        .record(
            Entry::new("inv-1", date!("2026-01-05"))
                .debit("Assets:Receivables", eur("100.00"), EUR)
                .credit("Income:Sales", eur("100.00"), EUR),
        )
        .unwrap();
    let payment = books
        .record(
            Entry::new("pay-1", date!("2026-01-10"))
                .debit("Assets:Cash", eur("100.00"), EUR)
                .credit("Assets:Receivables", eur("100.00"), EUR),
        )
        .unwrap();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(payment.id, 1),
            eur("100.00"),
            date!("2026-01-20")
        )),
        Rejection::PeriodSealed {
            period: Label::new("2026-01").unwrap(),
            date: date!("2026-01-20"),
        }
    );
    // Dated after the watermark it is recorded.
    books
        .clear(
            "clr-1",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(payment.id, 1),
            eur("100.00"),
            date!("2026-02-02"),
        )
        .unwrap();
}

#[test]
fn clearing_an_unknown_posting_is_clearing_target_unknown() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let ghost = PostingRef::at(
        EntryId::derive(&ledger(), &IdempotencyKey::new("never-recorded").unwrap()),
        0,
    );
    assert_eq!(
        rejected(books.clear("clr-1", ghost, payment, eur("500.00"), date!("2026-03-10"))),
        Rejection::ClearingTargetUnknown { posting: ghost }
    );
    let beyond = PostingRef::at(payment.entry, 2);
    assert_eq!(
        rejected(books.clear("clr-1", invoice, beyond, eur("500.00"), date!("2026-03-10"))),
        Rejection::ClearingTargetUnknown { posting: beyond }
    );
}

#[test]
fn clearing_a_posting_on_the_wrong_side_is_clearing_side_mismatch() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            payment,
            payment,
            eur("500.00"),
            date!("2026-03-10")
        )),
        Rejection::ClearingSideMismatch {
            posting: payment,
            expected: Side::Debit,
        }
    );
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            invoice,
            invoice,
            eur("500.00"),
            date!("2026-03-10")
        )),
        Rejection::ClearingSideMismatch {
            posting: invoice,
            expected: Side::Credit,
        }
    );
}

#[test]
fn clearing_postings_of_different_accounts_is_clearing_key_mismatch() {
    let Receivables {
        mut books,
        invoice,
        income,
        ..
    } = receivables();
    // books(): Assets:Cash 0, Income:Sales 1, …, Assets:Receivables 5.
    assert_eq!(
        rejected(books.clear("clr-1", invoice, income, eur("500.00"), date!("2026-03-10"))),
        Rejection::ClearingKeyMismatch {
            debit: BalanceKey {
                handle: handle(5),
                currency: EUR,
                layer: Layer::Settled,
            },
            credit: BalanceKey {
                handle: handle(1),
                currency: EUR,
                layer: Layer::Settled,
            },
        }
    );
}

#[test]
fn clearing_finer_than_the_currency_exponent_is_clearing_precision() {
    let mut books = Journal::<3>::with_precision(ledger())
        .with_salt_source(|| MemoSalt([7; 16]))
        .as_actor("setup");
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open(
            "Assets:Receivables",
            AccountKind::Asset,
            date!("2026-01-01"),
        )
        .unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))
        .unwrap();
    let amount = |text: &str| text.parse::<Amount<3>>().unwrap();
    let invoice = books
        .record(
            Entry::new("inv-1", date!("2026-03-01"))
                .debit("Assets:Receivables", amount("10.00"), EUR)
                .credit("Income:Sales", amount("10.00"), EUR),
        )
        .unwrap();
    let payment = books
        .record(
            Entry::new("pay-1", date!("2026-03-10"))
                .debit("Assets:Cash", amount("10.00"), EUR)
                .credit("Assets:Receivables", amount("10.00"), EUR),
        )
        .unwrap();
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(payment.id, 1),
            amount("0.001"),
            date!("2026-03-10")
        )),
        Rejection::ClearingPrecision {
            currency: EUR,
            exponent: 2,
        }
    );
    books
        .clear(
            "clr-1",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(payment.id, 1),
            amount("0.01"),
            date!("2026-03-10"),
        )
        .unwrap();
}

#[test]
fn resetting_a_position_that_is_no_apply_is_clearing_unknown() {
    let Receivables {
        mut books,
        invoice_position,
        ..
    } = receivables();
    assert_eq!(
        rejected(books.reset_clearing("rst-1", invoice_position, date!("2026-03-12"))),
        Rejection::ClearingUnknown {
            clearing: invoice_position,
        }
    );
    let beyond = books.head().size + 100;
    assert_eq!(
        rejected(books.reset_clearing("rst-1", beyond, date!("2026-03-12"))),
        Rejection::ClearingUnknown { clearing: beyond }
    );
}

#[test]
fn resetting_a_reset_is_clearing_unknown() {
    let Receivables {
        mut books,
        invoice,
        payment,
        ..
    } = receivables();
    let applied = books
        .clear(
            "clr-1",
            invoice,
            payment,
            eur("500.00"),
            date!("2026-03-10"),
        )
        .unwrap();
    let reset = books
        .reset_clearing("rst-1", applied, date!("2026-03-12"))
        .unwrap();
    assert_eq!(
        rejected(books.reset_clearing("rst-2", reset, date!("2026-03-12"))),
        Rejection::ClearingUnknown { clearing: reset }
    );
}

#[test]
fn a_side_mismatch_takes_precedence_over_a_clearing_before_the_posting() {
    let Receivables {
        mut books, payment, ..
    } = receivables();
    // Breaks 411 (the debit reference names a credit posting) and 415
    // (dated before the payment's 03-10): 411 wins.
    assert_eq!(
        rejected(books.clear(
            "clr-1",
            payment,
            payment,
            eur("500.00"),
            date!("2026-03-05")
        )),
        Rejection::ClearingSideMismatch {
            posting: payment,
            expected: Side::Debit,
        }
    );
}

#[test]
fn an_empty_journal_refuses_a_clearing_by_its_date_first() {
    let mut books = journal();
    let ghost = PostingRef::at(
        EntryId::derive(&ledger(), &IdempotencyKey::new("x").unwrap()),
        0,
    );
    assert_eq!(
        rejected(books.clear("clr-1", ghost, ghost, eur("1.00"), date!("2026-03-10"))),
        Rejection::NoPeriod {
            date: date!("2026-03-10"),
        }
    );
}
