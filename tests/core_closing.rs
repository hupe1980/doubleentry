//! Year-end closing: drafts that zero every income and expense slice
//! into an equity account, keeping the slices' dimensions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{books, code, eur, rejected};
use doubleentry::{
    AccountHandle, AccountKind, AccountPath, AccountRef, Amount, Date, Dimensions, Entry, Journal,
    Label, Layer, LedgerPolicy, Posting, Rejection, Side, currency::EUR, date,
    record::MAX_LEAF_BYTES,
};

const SALES: u32 = 1;
const RENT: u32 = 3;
const RETAINED: u32 = 4;

fn handle(index: u32) -> AccountRef {
    AccountRef::Handle(AccountHandle::new(index).unwrap())
}

fn segment(value: &str) -> Dimensions {
    Dimensions::new()
        .with(Label::new("segment").unwrap(), Label::new(value).unwrap())
        .unwrap()
}

/// `books()` with a policy requiring `segment` from 2026-01-01.
fn segmented_books() -> Journal {
    let mut books = books();
    books
        .set_policy(
            LedgerPolicy::new([Label::new("segment").unwrap()], None).unwrap(),
            date!("2026-01-01"),
        )
        .unwrap();
    books
}

/// A sale in a segment (both legs carry it).
fn segment_sale(key: &str, on: Date, amount: &str, value: &str) -> Entry {
    Entry::new(key, on)
        .posting(Posting::debit("Assets:Cash", eur(amount), EUR).dimension("segment", value))
        .posting(Posting::credit("Income:Sales", eur(amount), EUR).dimension("segment", value))
}

/// Rent paid in a segment (both legs carry it).
fn segment_rent(key: &str, on: Date, amount: &str, value: &str) -> Entry {
    Entry::new(key, on)
        .posting(Posting::debit("Expenses:Rent", eur(amount), EUR).dimension("segment", value))
        .posting(Posting::credit("Assets:Cash", eur(amount), EUR).dimension("segment", value))
}

/// (account, side, amount, dimensions) of a draft's postings.
fn legs(entry: &Entry) -> Vec<(AccountRef, Side, Amount, Dimensions)> {
    entry
        .postings()
        .iter()
        .map(|posting| {
            (
                posting.account().unwrap().clone(),
                posting.side(),
                posting.amount(),
                posting.get_dimensions().clone(),
            )
        })
        .collect()
}

/// Debits equal credits, every leg is Settled, within the limits.
#[track_caller]
fn assert_well_formed(entry: &Entry) {
    let (mut debit, mut credit) = (Amount::ZERO, Amount::ZERO);
    for posting in entry.postings() {
        assert_eq!(posting.get_layer(), Layer::Settled);
        assert_eq!(posting.currency(), EUR);
        match posting.side() {
            Side::Debit => debit = debit.checked_add(posting.amount()).unwrap(),
            Side::Credit => credit = credit.checked_add(posting.amount()).unwrap(),
        }
    }
    assert_eq!(debit, credit);
    assert!(entry.postings().len() <= Entry::<2>::MAX_POSTINGS);
    assert!(entry.encoded_len() <= MAX_LEAF_BYTES);
    assert!(entry.build_errors().is_empty());
}

// ── US8 acceptance scenarios ──────────────────────────────────────────────────

// PoC: f10_closing_postings_fail_required_dimensions
#[test]
fn ef10_closing_keeps_slices() {
    let mut books = segmented_books();
    books
        .record(segment_sale("s1", date!("2026-03-10"), "100.00", "Retail"))
        .unwrap();
    books
        .record(segment_sale(
            "s2",
            date!("2026-05-10"),
            "50.00",
            "Wholesale",
        ))
        .unwrap();
    books
        .record(segment_sale("s3", date!("2026-06-10"), "20.00", "Retail"))
        .unwrap();
    books
        .record(segment_rent("r1", date!("2026-07-01"), "30.00", "Retail"))
        .unwrap();

    let drafts = books
        .close_year("close-2026", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    assert_eq!(drafts.len(), 1);
    let draft = &drafts[0];
    assert_eq!(draft.key().unwrap().as_str(), "close-2026/1");
    assert_eq!(draft.booking_date(), date!("2026-12-31"));
    assert_well_formed(draft);
    let (retail, wholesale) = (segment("Retail"), segment("Wholesale"));
    assert_eq!(
        legs(draft),
        vec![
            // (EUR, Retail): Sales net −120 → debit, Rent net +30 → credit,
            // equity takes the 90 profit.
            (handle(SALES), Side::Debit, eur("120.00"), retail.clone()),
            (handle(RENT), Side::Credit, eur("30.00"), retail.clone()),
            (handle(RETAINED), Side::Credit, eur("90.00"), retail),
            // (EUR, Wholesale).
            (handle(SALES), Side::Debit, eur("50.00"), wholesale.clone()),
            (handle(RETAINED), Side::Credit, eur("50.00"), wholesale),
        ]
    );

    // Recording succeeds under the required-dimension policy …
    for draft in drafts {
        books.record(draft).unwrap();
    }
    // … and leaves every slice at net zero.
    assert!(
        books
            .close_year("close-2026-again", date!("2026-12-31"), "Equity:Retained")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn us8_nothing_to_close_gives_no_drafts() {
    let books = books();
    assert!(
        books
            .close_year("close", date!("2026-12-31"), "Equity:Retained")
            .unwrap()
            .is_empty()
    );
    // Balance-sheet movements only: still nothing.
    let mut books = books;
    books
        .record(
            Entry::new("move", date!("2026-02-01"))
                .debit("Assets:Cash", eur("5.00"), EUR)
                .credit("Equity:Retained", eur("5.00"), EUR),
        )
        .unwrap();
    assert!(
        books
            .close_year("close", date!("2026-12-31"), "Equity:Retained")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn us8_closed_income_account_is_named() {
    let mut books = books();
    books
        .record(common::sale("s1", date!("2026-10-05"), "10.00"))
        .unwrap();
    books
        .close_account("Income:Sales", date!("2026-11-30"))
        .unwrap();
    assert_eq!(
        rejected(books.close_year("close", date!("2026-12-31"), "Equity:Retained")),
        Rejection::ClosingAccountNotOpen {
            account: AccountPath::parse("Income:Sales").unwrap(),
            date: date!("2026-12-31"),
        }
    );
}

#[test]
fn us8_equity_of_kind_income_is_refused() {
    let mut books = books();
    books
        .open("Income:Other", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .record(common::sale("s1", date!("2026-02-05"), "10.00"))
        .unwrap();
    assert_eq!(
        rejected(books.close_year("close", date!("2026-12-31"), "Income:Other")),
        Rejection::ClosingEquityInScope {
            account: AccountPath::parse("Income:Other").unwrap(),
        }
    );
    assert_eq!(
        code(books.close_year("close", date!("2026-12-31"), "Expenses:Rent")),
        429
    );
}

// ── Rejection rows ────────────────────────────────────────────────────────────

#[test]
fn row_302_unknown_equity_account() {
    let books = books();
    assert_eq!(
        code(books.close_year("close", date!("2026-12-31"), "Equity:Nowhere")),
        302
    );
}

#[test]
fn row_303_equity_is_not_a_leaf() {
    let mut books = books();
    books
        .open("Equity:Group", AccountKind::Equity, date!("2026-01-01"))
        .unwrap();
    books
        .open("Equity:Group:Sub", AccountKind::Equity, date!("2026-01-01"))
        .unwrap();
    assert_eq!(
        rejected(books.close_year("close", date!("2026-12-31"), "Equity:Group")),
        Rejection::NotALeaf {
            account: AccountPath::parse("Equity:Group").unwrap(),
        }
    );
    // The leaf below it is fine.
    books
        .close_year("close", date!("2026-12-31"), "Equity:Group:Sub")
        .unwrap();
}

#[test]
fn row_100_key_too_long() {
    let books = books();
    let key = "k".repeat(130);
    assert_eq!(
        code(books.close_year(&key, date!("2026-12-31"), "Equity:Retained")),
        100
    );
}

// ── Edge cases ────────────────────────────────────────────────────────────────

#[test]
fn more_than_1024_slices_are_chunked() {
    let mut books = books();
    for i in 0..1_500 {
        books
            .record(
                Entry::new(format!("s{i}"), date!("2026-04-15"))
                    .debit("Assets:Cash", eur("1.00"), EUR)
                    .posting(
                        Posting::credit("Income:Sales", eur("1.00"), EUR)
                            .dimension("segment", format!("seg-{i:04}").as_str()),
                    ),
            )
            .unwrap();
    }
    let drafts = books
        .close_year("close-2026", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    assert!(drafts.len() >= 2, "{} drafts", drafts.len());
    let mut closed = 0usize;
    for (index, draft) in drafts.iter().enumerate() {
        assert_eq!(
            draft.key().unwrap().as_str(),
            format!("close-2026/{}", index + 1)
        );
        assert_well_formed(draft);
        closed += draft
            .postings()
            .iter()
            .filter(|posting| posting.account() == Some(&handle(SALES)))
            .count();
    }
    assert_eq!(closed, 1_500);
    for draft in drafts {
        books.record(draft).unwrap();
    }
    assert!(
        books
            .close_year("close-2026-again", date!("2026-12-31"), "Equity:Retained")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn income_and_expense_cancelling_in_a_slice_have_no_equity_leg() {
    let mut books = segmented_books();
    books
        .record(segment_sale("s1", date!("2026-03-10"), "40.00", "Retail"))
        .unwrap();
    books
        .record(segment_rent("r1", date!("2026-03-11"), "40.00", "Retail"))
        .unwrap();
    let drafts = books
        .close_year("close", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    assert_eq!(drafts.len(), 1);
    assert_well_formed(&drafts[0]);
    assert_eq!(
        legs(&drafts[0]),
        vec![
            (handle(SALES), Side::Debit, eur("40.00"), segment("Retail")),
            (handle(RENT), Side::Credit, eur("40.00"), segment("Retail")),
        ]
    );
    assert!(
        drafts[0]
            .postings()
            .iter()
            .all(|posting| posting.account() != Some(&handle(RETAINED)))
    );
    for draft in drafts {
        books.record(draft).unwrap();
    }
    assert!(
        books
            .close_year("close-again", date!("2026-12-31"), "Equity:Retained")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn closing_reads_through_the_closing_date_and_settled_only() {
    let mut books = books();
    books
        .record(common::sale("s1", date!("2026-06-30"), "10.00"))
        .unwrap();
    books
        .record(common::sale("s2", date!("2026-07-01"), "5.00"))
        .unwrap();
    let drafts = books
        .close_year("half", date!("2026-06-30"), "Equity:Retained")
        .unwrap();
    assert_eq!(drafts.len(), 1);
    assert_well_formed(&drafts[0]);
    assert_eq!(
        legs(&drafts[0]),
        vec![
            (handle(SALES), Side::Debit, eur("10.00"), Dimensions::new()),
            (
                handle(RETAINED),
                Side::Credit,
                eur("10.00"),
                Dimensions::new()
            ),
        ]
    );
    assert!(
        drafts[0]
            .postings()
            .iter()
            .all(|posting| posting.get_layer() == Layer::Settled)
    );
}
