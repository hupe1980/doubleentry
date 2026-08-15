+++
title = "Proofs"
description = "The append-only Merkle log behind RFC 6962-style inclusion and consistency proofs, and why a hash chain cannot do the same job."
weight = 6
+++

## The log

Entries are leaves in an append-only Merkle tree. The structure, proof
construction and proof verification follow the log described in
[RFC 6962](https://www.rfc-editor.org/rfc/rfc6962) and
[RFC 9162](https://www.rfc-editor.org/rfc/rfc9162) (Certificate Transparency),
with BLAKE3 in place of SHA-256 and a domain separation tag on every node.

Because the hash function differs, the published Certificate Transparency test
vectors do not apply — the golden vectors committed with the crate serve the same
purpose.

## Why a tree and not a chain

Two properties, and both are why this structure is used rather than a chained
hash over entries:

**Concurrency.** A leaf hash depends only on its own entry, so leaves can be
computed in parallel by unrelated writers. Only the sequencing step is ordered,
and it is cheap. A chained-hash log serialises every append by construction.

**Selective disclosure.** An inclusion proof establishes that one entry sits at
one index under a published root, in `O(log n)` hashes, *without revealing any
other entry*. A chain cannot do this — verifying one link means holding every
link before it.

## Inclusion

> This entry sits at this index under this root.

```rust
let head  = journal.head();
let proof = journal.prove_inclusion(recorded.require_index()?)?;

assert!(proof.verify(&recorded.content_hash, &head));
```

The path is logarithmic in the log size: proving one entry out of 1024 costs ten
hashes.

### Verify against a head, never a bare root

`verify` takes the whole tree head — size *and* root — and there is deliberately
no root-only form. The difference is not cosmetic: a proof's `leaf_index` and
`tree_size` *steer* the walk rather than being checked by it, and neighbouring
pairs steer it identically. The proof for leaf 1 of a two-leaf log folds exactly
the same way when relabelled as leaf 2 of three, so a bare-root check would
accept it under either name.

Nothing false about a real log follows — a root determines its own size, so short
of a collision no three-leaf tree has a two-leaf tree's root. But a verifier that
read the position back out of the proof would be reading a number the prover
chose. Pinning the size to a published head leaves exactly one labelling that
verifies, and costs one integer comparison.

The same holds at both ends of a consistency proof:

```rust
assert!(proof.verify(&snapshot, &later));
```

This is why a seal stores `trial_balance` and `accounts` as heads rather than
roots, and why `AccountRegistry::commitment()` returns one.

The boundary: a head has to come from somewhere the verifier already trusts. Hand
the walk a fabricated head that agrees with a forged proof and it is satisfied —
that is what "published head" means, and why publishing one to an auditor or an
append-only location is the step that makes any of this evidence.

## Proving against an archived head

A head published last quarter is the head an auditor actually holds. The log has
grown since, and its current root says nothing about theirs — so the proof has to
be built against *their* head:

```rust
let archived = journal.head_at(snapshot_size)?;
let proof    = journal.prove_inclusion_at(index, snapshot_size)?;

assert!(proof.verify(&entry.content_hash(), &archived));
```

`prove_consistency_between` is the same idea for two archived heads, neither of
them current: one is shown to be a prefix of the other without either party
learning the log's present size. Both are on `LedgerStore` as well, where
`head_at` is a row lookup rather than a replay — every entry stores the root as
of its own sequencing.

## Consistency

> The earlier log is a *prefix* of the later one.

Not merely "linked to" — provably append-only. Nothing was inserted, removed or
rewritten between the two snapshots.

```rust
let snapshot = journal.head();
// … more entries are recorded …
let later = journal.head();

let proof = journal.prove_consistency(snapshot.size)?;
assert!(proof.verify(&snapshot, &later));
```

This is what makes a published tree head useful. Publishing a head — to an
auditor, a timestamping service, or any append-only location — turns detectable
tampering into tampering detectable *by someone else*.

Consistency with the empty tree is a special case the verifier still checks: it
confirms the root it was handed *is* the empty root, so nobody can "prove"
consistency with a history that never existed.

## What is hashed

Every hash the engine produces is **domain-separated**: a length-prefixed tag
identifying what is being hashed is mixed in before the payload. Two different
structures can therefore never collide by encoding to the same bytes, and a value
hashed for one purpose is not a valid hash for another.

The bytes themselves come from a canonical encoding with exactly one
representation per value:

- Integers are little-endian and fixed width. No varints, no leading-zero ambiguity.
- Variable-length data carries a `u64` byte-length prefix, so no concatenation of
  two fields can be reinterpreted as a different split.
- `Option` is a `0x00`/`0x01` discriminant followed by the payload when set, so
  an absent value and an empty one are distinct.
- Sequences carry a `u64` element count, in an order the producer guarantees is
  canonical.

Lengths and counts are 64-bit rather than 32-bit so that no input can make one
saturate. A saturating prefix is worse than no prefix: it makes two distinct
values encode to the same bytes, which is exactly the collision the prefix is
there to rule out.

General-purpose serialisation formats are deliberately not used. Their map
ordering, integer widths and string escaping are free to change between versions,
which would silently change every hash in a ledger.

### The entry hash excludes the identifier

An entry's `content_hash` covers everything semantic — postings, dates,
description, provenance, dimensions, document reference — but **not** its
`EntryId`. The identifier is storage metadata, and two submissions of the same
logical transaction must hash identically for idempotency to be decidable.

Two entries cannot nonetheless collide: the idempotency key is inside the
preimage and unique across the ledger.

### Hashing your own documents

`Hash::digest` is public because you need it. `DocumentRef::new` takes the
content hash of a source document, and the alternative to offering a construction
is every caller inventing one — usually a bare SHA-256 with no domain separation,
which is exactly the mistake this design exists to avoid.

```rust
let document = DocumentRef::new(
    "INV-2026-0001",
    Hash::digest(b"acme/invoice/v1", pdf_bytes),
)?;
assert!(document.is_verifiable());
```

Pick a domain that names *your* document type. Tags beginning `doubleentry/` are
reserved for the engine, and reusing one would let a document hash be mistaken
for an entry hash.

## Cost

Appending is amortised `O(1)`: the log keeps the roots of the perfect subtrees
covering its leaves — one node per set bit in the size — and a new leaf merges
with any equal-sized subtree already on the stack. Reading the current root is
`O(log n)`.

Historical roots and proofs are derived from the stored leaves and cost `O(n)`.
They are audit-time operations, not write-path ones. The SQL backends are the
exception for `head_at`, which is a single indexed row read: every entry stores
the root as of its own sequencing, so a historical head never needs a replay.

A durable backend persists only the subtree stack (`MerkleAccumulator`), which
lets it answer "what is the root now" from `O(log n)` rows rather than rebuilding
the tree from every content hash it has ever stored. Restoring that stack checks
its *shape* against the claimed size rather than trusting it, so a row lost,
duplicated or returned out of order is caught rather than producing a
plausible-looking wrong root.
