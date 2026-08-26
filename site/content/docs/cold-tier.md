+++
title = "Cold tier"
description = "Compacting sealed periods into Apache Iceberg so the operational database stays small and the archive stays verifiable."
weight = 16
+++

## Why archive at all

A sealed period is finished: its entries will not change, and keeping them in the
operational database costs index size, vacuum time and backup weight forever.

The `iceberg` feature compacts sealed periods into Parquet under an Apache
Iceberg table, where any query engine can read them — DataFusion, DuckDB, Trino,
Spark — without weakening what the seal promises.

```rust
use doubleentry::storage::iceberg::ColdTier;

let cold   = ColdTier::new(table_ident);
let result = cold.compact(&store, &seal, &catalog).await?;

assert_eq!(result.verified_root, seal.tree_head.root);
```

## The seal travels with the data

Each compaction commits **one** Iceberg snapshot whose summary carries the seal
hash, the tree root, the trial-balance root, the accounts root, the period, and
the log position archived through.

An auditor handed a table name and a seal hash can verify the archive with
off-the-shelf tooling and no access to this crate. The commitment is not stored
beside the data in some sidecar the next migration forgets — it is in the
snapshot metadata the table format already maintains.

Entry rows carry their canonical encoding alongside the decoded columns, so an
auditor can recompute an entry's hash from the table alone rather than having to
trust the decoded fields.

## Incremental, but verified against the whole archive

Compaction resumes from the position the last one left behind, so archiving the
second period does not re-read the first. But the root it checks is the root over
**everything archived so far**, not just the new rows.

That is what the persisted accumulator in the snapshot summary is for: the
subtree stack is restored, the new leaves are folded in, and the resulting root
is compared against the seal. A mismatch aborts the compaction rather than
committing a snapshot whose contents do not match what it claims.

So each compaction reads `O(new rows)` while verifying `O(entire archive)`.

## Refused rather than papered over

Two failure modes abort:

- **`SealBehindArchive`** — the seal commits to a shorter log than the archive
  already holds. Archiving it would mean the table contains entries the seal does
  not cover.
- **A snapshot carrying no accumulator** — the table has a history this crate did
  not write, so there is no verified state to resume from.

Both are conditions where continuing would produce an archive that looks
verifiable and is not.

## What it does not do

It does not delete anything from the operational store. Pruning hot storage is a
retention decision with legal weight, and a library that made it for you would be
making it wrongly for someone.

## If you do prune, the tree survives it

Compaction is worth doing on its own — a queryable columnar mirror any engine can
read — and pruning is a separate decision you make afterwards, if at all.

When you do, **proofs keep working**. The SQL backends keep the Merkle tree in
its own table (`log_nodes`), so archiving a prefix of `entries` leaves it
untouched: the log keeps its length and its root, and an inclusion proof for an
archived entry is built and verified exactly as before.

That is the arrangement an archive is *for*. The proof comes from the hot store;
the entry it is checked against comes from Iceberg:

```rust
let proof = store.prove_inclusion_at(index, archived_head.size).await?;
// … the leaf comes from the Parquet file, not from the pruned row …
assert!(proof.verify(&archived_entry.content_hash(), &archived_head));
```

What pruning costs is the entry bodies: `get` stops finding them, a
statement stops listing them, and a balance folded over the pruned range comes
out short. Those answers live in the archive now — which is why the seal is in
the snapshot summary.

The cold tier is off by default — it pulls in Arrow, Parquet and object storage,
which is a large dependency surface for a feature most deployments do not need.
