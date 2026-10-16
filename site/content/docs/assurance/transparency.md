+++
title = "Transparency"
description = "Signed C2SP checkpoints, stock witnesses, policies, the auditor's Verifier, tiles and offline proof bundles."
weight = 3
+++

## What to call

The log side (the operator):

- [`transparency::LogKey`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.LogKey.html) — [`from_seed`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.LogKey.html#method.from_seed), [`vkey`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.LogKey.html#method.vkey), [`sign`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.LogKey.html#method.sign).
- [`Journal::checkpoint`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.checkpoint) / [`Ledger::checkpoint`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.checkpoint) → [`SignedCheckpoint`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.SignedCheckpoint.html).
- [`WitnessSession`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.WitnessSession.html), [`AddCheckpoint`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.AddCheckpoint.html), [`WitnessReply::read`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.WitnessReply.html#method.read), [`SignedCheckpoint::with_cosignature`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.SignedCheckpoint.html#method.with_cosignature).
- [`Journal::tile`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.tile), [`Journal::tile_entries`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.tile_entries), [`TileId`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.TileId.html).
- [`Journal::bundle_entry`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.bundle_entry), [`Journal::bundle_balance`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.bundle_balance), [`Journal::bundle_record`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.bundle_record) → [`ProofBundle`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.ProofBundle.html).

The auditor side:

- [`Policy`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Policy.html) — [`k_of_n`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Policy.html#method.k_of_n), [`builder`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Policy.html#method.builder), [`parse`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Policy.html#method.parse), [`verify`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Policy.html#method.verify).
- [`Verifier`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html) — [`update`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html#method.update), [`verify`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html#method.verify), [`pin`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html#method.pin), [`heads`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html#method.heads).
- [`ProofBundle::parse`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.ProofBundle.html#method.parse), [`ProofBundle::verify`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.ProofBundle.html#method.verify) → [`Proven`](https://docs.rs/doubleentry/latest/doubleentry/transparency/enum.Proven.html).
- [`transparency::Error`](https://docs.rs/doubleentry/latest/doubleentry/transparency/enum.Error.html) — every refusal, by name.

Signing and signature verification need the **`witness`** feature (Ed25519).
Without it every text format still parses and renders, and bundles verify by
hashes ([`ProofBundle::verify_inclusion`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.ProofBundle.html#method.verify_inclusion)).

## The gap proofs cannot close

Every [proof](@/docs/assurance/proofs.md) verifies against a tree head. An
operator who shows one head to the tax authority and another to the bank shows
two internally consistent histories: a **split view**. Only a party that is not
the operator, and remembers what it signed, can refuse it.

So the crate ships **no witness**. It speaks the [C2SP](https://c2sp.org)
formats used by the Go checksum database, Sigsum and the transparency-dev
witness network, and lets their stock `tlog-witness` servers do the
remembering.
[`testing::TestWitness`](https://docs.rs/doubleentry/latest/doubleentry/testing/struct.TestWitness.html)
(features `testing` and `witness`) is an in-memory test double.

| C2SP text | In the crate |
|---|---|
| `signed-note` v1.1.0 | `Note`, `KeyName`, `Vkey` |
| `tlog-checkpoint` v1.1.0 | `Checkpoint`, `SignedCheckpoint`, `LogKey` (Ed25519, `0x01`) |
| `tlog-cosignature` v1.1.0 | `Cosignature` (Ed25519, `0x04`) |
| `tlog-witness` v1.1.0 `add-checkpoint` | `AddCheckpoint`, `WitnessReply`, `WitnessSession` |
| `tlog-policy` | `Policy`, `PolicyBuilder` |
| `tlog-proof` v1 | `ProofBundle` |
| `tlog-tiles` v1.0.0 | `TileId`; `tile`, `tile_entries` |

Everything is sans-I/O: the crate builds requests and reads replies, your HTTP
client posts them. Keys come from caller seeds and timestamps from witnesses;
the crate reads no clock.

## Sign the head

```rust
use doubleentry::prelude::*;
use doubleentry::transparency::{Checkpoint, KeyName, LogKey, SignedCheckpoint, Vkey};

let id = LedgerId::new("example.com/ledgers/acme")?;
let books = Journal::new(id.clone());
let key = LogKey::from_seed(&id, [7; 32]); // the seed comes from your KMS

// Publish the verification key; auditors and witnesses configure it.
let published = key.vkey().to_string();
let vkey: Vkey = published.parse()?;

// The checkpoint is the head, signed as a C2SP signed note.
let signed = books.checkpoint(&key)?;
assert_eq!(signed.checkpoint().head(), books.head());
assert_eq!(signed.checkpoint().origin(), &KeyName::from(&id));

// Deterministic: signing the same head again gives the same bytes.
let again = key.sign(&Checkpoint::new(KeyName::from(&id), books.head()))?;
assert_eq!(again.to_bytes(), signed.to_bytes());

// Anyone with the vkey verifies it.
let parsed = SignedCheckpoint::parse(&signed.to_bytes())?;
assert_eq!(parsed.verify(&[vkey])?.len(), 1);
```

The origin is the ledger id; another ledger's key cannot sign
(`KeyNameMismatch`). Ed25519 signatures are verified strictly: non-canonical
`S`, small-order `R` and small-order keys are refused.

## Collect cosignatures

A witness receives an `add-checkpoint` request (the size it holds, a
consistency proof from there, the signed checkpoint), checks the proof against
what it signed before, and cosigns. A
[`WitnessSession`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.WitnessSession.html)
runs one checkpoint to one witness in at most three requests: on a 409 with the
size the witness really holds (`text/x.tlog.size`) it asks for a proof from
there; a witness whose size goes backwards ends the session with
`WitnessRegressed`.

```rust
use doubleentry::prelude::*;
use doubleentry::testing::TestWitness;
use doubleentry::transparency::{AddCheckpoint, LogKey, Next, Policy, WitnessReply, WitnessSession};

let id = LedgerId::new("example.com/ledgers/acme")?;
let mut books = Journal::new(id.clone());
let key = LogKey::from_seed(&id, [7; 32]);
// In production: somebody else's stock tlog-witness, configured with your vkey.
let mut witness = TestWitness::new("witness.example/w1", [9; 32])?.log(key.vkey());

// First contact: the witness holds nothing for this log (old = 0, no proof).
let first = books.checkpoint(&key)?;
let request = AddCheckpoint::new(&first, 0, None)?;
let reply = witness.post(&request.to_bytes()); // your HTTP POST to <prefix>/add-checkpoint
let cosignature =
    WitnessReply::read(reply.status, reply.content_type, &reply.body, &first, &witness.vkey())?;
let first = first.with_cosignature(&cosignature)?;
let mut held = first.checkpoint().head().size; // persist per witness

// Later: the session asks for the consistency proof the witness needs.
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
let next = books.checkpoint(&key)?;
let mut session = WitnessSession::new(next.clone(), witness.vkey(), held)?;
let cosigned = loop {
    let proof = match session.needed_proof() {
        Some((old, new)) => Some(books.prove_consistency(old, new)?),
        None => None,
    };
    let reply = witness.post(&session.request(proof.as_ref())?);
    match session.receive(reply.status, reply.content_type, &reply.body)? {
        Next::Cosigned { cosignature, witness_size } => {
            held = witness_size;
            break next.with_cosignature(&cosignature)?;
        }
        Next::Retry => continue, // 409: the session adopted the witness's size
        other => return Err(format!("unexpected: {other:?}").into()),
    }
};
assert_eq!(held, books.head().size);

// Publish only what your policy's quorum cosigned.
let policy = Policy::k_of_n(key.vkey(), 1, [(String::from("w1"), witness.vkey())])?;
policy.verify(&cosigned)?;
```

Every cosignature is verified before it is returned; its timestamp is the
witness's, returned and never judged. Every other outcome (400, 403, 404, 409,
422, 429, a malformed body, a witness ahead of the checkpoint) is a distinct
[`WitnessError`](https://docs.rs/doubleentry/latest/doubleentry/transparency/enum.WitnessError.html).
Persist the size each witness holds (0 when unknown); a wrong guess is
corrected through the 409.

## Policies

A policy is the `tlog-policy` text auditors and witness networks publish: known
logs, known witnesses, groups with thresholds, and a quorum.

```text
log example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs
witness w1 witness.example/w1+24b5d9fe+BP0XJDhaoMdbZPt4zWAvodmR/ev3axPFjtcC6sg16fYY
group witnesses 1 w1
quorum witnesses
```

[`Policy::k_of_n`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Policy.html#method.k_of_n)
builds the common case, `Policy::builder()` any other, `Policy::parse` reads the
text; `Display` writes the canonical form.

`Policy::verify` evaluates as transparency-dev/formats does: the checkpoint must
be signed by a policy log for its origin, **every** signature from a known key
must verify, unknown keys are ignored, and the quorum counts verified witness
cosignatures. Where the crate differs it is stricter, and the Go vectors record
both verdicts: ML-DSA-44 witnesses are kept but never counted; a witness key
that is also a log key is refused (`PolicyRule::WitnessIsLog`); two keys with
one (key name, key ID) are refused (`AmbiguousKey`).

## The auditor's Verifier

A [`Verifier`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html)
keeps one trusted head per origin: first a pin or the first validly signed
checkpoint, then **only forward, only through a consistency proof**, and, when
built from a policy, only to checkpoints its quorum cosigned. A smaller size is a
rollback; a second root at a trusted size is a fork, refused with both signed
checkpoints as evidence. A proof that does not verify is `Error::Proof` (no
evidence: anyone in between can corrupt one); a valid proof of a different
prefix is `Error::Inconsistent`.

```rust
use doubleentry::prelude::*;
use doubleentry::transparency::{KeyName, LogKey, Update, Verifier};

let id = LedgerId::new("example.com/ledgers/acme")?;
let key = LogKey::from_seed(&id, [7; 32]);
let mut books = Journal::new(id.clone());
let mut auditor = Verifier::new().log(key.vkey())?;

let first = books.checkpoint(&key)?;
assert_eq!(auditor.update(&first, None)?, Update::First);

books.declare_currency(EUR, 2, date!("2026-01-01"))?;
let next = books.checkpoint(&key)?;
let (old, new) = (first.checkpoint().head(), next.checkpoint().head());

// Without the consistency proof the head does not move.
assert!(auditor.update(&next, None).is_err());
let proof = books.prove_consistency(old.size, new.size)?;
assert_eq!(auditor.update(&next, Some(&proof))?, Update::Advanced { from: old });
assert_eq!(auditor.head(&KeyName::from(&id)), Some(new));

// Going back is a rollback.
assert!(auditor.update(&first, None).is_err());
```

Persist [`heads`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.Verifier.html#method.heads)
and restore them with `pin`; the verifier holds no other state.

## Proof bundles

A [`ProofBundle`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.ProofBundle.html)
is one `tlog-proof` file: a signed checkpoint, the record's leaf bytes and its
inclusion path, verifiable offline. `bundle_record` proves a record at a
position, `bundle_entry` an entry by id, `bundle_balance` one account's
[sealed balance](@/docs/assurance/proofs.md#sealed-balance).

```rust
use doubleentry::prelude::*;
use doubleentry::transparency::{LogKey, Policy, ProofBundle, Proven, Verifier};

let id = LedgerId::new("example.com/ledgers/acme")?;
let mut books = Journal::new(id.clone());
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 9..=9)?;
let invoice = books.record(
    Entry::new("invoice-2026-0001", date!("2026-09-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;
books.seal_period("2026-09")?;
let key = LogKey::from_seed(&id, [7; 32]);
let signed = books.checkpoint(&key)?;

// The operator hands out two files.
let entry_file = books.bundle_entry(invoice.id, &signed)?.to_bytes();
let cash_file = books
    .bundle_balance("Assets:Cash", EUR, Layer::Settled, "2026-09", &signed)?
    .to_bytes();

// The auditor verifies them offline, against its policy alone…
let policy = Policy::log_only(key.vkey());
let Proven::Record(entry) = ProofBundle::parse(&entry_file)?.verify(&policy)? else {
    return Err("an entry bundle proves a record".into());
};
assert_eq!(entry.entry_id, Some(invoice.id));
assert_eq!(entry.position, invoice.position);

// …or against the head its Verifier trusts.
let mut auditor = Verifier::from_policy(policy);
auditor.update(&signed, None)?;
let Proven::Balance(cash) = auditor.verify(&ProofBundle::parse(&cash_file)?)? else {
    return Err("a balance bundle proves a balance".into());
};
assert_eq!(cash.account.to_string(), "Assets:Cash");
assert_eq!(cash.balance.net(), amount!("1190.00"));
```

`Policy::log_only` needs no witness; in production the policy names the
witnesses required. A `Verifier` checks a bundle only at its trusted head
(`NotAtTrustedHead` otherwise).

## Tiles

[`TileId::changed`](https://docs.rs/doubleentry/latest/doubleentry/transparency/struct.TileId.html#method.changed)
names the `tlog-tiles` tiles that changed between two sizes; `tile` renders a
hash tile from stored nodes, so monitors can mirror the tree with stock tooling.

```rust
use doubleentry::prelude::*;
use doubleentry::transparency::TileId;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
let mut published = Vec::new();
for tile in TileId::changed(0, books.head().size) {
    let bytes = books.tile(tile)?; // write to <prefix>/<tile.path()>
    assert_eq!(bytes.len(), 32 * usize::from(tile.width()));
    published.push(tile.path());
}
assert_eq!(published, ["tile/0/000.p/2"]);
```

Hash tiles reveal only hashes. **Entry bundles**
([`tile_entries`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.tile_entries))
disclose every record in their range in full (memo text excepted): publish them
only where the books may be read.

## Tested against Go, both ways

- **Go → crate.** `interop/c2sp/gen` writes vectors from fixed seeds with
  `golang.org/x/mod/sumdb/{note,tlog}`, `transparency-dev/formats`,
  `transparency-dev/merkle` and the stock `transparency-dev/witness`. They are
  committed under `tests/vectors/c2sp/` and read on every test run, without Go.
- **Crate → Go.** `just interop` has `interop/c2sp/check` open the crate's
  checkpoints, requests, bundles, policies and tiles with the Go libraries, run
  the stock witness on ten consecutive requests (plus a stale size, a foreign
  key and a corrupted proof, refused with 409, 403 and 422), and hand its
  cosignatures back to the crate to verify.

Full programs: [`examples/transparency.rs`](https://github.com/hupe1980/doubleentry/blob/main/examples/transparency.rs)
(two witnesses, a two-of-two policy, tiles, bundles, an auditor) and
[`examples/witness.rs`](https://github.com/hupe1980/doubleentry/blob/main/examples/witness.rs)
(a witness cosigning each new head while an auditor follows).

## Limits

- No witness, no HTTP, no retry timing: the caller posts, backs off and persists
  each witness's size.
- A cosignature timestamp is the witness's claim, not a qualified time stamp.
- No ML-DSA signing, subtree cosignatures, COSE receipts or RFC 3161 tokens.
- A party that accepts an unwitnessed checkpoint is not protected from a split
  view.
