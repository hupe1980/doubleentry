+++
title = "Performance"
description = "The benchmarks behind write throughput, proof and audit costs, reports and Journal memory, and how to run them."
weight = 6
+++

Throughput depends on the store, the disk and the database configuration, so
measure on your own hardware. The benchmarks under `benches/` use criterion, are
compiled in CI (`just bench-build`) and are never run or gated there.

```text
just bench                    # everything (PostgreSQL scenarios need Docker)
just bench memory             # one store: memory, sqlite or postgres
just bench proofs             # proof and audit costs, every store
just bench journal_memory     # Journal heap use
```

## What is measured

| Bench | Scenarios |
|---|---|
| `throughput` | entries per second through `Ledger` on `MemoryStore`, a SQLite file and PostgreSQL 17: a 2-posting entry per `record`; an 8-posting entry with two dimensions per posting; `record_batch` of 1, 10 and 100 drafts; four tasks writing to one ledger; one draw on a limited account whose history holds 10⁴ and 10⁵ draws |
| `proofs` | on a ledger of 10⁶ entries per store: `prove_entry`, `prove_consistency` from an earlier size, `prove_balance` in a sealed period, and a full `audit()` |
| `report` | a current balance, a position-bounded one, a trial balance and the export of 100 000 entries, on `Journal` and on `Ledger` over `MemoryStore` |
| `core` | `audit` of 100 000 entries, and `record` on one hot account |
| `journal_memory` | net heap a `Journal` holds after 10⁵ and 10⁶ entries, for 2-posting entries and for 6-posting entries with two dimensions each |

SQLite runs with the store's own settings (WAL, `synchronous=FULL`);
PostgreSQL runs in a `postgres:17-alpine` container with its default
configuration. `DOUBLEENTRY_BENCH_ENTRIES=<n>` shrinks the `proofs` and
`journal_memory` ledgers for a quick run.

## What to expect

- **Writes** are serialised per ledger (see [Stores](@/docs/operating/stores.md)):
  one transaction per `record`, so commit latency bounds single-entry
  throughput. `record_batch` writes many drafts in one transaction; independent
  ledgers write in parallel.
- **A limited account** is checked against its maintained balance and
  outstanding hold totals, not by summing its history; the `throughput` bench
  compares a draw at 10⁴ and 10⁵ prior draws.
- **Proofs** read `O(log n)` stored nodes in one batched read; a sealed-balance
  proof reads one account's path, not the period's postings.
- **`audit()`** reads and replays every leaf and every table once: linear in the
  log.
- **`Journal`** keeps everything in memory and evicts nothing; its heap grows
  linearly with the entries.
