# Changelog

Notable changes to `doubleentry`, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned per
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Every version is
published on crates.io. ⚠️ marks a change to a hash, an encoding or the schema.

## [Unreleased]

## [0.9.0] — 2026-11-07

### ⚠️ Breaking

- ⚠️ Schema `doubleentry/2` (`entries` stores the entry kind). A 0.8 database
  is refused with `ConfigError::SchemaMismatch`; create the ledger anew. The
  log format, hashes and leaves are unchanged.
- `clear` and `reset_clearing` return `Cleared { position, replayed }`.
  **Migration:** read `.position`.
- A batch holds at most `MAX_BATCH` (1 000) items; more is rejection 124.
  Record a larger year-end close in chunks.
- `Op::Batch` holds `BatchItem`s, `Done::Batch` `BatchReceipt`s,
  `Done::Cleared` a `Cleared`; `PostingLine` has `kind`, `PostingRead` takes
  a `KindFilter`.
- An invalid schema name is `Error::Config`, not `Error::Backend`.

### Added

- `batch(items)`: entries, clearings and resets in one transaction, all or
  none, one receipt per item; `clear_batch` for clearings alone.
- `entry_id(key)`: an entry's id before it is recorded.
- `periods()`, `seals()`, `watermark()` and their `_at(head)` variants;
  `SealReceipt::{start, end, through}`.
- `BalanceQuery::of_kind` and `without_kind`.
- `ConfigError::ForeignSchema`: a schema or file holding other tables (or a
  0.7 ledger) is refused by name before any DDL.
- `Postgres::runtime_grants`, `SqlStore::<Postgres>::runtime_grants` and
  `grant_runtime`: least-privilege grants generated from the schema.
- `BatchItem` and `Clearing` in the prelude; `examples/payment_run.rs`.

## [0.8.1] — 2026-11-07

### Added

- `postgres-tls`: PostgreSQL over TLS (rustls on aws-lc-rs, Mozilla roots).
  Without it, a URL that requires TLS fails with an error naming the feature.
- `examples/outbox.rs` and an outbox section in the idempotency guide: one entry
  per committed business change.
- The PostgreSQL constructors document the connection URL and the pool budget.

### Changed

- TLS no longer comes from your own `sqlx` dependency. **Migration:** drop
  `sqlx` if you added it only for TLS, and enable `postgres-tls`. sqlx is an
  implementation detail; its upgrades ship without an API change.

## [0.8.0] — 2026-10-16

### ⚠️ Breaking

- A new model, hash, encoding and schema. 0.8 cannot read a 0.7 ledger and there
  is no migration: create a new ledger and re-record the books from their source
  documents. A 0.7 ledger stays readable with 0.7.
- A ledger is one append-only RFC 6962 SHA-256 Merkle log of typed records
  (genesis, account, currency and policy events, periods, entries, clearings,
  seals). One pure rule core decides every write; `Journal` (sync, in memory)
  and `Ledger<S: Store>` (async) call it with the same method names.
- SHA-256 replaces BLAKE3 for every hash.
- `EntryId` is derived from the ledger id and the idempotency key; no UUIDs.
- `Journal::new(LedgerId)` (`P = 2`; `Journal::<P>::with_precision` for other
  scales). The ledger id is the C2SP origin.
- `record(Entry) -> Result<Recorded, Error>` is the only way in; `record_batch`
  is atomic. Builders are infallible and take account paths or handles;
  `validate(&draft)` lists structural violations. `SealContext`, `Entry::seal`,
  `Entry<Balanced>`, `adopt_verified`, `EntryBatch` and `record_validated` are
  gone.
- Master data are dated records: `open`, `register(AccountSpec)`, `set_limit`,
  `set_class`, `close_account`, `reopen_account`; `as_actor` names the writer.
- Periods: `define_period(id, start, end)`, `define_months`, `seal_period`,
  `seal_through`. A period is sealed when a seal record exists.
- Proofs: `prove_entry`, `prove_entry_at`, `prove_consistency(old, new)`,
  `head_at`, `prove_balance(account, currency, layer, period)` →
  `log::SealedBalanceProof`, verified against a head alone.
- `audit()` on both façades replaces `verify_log`, `verify_balanced`,
  `verify_balances` and `verify_seals`.
- One `Error` with nine variants (`Rejected`, `BatchRejected`, `Value`,
  `Amount`, `Query`, `Transparency`, `Integrity`, `Backend`, `Config`) replaces
  every per-module error type; payloads live in `doubleentry::error`.
- `store::Store` replaces `LedgerStore` and `DynLedgerStore`;
  `store::SqlStore::sqlite(url)` / `SqlStore::postgres(url)` replace
  `SqliteStore` and `PostgresStore`. `Ledger::new` creates or opens; one schema,
  `doubleentry/1`, and no migrations.
- `transparency` (C2SP checkpoints, `WitnessSession`, `Policy`, `Verifier`)
  replaces the `witness` module; there is no in-crate witness.
- Report types moved to `doubleentry::report`; `BalanceQuery` stays at the root.
- `amount!`, `currency!` and `date!` are checked at compile time.
- sqlx 0.9; neither store feature enables a TLS stack.

### Added

- 76 rejections with stable numeric codes, grouped by stage
  (`Rejection::code`, `Rejection::stage`, `Error::code`).
- Idempotent retries: the same draft returns the original `Recorded` with
  `replayed == true`; the same key with other content is `KeyConflict` (200).
- Holds on the Pending layer: `Entry::open_hold`, `post_hold` (full or
  partial), `void_hold`, `expire_hold`, `hold`, `holds_due`.
- Balance limits checked inside the write, holds included.
- `Amount<P>` over `i128`: seven `Rounding` modes, `allocate`, `distribute`,
  `checked_mul_ratio`, `display_at`.
- Erasable memos behind a salted commitment (`erase_memo`, `memo_salted`,
  `with_salt_source`).
- Sealed balances: one account-indexed tree per (currency, layer), provable and
  named per account.
- Reads over one `BalanceQuery`: `balance`, `trial_balance`, `statement`,
  `rollup`, `open_items`, `check_assertion(s)`, `entry`, `entry_by_key`.
- `doubleentry::export`: head-pinned flat rows and `export::verify`.
- `store::MemoryStore`, `SqlStore` for SQLite and PostgreSQL
  (`SqlStore::<Postgres>::with_schema`), and the public conformance suite
  (`testing` feature).
- `transparency`: C2SP `signed-note`, `tlog-checkpoint`, `tlog-cosignature`,
  `tlog-witness`, `tlog-policy`, `tlog-proof` and `tlog-tiles`; `checkpoint`,
  `bundle_record`, `bundle_entry`, `bundle_balance`, `tile`, `tile_entries`.
- `Journal::from_log`, `Journal::leaves`, `Ledger::to_journal`.
- `doubleentry::prelude`.
- `tracing` feature: spans per façade operation and per SQL store write.

### Removed

- Modules `account`, `balance`, `canonical`, `checkpoint`, `clearing`,
  `closing`, `dimensions`, `entry`, `hash`, `merkle`, `money`, `period`,
  `posting`, `rollup`, `seal`, `storage`, `witness`.
- The `iceberg` feature and the cold tier, deferred sequencing, pruning,
  balance checkpoints, the seal hash chain, `schema/*.sql` and `migrate`.
- Dependencies `blake3`, `uuid`, `smallvec`, `iceberg`, `arrow-*`, `parquet`.

## [0.7.0] — 2026-08-26

- Account rollups over the path tree.
- ⚠️ One `BalanceQuery` for every balance read; reads fold by booking or value
  date; dimensions can be filtered and reported on.
- ⚠️ Entries balance per currency and layer; balance limits count both layers.
- ⚠️ Proofs read `O(log n)` stored nodes; seals prove the log was only appended
  to; a consistency proof from the empty tree is refused.
- Witness cosigning, `Journal::audit()`, `get_by_key`,
  `Amount::checked_mul_ratio` and `Rounding`.
- Fixed: SQLite parameter binding after a reused placeholder.

## [0.6.0] — 2026-08-17

- `prove_sealed_balance` returns `SealedBalanceOutcome`.
- Added `LedgerStore::all_open_items`.

## [0.5.0] — 2026-08-17

- ⚠️ The account-binding commitment covers only handle and path.
- Sealing a period closes every earlier date.
- Fixed: sealed-balance proofs through a store and after new registrations,
  paged statements and open items, proofs over a pruned log.

## [0.4.0] — 2026-08-15

- ⚠️ Seals commit to tree heads (size and root); the seal hash changed.
- Proofs verify against a `TreeHead`, not a bare root; proofs against an
  archived head; `LedgerStore::head_at`.

## [0.3.0] — 2026-08-14

- ⚠️ The entry hash and the seal hash changed.
- Added account-binding root, balance proofs, `BalanceLimit`, date-based
  balance assertions, period persistence.
- Balances take a prefix size, not an index.

## [0.2.0] — 2026-08-01

- Fixed: an entry's `kind` was hashed but not stored.
- Added `StatementLine::kind`.

## [0.1.0] — 2026-07-28

- Initial release.

[Unreleased]: https://github.com/hupe1980/doubleentry/compare/v0.8.1...HEAD
[0.8.1]: https://github.com/hupe1980/doubleentry/compare/v0.8.0...v0.8.1
[0.9.0]: https://github.com/hupe1980/doubleentry/compare/v0.8.1...v0.9.0
[0.8.0]: https://github.com/hupe1980/doubleentry/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/hupe1980/doubleentry/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/hupe1980/doubleentry/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/hupe1980/doubleentry/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/hupe1980/doubleentry/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/hupe1980/doubleentry/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/hupe1980/doubleentry/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/hupe1980/doubleentry/releases/tag/v0.1.0
