+++
title = "Persistence"
description = "The LedgerStore contract, the executable conformance suite that defines it, and the SQLite and PostgreSQL backends."
weight = 15
+++

## The contract

The engine keeps no storage of its own. `LedgerStore` defines what a backend must
do — and the [conformance suite](#conformance) is what decides whether an
implementation of it is correct.

A backend must guarantee all of the following:

1. **Append-only.** Recorded entries are never modified, and the store never
   removes one of its own accord. An operator may still prune a prefix archived
   to the [cold tier](@/docs/cold-tier.md) — see there for what that costs.
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
6. **Master data survives a restart, and can change.** An account comes back at
   the handle it was issued; a period comes back in the state it was left in.
   Re-registering a handle updates its classification, open window and balance
   limit, and refuses a different path — a store that could only ever *insert*
   could not close an account, and a rebound handle would repoint history.
7. **Balance limits are enforced in the write path.** A checked-then-written
   limit races: two appends that each fit the balance they read can together
   breach it. See [Accounts](@/docs/accounts.md#balance-limits).
8. **A posted-to leaf cannot gain a child.** Only leaves are postable, so
   `register_account` must refuse a path beneath an account that already carries
   postings. Otherwise the parent becomes an aggregation node and every later
   entry on it is refused — including the reversal that would correct it —
   leaving a balance that can never be moved. See
   [Accounts](@/docs/accounts.md#the-two-rules-it-does-enforce).
9. **Seals chain, and the chain describes the store's own log.** `seals()`
   returns them in chain order, and what comes back reproduces a chain that
   verifies — against the store's *own* ledger, so the first seal is checked as
   strictly as the last. Stronger than it sounds: each seal carries a consistency
   proof from its predecessor's tree, and the suite additionally rebuilds the log
   from `page()` and checks every seal's tree head against it. A chain that
   verifies over a history the store does not hold is not a chain this store may
   return. See
   [the chain](@/docs/periods-and-seals.md#why-the-fifth-rule-is-the-load-bearing-one).
10. **Seals bind their account handles, and the chain is checked against them.**
    A seal's `accounts` head is the commitment over the bindings the store itself
    holds, taken at the size the seal recorded — so renumbering the accounts
    table is caught, rather than merely being detectable by whoever thinks to
    look. The suite runs `SealChain::verify_against_accounts` over the stored
    chain and a registry rebuilt from the stored bindings, which is the check
    that turns the `accounts` head from evidence into evidence somebody reads.
11. **Periods seal in date order.** A closing balance is cumulative through the
   period's last day, so sealing one while an earlier period is still open lets
   an ordinary later booking restate it. Enforce it with
   `PeriodCalendar::check_sealable` rather than by hand — see
   [the sealed watermark](@/docs/periods-and-seals.md#the-sealed-watermark).
12. **A sealed balance stays provable and nameable.** `prove_sealed_balance`
   must keep answering after the registry has grown, an account has closed and a
   limit has changed — all routine — and it must refuse rather than return a
   proof when the rebuilt closing balance does not match the seal.
13. **Archived heads stay checkable, in `O(log n)`.** `head_at`,
    `prove_inclusion_at` and `prove_consistency_between` answer for a head
    published in the past — and answer it from a logarithmic number of reads
    rather than a replay. The cost is part of the contract, not an optimisation:
    a backend that rebuilds the tree per proof passes every correctness check and
    falls over at scale. Store the interior nodes and serve a
    `merkle::nodes` plan — see [Proofs](@/docs/proofs.md#cost).
14. **Entries are findable by their idempotency key.** The key is derived from a
    source transaction, so "did we already book message X?" is the question that
    follows — and the alternative to `get_by_key` is rebuilding the entry and
    re-submitting it, which answers by writing.
15. **Balances slice by dimension and by date, on either date.** A
    `BalanceQuery` narrows any balance read by reporting axis, date range, or log
    prefix, and the narrowings compose. The date range applies to the **booking**
    date by default and to the **value** date when the query says so — see
    [Two dates](@/docs/statements.md#two-dates-and-which-one-a-report-means). A
    backend that stores dimensions or a value date but cannot filter by them has
    a write-only feature.
16. **A statement scopes to a period and opens at the right figure.**
    `StatementPage::opening` carries in everything dated **before** the query's
    window plus everything the earlier pages showed, so a March statement opens
    at February's closing rather than at zero and `opening` plus the page's
    movements is the last line's `running`. The first half is defined by date,
    not by log position: entries arrive in recording order, so bounding it by
    position folds a later-dated entry into the middle of the statement and
    leaves a backdated one out of the opening. The suite books its fixture out of
    date order on purpose.
17. **Checkpoints never go backwards.** `save_checkpoint` leaves a *later*
    stored checkpoint in place. A checkpoint is a cache for a fold, so
    overwriting it with an earlier one records nothing and loses work — the next
    reader re-folds from further back — and it would make the stored value depend
    on which of two writers arrived last. Monotone, the write is idempotent and
    order-independent, which is what lets a caller checkpoint from more than one
    place without coordinating.

## Why a conformance suite ships with the library {#conformance}

A ledger's guarantees are only as good as its weakest backend. The ways a backend
can be subtly wrong — a read-then-write idempotency check that races, a batch
that half-lands, an index sequence with a gap — are exactly the ways that produce
no error and no symptom until an audit.

Publishing the trait alone would leave every implementor to guess what
"idempotent" or "atomic" means here. The suite makes the contract executable: a
backend either passes it or is not a backend.

One rule it cannot check is the first. "Append-only" is a claim about everything
a backend does *not* do, over all future time, and a suite only observes what it
can provoke. What it checks instead is every consequence it can reach — a record
read twice comes back identical, the log stays dense and complete across appends,
a refused write leaves nothing behind. Where the property has to be *enforced*
rather than tested, enforce it below the application: PostgreSQL can revoke
`UPDATE` and `DELETE`, and that is the difference between a convention and a
property.

```rust
use doubleentry::storage::{MemoryStore, conformance};

let store  = MemoryStore::<2>::new(LedgerId::new("my-ledger")?);
let report = conformance::block_on(conformance::check_all(&store));
report.assert_passed();
```

Twenty-five checks, each building its own accounts and entries, so a backend
needs to provide nothing but an empty store.

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

Two implementations is what keeps the contract honest. The seal chain carries an
explicit `chain_position` rather than ordering by `sealed_at, period_id`, because
a timestamp is neither chain order nor reliable — two seals in the same tick
order arbitrarily, and the fallback is lexical. A single backend can hide that
behind whatever its engine happens to do.

### Choosing between them

SQLite suits embedded and single-process deployments and needs no server.

PostgreSQL additionally enforces the balance invariant *in the database* via a
deferred constraint trigger — per currency **and layer**, the same key every
balance in the schema is reported against — enforces period non-overlap with an `EXCLUDE`
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
matches the stored one — and it is the same `TreeHead` a seal's `accounts` field
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

`Cursor` addresses an **entry**, which is what a log page is a list of.
Statements and open items are paged by [`PostingCursor`](@/docs/statements.md),
which addresses a **posting** — one entry may put several on one account, so a
boundary can fall inside an entry and an entry-addressed cursor cannot express
that position.

## Static and dynamic dispatch

`LedgerStore` uses `async fn` in trait, which compiles to static dispatch with no
per-call allocation but is not `dyn`-compatible. Where a backend must be chosen at
run time, `DynLedgerStore` boxes the futures and restores object safety. A blanket
implementation covers every `LedgerStore`, so any backend works either way
without extra code.
