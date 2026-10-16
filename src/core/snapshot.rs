//! The store protocol: how an async store runs the synchronous core inside
//! its write transaction.
//!
//! A [`Snapshot`] is a [`StateView`] over the facts loaded into it. Every
//! lookup it cannot answer is recorded as a [`Read`] and answered with
//! "absent" (or zero, or the permissive policy), so one run discovers every
//! read that does not depend on another. [`plan`] runs the operation and
//! returns those reads; the store loads them as [`Fact`]s and plans again
//! until nothing is missing (at most [`Op::max_rounds`] rounds), then calls
//! [`execute`] on the snapshot. The read set is derived from the code that
//! decides, so it cannot drift from the rules.
//!
//! ```
//! use doubleentry::store::{Fact, Header, Op, Outcome, Read, Snapshot, execute, plan};
//! use doubleentry::{AccountKind, AccountSpec, LedgerId};
//! use time::macros::date;
//!
//! // A ledger holding only its genesis record.
//! let header = Header {
//!     ledger: LedgerId::new("example.com/acme")?,
//!     size: 1,
//!     accounts: 0,
//!     entries: 0,
//!     watermark: None,
//!     last_booking: None,
//!     last_period: None,
//! };
//! let op: Op<2> = Op::Register {
//!     spec: AccountSpec::new("Assets:Cash", AccountKind::Asset, date!(2026 - 01 - 01)),
//!     actor: "setup".into(),
//! };
//!
//! let mut snapshot = Snapshot::new(header);
//! let reads = plan(&snapshot, &op);
//! assert_eq!(
//!     reads,
//!     [Read::AccountByPath("Assets".parse()?), Read::AccountByPath("Assets:Cash".parse()?)]
//! );
//! for read in reads {
//!     if let Read::AccountByPath(path) = read {
//!         snapshot.insert(Fact::AccountByPath(path, None)); // the store found no such account
//!     }
//! }
//! assert!(plan(&snapshot, &op).is_empty());
//! assert!(matches!(execute(&snapshot, &op), Ok(Outcome::Append(_))));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::fmt;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use time::Date;

use super::check::{Op, execute};
use super::holds::HoldView;
use super::state::{
    AccountView, BalanceKey, ClearingView, EntryMeta, EntryView, Header, Movement, PeriodView,
    Slice, StateView, Terms, Versioned,
};
use crate::model::{
    AccountHandle, AccountPath, Amount, Balance, Currency, EntryId, IdempotencyKey, LedgerPolicy,
    PeriodId, PostingRef,
};

/// One lookup the core made: a [`StateView`] method and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Read {
    /// [`StateView::entry_by_key`].
    EntryByKey(IdempotencyKey),
    /// [`StateView::account_by_path`].
    AccountByPath(AccountPath),
    /// [`StateView::account`].
    Account(AccountHandle),
    /// [`StateView::account_terms`].
    AccountTerms(AccountHandle, Date),
    /// [`StateView::currency`].
    Currency(Currency, Date),
    /// [`StateView::currency_latest`].
    CurrencyLatest(Currency),
    /// [`StateView::policy`].
    Policy(Date),
    /// [`StateView::policy_latest`].
    PolicyLatest,
    /// [`StateView::period_at`].
    PeriodAt(Date),
    /// [`StateView::period`].
    Period(PeriodId),
    /// [`StateView::first_unsealed`].
    FirstUnsealed,
    /// [`StateView::entry`].
    Entry(EntryId),
    /// [`StateView::reversed_by`].
    ReversedBy(EntryId),
    /// [`StateView::hold`].
    Hold(EntryId),
    /// [`StateView::outstanding_hold_debits`].
    OutstandingHoldDebits(AccountHandle, Currency),
    /// [`StateView::outstanding_hold_credits`].
    OutstandingHoldCredits(AccountHandle, Currency),
    /// [`StateView::balance`].
    Balance(BalanceKey),
    /// [`StateView::clearing_by_key`].
    ClearingByKey(IdempotencyKey),
    /// [`StateView::clearing_at`].
    ClearingAt(u64),
    /// [`StateView::applied`].
    Applied(PostingRef),
    /// [`StateView::all_balances`].
    AllBalances,
    /// [`StateView::postings_after`].
    PostingsAfter(Date),
    /// [`StateView::pnl_slices`].
    PnlSlices(Date),
}

/// The answer to one [`Read`], as a store loads it into a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fact<const P: u8> {
    /// [`Read::EntryByKey`].
    EntryByKey(IdempotencyKey, Option<EntryMeta>),
    /// [`Read::AccountByPath`].
    AccountByPath(AccountPath, Option<AccountView>),
    /// [`Read::Account`].
    Account(AccountHandle, Option<AccountView>),
    /// [`Read::AccountTerms`].
    AccountTerms(AccountHandle, Date, Option<Terms>),
    /// [`Read::Currency`].
    Currency(Currency, Date, Option<u8>),
    /// [`Read::CurrencyLatest`].
    CurrencyLatest(Currency, Option<Versioned<u8>>),
    /// [`Read::Policy`].
    Policy(Date, LedgerPolicy),
    /// [`Read::PolicyLatest`].
    PolicyLatest(Option<Versioned<LedgerPolicy>>),
    /// [`Read::PeriodAt`].
    PeriodAt(Date, Option<PeriodView<P>>),
    /// [`Read::Period`].
    Period(PeriodId, Option<PeriodView<P>>),
    /// [`Read::FirstUnsealed`].
    FirstUnsealed(Option<PeriodView<P>>),
    /// [`Read::Entry`].
    Entry(EntryId, Option<EntryView<P>>),
    /// [`Read::ReversedBy`].
    ReversedBy(EntryId, Option<EntryId>),
    /// [`Read::Hold`].
    Hold(EntryId, Option<HoldView<P>>),
    /// [`Read::OutstandingHoldDebits`].
    OutstandingHoldDebits(AccountHandle, Currency, Amount<P>),
    /// [`Read::OutstandingHoldCredits`].
    OutstandingHoldCredits(AccountHandle, Currency, Amount<P>),
    /// [`Read::Balance`].
    Balance(BalanceKey, Balance<P>),
    /// [`Read::ClearingByKey`].
    ClearingByKey(IdempotencyKey, Option<ClearingView<P>>),
    /// [`Read::ClearingAt`].
    ClearingAt(u64, Option<ClearingView<P>>),
    /// [`Read::Applied`].
    Applied(PostingRef, Amount<P>),
    /// [`Read::AllBalances`].
    AllBalances(Vec<(BalanceKey, Balance<P>)>),
    /// [`Read::PostingsAfter`].
    PostingsAfter(Date, Vec<Movement<P>>),
    /// [`Read::PnlSlices`].
    PnlSlices(Date, Vec<Slice<P>>),
}

impl<const P: u8> Fact<P> {
    /// The read this fact answers.
    #[must_use]
    pub fn read(&self) -> Read {
        match self {
            Self::EntryByKey(key, _) => Read::EntryByKey(key.clone()),
            Self::AccountByPath(path, _) => Read::AccountByPath(path.clone()),
            Self::Account(handle, _) => Read::Account(*handle),
            Self::AccountTerms(handle, on, _) => Read::AccountTerms(*handle, *on),
            Self::Currency(code, on, _) => Read::Currency(*code, *on),
            Self::CurrencyLatest(code, _) => Read::CurrencyLatest(*code),
            Self::Policy(on, _) => Read::Policy(*on),
            Self::PolicyLatest(_) => Read::PolicyLatest,
            Self::PeriodAt(date, _) => Read::PeriodAt(*date),
            Self::Period(id, _) => Read::Period(id.clone()),
            Self::FirstUnsealed(_) => Read::FirstUnsealed,
            Self::Entry(id, _) => Read::Entry(*id),
            Self::ReversedBy(id, _) => Read::ReversedBy(*id),
            Self::Hold(id, _) => Read::Hold(*id),
            Self::OutstandingHoldDebits(handle, code, _) => {
                Read::OutstandingHoldDebits(*handle, *code)
            }
            Self::OutstandingHoldCredits(handle, code, _) => {
                Read::OutstandingHoldCredits(*handle, *code)
            }
            Self::Balance(key, _) => Read::Balance(*key),
            Self::ClearingByKey(key, _) => Read::ClearingByKey(key.clone()),
            Self::ClearingAt(position, _) => Read::ClearingAt(*position),
            Self::Applied(posting, _) => Read::Applied(*posting),
            Self::AllBalances(_) => Read::AllBalances,
            Self::PostingsAfter(date, _) => Read::PostingsAfter(*date),
            Self::PnlSlices(date, _) => Read::PnlSlices(*date),
        }
    }

    /// The fact a view gives for a read.
    #[must_use]
    pub fn answer(view: &impl StateView<P>, read: &Read) -> Self {
        match read {
            Read::EntryByKey(key) => Self::EntryByKey(key.clone(), view.entry_by_key(key)),
            Read::AccountByPath(path) => {
                Self::AccountByPath(path.clone(), view.account_by_path(path))
            }
            Read::Account(handle) => Self::Account(*handle, view.account(*handle)),
            Read::AccountTerms(handle, on) => {
                Self::AccountTerms(*handle, *on, view.account_terms(*handle, *on))
            }
            Read::Currency(code, on) => Self::Currency(*code, *on, view.currency(*code, *on)),
            Read::CurrencyLatest(code) => Self::CurrencyLatest(*code, view.currency_latest(*code)),
            Read::Policy(on) => Self::Policy(*on, view.policy(*on)),
            Read::PolicyLatest => Self::PolicyLatest(view.policy_latest()),
            Read::PeriodAt(date) => Self::PeriodAt(*date, view.period_at(*date)),
            Read::Period(id) => Self::Period(id.clone(), view.period(id)),
            Read::FirstUnsealed => Self::FirstUnsealed(view.first_unsealed()),
            Read::Entry(id) => Self::Entry(*id, view.entry(*id)),
            Read::ReversedBy(id) => Self::ReversedBy(*id, view.reversed_by(*id)),
            Read::Hold(id) => Self::Hold(*id, view.hold(*id)),
            Read::OutstandingHoldDebits(handle, code) => Self::OutstandingHoldDebits(
                *handle,
                *code,
                view.outstanding_hold_debits(*handle, *code),
            ),
            Read::OutstandingHoldCredits(handle, code) => Self::OutstandingHoldCredits(
                *handle,
                *code,
                view.outstanding_hold_credits(*handle, *code),
            ),
            Read::Balance(key) => Self::Balance(*key, view.balance(*key)),
            Read::ClearingByKey(key) => Self::ClearingByKey(key.clone(), view.clearing_by_key(key)),
            Read::ClearingAt(position) => Self::ClearingAt(*position, view.clearing_at(*position)),
            Read::Applied(posting) => Self::Applied(*posting, view.applied(*posting)),
            Read::AllBalances => Self::AllBalances(view.all_balances()),
            Read::PostingsAfter(date) => Self::PostingsAfter(*date, view.postings_after(*date)),
            Read::PnlSlices(date) => Self::PnlSlices(*date, view.pnl_slices(*date)),
        }
    }
}

/// A [`StateView`] over loaded facts that records every lookup it could not
/// answer.
pub struct Snapshot<const P: u8> {
    header: Header<P>,
    facts: BTreeMap<Read, Fact<P>>,
    misses: Cell<BTreeSet<Read>>,
}

impl<const P: u8> fmt::Debug for Snapshot<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("header", &self.header)
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

impl<const P: u8> Snapshot<P> {
    /// A snapshot holding only the ledger's header.
    #[must_use]
    pub fn new(header: Header<P>) -> Self {
        Self {
            header,
            facts: BTreeMap::new(),
            misses: Cell::new(BTreeSet::new()),
        }
    }

    /// Loads a fact (replacing an earlier answer to the same read).
    pub fn insert(&mut self, fact: Fact<P>) {
        self.facts.insert(fact.read(), fact);
    }

    /// The answer to `read`, or `None` (recorded as a miss) when it is not
    /// loaded.
    fn find<T>(&self, read: Read, pick: impl FnOnce(&Fact<P>) -> Option<T>) -> Option<T> {
        let found = self.facts.get(&read).and_then(pick);
        if found.is_none() {
            let mut misses = self.misses.take();
            misses.insert(read);
            self.misses.set(misses);
        }
        found
    }

    /// The reads missed since the last call.
    fn take_misses(&self) -> Vec<Read> {
        self.misses.take().into_iter().collect()
    }
}

/// The reads the operation needs that `snapshot` does not hold yet, sorted.
/// Empty means every lookup was answered and [`execute`] on the snapshot is
/// final.
#[must_use]
pub fn plan<const P: u8>(snapshot: &Snapshot<P>, op: &Op<P>) -> Vec<Read> {
    snapshot.take_misses();
    // Only the lookups matter here; the outcome is decided once they are
    // all answered.
    let _decided = execute(snapshot, op);
    snapshot.take_misses()
}

/// A fact's answer from the snapshot: `None` (and a recorded miss) when
/// the snapshot holds no fact for the read.
macro_rules! lookup {
    ($self:ident, $read:expr, $fact:pat => $value:expr) => {
        $self.find($read, |fact| match fact {
            $fact => Some($value),
            _ => None,
        })
    };
}

impl<const P: u8> StateView<P> for Snapshot<P> {
    fn header(&self) -> Header<P> {
        self.header.clone()
    }

    fn complete(&self) -> bool {
        let misses = self.misses.take();
        let complete = misses.is_empty();
        self.misses.set(misses);
        complete
    }

    fn entry_by_key(&self, key: &IdempotencyKey) -> Option<EntryMeta> {
        lookup!(self, Read::EntryByKey(key.clone()), Fact::EntryByKey(_, meta) => *meta).flatten()
    }

    fn account_by_path(&self, path: &AccountPath) -> Option<AccountView> {
        lookup!(self, Read::AccountByPath(path.clone()), Fact::AccountByPath(_, account) => account.clone())
        .flatten()
    }

    fn account(&self, handle: AccountHandle) -> Option<AccountView> {
        lookup!(self, Read::Account(handle), Fact::Account(_, account) => account.clone()).flatten()
    }

    fn account_terms(&self, handle: AccountHandle, on: Date) -> Option<Terms> {
        lookup!(self, Read::AccountTerms(handle, on), Fact::AccountTerms(_, _, terms) => terms.clone())
        .flatten()
    }

    fn currency(&self, code: Currency, on: Date) -> Option<u8> {
        lookup!(self, Read::Currency(code, on), Fact::Currency(_, _, exponent) => *exponent)
            .flatten()
    }

    fn currency_latest(&self, code: Currency) -> Option<Versioned<u8>> {
        lookup!(self, Read::CurrencyLatest(code), Fact::CurrencyLatest(_, latest) => latest.clone())
            .flatten()
    }

    fn policy(&self, on: Date) -> LedgerPolicy {
        lookup!(self, Read::Policy(on), Fact::Policy(_, policy) => policy.clone())
            .unwrap_or_default()
    }

    fn policy_latest(&self) -> Option<Versioned<LedgerPolicy>> {
        lookup!(self, Read::PolicyLatest, Fact::PolicyLatest(latest) => latest.clone()).flatten()
    }

    fn period_at(&self, date: Date) -> Option<PeriodView<P>> {
        lookup!(self, Read::PeriodAt(date), Fact::PeriodAt(_, period) => period.clone()).flatten()
    }

    fn period(&self, id: &PeriodId) -> Option<PeriodView<P>> {
        lookup!(self, Read::Period(id.clone()), Fact::Period(_, period) => period.clone()).flatten()
    }

    fn first_unsealed(&self) -> Option<PeriodView<P>> {
        lookup!(self, Read::FirstUnsealed, Fact::FirstUnsealed(period) => period.clone()).flatten()
    }

    fn entry(&self, id: EntryId) -> Option<EntryView<P>> {
        lookup!(self, Read::Entry(id), Fact::Entry(_, entry) => entry.clone()).flatten()
    }

    fn reversed_by(&self, id: EntryId) -> Option<EntryId> {
        lookup!(self, Read::ReversedBy(id), Fact::ReversedBy(_, by) => *by).flatten()
    }

    fn hold(&self, id: EntryId) -> Option<HoldView<P>> {
        lookup!(self, Read::Hold(id), Fact::Hold(_, hold) => hold.clone()).flatten()
    }

    fn outstanding_hold_debits(&self, handle: AccountHandle, currency: Currency) -> Amount<P> {
        lookup!(self, Read::OutstandingHoldDebits(handle, currency), Fact::OutstandingHoldDebits(_, _, amount) => *amount)
        .unwrap_or(Amount::ZERO)
    }

    fn outstanding_hold_credits(&self, handle: AccountHandle, currency: Currency) -> Amount<P> {
        lookup!(self, Read::OutstandingHoldCredits(handle, currency), Fact::OutstandingHoldCredits(_, _, amount) => *amount)
        .unwrap_or(Amount::ZERO)
    }

    fn balance(&self, key: BalanceKey) -> Balance<P> {
        lookup!(self, Read::Balance(key), Fact::Balance(_, balance) => *balance)
            .unwrap_or(Balance::ZERO)
    }

    fn clearing_by_key(&self, key: &IdempotencyKey) -> Option<ClearingView<P>> {
        lookup!(self, Read::ClearingByKey(key.clone()), Fact::ClearingByKey(_, clearing) => clearing.clone())
        .flatten()
    }

    fn clearing_at(&self, position: u64) -> Option<ClearingView<P>> {
        lookup!(self, Read::ClearingAt(position), Fact::ClearingAt(_, clearing) => clearing.clone())
            .flatten()
    }

    fn applied(&self, posting: PostingRef) -> Amount<P> {
        lookup!(self, Read::Applied(posting), Fact::Applied(_, amount) => *amount)
            .unwrap_or(Amount::ZERO)
    }

    fn all_balances(&self) -> Vec<(BalanceKey, Balance<P>)> {
        lookup!(self, Read::AllBalances, Fact::AllBalances(balances) => balances.clone())
            .unwrap_or_default()
    }

    fn postings_after(&self, date: Date) -> Vec<Movement<P>> {
        lookup!(self, Read::PostingsAfter(date), Fact::PostingsAfter(_, movements) => movements.clone())
        .unwrap_or_default()
    }

    fn pnl_slices(&self, through: Date) -> Vec<Slice<P>> {
        lookup!(self, Read::PnlSlices(through), Fact::PnlSlices(_, slices) => slices.clone())
            .unwrap_or_default()
    }
}
