//! Checkpoints others can cosign and auditors can check: the C2SP
//! transparency formats over the ledger's RFC 6962 log.
//!
//! # What is claimed
//!
//! Byte-for-byte compatibility with these C2SP texts, tested against
//! vectors the Go reference implementations generated (`tests/vectors/c2sp/`,
//! produced by `interop/c2sp/gen` from `golang.org/x/mod/sumdb/{note,tlog}`,
//! `github.com/transparency-dev/{formats,merkle,witness}`) and, the other
//! way round, crate output those implementations accept
//! (`interop/c2sp/check`):
//!
//! | Text | Here |
//! |---|---|
//! | `signed-note` v1.1.0 | [`Note`], [`KeyName`], [`Vkey`] |
//! | `tlog-checkpoint` v1.1.0 | [`Checkpoint`], [`SignedCheckpoint`], [`LogKey`] (`0x01`) |
//! | `tlog-cosignature` v1.1.0 | [`Cosignature`] (Ed25519 `0x04`) |
//! | `tlog-witness` v1.1.0 `add-checkpoint` | [`AddCheckpoint`], [`WitnessReply`], [`WitnessSession`] |
//! | `tlog-policy` | [`Policy`], [`PolicyBuilder`] |
//! | `tlog-proof` v1 | [`ProofBundle`] |
//! | `tlog-tiles` v1.0.0 | [`TileId`]; the façades' `tile` and `tile_entries` |
//!
//! # The log side and the auditor side
//!
//! A ledger signs its head as a checkpoint ([`Journal::checkpoint`]), asks
//! stock `tlog-witness` witnesses to cosign it (the caller does HTTP: the
//! crate builds each request, reads each reply, runs the retry-on-409 loop
//! and verifies the cosignature), publishes tiles, and hands out
//! [`ProofBundle`]s for one entry, one seal or one sealed balance. An
//! auditor keeps a [`Verifier`]: a trusted head per ledger that moves only
//! through a consistency proof (and, if it wants, only when its
//! [`Policy`]'s quorum of witnesses cosigned), and checks bundles against it.
//!
//! # What is not here
//!
//! No witness (`testing::TestWitness` is a test double), no HTTP, no
//! clock, no randomness: keys come from caller seeds, timestamps from witnesses, and
//! nothing judges a timestamp. No ML-DSA-44: a `0x06` witness in a policy is
//! kept but never counted (fail closed); a `0x06` key anywhere else is
//! refused as unsupported. No subtree cosignatures, COSE receipts or RFC
//! 3161 tokens.
//!
//! # Strictness
//!
//! Parsers refuse everything that is not the one canonical text — non-UTF-8,
//! control characters, non-canonical base64 or decimals, roots that are not
//! 32 bytes, empty lines, a repeated (key name, key ID), more than 100
//! signatures, more than 63 proof hashes — each with its own [`Error`]
//! variant, and never panic. Every input has a total size limit, checked
//! before anything is decoded: a note (and a witness's 200 reply)
//! [`MAX_NOTE_BYTES`] (1 000 000), an `add-checkpoint` body
//! [`MAX_REQUEST_BYTES`], a bundle [`MAX_BUNDLE_BYTES`] with an `extra` of at
//! most [`MAX_EXTRA_BYTES`]. Every Ed25519 verification is strict
//! (`verify_strict`: non-canonical `S`, small-order `R` and `A` refused) and
//! a vkey whose key does not decode or has small order is refused. A set of
//! trusted keys (a policy, a [`Verifier`]'s log keys, a key list) with two
//! different keys of one (key name, key ID) is refused, never resolved by
//! taking the first.
//!
//! # Disclosure
//!
//! Hash tiles reveal nothing but hashes. Entry bundles
//! ([`Journal::tile_entries`]) disclose every record in their range, leaf
//! bytes in full — memo text excepted, which is only committed (a salted
//! hash). A sealed-balance bundle discloses the seal (its totals per
//! currency and layer), one account's registration and one balance.
//!
//! Without the `witness` feature every text format parses and renders and
//! bundles verify by hashes; signing and signature verification need it.
//!
//! [`Journal::checkpoint`]: crate::Journal::checkpoint
//! [`Journal::tile_entries`]: crate::Journal::tile_entries

mod base64;
mod bundle;
mod checkpoint;
mod error;
mod note;
mod policy;
pub(crate) mod prove;
mod signing;
mod tiles;
#[cfg(feature = "witness")]
mod verifier;
mod witness;

#[cfg(fuzzing)]
pub(crate) use bundle::decode_balance_leaf;
pub use bundle::{
    BALANCE_PROOF_TAG, MAX_BUNDLE_BYTES, MAX_EXTRA_BYTES, ProofBundle, Proven, ProvenRecord,
};
pub use checkpoint::{Checkpoint, SignedCheckpoint};
pub use error::{Error, ForkEvidence, PolicyRule, Syntax, WitnessError};
pub use note::{KeyName, MAX_NOTE_BYTES, MAX_SIGNATURES, Note, NoteSignature};
pub use policy::{MAX_POLICY_ITEMS, Policy, PolicyBuilder, PolicyVerdict, Threshold};
#[cfg(feature = "witness")]
pub use signing::LogKey;
pub use signing::{Cosignature, KeyType, Verified, Vkey};
pub use tiles::{TileId, TileKind};
#[cfg(feature = "witness")]
pub use verifier::{Update, Verifier};
pub use witness::{AddCheckpoint, MAX_REQUEST_BYTES};
#[cfg(feature = "witness")]
pub use witness::{Next, WitnessReply, WitnessSession};

pub(crate) use bundle::balance_bundle;
#[cfg(all(feature = "witness", feature = "testing"))]
pub(crate) use signing::{cosign, verify_line, vkey};
pub(crate) use tiles::entry_bundle;
