+++
title = "Design"
description = "The principles behind the crate, and what is deliberately not in it."
weight = 7
+++

## What to call

- [`Journal`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html) and [`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html): two façades over one rule core.
- [`store::Store`](https://docs.rs/doubleentry/latest/doubleentry/store/trait.Store.html): the one backend contract.
- [`Error`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html) and [`Rejection`](https://docs.rs/doubleentry/latest/doubleentry/enum.Rejection.html): one error type, stable rejection codes.
- [`record::Record`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.Record.html): the records of the one log.

## In one paragraph

A ledger is one entity's books, kept as **one append-only RFC 6962 SHA-256
Merkle log of typed records**. Every rule is decided by **one pure function**,
the rule core: a draft and a read-only view of the state in, the records to
append or a typed `Rejection` out. `Journal` (sync, in memory) and `Ledger<S>`
(async, over a `Store`, validating inside the store's write transaction) both
call it. A seal commits per (currency, layer) to an account-indexed tree of
gross balances, so one balance can be proven to a third party. The log head is
published as a C2SP signed checkpoint that independent witnesses cosign, and
`audit()` replays the log through the rule core.

## Principles

1. **A calculation library with one rule core.** All rules live in one pure
   core: no clock, no randomness, no hash-map order, no I/O, no `async`, no
   `unsafe`; `just purity` enforces it over the core, the model, the log,
   reporting, transparency and export. Entry ids derive from your idempotency
   key; every date and timestamp is an argument.
2. **Exact, total money.** `Amount<P>` is an `i128` scaled by 10^P (`P` at most
   18, default 2), with a symmetric range. No floats, no arithmetic operators:
   every fallible operation returns a typed error. Excess precision is refused,
   never rounded. Balances are gross (debit, credit) pairs.
3. **The write is the authority.** State changes only by appending a record
   through the rule core inside the store's write transaction, under the
   ledger's single-writer lock. A period is sealed only by a seal record.
4. **One log, standard proofs.** One canonical encoding, one RFC 6962 tree,
   proofs against a tree head, vacuous proofs refused. Checkpoints and witness
   messages are C2SP formats, tested against the Go implementations.
5. **One contract, every store.** The conformance suite decides whether a store
   keeps the `Store` contract; database tests use real databases. A database
   records its schema identifier, and a mismatch is refused.
6. **Developer experience.** One error type, one prelude, the same methods on
   `Journal` and `Ledger`, accounts by path, compile-checked money literals, no
   driver types in the public API, every documented code block compiled.

## Not in the crate

These are decisions:

- **FX conversion and revaluation.** Rates are data on the entry; what to
  convert and how (IAS 21, § 256a HGB) is yours.
- **Conditional writes.** A write is decided by the rules and the state.
- **Pruning.** Books are kept for 8 to 10 years; the crate deletes nothing.
- **Migrations.** A database of another schema is refused, not converted.
- **Hash agility.** SHA-256 and RFC 6962 only.
- **HTTP and a witness.** The transparency code is sans-I/O, and a witness is
  worth having only if somebody else runs it.
- **A query language, change-data capture, metadata maps.** Reads are typed
  queries; dimensions are the extension point for postings.
- **Lots and cost basis.** Inventory and securities accounting sit above the
  ledger.
- **A chart of accounts.** Paths, kinds and classes are yours.
