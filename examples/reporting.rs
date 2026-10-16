//! A month's sales by segment, the account tree rolled up, and a bank statement.
use doubleentry::Posting;
use doubleentry::prelude::*;

const START: Date = date!("2026-01-01");
const BANK: &str = "Assets:Bank:Main";

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/shop")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 1..=12)?;
    books.open(BANK, AccountKind::Asset, START)?;
    books.open("Assets:Cash", AccountKind::Asset, START)?;
    books.open("Income:Sales", AccountKind::Income, START)?;
    let sales = [
        (
            "s1",
            date!("2026-03-02"),
            BANK,
            amount!("1000.00"),
            "Retail",
        ),
        (
            "s2",
            date!("2026-03-05"),
            "Assets:Cash",
            amount!("190.00"),
            "Wholesale",
        ),
        ("s3", date!("2026-03-20"), BANK, amount!("50.00"), "Retail"),
        ("s4", date!("2026-04-02"), BANK, amount!("70.00"), "Retail"),
    ];
    for (key, day, to, eur, segment) in sales {
        let revenue = Posting::credit("Income:Sales", eur, EUR).dimension("segment", segment);
        books.record(Entry::new(key, day).debit(to, eur, EUR).posting(revenue))?;
    }
    let march = BalanceQuery::between(date!("2026-03-01"), date!("2026-03-31"));
    let retail = march.clone().matching("segment", "Retail");
    let retail = books.balance("Income:Sales", EUR, &retail)?;
    assert_eq!(retail.net(), amount!("-1050.00"));
    println!("retail sales in March {:>12}", retail.net());

    print!("{}", books.rollup(EUR, &march)?); // the account tree, indented and aligned
    print!(
        "{}",
        books.statement(BANK, EUR, &BalanceQuery::period("2026-03"))?
    ); // opening, lines, closing
    Ok(())
}
