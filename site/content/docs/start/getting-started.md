+++
title = "Getting started"
description = "Install the crate, set up a Journal, record a balanced entry, read a balance, prove the entry, seal and audit."
weight = 1
+++

## What to call

[`Journal`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html) — a ledger in memory,
written synchronously. Set up with `declare_currency`, `open` and `define_period` (or `define_months`),
book with [`Entry`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html) and `record`,
read with `balance`, prove with `prove_entry` against `head()`, close with `seal_period`, check with
`audit`. All of it comes in with
[`doubleentry::prelude`](https://docs.rs/doubleentry/latest/doubleentry/prelude/index.html).

```sh
cargo add doubleentry
```

No feature is on by default, and the `Journal` needs none
([Features and MSRV](@/docs/start/features.md)).

## Books from zero

Before the first booking a ledger needs a **currency** with its exponent
(EUR 2, JPY 0), **accounts** by path, each with a kind, and a **period** the
booking date falls into. Each is a record in the log, signed by the actor set
with `as_actor` (`system` by default).

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))?;

let sale = Entry::new("invoice-2026-0001", date!("2026-03-15"))
    .debit("Assets:Cash", amount!("1190.00"), EUR)
    .credit("Income:Sales", amount!("1190.00"), EUR);
let rec = books.record(sale.clone())?;
assert_eq!(rec.position, 5); // genesis, currency, two accounts, period, then the entry

let retry = books.record(sale)?; // a redelivered message: nothing is written
assert!(retry.replayed && retry.id == rec.id);
```

`date!` and `amount!` are checked at compile time: `date!("2026-02-30")` and
`amount!("1.001")` (three decimals at scale 2) do not compile. `Journal::new`
has two decimal places; other scales are on [Money](@/docs/concepts/money.md).

Every setup call is idempotent — the same definition again writes nothing, a
different one is refused — so a service can run its setup on every start. The
idempotency key (`"invoice-2026-0001"`) is the entry's identity: the same key
with other content is refused with code 200
([Idempotency](@/docs/concepts/idempotency.md)).

## Read, prove, seal, audit

`balance` returns the gross [`Balance`](@/docs/concepts/debits-and-credits.md)
— debits and credits apart, `net()` = debit − credit. `prove_entry` is an
inclusion proof against `head()`; a verifier needs only the head, the proof and
the leaf bytes. `seal_period` commits every account's balance through the
period's end and closes it to bookings. `audit()` replays the log through the
rule core and compares the result with the journal's state.

```rust
use doubleentry::prelude::*;
use doubleentry::record::Record;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let rec = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;

let sales = books.balance("Income:Sales", EUR, &BalanceQuery::all())?;
assert_eq!(sales.to_string(), "dr 0.00 cr 1190.00 net -1190.00 (1 posting)");

// What a third party receives: the head, the proof and the leaf bytes.
let head = books.head();
let proof = books.prove_entry(rec.id)?;
let leaf = books.leaves().nth(usize::try_from(rec.position)?).ok_or("no leaf")?;
assert!(matches!(proof.verify_record::<2>(&head, leaf.bytes)?, Record::Entry(_)));

books.seal_period("2026-03")?;
let late = books.record(
    Entry::new("invoice-2026-0002", date!("2026-03-20"))
        .debit("Assets:Cash", amount!("10.00"), EUR)
        .credit("Income:Sales", amount!("10.00"), EUR),
);
assert_eq!(late.err().and_then(|e| e.code()), Some(301)); // PeriodSealed

let report = books.audit()?;
assert_eq!((report.entries, report.seals, report.accounts), (1, 1, 2));
```

A refusal is `Error::Rejected` with a stable code
([Errors](@/docs/start/errors.md)) and leaves the journal unchanged. Full
programs are in
[`examples/`](https://github.com/hupe1980/doubleentry/tree/main/examples), for
instance [month_close](https://github.com/hupe1980/doubleentry/blob/main/examples/month_close.rs).
Next: the same calls over SQLite, [With a database](@/docs/start/with-a-database.md).

## Why

- **Setup is data in the log.** The master data an entry was checked against is
  itself tamper-evident and provable.
- **Builders never fail.** A malformed path, label or amount comes back from
  `record` as a coded `Rejection`, so validation has one home.

## Limits

- A `Journal` lives in memory; durable books need a `Ledger` over a store.
- There is no chart of accounts: paths and kinds are yours.
- A proof shows a record is in the log the head describes, not that everyone
  was shown that head — that is [transparency](@/docs/assurance/transparency.md).
