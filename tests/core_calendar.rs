//! The calendar and seals: periods back to back, sealed only by a seal
//! record whose trees commit to every balance through the period's end.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{books, code, eur, journal, month_end, month_start, rejected, sale};
use doubleentry::{
    AccountHandle, AccountKind, Amount, Balance, Date, Entry, Journal, Label, Layer, PostingRef,
    Rejection, Side,
    currency::EUR,
    date,
    error::LabelField,
    log::build_seal_trees,
    record::{Seal, TreeHeadEntry},
};
use proptest::prelude::*;

// ── An independent fold of what the test booked ──────────────────────────────

/// Every posting the test recorded successfully: (handle, side, amount, date).
#[derive(Default)]
struct Fold {
    postings: Vec<(u32, Side, Amount, Date)>,
}

impl Fold {
    /// Records `entry` (whose postings name accounts by handle) and, when the
    /// journal accepts it, remembers its postings.
    fn record(&mut self, books: &mut Journal, entry: Entry) -> Result<(), doubleentry::Error> {
        let on = entry.booking_date();
        let postings: Vec<_> = entry
            .postings()
            .iter()
            .map(|posting| {
                let handle = match posting.account() {
                    Some(doubleentry::AccountRef::Handle(handle)) => handle.get(),
                    other => panic!("fold entries name accounts by handle, got {other:?}"),
                };
                (handle, posting.side(), posting.amount(), on)
            })
            .collect();
        books.record(entry)?;
        self.postings.extend(postings);
        Ok(())
    }

    /// The seal tree heads `build_seal_trees` gives for postings dated on or
    /// before `through`.
    fn expected(&self, accounts: u32, through: Date) -> Vec<TreeHeadEntry> {
        let mut balances: std::collections::BTreeMap<u32, Balance> =
            std::collections::BTreeMap::new();
        for &(handle, side, amount, on) in &self.postings {
            if on <= through {
                let balance = balances.entry(handle).or_insert(Balance::ZERO);
                *balance = balance.post(side, amount).unwrap();
            }
        }
        build_seal_trees(
            accounts,
            balances.into_iter().map(|(handle, balance)| {
                (
                    AccountHandle::new(handle).unwrap(),
                    EUR,
                    Layer::Settled,
                    balance,
                )
            }),
        )
        .unwrap()
        .iter()
        .map(|tree| tree.head().unwrap())
        .collect()
    }
}

fn h(index: u32) -> AccountHandle {
    AccountHandle::new(index).unwrap()
}

/// A transfer between two handles.
fn transfer(key: &str, on: Date, debit: u32, credit: u32, amount: Amount) -> Entry {
    Entry::new(key, on)
        .debit(h(debit), amount, EUR)
        .credit(h(credit), amount, EUR)
}

/// A fresh journal with EUR, `Assets:Cash` (0), `Income:Sales` (1) and
/// `Expenses:Rent` (2), and the periods March and April only.
fn march_april() -> Journal {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-03-01")).unwrap();
    assert_eq!(
        books
            .open("Assets:Cash", AccountKind::Asset, date!("2026-03-01"))
            .unwrap(),
        h(0)
    );
    assert_eq!(
        books
            .open("Income:Sales", AccountKind::Income, date!("2026-03-01"))
            .unwrap(),
        h(1)
    );
    assert_eq!(
        books
            .open("Expenses:Rent", AccountKind::Expense, date!("2026-03-01"))
            .unwrap(),
        h(2)
    );
    books
        .define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))
        .unwrap();
    books
        .define_period("2026-04", date!("2026-04-01"), date!("2026-04-30"))
        .unwrap();
    books
}

fn trees(seal: &Seal) -> &[TreeHeadEntry] {
    &seal.parts().trees
}

// ── US3 acceptance scenarios ──────────────────────────────────────────────────

#[test]
fn us3_seal_commits_balances_through_the_end() {
    let mut books = march_april();
    let mut fold = Fold::default();
    fold.record(
        &mut books,
        transfer("m1", date!("2026-03-05"), 0, 1, eur("100.00")),
    )
    .unwrap();
    fold.record(
        &mut books,
        transfer("m2", date!("2026-03-31"), 2, 0, eur("30.00")),
    )
    .unwrap();
    // April entries are booked before March is sealed.
    fold.record(
        &mut books,
        transfer("a1", date!("2026-04-01"), 0, 1, eur("70.00")),
    )
    .unwrap();
    fold.record(
        &mut books,
        transfer("a2", date!("2026-04-30"), 2, 0, eur("5.00")),
    )
    .unwrap();

    let size = books.head().size;
    let receipt = books.seal_period("2026-03").unwrap();
    assert_eq!(receipt.period.as_str(), "2026-03");
    assert_eq!(receipt.position, size);
    assert_eq!(books.head().size, size + 1);
    let parts = receipt.seal.parts();
    assert_eq!(parts.period.as_str(), "2026-03");
    assert_eq!(parts.start, date!("2026-03-01"));
    assert_eq!(parts.end, date!("2026-03-31"));
    assert_eq!(parts.through, receipt.position - 1);
    assert_eq!(parts.accounts, 3);
    let expected = fold.expected(3, date!("2026-03-31"));
    assert_eq!(expected.len(), 1);
    assert_eq!(parts.trees, expected);
    // Totals exclude April: 100 + 30 on each side, four postings.
    let totals = parts.trees[0].totals;
    assert_eq!(totals.debit(), eur("130.00"));
    assert_eq!(totals.credit(), eur("130.00"));
    assert_eq!(totals.postings(), 4);
    // The fold through April differs: April really was excluded.
    assert_ne!(fold.expected(3, date!("2026-04-30")), expected);
}

#[test]
fn us3_period_gap_is_refused() {
    let mut books = journal();
    books
        .define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))
        .unwrap();
    let size = books.head().size;
    assert_eq!(
        rejected(books.define_period("2026-05", date!("2026-05-01"), date!("2026-05-31"))),
        Rejection::PeriodGap {
            expected_start: date!("2026-04-01"),
            start: date!("2026-05-01"),
        }
    );
    assert_eq!(books.head().size, size);
}

#[test]
fn us3_sealing_april_before_march_is_refused() {
    let mut books = march_april();
    let size = books.head().size;
    assert_eq!(
        rejected(books.seal_period("2026-04")),
        Rejection::PredecessorNotSealed {
            predecessor: Label::new("2026-03").unwrap(),
            earliest: Label::new("2026-03").unwrap(),
        }
    );
    assert_eq!(books.head().size, size);
}

#[test]
fn us3_resealing_returns_the_existing_receipt() {
    let mut books = march_april();
    books
        .record(transfer("m1", date!("2026-03-05"), 0, 1, eur("1.00")))
        .unwrap();
    let first = books.seal_period("2026-03").unwrap();
    let head = books.head();
    let again = books.seal_period("2026-03").unwrap();
    assert_eq!(again, first);
    assert_eq!(books.head(), head);
}

#[test]
fn us3_nothing_on_or_before_the_watermark() {
    let mut books = march_april();
    let sale = books
        .record(transfer("m1", date!("2026-03-05"), 0, 1, eur("100.00")))
        .unwrap();
    let rent = books
        .record(transfer("m2", date!("2026-03-06"), 2, 0, eur("30.00")))
        .unwrap();
    books.seal_period("2026-03").unwrap();
    let size = books.head().size;

    // An entry dated on the last sealed day.
    assert_eq!(
        rejected(books.record(transfer("late", date!("2026-03-31"), 0, 1, eur("1.00")))),
        Rejection::PeriodSealed {
            period: Label::new("2026-03").unwrap(),
            date: date!("2026-03-31"),
        }
    );
    // A clearing dated in March (cash debit of the sale against the cash
    // credit of the rent).
    let clearing = books.clear(
        "clear-1",
        PostingRef {
            entry: sale.id,
            index: 0,
        },
        PostingRef {
            entry: rent.id,
            index: 1,
        },
        eur("30.00"),
        date!("2026-03-20"),
    );
    assert_eq!(code(clearing), 301);
    // A master-data change effective on the watermark.
    assert_eq!(
        rejected(books.open("Assets:Bank", AccountKind::Asset, date!("2026-03-31"))),
        Rejection::AtOrBelowWatermark {
            effective: date!("2026-03-31"),
            watermark: date!("2026-03-31"),
        }
    );
    assert_eq!(books.head().size, size);

    // The day after the watermark is open.
    books
        .clear(
            "clear-2",
            PostingRef {
                entry: sale.id,
                index: 0,
            },
            PostingRef {
                entry: rent.id,
                index: 1,
            },
            eur("30.00"),
            date!("2026-04-01"),
        )
        .unwrap();
    books
        .open("Assets:Bank", AccountKind::Asset, date!("2026-04-01"))
        .unwrap();
}

/// Scenario 6: there is no API that marks a period sealed without a seal
/// record — `Journal` has no such method. After `define_period` the only
/// operation that changes whether March accepts entries is `seal_period`.
#[test]
fn us3_only_a_seal_seals() {
    let mut books = march_april();
    for (index, day) in [1u8, 15, 31].into_iter().enumerate() {
        let on = Date::from_calendar_date(2026, time::Month::March, day).unwrap();
        books
            .record(transfer(&format!("before-{index}"), on, 0, 1, eur("1.00")))
            .unwrap();
    }
    // Redefining March identically and sealing nothing else leaves it open.
    books
        .define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))
        .unwrap();
    books
        .record(transfer(
            "still-open",
            date!("2026-03-31"),
            0,
            1,
            eur("1.00"),
        ))
        .unwrap();
    let receipt = books.seal_period("2026-03").unwrap();
    assert_eq!(
        code(books.record(transfer("after", date!("2026-03-15"), 0, 1, eur("1.00")))),
        301
    );
    // The seal record is in the log at the receipt's position.
    assert_eq!(receipt.position + 1, books.head().size);
}

// ── Rejection rows ────────────────────────────────────────────────────────────

#[test]
fn row_107_period_id_is_a_label() {
    for id in ["", "  x"] {
        let mut books = journal();
        let size = books.head().size;
        match rejected(books.define_period(id, date!("2026-03-01"), date!("2026-03-31"))) {
            Rejection::InvalidLabel { field, .. } => assert_eq!(field, LabelField::PeriodId),
            other => panic!("expected 107 for {id:?}, got {other:?}"),
        }
        assert_eq!(books.head().size, size);
        assert_eq!(code(books.seal_period(id)), 107);
    }
}

#[test]
fn row_121_end_before_start() {
    let mut books = journal();
    assert_eq!(
        rejected(books.define_period("bad", date!("2026-03-02"), date!("2026-03-01"))),
        Rejection::InvalidPeriodRange {
            start: date!("2026-03-02"),
            end: date!("2026-03-01"),
        }
    );
    // A one-day period is valid.
    let size = books.head().size;
    books
        .define_period("day", date!("2026-03-02"), date!("2026-03-02"))
        .unwrap();
    assert_eq!(books.head().size, size + 1);
    books
        .define_period("next", date!("2026-03-03"), date!("2026-03-03"))
        .unwrap();
}

#[test]
fn row_311_unknown_period() {
    let mut books = books();
    assert_eq!(
        rejected(books.seal_period("2027-01")),
        Rejection::UnknownPeriod {
            period: Label::new("2027-01").unwrap(),
        }
    );
}

#[test]
fn row_425_period_exists_with_other_dates() {
    let mut books = books();
    let size = books.head().size;
    assert_eq!(
        rejected(books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-30"))),
        Rejection::PeriodExists {
            period: Label::new("2026-03").unwrap(),
            start: date!("2026-03-01"),
            end: date!("2026-03-31"),
        }
    );
    // An identical re-definition is unchanged.
    books
        .define_period("2026-03", month_start(3), month_end(3))
        .unwrap();
    assert_eq!(books.head().size, size);
}

#[test]
fn first_period_may_start_on_any_day() {
    let mut books = journal();
    books
        .define_period("odd", date!("2026-03-17"), date!("2026-04-09"))
        .unwrap();
    books
        .define_period("odd-2", date!("2026-04-10"), date!("2026-04-10"))
        .unwrap();
}

#[test]
fn row_426_before_the_first_and_gaps() {
    let mut books = journal();
    books
        .define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))
        .unwrap();
    // Before the first period.
    assert_eq!(
        rejected(books.define_period("2026-02", date!("2026-02-01"), date!("2026-02-28"))),
        Rejection::PeriodGap {
            expected_start: date!("2026-04-01"),
            start: date!("2026-02-01"),
        }
    );
    // A one-day gap.
    assert_eq!(
        rejected(books.define_period("2026-04", date!("2026-04-02"), date!("2026-04-30"))),
        Rejection::PeriodGap {
            expected_start: date!("2026-04-01"),
            start: date!("2026-04-02"),
        }
    );
    // An overlap.
    assert_eq!(
        code(books.define_period("2026-04", date!("2026-03-31"), date!("2026-04-30"))),
        426
    );
}

#[test]
fn row_427_predecessor_not_sealed() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.seal_period("2026-03")),
        Rejection::PredecessorNotSealed {
            predecessor: Label::new("2026-02").unwrap(),
            earliest: Label::new("2026-02").unwrap(),
        }
    );
    books.seal_period("2026-02").unwrap();
    books.seal_period("2026-03").unwrap();
}

// ── Edge cases ────────────────────────────────────────────────────────────────

#[test]
fn seal_of_an_empty_period_repeats_the_cumulative_balances() {
    let mut books = books();
    let mut fold = Fold::default();
    fold.record(
        &mut books,
        transfer("jan", date!("2026-01-10"), 0, 1, eur("10.00")),
    )
    .unwrap();
    fold.record(
        &mut books,
        transfer("mar", date!("2026-03-10"), 3, 0, eur("4.00")),
    )
    .unwrap();
    let january = books.seal_period("2026-01").unwrap();
    let february = books.seal_period("2026-02").unwrap();
    assert_eq!(trees(&february.seal), trees(&january.seal));
    assert_eq!(
        trees(&february.seal).to_vec(),
        fold.expected(5, month_end(1))
    );
    assert_eq!(february.seal.parts().through, february.position - 1);
}

#[test]
fn seal_with_no_used_pair_has_no_trees() {
    let mut books = books();
    let receipt = books.seal_period("2026-01").unwrap();
    assert!(receipt.seal.parts().trees.is_empty());
    assert_eq!(receipt.seal.parts().accounts, 5);

    // Postings only after the end: still no trees.
    let mut books = common::books();
    books
        .record(sale("feb", date!("2026-02-01"), "1.00"))
        .unwrap();
    let receipt = books.seal_period("2026-01").unwrap();
    assert!(receipt.seal.parts().trees.is_empty());
}

// ── SC-006: random histories booking ahead ────────────────────────────────────

/// One random booking: (debit index, credit index, cents, month offset, day).
type Booking = (u32, u32, u64, u8, u8);

fn booking() -> impl Strategy<Value = Booking> {
    (0u32..5, 0u32..5, 1u64..100_000, 0u8..6, 1u8..=28)
}

/// Books `batches[k]` into months `k + 1 ..= 6`, then seals month `k + 1`,
/// for `k` in `0..4`; checks every seal against the fold and returns them.
fn run_history(batches: &[Vec<Booking>]) -> Result<Vec<Seal>, TestCaseError> {
    let mut books = books();
    let mut fold = Fold::default();
    let mut seals: Vec<Seal> = Vec::new();
    for (k, batch) in batches.iter().enumerate() {
        let first = u8::try_from(k).unwrap() + 1;
        for (i, &(debit, credit, cents, offset, day)) in batch.iter().enumerate() {
            if debit == credit {
                continue;
            }
            let month = first + offset % (7 - first);
            let on =
                Date::from_calendar_date(2026, time::Month::try_from(month).unwrap(), day).unwrap();
            let amount = Amount::from_minor(i128::from(cents)).unwrap();
            match fold.record(
                &mut books,
                transfer(&format!("e-{k}-{i}"), on, debit, credit, amount),
            ) {
                Ok(()) => {}
                // The wallet (handle 2) may not go into debit.
                Err(error) => {
                    let code = match error {
                        doubleentry::Error::Rejected(rejection) => rejection.code(),
                        other => panic!("unexpected error {other:?}"),
                    };
                    prop_assert_eq!(code, 501);
                }
            }
        }
        let receipt = books.seal_period(format!("2026-{first:02}")).unwrap();
        let parts = receipt.seal.parts();
        prop_assert_eq!(parts.through, receipt.position - 1);
        prop_assert_eq!(parts.end, month_end(first));
        prop_assert_eq!(&parts.trees, &fold.expected(5, month_end(first)));
        seals.push(receipt.seal);
    }
    Ok(seals)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn seal_leaves_equal_fold_through_end(
        batches in prop::collection::vec(prop::collection::vec(booking(), 0..24), 4),
    ) {
        run_history(&batches)?;
    }

    #[test]
    fn seal_totals_never_decrease(
        batches in prop::collection::vec(prop::collection::vec(booking(), 0..24), 4),
    ) {
        let seals = run_history(&batches)?;
        for pair in seals.windows(2) {
            let (earlier, later) = (&pair[0], &pair[1]);
            prop_assert!(later.check_follows(earlier).is_ok());
            for tree in trees(earlier) {
                let next = later.tree(tree.currency, tree.layer);
                prop_assert!(next.is_some(), "a used pair disappeared");
                let next = next.unwrap().totals;
                prop_assert!(next.debit() >= tree.totals.debit());
                prop_assert!(next.credit() >= tree.totals.credit());
                prop_assert!(next.postings() >= tree.totals.postings());
            }
        }
    }
}
