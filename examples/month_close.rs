//! Close and seal March, then prove one sealed balance to an auditor who trusts only a head.
use doubleentry::prelude::*;

const START: Date = date!("2026-01-01");

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 1..=12)?;
    books.open("Assets:Cash", AccountKind::Asset, START)?;
    books.open("Income:Sales", AccountKind::Income, START)?;
    for (key, day) in [("s-1", date!("2026-03-03")), ("s-2", date!("2026-03-28"))] {
        let sale = Entry::new(key, day).debit("Assets:Cash", amount!("150.00"), EUR);
        books.record(sale.credit("Income:Sales", amount!("150.00"), EUR))?;
    }
    // Periods seal in order: January and February first, empty or not.
    let sealed = books.seal_through("2026-03")?;
    assert_eq!(sealed.len(), 3);
    let published = books.head(); // what the auditor archives
    let proof = books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-03")?;

    // The auditor holds the proof and a trusted head; nothing else.
    let proven = proof.verify(&published)?;
    assert_eq!(proven.balance.net(), amount!("300.00"));
    let (account, balance, period) = (&proven.account, &proven.balance, &proven.period);
    println!("{account} held {balance} at the close of {period}");
    Ok(())
}
