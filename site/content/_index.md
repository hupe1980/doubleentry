+++
title = "doubleentry"
description = "An immutable, tamper-evident double-entry bookkeeping engine for Rust: one pure rule core, exact integer money, and one append-only RFC 6962 Merkle log with inclusion, consistency and sealed-balance proofs and C2SP checkpoints."
template = "index.html"

[extra]
# Marks this as the landing page, so the <title> reads "doubleentry —
# Bookkeeping you can prove." rather than "doubleentry · doubleentry".
home = true
+++

## In practice

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))?;

let rec = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?; // Ok(Recorded) | Err(Error::Rejected(r))

let proof = books.prove_entry(rec.id)?; // verifies against books.head()
assert_eq!((proof.index, proof.size), (rec.position, books.head().size));
```

A ledger is one append-only RFC 6962 log of typed records — master data,
periods, entries, clearings, seals. One pure rule core decides every write,
returning the records to append or one `Rejection` with a stable code. `Journal`
runs it in memory; `Ledger` runs it inside a store's write transaction (SQLite,
PostgreSQL or memory), with the same calls `.await`ed. The idempotency key is the entry's
identity, so recording the same draft again returns the original receipt.
