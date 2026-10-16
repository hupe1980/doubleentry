+++
title = "Corrections"
description = "Nothing is edited: a correction is a reversal that mirrors the wrong entry exactly, booked in an open period."
weight = 7
+++

## What to call

[`reversal(id, key, on)`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.reversal)
builds the draft that reverses entry `id`; record it with `record`.
[`Entry::reverses(target)`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.reverses)
marks a hand-built draft as a reversal.

## Nothing is edited

An entry, once recorded, is never changed or deleted; the API has no call for
either. A wrong booking is undone by a second entry, the **reversal**, and the
right one is booked by a third. All three stay in the books and can be proven.

`reversal` returns a draft with the target's postings in the same order — same
accounts, amounts, currencies, layers and dimensions — with every side swapped.
Building it checks nothing; recording it checks everything.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Expenses:Rent", AccountKind::Expense, date!("2026-01-01"))?;
books.open("Expenses:Office", AccountKind::Expense, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;

let wrong = books.record(
    Entry::new("bill-77", date!("2026-03-15"))
        .debit("Expenses:Rent", amount!("420.00"), EUR)
        .credit("Assets:Cash", amount!("420.00"), EUR),
)?;

let half = Entry::new("bill-77-half", date!("2026-03-16"))
    .reverses(wrong.id)
    .debit("Assets:Cash", amount!("210.00"), EUR)
    .credit("Expenses:Rent", amount!("210.00"), EUR);
assert_eq!(books.record(half).unwrap_err().code(), Some(403)); // not a mirror

books.record(books.reversal(wrong.id, "bill-77-rev", date!("2026-03-16"))?)?;
books.record(
    Entry::new("bill-77-fix", date!("2026-03-16"))
        .debit("Expenses:Office", amount!("420.00"), EUR)
        .credit("Assets:Cash", amount!("420.00"), EUR),
)?;
let rent = books.balance("Expenses:Rent", EUR, &BalanceQuery::all())?;
assert_eq!(rent.to_string(), "dr 420.00 cr 420.00 net 0.00 (2 postings)");

let twice = books.reversal(wrong.id, "bill-77-rev-2", date!("2026-03-17"))?;
assert_eq!(books.record(twice).unwrap_err().code(), Some(401)); // AlreadyReversed
```

## The rules

A reversal is checked in the references stage, in this order:

| Code | Rule |
|---|---|
| 400 | The target exists. |
| 450 | The target neither opens nor resolves a [hold](@/docs/concepts/holds.md). |
| 401 | The target has not been reversed: at most one reversal per entry. |
| 402 | The reversal is not dated before its target. |
| 403 | The reversal **mirrors** the target: posting *i* is the target's posting *i* with its side swapped. |

An entry cannot both reverse and act on a hold (144). Otherwise a reversal is an
ordinary entry — open period, open accounts, limits — and can itself be
reversed. A partial or reordered hand-built reversal is refused with 403,
naming the first posting that differs.

## Correcting a sealed period

Nothing can be booked into a sealed period (301). A mistake found later is
reversed **in an open period**; the original stays where it was, and each seal
proves what the books said at the time.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Expenses:Rent", AccountKind::Expense, date!("2026-01-01"))?;
books.define_months(2026, 3..=4)?;
let wrong = books.record(
    Entry::new("bill-77", date!("2026-03-15"))
        .debit("Expenses:Rent", amount!("420.00"), EUR)
        .credit("Assets:Cash", amount!("420.00"), EUR),
)?;
books.seal_period("2026-03")?;

let into_march = books.reversal(wrong.id, "bill-77-rev", date!("2026-03-31"))?;
assert_eq!(books.record(into_march).unwrap_err().code(), Some(301));
books.record(books.reversal(wrong.id, "bill-77-rev", date!("2026-04-01"))?)?;

assert_eq!(books.balance("Expenses:Rent", EUR, &BalanceQuery::period("2026-03"))?.net(), amount!("420.00"));
assert!(books.balance("Expenses:Rent", EUR, &BalanceQuery::all())?.net().is_zero());
```

The full program is
[`examples/reversal.rs`](https://github.com/hupe1980/doubleentry/blob/main/examples/reversal.rs).

## Why

A reversal is a fact of its own, with its own key, date and proof. The exact
mirror rule means an entry is never marked reversed while its amounts do not net
to zero, and an auditor can check the rule without guessing.

## Limits

- No partial reversal: correct part of an amount with an ordinary adjusting
  entry.
- `reversal` copies the postings, not the memo, kind, documents, value date or
  rates; set those on the returned draft.
- A hold or a hold resolution cannot be reversed (450): void the hold, or
  correct a capture with a new entry.
