+++
title = "PostgreSQL"
description = "Roles and privileges, creating the schema once, triggers, the advisory lock, TLS, and isolation."
weight = 2
+++

## What to call

- [`SqlStore::postgres`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.postgres)`(url)` /
  [`postgres_with`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.postgres_with)`(url, options)`;
  [`with_schema`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.with_schema) for many ledgers on one pool.
- Create once, privileged: [`Store::create`](https://docs.rs/doubleentry/latest/doubleentry/store/trait.Store.html#tymethod.create)
  with a [`Genesis`](https://docs.rs/doubleentry/latest/doubleentry/record/struct.Genesis.html).
- Open, as the service: [`Ledger::new`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.new),
  which runs no DDL when the ledger exists.
- Errors: [`Error::Config`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#variant.Config),
  [`StoreErrorKind::InvalidSchemaName`](https://docs.rs/doubleentry/latest/doubleentry/store/enum.StoreErrorKind.html#variant.InvalidSchemaName).

Everything here needs the `postgres` feature; the examples compile but are not
run (they need a server).

## Two roles

A ledger lives in one schema (`SqlOptions::schema`, default `doubleentry`).
Creating it is DDL: the schema, 23 tables, indexes, trigger functions and
triggers, then the genesis record and the `meta` row. Running it needs no DDL.

```rust,no_run
use doubleentry::record::Genesis;
use doubleentry::store::{Postgres, SqlOptions, SqlStore, Store};
use doubleentry::LedgerId;

// The privileged job, once per ledger (a repeat with the same genesis is a no-op).
let options = SqlOptions { schema: "ledger".into(), ..SqlOptions::default() };
let admin = SqlStore::<Postgres>::postgres_with("postgres://owner:secret@db/books", options).await?;
let info = admin.create(&Genesis { ledger: LedgerId::new("example.com/acme")? }).await?;
assert_eq!(info.size, 1);
admin.grant_runtime("app").await?; // the role must exist; one transaction
admin.close().await;
```

`create` serialises concurrent creators on an advisory lock. On a schema that
already holds this genesis it writes nothing; on one holding another ledger,
scale or schema identifier it is refused as `Error::Config`. A schema that
holds tables but no ledger — another application's, or a doubleentry 0.7
ledger — is refused as
`ConfigError::ForeignSchema`, naming the tables, and nothing is created in it.
`open` only reads the `meta` row and the genesis record.

`grant_runtime(role)` applies exactly the statements
`SqlStore::<Postgres>::runtime_grants(role)` returns — or, without a
connection, `Postgres::runtime_grants(schema, role)`, for a migration tool or a
DBA to review. They are generated from the schema's own table catalogue, so a
release that adds a table also adds its grant, and they name every table and
column explicitly: `GRANT … ON ALL TABLES` would only reach the tables that
exist when it runs. For schema `ledger` and role `app`:

<!-- runtime-grants -->
```sql
REVOKE ALL ON SCHEMA "ledger" FROM "app";
REVOKE ALL ON ALL TABLES IN SCHEMA "ledger" FROM "app";
GRANT USAGE ON SCHEMA "ledger" TO "app";
GRANT SELECT, INSERT ON "ledger".meta TO "app";
GRANT SELECT, INSERT ON "ledger".leaves TO "app";
GRANT SELECT, INSERT ON "ledger".nodes TO "app";
GRANT SELECT, INSERT ON "ledger".accounts TO "app";
GRANT SELECT, INSERT ON "ledger".account_versions TO "app";
GRANT SELECT, INSERT ON "ledger".currency_versions TO "app";
GRANT SELECT, INSERT ON "ledger".policy_versions TO "app";
GRANT SELECT, INSERT ON "ledger".periods TO "app";
GRANT SELECT, INSERT ON "ledger".postings TO "app";
GRANT SELECT, INSERT ON "ledger".posting_dims TO "app";
GRANT SELECT, INSERT ON "ledger".seals TO "app";
GRANT SELECT, INSERT ON "ledger".seal_leaves TO "app";
GRANT SELECT, INSERT ON "ledger".seal_nodes TO "app";
GRANT SELECT, INSERT ON "ledger".clearings TO "app";
GRANT SELECT, INSERT ON "ledger".clearing_resets TO "app";
GRANT SELECT, INSERT ON "ledger".clearing_refs TO "app";
GRANT SELECT, INSERT ON "ledger".balances TO "app";
GRANT SELECT, INSERT ON "ledger".cleared TO "app";
GRANT SELECT, INSERT ON "ledger".outstanding TO "app";
GRANT SELECT, INSERT ON "ledger".slices TO "app";
GRANT SELECT, INSERT ON "ledger".memos TO "app";
GRANT SELECT, INSERT ON "ledger".holds TO "app";
GRANT SELECT, INSERT ON "ledger".entries TO "app";
GRANT UPDATE ON "ledger".balances TO "app";
GRANT UPDATE ON "ledger".cleared TO "app";
GRANT UPDATE ON "ledger".outstanding TO "app";
GRANT UPDATE ON "ledger".slices TO "app";
GRANT UPDATE (salt, text) ON "ledger".memos TO "app";
GRANT UPDATE (resolved_by, how) ON "ledger".holds TO "app";
GRANT UPDATE (fingerprint) ON "ledger".entries TO "app";
```

The list starts by revoking everything, so applying it again leaves exactly
these. `UPDATE` covers the four maintained-value tables (caches `audit()`
recomputes) and three columns, each also limited by a trigger: a hold resolves
once, a memo is erased (`salt`, `text` to NULL), a fingerprint is nulled only
with its entry's memo. No `DELETE`, no `TRUNCATE`, no DDL, no ownership: the
role must not own the schema or its tables, because an owner bypasses grants
and can drop triggers. The crate's PostgreSQL tests run the conformance suite
as a role with only these grants, and check that leaving out any one of them
makes a write or a read fail.

```rust,no_run
use doubleentry::prelude::*;
use doubleentry::store::{Postgres, SqlOptions, SqlStore};

// The service, as `app`.
let options = SqlOptions { schema: "ledger".into(), max_connections: 20, ..SqlOptions::default() };
let store = SqlStore::<Postgres>::postgres_with("postgres://app:secret@db/books", options).await?;
let books = Ledger::new(store, LedgerId::new("example.com/acme")?).await?;
books
    .record(
        Entry::new("psp-0001", date!("2026-03-15"))
            .debit("Assets:Bank", amount!("25.00"), EUR)
            .credit("Liabilities:Wallets:alice", amount!("25.00"), EUR),
    )
    .await?;
```

## Triggers

Every table of the log and its indexes refuses `UPDATE` and `DELETE` by a row
trigger; the maintained tables refuse `DELETE`; `memos`, `holds` and `entries`
carry the column-scoped triggers above; every table refuses `TRUNCATE`. A
refusal raises SQLSTATE `P0001` with a message starting `doubleentry:`,
reported as `Error::Integrity`. What a trigger cannot refuse (an extra inserted
row, a wrong maintained value, an owner dropping triggers) `audit()` finds.

## Schema names

`SqlOptions::schema` must match `[A-Za-z_][A-Za-z0-9_]{0,62}`, or it is refused
with `ConfigError::InvalidSchemaName` (`StoreErrorKind::InvalidSchemaName` on
the store) before connecting. It is always quoted
(case is kept) and never put into the `search_path`. `with_schema` returns a
store for another schema over the same pool, without connecting:

```rust,no_run
use doubleentry::store::{Postgres, SqlStore};
use doubleentry::{Ledger, LedgerId};

let pool = SqlStore::<Postgres>::postgres("postgres://app:secret@db/books").await?;
for tenant in ["acme", "globex"] {
    let store = pool.with_schema(&format!("ledger_{tenant}"))?;
    let books = Ledger::new(store, LedgerId::new(format!("example.com/{tenant}"))?).await?;
    books.audit().await?;
}
```

## Locking and isolation

- **Writes**: `BEGIN ISOLATION LEVEL READ COMMITTED`, pinned per transaction
  whatever the server default, then `pg_advisory_xact_lock(key)`. The key is the
  first 8 bytes of a SHA-256 over the schema name and the ledger id, so ledgers
  never wait for each other. One writer per ledger, any number of instances.
- **Reads**: `REPEATABLE READ READ ONLY`, one snapshot, no lock.
- **Connections**: `max_connections` (default 10); each connection sets
  `plan_cache_mode = force_generic_plan`. A `BEGIN` that fails on a connection
  the server closed is retried on the next one. A failure while committing is
  `Error::Backend` with `may_have_applied()`: record the same draft again, and a
  committed entry comes back as a replay.

Text columns are `COLLATE "C"`, so paths and dimension values compare by bytes on
every locale. Amounts are 16-byte big-endian integers (`BYTEA`); SQL never
computes with money.

## Connections

The URL carries your settings: `application_name` (or `PGAPPNAME`) for
`pg_stat_activity`, and `sslmode`, `sslrootcert`, `sslcert`, `sslkey` for TLS.
The store owns its pool — `SqlOptions::max_connections`, shared by every
`with_schema` store — and joins no external transaction; to post together with
your own write, use an
[outbox](@/docs/concepts/idempotency.md#together-with-your-own-database-the-outbox).

## TLS

Enable `postgres-tls` (rustls on aws-lc-rs) and ask for TLS in the URL:
`postgres://app:secret@db/books?sslmode=verify-full&sslrootcert=/etc/ssl/db-ca.pem`.

## Limits

- The crate creates no roles; it runs GRANTs only when asked
  (`grant_runtime`).
- The schema owner or a superuser can drop the triggers: detect it with
  `audit()` and witnessed checkpoints.
- Connection poolers (PgBouncer and similar) are not tested.
