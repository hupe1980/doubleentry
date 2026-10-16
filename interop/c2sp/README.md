# C2SP interop harness

Two Go programs check that `doubleentry::transparency` matches the C2SP formats byte for
byte, against the Go reference implementations.

| Program | Does |
|---|---|
| `gen` | Writes `tests/vectors/c2sp/` from fixed seeds with `golang.org/x/mod/sumdb/{note,tlog}`, `transparency-dev/formats`, `transparency-dev/merkle` and the stock `transparency-dev/witness` handler: checkpoints, vkeys, malformed notes, RFC 6962 roots and proofs, witness requests and replies, cosignatures, policies, `tlog-proof` bundles and tiles. Output is deterministic; the crate's tests read it without Go. |
| `check` | Verifies what the crate writes to `target/interop/`: checkpoints, policies, bundles and tiles, and the crate's requests against the stock witness (accepted, stale 409, foreign key 403, bad proof 422). It writes the witness's replies back for the crate's `witness_roundtrip` test. |

Where the crate is deliberately stricter than Go (non-canonical notes, ML-DSA-44 cosignatures
never counted, a witness key equal to the log key, two keys with one name and key ID in a
policy), the vector records both verdicts on a `divergence` line.

Run with `just interop` from the repository root (Go 1.27). Versions are pinned in `go.mod`.
This module is not part of the published crate.
