+++
title = "Witnessing"
description = "The split-view attack that inclusion and consistency proofs cannot see, and the independent cosigner that refuses to participate in it."
weight = 7
+++

## The gap proofs cannot close

The [proofs](@/docs/proofs.md) page ends on an admission:

> The boundary: a head has to come from somewhere the verifier already trusts.

That sentence is where a determined operator lives. Every guarantee this engine
makes is *relative to a tree head* — inclusion is against a head, consistency is
between two heads, a seal commits to a head. So an operator who can serve two
heads can serve two histories:

```
                    ┌── head H₁ ──▶ the tax authority
   the operator ────┤
                    └── head H₂ ──▶ the bank
```

Behind each head sits a complete, internally consistent log. Every inclusion
proof the tax authority checks verifies. Every consistency proof the bank checks
verifies. Both parties can audit for years and neither will find anything,
because there is nothing wrong *inside* either view. This is a **split view**,
and no proof can detect it: each party only ever sees one side.

The crate's own test suite asserts the weakness rather than glossing it —
`two_histories_are_each_internally_perfect` in `tests/witness.rs` builds both
logs and checks that every proof in each one passes.

## What a witness is

Somebody who is not the operator, who remembers.

A witness holds one row per log — an origin, a size, a root — and applies one
rule above all others: **it will not vouch for a second history at a size it has
already vouched for.** Offered a head at a size it knows, with a different root,
it refuses. Permanently. There is no later head that could make the claim honest
again, because it has already put its name to the other one.

That is the whole mechanism. The rest is hygiene:

| Situation | Answer |
|---|---|
| Log not tracked | `Untracked` — refused |
| Same size, same root | accepted, `advanced: false` |
| **Same size, different root** | **`Fork` — refused** |
| Smaller size | `Shrunk` — a log does not shrink |
| Larger size, no proof | `MissingProof` |
| Larger size, proof fails | `BadProof` |
| Larger size, proof verifies | accepted, `advanced: true` |

A verifier then refuses any head that is not cosigned by the witnesses *it*
chose. To show that verifier a forked history, the operator now has to compromise
those witnesses too — and they are deliberately not the operator's.

## Using one

```rust
use doubleentry::witness::{MemoryWitnessStore, Origin, Witness};

let origin = Origin::new("example.com/ledgers/acme-gmbh")?;

// Step one is out of band, and has to be. Everything else the witness does is
// relative to a head it already holds, so the *first* head is the one thing it
// cannot check. Get it the way you would get a certificate fingerprint.
let mut witness = Witness::new(MemoryWitnessStore::new());
witness.trust(origin.clone(), archived_head)?;

// Later: the operator offers a newer head, and has to prove it extends.
let now   = journal.head();
let proof = journal.prove_consistency_between(archived_head.size, now.size)?;

let accepted = witness.offer(&origin, now, Some(&proof))?;
assert!(accepted.advanced);
```

Offer the forked head instead and you get `WitnessError::Fork`, naming both
roots — and the witness does not move.

### Cosigning

A witness that cannot sign produces nothing anyone downstream can check. With the
`witness` feature, wrap it in a `Cosigner`:

```rust
use doubleentry::witness::signing::{Cosigner, SigningKey};
use doubleentry::witness::{KeyName, SignedTreeHead};

let key = SigningKey::from_seed(KeyName::new("auditor.example")?, seed);
let public = key.verifying_key();          // what verifiers hold

let mut cosigner = Cosigner::new(witness, key);
let signature = cosigner.cosign(&origin, now, Some(&proof), unix_seconds)?;

let mut published = SignedTreeHead::new(origin, now);
published.add_signature(signature);
println!("{}", published.to_note());
```

`Cosigner::cosign` runs the state machine **first** and signs only if it returns.
A refused head produces an error and no bytes at all — which matters, because
`SigningKey::cosign_body` will sign anything, and a signature made outside the
state machine defeats the entire arrangement.

## The wire format is somebody else's

The note is [C2SP `signed-note`](https://c2sp.org/signed-note) around a
[C2SP `tlog-checkpoint`](https://c2sp.org/tlog-checkpoint) body, with
[C2SP `tlog-cosignature`](https://c2sp.org/tlog-cosignature) signatures — the
formats Certificate Transparency, the Go checksum database, Sigsum and sigstore
all speak:

```
example.com/ledgers/acme-gmbh                 ← origin
4812                                          ← tree size
3q2+7wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=  ← base64 root
                                              ← blank line
— auditor.example BASE64(keyhash‖time‖sig)    ← cosignature
```

This is not decoration. **A witness is only worth having if it is somebody
else's**, and a format only this crate understands could only ever be cosigned by
software only this crate ships — which is not an independent check, it is the
same check run twice.

Everything above the blank line is the **body**, and the body is what a
signature covers, byte for byte. `SignedTreeHead::body()` hands back the text for
exactly that reason: sign what you read, not what you re-rendered.

Parsing is strict where it decides anything, and the strictness is load-bearing.
A tree size with a leading zero, a non-canonical base64 group, a key name
carrying a Unicode space — all refused. Laxity would mean two texts for one head,
and a signature covers the text.

Extension lines go the other way. They carry meaning this crate does not assign,
so they are kept **verbatim** and rendered back: dropping them would make `body()`
produce something nobody signed, and refusing them would mean refusing to cosign
for any log that uses one. This crate never writes an extension line — the
specification calls them NOT RECOMMENDED, because monitors cannot audit them —
but it reads them, which is what lets it witness somebody else's log.

### Verified against Go

Interop is a claim worth checking rather than asserting. `tests/witness.rs` pins
the exact bytes of a note, and those bytes were cross-checked against Go's
`crypto/ed25519` and the key-hash construction in `golang.org/x/mod/sumdb/note`:
the derived public key matches, both key hashes match, and `ed25519.Verify`
accepts the cosignature this crate produced.

## Why there is no `HttpWitness`

[C2SP `tlog-witness`](https://c2sp.org/tlog-witness) puts this behind
`POST /add-checkpoint`. That transport is **not** in this crate, and the omission
is deliberate: it is a request body, a response body and four status codes.
Shipping an HTTP stack to save fifteen lines would put an async runtime and a TLS
implementation into the dependency tree of every user who never touches a
witness.

`AddCheckpoint` is the request as bytes. Hand it to whatever client you already
have:

```rust
let request = AddCheckpoint::new(SignedTreeHead::new(origin, head), proof);
let response = my_http_client.post(url, request.to_body())?;
```

And on the serving side, `WitnessError` documents its status code per variant:
`Untracked` → 404, `Fork` and `Shrunk` → 409, `MissingProof` and `BadProof` →
422.

## Two things that are not optional

**Durability.** A witness whose state does not survive a restart is a witness
that can be made to forget a fork by being restarted — not a difficult thing for
an operator to arrange. `WitnessStore::store` must be durable *before it
returns*. `MemoryWitnessStore` is not, and says so; use it for tests and for
reloading state that lives somewhere else.

**No trust-on-first-use.** `Witness::trust` refuses a log it already tracks. A
witness that adopted whatever head it was shown first could be introduced to the
fork as easily as to the log, by the same operator, and would then cosign the
fork with complete sincerity. Re-pointing an existing witness is exactly how a
fork would be laundered, so calling the setup function twice will not do it.

Prefer a starting head with at least one entry in it. From size 0 no consistency
proof is possible — every log extends the empty tree, which is why this crate
[refuses to build one](@/docs/proofs.md#never-from-the-empty-tree) — so the first
non-empty head has to be taken on trust.

## What it does not do

A witness never sees an entry, a balance, an account or a period. It sees an
origin, a size and a root. That is what lets it be someone with no business
seeing your books: your auditor, a notary, a trade association, the counterparty,
a second machine in a different administrative domain.

It also does not make anything *available*. A witness stops a forked history from
being believed; it does not stop a log from being withheld. Those are different
problems and only the first has a cryptographic answer.

## Determinism

A cosignature covers a timestamp, and this engine reads no clock. The timestamp
is therefore an argument — `Cosigner::cosign` takes it — never something taken
from the environment, and `ed25519-dalek` is pulled in with `rand_core` off so
that stays true by construction rather than by discipline.

Pass `SystemTime::now()` at the call site, where a reader can see it.
