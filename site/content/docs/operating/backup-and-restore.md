+++
title = "Backup and restore"
description = "Back up with the database's own tools, then audit() the restored books before writing."
weight = 4
+++

## What to call

- After every restore, before the first write: [`Ledger::audit`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.audit)
  → [`AuditReport`](https://docs.rs/doubleentry/latest/doubleentry/struct.AuditReport.html), or
  [`Error::Integrity`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#variant.Integrity) at the first difference.
- Then compare [`Ledger::head`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.head)
  with the last head you published (a [`Verifier`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html) holds it).
- SQLite file copies: [`SqlStore::close`](https://docs.rs/doubleentry/latest/doubleentry/store/struct.SqlStore.html#method.close) first.

The crate has no backup function; the database's tools copy consistently.
Back up the **whole** database or schema, not only the log: the tables also
hold side data no hash covers (memo texts and salts, the draft fingerprints
idempotency compares) and the indexes and maintained values `audit()` checks.

## SQLite

The store runs in WAL mode, so committed transactions may still sit in the
`-wal` file.

- **Online**: `VACUUM INTO` (or the `sqlite3` shell's `.backup`) writes a
  consistent copy without blocking the writer.
- **Offline**: `SqlStore::close()` (or stop every process); the file alone is
  then complete.
- **Never** copy just the `.db` file of an open database.

```rust
use doubleentry::prelude::*;
use doubleentry::store::{SqlStore, Sqlite};
use sqlx::Connection;

let dir = tempfile::tempdir()?;
let (live, backup) = (dir.path().join("live.db"), dir.path().join("backup.db"));
let books = Ledger::new(SqlStore::<Sqlite>::sqlite(&format!("sqlite://{}", live.display())).await?, LedgerId::new("example.com/acme")?).await?;
books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
books.define_months(2026, 3..=3).await?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
books
    .record(
        Entry::new("sale-1", date!("2026-03-05"))
            .debit("Assets:Cash", amount!("10.00"), EUR)
            .credit("Income:Sales", amount!("10.00"), EUR),
    )
    .await?;

// Online backup from a connection of your own, while the ledger stays open.
let mut connection = sqlx::SqliteConnection::connect(&format!("sqlite://{}", live.display())).await?;
sqlx::query("VACUUM INTO $1").bind(backup.display().to_string()).execute(&mut connection).await?;

// Restore: open the copy, audit it, compare it with the head you published.
let restored = Ledger::new(SqlStore::<Sqlite>::sqlite(&format!("sqlite://{}", backup.display())).await?, LedgerId::new("example.com/acme")?).await?;
let report = restored.audit().await?;
assert_eq!(report.entries, 1);
assert_eq!(restored.head().await?, books.head().await?);
```

The `sqlx` lines need `sqlx` with its `sqlite` feature in your application.
[examples/restore_audit.rs](https://github.com/hupe1980/doubleentry/blob/main/examples/restore_audit.rs)
shows the offline path.

## PostgreSQL

- **Logical**: `pg_dump --schema=ledger_acme`, restored with `pg_restore` or
  `psql` under the **same schema name** (the trigger functions name their
  schema). The dump carries the grants; the roles must exist on the target.
- **Physical**: base backups with WAL archiving (`pg_basebackup`, or your
  provider's point-in-time recovery).

The triggers come back with the schema.

## Restore, then audit, then compare heads

`audit()` replays every record through the rule core and compares every table
the store keeps with the replay, byte for byte (see [Audit](@/docs/assurance/audit.md)).
It proves the copy is internally consistent; it cannot know about bookings the
backup never had. For that, compare the restored head with the **last head you
published**:

- equal: the restore is complete;
- smaller but consistent: the backup predates published bookings. Their keys
  are unknown again, so a redelivered message is recorded as new;
- not a prefix: this is not the published ledger.

A rollback is visible to anyone holding a later head: a
[`Verifier`](@/docs/assurance/transparency.md) refuses a smaller head, and a
witness cosigns only checkpoints that extend what it holds. Appending to a
rolled-back ledger forks the published history. So publish checkpoints only for
sizes you know are in a backup (with continuous WAL archiving, every committed
size is).

A journal rebuilt from the leaves alone (`Journal::from_log`) has no memo texts
and no fingerprints: a retry of an old key is then a `KeyConflict` instead of a
replay. Back up the database, not just the log.

## Limits

- The crate does not schedule, store or encrypt backups, and does not re-create
  lost bookings.
