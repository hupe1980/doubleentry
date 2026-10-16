//! Every read, once, everywhere: balances and the trial balance by every
//! query form, statements, the rollup, balance assertions and open items —
//! the same answer from `Journal`, `MemoryStore` and SQLite — and their
//! regressions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

#[path = "common/fixture.rs"]
mod fixture;

use doubleentry::store::{MemoryStore, Store};
use doubleentry::{
    AccountKind, Amount, Balance, BalanceQuery, Entry, Error, Journal, Ledger, LedgerId, TreeHead,
    currency::{EUR, JPY, USD},
    date,
    error::{IntegrityKind, QueryError},
    log::Hash,
    report::{AssertAt, AssertionOutcome, BalanceAssertion, DateBasis, Deviation, Rollup},
};
use fixture::{Fixture, fixture, journal};

fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

fn query_error<T: std::fmt::Debug>(result: Result<T, Error>) -> QueryError {
    match result {
        Err(Error::Query(error)) => error,
        other => panic!("expected a query error, got {other:?}"),
    }
}

/// Every query form of the scenarios below.
fn queries() -> Vec<BalanceQuery> {
    let march = BalanceQuery::period("2026-03");
    vec![
        BalanceQuery::all(),
        BalanceQuery::all().pending(),
        BalanceQuery::through_position(30),
        BalanceQuery::through(date!("2026-03-16")),
        BalanceQuery::through(date!("2026-03-31")).by_value_date(),
        BalanceQuery::between(date!("2026-03-01"), date!("2026-04-30")),
        march.clone(),
        march.by_value_date(),
        BalanceQuery::all().matching("cost_centre", "A"),
        BalanceQuery::all().matching("cost_centre", "B"),
        BalanceQuery::all().missing("cost_centre"),
        BalanceQuery::through(date!("2026-04-09")),
    ]
}

const ACCOUNTS: [&str; 5] = [
    "Assets:Bank",
    "Income:Sales",
    "Assets:Receivables",
    "Expenses:Travel",
    "Expenses:Rent",
];

// ── Every store agrees (SC-001 on the fixture) ─────────────────────────────

async fn agrees<S: Store<2>>(store: S) {
    let (books, _) = fixture(store).await;
    let (journal, _) = journal().await;
    assert_eq!(books.head().await.unwrap(), journal.head());
    for query in queries() {
        assert_eq!(
            books.trial_balance(&query).await.unwrap(),
            journal.trial_balance(&query).unwrap(),
            "{query:?}"
        );
        for currency in [EUR, USD, JPY] {
            assert_eq!(
                books.rollup(currency, &query).await.unwrap(),
                journal.rollup(currency, &query).unwrap()
            );
            for account in ACCOUNTS {
                assert_eq!(
                    books.balance(account, currency, &query).await.unwrap(),
                    journal.balance(account, currency, &query).unwrap(),
                    "{account} {currency} {query:?}"
                );
                assert_eq!(
                    books.statement(account, currency, &query).await.unwrap(),
                    journal.statement(account, currency, &query).unwrap()
                );
                assert_eq!(
                    books.open_items(account, currency, &query).await.unwrap(),
                    journal.open_items(account, currency, &query).unwrap()
                );
            }
        }
    }
    let claims = assertions();
    assert_eq!(
        books.check_assertions(&claims).await.unwrap(),
        journal.check_assertions(&claims).unwrap()
    );
}

#[tokio::test]
async fn us1_every_read_agrees_on_memory() {
    agrees(MemoryStore::new()).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn us1_every_read_agrees_on_sqlite() {
    let store = doubleentry::store::SqlStore::sqlite("sqlite::memory:")
        .await
        .unwrap();
    agrees(store).await;
}

// ── US1: balances and the trial balance ────────────────────────────────────

#[tokio::test]
async fn us1_booking_and_value_basis() {
    let (books, _) = journal().await;
    let march = BalanceQuery::through(date!("2026-03-31"));
    // 1000 − 300 + 100 + 50 + 20 − 50 (the reversal).
    let by_booking = books.balance("Assets:Bank", EUR, &march).unwrap();
    assert_eq!(by_booking.net(), eur("820.00"));
    // The month-end sale is value-dated 2026-04-02.
    let by_value = books
        .balance("Assets:Bank", EUR, &march.by_value_date())
        .unwrap();
    assert_eq!(by_value.net(), eur("720.00"));
}

#[tokio::test]
async fn us1_backdated_entries_count_by_date_not_position() {
    let (books, fixture) = journal().await;
    assert!(fixture.backdated_sale.position > fixture.late_sale.position);
    let balance = books
        .balance(
            "Assets:Bank",
            EUR,
            &BalanceQuery::through(date!("2026-03-16")),
        )
        .unwrap();
    // 1000 − 300 + the backdated 20; not the 50 booked on 2026-03-20.
    assert_eq!(balance.net(), eur("720.00"));
    assert_eq!(balance.postings(), 3);
}

#[tokio::test]
async fn us1_dimension_filters_partition_the_balance() {
    let (books, _) = journal().await;
    let all = BalanceQuery::all();
    let slice = |query: BalanceQuery| books.balance("Income:Sales", EUR, &query).unwrap();
    let a = slice(all.clone().matching("cost_centre", "A"));
    let b = slice(all.clone().matching("cost_centre", "B"));
    let missing = slice(all.clone().missing("cost_centre"));
    // A: the January sale, the 2026-03-20 sale and its reversal.
    assert_eq!((a.credit(), a.debit()), (eur("1050.00"), eur("50.00")));
    assert_eq!(b.credit(), eur("20.00"));
    let whole = a.checked_add(&b).unwrap().checked_add(&missing).unwrap();
    assert_eq!(whole, slice(all.clone()));
    let contradiction = all.matching("cost_centre", "A").missing("cost_centre");
    assert!(slice(contradiction).is_zero());
}

#[tokio::test]
async fn us1_position_beyond_the_head_is_refused() {
    let (books, _) = journal().await;
    let size = books.head().size;
    let error = query_error(books.balance(
        "Assets:Bank",
        EUR,
        &BalanceQuery::through_position(size + 150),
    ));
    assert_eq!(
        error,
        QueryError::PositionBeyondHead {
            position: size + 150,
            size
        }
    );
}

#[tokio::test]
async fn us1_unfiltered_trial_balances_balance() {
    let (books, _) = journal().await;
    for query in queries() {
        let trial = books.trial_balance(&query).unwrap();
        if !trial.is_filtered() {
            assert!(trial.is_balanced(), "{query:?}");
        }
    }
    // Cost centre A holds the sales side of its entries only: a slice.
    let slice = books
        .trial_balance(&BalanceQuery::all().matching("cost_centre", "A"))
        .unwrap();
    assert!(slice.is_filtered() && !slice.is_balanced());
    // Holds are the Pending layer's only writer: the fixture's two are
    // resolved, so every Pending row nets to zero.
    let pending = books.trial_balance(&BalanceQuery::all().pending()).unwrap();
    let paths: Vec<String> = pending
        .rows()
        .iter()
        .map(|row| row.path.to_string())
        .collect();
    assert_eq!(paths, ["Assets:Bank", "Assets:Cash"]);
    assert!(pending.is_balanced());
    assert!(pending.rows().iter().all(|row| row.balance.net().is_zero()));
    let settled = books.trial_balance(&BalanceQuery::all()).unwrap();
    let currencies: Vec<_> = settled.currencies().collect();
    assert_eq!(currencies, [EUR, JPY, USD]);
    // Rows by (path, currency), with the class at the head.
    let paths: Vec<String> = settled
        .rows()
        .iter()
        .map(|row| row.path.to_string())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    let travel = settled
        .get(&"Expenses:Travel".parse().unwrap(), EUR)
        .unwrap();
    assert_eq!(travel.class.as_ref().map(|c| c.as_str()), Some("capital"));
    assert_eq!(travel.kind, AccountKind::Expense);
}

#[tokio::test]
async fn us1_a_pinned_head_reads_that_prefix_only() {
    let (books, _) = fixture(MemoryStore::new()).await;
    let (journal, _) = journal().await;
    let size = journal.head().size / 2 + 4;
    let head = books.head_at(size).await.unwrap();
    let prefix = Journal::from_log(journal.id(), journal.leaves().take(size as usize)).unwrap();
    let pinned = BalanceQuery::all().pinned(head);
    let expected = prefix.trial_balance(&BalanceQuery::all()).unwrap();
    assert_eq!(books.trial_balance(&pinned).await.unwrap(), expected);
    assert_eq!(journal.trial_balance(&pinned).unwrap(), expected);
    // A head the log does not have at its size.
    let forged = TreeHead {
        size,
        root: Hash::from_bytes([0xee; 32]),
    };
    let forged = BalanceQuery::all().pinned(forged);
    match books.trial_balance(&forged).await {
        Err(Error::Integrity(error)) => assert_eq!(error.kind(), IntegrityKind::HeadMismatch),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        journal.trial_balance(&forged),
        Err(Error::Integrity(_))
    ));
    // A head beyond the log.
    let ahead = TreeHead {
        size: journal.head().size + 20,
        root: journal.head().root,
    };
    assert_eq!(
        query_error(journal.balance("Assets:Bank", EUR, &BalanceQuery::all().pinned(ahead))),
        QueryError::PositionBeyondHead {
            position: ahead.size,
            size: journal.head().size
        }
    );
}

#[test]
fn us1_a_genesis_only_ledger_has_an_empty_balanced_trial_balance() {
    let books = Journal::new(LedgerId::new("example.com/empty").unwrap());
    let trial = books.trial_balance(&BalanceQuery::all()).unwrap();
    assert!(trial.rows().is_empty() && trial.is_balanced());
    assert_eq!(
        query_error(books.statement("Assets:Bank", EUR, &BalanceQuery::all())),
        QueryError::UnknownAccount {
            account: String::from("Assets:Bank")
        }
    );
}

#[tokio::test]
async fn us1_invalid_questions_are_query_errors() {
    let (books, _) = journal().await;
    let backwards = BalanceQuery::between(date!("2026-03-31"), date!("2026-03-01"));
    assert_eq!(
        query_error(books.balance("Assets:Bank", EUR, &backwards)),
        QueryError::InvalidWindow {
            start: date!("2026-03-31"),
            end: date!("2026-03-01")
        }
    );
    // A grouping node is no account: a subtree is a rollup.
    for named in ["Assets", "Assets:Nowhere", "not:a: path"] {
        assert_eq!(
            query_error(books.balance(named, EUR, &BalanceQuery::all())),
            QueryError::UnknownAccount {
                account: named.to_owned()
            }
        );
    }
    assert_eq!(
        query_error(books.balance("Assets:Bank", EUR, &BalanceQuery::all().missing(" "))),
        QueryError::InvalidLabel {
            text: String::from(" ")
        }
    );
    // An account registered after the prefix read is not in it.
    let before = BalanceQuery::through_position(2);
    assert!(matches!(
        query_error(books.balance("Expenses:Travel", EUR, &before)),
        QueryError::UnknownAccount { .. }
    ));
}

// ── US2: statements ────────────────────────────────────────────────────────

#[tokio::test]
async fn us2_a_period_statement_reconciles() {
    let (books, fixture) = journal().await;
    let march = books
        .statement("Assets:Bank", EUR, &BalanceQuery::period("2026-03"))
        .unwrap();
    let through = |day| {
        books
            .balance("Assets:Bank", EUR, &BalanceQuery::through(day))
            .unwrap()
    };
    assert_eq!(march.opening, through(date!("2026-02-28")));
    assert_eq!(march.closing, through(date!("2026-03-31")));
    assert_eq!(
        (march.start, march.end),
        (Some(date!("2026-03-01")), Some(date!("2026-03-31")))
    );
    // By booking date, then position: the backdated sale first.
    let order: Vec<u64> = march.lines.iter().map(|line| line.position).collect();
    assert_eq!(
        order,
        [
            fixture.backdated_sale.position,
            fixture.late_sale.position,
            fixture.reversal.position,
            fixture.month_end_sale.position
        ]
    );
    let mut running = march.opening;
    for line in &march.lines {
        running = running.post(line.side, line.amount).unwrap();
        assert_eq!(line.running, running);
    }
    assert_eq!(running, march.closing);
}

#[tokio::test]
async fn us2_a_value_date_statement_moves_the_late_value() {
    let (books, fixture) = journal().await;
    let statement = |period: &str| {
        books
            .statement(
                "Assets:Bank",
                EUR,
                &BalanceQuery::period(period).by_value_date(),
            )
            .unwrap()
    };
    let (march, april) = (statement("2026-03"), statement("2026-04"));
    let in_march = march
        .lines
        .iter()
        .any(|line| line.entry == fixture.month_end_sale.id);
    let in_april = april
        .lines
        .iter()
        .any(|line| line.entry == fixture.month_end_sale.id);
    assert!(!in_march && in_april);
    assert_eq!(march.closing, april.opening);
    let through = BalanceQuery::through(date!("2026-03-31")).by_value_date();
    assert_eq!(
        march.closing,
        books.balance("Assets:Bank", EUR, &through).unwrap()
    );
}

#[tokio::test]
async fn us2_a_quiet_window_reads_as_unchanged() {
    let (books, _) = journal().await;
    let may = books
        .statement("Assets:Bank", EUR, &BalanceQuery::period("2026-05"))
        .unwrap();
    assert!(may.lines.is_empty());
    assert_eq!(may.closing, may.opening);
    assert!(!may.opening.is_zero());
}

#[tokio::test]
async fn us2_an_unknown_period_is_refused() {
    let (books, _) = journal().await;
    assert_eq!(
        query_error(books.statement("Assets:Bank", EUR, &BalanceQuery::period("2027-13"))),
        QueryError::UnknownPeriod {
            id: String::from("2027-13")
        }
    );
}

// ── US3: the rollup ────────────────────────────────────────────────────────

fn balance_sheet() -> Journal {
    let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    for path in ["Assets:Bank:Main", "Assets:Cash"] {
        books.open(path, AccountKind::Asset, on).unwrap();
    }
    books
        .open("Equity:Capital", AccountKind::Equity, on)
        .unwrap();
    books.define_months(2026, 3..=3).unwrap();
    books
        .record(
            Entry::new("capital", date!("2026-03-01"))
                .debit("Assets:Bank:Main", eur("1000.00"), EUR)
                .debit("Assets:Cash", eur("190.00"), EUR)
                .credit("Equity:Capital", eur("1190.00"), EUR),
        )
        .unwrap();
    books
}

#[test]
fn us3_grouping_nodes_are_inferred() {
    let books = balance_sheet();
    let sheet = books.rollup(EUR, &BalanceQuery::all()).unwrap();
    let node = |path: &str| sheet.get(&path.parse().unwrap()).unwrap().clone();
    let (assets, bank, main) = (
        node("Assets"),
        node("Assets:Bank"),
        node("Assets:Bank:Main"),
    );
    for inferred in [&assets, &bank] {
        assert!(!inferred.is_registered() && inferred.kind.is_none());
        assert!(inferred.own.is_zero());
    }
    assert_eq!(assets.subtree.net(), eur("1190.00"));
    assert_eq!(bank.subtree.net(), eur("1000.00"));
    assert_eq!(
        (main.own, main.kind),
        (main.subtree, Some(AccountKind::Asset))
    );
    assert_eq!(main.own.net(), eur("1000.00"));
}

#[tokio::test]
async fn us3_truncating_keeps_totals_and_roots_sum_to_the_trial_balance() {
    let (books, _) = journal().await;
    for query in [BalanceQuery::all(), BalanceQuery::period("2026-03")] {
        let trial = books.trial_balance(&query).unwrap();
        for currency in trial.currencies() {
            let sheet = Rollup::of(&trial, currency).unwrap();
            assert_eq!(sheet, books.rollup(currency, &query).unwrap());
            assert_eq!(sheet.total().unwrap(), trial.totals(currency).unwrap());
            let short = sheet.to_depth(1);
            assert!(short.nodes().iter().all(|node| node.depth() == 1));
            assert_eq!(short.total().unwrap(), sheet.total().unwrap());
            for node in short.nodes() {
                assert_eq!(sheet.get(&node.path), Some(node));
            }
        }
    }
}

#[test]
fn us3_the_rollup_prints_as_an_indented_report() {
    let sheet = balance_sheet().rollup(EUR, &BalanceQuery::all()).unwrap();
    assert_eq!(
        sheet.to_string(),
        "Assets      1190.00\n  Bank      1000.00\n    Main    1000.00\n  Cash       190.00\nEquity     -1190.00\n  Capital  -1190.00\n"
    );
}

// ── US4: balance assertions ────────────────────────────────────────────────

fn end_of(date: doubleentry::Date, basis: DateBasis) -> AssertAt {
    AssertAt::EndOfDay { date, basis }
}

fn assertions() -> Vec<BalanceAssertion> {
    vec![
        BalanceAssertion::new(
            "Assets:Bank",
            EUR,
            end_of(date!("2026-03-31"), DateBasis::Booking),
            eur("820.00"),
        ),
        BalanceAssertion::new(
            "Assets:Bank",
            EUR,
            end_of(date!("2026-03-31"), DateBasis::Value),
            eur("730.00"),
        ),
        BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(20), eur("0.00")),
        BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(20), eur("0.00")).pending(),
    ]
}

#[tokio::test]
async fn us4_end_of_day_is_inclusive() {
    let (books, _) = journal().await;
    let claim = |day, expected| {
        books
            .check_assertion(&BalanceAssertion::new(
                "Assets:Bank",
                EUR,
                end_of(day, DateBasis::Booking),
                eur(expected),
            ))
            .unwrap()
    };
    // The 2026-03-31 sale is in, the 2026-04-01 payment is not.
    assert!(claim(date!("2026-03-31"), "820.00").is_held());
    assert!(!claim(date!("2026-03-31"), "920.00").is_held());
    assert!(claim(date!("2026-04-01"), "920.00").is_held());
}

#[tokio::test]
async fn us4_a_failure_reports_expected_actual_and_difference() {
    let (books, _) = journal().await;
    let outcomes = books.check_assertions(&assertions()).unwrap();
    assert_eq!(
        outcomes
            .iter()
            .map(AssertionOutcome::is_held)
            .collect::<Vec<_>>(),
        [true, false, true, true]
    );
    let AssertionOutcome::Failed(failure) = &outcomes[1] else {
        panic!("the value-date claim is 10.00 too high");
    };
    assert_eq!(failure.actual, eur("720.00"));
    assert_eq!(failure.difference.direction, Deviation::Shortfall);
    assert_eq!(failure.difference.units, 1000);
    assert_eq!(
        failure.to_string(),
        "Assets:Bank EUR settled at the end of 2026-03-31 by value date: expected 730.00, actual 720.00, shortfall 10.00"
    );
}

#[tokio::test]
async fn us4_an_assertion_beyond_the_head_is_refused() {
    let (books, _) = journal().await;
    let size = books.head().size;
    let claim = BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(size), eur("0.00"));
    assert_eq!(
        query_error(books.check_assertion(&claim)),
        QueryError::PositionBeyondHead {
            position: size,
            size
        }
    );
}

// ── US5: open items ────────────────────────────────────────────────────────

#[tokio::test]
async fn us5_residuals_follow_clearings_and_their_reset() {
    let (books, fixture) = journal().await;
    let open = |day| {
        books
            .open_items("Assets:Receivables", EUR, &BalanceQuery::through(day))
            .unwrap()
    };
    // Before the reset: the invoice has 19.00 open; the payment is fully
    // applied.
    let before = open(date!("2026-04-09"));
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].entry, fixture.invoice.id);
    assert_eq!(
        (before[0].original, before[0].applied, before[0].residual),
        (eur("119.00"), eur("100.00"), eur("19.00"))
    );
    // The reset dated 2026-04-10 reopens both, oldest first.
    let after = open(date!("2026-04-10"));
    let residuals: Vec<(u64, Amount)> = after
        .iter()
        .map(|item| (item.position, item.residual))
        .collect();
    assert_eq!(
        residuals,
        [
            (fixture.invoice.position, eur("119.00")),
            (fixture.payment.position, eur("100.00"))
        ]
    );
    assert_eq!(
        after,
        books
            .open_items("Assets:Receivables", EUR, &BalanceQuery::all())
            .unwrap()
    );
    assert!(fixture.reset > fixture.clearing);
}

#[tokio::test]
async fn us5_a_prefix_before_the_clearing_ignores_it() {
    let (books, fixture) = journal().await;
    let before = BalanceQuery::through_position(fixture.clearing - 1);
    let open = books
        .open_items("Assets:Receivables", EUR, &before)
        .unwrap();
    assert!(open.iter().all(|item| item.applied.is_zero()));
    let at = BalanceQuery::through_position(fixture.clearing);
    let open = books.open_items("Assets:Receivables", EUR, &at).unwrap();
    assert_eq!(open.len(), 1);
}

// ── Regressions ────────────────────────────────────────────────────────────

/// `report_qe3_position_beyond_head_refused`: 0.7 answered an assertion
/// "after 1 000 entries" on a one-entry journal by reading everything.
// PoC: qe3 qe3_assertion_over_unreached_prefix_holds
#[tokio::test]
async fn report_qe3_position_beyond_head_refused() {
    let (journal, _) = journal().await;
    let (books, _) = fixture(MemoryStore::new()).await;
    let size = journal.head().size;
    let beyond = QueryError::PositionBeyondHead {
        position: 1_000,
        size,
    };
    let query = BalanceQuery::through_position(1_000);
    assert_eq!(
        query_error(journal.balance("Assets:Bank", EUR, &query)),
        beyond
    );
    assert_eq!(query_error(journal.trial_balance(&query)), beyond);
    assert_eq!(
        query_error(books.balance("Assets:Bank", EUR, &query).await),
        beyond
    );
    assert_eq!(query_error(books.trial_balance(&query).await), beyond);
    let claim = BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(1_000), eur("0.00"));
    assert_eq!(query_error(journal.check_assertion(&claim)), beyond);
    assert_eq!(query_error(books.check_assertion(&claim).await), beyond);
}

/// `report_qe10_assertion_difference_never_overflows`: 0.7 failed with an
/// overflow instead of reporting the mismatch. A balance of `i128::MAX`
/// units is unreachable through a ledger, so the pure check is tested.
// PoC: qe10
#[test]
fn report_qe10_assertion_difference_never_overflows() {
    let actual: Balance = Balance::from_parts(Amount::MAX, Amount::ZERO, 1).unwrap();
    let claim = BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(1), Amount::MIN);
    let AssertionOutcome::Failed(failure) = claim.check(&actual) else {
        panic!("i128::MAX is not -i128::MAX");
    };
    assert_eq!(failure.actual, Amount::MAX);
    assert_eq!(failure.difference.direction, Deviation::Excess);
    assert_eq!(failure.difference.units, 2 * i128::MAX.unsigned_abs());
    assert!(failure.to_string().ends_with(&format!(
        "excess {}.{:02}",
        2 * i128::MAX.unsigned_abs() / 100,
        2 * i128::MAX.unsigned_abs() % 100
    )));
}

/// `no_sql_amount_aggregates` (SC-006, E-F9): the SQL of every store sums
/// no money — 0.7's `SUM` overflowed where Rust did not.
// PoC: E-F9
#[test]
fn no_sql_amount_aggregates() {
    fn scan(dir: &std::path::Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, found);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap().to_uppercase();
            for aggregate in ["SUM(", "AVG(", "TOTAL(", "SUM (", "AVG (", "TOTAL ("] {
                if text.contains(aggregate) {
                    found.push(format!("{} uses {aggregate}", path.display()));
                }
            }
        }
    }
    let mut found = Vec::new();
    scan(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/sql"),
        &mut found,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[tokio::test]
async fn reads_of_a_fixture_ledger_check_out() {
    let (books, fixture): (Ledger<MemoryStore>, Fixture) = fixture(MemoryStore::new()).await;
    books.audit().await.unwrap();
    assert_eq!(books.memo(fixture.erased).await.unwrap(), None);
}
