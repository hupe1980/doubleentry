+++
title = "Export"
description = "Flat, verifiable rows for an auditor: Konto/Gegenkonto pairing, minor units, and a rebuildable root."
weight = 7
+++

## What to call

- [`Journal::export`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.export)`(scope, head)`: an iterator of rows;
  [`Ledger::export`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.export)`(scope, head)`:
  [`ExportPages`](https://docs.rs/doubleentry/latest/doubleentry/export/struct.ExportPages.html), read with
  [`next_page`](https://docs.rs/doubleentry/latest/doubleentry/export/struct.ExportPages.html#method.next_page).
- [`ExportScope`](https://docs.rs/doubleentry/latest/doubleentry/export/enum.ExportScope.html): `All`,
  [`ExportScope::period`](https://docs.rs/doubleentry/latest/doubleentry/export/enum.ExportScope.html#method.period), `Positions`.
- [`ExportRow`](https://docs.rs/doubleentry/latest/doubleentry/export/enum.ExportRow.html): leaf, entry, posting, pair,
  document, rate, account, currency, seal and seal-total rows.
- For the recipient: [`export::verify`](https://docs.rs/doubleentry/latest/doubleentry/export/fn.verify.html)`(rows, &head)`
  ([`verify_with_precision`](https://docs.rs/doubleentry/latest/doubleentry/export/fn.verify_with_precision.html) for `P ≠ 2`),
  [`rebuild_entry`](https://docs.rs/doubleentry/latest/doubleentry/export/fn.rebuild_entry.html),
  [`pairs`](https://docs.rs/doubleentry/latest/doubleentry/export/fn.pairs.html).

## The books as rows

An export is one pass over the log, in position order, up to a tree head (the
current one or one you pin, such as a signed checkpoint):

- a **leaf row** per record: position, kind, leaf hash;
- an **entry row** per entry: id, key, dates, capture time, kind, period, memo
  commitment (and text and salt while not erased), reversal and hold fields;
- per entry, its **posting rows** (account handle and path, kind and class in
  force on the booking date, side, amount, currency, layer, dimensions),
  **pair rows** (Konto/Gegenkonto), **document rows** and **rate rows**;
- an **account row** per account master-data record and a **currency row** per
  declaration;
- a **seal row** per seal and a **seal-total row** per (currency, layer):
  balance-tree root, totals, posting count.

Genesis, policy, period and clearing records have their leaf row only.
**Amounts are integers**: minor units at the currency's exponent, with the
exponent beside them (`119.00 EUR` is `11900`, exponent `2`); never a formatted
decimal or a float.

```rust
use doubleentry::export::{ExportRow, ExportScope, verify};
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.open("Liabilities:VAT", AccountKind::Liability, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.record(
    Entry::new("INV-1", date!("2026-03-15"))
        .document(Document::new("invoice", "INV-1")?)
        .debit("Assets:Receivables", amount!("119.00"), EUR)
        .credit("Income:Sales", amount!("100.00"), EUR)
        .credit("Liabilities:VAT", amount!("19.00"), EUR),
)?;

let head = books.head(); // or the head of a signed checkpoint
let rows: Vec<ExportRow> = books.export(ExportScope::period("2026-03"), Some(head))?.collect::<Result<_>>()?;

let postings: Vec<(String, i128, u8)> = rows
    .iter()
    .filter_map(|row| match row {
        ExportRow::Posting(p) => Some((p.path.to_string(), p.minor_units, p.exponent)),
        _ => None,
    })
    .collect();
assert_eq!(postings[0], ("Assets:Receivables".to_owned(), 11_900, 2));

let pairs: Vec<(String, String, i128)> = rows
    .iter()
    .filter_map(|row| match row {
        ExportRow::Pair(p) => Some((p.debit_path.to_string(), p.credit_path.to_string(), p.minor_units)),
        _ => None,
    })
    .collect();
assert_eq!(pairs, [
    ("Assets:Receivables".to_owned(), "Income:Sales".to_owned(), 10_000),
    ("Assets:Receivables".to_owned(), "Liabilities:VAT".to_owned(), 1_900),
]);

verify(rows, &head)?; // what the recipient runs
```

## Konto / Gegenkonto

German booking formats want every line as an amount from one account to one
counter-account. The export derives pairs deterministically per (currency,
layer):

1. each debit, in posting order, pairs with the first unused credit of the
   identical amount;
2. the rest are merged greedily: the first open debit with the first open
   credit, for the smaller remaining amount.

Each posting's pairs sum exactly to its amount; no pair joins two currencies or
layers. `pairs` is a pure function, so the recipient recomputes it.

## Scope and paging

`ExportScope::period(id)` exports the entries booked in that period and its seal;
`Positions(range)` those at the positions. **Leaf, account and currency rows are
always complete up to the head**, so a scoped export verifies like a full one.
An unknown period is `Error::Query` (`UnknownPeriod`).

On a `Ledger`, `next_page(limit)` reads 1–1 000 rows per page (the limit is
clamped), each in one snapshot. Pinned to a head, the export ignores later
writes; the page size changes where pages break, never the rows.

```rust
use doubleentry::export::{ExportScope, verify};
use doubleentry::prelude::*;
use doubleentry::store::MemoryStore;

let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
books.define_months(2026, 3..=3).await?;
let sale = |key: &str| {
    Entry::new(key, date!("2026-03-15"))
        .debit("Assets:Bank", amount!("10.00"), EUR)
        .credit("Income:Sales", amount!("10.00"), EUR)
};
books.record(sale("sale-1").memo("cash sale")).await?;
let head = books.head().await?;
books.record(sale("sale-2")).await?; // after the pin: not in the export

let mut export = books.export(ExportScope::All, Some(head)).await?;
let mut rows = Vec::new();
while let Some(page) = export.next_page(500).await? {
    rows.extend(page);
}
verify(rows, &head)?;
```

`verify(rows, &head)` trusts nothing the exporter computed:

- the leaf rows rebuild the head's size and root (`RootMismatch`);
- every entry rebuilds from its rows to its leaf bytes and hash (`EntryMismatch`);
- every memo present re-commits to its commitment (`MemoMismatch`);
- every entry's pair rows are exactly the pairing of its postings (`PairMismatch`);
- the rows are in order with matching counts (`Malformed`).

A recipient holding a [signed checkpoint](@/docs/assurance/transparency.md)
verifies its signature first and passes its head here.

## Data access (§ 147 Abs. 6 AO)

The export is the crate's side of *Datenüberlassung*: every booking with its
documents, rates, master data in force and seals, as rows that rebuild the
published hashes. The file format (DATEV EXTF, a GDPdU `index.xml` with its
tables, SAF-T) is the integrator's adapter: each needs settings the crate does
not have, such as SKR03/SKR04 account numbers and the client header. The
[`export` module](https://docs.rs/doubleentry/latest/doubleentry/export/index.html)
rustdoc maps every field class of these formats to a row field or an adapter
setting. See also [GoBD](@/docs/assurance/gobd.md).

## Limits

- No file is written: no CSV, XML, DATEV or SAF-T.
- Account, currency and seal rows describe their records but are not re-hashed
  by `verify`; policy, period and clearing records are leaf hashes only.
- An erased memo is gone from the export too; its commitment stays.
- Period balances are not rows: run `trial_balance` or `statement`.
