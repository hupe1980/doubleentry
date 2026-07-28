//! An immutable, tamper-evident double-entry bookkeeping engine.
//!
//! `doubleentry` enforces the invariants of double-entry bookkeeping inside the
//! engine and can prove afterwards that it did. It is a calculation library, not
//! a platform: no I/O, no async, no database, and no opinion about your chart of
//! accounts.
//!
//! # What it guarantees
//!
//! - **Balanced by construction.** An entry reaches a persistable state only by
//!   passing validation, and the validated type cannot be constructed any other
//!   way. Unbalanced data cannot reach storage without a bug in this crate.
//! - **Exact.** Money is a scaled integer with a compile-time precision. Every
//!   arithmetic operation that can overflow returns a `Result`. There is no
//!   floating point and nothing panics on a hostile input.
//! - **Deterministic.** The engine reads no clock and no random number generator.
//!   Identical inputs produce identical bytes, hashes, and orderings, which is
//!   what makes replay and reproducible testing possible.
//! - **Verifiable.** Entries are leaves in an append-only Merkle log. A third
//!   party holding only a tree head can be given an `O(log n)` proof that an
//!   entry is included, and an `O(log n)` proof that the log was only appended
//!   to. Closing a period emits a [`Seal`] that commits to both its entries and
//!   its closing balances, chained to the seal before it.
//!
//! # Layout
//!
//! | Module | Purpose |
//! |---|---|
//! | [`money`] | `Amount<P>` and `Currency` — exact scaled-integer arithmetic |
//! | [`account`] | The account tree and what may be posted to |
//! | [`dimensions`] | Typed tags orthogonal to the account path |
//! | [`posting`] | `Direction`, `Layer`, and the atomic movement |
//! | [`entry`] | The type-state entry and its validation |
//! | [`balance`] | Gross-preserving balances and trial balances |
//! | [`period`] | Period definitions and their lifecycle |
//! | [`journal`] | Append-only journal, idempotency, corrections, statements |
//! | [`clearing`] | Open-item matching and residual tracking |
//! | [`closing`] | Year-end closing entries |
//! | [`storage`] | Persistence traits, in-memory / SQLite / PostgreSQL backends, Iceberg cold tier, conformance suite |
//! | [`merkle`] | The append-only log and its proofs |
//! | [`seal`] | Period seals and the seal chain |
//! | [`checkpoint`] | Balance checkpoints and external assertions |
//! | [`canonical`] | The byte encoding everything is hashed from |
//! | [`hash`] | Domain-separated digests |
//!
//! # Serialisation
//!
//! With the `serde` feature, values round-trip through their own constructors,
//! so an invariant that holds for a constructed value also holds for one read
//! back. Money serialises as a decimal string rather than a raw scaled integer,
//! and deserialising an entry yields a [`Draft`] — the balanced witness is
//! re-established by validation on the receiving side, never trusted from the
//! wire.
//!
//! # What it is not
//!
//! Not an ERP, not an ORM, not a reporting engine, not a chart of accounts, and
//! not a payments library. It produces validated, balanced, provable entries and
//! leaves every domain decision to the caller.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Assertions and fixture setup are allowed to panic; library code is not.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

pub mod account;
pub mod balance;
pub mod canonical;
pub mod checkpoint;
pub mod clearing;
pub mod closing;
pub mod dimensions;
pub mod entry;
pub mod hash;
pub mod journal;
pub mod merkle;
pub mod money;
pub mod period;
pub mod posting;
pub mod seal;
mod serde_support;
pub mod storage;

pub use account::{Account, AccountError, AccountId, AccountKind, AccountPath, AccountRegistry};
pub use balance::{Balance, BalanceKey, TrialBalance};
pub use canonical::{Canonical, CanonicalWriter};
pub use checkpoint::{AssertionOutcome, BalanceAssertion, Checkpoint, CheckpointError};
pub use clearing::{
    ClearedItem, Clearing, ClearingError, ClearingEvent, ClearingId, ClearingRegister, OpenItem,
    PostingLookup, PostingRef,
};
pub use closing::{ClosingError, closing_postings};
pub use dimensions::{
    ActivityId, CostObjectId, DimensionError, Dimensions, Label, PartyId, SegmentId,
};
pub use entry::{
    Balanced, CurrencyPolicy, Description, DocumentRef, Draft, Entry, EntryFieldError, EntryId,
    IdempotencyKey, IntegrityError, LedgerPolicy, Provenance, SealContext, ValidationError,
    ValidationErrors,
};
pub use hash::{Hash, ParseHashError};
pub use journal::{Journal, JournalError, LogIndex, NotSequenced, Recorded};
pub use merkle::{
    ConsistencyProof, InclusionProof, MerkleAccumulator, MerkleLog, ProofError, TreeHead,
    empty_root, leaf_hash,
};
pub use money::{Amount, Currency, MoneyError};
pub use period::{LedgerId, Period, PeriodCalendar, PeriodError, PeriodId, PeriodState};
pub use posting::{Direction, Layer, Posting};
pub use seal::{PeriodCoverage, Seal, SealChain, SealChainError, trial_balance_root};
pub use storage::{
    BatchError, Cursor, DynLedgerStore, EntryBatch, LedgerStore, MemoryStore, MemoryStoreError,
    Page, StatementLine, StatementPage, StoredEntry,
};

/// Compiles and runs every example in the crate's README.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeExamples;
