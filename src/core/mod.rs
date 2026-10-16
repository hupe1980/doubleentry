//! The rule core: one family of pure functions decides every write.
//!
//! [`execute`] takes a read-only [`StateView`] of the ledger — its state at
//! the position the next record is appended — and one [`Op`], and returns
//! either an [`Outcome`] (`Append` exactly one record with everything a
//! store maintains for it, `Batch` for an atomic batch, or `Unchanged` when
//! the operation is already in effect) or exactly one
//! [`Rejection`](crate::Rejection). It reads no clock,
//! draws no randomness, performs no I/O, has no `async` and no `unsafe`, and
//! iterates only ordered maps: equal views and operations give byte-equal
//! outcomes. The façades build an `Op` and apply the outcome; no façade and
//! no store implements a rule.
//!
//! # Stages
//!
//! Every operation runs the same five stages in order, and a stage runs only
//! when every earlier one passed:
//!
//! 1. **Structural** — the input alone: key, postings, balance per
//!    (currency, layer), labels, sizes, the shape of a hold. Every
//!    violation is computed; the one with the lowest code wins, and
//!    [`validate`] lists them all.
//! 2. **Idempotency** — a key already recorded with the same content is a
//!    replay (`Unchanged`, not an error), with other content a key conflict.
//!    It precedes every state rule, so a retry after the period was sealed
//!    or the account closed still returns the original receipt.
//! 3. **Context** — the state on the booking date: a defined, unsealed
//!    period; existing, leaf, open accounts; declared currencies and their
//!    exponents; the policy's required dimensions and value-date drift.
//! 4. **References** — the records the input names: reversal targets, the
//!    hold a resolution names (whose postings the core then derives),
//!    cleared postings, master data and the calendar.
//! 5. **Limits** — overflow of any gross figure, then balance limits.
//!
//! Within a stage the checks run in a fixed order, so an input that breaks
//! several rules always gets the same rejection.
//!
//! # Records
//!
//! Every write is one record of the log: master data ([`AccountSpec`],
//! currencies, the policy), periods, entries, clearings and seals. The
//! calendar, the account registry, the open items and the sealed set are
//! folds of the log; "sealed" means a seal record exists. An `audit` replays
//! every leaf through this core ([`Replayer`]).
//!
//! # Nothing is validated anywhere else
//!
//! There is no pre-validated entry, so nothing validated against other
//! state, or earlier, can be recorded:
//!
//! ```compile_fail
//! use doubleentry::{Entry, Journal, LedgerId, date};
//! let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
//! books.record_validated(Entry::new("k", date!("2026-01-01"))); // stale-ok
//! ```
//!
//! ```compile_fail
//! use doubleentry::{Entry, date};
//! let _ = Entry::<2>::new("k", date!("2026-01-01")).adopt_verified([0u8; 32]); // stale-ok
//! ```
//!
//! And no period is sealed but by its seal record:
//!
//! ```compile_fail
//! use doubleentry::{Journal, LedgerId};
//! let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
//! books.calendar_mut(); // stale-ok
//! ```
//!
//! ```compile_fail
//! use doubleentry::{Journal, LedgerId};
//! let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
//! books.transition_period("2026-03", "Sealed"); // stale-ok
//! ```

mod audit;
mod batch;
mod calendar;
mod check;
mod clearing;
mod closing;
mod context;
mod holds;
mod limits;
mod master;
mod references;
mod seal;
mod snapshot;
mod state;
mod structural;
#[cfg(test)]
mod tests;
mod writeset;

pub use audit::{AuditFailure, AuditFailureKind, AuditReport, ReplayLeaf, Replayer};
pub use check::{ChangeInput, Op, execute, validate};
pub use holds::{HoldEffect, HoldState, HoldStatus, HoldView, ResolutionKind};
pub use master::AccountSpec;
pub use snapshot::{Fact, Read, Snapshot, plan};
pub use state::{
    AccountView, BalanceKey, ClearingView, EntryMeta, EntryView, Header, LatestTerms, Movement,
    PeriodView, Slice, StateView, Terms, Versioned, Window,
};
pub use writeset::{
    AccountTerm, BalanceDelta, Done, IndexRow, MemoPlain, Outcome, Outstanding, PostingRow,
    Recorded, SealReceipt, WriteSet,
};

pub(crate) use calendar::months;
pub(crate) use check::{account_ref, check};
pub(crate) use holds::posted;
pub(crate) use references::reversal;
pub(crate) use state::{Line, MemState};
pub(crate) use structural::build_error as build_rejection;
