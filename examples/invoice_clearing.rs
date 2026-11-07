//! An invoice, a partial payment cleared against it in the same commit, and
//! the open item that remains.
#![allow(clippy::indexing_slicing)] // an example asserts its outcomes; the library never panics
use doubleentry::prelude::*;

const AR: &str = "Assets:Receivables:Customer-42";
const START: Date = date!("2026-01-01");

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 1..=12)?;
    books.open(AR, AccountKind::Asset, START)?;
    books.open("Assets:Bank", AccountKind::Asset, START)?;
    books.open("Income:Sales", AccountKind::Income, START)?;
    books.open("Liabilities:VAT", AccountKind::Liability, START)?;

    let invoice = books.record(
        Entry::new("INV-1", date!("2026-03-01"))
            .document(Document::new("invoice", "INV-1")?)
            .debit(AR, amount!("1190.00"), EUR)
            .credit("Income:Sales", amount!("1000.00"), EUR)
            .credit("Liabilities:VAT", amount!("190.00"), EUR),
    )?;
    let (paid, on) = (amount!("500.00"), date!("2026-03-10"));
    let debit = PostingRef::at(invoice.id, 0); // the invoice's receivable leg
    let credit = PostingRef::at(books.entry_id("PAY-1")?, 1); // the payment's, before it exists
    books.batch([
        Entry::new("PAY-1", on)
            .debit("Assets:Bank", paid, EUR)
            .credit(AR, paid, EUR)
            .into(),
        Clearing::new("CLR-1", debit, credit, paid, on).into(),
    ])?; // one commit: never a payment without its assignment

    let open = books.open_items(AR, EUR, &BalanceQuery::all())?;
    assert_eq!(open[0].residual, amount!("690.00"));
    println!("{}", open[0]); // 2026-03-01 18/0 debit 1190.00, applied 500.00, open 690.00
    Ok(())
}
