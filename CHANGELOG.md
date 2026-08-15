# Changelog

Notable changes to `doubleentry`, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned per
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Nothing has been published to crates.io yet.** `v0.1.0` through `v0.3.0` are
development tags; `0.4.0` is the first release intended to reach the registry.
Until 1.0 every release may break, and this one does — the reasoning is in each
entry, because a breaking change without a reason is just churn.

## [0.4.0] — 2026-08-15

### ⚠️ Hashes and schema changed

A seal now commits to a **tree head** — a size *and* a root — everywhere it
previously committed to a bare root. That changes the seal preimage, so the
reference seal vector moved:

| Vector | before | after |
|---|---|---|
| Reference seal hash | `ab58bc1a…` | `7f6f0218…` |

The entry hash, the account-binding commitment and every Merkle constant are
**unchanged**. The `seals` table gains `trial_balance_size` and `accounts_size`;
apply `schema/sqlite.sql` or `schema/postgres.sql` as they now stand. Nothing is
published to crates.io, so there is no migration and no ledger in the world to
be compatible with.

### Changed

- **Proofs verify against a `TreeHead`, never a bare root.**
  `InclusionProof::verify` now takes `&TreeHead` in place of `&Hash`, and
  `ConsistencyProof::verify` takes two. There is deliberately no root-only form
  left to reach for.

  The reason is a real defect, not tidiness. `leaf_index` and `tree_size` *steer*
  the walk rather than being checked by it, and neighbouring pairs steer it
  identically — so against a bare root a genuine proof for leaf 1 of a two-leaf
  log is accepted **unchanged** as a proof for leaf 2 of three, a position that
  log does not have. Rewriting the index alone fails and rewriting it past
  `tree_size` is refused; it is rewriting both together that aliases. Consistency
  proofs alias the same way in `new_size`. No false claim about a real log
  follows — a root determines its own size — but a verifier reading the position
  back was reading a number the prover chose. Pinning the size to a head the
  verifier already trusts leaves exactly one labelling that verifies, and costs
  one integer comparison.
- **`Seal::trial_balance_root` → `Seal::trial_balance`** and
  **`Seal::accounts_root` → `Seal::accounts`**, both now `TreeHead`. A seal is
  what a `BalanceProof` and an `AccountBindingProof` are checked against, so it
  has to carry the half that was missing.
- **`AccountRegistry::commitment` returns `TreeHead`**, and
  **`trial_balance_root` is now `trial_balance_head`**. `TrialBalanceCommitment`
  gains `head()` beside `root()`.
- `BalanceProof::verify` and `AccountBindingProof::verify` take the
  corresponding head.

### Added

- **Proofs against an archived head.** `MerkleLog::inclusion_proof_at`,
  `consistency_proof_between`, and `head_at`, mirrored on `Journal` as
  `prove_inclusion_at` / `prove_consistency_between` / `head_at` and on
  `LedgerStore` as all three. An auditor archives a head and comes back later;
  the log has grown and its current root proves nothing about the head they
  hold, so a proof against the present log was no use to them. The general
  consistency form relates two archived heads without either party learning the
  log's present size.
- **`LedgerStore::head_at`** is an indexed row read on both SQL backends, not a
  replay: every entry already stores the root as of its own sequencing. The
  conformance suite checks every historical head against a rebuilt log, which is
  precisely where a stored column and a replay could drift apart.
- Iceberg snapshots carry `doubleentry.trial_balance_size` and
  `doubleentry.accounts_size`, so a reader working from the table alone has both
  halves of each sealed head.

### Tests

- Exhaustive **proof-deformation** coverage for both proof types: every sibling
  insertion, deletion, duplication, adjacent swap and truncation point over
  every leaf of every log shape up to 24, plus property tests. The suite
  previously altered a path *value* but never its *length*, so padding and
  truncation went unexercised on the verifier that rejects them.
- The `leaf_index >= tree_size` range guard is now covered. It is load-bearing:
  without it a genuine proof for leaf 0 of eight verifies while claiming index
  8, the surplus index bits shifting off the top of the walk.
- The `old_size > new_size` guard is now covered, and is load-bearing twice
  over: it refuses a log shown to shrink, and it stands between a `new_size` of
  zero and an underflow in a module that turns the checked-arithmetic lint off.
  Handing the two heads over in the wrong order reaches it.
- Equal-size consistency proofs are checked negatively as well as positively —
  with no path to walk, the equality of the two roots is the entire check.
- **Golden vectors for proof paths**, inclusion and consistency, at six
  `(index, size)` and six `(old, new)` pairs. Sibling ordering within a path can
  be changed without moving any root, which would invalidate every proof ever
  handed out while leaving the root vectors green. RFC 6962 publishes proof
  vectors for the same reason.

## [0.3.0] — 2026-08-14

### ⚠️ Hashes changed

Two of the crate's committed golden vectors moved. Any hash, seal or proof
produced by `0.2.0` is invalid under `0.3.0`, and there is no migration: the
crate is unreleased, so there are no ledgers in the world to be compatible with.

| Vector | `0.2.0` | `0.3.0` |
|---|---|---|
| Reference entry content hash | `f66e3336…` | `5bd373dc…` |
| Reference seal hash | `98cde30e…` | `ab58bc1a…` |

The Merkle constants — the empty root, the leaf hash, and the roots for known
tree sizes — are **unchanged**, so the log structure itself is untouched.

After 1.0 a change of this kind additionally means bumping the encoding version
in the domain tag, so old bytes can never be silently reinterpreted under a new
format.

### Added

- **`Seal::accounts_root`** — a third Merkle root, over the handle-to-account
  bindings in force when the period was sealed. A trial-balance leaf names its
  account by handle, so without this the handles float: re-registering the same
  paths in a different order would leave every seal and every balance proof
  verifying byte for byte while each balance quietly referred to a different
  account. Comes with `AccountRecord`, `AccountBindingProof`,
  `account_binding_leaf`, `AccountRegistry::{commitment, prove_binding, records,
  restore, from_records}`.
- **`BalanceProof` and `TrialBalanceCommitment`** — prove that one account held
  one balance under a seal, in `O(log n)`, disclosing nothing else.
  `BalanceProof::verify_naming` checks the balance *and* the account it belongs
  to against a single seal, which is what turns "handle `#7` held this" into
  "`Assets:Cash` held this" without handing over the chart of accounts.
- **`BalanceLimit`** on an account — `NoCreditBalance` for an asset that cannot
  be overdrawn, `NoDebitBalance` for a liability that cannot be drawn beyond
  what was funded. Checked when an entry is *recorded*, against the balance the
  whole entry would leave behind, per currency and per layer independently.
  Both SQL backends enforce it inside the append transaction — PostgreSQL takes
  a row lock on the constrained account — because a limit checked before the
  write reads a pre-image that two concurrent appends both see, each fitting it
  and together breaching it.
- **Date-based balance assertions** — `BalanceAssertion::on_date` and
  `Journal::balance_on_date` fold by booking date. A bank statement says "as at
  31 March", not "after 4 812 entries", and folding by date is what puts a
  late-arriving backdated entry in the period it economically belongs to.
- **`LedgerStore::{define_period, transition_period, periods}`** — the period
  calendar is store state. A calendar held only in the caller's memory comes
  back open after a restart and starts accepting postings into books that have
  already been committed to.
- **`SealChain::from_seals`** — rebuild and re-check a stored chain in one call.
  Seals read back from a table are rows, not evidence, until a chain has
  accepted them.
- `MerkleAccumulator::try_from_parts` and `MalformedAccumulator`, so a backend
  that persists only the perfect-subtree cover can prove the rows it read back
  are the rows it wrote.
- `Hash::digest` and `RESERVED_DOMAIN_PREFIX`, so a caller hashing its own
  source documents for `DocumentRef` uses the engine's domain-separated
  construction rather than inventing a bare SHA-256.
- Two conformance checks, bringing the executable storage contract to twenty:
  posting dimensions survive a round-trip, and balance limits are enforced.

### Changed

- **Balances take a prefix *size*, not an index.** `Journal::balance`,
  `Journal::trial_balance`, `LedgerStore::balance`, `LedgerStore::trial_balance`
  and `LedgerStore::balances` now take `Option<u64>` counting entries: `Some(0)`
  is the empty ledger, `None` is everything so far. It is the same number a
  `TreeHead` carries, so a balance and the root it belongs with are named the
  same way — and a "last index included" of zero could not express an empty
  prefix at all. Both SQL backends moved from `log_index <= n` to
  `log_index < size`.
- **`Checkpoint` lost its `through_index` field.** The tree head already carries
  `size`; it now does double duty, naming the prefix the balance covers *and*
  pinning the history that prefix belongs to. `Checkpoint::new` takes three
  arguments, `Checkpoint::size()` reports the prefix, and
  `CheckpointError::IndexOutOfRange` became `SizeOutOfRange`. Two fields that
  must agree are two fields that can disagree, and these did — see *Fixed*.
- **`BalanceAssertion::at` is an `AssertAt` enum**, replacing `Option<u64>`;
  `at_index(i)` becomes `over_prefix(size)`.
- **`SealChain::new` takes a `LedgerId`**, and no longer implements `Default`.
- **`LedgerStore::register_account` is an upsert.** Re-registering a handle
  updates its classification, open window and balance limit; the path at a
  handle stays immutable and rebinding one is refused. Mirrored by
  `AccountRegistry::restore`.
- `Journal` gained `define_period` / `transition_period` / `seal_period`,
  replacing direct calendar manipulation for the sealing path — the seal has to
  commit to the balances before the period's state changes, which a bare
  transition cannot do.
- `schema/postgres.sql` and `schema/sqlite.sql`: `accounts.balance_limit` and
  `seals.accounts_root` added, `checkpoints.through_index` dropped. The crate is
  unreleased, so the reference DDL changed in place rather than by migration.

### Fixed

- **A checkpoint taken over an empty journal broke as soon as anything was
  recorded.** `Checkpoint.through_index: None` meant "empty prefix", while
  `Journal::balance(key, None)` meant "the current balance" — the same `None`,
  opposite meanings. The checkpoint verified when taken and returned
  `BalanceMismatch` forever after. Removing the field removes the ambiguity.
- **Master-data changes could not be persisted.** `AccountRegistry::close`
  existed and `accounts.closed_on` existed, but `register_account` was
  `ON CONFLICT DO NOTHING` in both SQL backends — so closing an account in a
  durable ledger was silently a no-op.
- **A seal chain did not enforce its own ledger.** `ForeignLedger` was only
  raised when comparing a seal against a predecessor, so a chain of length one
  accepted a seal from any books at all — while the `LedgerId` sits inside the
  seal preimage precisely to prevent that. The chain now names the ledger it
  covers and checks the first seal as strictly as the last.
- **One period could be sealed twice in a chain**, giving two commitments to one
  period's closing balances with nothing saying which the books mean. Now
  `SealChainError::DuplicatePeriod`.

### Removed

- **The shipped dimension newtypes** `ActivityId`, `CostObjectId`, `PartyId` and
  `SegmentId`. Naming four axes in the library was a chart of accounts by
  another route, and had to be worked around by everyone whose fifth axis
  mattered. Use `Dimensions` with caller-named `Label` axes;
  `LedgerPolicy::requiring` is how you insist a posting carries one.

### Notes

A balance limit can refuse a **reversal**: undoing a funding entry withdraws
money the account may since have committed, and a limit constrains the resulting
balance, so it cannot make an exception for a correction. Reverse whatever
consumed the funding first, or lift the limit deliberately. The interaction was
found by the randomised simulation and is pinned by a test and a checked-in
proptest seed.

## [0.2.0] — 2026-08-01

### Fixed

- **An entry's `kind` was hashed but never stored.** `Entry::with_kind` existed
  in the engine and the label was folded into the content hash, but neither SQL
  schema had a column for it and neither backend wrote it. Because `get`
  rehydrates through `adopt_verified`, which recomputes the hash and refuses a
  mismatch, that did not under-report the field — it made every kinded entry
  unreadable after a round-trip. Added `entries.kind` to both reference schemas
  and the read/write path to both backends.

### Added

- `StatementLine::kind`, so a statement can be grouped or filtered by document
  type without a second lookup per line.
- A conformance check that `kind` survives a store round-trip, including under
  PostgreSQL's deferred sequencing mode — which is what would have caught the
  above.

### Changed

- `StatementLine` is no longer `Copy`, since it now carries a `Label`.

## [0.1.0] — 2026-07-28

Initial development tag: the balanced-by-construction entry, exact
scaled-integer money, the canonical encoding and domain-separated hashes, the
append-only Merkle log with inclusion and consistency proofs, period seals and
the seal chain, open-item clearing, closing entries, the `LedgerStore` contract
with its conformance suite, and the in-memory, SQLite, PostgreSQL and Iceberg
backends.

[0.4.0]: https://github.com/hupe1980/doubleentry/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/hupe1980/doubleentry/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/hupe1980/doubleentry/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/hupe1980/doubleentry/releases/tag/v0.1.0
