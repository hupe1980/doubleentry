+++
title = "Cold tier"
description = "Compacting sealed periods into Apache Iceberg so the operational database stays small and the archive stays verifiable."
weight = 15
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

## If you do prune, know what it costs

Compaction is worth doing on its own — a queryable columnar mirror any engine can
read — and pruning is a separate decision you make afterwards, if at all.

A store builds its inclusion and consistency proofs from the leaves it holds. So
removing an archived prefix renumbers every leaf after it, and the store can no
longer build a proof for **any** entry — not only the archived ones. The tree
head does not notice: it is read from the last row's stored root, so head and
proofs would disagree silently.

That is the shape of failure this crate refuses to ship, so the SQL backends
check that the log they read back is dense from zero and return `LogNotDense`,
naming the hole:

```text
the log is missing entries: expected log index 0, found 5;
proofs are built from the stored leaves and a hole renumbers every one after it
```

Without the check, an index past the shortened set reported `IndexOutOfRange` for
an index that is genuinely in range, and one *inside* it returned a proof for a
different entry — caught only if the caller verified before handing it to an
auditor.

Proofs over an archived period come from the archive and its seal after that.
The seal is in the snapshot summary precisely so they can.

The cold tier is off by default — it pulls in Arrow, Parquet and object storage,
which is a large dependency surface for a feature most deployments do not need.
