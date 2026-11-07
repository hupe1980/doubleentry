//! An immutable, tamper-evident double-entry bookkeeping engine.
//!
//! A **ledger** is one entity's books, kept as one append-only
//! [RFC 6962](https://www.rfc-editor.org/rfc/rfc6962) SHA-256 Merkle log of
//! typed records: a genesis record, account events, currency declarations,
//! policy events, period definitions, entries, clearings and seals. Every record has exactly one
//! canonical byte form, so a position in the log, a leaf hash and a proof mean
//! the same thing to every implementation.
//!
//! Every write is decided by **one pure function**, the rule core: it takes
//! the draft and a read-only view of the ledger at the position the record
//! will be appended, and returns either the record to write or one
//! [`Rejection`] with a stable code. No façade and no store implements a
//! rule; [`Journal::audit`] and [`Ledger::audit`] replay the whole log
//! through the same core.
//!
//! # Where things are
//!
//! - **The crate root** — what bookkeeping code names: the two façades with
//!   the same methods, [`Journal`] (sync, in memory) and [`Ledger`] (async,
//!   over a store); the one [`Error`] and its [`Rejection`]; the values
//!   (exact [`Amount`]s, [`Currency`] codes, validated names such as
//!   [`AccountPath`] and [`Label`], the derived [`EntryId`]); the [`Entry`]
//!   draft; [`BalanceQuery`]; the [`TreeHead`] proofs verify against.
//! - [`currency`](mod@currency) — the currency constants (`EUR`, `JPY`, …).
//! - [`report`] — what the reads return: [`TrialBalance`](report::TrialBalance),
//!   [`Statement`](report::Statement), [`Rollup`](report::Rollup),
//!   [`OpenItem`](report::OpenItem)s and balance assertions.
//! - [`export`] — the GoBD export view: verifiable flat rows.
//! - [`error`] — the payloads [`Error`]'s variants carry.
//! - [`log`] — proofs and the RFC 6962 hashing an independent verifier
//!   re-implements; [`record`] — the eight record kinds and their canonical
//!   bytes.
//! - [`prelude`] — the everyday vocabulary in one import.
//! - [`store`] — where a ledger is kept: the [`Store`](store::Store)
//!   contract, [`MemoryStore`](store::MemoryStore), the SQL store, and the
//!   rule core's protocol for implementers of another store.
//! - [`transparency`] — C2SP checkpoints, witness requests and replies,
//!   policies, the auditor's verifier, proof bundles and tiles.
//! - `testing` (feature `testing`) — the conformance suite every store
//!   passes.
//!
//! The user guide covers the same ground:
//! [Start](https://hupe1980.github.io/doubleentry/docs/start/),
//! [Concepts](https://hupe1980.github.io/doubleentry/docs/concepts/),
//! [Reporting](https://hupe1980.github.io/doubleentry/docs/reporting/),
//! [Operating](https://hupe1980.github.io/doubleentry/docs/operating/) and
//! [Assurance](https://hupe1980.github.io/doubleentry/docs/assurance/).
//!
//! # Example
//!
//! ```
//! use doubleentry::{AccountKind, Entry, Journal, LedgerId, Rejection, amount, date, currency::EUR};
//!
//! let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
//! books.declare_currency(EUR, 2, date!("2026-01-01"))?;
//! books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
//! books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
//! books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))?;
//!
//! let invoice = Entry::new("invoice-2026-0001", date!("2026-03-15"))
//!     .debit("Assets:Cash", amount!("1190.00"), EUR)
//!     .credit("Income:Sales", amount!("1190.00"), EUR);
//! let recorded = books.record(invoice.clone())?;
//! assert!(!recorded.replayed);
//! assert!(books.record(invoice)?.replayed); // a retry changes nothing
//!
//! // An unbalanced draft is refused with a stable code.
//! let refused = books.record(
//!     Entry::new("invoice-2026-0002", date!("2026-03-16"))
//!         .debit("Assets:Cash", amount!("10.00"), EUR)
//!         .credit("Income:Sales", amount!("9.00"), EUR),
//! );
//! assert_eq!(refused.err().as_ref().and_then(|e| e.rejection()).map(Rejection::code), Some(106));
//!
//! books.seal_period("2026-03")?;
//! books.audit()?;
//! # Ok::<(), doubleentry::Error>(())
//! ```
//!
//! # A guide
//!
//! 1. **Set up the books** — [`declare_currency`](Journal::declare_currency)
//!    (with its exponent), [`define_months`](Journal::define_months) (or
//!    [`define_period`](Journal::define_period)), [`open`](Journal::open)
//!    accounts by path, and optionally [`set_limit`](Journal::set_limit).
//!    Every setup call is idempotent — repeating it with the same definition
//!    writes nothing, a different one is refused — so a service runs it on
//!    every start.
//! 2. **Draft and record** — [`Entry`] builders are infallible; a malformed
//!    draft is refused by [`record`](Journal::record) with a [`Rejection`]
//!    and a stable code. The idempotency key is the entry's identity: a
//!    retry of the same draft returns the original [`Recorded`] with
//!    `replayed == true`. [`batch`](Journal::batch) records entries,
//!    clearings and resets atomically, in order, in one transaction.
//! 3. **Read** — [`balance`](Journal::balance) and the reports take a
//!    [`BalanceQuery`] (a log prefix, a date window by booking or value
//!    date, a period, dimension filters); both façades answer them alike.
//! 4. **Close and prove** — [`seal_period`](Journal::seal_period) commits
//!    every balance through the period's end; periods are sealed in
//!    calendar order, and [`seal_through`](Journal::seal_through) seals every
//!    open period up to one in a single call;
//!    [`prove_entry`](Journal::prove_entry),
//!    [`prove_consistency`](Journal::prove_consistency) and
//!    [`prove_balance`](Journal::prove_balance) give proofs that verify
//!    against a [`TreeHead`] alone; [`transparency`] signs heads and lets
//!    independent witnesses cosign them.
//! 5. **Keep it durable** — the same calls on a [`Ledger`] over a
//!    [`store`] (`MemoryStore`, SQLite, PostgreSQL), each `.await`ed; the
//!    store runs the rule core inside its write transaction.
//!    [`audit`](Ledger::audit) replays the log and compares.
//!
//! # Errors
//!
//! Every operation of either façade returns [`Result`], whose error is the
//! one [`Error`]. Match on its variant to decide what to do:
//!
//! | Error | [`is_transient`](Error::is_transient) | [`may_have_applied`](Error::may_have_applied) | What the caller does |
//! |---|---|---|---|
//! | [`Rejected(r)`](Error::Rejected) | false | false | show `r` and its code; fix the input; never retry unchanged |
//! | [`BatchRejected { index, rejection }`](Error::BatchRejected) | false | false | nothing of the batch was written; fix item `index` (or drop it) and record the batch again |
//! | [`Value(e)`](Error::Value), [`Amount(e)`](Error::Amount) | false | false | fix the value or the arithmetic at the call site (never from a ledger operation) |
//! | [`Query(e)`](Error::Query) | false | false | fix the question: the entry, period, window or position asked for |
//! | [`Transparency(e)`](Error::Transparency) | false | false | the artefact is refused: a forged, stale or foreign checkpoint, bundle or reply; never retry it unchanged |
//! | [`Integrity(e)`](Error::Integrity) | false | false | stop writing; investigate position `e.position()`; restore from a backup and [`audit`](Journal::audit) |
//! | [`Backend(b)`](Error::Backend), transient, definite | true | false | retry later with the **same** draft |
//! | [`Backend(b)`](Error::Backend), outcome unknown | either | true | retry with the **same** draft: it applies once, or comes back with `replayed == true` |
//! | [`Backend(b)`](Error::Backend), permanent, definite | false | false | do not retry automatically: an operator problem (configuration, permissions) |
//! | [`Config(e)`](Error::Config) | false | false | the store holds another ledger, scale or schema: fix the URL, schema name, ledger id or `P` — the data is fine |
//!
//! `{:?}` on an [`Error`] prints its `Display` line, stable code included, so
//! `fn main() -> doubleentry::Result<()>` reports `rejected: [427] …`.
//!
//! ```
//! use doubleentry::prelude::*;
//!
//! # fn record(books: &mut Journal, entry: Entry) -> Result<()> {
//! match books.record(entry) {
//!     Ok(recorded) if recorded.replayed => {} // a retry: nothing written
//!     Ok(_) => {}
//!     Err(Error::Rejected(rejection)) => eprintln!("refused: {rejection}"),
//!     Err(error) if error.is_transient() => return Err(error), // retry later, same draft
//!     Err(error) => return Err(error),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Feature flags
//!
//! All features are off by default.
//!
#![doc = document_features::document_features!()]
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

mod core;
pub mod error;
pub mod export;
mod facade;
mod journal;
mod ledger;
pub mod log;
mod model;
pub mod prelude;
pub mod report;
pub mod store;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(feature = "tracing")]
mod trace;
pub mod transparency;

/// Hooks for the `fuzz/` package (cargo-fuzz sets `cfg(fuzzing)`): parsers
/// that are not public API, reached only from the fuzz targets. Not built
/// otherwise, so invisible to docs, `cargo public-api` and semver.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz_api {
    /// Decodes a seal balance-tree leaf (`0x81 ‖ i128 debit ‖ i128 credit ‖
    /// u64 postings`) as a sealed-balance bundle does, refusing anything
    /// that does not re-encode to the same bytes.
    ///
    /// # Errors
    ///
    /// The bundle's layout error for anything but a canonical balance leaf.
    pub fn decode_balance_leaf<const P: u8>(
        leaf: &[u8],
    ) -> Result<crate::Balance<P>, crate::transparency::Error> {
        crate::transparency::decode_balance_leaf(leaf)
    }
}

pub use crate::core::{
    AccountSpec, AuditReport, BatchItem, BatchReceipt, Cleared, Clearing, HoldState, HoldStatus,
    Recorded, ResolutionKind, SealReceipt, validate,
};
pub use error::rejection::Rejection;
pub use error::top::{Error, Result};
pub use facade::{Account, Period};
pub use journal::Journal;
pub use ledger::Ledger;
pub use log::proof::TreeHead;
pub use model::{
    AccountHandle, AccountKind, AccountPath, AccountRef, Amount, Balance, BalanceLimit,
    CaptureTime, Currency, Dimensions, Document, Entry, EntryId, EntryRecord, IdempotencyKey,
    Label, Layer, LedgerId, LedgerPolicy, MemoSalt, PeriodId, PostAmount, Posting, PostingRef,
    Rate, Rounding, Side,
};
pub use report::query::BalanceQuery;
pub use store::read::{HoldCursor, Page, PageRead};

/// The currency codes as constants — `use doubleentry::currency::JPY;`. The
/// [`prelude`] brings the five most common; any other ISO 4217 code is
/// [`Currency::new`] or the [`currency!`] literal.
pub mod currency {
    pub use crate::model::{
        AUD, BHD, BRL, CAD, CHF, CNY, CZK, DKK, EUR, GBP, HKD, HUF, INR, JPY, KRW, KWD, MXN, NOK,
        NZD, PLN, SEK, SGD, TRY, USD, ZAR,
    };
}

/// The log's record kinds and their canonical byte form — what an exporter
/// or an independent verifier decodes; bookkeeping code rarely names them.
///
/// Every leaf is `kind ‖ canonical(record)`: `Record::decode` reads one
/// strictly, `Record::to_leaf_bytes` writes it.
pub mod record {
    pub use crate::model::{
        AccountAction, AccountChange, AccountEvent, AccountRegistration, ClearingAction,
        ClearingEvent, CurrencyDeclaration, DecodeError, EntryParts, FORMAT, Fingerprint, Genesis,
        HoldAction, MAX_LEAF_BYTES, MemoCommitment, PeriodDefinition, PolicyEvent, PostingRecord,
        Record, RecordKind, Seal, SealOrderError, SealParts, TreeHeadEntry,
    };
}

/// The calendar date every booking date, value date and effective date is
/// (re-exported from `time`, so [`date!`] needs no dependency of yours).
pub use time::Date;

/// A [`Date`] from a `YYYY-MM-DD` literal, checked at compile time.
///
/// ```
/// use doubleentry::{Date, date};
///
/// const MARCH_15: Date = date!("2026-03-15");
/// assert_eq!(MARCH_15.to_string(), "2026-03-15");
/// assert_eq!(date!("2024-02-29").ordinal(), 60); // a leap day
/// ```
///
/// A day the month does not have:
///
/// ```compile_fail
/// let _ = doubleentry::date!("2026-02-30");
/// ```
///
/// Anything but four, two and two digits:
///
/// ```compile_fail
/// let _ = doubleentry::date!("2026-3-1");
/// ```
#[macro_export]
macro_rules! date {
    ($literal:literal) => {
        const {
            // Total at run time: this panic can only fire while the inline
            // `const` block is evaluated, i.e. as a compile error.
            #[allow(clippy::panic)]
            let date = match $crate::__date($literal) {
                ::core::option::Option::Some(date) => date,
                ::core::option::Option::None => ::core::panic!(::core::concat!(
                    "date!(",
                    ::core::stringify!($literal),
                    "): expected an existing date written YYYY-MM-DD"
                )),
            };
            date
        }
    };
}

/// The date a `YYYY-MM-DD` text names; `None` for any other text or a day
/// that does not exist. The engine of [`date!`].
#[doc(hidden)]
#[must_use]
pub const fn __date(text: &str) -> Option<Date> {
    let [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = text.as_bytes() else {
        return None;
    };
    let (Some(year), Some(month), Some(day)) = (
        digits(&[*y0, *y1, *y2, *y3]),
        digits(&[*m0, *m1]),
        digits(&[*d0, *d1]),
    ) else {
        return None;
    };
    let month = match month {
        1 => time::Month::January,
        2 => time::Month::February,
        3 => time::Month::March,
        4 => time::Month::April,
        5 => time::Month::May,
        6 => time::Month::June,
        7 => time::Month::July,
        8 => time::Month::August,
        9 => time::Month::September,
        10 => time::Month::October,
        11 => time::Month::November,
        12 => time::Month::December,
        _ => return None,
    };
    // At most four and two digits: 9 999 fits an `i32`, 99 a `u8`.
    match Date::from_calendar_date(year as i32, month, day as u8) {
        Ok(date) => Some(date),
        Err(_) => None,
    }
}

/// The value of a run of ASCII digits; `None` for any other byte.
const fn digits(bytes: &[u8]) -> Option<u16> {
    let mut value: u16 = 0;
    let mut rest = bytes;
    while let [first, tail @ ..] = rest {
        if !first.is_ascii_digit() {
            return None;
        }
        value = match value.checked_mul(10) {
            Some(value) => value,
            None => return None,
        };
        value = match value.checked_add(first.wrapping_sub(b'0') as u16) {
            Some(value) => value,
            None => return None,
        };
        rest = tail;
    }
    Some(value)
}

/// Every code block of the README, compiled and run as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeExamples;
