+++
title = "With a database"
description = "The same calls on a Ledger over SQLite: create or open, retry safely, keep books durable."
weight = 2
+++

## What to call

[`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html) — the `Journal`'s
methods, `async` and on `&self`, over a store:
[`SqlStore`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html) with
`Sqlite` (feature `sqlite`) or `Postgres` (feature `postgres`), or
[`MemoryStore`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.MemoryStore.html).

```sh
cargo add doubleentry --features sqlite
cargo add tokio --features rt-multi-thread,macros
```

The `sqlite` feature bundles SQLite; nothing is installed on the machine.

## Create or open

`SqlStore::sqlite(url)` only connects (`sqlite://path/to/books.db` is created
if missing). `Ledger::new(store, id)` creates the ledger when the database holds
none and opens it otherwise; a database holding another ledger id, or this one
at another scale, is refused as `Error::Config` without reading or changing
anything. With idempotent setup, the first start and the hundredth run the same
code.

```rust
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};

let store = SqlStore::<Sqlite>::sqlite("sqlite::memory:").await?;
let books = Ledger::new(store.clone(), LedgerId::new("example.com/acme")?).await?;

for _start in 0..2 {
    // The setup a service runs on every start: the second run writes nothing.
    books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    books.define_months(2026, 1..=12).await?;
    books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
}
assert_eq!(books.head().await?.size, 16); // genesis, currency, 12 months, 2 accounts

let again = Ledger::new(store.clone(), LedgerId::new("example.com/acme")?).await?;
assert_eq!(again.head().await?, books.head().await?);
let other = Ledger::new(store, LedgerId::new("example.com/other")?).await;
assert!(matches!(other, Err(Error::Config(_))));
```

A `Ledger` is a cheap clone sharing its store; hand one to every task.

## Record, in batches, and retry

The store runs the rule core **inside its write transaction**, under the
ledger's single-writer lock, so the state an entry is checked against is the
state it is appended to. `record_batch` writes many drafts in one transaction:
all or nothing.

A backend failure says whether a retry may succeed
([`is_transient`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#method.is_transient))
and whether the write may have committed although it failed
([`may_have_applied`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#method.may_have_applied)).
Either way, record **the same draft** again: a committed entry comes back with
`replayed == true`, an uncommitted one is written once.

```rust
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};

async fn book(books: &Ledger<SqlStore<Sqlite>>, entry: Entry) -> Result<Recorded> {
    let mut attempt = 0;
    loop {
        match books.record(entry.clone()).await {
            Err(e) if (e.is_transient() || e.may_have_applied()) && attempt < 3 => attempt += 1,
            outcome => return outcome,
        }
    }
}

let store = SqlStore::<Sqlite>::sqlite("sqlite::memory:").await?;
let books = Ledger::new(store, LedgerId::new("example.com/shop")?).await?;
books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
books.define_months(2026, 3..=3).await?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;

let sale = Entry::new("psp-0001", date!("2026-03-15"))
    .debit("Assets:Bank", amount!("25.00"), EUR)
    .credit("Income:Sales", amount!("25.00"), EUR);
assert!(!book(&books, sale.clone()).await?.replayed);
assert!(book(&books, sale).await?.replayed); // delivered twice, booked once

let day: Vec<Entry> = (2..=4)
    .map(|n| {
        Entry::new(format!("psp-000{n}"), date!("2026-03-16"))
            .debit("Assets:Bank", amount!("10.00"), EUR)
            .credit("Income:Sales", amount!("10.00"), EUR)
    })
    .collect();
assert_eq!(books.record_batch(day).await?.len(), 3);

let bank = books.balance("Assets:Bank", EUR, &BalanceQuery::all()).await?;
assert_eq!(bank.net(), amount!("55.00"));
books.audit().await?; // replay the log and compare every table
```

The full service is
[examples/sqlite_service.rs](https://github.com/hupe1980/doubleentry/blob/main/examples/sqlite_service.rs).

## Without an async runtime of your own

The SQL stores run on tokio (through sqlx). A synchronous program builds a
runtime and hands each call to `block_on`.

```rust
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};

let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
let store = runtime.block_on(SqlStore::<Sqlite>::sqlite("sqlite::memory:"))?;
let books = runtime.block_on(Ledger::new(store, LedgerId::new("example.com/cli")?))?;
runtime.block_on(books.declare_currency(EUR, 2, date!("2026-01-01")))?;
assert_eq!(runtime.block_on(books.exponent(EUR))?, Some(2));
```

## PostgreSQL and memory

`SqlStore<Postgres>` (feature `postgres`) keeps one ledger per PostgreSQL
schema — `doubleentry` by default, another through `SqlOptions` or
`with_schema`. TLS is the `postgres-tls` feature; see
[Features and MSRV](@/docs/start/features.md#tls-for-postgresql). Roles and
privileges are on [PostgreSQL](@/docs/operating/postgresql.md).

```rust,no_run
use doubleentry::prelude::*;
use doubleentry::store::{Postgres, SqlStore};

let store = SqlStore::<Postgres>::postgres("postgres://app:secret@db.internal/books").await?;
let books = Ledger::new(store, LedgerId::new("example.com/acme")?).await?;
books.audit().await?;
```

`MemoryStore` is the same store contract in memory, for tests and embedded
programs; it persists nothing. `Ledger::to_journal()` replays a store's log into
a `Journal`.

## Limits

- No migrations: a database of another schema version is refused when opened
  ([Stores](@/docs/operating/stores.md)).
- One writer per ledger: writes to one ledger are serialised. Reads do not wait
  for a writer.
- Durability is the database's
  ([Backup and restore](@/docs/operating/backup-and-restore.md)).
