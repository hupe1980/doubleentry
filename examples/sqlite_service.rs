//! A payment service over SQLite: at-least-once delivery, bounded retries, one error type.
#![allow(clippy::panic, clippy::arithmetic_side_effects)] // an example asserts its outcomes; the library never panics
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};

const START: Date = date!("2026-01-01");

const BANK: &str = "Assets:Bank";
const WALLET: &str = "Liabilities:Wallets:alice";

struct Payments {
    ledger: Ledger<SqlStore<Sqlite>>,
}

impl Payments {
    async fn pay_in(&self, message_id: &str, eur: Amount) -> Result<Recorded> {
        let entry = Entry::new(message_id, date!("2026-03-15"));
        let entry = entry.debit(BANK, eur, EUR).credit(WALLET, eur, EUR);
        let mut attempt = 0;
        loop {
            // The same key on every attempt: a retry can never book twice.
            match self.ledger.record(entry.clone()).await {
                Err(e) if e.is_transient() && attempt < 3 => attempt += 1,
                outcome => return outcome,
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let store = SqlStore::sqlite("sqlite::memory:").await?;
    let ledger = Ledger::new(store, LedgerId::new("example.com/payments")?).await?;
    ledger.declare_currency(EUR, 2, START).await?; // a repeat is a no-op
    ledger.define_months(2026, 1..=12).await?;
    ledger.open(BANK, AccountKind::Asset, START).await?;
    ledger.open(WALLET, AccountKind::Liability, START).await?;
    let service = Payments { ledger };

    let first = service.pay_in("psp-0001", amount!("25.00")).await?;
    let again = service.pay_in("psp-0001", amount!("25.00")).await?; // redelivered
    assert!(again.replayed && again.id == first.id);
    match service.pay_in("psp-0001", amount!("26.00")).await {
        Err(Error::Rejected(r @ Rejection::KeyConflict { .. })) => println!("{r}"),
        other => panic!("expected KeyConflict, got {other:?}"),
    }
    let wallet = service
        .ledger
        .balance(WALLET, EUR, &BalanceQuery::all())
        .await?;
    assert_eq!(wallet.net(), amount!("-25.00"));
    Ok(())
}
