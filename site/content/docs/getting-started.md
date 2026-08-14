+++
title = "Getting started"
description = "Install doubleentry, register accounts, record a balanced entry, and read a balance and a proof back out."
weight = 1
+++

## Install

```toml
[dependencies]
doubleentry = "0.3"
time = { version = "0.3", features = ["macros"] }
```

Nothing else is required. The core engine performs no I/O, spawns no tasks, and
pulls in no async runtime. Storage backends are opt-in
[features](@/docs/features.md).

## The journal

A `Journal` is one entity's books: its accounts, its calendar, its policy, its
entries, the Merkle log that commits to them, its seals and its clearings. It
holds all of that together because every piece is needed to decide whether the
next booking is legal, and splitting them across your own variables is how they
drift apart.

```rust
use doubleentry::period::LedgerId;
use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal};
use time::macros::date;

type Eur = Amount<2>;

let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
```

The `2` is the ledger's **scale** — the number of decimal places every amount
carries, fixed at compile time. See [Money](@/docs/money.md) for how to choose
it.

## Accounts

Accounts are paths in a hierarchy you define. The engine imposes no chart of
accounts: it does not know what `Assets` means, does not require that name, and
validates against no national or corporate scheme.

```rust
let cash    = journal.accounts_mut().register_path("Assets:Cash",  date!(2026-01-01))?;
let revenue = journal.accounts_mut().register_path("Income:Sales", date!(2026-01-01))?;
```

Registration returns an `AccountId` — a dense handle, cheap to compare and to
index by. Two structural rules *are* enforced, because no downstream layer can
repair them:

- **Only leaves are postable.** An account with children is an aggregation path.
  Posting to both a node and its descendants makes every rollup double-count.
- **Postings fall inside the account's open window.** An account has an opening
  date and an optional closing date.

## Recording an entry

```rust
let recorded = journal.record(
    Entry::new(
        EntryId::generate(),
        IdempotencyKey::new(b"invoice-2026-0001".to_vec())?,
        date!(2026-03-15),
    )
    .debit(cash,     Eur::parse("1190.00")?, Currency::EUR)
    .credit(revenue, Eur::parse("1190.00")?, Currency::EUR),
)?;

assert!(recorded.is_new);
```

`record` validates the draft against *this* journal's accounts, calendar and
policy, then appends it. There is no separate context to build and no second
object to keep in step — the things validation consults and the thing that stores
the result are the same thing.

The `IdempotencyKey` is what makes a retry safe across an at-least-once delivery
path. See [Idempotency](@/docs/idempotency.md).

## Reading it back

```rust
use doubleentry::{BalanceKey, Layer};

let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };
let balance = journal.balance(&key, None)?;

assert_eq!(balance.debits, Eur::parse("1190.00")?);
```

Balances carry gross debit *and* credit totals rather than only the net — see
[Debits and credits](@/docs/debits-and-credits.md) for why that distinction is
not optional.

## Proving it

```rust
let head  = journal.head();
let proof = journal.prove_inclusion(recorded.require_index()?)?;

assert!(proof.verify(&recorded.content_hash, &head.root));
```

That is the whole loop: record, read, prove. Everything else on this site is
detail on one of those three.

## Where to go next

- [Entries and validation](@/docs/entries.md) — what "balanced by construction"
  actually claims, and what it does not.
- [Proofs](@/docs/proofs.md) — inclusion and consistency, and why the log is a
  Merkle tree rather than a hash chain.
- [Persistence](@/docs/persistence.md) — when the books have to survive the
  process.
