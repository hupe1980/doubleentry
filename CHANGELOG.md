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

## [0.7.0] — 2026-08-26

### Accounts roll up: the report the tree exists for

`AccountPath` is a hierarchy and only leaves are postable, which is what makes a
*node's* balance a single defensible number: everything beneath it. There was no
way to ask for it.

```text
Assets                            1190.00
  Assets:Bank                     1000.00
    Assets:Bank:Main              1000.00
  Assets:Cash                      190.00
Income                           -1190.00
  Income:Sales                   -1190.00
```

- New `rollup` module — `Rollup`, `RollupNode` — plus `Journal::rollup`. Nodes
  come back in path order, which for segment-wise paths is depth-first pre-order,
  so indenting by `depth()` is the report.
- **Grouping nodes are inferred** from the paths that exist, so a chart that
  registers only leaves still produces a tree. That is grouping by path prefix,
  not an invented account: a registered node carries its handle and `AccountKind`,
  and `is_registered()` says which.
- Each node carries `own` (posted directly here) and `subtree` (here and below).
  Neither reconstructs the other, and both are gross-preserving.
- One `(currency, layer)` per report. A parent cannot hold the sum of its
  children in two currencies.
- `to_depth(n)` summarises without recomputing: a subtree total already carries
  everything beneath it.
- Deliberately not a `TrialBalance`, whose rows sum to zero — these include both
  a parent and its children. The *roots* do sum to the trial balance.

No backend method: the fold is pure over a trial balance and a registry, both of
which a `LedgerStore` already serves, so a report from a database and one from
memory are the same report.

### ⚠️ Balance reads fold by booking date or value date

`value_date` was stored on every entry, hashed into it, written to both schemas
and bounded by `LedgerPolicy::with_max_value_date_drift`. Nothing could read it
back.

An invoice booked on 28 March with value 2 April belongs to **March's books and
April's cash**, and checking a bank statement against a booking-dated fold is the
classic reconciliation mistake.

```rust
let march = BalanceQuery::between(date!(2026 - 03 - 01), date!(2026 - 03 - 31));
journal.balance(&cash, march)?;                  // the books
journal.balance(&cash, march.by_value_date())?;  // the cash
```

- New `DateBasis`, and `BalanceQuery::by_value_date` / `by_booking_date` /
  `basis`. Booking date remains the default.
- New `EntryDates` and `Entry::dates`. `BalanceQuery::includes_entry` takes an
  `EntryDates` rather than a bare `Date`, so the query picks the date it means
  instead of the caller picking — and two adjacent `Date` arguments cannot be
  transposed.
- `AssertAt::OnDate` gains a `basis` field; `BalanceAssertion::on_value_date`
  joins `on_date`.
- `StatementLine` gains `value_date`: a value-dated statement still has to say
  which period each line was booked in.

The basis is a **reporting** choice. Period membership, the sealed watermark and
what a seal commits to stay booking-dated — a settlement instruction must not be
able to reopen sealed books.

**Migration.** ⚠️ Both reference schemas add `entries_value_date`; an index on
one date and not the other makes half the queries a sequential scan. `AssertAt`
and `StatementLine` gained fields, so struct literals need them.

### The seal chain is checked against the account registry

A trial-balance leaf names its account by **handle**, so re-registering the same
paths in a different order repoints every sealed balance while every hash in the
chain still matches. `Seal::accounts` records the commitment that exposes it;
nothing compared the two on any routine path.

- New `SealChain::verify_against_accounts`, the registry half of
  `verify_against_log`. It compares each seal's `accounts` head against the
  registry's commitment *at the size the seal recorded* — the registry has grown
  since, so a comparison against the current one would fail for every seal but
  the last.
- New `AccountRegistry::commitment_at`. A registry only ever grows, so its
  history is exactly its own prefixes.
- New `SealChainError::AccountsRebound` and `SealChainError::BeyondTheRegistry`.
- `Journal::verify_seals` runs both halves, so `Journal::audit` covers it, and
  the conformance suite holds every backend to it.

### Recording costs the entry, not the chart of accounts

`Journal::record` cloned the whole maintained trial balance per entry so it could
apply the postings to a copy and discard it if a balance limit refused. That made
a booking cost `O(accounts × currencies × layers)`.

Balances are now applied in place with an exact undo — every key saved before it
is first touched, restored in reverse, and *absence* restored as absence, since a
trial balance with no row for a key says the account never moved.

Measured on a debug build: at five thousand accounts an append was 6.5× the cost
it was at fifty, and the curve does not flatten. The two are now within noise.
`tests/scale.rs` guards both axes a ledger grows on — a two-account fixture is
blind to the second.


### ⚠️ Proofs are `O(log n)` reads, and the log stores the tree

Every proof a SQL backend produced rebuilt the entire Merkle tree first. Both
backends read every content hash in the ledger, held them all at once, and folded
the whole thing — to answer a question whose answer is under a kilobyte.

At a hundred million entries that is roughly three gigabytes of memory per
`prove_inclusion` call. Not a slowdown: an outage. The crate's own documentation
called proofs "audit-time operations, not write-path ones", which was true about
*time* and quietly wrong about memory.

The in-memory path was no better in shape and worse in one specific place:
`TrialBalanceCommitment::prove_all` called an `O(n)` proof once per row, so
proving a whole trial balance was **quadratic** in the size of the chart of
accounts.

The fix is the one Certificate Transparency, Trillian and Go's `sumdb/tlog` all
use: store the interior nodes. A node whose subtree is complete can never change
in an append-only log, so it is written once and read back forever; only the
ragged right edge is folded at read time.

| Operation | Before | After |
|---|---|---|
| Inclusion / consistency proof | `O(n)` time **and memory** | `O(log n)` |
| Historical root or head | `O(n)` (memory) / one row (SQL) | `O(log n)` |
| `TrialBalanceCommitment::prove_all` | `O(n²)` | `O(n log n)` |
| Append | amortised `O(1)` | unchanged |
| Storage | 32 bytes per entry | `2n − popcount(n)` hashes — just under two per entry |

New `merkle::nodes` module: the numbering, and `RootPlan` / `InclusionPlan` /
`ConsistencyPlan`. Reading is split into *plan, fetch, assemble* because storage
is asynchronous and this crate is not — a plan names the positions, the caller
fetches them however it likes, and a pure fold produces the proof. **The
in-memory log and both SQL backends run the same two functions**, so a proof from
a database and a proof from memory are the same proof.

The numbering is Go `sumdb/tlog`'s `StoredHashIndex` exactly, so the on-disk
layout is one other transparency-log tooling already understands.

RFC 6962's `MTH`, `PATH` and `SUBPROOF` are kept, transcribed literally, as a
**test-only reference**, and the fast path is compared against them exhaustively
over every size and position up to 130 leaves. An optimisation with no
independent statement of what it is optimising is one nobody can audit.

`MerkleLog` now holds nodes rather than payloads:

- `leaves()` is **removed** — the payloads are not kept. `nodes()` returns the
  stored tree.
- `verify_incremental_state()` is **replaced by** `verify_structure()`, which
  checks that every interior node is the hash of the two beneath it. Strictly
  stronger, and it needs no payloads: a single altered node fails wherever it
  sits, rather than only being visible as a changed root.
- `MerkleAccumulator` gains `push_recording`, which returns the nodes an append
  completed. It is no longer a *storage* format — a backend that keeps its tree
  reads the subtree cover back in `O(log n)` — but it is still how the write side
  gets those nodes without reading anything.

**Migration.** ⚠️ Both reference schemas change. `log_subtrees` is replaced by
`log_nodes (node_index, node)`, and `entries.tree_root` is **dropped**: a
historical head is now the same `O(log n)` lookup, so the column was a second
copy of a derived value, and two fields that must agree are two fields that can
disagree. The project is pre-1.0 and this is a hard cut — recreate the database.

### Pruning an archived prefix no longer costs provability

A consequence of the above, large enough to state on its own.

The cold tier's protocol ends "only then may the operational store drop the
rows", and doing so used to break the hot store's proofs completely: the tree was
derived from the stored leaves, so a hole renumbered every leaf after it.
`prove_inclusion` returned a proof for a *different* entry while the head went on
reporting the truth. The backends had to detect the hole and refuse
(`LogNotDense`), which was the best available answer and not a good one.

The tree now lives in `log_nodes`, apart from `entries`. Archiving a prefix
leaves it untouched: the log keeps its length and its root, and inclusion and
consistency proofs keep working *across the archived range*. The proof comes from
the hot store and the leaf it is checked against comes from the archive — which
is the arrangement an archive is for.

What pruning still costs is the entry bodies: `get` stops finding them, a
statement stops listing them. Those answers live in the archive.

`LogNotDense` is **removed** from both backend error types; the condition it
named is no longer reachable. Two new variants replace it where genuine
corruption is possible: `MissingNode` (a node position the store cannot produce —
nodes are never updated or deleted, so this means a partial restore) and
`PartialLog` (a node count no whole number of records produces).

`Seal::build` now has a durable counterpart, `Seal::from_parts`, for a backend
that reads its head and consistency proof out of storage rather than deriving
them from a log it holds. It **checks** the parts rather than trusting them — the
proof has to verify against the predecessor's head and this one before anything
is hashed — and both constructors share one rule with `SealChain::verify`, so a
seal that could never chain is never written. Both now return
`Result<_, SealChainError>`.

### ⚠️ A seal chain now proves the log was only appended to

`SealChain::verify` checked that seals hash to their own contents, name their
predecessor, grow monotonically, name one ledger, and seal each period once. All
of that is about the *seals*, and none of it looks at the entries underneath.

Two seals claiming tree sizes 100 and 200 with **entirely unrelated roots**
satisfied every one of those rules. `prev_seal` matches, both hashes are
self-consistent, sizes grow, the registry grows. A log rebuilt from scratch
between two closes verified byte for byte — which is precisely the alteration a
seal exists to expose. The crate had `ConsistencyProof` sitting in the next module
and never applied it to the chain.

`Seal` now carries `prev_consistency`: a proof that the predecessor's log is a
**prefix** of this one's, inside the seal preimage so it cannot be swapped for one
relating a different pair of trees. `SealChain::verify` checks it, turning *"these
commitments are in order"* into *"this history was only ever appended to"* — and
it does so **offline**, for a recipient holding the seals and no database.

`None` is admissible in exactly two places, both pinned rather than tolerated: the
genesis seal, and one following a predecessor whose tree was empty (no proof from
the empty tree exists, so its root is checked against the empty root instead).
Anything else is `SealChainError::MissingConsistency`. The field cannot be dropped
to make a rewritten history verify.

Also new: `SealChain::verify_against_log`, which additionally recomputes each
seal's tree head from a log and compares. `verify` alone is satisfied by any
internally consistent chain, *including one over a history nobody holds*;
`Journal::verify_seals` now runs the stronger form, and the conformance suite
rebuilds the log from `page()` and holds every backend to it.

`Seal::build` takes `&MerkleLog` and `Option<&Seal>` instead of a `TreeHead` and
an `Option<Hash>`, and returns `Result`. Both changes exist so the proof and the
head it relates cannot come from different trees.

**Migration.** ⚠️ The seal preimage gained a field, so **every `seal_hash`
changes**. Both reference schemas gain a `prev_consistency` BLOB/BYTEA column on
`seals`. An existing ledger cannot be migrated by adding the column: seals written
before this carry no proof, so the chain will refuse them at the first link. The
project is pre-1.0 and this is a hard cut — re-seal from a fresh chain, or stay on
0.6. Cost is `O(log n)` hashes per seal; a billion-entry log adds under a kilobyte
to each.

### ⚠️ A consistency proof from the empty tree is refused, not returned

`consistency_proof(0)` used to return a proof, and that proof verified. Correctly,
too — every log extends the empty tree — and that was the trap. The statement it
makes is *true of every history that has ever existed*, so it constrains neither
the new root nor anything else. An auditor who archived a head **before the first
entry** got `true` back from a check that examined nothing, indistinguishable from
a genuine verification unless they knew to ask.

The previous release documented the trap on `ConsistencyProof::is_vacuous` and
left the behaviour. Documenting a footgun the crate could refuse is not this
crate's standard: `Entry` refuses to deserialise as `Balanced`, `Seal` re-checks
its hash on the way in, `InclusionProof::verify` takes a head rather than a root.

So it is refused at both ends. `MerkleLog::consistency_proof(0)` and
`consistency_proof_between(0, _)` return `ProofError::EmptyOldTree`, and
`ConsistencyProof::verify` returns `false` for `old_size == 0` — both, because the
fields are public and a proof arrives over a wire. A `true` from `verify` now
always means something was checked.

This is what the specifications do.
[RFC 9162 §2.1.4.2](https://www.rfc-editor.org/rfc/rfc9162#section-2.1.4.2)
defines consistency only for `0 < m ≤ n`, and Go's `tlog.ProveTree` refuses
`m < 1`.

`ConsistencyProof::is_vacuous` is **removed**. With construction and verification
both refusing, it had nothing left to warn about.

`ConsistencyProof` also gains a canonical encoding — `Canonical` plus
`from_canonical_bytes` — because a seal now carries one and a backend has to store
and read it back. The decode is strict: trailing bytes, a truncated path, or a
declared count the buffer cannot satisfy are all refused.

**Migration.** Archive tree heads with at least one entry in them. Code calling
`consistency_proof(0)` was getting an answer that meant nothing and now gets an
error saying so.

### Witness cosigning: closing the split view

Every guarantee this crate makes is *relative to a tree head*. Inclusion is
against a head, consistency is between two heads, a seal commits to a head. So an
operator who can serve two heads can serve two histories — head `H₁` to the tax
authority, `H₂` to the bank — with a complete, internally consistent log behind
each. Every proof either party checks verifies, because nothing is wrong *inside*
either view. No proof can detect this; each party only ever sees one side.

The [proofs guide](https://hupe1980.github.io/doubleentry/docs/proofs/) has always
ended on the admission — *"a head has to come from somewhere the verifier already
trusts"* — and offered nothing to get there. `doubleentry::witness` is that
somewhere.

A witness holds one row per log (origin, size, root) and applies one rule above
all others: **it will not vouch for a second history at a size it has already
vouched for.** Offered a head at a known size with a different root, it returns
`WitnessError::Fork` and never signs. Growth must be *proven*, not asserted —
a later head without a verifying `ConsistencyProof` is refused.

```rust
let mut witness = Witness::new(MemoryWitnessStore::new());
witness.trust(origin.clone(), archived_head)?;          // out of band, deliberately

assert!(matches!(
    witness.offer(&origin, forked_head, None),
    Err(WitnessError::Fork { .. })
));
```

Two decisions worth stating, because both were live:

**No trust-on-first-use.** `Witness::trust` refuses a log it already tracks. A
witness that adopted whatever head it was shown first could be introduced to the
fork as easily as to the log, by the same operator, and would cosign the fork with
complete sincerity. Re-pointing an existing witness is exactly how a fork would be
laundered, so calling the setup function twice will not do it.

**No HTTP.** [C2SP `tlog-witness`](https://c2sp.org/tlog-witness) puts this behind
`POST /add-checkpoint`; `AddCheckpoint` gives you that request and response as
bytes and stops there. Shipping an async runtime and a TLS stack to save fifteen
lines would put both in the dependency tree of every user who never touches a
witness. `WitnessError` documents its status code per variant.

The state machine needs no cryptography and is always available. The new
`witness` **feature** adds Ed25519 keys — `ed25519-dalek` and `sha2` — so a
decision becomes a signature a third party can check. `Cosigner::cosign` runs the
state machine first and signs only if it returns: a refused head produces an error
and no bytes at all.

The wire format is [C2SP `signed-note`](https://c2sp.org/signed-note) around a
[`tlog-checkpoint`](https://c2sp.org/tlog-checkpoint) body with
[`tlog-cosignature`](https://c2sp.org/tlog-cosignature) signatures — what
Certificate Transparency, the Go checksum database, Sigsum and sigstore speak.
Not decoration: a witness is only worth having if it is *somebody else's*, and a
bespoke format could only ever be cosigned by software this crate ships. Key
hashes use SHA-256 rather than the BLAKE3 used everywhere else here, because
interoperating is the whole point and a substitution would silently disagree with
every existing implementation.

That claim is checked rather than asserted. `tests/witness.rs` pins the exact
bytes of a note, and those bytes were cross-checked against Go's
`crypto/ed25519` and the key-hash construction in `golang.org/x/mod/sumdb/note`:
the derived public key matches, both key hashes match, and `ed25519.Verify`
accepts the cosignature this crate produced. Extension lines are read and
rendered back verbatim — never written, since the specification calls them NOT
RECOMMENDED — because dropping them would render a body nobody signed and
refusing them would mean refusing to cosign for any log that uses one.

The engine still reads no clock. A cosignature covers a timestamp, so the
timestamp is an **argument**; `ed25519-dalek` is pulled in with `rand_core` off so
that holds by construction rather than by discipline.

**Migration.** Purely additive. Nothing to do.

### `Journal::audit()` — one call for "is everything alright"

`verify_log`, `verify_balances`, `verify_balanced` and `verify_seals` are all
public and all worth calling. Knowing to call *all four* was left to the reader,
and getting it wrong is silent: a caller who checks the log and not the seals has
verified something real and concluded something broader.

```rust
let report = journal.audit();
assert!(report.passed(), "{report}");
```

All four run — the first failure is not necessarily the most informative — and a
failing report names which check and why. `AuditReport` and `AuditCheck` are
deliberately the same shape as the conformance suite's `Report` and
`CheckResult`: both answer "did this hold up", and a reader who has met one
should not have to learn the other.

### `save_checkpoint` no longer goes backwards

`LedgerStore::load_checkpoint` was documented as returning "the most recent
checkpoint", and all three backends did an unconditional upsert. Saving an
*earlier* checkpoint silently replaced a later one, and two concurrent writers
left whichever arrived last.

A checkpoint is a cache for a fold over the journal, so that is not a write, it is
a loss: the next reader re-folds from further back. The upsert is now conditional
on the tree size not going backwards, in `MemoryStore`, SQLite and PostgreSQL
alike, which makes it idempotent and order-independent — two writers racing leave
the same row whichever wins. That is what lets a caller checkpoint from more than
one place without coordinating.

Rule 17 of the `LedgerStore` contract, and the conformance suite now checks it.

**Migration.** No schema change. Behaviour only.

### `head_at` names the real log size when it refuses

Both SQL backends reported `ProofError::SizeOutOfRange { size: 0 }` for a head
they could not produce, whatever the log actually held. The error is read by
whoever was told their archived head could not be reproduced, and `size: 0`
against a populated log sends them looking at their own records rather than at the
prefix the store is missing. It now reports the real length, at the cost of one
round trip on a path that has already failed.

### ⚠️ Entries must balance per currency **and layer**

`Entry::seal` totalled debits and credits per currency alone. Every balance the
engine reports — `TrialBalance`, `balance`, a statement, the trial-balance root a
seal commits to — is keyed on `(account, currency, layer)`, and
`verify_balanced` checks each currency and layer independently. Validation did
not.

So this sealed, recorded, and persisted cleanly:

```rust
Entry::new(id, key, on)
    .post(Posting::debit(cash, Eur::parse("50.00")?, EUR).in_layer(Layer::Pending))
    .post(Posting::credit(revenue, Eur::parse("50.00")?, EUR))   // settled
```

It nets to zero in EUR and leaves **both** layers permanently out of balance.
`journal.verify_balanced()` returns `false` from then on, for good: the log is
append-only, every posting in the entry looks ordinary, and nothing downstream
points at it. One legal-looking booking falsified the engine's own consistency
check.

`ValidationError::Unbalanced` and `ValidationError::Overflow` now carry a
`layer`, and an entry that only balances across layers is refused naming both.
PostgreSQL's deferred trigger groups by `(currency, layer)` to match; the old
`postings_balance_per_currency()` function is dropped by name during migration so
a database created earlier cannot keep enforcing the weaker rule alongside it.

**Migration.** No hash, encoding or column changes, so existing rows are
unaffected and re-reading them is not required. Run `migrate` to replace the
trigger. Then check for entries already recorded that only balance across
layers — `journal.verify_balanced()`, or in SQL:

```sql
SELECT entry_id FROM postings GROUP BY entry_id, currency, layer
HAVING SUM(CASE WHEN direction = 'D' THEN amount_minor ELSE -amount_minor END) <> 0;
```

Any row it returns has to be offset by a compensating entry; nothing can remove
it. Code that split a hold and its settlement across two entries to work around
the old rule can now book them as one atomic entry, which is what the layer was
for.

### ⚠️ A leaf that has been posted to can no longer acquire a child

"Only leaves are postable" was checked when an entry was validated and nowhere
else, so it held in one direction only. Registering `Assets:Cash:Petty` after
`Assets:Cash` had been posted to turned the parent into an aggregation node —
and then *every* later entry on it was refused, **including the reversal that
would have corrected it**. The balance already sitting there could never be
moved again. The account was bricked, silently, by an ordinary master-data
change.

`AccountRegistry` cannot make that check: it holds no postings. So registration
moved to where the postings are.

- `Journal::accounts_mut()` is **gone**. In its place:
  `register_account`, `register_path`, `restore_account`, `close_account`,
  `reopen_account`, `set_account_limit` — all returning `JournalError`.
  `journal.accounts_mut().register_path(p, d)?` becomes
  `journal.register_path(p, d)?`; the rest rename likewise. `accounts()` is
  unchanged.
- New `JournalError::AncestorHasPostings`, and the same variant on
  `SqliteError` and `PostgresError`, raised by `LedgerStore::register_account`.
- New conformance check, `check_a_posted_leaf_cannot_gain_a_child`, so every
  backend is held to it. `check_all` now runs **21** checks.

`AccountRegistry` keeps its own `register`/`close`/`set_limit` for building a
registry outside a journal — for a `SealContext`, or for a backend rehydrating
one. Those cannot see postings and do not pretend to.

**Migration.** Rename the call sites. No schema or hash change. A ledger that
already contains a posted-to node keeps working exactly as before — the rule
governs registrations from now on, like every other master-data rule here.

### ⚠️ A balance limit now folds both layers, so a reservation counts against it

The limit was checked per `(account, currency, layer)`, against a pending balance
that starts at zero. That is neither of the two things it could sensibly mean,
and it failed in both directions at once.

A cash box limited to `NoCreditBalance` could not reserve an outflow **at all** —
the pending layer had nothing in it, so the first pending credit crossed zero and
was refused. The limit did not constrain reservations; it forbade them, which is
the one thing the pending layer exists for. And in the other direction the two
layers never met: whatever a limited account held in one had no bearing on the
other, so a hold and a settled draw could each fit while together exceeding the
funds.

Both are now one question about the same money.
`BalanceLimit::headroom_minor(settled, pending)` returns the room left in minor
units — negative when breached — folding the layers **asymmetrically**: a pending
net working against the limit consumes room, one working for it grants none. An
outflow already promised is gone; an inflow merely reserved has not arrived.
Discharging a reservation nets its pending side to zero and the room returns.

- `BalanceLimit::permits` takes `(&settled, &pending)` rather than one balance.
- `JournalError::LimitBreached`, `SqliteError::LimitBreached` and
  `PostgresError::LimitBreached` drop `layer` and `net_minor` for a single
  `headroom_minor: i128` — negative, magnitude equal to the shortfall. A breach
  is no longer attributable to a layer, because the check is not per layer.
- Both SQL backends read the settled and pending totals in one pass and call
  `headroom_minor`, so a rule with this much subtlety in it has exactly one
  implementation.
- The conformance check for limits now covers a reservation against an empty
  account, a reservation within the funds, and a settled draw that ignores an
  outstanding hold.

**Migration.** Rename the error fields at any call site that matched on them.
Nothing stored changes. Behaviour changes in two ways and both are the point:
reservations against limited accounts that were previously refused now succeed
when the funds are there, and settled draws that previously ignored an
outstanding hold are now refused. If your application was maintaining its own
"available balance" to work around the old rule, it can stop.

### ⚠️ Dimensions can be reported on

Dimensions were **write-only**. You could attach them to a posting, they were
hashed into the entry, stored in `posting_dimensions`, and rehydrated on read —
and there was no API anywhere in the crate that could filter, group or total by
one. The index `(axis, value)` the reference schema ships for exactly this was
never queried by anything.

The docs said otherwise in three places: "the axes reporting slices by", "lets a
trial balance group by any axis without restructuring the tree", and a note that
`WHERE axis = 'activity' AND value = 'Network'` "is an index scan here". The
first two described an API that did not exist; the third described an index
nothing used.

- **`DimensionFilter`** — a conjunction of clauses, in `dimensions`:
  `matching(axis, value)` requires an equality, `missing(axis)` requires the axis
  to be absent.

  `missing` is not an afterthought. Slice by the values an axis takes and the
  totals do not add up to the trial balance, because unattributed postings land
  in none of the slices — silently, which is the worst way for a report to be
  wrong. `LedgerPolicy::requiring` stops that happening going forward; `missing`
  is how you find what slipped through before you turned it on.

- **`Journal::dimension_values`** and **`LedgerStore::dimension_values`** — the
  values an axis actually takes. The values are the caller's and nothing else
  knows which are in use, so a report cannot slice by an axis without this.

- **Both SQL backends turn an equality clause into an inner join** on
  `posting_dimensions`, so the planner drives from `(axis, value)` rather than
  testing every posting. It cannot duplicate a posting, because
  `(entry_id, posting_index, axis)` is the primary key. An absence clause has no
  row to join to and stays a `NOT EXISTS`.

- **New conformance check**, `check_balances_slice_by_dimension`, covering an
  equality, an absence, and that the two partition the account.

### ⚠️ One `BalanceQuery` replaces the prefix and date read methods

Adding dimensional filtering to `trial_balance(size)` and
`trial_balance_through_date(end)` would have produced a method per combination,
on the journal and on the trait and in three backends. They are the same fold
with different predicates, so they are now one:

```rust
journal.trial_balance(BalanceQuery::all())?;
journal.trial_balance(BalanceQuery::over_prefix(1_204))?;
journal.trial_balance(BalanceQuery::through(date!(2026 - 03 - 31)))?;
journal.trial_balance(BalanceQuery::between(a, b).matching(&filter))?;
```

- `Journal::trial_balance`, `Journal::balance`, `LedgerStore::trial_balance`,
  `LedgerStore::balance` and `LedgerStore::balances` take a `BalanceQuery`
  instead of `Option<u64>`.
- `Journal::trial_balance_through_date`, `Journal::balance_on_date` and
  `LedgerStore::trial_balance_through_date` are **gone** — they are
  `BalanceQuery::through(…)`. The trait loses a method and gains two.
- `BalanceQuery::between` is new and fills a real hole: there was no way to ask
  for a period's **activity** at all, only for the cumulative position at a date.
  That is the difference between an income statement and a balance sheet, and
  the workaround was to fold twice and subtract.
- `AssertAt::query()` bridges the serialisable assertion form to the query every
  read takes. `AssertAt` itself is unchanged — an assertion travels, and a
  borrowed filter cannot.

`BalanceQuery::all()` is a fast path: the journal serves its maintained totals
rather than re-folding, exactly as `None` did.

**Migration.** Mechanical. `None` → `BalanceQuery::all()`, `Some(n)` →
`BalanceQuery::over_prefix(n)`, `trial_balance_through_date(d)` →
`trial_balance(BalanceQuery::through(d))`, `balance_on_date(&k, d)` →
`balance(&k, BalanceQuery::through(d))`. No schema or hash change.

### ⚠️ Statements scope to a period, and report what they open at

`statement` listed an account's whole history and nothing else. A statement for
one period — the most ordinary ledger read there is — meant paging every posting
ever made on the account and filtering in the caller.

- `Journal::statement` and `LedgerStore::statement` take a `BalanceQuery`, so
  "this account, March, this reporting axis" is one query.
- **`StatementPage::opening`** is new: the balance the page was entered with.
  Both SQL backends already computed it to page correctly and threw it away, so
  a caller wanting an opening balance issued a second query for a number the
  first one already had.

  It is two disjoint folds — everything the query narrows to that was booked
  **before** its window, plus the lines the earlier pages already showed. So
  `opening` plus the page's movements is the last line's `running`.

  The first fold is by **date**, not by log position, and that is the subtle
  part. Entries are appended in recording order, so bounding the carry set by
  position folds a later-dated entry into the middle of the statement and leaves
  a backdated one out of the opening — both from ordinary bookings, and the
  identity above then fails for any ledger that has ever taken one.
  `BalanceQuery::opening()` is what it is folded over, and
  `Journal::statement_opening` takes the cursor the page resumes from.

- New conformance checks: `check_statements_scope_to_a_period`, whose fixture is
  booked out of date order on purpose and which pages a statement one line at a
  time, and `check_queries_fold_by_value_date`. The first found a real bug in the
  SQLite backend on its first run — see below.

**Migration.** `store.statement(key, cursor)` →
`store.statement(key, BalanceQuery::all(), cursor)`, and `StatementPage` gained
a field, so a struct literal needs `opening`.

### Fixed

- **The SQLite query builder mis-bound every parameter after a reused
  placeholder.** SQLite reads a bare `?` positionally: every *occurrence* is a
  separate parameter, not a second reference to the same one. The generated
  statement query names its cursor bound twice — `(index > x OR (index = x AND
  …))` — so from that point on every value was bound one position early, and the
  database reported a datatype mismatch rather than a wrong answer.

  Placeholders are now numbered (`?1`, `?2`, …). PostgreSQL was unaffected: `$n`
  is a reference, not a position. Caught by the new conformance check, which is
  the argument for the suite in one line.

### Added

- **`LedgerStore::get_by_key` and `Journal::get_by_key`.** The lookup the
  idempotency key exists for. The guidance is to derive a key from the source
  transaction — a message id, an external document reference, a `(run, line)`
  pair from a batch import — and the question that follows is "did we already
  book message X?". There was no way to ask it: the only route was to rebuild the
  whole entry and re-submit it, which answers by writing. Backed by the same
  unique index that makes the append idempotent, so it is a lookup rather than a
  scan. `Journal::index_of_key` returns just the position.

- **`Amount::checked_mul_ratio`, `checked_mul_int`, `rescale`, and `Rounding`.**
  The gap this fills was the crate's largest: there was no way to multiply an
  amount by a rate. `net × 19 / 100` for VAT, `principal × days / 365` for
  interest, `eur × rate / 10⁶` for a conversion — every one of them forced a
  caller down to `to_minor()` and raw `i64` arithmetic, in the one place the type
  exists to protect. The intermediate product overflows an `i64` long before
  either operand is unreasonable, so the workaround was not merely inelegant.

  `checked_mul_ratio` is exact in `i128` and only the result has to fit.
  `Rounding` has seven modes named after `java.math.RoundingMode` and Python's
  `decimal`, so a rule written against a specification in either transcribes
  rather than translates. The mode is a required argument: there is no answer
  that is right everywhere, and choosing one silently is how a ledger drifts.

  `rescale::<Q>` restates an amount at another precision — exact when widening,
  rounded when narrowing. Paired with `checked_mul_ratio` it is what makes a
  conversion at a published rate expressible: parse the rate at the precision it
  was published in, apply it, restate the result in the scale the receiving books
  are kept in. The engine still does not convert currencies; it makes the
  arithmetic in the middle exact and total.

  `MoneyError::DivideByZero` is new.

### Changed

- **`SealChain` indexes its periods by position, so `get` is a lookup.** It was
  a linear scan over every seal. A ledger on daily periods reaches thousands
  within a decade, and `get` is on the path `prove_sealed_balance` takes.
  `check_link` now takes a predicate rather than a set, so appending consults the
  index it maintains while verification rebuilds one — verification that trusted
  the cache would only be checking that the cache agreed with itself.

- ⚠️ **`log_subtrees.position` is dropped from both schemas.** The heights in a
  Merkle cover are one per set bit in the log size: distinct, strictly
  descending, and already the primary key. `ORDER BY height DESC` is the order
  the accumulator folds in, so the column was a second thing that had to agree
  with the first — exactly what the `checkpoints` comment in the same file argues
  against. A `height BETWEEN 0 AND 63` check replaces it.

  **Migration.** `ALTER TABLE log_subtrees DROP COLUMN position;` — or drop and
  let the next append rebuild it, since the table is derived state.

- ⚠️ **A period entirely at or below the sealed watermark can no longer be
  defined.** Its dates are already frozen, so nothing could ever be booked into
  it and `check_sealable` would refuse it forever — leaving a period whose own
  state said `open` while `state_on` said `sealed` for every date it covered.
  New `PeriodError::DefinedBelowWatermark`.

  A period arriving already **sealed** is still admitted, in any order: that is
  the replay path a durable backend takes, and a sealed period below the
  watermark is exactly what a sealed history looks like. The refusal is for the
  unreachable case — `check_sealable` will not seal a period while an earlier one
  is open, so an unsealed period below the watermark can only have been defined
  after the fact.

- ⚠️ **`PeriodCalendar::ensure` refuses to reopen a sealed period.** `transition`
  refuses `Sealed -> Open` outright; `ensure` is the replay path and inserted
  whatever state it was handed, so a stale row arrived at the same place by the
  back door. The watermark meant nothing could actually be booked, but the
  period's own state was wrong. Now `PeriodError::InvalidTransition`.

- **The SQLite backend reads `log_index` as nullable on the replay path.** It is
  nullable in the schema and this backend always assigns it inline, so the read
  never failed — but a column that *can* be NULL read as if it cannot turns a
  stale row into a decode error rather than the honest answer.

- **`TrialBalanceCommitment::prove` binary-searches the committed rows** instead
  of scanning them. Rows come out of a `TrialBalance` in key order, so proving a
  hundred accounts out of a hundred thousand costs `O(k log n)`.

- **The three hand-rolled hex and base64 decoders use `as_chunks` rather than
  `chunks_exact`.** A fixed-size chunk destructures irrefutably, so each loop
  loses an error arm that could not be reached, and `IdempotencyKey::parse_hex`
  and `base64::decode` read their length rules off the split instead of
  duplicating them in a separate test.

- **`just lint`, `fmt-check`, `test`, `doc` and `package` run on `stable`.**
  `rust-toolchain.toml` pins the MSRV, which is right for compiling and wrong for
  linting: clippy gains lints with every release, so those recipes checked an
  older rulebook than the CI jobs they mirror and passed locally while CI failed.
  `just msrv` stays the one lane that deliberately uses the pin.

- **The merkle property tests no longer reject two thirds of what they
  generate.** Sizes and indices were drawn from independent ranges and filtered
  with `prop_assume!`, so the tests explored a fraction of the cases their count
  suggested — and proptest abandons a run once its global reject budget is spent,
  so raising the count made the suite stop running instead. They are generated
  together now, and the suite is clean at 4 000 cases.

- **The simulation gained a straddling-value-date operation** and two invariants
  re-checked after every step: that a rollup redistributes the trial balance
  without changing it, and that a statement opens where the window before it
  closed, on both date bases.

### Documentation

- **The stated reason for the leaf rule was wrong.** Six places said posting to
  both a node and its descendants "makes every rollup double-count, and no
  reporting layer can repair it". It does not: `descendants_of` includes the node
  itself, so a subtree rollup that sums the node's own postings alongside its
  children's is the correct total. The real defect is ambiguity — an account that
  is both a bucket and a container has two defensible balances, its own postings
  and its subtree's, and every report has to pick one silently — and, now stated
  where it belongs, that such an account cannot correct itself.

- **The `LedgerStore` contract said the conformance suite "checks each one".** It
  cannot check the first. "Append-only" is a claim about everything a backend
  does *not* do, over all future time, and a suite only observes what it can
  provoke. Both the trait docs and the persistence guide now say so, name the
  consequences the suite *does* reach, and point at the `GRANT` that turns the
  convention into a property.

- **The contract list in the trait docs had drifted from the guide** — nine items
  against twelve. Both are now the same seventeen, including the balance-limit
  rule, the sealed-balance rule, the archived-head rule, the new leaf rule, the
  key lookup, dimensional slicing, the scoped statement and the checkpoint rule.

- **The pending/settled guide** gains the whole two-phase story: why each layer
  balances on its own, the hold-and-discharge shape that makes expressible as one
  atomic entry, and how a reservation interacts with a balance limit.

- **`money.md`** gains a section on rates, rounding modes and crossing scales,
  and `accounts.md` a worked account of what a reservation does to headroom —
  including both wrong designs and how each fails.

- **`dimensions.md`** gains the reporting half it always described and never
  had, `statements.md` is rewritten around `BalanceQuery`, and the README gains
  a reporting section. A new **`reporting.md`** covers the account rollup. The
  conformance suite is now **25** checks, stated consistently in all three places
  that count it.

- **`accounts.md` states what master data costs you.** An account's
  classification, open window and limit are outside every commitment for reasons
  the page explained at length — and it never said the consequence: the ledger
  records their *current* state, not their history. Nothing says when an account
  closed or who closed it, and none of it is tamper-evident.

- **`open-items.md` states what it does not do.** An open-item list takes no
  `BalanceQuery`, because a residual is not a filtered posting: "what was open
  as at 31 March" needs the clearings replayed to that date. The events are on
  file and carry their dates; the ageing buckets are the caller's.

- The changelog's own `[Unreleased]` heading was never renamed when `v0.6.0` was
  tagged, and the compare links for `0.5.0` and `0.6.0` were missing. Both fixed.

## [0.6.0] — 2026-08-17

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

[Unreleased]: https://github.com/hupe1980/doubleentry/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/hupe1980/doubleentry/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/hupe1980/doubleentry/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/hupe1980/doubleentry/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/hupe1980/doubleentry/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/hupe1980/doubleentry/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/hupe1980/doubleentry/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/hupe1980/doubleentry/releases/tag/v0.1.0
