//! A payment run as an accounting service books it (Zahlungszuordnung): a
//! customer pays three invoices at once, the payment and its FIFO
//! assignment commit together, a retry replays, and an over-assignment is
//! refused whole. Then the reads the service shows: the locked periods and
//! a year's totals per entry kind.
#![allow(clippy::indexing_slicing)] // an example asserts its outcomes; the library never panics
use doubleentry::BatchReceipt;
use doubleentry::prelude::*;

const AR: &str = "Assets:Receivables:Customer-42";
const START: Date = date!("2026-01-01");

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 1..=12)?;
    for (path, kind) in [
        (AR, AccountKind::Asset),
        ("Assets:Bank", AccountKind::Asset),
    ] {
        books.open(path, kind, START)?;
    }
    books.open("Income:Sales", AccountKind::Income, START)?;

    // Three open invoices, oldest first.
    let invoices = [
        ("INV-1", amount!("100.00")),
        ("INV-2", amount!("250.00")),
        ("INV-3", amount!("80.00")),
    ];
    for (key, total) in invoices {
        books.record(
            Entry::new(key, date!("2026-02-10"))
                .kind("invoice")
                .debit(AR, total, EUR)
                .credit("Income:Sales", total, EUR),
        )?;
    }

    // 400.00 arrives: the payment and its assignment, FIFO, in one batch.
    let (paid, on) = (amount!("400.00"), date!("2026-03-05"));
    let payment = PostingRef::at(books.entry_id("PAY-7")?, 1);
    let mut run: Vec<BatchItem> = vec![
        Entry::new("PAY-7", on)
            .kind("payment")
            .debit("Assets:Bank", paid, EUR)
            .credit(AR, paid, EUR)
            .into(),
    ];
    let mut left = paid;
    for (key, total) in invoices {
        let take = total.min(left);
        if take.is_positive() {
            let invoice = PostingRef::at(books.entry_id(key)?, 0);
            run.push(Clearing::new(format!("PAY-7/{key}"), invoice, payment, take, on).into());
            left = left.checked_sub(take)?;
        }
    }
    let receipts = books.batch(run.clone())?;
    assert_eq!(receipts.len(), 4); // the payment and three assignments
    // The outbox worker retries after a timeout: nothing is booked twice.
    assert!(books.batch(run)?.iter().all(BatchReceipt::replayed));

    // Assigning more than the payment has left is refused whole.
    let refused = books.batch([
        Entry::new("PAY-8", on)
            .debit("Assets:Bank", amount!("10.00"), EUR)
            .credit(AR, amount!("10.00"), EUR)
            .into(),
        Clearing::new(
            "PAY-8/INV-3",
            PostingRef::at(books.entry_id("INV-3")?, 0),
            PostingRef::at(books.entry_id("PAY-8")?, 1),
            amount!("20.00"),
            on,
        )
        .into(),
    ]);
    assert!(matches!(
        refused,
        Err(Error::BatchRejected {
            index: 1,
            rejection: Rejection::OverApplied { .. }
        })
    ));
    assert!(books.entry_by_key("PAY-8").is_none());

    let open = books.open_items(AR, EUR, &BalanceQuery::all())?;
    assert_eq!(open[0].residual, amount!("30.00")); // INV-3: 80.00, 50.00 assigned

    // The month closes: the service reads what is locked.
    books.seal_through("2026-02")?;
    assert_eq!(books.watermark()?, Some(date!("2026-02-28")));
    let locked: Vec<String> = books
        .periods()?
        .iter()
        .filter(|p| p.is_sealed())
        .map(|p| p.id.to_string())
        .collect();
    assert_eq!(locked, ["2026-01", "2026-02"]);

    // A year's totals per entry kind, without a mirrored dimension.
    let year = BalanceQuery::between(START, date!("2026-12-31"));
    let invoiced = books.balance(AR, EUR, &year.clone().of_kind("invoice"))?;
    let received = books.balance(AR, EUR, &year.of_kind("payment"))?;
    assert_eq!(
        (invoiced.debit(), received.credit()),
        (amount!("430.00"), amount!("400.00"))
    );
    println!(
        "invoiced {} · received {} · open {}",
        invoiced.debit(),
        received.credit(),
        open[0].residual
    );
    Ok(())
}
