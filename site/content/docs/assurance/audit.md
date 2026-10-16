+++
title = "Audit"
description = "audit() replays the log through the rule core and compares every persisted table."
weight = 4
+++

## What to call

- [`Journal::audit`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.audit) and [`Ledger::audit`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.audit) → [`AuditReport`](https://docs.rs/doubleentry/latest/doubleentry/struct.AuditReport.html), or [`Error::Integrity`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#variant.Integrity).
- [`IntegrityError`](https://docs.rs/doubleentry/latest/doubleentry/error/struct.IntegrityError.html): [`kind`](https://docs.rs/doubleentry/latest/doubleentry/error/struct.IntegrityError.html#method.kind) and [`position`](https://docs.rs/doubleentry/latest/doubleentry/error/struct.IntegrityError.html#method.position); its `source()` is the detailed [`AuditFailure`](https://docs.rs/doubleentry/latest/doubleentry/error/struct.AuditFailure.html).
- [`Journal::from_log`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.from_log): a journal rebuilt from leaves through the same replay.
- [`store::ProjectionPart`](https://docs.rs/doubleentry/latest/doubleentry/store/enum.ProjectionPart.html): the tables an audit compares.

## A replay, not a checksum

`audit()` starts from an empty state and feeds every leaf, from position 0,
through the **same rule core** that decided it:

1. Each leaf is decoded strictly and turned back into the operation that wrote it.
2. The core decides that operation against the replayed state and must produce
   **exactly that leaf**. A record the core would refuse (an unbalanced entry, an
   entry in a sealed period) or would not have written (a repeated key, a no-op
   master-data change) fails at its position.
3. Each entry's draft fingerprint is recomputed from its record and memo, and
   each memo must match its commitment.
4. Each seal's balance trees and totals are rebuilt.
5. The replayed state **projects every table the store keeps**, compared row
   for row and byte for byte: leaves and stored tree nodes, the id ↔ key ↔
   position index, master-data versions, periods, postings with dates and
   dimensions, seal leaves and nodes, clearings, memos and fingerprints,
   maintained balances, hold totals and the hold index, the meta row. A crate
   test fails if the schema has a column no projection part covers.

A store that passes holds exactly what the rule core produces from its log.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let sale = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;
books.seal_period("2026-03")?;

let report = books.audit()?;
assert_eq!(report.head, books.head());
assert_eq!((report.entries, report.accounts, report.seals), (1, 2, 1));

// Someone edits one byte of the entry's leaf.
let leaves: Vec<_> = books.leaves().collect();
let at = usize::try_from(sale.position)?;
let mut forged = leaves[at].bytes.to_vec();
let last = forged.len() - 1;
forged[last] ^= 0x01;
let mut tampered = leaves.clone();
tampered[at].bytes = &forged;

match Journal::<2>::from_log(books.id(), tampered) {
    Err(Error::Integrity(error)) => assert_eq!(error.position(), Some(sale.position)),
    other => return Err(format!("a forged leaf must fail, got {other:?}").into()),
}
```

A failure is `Error::Integrity` at the **first** difference, with its kind and,
for a leaf, its position; for a table difference, `source()` names the table and
the first row where store and replay part. Never retry it: restore from an
audited backup and find out who wrote to the database.

The SQLite and PostgreSQL tests tamper with real databases, triggers dropped as
a superuser could, and require the audit to fail: an edited leaf byte, swapped
entry ids, edited balances, moved posting dates, rewritten account versions,
edited seal nodes, deleted clearing references, nulled fingerprints, zeroed tree
nodes, flipped hold states, a replaced genesis hash.

## Verify on read

Between audits, every read that hands a stored record to the core or a caller
verifies it: the entry id must derive from the key its leaf commits to, the
leaf bytes must hash to the stored hash. Proofs are verified before a store
hands them out. A tampered index cannot redirect a reversal or a clearing.

## When to run it

`O(n)` in the log: every leaf and every table read once. On a `Ledger` it takes
no write lock and follows a growing log until the tables match one size. Run it:

- after every [restore](@/docs/operating/backup-and-restore.md), before writing;
- periodically (nightly or weekly, by size);
- before handing the books to an auditor, with a witnessed checkpoint of that
  head;
- after any manual intervention in the database.

```rust
use doubleentry::prelude::*;
use doubleentry::store::MemoryStore;

let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
let report = books.audit().await?;
assert_eq!(report.head, books.head().await?);
```

## Limits

- A **consistent rewrite of the whole log** passes: nothing inside the store
  contradicts it. [Witnessed checkpoints](@/docs/assurance/transparency.md)
  catch it.
- It detects tampering; it does not prevent it.
- It checks the books against the rules, not against the world: a correctly
  recorded wrong amount passes.
