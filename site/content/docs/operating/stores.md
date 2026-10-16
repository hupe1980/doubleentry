+++
title = "Stores"
description = "MemoryStore, SQLite and PostgreSQL: one contract, one writer per ledger, reads that never block, a schema identifier checked on open."
weight = 1
+++

## What to call

- [`Ledger::new`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.new)`(store, ledger_id)` —
  creates the ledger when the store holds none, opens it otherwise.
- [`MemoryStore`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.MemoryStore.html);
  [`SqlStore::sqlite`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.sqlite) (feature `sqlite`),
  [`SqlStore::postgres`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.postgres) (feature `postgres`),
  their `_with` forms taking [`SqlOptions`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlOptions.html),
  [`close`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.close).
- The contract: [`Store`](https://docs.rs/doubleentry/latest/doubleentry/store/trait.Store.html);
  its conformance suite: [`testing::conformance`](https://docs.rs/doubleentry/latest/doubleentry/testing/fn.conformance.html) (feature `testing`).
- The schema check: [`SCHEMA_ID`](https://docs.rs/doubleentry/latest/doubleentry/store/constant.SCHEMA_ID.html),
  [`ConfigError`](https://docs.rs/doubleentry/latest/doubleentry/error/enum.ConfigError.html).

```rust
use doubleentry::prelude::*;
use doubleentry::store::{SqlOptions, SqlStore, Sqlite};
use std::time::Duration;

let dir = tempfile::tempdir()?;
let url = format!("sqlite://{}", dir.path().join("books.db").display());
let options = SqlOptions { busy_timeout: Duration::from_secs(2), ..SqlOptions::default() };
let store = SqlStore::<Sqlite>::sqlite_with(&url, options).await?;
let books = Ledger::new(store.clone(), LedgerId::new("example.com/acme")?).await?; // create or open
books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
store.close().await;

let reopened = Ledger::new(SqlStore::<Sqlite>::sqlite(&url).await?, LedgerId::new("example.com/acme")?).await?;
assert!(reopened.account("Assets:Cash").await?.is_some());
```

## Choosing a store

| | `MemoryStore` | `SqlStore<Sqlite>` | `SqlStore<Postgres>` |
|---|---|---|---|
| Durable | no | yes, one file | yes |
| Ledgers | one per store value | one per file | one per schema; one pool serves many |
| Writers | one at a time (a lock) | one at a time (`BEGIN IMMEDIATE`) | one at a time per ledger (advisory lock) |
| Readers | never wait | never wait (WAL) | never wait (`REPEATABLE READ READ ONLY`) |
| Immutability | in-process only | triggers | triggers, incl. `TRUNCATE` |

Every store answers every operation with the same result and the same
`Rejection` as the in-memory `Journal`; the conformance suite asserts it field
for field. Switching stores changes the constructor, not the code.
`Ledger::to_journal` turns durable books into an in-memory replica.

The constructors only connect; nothing is read or created until `Ledger::new`.
A `SqlStore` is a cheap, cloneable handle around one pool, and so is a `Ledger`.

- **SQLite** runs every connection with `journal_mode=WAL`, `synchronous=FULL`,
  `foreign_keys=ON` and `recursive_triggers=ON` (a connection without the last
  two is refused). Writers queue on the file lock for up to
  `SqlOptions::busy_timeout` (5 s by default), then fail as `Error::Backend` with
  `is_transient()`. `sqlite::memory:` uses one connection.
- **PostgreSQL**: one ledger per schema (`SqlOptions::schema`, default
  `doubleentry`); see [PostgreSQL](@/docs/operating/postgresql.md).

## One writer per ledger

`record` runs one transaction: take the ledger's single-writer lock, read the
facts the rule core asks for, run the core, persist, commit. Validation happens
**inside** the writing transaction, against state no other writer can change.
Positions are assigned there, so log order is commit order. Readers take no lock:
each read runs in one snapshot. `record_batch` amortises the per-write round
trips.

## The `Store` contract

`Store<const P: u8 = 2>` has eighteen async primitives: `create`, `open`,
`write`, `size`, `read_leaves`, `read_nodes`, `seals`, `accounts`,
`account_records`, `entry`, `holds`, `clearings`, `projection`, `seal_nodes`,
`balances`, `postings`, `memo`, `erase_memo`. Proofs, statements, reports, export
and audit are built once over these by the façade; a store decides nothing.

The methods return `impl Future + Send`, so the trait is not object safe and
`Ledger<S>` is generic. To pick a store at run time, wrap the candidates in an
enum and implement `Store` by delegation; the
[`Store`](https://docs.rs/doubleentry/latest/doubleentry/store/trait.Store.html)
rustdoc has the complete enum.

A store of your own proves the contract with the public suite: an
exact-rejection table per code, a seeded simulation with retries, and concurrent
appends, batches, draws and erasures, each on a fresh store from your factory.
A failure names the check, seed and step (`DOUBLEENTRY_SEED=… DOUBLEENTRY_CHECK=…`).

```rust
use doubleentry::store::MemoryStore;
use doubleentry::testing::{Config, conformance_with};

// Your store: `conformance(|| async { MyStore::fresh().await })` runs everything.
let config = Config { check: Some(String::from("tables/2")), ..Config::default() };
let report = conformance_with(|| async { MemoryStore::new() }, config).await?;
assert!(!report.checks.is_empty());
```

## Schema identifier, no migrations

A SQL store's `meta` table holds one row: the schema identifier
`doubleentry/1` (`SCHEMA_ID`), the ledger id, the scale `P` and the genesis
leaf hash. `open` reads it before anything else:

| Found | Result |
|---|---|
| no ledger | `NotCreated`; `Ledger::new` then creates it |
| another schema identifier | `Error::Config(SchemaMismatch { found })`; nothing is read or changed |
| another ledger id, or another scale `P` | `Error::Config(LedgerMismatch { .. })` |
| a genesis record that does not hash to the `meta` row's | `Error::Integrity` (`GenesisMismatch`) |

Every write re-checks the row. There are no migrations: a database of another
schema is refused, never converted. Books move to a new schema by recording
them from their source documents into a new ledger with the same idempotency
keys; keep the old database, and the crate version that wrote it, for its
retention period.

```rust
use doubleentry::error::ConfigError;
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};

let dir = tempfile::tempdir()?;
let url = format!("sqlite://{}", dir.path().join("books.db").display());
Ledger::new(SqlStore::<Sqlite>::sqlite(&url).await?, LedgerId::new("example.com/acme")?).await?;

let other = Ledger::new(SqlStore::<Sqlite>::sqlite(&url).await?, LedgerId::new("example.com/other")?).await;
assert!(matches!(other, Err(Error::Config(ConfigError::LedgerMismatch { .. }))));
let finer = Ledger::<_, 4>::with_precision(SqlStore::<Sqlite, 4>::sqlite(&url).await?, LedgerId::new("example.com/acme")?).await;
assert!(matches!(finer, Err(Error::Config(_))));
assert_eq!(doubleentry::store::SCHEMA_ID, "doubleentry/1");
```

## Integrity

Every stored record a read hands out is verified first: the entry id must
re-derive from its key, the leaf bytes must hash to the stored leaf hash. A
mismatch is `Error::Integrity`, never a wrong answer.

The databases refuse, by trigger, `UPDATE` and `DELETE` on the log and its
indexes. The exceptions are column-scoped: memo erasure (with its fingerprint)
and a hold's single resolution. PostgreSQL also refuses `TRUNCATE`; SQLite
refuses a `REPLACE`. Maintained values (balances, open-hold totals, dimension
slices, applied clearings) are updated by the append and cannot be deleted;
[`audit()`](@/docs/assurance/audit.md) recomputes them and compares every other
table too.

No store can **prevent** the database owner from dropping a trigger. It makes
the edit **detectable**: the row fails verification or `audit()`, and a
rewritten history no longer matches a [witnessed](@/docs/assurance/transparency.md)
checkpoint.

## Tracing

With the `tracing` feature, the façades open one span per operation (target
`doubleentry`): `record`, `record_batch`, `seal_period`, `audit`, `checkpoint` at
`INFO`, `prove` at `DEBUG`. Fields: the ledger id, the first 64 bytes of the key
(or the period id), `outcome` (`new`, `replay`, `ok`, `rejected`, `failed`),
the rejection `code`, the `position` (for a batch, the failing `index`). Stores
add a `DEBUG` span per write and the SQL store one per read. Spans never carry an
amount, a memo, an account path or a document reference; the crate emits no
events.

## Limits

- `MemoryStore` is neither durable nor shared between processes.
- No store replicates, shards or fails over: that is the database's job.
