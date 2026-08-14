+++
title = "Design boundaries and testing"
description = "What the crate deliberately is not, the two structural rules it does enforce, and how its invariants are verified."
weight = 17
+++

## What it is not

Not an ERP, an ORM, a reporting engine, a chart of accounts, a payments library,
a policy engine, or a distributed system. It produces validated, balanced,
provable entries and leaves every domain decision to you.

Specifically, it does **not**:

- **Convert currencies.** A rate is a business decision with a source, a
  timestamp and a policy behind it. A library that guessed one would be wrong in
  a way no downstream report could detect.
- **Ship a chart of accounts.** It does not know what `Assets` means, does not
  require that name, and validates against no national or corporate scheme.
- **Name your reporting axes.** See [Dimensions](@/docs/dimensions.md).
- **Decide your retention.** Pruning archived history has legal weight.
- **Read a clock.** Identity and dates are supplied by you, which is what makes
  replay reproduce byte-identical output.

## What it does enforce

Two structural rules, because no downstream layer can repair either:

- **Only leaves are postable.** Posting to both a node and its descendants makes
  every rollup double-count, and no reporting layer can undo it afterwards.
- **Postings fall inside the account's open window.**

And one rule you opt into per account, for the same reason — an overdrawn cash
account is not something a report can repair either:

- **A [balance limit](@/docs/accounts.md#balance-limits)**, when set, is checked
  against the balance an entry would leave behind, per currency and per layer.

Everything else that is enforced follows from double-entry itself: entries
balance per currency, amounts are positive magnitudes with an explicit side, and
sealed periods do not accept postings.

## Determinism as a property, not a habit

The engine reads no clock, no random number generator, and no hash-map iteration
order. Identical inputs produce identical bytes, hashes and orderings.

This is checked in CI rather than trusted: a job greps the engine for
`SystemTime::now`, `Instant::now`, `std::fs`, `std::env`, `std::net`,
`std::process`, `async` and `unsafe`, and fails the build if any reach it.
Storage backends are exempt by construction — talking to a database is their
whole job — so the check covers the core only.

The two clock-reading helpers that do exist, `EntryId::generate` and
`ClearingId::generate`, are marked as such and are never called by the engine.

## Lints as a floor

The crate forbids `unsafe_code`, and **denies** `arithmetic_side_effects`,
`indexing_slicing`, `unwrap_used`, `expect_used` and `panic` in library code.

A ledger that can panic mid-append is a ledger that can tear a batch in half. So
every arithmetic path goes through a checked operation that surfaces overflow as
a typed error, and there is no index that can be out of bounds.

## How the invariants are tested

**Property tests** over generated inputs rather than hand-picked cases:
splitting conserves money for any total and weights; inclusion and consistency
proofs verify for every log size and every prefix; tampering with any committed
leaf changes the root; seals detect any edit to their own fields; checkpoints
always re-derive; clearing never over-applies and never moves money; closing
always balances and always flattens; canonical encoding is a pure function of the
value; and a journal of balanced entries always folds to a balanced trial balance.

**Simulation.** Long, randomly generated sequences of appends, replays,
reversals, clearings, resets, seals, and imposing and lifting balance limits,
with *every* invariant re-checked after each step — including that an entry the
engine accepted leaves every limit in force satisfied, and that a checkpoint
taken at any point still re-derives. Sequences are generated from a seed, so a
failure reproduces exactly, which is what the deterministic kernel is for.

**Golden vectors.** Committed hashes for the canonical encoding, the seal
preimage and the Merkle log. Nothing in the compiler stops a field being
reordered or a domain tag being edited, and every such change silently
invalidates every hash ever written while leaving the rest of the suite green.

**Robustness.** No input, however malformed, may panic a parser or a
deserialiser. A panic on hostile input is a denial of service in something meant
to be a system of record.

**Cost.** A guard on the shape of the curve, not on wall-clock: appending four
times as many entries must not take sixteen times as long. Quadratic behaviour in
a ledger fails no test — it just makes the books slower the longer they are kept.

**Conformance.** The storage contract, executable — twenty checks, run against
the in-memory, SQLite and PostgreSQL backends, and against PostgreSQL twice, once
per sequencing mode. See [Persistence](@/docs/persistence.md#conformance).

**Real databases.** SQLite runs in-process; the PostgreSQL tests start a
throwaway container. Nothing is mocked: the constraints, the locking, the
deferred triggers and the behaviour under concurrent appends are the real ones,
because those are exactly the parts that cannot be checked any other way.

```console
cargo test                        # everything but the databases
cargo test --features sqlite      # adds SQLite; no server needed
cargo test --features postgres    # adds PostgreSQL; needs Docker
cargo test --features iceberg     # adds the cold tier; writes to a temp dir
cargo clippy --all-targets --all-features
```

## On changing the encoding

Before the first release, a deliberate format revision means regenerating the
golden vectors, because there are no ledgers in the world to be compatible with.

After it, it means bumping the encoding version in the domain tag as well — so
old bytes are never silently reinterpreted under a new format — and writing down
how existing seals migrate.
