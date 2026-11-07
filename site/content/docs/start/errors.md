+++
title = "Errors"
description = "The one Error type, its nine variants, stable rejection codes, and what to retry."
weight = 3
+++

## What to call

Every operation returns [`doubleentry::Result`](https://docs.rs/doubleentry/latest/doubleentry/type.Result.html)
with one [`Error`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html).
`Error::code` and `Error::rejection` reach the
[`Rejection`](https://docs.rs/doubleentry/latest/doubleentry/enum.Rejection.html) inside it;
`is_transient` and `may_have_applied` say what to retry. Payload types are in
[`doubleentry::error`](https://docs.rs/doubleentry/latest/doubleentry/error/index.html).

## Nine variants

| Variant | Means |
|---|---|
| `Rejected(Rejection)` | the ledger refused the input — any write |
| `BatchRejected { index, rejection }` | item `index` of a batch (`batch`, `record_batch`, `clear_batch`) was refused; nothing of the batch was written |
| `Value(ValueError)` | a value constructor (`LedgerId::new`, `Label::new`, …) refused its input |
| `Amount(AmountError)` | money parsing or arithmetic failed |
| `Query(QueryError)` | a read asked an impossible question: an unknown account, entry or period, a position past the head |
| `Transparency(…)` | a checkpoint, cosignature, policy, proof or bundle was refused |
| `Integrity(IntegrityError)` | the ledger's stored data fails verification |
| `Backend(BackendError)` | the database failed, or the OS had no randomness for a memo salt |
| `Config(ConfigError)` | the store holds another ledger, scale or schema, or tables that are no ledger (`ForeignSchema`); a schema or role name is not an identifier |

`Value` and `Amount` never come from a ledger operation: draft builders cannot
fail, so a malformed path or amount inside a draft is `Rejected` with a code.

```rust
use doubleentry::prelude::*;
use doubleentry::error::{QueryError, ValueError};

assert!(matches!(LedgerId::new("has space").map_err(Error::from), Err(Error::Value(ValueError::InvalidCharacter { .. }))));
assert!(matches!(Amount::<2>::parse("1.001").map_err(Error::from), Err(Error::Amount(_))));

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;

let bad_path = books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets::Cash", amount!("1.00"), EUR)
        .credit("Income:Sales", amount!("1.00"), EUR),
);
assert_eq!(bad_path.err().and_then(|e| e.code()), Some(103)); // InvalidPath

let read = books.balance("Assets:Bank", EUR, &BalanceQuery::all());
assert!(matches!(read, Err(Error::Query(QueryError::UnknownAccount { .. }))));
```

## Stable rejection codes

Every `Rejection` has a numeric `code()`. Codes are append-only: a code keeps
its meaning, and a retired code is never reused. The hundreds name the
[stage](@/docs/concepts/validation.md) that refused the input; the stages and
the checks within them run in a fixed order, so an input that breaks several
rules always gets the same code, from `Journal`, `Ledger` and every store.

| Codes | Stage | Checks |
|---|---|---|
| 100–124, 140–145 | structural | the input alone; the shape of a hold or resolution; a batch of more than 1 000 items (124) |
| 200 | idempotency | a key already used for other content |
| 300–311 | context | periods, accounts, currencies, policy, watermark on the booking date |
| 400–431, 440–450 | references | reversal targets, cleared postings, master data, the hold a resolution names |
| 500–501 | limits | overflow and balance limits |

The full list is on
[`Rejection`](https://docs.rs/doubleentry/latest/doubleentry/enum.Rejection.html).
`Display` and `{:?}` both print one line with the code first —
`rejected: [106] EUR Settled does not balance: debits 10.00, credits 9.00` —
so a `main` returning `doubleentry::Result<()>` reports the code; `{:#?}` shows
the structure.

## What to retry

`is_transient()` (a retry may succeed) and `may_have_applied()` (the write may
have committed although it failed) are only ever `true` for `Backend`.

| Error | What the caller does |
|---|---|
| `Rejected`, `BatchRejected` | fix the input (or drop item `index`); never retry unchanged |
| `Value`, `Amount`, `Query` | fix the call |
| `Transparency` | the artefact is forged, stale or foreign; never retry it unchanged |
| `Integrity(e)` | stop writing; investigate `e.position()`; restore and `audit` |
| `Backend`, transient or outcome unknown | retry with the **same** draft: it applies once or replays |
| `Backend`, permanent | an operator problem: configuration, permissions |
| `Config` | fix the URL, schema name, ledger id or `P`; for `ForeignSchema`, give the ledger an empty schema (or file) of its own; the data is fine |

```rust
use doubleentry::prelude::*;
use doubleentry::error::BackendError;

let lost = Error::from(BackendError::transient(std::io::Error::other("connection reset")).outcome_unknown());
assert!(lost.is_transient() && lost.may_have_applied());
assert_eq!(lost.code(), None);
```

## In an HTTP handler

Map the variants once, at the edge. Key on the code, not the `Display` text.

```rust
use doubleentry::prelude::*;

fn respond(outcome: Result<Recorded>) -> (u16, Option<u16>) {
    match outcome {
        Ok(r) if r.replayed => (200, None), // a retry: the original receipt
        Ok(_) => (201, None),
        Err(Error::Rejected(r @ Rejection::KeyConflict { .. })) => (409, Some(r.code())),
        Err(Error::Rejected(r)) => (422, Some(r.code())),
        Err(e) if e.is_transient() || e.may_have_applied() => (503, None),
        Err(_) => (500, None), // Integrity, Backend, Config: page an operator
    }
}

let mut books = Journal::new(LedgerId::new("example.com/shop")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let order = |key: &str, on, eur| {
    Entry::new(key, on)
        .debit("Assets:Cash", eur, EUR)
        .credit("Income:Sales", eur, EUR)
};

assert_eq!(respond(books.record(order("order-17", date!("2026-03-15"), amount!("20.00")))).0, 201);
assert_eq!(respond(books.record(order("order-17", date!("2026-03-15"), amount!("20.00")))).0, 200);
assert_eq!(respond(books.record(order("order-17", date!("2026-03-15"), amount!("21.00")))), (409, Some(200)));
assert_eq!(respond(books.record(order("order-18", date!("2026-04-02"), amount!("5.00")))), (422, Some(300)));
```

## Limits

- `Display` texts are for people and may be reworded; the codes and variants
  are the contract.
- `is_transient()` is the store's judgement of one failure, not a promise.
  Bound the retries.
- `Rejection` is `#[non_exhaustive]`: a `match` needs a catch-all arm.
- `BackendError` is opaque: the driver error is reachable through `source()`,
  but no `sqlx` type is part of the API.
