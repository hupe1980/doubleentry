# Changelog

Notable changes to `doubleentry`, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned per
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Every version is published on crates.io**, `0.1.0` onward, none yanked. Until
1.0 a release may break compatibility, and several have; the reasoning is in
each entry, because a breaking change without a reason is just churn.

Entries marked ⚠️ change a hash, an encoding or the schema. Read those before
upgrading a ledger that already holds data — each one now says what an existing
ledger has to do, which earlier revisions of this file wrongly said was nothing.

## [Unreleased]

Renamed to `[0.6.0]` when tagged, as `[Unreleased]` was for `0.5.0`.

Everything here answers integration feedback from `accountingd` against `0.5.0`.
Two of the three observations were valid; the third rested on a hazard that does
not exist, and is recorded below with what the real cost is instead.

### Changed

- **`prove_sealed_balance` returns `SealedBalanceOutcome`, not
  `Option<SealedBalance>`.** "The account has no row" and "the account did not
  exist yet" are both *answers* — the books are intact and the question simply
  has a negative reply — so both now sit on the `Ok` side as `NoRow` and
  `NotYetRegistered`, with `is_absent()` for the common case and `into_proven()`
  for the rest. `SealedBalanceError::NotYetRegistered` is gone.

  Not cosmetic, and worse than it was reported as. `LedgerStore::Error` is the
  *backend's* type and is only required to be `From<SealedBalanceError>`. There
  is no route back, so an answer routed through the error path was
  **unreachable** from generic code over `S: LedgerStore<P>` — the suggested
  remedy of a `SealedBalanceError::is_absent()` could not have been called. The
  error type is now only ever a real failure.

### Added

- **`LedgerStore::all_open_items`.** The drain loop, once, in the crate, for the
  callers that genuinely need the whole set: allocating a payment across invoices,
  or totalling what an account has outstanding. Both are answered wrongly by a
  partial list and `next` is easy to leave unread. Explicitly unbounded — it is
  the read `open_items` is paged to avoid, offered because some questions have no
  bounded answer.

  Worth stating what a partial read does *not* cost, since it is the obvious
  guess: it cannot clear a newer item ahead of an older one. Pages come oldest
  first, so the first page **is** the oldest items and FIFO over it is correct
  FIFO. What it costs is completeness — a payment larger than the page's
  residuals under-allocates, and a total comes out short.

### Documentation

- **The changelog claimed nothing was published to crates.io. Everything is.**
  `0.1.0` onward, none yanked. The claim was wrong in every revision of this
  file, and it was not idle: it was the stated *justification* for three
  hash-breaking releases — "there is no migration", "no ledger in the world to be
  compatible with". Anyone upgrading a real ledger was told there was nothing to
  do.

  Each ⚠️ entry now says what an existing ledger actually faces, and the three
  differ sharply: `0.3.0` moved the **entry** hash, so `0.2.0` rows become
  unreadable and have to be re-recorded; `0.4.0` moved the **seal preimage**, so
  earlier seals fail `is_self_consistent()` while entries stay readable; `0.5.0`
  moved only the **binding leaf**, so seals still verify and balances still
  prove, but balances in periods sealed earlier cannot be *named*.

- **The rule for changing an encoding assumed a pre-release crate.** Both the
  design guide and the golden-vector tripwire — the text a developer reads at the
  moment they break a vector — said a revision needs no tag bump "before the
  first release". That release was `0.1.0`. Both now state the post-release rule
  and record the two revisions that shipped without it (`0.3.0`'s entry
  encoding, `0.5.0`'s binding leaf, both still tagged `v1`), with why re-tagging
  them now would cost more than the ambiguity does.

- **The sealed watermark is derived, never stored — and now says so.**
  `PeriodCalendar::from_periods` and `sealed_through` document that each sealed
  period advances it as it is defined, so replaying a period table reconstructs
  it exactly and a restart keeps every gap the seals closed. It was already true
  and already tested; an integrator had to read the source to confirm it, which
  is a documentation defect rather than a code one.

- The `0.5.0` watermark entry gained a ⚠️ **Changed** note stating the migration
  consequence directly — seal any period and every earlier date is closed —
  rather than leaving it to be inferred from the soundness reasoning.

## [0.5.0] — 2026-08-17

### ⚠️ The account-binding commitment changed

`account_binding_leaf` now covers the handle and the path and nothing else. The
classification, open window and balance limit are **out** of it. That moves two
vectors:

| Vector | before | after |
|---|---|---|
| Account binding commitment | `65ca7c50…` | `17d6ae22…` |
| Reference seal hash | `7f6f0218…` | `5dbfe84f…` |

The entry hash, the trial-balance leaf and every Merkle constant are
**unchanged**, as are all proof-path vectors. No schema change.

**Upgrading a ledger sealed under `0.4.0` or earlier.** Entries are untouched:
their content hashes did not move, so everything stays readable. Seals are
untouched too — `seal_hash` covers the `accounts` root as a *stored value*, not
as a recomputation, so old seals remain self-consistent and the chain still
verifies. `BalanceProof::verify_against` still holds, because the trial-balance
root did not move.

What does break is **naming**: `AccountBindingProof` is computed from
`account_binding_leaf`, so a proof built by `0.5.0` will not verify against an
`accounts` root recorded by `0.4.0`. `verify_naming` therefore fails for periods
sealed before the upgrade. There is no in-place repair — re-deriving those roots
would change every seal hash and break the chain — so a balance in an older
period is provable but not nameable. Periods sealed from `0.5.0` on are both.

### Fixed

- **A sealed balance became unnameable the moment anything about the accounts
  changed.** `Seal::accounts` exists so a trial-balance handle can be resolved to
  an account, and `BalanceProof::verify_naming` documents that as the complete
  claim an auditor wants — but the only way to build the second half was
  `AccountRegistry::prove_binding`, which proves against the registry *as it
  stands now*. Register one more account and every already-sealed balance stopped
  verifying. Silently: `verify_naming` returned `false`, which reads as "the
  books are wrong" rather than "you built the proof against the wrong registry".

  The deeper cause was that the binding leaf hashed the whole `Account`,
  including three fields the registry's own mutators exist to change. So
  `close()` and `set_limit()` — routine master data — also retroactively
  invalidated every binding proof against every seal ever issued. That made
  truncating the record list to `seal.accounts.size` an unsound workaround: it
  recovers the *set* of accounts at that size but not their master data as of
  then.

  Two changes. The leaf now covers only the handle and the path — the account's
  identity, and precisely the fields that never change, which is the line
  `AccountRegistry::restore` already drew ("the path is immutable and everything
  else is master data"). And `AccountRegistry::prove_binding_at(id, size)` proves
  against the commitment the registry had at a size, mirroring
  `MerkleLog::inclusion_proof_at`. Pass `seal.accounts.size` and the proof
  verifies under `seal.accounts`, whatever has happened since.

  `AccountBindingProof` now carries `id` and `path` instead of an
  `AccountRecord`, because a proof should carry exactly what it establishes —
  reading `closed_on` off a "verified" proof that never covered it is the
  opposite mistake. `account_binding_leaf` takes `(AccountId, &AccountPath)`.

- **Proving a sealed balance through a `LedgerStore` was impossible.** A seal
  commits to the closing balance folded by *booking date*; the trait only exposed
  `trial_balance(size)`, which folds by *log prefix*. Sealing March in April is
  the normal case, so at seal time the log already holds April entries and the
  two answers differ — meaning no caller could reconstruct the commitment a seal
  recorded, and the natural attempt produced one that silently did not match.

  `LedgerStore::trial_balance_through_date` is now part of the trait (it existed
  as a private method on both SQL backends), and `LedgerStore::prove_sealed_balance`
  is a provided method that does the whole recipe — find the seal, rebuild the
  closing balance the way the seal built it, **check the rebuild against the
  seal**, prove the row, prove the binding at `seal.accounts.size` — returning a
  `SealedBalance`. The middle step is the one that matters and the one nothing
  previously forced: skip it and you hold a proof against a commitment you
  computed yourself, which is internally consistent and evidence of nothing.
  A mismatch is now `SealedBalanceError::Restated` and no proof is returned.

- **A sealed closing balance could be restated afterwards by an ordinary
  booking.** This was a soundness defect, not a hardening opportunity: a seal
  claims its closing balances are exact — every entry booked on or before the
  period's last day and nothing else — and two entirely legal writes could
  falsify that claim while the seal, its balance proofs, its binding proofs and
  the whole chain went on verifying byte for byte.

  Both routes came from the same missing rule. Sealing March while February was
  still `Open` left February accepting postings that fold into March's
  cumulative closing balance; and a date that no period covered reported `Open`
  forever, so a booking into an undefined February did the same thing even when
  the calendar had never mentioned it.

  `PeriodCalendar` now carries a **sealed watermark** — the greatest end date
  among its sealed periods, maintained as they seal and never moving backwards.
  `state_on` consults it first, so every date at or before it reports `Sealed`
  whether or not a period covers it: a gap below a seal is not an opening to
  book through, it is a range already committed to. `PeriodCalendar::sealed_through`
  exposes it.

  `PeriodCalendar::check_sealable` is the new single home for the sealing
  preconditions — defined, `Closing`, every earlier defined period already
  sealed, and ending after the watermark. `Journal::seal_period` and both SQL
  backends call it instead of each re-implementing the first two checks and
  neither implementing the last two. The conformance suite fails a backend that
  seals out of order, so this is part of what a `LedgerStore` *is*.

- **A pruned log built proofs for the wrong entries.** The cold tier's protocol
  ends "only then may the operational store drop the rows", and the `LedgerStore`
  contract said in the same breath that entries "are never modified or removed".
  Both could not be true, and the consequence of resolving it in favour of the
  cold tier was undocumented and bad.

  Proofs are built from the leaves a store holds, so removing an archived prefix
  renumbers every leaf after it. The tree head does not notice — it is read from
  the last row's stored root — so head and proofs disagreed silently. Measured on
  a ten-entry log with the first five pruned: `prove_inclusion(7)` reported
  `IndexOutOfRange { index: 7, size: 5 }` for an index that is genuinely in
  range, and `prove_inclusion(3)` returned a proof for **log entry 8**, caught
  only if the caller verified before handing it to an auditor.

  Both SQL backends now check that the log they read back is dense from zero and
  return `LogNotDense` naming the hole — the same "checked, not trusted" the
  accumulator's subtree cover already got, applied to the leaf set it was
  missing from. The contract and the cold-tier protocol now agree with each
  other and say what pruning costs.

- **`open_items` was unbounded.** `page` and `statement` were both paged — "an
  account statement over ten years is not a response body" — while open items on
  the same account came back as one `Vec` of whatever size. That is the same
  hazard the crate names elsewhere as "the difference between a report and an
  outage", and a receivables control account is exactly where it bites.

  `LedgerStore::open_items` now takes a `PostingCursor` and returns an
  `OpenItemPage`, in the same log order and behind the same cursor as a
  statement — they are the filtered and unfiltered views of the same postings, so
  they now read alike. `Journal::open_items` stays unpaged, as
  `Journal::statement` does: an in-memory journal already holds everything.

  `OpenItem` gained `position: PostingPosition`, which is what the list is
  ordered by and what a page resumes after. `PostingPosition` moved from
  `storage` to `clearing`, beside `PostingRef` — the two are the ways of
  addressing a posting, and the distinction is load-bearing: a reference *names*
  one by entry identifier, a position *locates* it in the log.
  `StatementCursor` is renamed `PostingCursor`, since it now pages both.

- **Open items came back in entry-identifier order, not oldest first.**
  `ClearingRegister::open_items` sorted by `PostingRef`, which orders by entry
  **identifier**. Identity is caller-supplied — the engine never generates one on
  the deterministic path — so that ordering is chronological only when a caller
  happens to use `EntryId::generate()`, whose UUIDv7 values are time-ordered.
  Bring your own identifiers and the list silently reverses. FIFO clearing is
  what open items are *for*, so this was the wrong order, arrived at by an
  ordering the crate rejects everywhere else (`LogIndex`: "a wall clock is
  neither monotonic nor agreed between writers, and the index is both").

  The register now imposes no order — it returns candidates as supplied, because
  it knows nothing about the log and cannot honestly claim age — and the journal
  and both SQL backends supply them in log order. The conformance suite checks
  it, with a fixture carrying **descending** identifiers so the two orders are
  actually distinguishable; with `EntryId::generate()` throughout they coincide
  and a wrong implementation passes by luck.

- **Paging a statement silently dropped postings.** `StatementPage::next`
  handed back a `Cursor`, which addresses an *entry* — but a statement is a list
  of **postings**, and one entry may put several on the same account. A split
  receipt booked as three lines against one credit is an ordinary entry, so a
  page boundary can fall inside one. Resuming then asked for
  `log_index > after`, skipping every remaining posting of that entry:
  permanently, since the cursor had already moved past it, and invisibly, since
  the running balance stayed internally consistent across the gap.

  All three backends had it, and the "statement pagination is exact" conformance
  check missed it because its fixture never put two postings on one account —
  every boundary fell on an entry edge, so an entry-addressed cursor passed by
  luck. The check now seeds a three-posting entry *and* asserts that some entry
  really did contribute two adjacent lines, so it cannot quietly stop exercising
  the boundary.

  New `PostingPosition` (log index + posting index, ordered as the pair),
  `StatementCursor`, and `StatementLine::position()`.
  `LedgerStore::statement` takes a `StatementCursor`; `Cursor` still pages the
  log, where addressing an entry is right.

- **The reference implementation could not do what the storage trait could.**
  `prove_sealed_balance` landed on `LedgerStore` only, leaving `Journal` — which
  is the semantics a backend is *defined* to agree with — without it. It is now
  on both, and both call the same `SealedBalance::assemble`, so the recipe has
  one home rather than two that can drift. Same reasoning as
  `PeriodCalendar::check_sealable`.

- **A `SealedBalance` could not leave the process that built it.** Every part of
  it serialises — `Seal`, `BalanceProof`, `AccountBindingProof` — but the bundle
  did not, and the bundle is the thing an auditor is handed. It now derives
  serde under the `serde` feature. A seal edited on the wire still fails to
  deserialise at all, so a recipient who never calls `verify` cannot be fooled
  either.

- **`SealChain` did not notice a shrinking account registry.** Handles are dense
  positions and are never reissued, so a registry only grows; a seal committing
  to fewer bindings than its predecessor is one rebuilt from a truncated set,
  which renumbers the handles every earlier balance is keyed on. New
  `SealChainError::ShrunkenRegistry`. Tree-head monotonicity was already checked;
  this is its counterpart for the third root.

### Changed

- **⚠️ Sealing any period now closes every earlier date.** The watermark below is
  a soundness fix, but for an existing integration it is first of all a
  *behaviour* change, so it is repeated here: `state_on` consults
  `sealed_through` before the covering period, so a date at or before the
  greatest sealed end date is refused — **including one no period covers**.

  If you seal sparsely — annually, or only for audited years — bookings are now
  refused across ranges you never sealed, arriving as an ordinary
  `ValidationError::ClosedPeriod`. That is correct, since those dates fold into a
  sealed cumulative closing balance, but it is not something to discover from a
  rejected posting. Corrections into a closed range book into an open period
  carrying `original_booking_date`, as they always have.

- **`PeriodError` gained `NotClosing`, `SealedOutOfOrder` and
  `UnsealedPredecessor`;** `JournalError::PeriodNotClosing` and
  `JournalError::UnknownPeriod` are **removed**, as are the identical variants on
  `SqliteError` and `PostgresError`. Those were three copies of one rule that
  could drift apart — and did, in that none of them enforced ordering. They are
  now one `PeriodError` surfaced through the existing `Period(#[from] …)`
  variants. Match on `JournalError::Period(PeriodError::NotClosing { .. })`
  where you matched `JournalError::PeriodNotClosing { .. }`.

- **`SealedBalance` and `SealedBalanceError` live in `seal`, not `storage`.**
  They are seal artifacts — a seal plus two proofs — not persistence ones, and
  putting them in `storage` would have forced `Journal` to depend on the storage
  layer to offer the same operation. Re-exported from the crate root either way,
  so `doubleentry::SealedBalance` is unchanged.

- **`LedgerStore::Error` must now convert from `SealedBalanceError` and
  `AccountError`.** The bounds sit on the associated type rather than on
  `prove_sealed_balance`, because a `where` clause there made the method
  uncallable through a generic `S: LedgerStore<P>` — including from the
  conformance suite, which is the tell that it was the wrong place. Two
  `#[from]` variants on a `thiserror` enum satisfy it.

- **`SealChain::verify` is linear rather than quadratic.** The one-seal-per-period
  rule rescanned the whole prefix at every position, which is `O(n²)` in exactly
  the operation an auditor runs; the periods seen so far are now carried along.
  A ledger on daily periods passes 3,600 seals within a decade.

### Documentation

- `BalanceLimit::permits` claimed an overflow computing the net counts as a
  breach. It compares the gross totals directly and cannot overflow — the doc
  described an implementation that no longer existed.
- `closing_postings` now states that an account without an `AccountKind` is
  silently out of scope, since that is the way a close quietly does nothing.
- **A from-empty consistency proof is vacuous, and now says so.**
  `ConsistencyProof::is_vacuous` is new, and `verify` plus all four
  proof-building methods carry the warning. Every log extends the empty tree, so
  a proof taken at `old_size == 0` verifies against any root at the right size —
  correct mathematics, and a trap: an auditor who archived a head before the
  first entry gets `true` from a check that examined nothing, indistinguishable
  from a real verification. Documented at the call sites that build one, not only
  inside `verify`'s body where it was already noted.

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
apply `schema/sqlite.sql` or `schema/postgres.sql` as they now stand.

**Upgrading a ledger sealed under `0.3.0` or earlier.** Entries are readable —
the entry hash did not move. Seals are not: the sizes are new fields in the seal
preimage, so every seal issued before this release fails `is_self_consistent()`
under `0.4.0`, and the chain with it. Those seals cannot be repaired, only
superseded; the periods they covered stay auditable through the entries
themselves, which are unchanged and still provable against the log.

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
produced by `0.2.0` is invalid under `0.3.0`.

This is the widest of the breaks, because the **entry** hash moved. Every stored
`content_hash` written by `0.2.0` disagrees with what `0.3.0` computes, so
`adopt_verified` refuses the row and the entry becomes unreadable rather than
merely unprovable. A ledger holding `0.2.0` data cannot be carried forward by
upgrading; it has to be re-recorded, which re-hashes and re-seals it from the
source documents.

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
