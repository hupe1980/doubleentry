//! Wallets that cannot be overdrawn: a limit, an idempotent retry, one error to match.
#![allow(clippy::panic)] // an example asserts its outcomes; the library never panics
use doubleentry::prelude::*;

const START: Date = date!("2026-01-01");

const BANK: &str = "Assets:Bank";
const ALICE: &str = "Liabilities:Wallets:alice";

fn move_money(
    books: &mut Journal,
    key: &str,
    from: &str,
    to: &str,
    eur: Amount,
) -> Result<Recorded> {
    let on = date!("2026-03-02");
    books.record(
        Entry::new(key, on)
            .debit(to, eur, EUR)
            .credit(from, eur, EUR),
    )
}

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/wallets")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 1..=12)?;
    books.open(BANK, AccountKind::Asset, START)?;
    books.open(ALICE, AccountKind::Liability, START)?;
    books.set_limit(ALICE, BalanceLimit::NoDebitBalance, START)?;

    move_money(&mut books, "top-up-1", ALICE, BANK, amount!("100.00"))?;
    let first = move_money(&mut books, "withdraw-1", BANK, ALICE, amount!("30.00"))?;
    let retry = move_money(&mut books, "withdraw-1", BANK, ALICE, amount!("30.00"))?;
    assert!(retry.replayed && retry.id == first.id); // a retry never books twice

    match move_money(&mut books, "withdraw-2", BANK, ALICE, amount!("80.00")) {
        Err(Error::Rejected(r @ Rejection::LimitExceeded { .. })) => println!("refused: {r}"),
        other => panic!("expected a limit refusal, got {other:?}"),
    }
    let wallet = books.balance(ALICE, EUR, &BalanceQuery::all())?;
    assert_eq!(wallet.net(), amount!("-70.00")); // a funded wallet is credit-normal
    println!("alice {wallet:>30}");
    Ok(())
}
