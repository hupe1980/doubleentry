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
//!   Debits equal credits per currency *and per layer*: every balance the engine
//!   reports is keyed on both, so an entry whose reservation were offset by a
//!   settled movement would falsify the trial balance permanently.
//! - **Exact.** Money is a scaled integer with a compile-time precision. Every
//!   arithmetic operation that can overflow returns a `Result`. There is no
//!   floating point and nothing panics on a hostile input.
//! - **Bounded, where you say so.** An account can be forbidden from crossing
//!   zero — a cash box that cannot be overdrawn, a wallet that cannot be drawn
//!   beyond what was funded. The limit is checked against the balance an entry
//!   would leave behind, with an outstanding reservation counted against it and
//!   an expected inflow not, and the SQL backends check it inside the write, so
//!   two concurrent draws cannot together breach it.
//! - **Hierarchical.** Account paths are a tree and only leaves are postable,
//!   which makes a *node's* balance a single defensible number rather than two.
//!   [`Rollup`] is that number for every node at once — gross totals intact,
//!   grouping nodes inferred from the paths that exist, summarisable to any
//!   depth, and a pure function of a trial balance and a registry.
//! - **Reportable on either date.** An entry carries a booking date and a value
//!   date, and any balance, trial balance or statement folds by whichever the
//!   query names — the books, or the cash. It stays a *reporting* choice: period
//!   membership and what a seal commits to are booking-date questions, or a
//!   settlement instruction could reopen sealed books.
//! - **Deterministic.** The engine reads no clock and no random number generator.
//!   Identical inputs produce identical bytes, hashes, and orderings, which is
//!   what makes replay and reproducible testing possible.
//! - **Verifiable.** Entries are leaves in an append-only Merkle log. A third
//!   party holding only a tree head can be given an `O(log n)` proof that an
//!   entry is included, and an `O(log n)` proof that the log was only appended
//!   to. Closing a period emits a [`Seal`] that commits to its entries, its
//!   closing balances, and the account registry those balances are keyed on,
//!   chained to the seal before it — so a single closing balance can be proven
//!   against that seal with a [`BalanceProof`], and *named* with an
//!   [`AccountBindingProof`], without handing over the rest of the trial balance
//!   or the rest of the chart of accounts. Seals chain, and each carries a
//!   consistency proof from its predecessor's tree — so the chain establishes
//!   that the log was only ever *appended to*, checkable by someone holding the
//!   seals and nothing else.
//! - **Witnessable.** Every guarantee above is relative to a tree head, so an
//!   operator who can publish two heads can publish two histories and prove
//!   either one. [`witness`] closes that: an independent party remembers the
//!   last head it vouched for, demands a consistency proof to move forward, and
//!   **refuses a second history at the same size**. With the `witness` feature
//!   it cosigns in the C2SP signed-note format, so the refusal is something a
//!   third party can check.
//!
//! # Where to start
//!
//! This page is the API reference. The guide — why each guarantee is built the
//! way it is — lives at <https://hupe1980.github.io/doubleentry>.
//!
//! A [`Journal`] is one entity's books: its accounts, its calendar, its policy,
//! its entries, its Merkle log, its seals, its clearings. Register accounts on
//! it, hand it drafts, and read balances, statements and proofs back out. When
//! the books have to survive the process, put a [`LedgerStore`] behind it —
//! [`MemoryStore`], `SqliteStore` or `PostgresStore` — and the semantics are the
//! same, because the [`conformance`](storage::conformance) suite is what decides
//! whether a backend is one.
//!
//! # Layout
//!
//! | Module | Purpose |
//! |---|---|
//! | [`money`] | `Amount<P>`, `Currency` and `Rounding` — exact scaled-integer arithmetic |
//! | [`account`] | The account tree, what may be posted to, and balance limits |
//! | [`dimensions`] | `Label`, `DimensionFilter`, and the caller-named axes a posting is sliced by |
//! | [`posting`] | `Direction`, `Layer`, and the atomic movement |
//! | [`entry`] | The type-state entry and its validation |
//! | [`balance`] | Gross-preserving balances, trial balances, and `BalanceQuery` |
//! | [`period`] | Period definitions and their lifecycle |
//! | [`journal`] | The books: accounts, calendar, entries, idempotency, corrections, statements |
//! | [`clearing`] | Open-item matching and residual tracking |
//! | [`closing`] | Year-end closing entries |
//! | [`rollup`] | Hierarchical reporting: a trial balance folded up the account tree |
//! | [`storage`] | Persistence traits, in-memory / SQLite / PostgreSQL backends, Iceberg cold tier, conformance suite |
//! | [`merkle`] | The append-only log and its proofs |
//! | [`seal`] | Period seals, the seal chain, and balance proofs |
//! | [`checkpoint`] | Balance checkpoints and external assertions |
//! | [`witness`] | Witness cosigning: the independent check on a split view |
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
pub mod rollup;
pub mod seal;
mod serde_support;
pub mod storage;
pub mod witness;

pub use account::{
    Account, AccountBindingProof, AccountError, AccountId, AccountKind, AccountPath, AccountRecord,
    AccountRegistry, BalanceLimit, account_binding_leaf,
};
pub use balance::{Balance, BalanceKey, BalanceQuery, DateBasis, EntryDates, TrialBalance};
pub use canonical::{Canonical, CanonicalReader, CanonicalWriter};
pub use checkpoint::{AssertAt, AssertionOutcome, BalanceAssertion, Checkpoint, CheckpointError};
pub use clearing::{
    ClearedItem, Clearing, ClearingError, ClearingEvent, ClearingId, ClearingRegister, OpenItem,
    PostingLookup, PostingPosition, PostingRef,
};
pub use closing::{ClosingError, closing_postings};
pub use dimensions::{DimensionError, DimensionFilter, Dimensions, Label};
pub use entry::{
    Balanced, CurrencyPolicy, Description, DocumentRef, Draft, Entry, EntryFieldError, EntryId,
    IdempotencyKey, IntegrityError, LedgerPolicy, Provenance, SealContext, ValidationError,
    ValidationErrors,
};
pub use hash::{Hash, ParseHashError, RESERVED_DOMAIN_PREFIX};
pub use journal::{
    AuditCheck, AuditReport, Journal, JournalError, LogIndex, NotSequenced, Recorded,
};
pub use merkle::{
    ConsistencyProof, InclusionProof, MalformedAccumulator, MalformedProof, MerkleAccumulator,
    MerkleLog, ProofError, TreeHead, empty_root, leaf_hash,
};
pub use money::{Amount, Currency, MoneyError, Rounding};
pub use period::{LedgerId, Period, PeriodCalendar, PeriodError, PeriodId, PeriodState};
pub use posting::{Direction, Layer, Posting};
pub use rollup::{Rollup, RollupNode};
pub use seal::{
    BalanceProof, PeriodCoverage, Seal, SealChain, SealChainError, SealedBalance,
    SealedBalanceError, SealedBalanceOutcome, TrialBalanceCommitment, balance_leaf,
    trial_balance_head,
};
pub use storage::{
    BatchError, Cursor, DynLedgerStore, EntryBatch, LedgerStore, MemoryStore, MemoryStoreError,
    OpenItemPage, Page, PostingCursor, StatementLine, StatementPage, StoredEntry,
};

/// Compiles and runs every example in the crate's README.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeExamples;
