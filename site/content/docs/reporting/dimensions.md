+++
title = "Dimensions"
description = "Caller-named reporting axes on postings, required dimensions, and dimension-filtered reads."
weight = 4
+++

## What to call

- On a posting: [`Posting::dimension`](https://docs.rs/doubleentry/latest/doubleentry/struct.Posting.html#method.dimension)
  or [`Posting::dimensions`](https://docs.rs/doubleentry/latest/doubleentry/struct.Posting.html#method.dimensions)
  with a [`Dimensions`](https://docs.rs/doubleentry/latest/doubleentry/struct.Dimensions.html) set, added with
  [`Entry::posting`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.posting).
- Required axes: a [`LedgerPolicy`](https://docs.rs/doubleentry/latest/doubleentry/struct.LedgerPolicy.html)
  recorded with [`set_policy`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.set_policy).
- Reading a slice: [`BalanceQuery::matching`](https://docs.rs/doubleentry/latest/doubleentry/struct.BalanceQuery.html#method.matching)
  and [`missing`](https://docs.rs/doubleentry/latest/doubleentry/struct.BalanceQuery.html#method.missing), on any read
  that takes a query.

## Axes are yours

A dimension is an `axis → value` pair on one posting (cost centre, project,
tax key); the crate names no axis. A posting carries at most eight
(`Dimensions::MAX_AXES`), one value per axis, in ascending axis order. Axis and
value are labels: 1–64 characters, normalised to NFC, no edge whitespace, no
control or invisible characters. A bad
label, a second value for an axis or a ninth axis is refused by `record` as
`InvalidDimensions` (105).

Dimensions are part of the entry's leaf: proofs, seals and the export carry
them, and they cannot change after recording. A wrong one is corrected by a
[reversal](@/docs/concepts/corrections.md) and a new entry.

## Required dimensions

The ledger policy names the axes every posting must carry, both legs included.
It is master data with an effective date; an entry is checked against the
policy in force on its booking date, and a posting without a required axis is
`MissingDimension` (307). A policy must take effect after the latest booking
date already recorded (`PolicyBeforeBookings`, 431). The policy also sets the
largest allowed distance between value and booking date (`ValueDateDrift`, 308).

```rust
use doubleentry::prelude::*;
use doubleentry::{LedgerPolicy, Posting};

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Expenses:Travel", AccountKind::Expense, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.set_policy(LedgerPolicy::new([Label::new("cost-center")?], None)?, date!("2026-01-01"))?;

let untagged = books.record(
    Entry::new("trip-1", date!("2026-03-10"))
        .debit("Expenses:Travel", amount!("80.00"), EUR)
        .credit("Assets:Bank", amount!("80.00"), EUR),
);
assert_eq!(untagged.err().and_then(|e| e.code()), Some(307)); // MissingDimension

let twice = books.record(
    Entry::new("trip-1", date!("2026-03-10"))
        .posting(Posting::debit("Expenses:Travel", amount!("80.00"), EUR).dimension("cost-center", "100").dimension("cost-center", "200"))
        .posting(Posting::credit("Assets:Bank", amount!("80.00"), EUR).dimension("cost-center", "100")),
);
assert_eq!(twice.err().and_then(|e| e.code()), Some(105)); // InvalidDimensions

books.record(
    Entry::new("trip-1", date!("2026-03-10"))
        .posting(Posting::debit("Expenses:Travel", amount!("80.00"), EUR).dimension("cost-center", "100"))
        .posting(Posting::credit("Assets:Bank", amount!("80.00"), EUR).dimension("cost-center", "100")),
)?;
```

## Reading a slice

`matching(axis, value)` keeps postings whose axis has that value;
`missing(axis)` keeps postings without it. Clauses are conjunctive. A name that
is not a label is `Error::Query` (`InvalidLabel`). A filter can split an entry,
so a filtered trial balance is a **slice**: `is_filtered()` says so, and
`is_balanced()` may be false. Tag both legs when a slice should balance.

```rust
use doubleentry::prelude::*;
use doubleentry::Posting;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Expenses:Travel", AccountKind::Expense, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
for (key, project, eur) in [("t-1", "alpha", amount!("80.00")), ("t-2", "beta", amount!("20.00"))] {
    books.record(
        Entry::new(key, date!("2026-03-10"))
            .posting(Posting::debit("Expenses:Travel", eur, EUR).dimension("project", project))
            .credit("Assets:Bank", eur, EUR),
    )?;
}

let alpha = BalanceQuery::period("2026-03").matching("project", "alpha");
assert_eq!(books.balance("Expenses:Travel", EUR, &alpha)?.net(), amount!("80.00"));
let slice = books.trial_balance(&alpha)?;
assert!(slice.is_filtered() && !slice.is_balanced()); // the bank legs carry no project
let untagged = books.trial_balance(&BalanceQuery::period("2026-03").missing("project"))?;
assert_eq!(untagged.rows().len(), 1); // Assets:Bank only
```

Every store returns the same slice. Paths and dimension values compare **by
bytes** everywhere (PostgreSQL text columns are `COLLATE "C"`), so ordering
does not depend on the database locale. [`close_year`](@/docs/reporting/closing.md)
closes each dimension slice on its own.

## Limits

- No hierarchy of values, no allocation, no check of a value against a list:
  validate values before recording.
- A dimension value is in the hash forever: no personal data (see
  [Privacy](@/docs/operating/privacy.md)).
