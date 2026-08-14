+++
title = "Features and serialisation"
description = "The crate's cargo features, and the rule every validated type follows on the way in from the wire."
weight = 15
+++

## Cargo features

| Feature | Effect |
|---|---|
| `serde` | `Serialize` / `Deserialize` on public types. Transport only — the canonical encoding used for hashing never routes through serde |
| `sqlite` | A SQLite-backed `LedgerStore` on `sqlx`, plus the reference schema |
| `postgres` | A PostgreSQL-backed `LedgerStore` on `sqlx`, plus the reference schema |
| `iceberg` | An [Apache Iceberg cold tier](@/docs/cold-tier.md) for sealed periods |

All are off by default. The core engine has five dependencies and performs no
I/O.

## Validation survives the wire

Deriving `Deserialize` on a newtype whose constructor validates its input
silently defeats that validation — and does so precisely for values that came
from outside the program, which is where validation matters most.

Every validated type here therefore round-trips through its **own constructor**:

```rust
// Values no constructor would accept do not survive a round trip.
assert!(serde_json::from_str::<Currency>("\"eur\"").is_err());   // must be uppercase
assert!(serde_json::from_str::<Label>("\"\"").is_err());          // must be non-empty
```

An invariant that holds for a constructed value holds for one read back, or the
type guarantees nothing.

## Money is a string

```rust
assert_eq!(serde_json::to_string(&Eur::parse("12.34")?)?, "\"12.34\"");
assert!(serde_json::from_str::<Eur>("1234").is_err());
```

Never a float, and never the raw scaled integer. The integer is meaningless
without knowing the scale, so a consumer reading it at the wrong precision would
silently misread every amount by a factor of ten.

## Deserialising an entry yields a Draft

Never a `Balanced` entry. The balanced type is a *witness that validation ran*,
and a witness that can be read off a wire is not a witness — a peer could assert
it for postings that do not balance, against accounts that do not exist, in a
period that is closed.

Received entries are therefore drafts, and the receiver re-runs `seal` against
its **own** accounts, calendar and policy:

```rust
let draft: Entry<Draft, 2> = serde_json::from_str(&payload)?;
let entry = draft.seal(&journal.context())?;   // validated here, not there
```

Round-tripping is lossless: sealing a deserialised draft that was valid
reproduces the same content hash.

## Seals are checked on the way in

A `Seal` read from JSON is verified against its own `seal_hash` before it is
returned, and deserialisation fails if it does not match. The one type whose
entire purpose is to be a commitment should not be constructible as a commitment
to nothing — and a caller who never thinks to call `is_self_consistent` still
cannot be handed a forged one.

## Rehydration checks the hash, not the rules

A backend reading back an entry **it wrote itself** uses `adopt_verified`, which
compares the content hash rather than re-running validation.

Re-running validation would be wrong here: validation is against the ledger's
*current* accounts, calendar and policy, so a historical entry would start
failing the day its account closed or its period sealed — even though it was
valid when written and must stay readable forever.

Comparing the hash is also *stronger*. It proves the bytes are exactly what
passed validation originally; re-validation would happily accept a **different**
entry that also happens to balance.
