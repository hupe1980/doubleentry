//! A ledger entry exactly once per committed business change: a transactional outbox.
#![allow(clippy::panic, clippy::arithmetic_side_effects)] // an example asserts its outcomes; the library never panics
use std::cell::RefCell;

use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};

const START: Date = date!("2026-01-01");
const RECEIVABLES: &str = "Assets:Receivables";
const SALES: &str = "Income:Sales";

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Pending,
    Done,
    Parked(u16), // a final refusal: a person decides
}

/// An outbox row, written in the same transaction as the invoice. It holds the
/// whole draft: must not be recomputed from mutable state (rates, current
/// prices); otherwise a retry is `KeyConflict` (200).
#[derive(Clone)]
struct Row {
    key: String,
    amount: Amount,
    date: Date,
    status: Status,
}

/// Stands in for the service's own database: invoices and the outbox table.
#[derive(Default)]
struct Shop(RefCell<(Vec<String>, Vec<Row>)>);

impl Shop {
    /// One business transaction: all or nothing.
    fn commit<E>(
        &self,
        change: impl FnOnce(&mut Vec<String>, &mut Vec<Row>) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut db = self.0.borrow_mut();
        let (mut invoices, mut outbox) = db.clone();
        change(&mut invoices, &mut outbox)?; // an error rolls back: nothing is swapped in
        *db = (invoices, outbox);
        Ok(())
    }

    /// The invoice and its outbox row, in one transaction.
    fn invoice(&self, key: &str, amount: Amount, date: Date) -> Result<(), &'static str> {
        let row = Row {
            key: key.into(),
            amount,
            date,
            status: Status::Pending,
        };
        self.commit(|invoices, outbox| {
            invoices.push(key.into());
            outbox.push(row);
            Ok(())
        })
    }

    fn rows(&self) -> Vec<Row> {
        self.0.borrow_mut().1.clone()
    }

    fn status(&self, key: &str) -> Option<Status> {
        self.rows()
            .into_iter()
            .find(|row| row.key == key)
            .map(|row| row.status)
    }

    fn mark(&self, key: &str, status: Status) {
        let mut db = self.0.borrow_mut();
        db.1.iter_mut()
            .filter(|row| row.key == key)
            .for_each(|row| row.status = status.clone());
    }
}

/// The same row always rebuilds the same draft.
fn draft(row: &Row) -> Entry {
    Entry::new(row.key.as_str(), row.date)
        .debit(RECEIVABLES, row.amount, EUR)
        .credit(SALES, row.amount, EUR)
}

/// The worker: runs on start-up and then continuously.
async fn drain(ledger: &Ledger<SqlStore<Sqlite>>, shop: &Shop) -> Result<()> {
    for row in shop
        .rows()
        .into_iter()
        .filter(|row| row.status == Status::Pending)
    {
        let status = match ledger.record(draft(&row)).await {
            Ok(_) => Status::Done, // first time or a replay: either way it is booked once
            Err(e) if e.is_transient() || e.may_have_applied() => Status::Pending, // retry, same key
            Err(Error::Rejected(r)) => Status::Parked(r.code()),
            Err(e) => return Err(e), // integrity or configuration: stop and alert
        };
        shop.mark(&row.key, status);
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let store = SqlStore::sqlite("sqlite::memory:").await?;
    let ledger = Ledger::new(store, LedgerId::new("example.com/shop")?).await?;
    ledger.declare_currency(EUR, 2, START).await?;
    ledger.define_months(2026, 1..=12).await?;
    ledger.open(RECEIVABLES, AccountKind::Asset, START).await?;
    ledger.open(SALES, AccountKind::Income, START).await?;
    ledger.seal_period("2026-01").await?;
    let shop = Shop::default();

    // 1. The business commit lands, then the process dies before the worker runs.
    shop.invoice("inv-A", amount!("100.00"), date!("2026-03-02"))?;
    drain(&ledger, &shop).await?; // after the restart
    println!("A: {:?}", shop.status("inv-A"));

    // 2. The worker recorded B, then died before marking the row done.
    shop.invoice("inv-B", amount!("40.00"), date!("2026-03-05"))?;
    let Some(row) = shop.rows().pop() else {
        panic!("B's row is in the outbox")
    };
    ledger.record(draft(&row)).await?;
    drain(&ledger, &shop).await?; // the replay is success
    assert!(ledger.record(draft(&row)).await?.replayed);
    println!("B: {:?}, recorded once", shop.status("inv-B"));

    // 3. The business transaction fails: no invoice, no outbox row, no entry.
    let failed = shop.commit(|invoices, _| {
        invoices.push("inv-C".into());
        Err("the customer's credit check failed")
    });
    assert!(failed.is_err() && shop.status("inv-C").is_none());
    println!("C: rolled back");

    // 4. A final refusal is parked, never retried in a loop.
    shop.invoice("inv-D", amount!("15.00"), date!("2026-01-20"))?; // January is sealed
    drain(&ledger, &shop).await?;
    let Some(Status::Parked(code)) = shop.status("inv-D") else {
        panic!("D should be parked")
    };
    println!("D: parked with rejection {code}");

    for (key, booked) in [
        ("inv-A", true),
        ("inv-B", true),
        ("inv-C", false),
        ("inv-D", false),
    ] {
        assert_eq!(ledger.entry_by_key(key).await?.is_some(), booked, "{key}");
    }
    Ok(())
}
