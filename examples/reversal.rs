//! A wrong booking in a sealed month, reversed and rebooked in the next one.
#![allow(clippy::panic)] // an example asserts its outcomes; the library never panics
use doubleentry::prelude::*;

const START: Date = date!("2026-01-01");

fn bill(key: &str, on: Date, expense: &str) -> Entry {
    let rent = amount!("420.00");
    Entry::new(key, on)
        .debit(expense, rent, EUR)
        .credit("Assets:Cash", rent, EUR)
}

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 3..=4)?;
    books.open("Assets:Cash", AccountKind::Asset, START)?;
    books.open("Expenses:Rent", AccountKind::Expense, START)?;
    books.open("Expenses:Office", AccountKind::Expense, START)?;

    let wrong = books.record(bill("bill-77", date!("2026-03-15"), "Expenses:Rent"))?;
    books.seal_period("2026-03")?;
    let late = books.record(bill("bill-78", date!("2026-03-20"), "Expenses:Rent"));
    assert!(matches!(
        late,
        Err(Error::Rejected(Rejection::PeriodSealed { .. }))
    ));

    books.record(books.reversal(wrong.id, "bill-77-rev", date!("2026-04-01"))?)?;
    books.record(bill("bill-77-fix", date!("2026-04-01"), "Expenses:Office"))?;
    let twice = books.reversal(wrong.id, "bill-77-rev-2", date!("2026-04-02"))?;
    match books.record(twice) {
        Err(Error::Rejected(r @ Rejection::AlreadyReversed { .. })) => println!("{r}"),
        other => panic!("expected AlreadyReversed, got {other:?}"),
    }
    let rent = books.balance("Expenses:Rent", EUR, &BalanceQuery::all())?;
    assert_eq!(rent.net(), amount!("0.00"));
    println!("rent {rent}"); // the gross 420.00 / 420.00 survives the reversal
    Ok(())
}
