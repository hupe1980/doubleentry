+++
title = "Periods and seals"
description = "A contiguous calendar of periods, seals that commit every balance through a period's end, and the watermark below which nothing changes."
weight = 9
+++

## What to call

`define_period(id, start, end)` and `define_months(year, months)` build the
calendar; `seal_period(id)` and `seal_through(id)` seal it, returning a
[`SealReceipt`](https://docs.rs/doubleentry/latest/doubleentry/struct.SealReceipt.html) with the
[`Seal`](https://docs.rs/doubleentry/latest/doubleentry/record/struct.Seal.html) record;
`prove_balance` proves one sealed balance ([Proofs](@/docs/assurance/proofs.md)).
`periods()`, `seals()` and `watermark()` read the calendar back — each also
`_at(head)`, as it was at a checkpoint.

## A contiguous calendar

A ledger books only into **defined periods**, each a record with an id, a first
and a last day. The first may start on any day; every later one starts **the
day after the last one ends** — no gaps, no overlaps. Periods may have any
length. Defining a period again identically writes nothing;
`define_months(2026, 1..=12)` defines `2026-01` … `2026-12` and skips months
already defined.

| Code | Refused because |
|---|---|
| 121 | the period ends before it starts |
| 425 | the id is already defined with other dates |
| 426 | the period does not start the day after the last one ends |
| 300 | an entry's date lies in no defined period |
| 301 | an entry's date lies in a sealed period |

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.define_months(2026, 1..=3)?;
books.define_months(2026, 1..=3)?; // already defined: writes nothing

let gap = books.define_period("2026-Q2", date!("2026-04-02"), date!("2026-06-30"));
assert_eq!(gap.unwrap_err().code(), Some(426));
books.define_period("2026-Q2", date!("2026-04-01"), date!("2026-06-30"))?;
```

## Sealing

A period is **open** until a seal record for it exists, and **sealed** from then
on, permanently. Seals follow the calendar: `seal_period(id)` is refused while
an earlier period is open (427); `seal_through(id)` seals every open period up
to `id`, earliest first. Each seal is its own write, so a failed
`seal_through` keeps the seals before it and a second call continues. Sealing a
sealed period returns its receipt and writes nothing.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 1..=12)?;
let sale = books.record(
    Entry::new("sale-1", date!("2026-01-10"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;

assert_eq!(books.seal_period("2026-03").unwrap_err().code(), Some(427));
let receipts = books.seal_through("2026-03")?;
let sealed: Vec<String> = receipts.iter().map(|r| r.period.to_string()).collect();
assert_eq!(sealed, ["2026-01", "2026-02", "2026-03"]);

let january = &receipts[0].seal;
assert_eq!((january.parts().through, january.parts().accounts), (sale.position, 2));
let eur = january.tree(EUR, Layer::Settled).ok_or("no EUR tree")?;
assert_eq!(eur.totals.debit(), amount!("1190.00"));
assert!(january.tree(EUR, Layer::Pending).is_none()); // no holds: zero everywhere
```

## What a seal commits to

The seal is a log record (kind `0x08`), provable like any entry.

| Field | Meaning |
|---|---|
| period, start, end | the sealed period |
| `through` | the position of the last record the seal covers |
| `accounts` | the registered accounts at `through`: the size of every balance tree |
| `trees` | per (currency, layer) with postings: a balance-tree root and its totals |

Leaf *i* of a balance tree is account handle *i*'s cumulative **gross** balance
through the period's end — debit, credit, posting count — or the zero leaf.
Postings in later periods are not counted. Indexing by handle binds every row to
its account: no account has two rows, and "no balance" is provable. A
(currency, layer) without a tree is zero for every account. Totals are checked
sums of the leaves and never decrease from one seal to the next.

From a seal, `prove_balance(account, currency, layer, period)` returns the leaf,
the seal's inclusion in the log and the inclusion of the account's register
record; a third party verifies it against a tree head alone.

## The watermark

The **watermark** is the last day of the latest sealed period. Entries dated on
or before it are refused (301), master data taking effect on or before it is
refused (309), and a policy must also take effect after the latest booking date
(431). A sealed period's figures are never recomputed under other master data.
Corrections go into a later open period
([Corrections](@/docs/concepts/corrections.md)).

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Liabilities:Wallet", AccountKind::Liability, date!("2026-01-01"))?;
books.define_months(2026, 3..=4)?;
books.seal_period("2026-03")?;

let back = books.set_limit("Liabilities:Wallet", BalanceLimit::NoDebitBalance, date!("2026-03-31"));
assert_eq!(back.unwrap_err().code(), Some(309));
books.set_limit("Liabilities:Wallet", BalanceLimit::NoDebitBalance, date!("2026-04-01"))?;
```

## Reading the calendar

A service shows which periods are locked (festgeschrieben) and refuses
back-dated input before it reaches the ledger. `watermark()` is the one
"closed through" date; `periods()` lists every period with its dates and, once
sealed, its `SealReceipt`; `seals()` is the sealed ones, each with its first
and last day (`start()`, `end()`), its position and the last record it covers
(`through()`). Ids are whatever the calendar was defined with — a quarter, a
13th period — so nothing is parsed from them.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.define_period("FY2026-Q1", date!("2026-01-01"), date!("2026-03-31"))?;
books.define_period("FY2026-Q2", date!("2026-04-01"), date!("2026-06-30"))?;
assert_eq!(books.watermark()?, None);
let before = books.head();
books.seal_period("FY2026-Q1")?;

assert_eq!(books.watermark()?, Some(date!("2026-03-31")));
let locked: Vec<String> = books.periods()?.iter().filter(|p| p.is_sealed()).map(|p| p.id.to_string()).collect();
assert_eq!(locked, ["FY2026-Q1"]);
let seal = &books.seals()?[0];
assert_eq!((seal.start(), seal.end()), (date!("2026-01-01"), date!("2026-03-31")));
assert_eq!(books.watermark_at(before)?, None); // as an auditor's checkpoint saw it
```

## Why

The calendar is a fold of period records, so it cannot have gaps; "sealed"
means a seal record exists, so the state cannot exist without the record. Seals
in calendar order make the watermark one date, checked in one comparison.

## Limits

- A seal is a record, not a signature: third parties trust it through the
  signed, witnessed tree head that includes it
  ([Transparency](@/docs/assurance/transparency.md)).
- There is no unseal.
- Sealing books no year-end close; that is
  [`close_year`](@/docs/reporting/closing.md), recorded as entries before the
  seal.
