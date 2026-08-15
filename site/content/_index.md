+++
title = "doubleentry"
description = "An immutable, tamper-evident double-entry bookkeeping engine for Rust. Balanced by construction, exact integer money, and an append-only Merkle log with inclusion, consistency and balance proofs."
template = "index.html"

[extra]
# Marks this as the landing page, so the <title> reads "doubleentry —
# Bookkeeping you can prove." rather than "doubleentry · doubleentry".
home = true
+++

## In practice

A journal is one entity's books — its accounts, its calendar, its policy, its
entries, and the Merkle log that commits to them. Record an entry, then prove it
was recorded.

```rust
use doubleentry::period::LedgerId;
use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal};
use time::macros::date;

type Eur = Amount<2>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);

    // Accounts are paths in a hierarchy you define. Only leaves are postable.
    let cash    = journal.accounts_mut().register_path("Assets:Cash",  date!(2026-01-01))?;
    let revenue = journal.accounts_mut().register_path("Income:Sales", date!(2026-01-01))?;

    let recorded = journal.record(
        Entry::new(
            EntryId::generate(),
            IdempotencyKey::new(b"invoice-2026-0001".to_vec())?,
            date!(2026-03-15),
        )
        .debit(cash,     Eur::parse("1190.00")?, Currency::EUR)
        .credit(revenue, Eur::parse("1190.00")?, Currency::EUR),
    )?;

    // Prove the entry is committed to, without revealing any other entry.
    let head  = journal.head();
    let proof = journal.prove_inclusion(recorded.require_index()?)?;
    assert!(proof.verify(&recorded.content_hash, &head));

    Ok(())
}
```

Validation runs against *this* journal's accounts, calendar and policy. There is
no separate context to build and no second object to keep in step — what
validation consults and what stores the result are the same thing.
