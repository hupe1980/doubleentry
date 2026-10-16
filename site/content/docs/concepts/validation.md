+++
title = "Validation"
description = "The rule core's five stages, their precedence, and the one Rejection every façade and store returns."
weight = 5
+++

## What to call

`record` validates and writes in one call; a refusal is
[`Error::Rejected`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#variant.Rejected)
with a [`Rejection`](https://docs.rs/doubleentry/latest/doubleentry/enum.Rejection.html)
(`code()`, `stage()`). [`validate`](https://docs.rs/doubleentry/latest/doubleentry/fn.validate.html)
lists a draft's structural violations without reading the ledger.

## One rule core, five stages

Every rule lives in one pure function, the *rule core*: it takes the input and
a read-only view of the ledger at the position the record would be appended,
and returns the records to write or exactly one `Rejection`. It reads no clock,
draws no randomness and does no I/O; no façade or store has a rule of its own.

| # | Stage | Checks | Codes |
|---|---|---|---|
| 1 | Structural | the input alone: key, postings, positive amounts, balance per (currency, layer), labels, paths, memo, sizes, hold shape | 100–145 |
| 2 | Idempotency | a used key: same content replays, other content conflicts | 200 |
| 3 | Context | the state on the booking date: an unsealed period; accounts that exist, are leaves and open; declared currencies; policy; the master-data watermark | 300–311 |
| 4 | References | the records the input names: reversal targets, holds, cleared postings, master data, the calendar | 400–450 |
| 5 | Limits | gross overflow, then balance limits | 500–501 |

A stage runs only when every earlier one has passed, and the checks within a
stage run in a fixed order, so an input that breaks several rules always gets
the same code. `record` returns the first; `validate` returns every structural
one, in that order.

```rust
use doubleentry::prelude::*;
use doubleentry::{error::Stage, validate};

// An empty key, one posting, and therefore unbalanced.
let draft: Entry = Entry::new("", date!("2026-03-15")).debit("Assets:Cash", amount!("1.00"), EUR);
let found: Vec<u16> = validate(&draft).iter().map(Rejection::code).collect();
assert_eq!(found, [100, 101, 106]);
assert!(validate(&draft).iter().all(|r| r.stage() == Stage::Structural));

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
assert_eq!(books.record(draft).unwrap_err().code(), Some(100));
```

## Precedence in practice

- **Malformed is malformed everywhere.** An unbalanced draft in a sealed period
  is refused as unbalanced (106), not as sealed (301).
- **A retry always resolves.** Idempotency runs before every state rule, so a
  retry of a recorded entry replays even after its period was sealed.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 1..=2)?;
let sale = |key: &str, credit| {
    Entry::new(key, date!("2026-01-15"))
        .debit("Assets:Cash", amount!("10.00"), EUR)
        .credit("Income:Sales", credit, EUR)
};
let first = books.record(sale("sale-1", amount!("10.00")))?;
books.seal_period("2026-01")?;

assert_eq!(books.record(sale("sale-2", amount!("9.00"))).unwrap_err().code(), Some(106));
let again = books.record(sale("sale-1", amount!("10.00")))?; // a retry in the sealed month
assert!(again.replayed && again.position == first.position);
assert_eq!(books.record(sale("sale-3", amount!("10.00"))).unwrap_err().code(), Some(301));
```

## The same rejection everywhere

`Journal`, `Ledger` and every store return the **same** `Rejection` for the same
input, field for field; the conformance suite
([`testing`](https://docs.rs/doubleentry/latest/doubleentry/testing/index.html))
checks every code against each store.

`Ledger::record` hands the draft to the store, which opens its write
transaction, takes the ledger's lock, runs the core and writes before it
commits. The state a draft is checked against is the state it is appended to,
and the public API has no pre-validated entry that could be written later.

## Master data cannot reach back

A master-data change that takes effect on or before the last sealed day is
refused (309). A policy must also take effect after the latest booking date
already recorded (431), so it never governs an entry checked under another.

```rust
use doubleentry::prelude::*;
use doubleentry::LedgerPolicy;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("controller");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("10.00"), EUR)
        .credit("Income:Sales", amount!("10.00"), EUR),
)?;
let policy = LedgerPolicy::new([Label::new("costcenter")?], None)?;
assert_eq!(books.set_policy(policy.clone(), date!("2026-03-15")).unwrap_err().code(), Some(431));
books.set_policy(policy, date!("2026-03-16"))?;
```

## Why

One pure core with one ordered list of stages gives one answer, testable once
and checkable against every store. Replays resolve before any business rule
(TigerBeetle's result order), so a client retrying after a timeout never gets a
refusal for a write that succeeded. Running the core inside the write
transaction leaves no gap between check and append.

## Limits

- One rejection per write: fix it, and the next attempt may stop at a later
  stage.
- A rejection is final for that input and state; only `Error::Backend` may
  succeed on retry ([Errors](@/docs/start/errors.md)).
