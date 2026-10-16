//! Three currencies with their own exponents, FX recorded as evidence, aligned columns.
use doubleentry::prelude::*;

const START: Date = date!("2026-01-01");

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/treasury")?);
    for (currency, exponent) in [(EUR, 2), (USD, 2), (JPY, 0)] {
        books.declare_currency(currency, exponent, START)?;
    }
    books.define_months(2026, 1..=12)?;
    for path in ["Assets:Bank:EUR", "Assets:Bank:USD", "Assets:Bank:JPY"] {
        books.open(path, AccountKind::Asset, START)?;
    }
    books.open("Equity:Capital", AccountKind::Equity, START)?;
    books.open("Equity:FX", AccountKind::Equity, START)?;

    let fund = |yen| {
        Entry::new("fund", date!("2026-03-01"))
            .debit("Assets:Bank:EUR", amount!("10000.00"), EUR)
            .credit("Equity:Capital", amount!("10000.00"), EUR)
            .debit("Assets:Bank:JPY", yen, JPY)
            .credit("Equity:Capital", yen, JPY)
    };
    let refused = books.record(fund(amount!("1.50"))); // the yen has no minor unit
    assert!(matches!(
        refused,
        Err(Error::Rejected(Rejection::ExcessPrecision { .. }))
    ));
    books.record(fund(amount!("150000")))?;
    books.record(
        Entry::new("fx-1", date!("2026-03-02"))
            .rate(Rate::new(EUR, USD, 1_087_350, -6)?) // evidence, not a conversion
            .credit("Assets:Bank:EUR", amount!("1000.00"), EUR)
            .debit("Equity:FX", amount!("1000.00"), EUR)
            .debit("Assets:Bank:USD", amount!("1087.35"), USD)
            .credit("Equity:FX", amount!("1087.35"), USD),
    )?;
    for row in books.trial_balance(&BalanceQuery::all())?.rows() {
        let (account, currency, net) = (&row.path, row.currency, row.balance.net());
        println!("{account:<16} {currency:>4} {net:>10}");
    }
    Ok(())
}
