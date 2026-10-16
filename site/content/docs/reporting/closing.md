+++
title = "Year-end closing"
description = "close_year: closing entries per account and dimension slice, returned as drafts for you to record."
weight = 6
+++

## What to call

- [`close_year`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.close_year)`(key, on, equity)`
  → `Vec<`[`Entry`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html)`>`: the closing drafts; it writes nothing.
- Record them with [`record_batch`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.record_batch),
  then seal with [`seal_through`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.seal_through).
- The same on [`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.close_year), `async`.

## Closing the income statement

`close_year` brings every income and expense account to zero into an equity
account and hands back ordinary drafts:

- **What closes**: every (income or expense account, currency, dimension set)
  with a non-zero **Settled** net over the postings dated on or before `on`. Each
  slice gets one posting that flattens it, with the slice's dimensions.
- **Into what**: per (currency, dimension set), one posting on `equity` with the
  same dimensions. The equity account must be a leaf and not itself an income or
  expense account.
- **Shape**: dated `on`, keyed `{key}/1`, `{key}/2`, …, packed to the posting
  limit (1 024) and the leaf limit (65 535 bytes); a group that does not fit is
  split, with an equity leg in each part, so every draft balances.
- **What stays**: the Pending layer; open holds carry over.

The drafts go through the rule core like any entry: the period containing `on`
must be open, and a required-dimension policy is met because each leg keeps its
slice's dimensions. History is read through `on`, not a fiscal-year window:
earlier years are zero because their own closing zeroed them.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 1..=12)?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.open("Expenses:Rent", AccountKind::Expense, date!("2026-01-01"))?;
books.open("Equity:Retained", AccountKind::Equity, date!("2026-01-01"))?;
books.record(
    Entry::new("sale-1", date!("2026-03-10"))
        .debit("Assets:Bank", amount!("1000.00"), EUR)
        .credit("Income:Sales", amount!("1000.00"), EUR),
)?;
books.record(
    Entry::new("rent-1", date!("2026-07-01"))
        .debit("Expenses:Rent", amount!("300.00"), EUR)
        .credit("Assets:Bank", amount!("300.00"), EUR),
)?;

let drafts = books.close_year("close-2026", date!("2026-12-31"), "Equity:Retained")?;
assert_eq!(drafts[0].key().map(|key| key.as_str()), Some("close-2026/1"));
books.record_batch(drafts)?;

let all = BalanceQuery::all();
assert!(books.balance("Income:Sales", EUR, &all)?.net().is_zero());
assert!(books.balance("Expenses:Rent", EUR, &all)?.net().is_zero());
assert_eq!(books.balance("Equity:Retained", EUR, &all)?.net(), amount!("-700.00")); // a 700.00 profit
assert!(books.close_year("close-2026", date!("2026-12-31"), "Equity:Retained")?.is_empty());

books.seal_through("2026-12")?; // nothing can be booked into 2026 any more
```

`record_batch` is one transaction, and an identical batch sent again replays.
If a `Ledger` write fails with [`may_have_applied`](@/docs/start/errors.md),
call `close_year` again with the same arguments: an empty result means the batch
was recorded; otherwise record the same drafts again.

`close_year` returns `Error::Rejected` with 100 when `{key}/N` is not a valid
key (the suffix counts towards the 128-byte limit), 302 when the equity account
is unknown, 303 when it is not a leaf, 429 (`ClosingEquityInScope`) when it is
an income or expense account, 430 (`ClosingAccountNotOpen`) when an account to
close is not open on `on`.

## Limits

- No carry-forward of balance-sheet accounts: assets, liabilities and equity
  keep their balances in a continuous ledger.
- No fiscal-year object: the periods you define and seal are the calendar.
- The whole result goes to one equity account per slice; distribute it with
  ordinary entries.
- `close_year` seals nothing; [sealing](@/docs/concepts/periods-and-seals.md)
  is a separate step.
