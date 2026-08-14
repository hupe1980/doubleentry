+++
title = "Persistence"
description = "The LedgerStore contract, the executable conformance suite that defines it, and the SQLite and PostgreSQL backends."
weight = 13
+++

## The contract

The engine keeps no storage of its own. `LedgerStore` defines what a backend must
do — and the [conformance suite](#conformance) is what decides whether an
implementation of it is correct.

A backend must guarantee all of the following:

1. **Append-only.** Recorded entries are never modified or removed.
2. **Atomic batches.** Every entry in a batch lands, or none does.
3. **Idempotent.** Re-appending an entry whose key is present with identical
   content is a no-op returning the original outcome; the same key with different
   content is an error, never an overwrite. The uniqueness check must be part of
   the write itself — a read-then-write races under concurrency.
4. **Dense, ordered indices.** Log indices start at zero and increase by one per
   entry, with no gaps, in commit order.
5. **Stable reads.** A record read twice returns the same bytes and the same
   content hash — every field, including `kind` and each posting's dimensions,
   because those are covered by that hash.
6. **Master data survives a restart.** An account comes back at the handle it was
   issued; a period comes back in the state it was left in.
7. **Seals chain.** `seals()` returns them in chain order, and what comes back
   reproduces a chain that verifies.
8. **Seals bind their account handles.** A seal's `accounts_root` is the
   commitment over the bindings the store itself holds.

## Why a conformance suite ships with the library {#conformance}

A ledger's guarantees are only as good as its weakest backend. The ways a backend
can be subtly wrong — a read-then-write idempotency check that races, a batch
that half-lands, an index sequence with a gap — are exactly the ways that produce
no error and no symptom until an audit.

Publishing the trait alone would leave every implementor to guess what
"idempotent" or "atomic" means here. The suite makes the contract executable: a
backend either passes it or is not a backend.

```rust
use doubleentry::storage::{MemoryStore, conformance};

let store  = MemoryStore::<2>::new(LedgerId::new("my-ledger")?);
let report = conformance::block_on(conformance::check_all(&store));
report.assert_passed();
```

Nineteen checks, each building its own accounts and entries, so a backend needs
to provide nothing but an empty store.

## Backends

| Backend | Feature | Verified against |
|---|---|---|
| In-memory | always on | the conformance suite |
| SQLite | `sqlite` | a real database, in-process — no server, no container |
| PostgreSQL | `postgres` | a real database, via testcontainers |

All of them run the same suite — PostgreSQL runs it twice, once per sequencing
mode — and a test asserts they **agree**: same log indices, same content hashes,
same tree root, same trial balance for the same operations. An abstraction that
only one implementation satisfies is not an abstraction.

Writing the second backend paid for itself immediately. The PostgreSQL store had
been ordering the seal chain by `sealed_at, period_id`, which is neither chain
order nor reliable: two seals in the same clock tick order arbitrarily, and the
fallback is lexical. SQLite has no sub-second timestamp ordering at all, which
forced the question and produced the right answer for both — an explicit
`chain_position`.

### Choosing between them

SQLite suits embedded and single-process deployments and needs no server.

PostgreSQL additionally enforces the balance invariant *in the database* via a
deferred constraint trigger, enforces period non-overlap with an `EXCLUDE`
constraint, and supports revoking `UPDATE` and `DELETE` — so where the ledger
must be defended against processes other than this one, it is the stronger
choice. Immutability enforced only in application code is a convention;
enforced by `GRANT` it is a property.

Both reference schemas state which guarantees they can and cannot carry.

## One ledger per database

A ledger is the isolation boundary: its own log, its own dense index space, its
own Merkle tree, its own seal chain, its own accounts.

That is deliberately stronger than a filter column. A seal commits to one
entity's history, so a shared log would have each tenant's seal committing to
every other tenant's entries — and an inclusion proof shown to one auditor would
reveal how many entries the others hold. Filtering by a column cannot fix that,
and it fails open: one query missing its predicate is a silent leak. Separation
here is physical, so there is no predicate to forget.

`migrate` records which ledger a database holds and refuses to open one belonging
to a different ledger.

### Sharing a database with your application

One ledger per database does not mean a database with nothing else in it. On
PostgreSQL the ledger's tables live in their own schema — `doubleentry` — leaving
`public` to you:

```rust
let store = PostgresStore::<2>::connect(&url, LedgerId::new("acme-gmbh")?).await?;
store.migrate().await?;
```

`connect` sets `search_path` so unqualified names resolve to the ledger's schema.
This matters because `accounts` is a name many applications have already spent on
something else, and a ledger that squats on it in `public` cannot be adopted
without a rename.

The schema is a **default, not a policy** — pass your own, including `public`
when the database belongs to the ledger alone:

```rust
let store = PostgresStore::<2>::connect_with(&url, ledger, "public").await?;
```

Isolation is only real if it is in effect, so `migrate` **verifies** it rather
than assuming it: if unqualified names would resolve somewhere else it returns
`WrongSearchPath` instead of quietly creating a second set of tables.

SQLite needs none of this — one ledger per file already is the isolation.

## Sequencing

Assigning log positions inline means serialising appends: the next index cannot
be read until the previous writer has committed. Assigning them out of band lets
writers insert concurrently and leaves ordering to a single sequencer — at the
cost of a window in which an entry is durable but not yet provable.

Both are legitimate, and the contract covers both. `Recorded::index` is an
`Option`, and that is not an implementation leak: it is the difference between
*recorded* and *committed to*. An entry with no index is safe from loss but
cannot yet be proven to sit anywhere in particular.

In deferred mode the sequencer must advance on a **commit-order watermark**,
never on a high-water mark over an unordered column:

```sql
SELECT ... FROM entries
WHERE log_index IS NULL
  AND insert_xid < pg_snapshot_xmin(pg_current_snapshot())
ORDER BY insert_xid, entry_id
```

The predicate admits only rows whose inserting transaction has finished. A row
still in flight is left for the next pass rather than skipped — which is the
whole point, since a skipped row would appear *behind* the reader once it
committed and would never be picked up again.

That watermark is cluster-wide: a transaction left open anywhere in the instance
holds it back. The behaviour is safe rather than lossy — the sequencer declines
to place what it cannot yet prove is settled — but sequencing latency is bounded
by the longest open transaction in the cluster, which is worth monitoring.

## Two details worth knowing

**Account handles are ledger state.** A handle is the account's position in
registration order, and it is written into every posting row and into every
trial-balance leaf a seal commits to. Rebuild a registry in a different order and
you repoint history. So restore from stored records rather than re-registering
paths:

```rust
let registry = AccountRegistry::from_records(store.accounts().await?)?;
```

`AccountRegistry::commitment()` is a cheap way to assert a locally built registry
matches the stored one — and it is the same value a seal's `accounts_root`
carries.

**SQLite foreign keys belong to the pool.** `PRAGMA foreign_keys` is *per
connection* and defaults to `OFF`, so a store that set it during migration would
configure one pooled connection and leave the rest ignoring every `REFERENCES`
clause. `migrate` therefore **verifies** it and refuses a pool that does not
enforce it. `sqlx` enables it by default, so an ordinary pool already passes.

## The calendar lives in the store

Periods are store state, not caller state. A sealed period held only in memory
comes back **open** after a restart and starts accepting postings into books that
have already been committed to.

So `define_period`, `transition_period`, `periods` and `seal_period` are all on
the trait, and sealing reads the period's state from storage rather than from an
argument.

```rust
store.define_period(&Period::new(march.clone(), start, end)?).await?;
store.transition_period(&march, PeriodState::Closing).await?;
let seal = store.seal_period(&march).await?;
```

Re-defining an identical period is a no-op, so you may declare your calendar on
every start-up. Re-defining the same identifier over a *different* range is an
error: that moves the boundary of a period entries have already been booked into.

## Reads are paged, not streamed

A cursor maps onto `WHERE index > ? ORDER BY index LIMIT ?` in any SQL backend,
survives a dropped connection, and needs no async-iteration machinery — so the
crate stays free of a futures dependency and a backend stays free of an executor
choice.

```rust
let mut cursor = Some(Cursor::start());
while let Some(c) = cursor {
    let page = store.page(c).await?;
    for record in &page.records { /* … */ }
    cursor = page.next;
}
```

## Static and dynamic dispatch

`LedgerStore` uses `async fn` in trait, which compiles to static dispatch with no
per-call allocation but is not `dyn`-compatible. Where a backend must be chosen at
run time, `DynLedgerStore` boxes the futures and restores object safety. A blanket
implementation covers every `LedgerStore`, so any backend works either way
without extra code.
