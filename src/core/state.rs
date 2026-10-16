//! What the core reads ([`StateView`]) and the in-memory state that
//! implements it — and projects, for an audit, every table a store keeps.
//!
//! # Version selection
//!
//! Master data is a history of versions per (subject, field): an account's
//! window, limit and class, each currency's exponent, the ledger policy. A
//! field's effective dates never decrease in log order, so the version **in
//! force on a date** is the last one whose effective date is on or before it,
//! and the **latest** version is the last one. Entries are checked against
//! the window, class, exponent and policy in force on their booking date, and
//! against the latest limit, since a limit constrains the cumulative balance,
//! which already holds later-dated entries.

mod mem;

use core::fmt;

use time::Date;

use super::holds::HoldView;
use super::writeset::SealReceipt;
use crate::model::{
    AccountHandle, AccountKind, AccountPath, AccountRegistration, Amount, Balance, BalanceLimit,
    ClearingEvent, Currency, Dimensions, EntryId, EntryRecord, Fingerprint, IdempotencyKey, Label,
    Layer, LedgerId, LedgerPolicy, PeriodId, PostingRef, Side,
};

pub(crate) use mem::{Line, MemState};

/// One maintained balance: an account, a currency, a layer.
///
/// Ordered by handle, then currency, then layer — the order of balance
/// deltas, seal trees and limit checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceKey {
    /// The account.
    pub handle: AccountHandle,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
}

impl fmt::Display for BalanceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.handle, self.currency, self.layer)
    }
}

/// The read-only state the core decides against: the ledger as it is at the
/// position the next record is appended.
///
/// Synchronous: a store loads the facts an operation needs into a
/// [`Snapshot`](super::Snapshot) (asking [`plan`](super::plan) which ones) and
/// the core reads them from there. Every method is bounded by the operation
/// except the three that serve sealing and year-end closing.
///
/// A view of a created ledger always holds its genesis record, so
/// [`Header::size`] is at least 1 for every operation but genesis itself.
pub trait StateView<const P: u8> {
    /// The ledger's counters; always available.
    fn header(&self) -> Header<P>;
    /// The entry recorded under a key.
    fn entry_by_key(&self, key: &IdempotencyKey) -> Option<EntryMeta>;
    /// The account registered under a path.
    fn account_by_path(&self, path: &AccountPath) -> Option<AccountView>;
    /// The account registered under a handle.
    fn account(&self, handle: AccountHandle) -> Option<AccountView>;
    /// An account's terms in force on a date; `None` before it opened.
    fn account_terms(&self, handle: AccountHandle, on: Date) -> Option<Terms>;
    /// A currency's exponent in force on a date; `None` while undeclared.
    fn currency(&self, code: Currency, on: Date) -> Option<u8>;
    /// A currency's latest declaration.
    fn currency_latest(&self, code: Currency) -> Option<Versioned<u8>>;
    /// The policy in force on a date (the permissive default before the
    /// first policy record).
    fn policy(&self, on: Date) -> LedgerPolicy;
    /// The latest policy record.
    fn policy_latest(&self) -> Option<Versioned<LedgerPolicy>>;
    /// The period containing a date.
    fn period_at(&self, date: Date) -> Option<PeriodView<P>>;
    /// The period with an id.
    fn period(&self, id: &PeriodId) -> Option<PeriodView<P>>;
    /// The earliest period without a seal (sealing names it when a period
    /// is sealed out of order).
    fn first_unsealed(&self) -> Option<PeriodView<P>>;
    /// The entry with an id.
    fn entry(&self, id: EntryId) -> Option<EntryView<P>>;
    /// The entry that reversed an entry.
    fn reversed_by(&self, id: EntryId) -> Option<EntryId>;
    /// The hold opened by an entry, open or resolved; `None` when the entry
    /// is not a hold.
    fn hold(&self, id: EntryId) -> Option<HoldView<P>>;
    /// The amount open holds have reserved as debits on (account, currency).
    fn outstanding_hold_debits(&self, handle: AccountHandle, currency: Currency) -> Amount<P>;
    /// The amount open holds have reserved as credits on (account, currency).
    fn outstanding_hold_credits(&self, handle: AccountHandle, currency: Currency) -> Amount<P>;
    /// A maintained balance (zero when nothing was posted to it).
    fn balance(&self, key: BalanceKey) -> Balance<P>;
    /// The clearing record (apply or reset) recorded under a key.
    fn clearing_by_key(&self, key: &IdempotencyKey) -> Option<ClearingView<P>>;
    /// The clearing record at a position.
    fn clearing_at(&self, position: u64) -> Option<ClearingView<P>>;
    /// The amount cleared against a posting, net of resets.
    fn applied(&self, posting: PostingRef) -> Amount<P>;
    /// Every non-zero maintained balance, ascending by key (sealing).
    fn all_balances(&self) -> Vec<(BalanceKey, Balance<P>)>;
    /// Every posting booked after a date (sealing).
    fn postings_after(&self, date: Date) -> Vec<Movement<P>>;
    /// The Settled balance through a date of every (income or expense
    /// account, currency, dimension set) with postings, ascending
    /// (year-end closing).
    fn pnl_slices(&self, through: Date) -> Vec<Slice<P>>;
    /// Whether every lookup so far was answered: `false` only on a
    /// [`Snapshot`](super::Snapshot) still missing facts, i.e. while
    /// [`plan`](super::plan) runs.
    fn complete(&self) -> bool {
        true
    }
}

/// A ledger's counters and calendar position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header<const P: u8> {
    /// The ledger's id.
    pub ledger: LedgerId,
    /// The number of records: the position of the next one.
    pub size: u64,
    /// The number of registered accounts: the next handle.
    pub accounts: u32,
    /// The number of entries.
    pub entries: u64,
    /// The last day of the last sealed period.
    pub watermark: Option<Date>,
    /// The latest booking date of any posting; `None` before the first. A
    /// policy record may only take effect after it (431).
    pub last_booking: Option<Date>,
    /// The last defined period.
    pub last_period: Option<PeriodView<P>>,
}

/// What the idempotency index holds for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryMeta {
    /// The entry's id.
    pub id: EntryId,
    /// The entry's position.
    pub position: u64,
    /// The fingerprint of the draft recorded under the key; `None` when it is
    /// unknown (after memo erasure, or replayed without side data), which
    /// makes every later submission of the key a key conflict.
    pub fingerprint: Option<Fingerprint>,
}

/// An account's first and last open day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Window {
    /// The first open day.
    pub open_from: Date,
    /// The last open day; `None` while open-ended.
    pub open_to: Option<Date>,
}

impl Window {
    /// Whether the account is open on `date`.
    #[must_use]
    pub fn contains(&self, date: Date) -> bool {
        self.open_from <= date && self.open_to.is_none_or(|open_to| date <= open_to)
    }
}

/// A registered account: its registration, its latest master data, and what
/// its postings and descendants imply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    /// The registration as recorded.
    pub registration: AccountRegistration,
    /// Each field's latest version.
    pub latest: LatestTerms,
    /// Whether no registered account lies below it.
    pub is_leaf: bool,
    /// The latest booking date of its postings; `None` without postings.
    pub last_booked: Option<Date>,
}

impl AccountView {
    /// The handle.
    #[must_use]
    pub const fn handle(&self) -> AccountHandle {
        self.registration.handle
    }

    /// The path.
    #[must_use]
    pub const fn path(&self) -> &AccountPath {
        &self.registration.path
    }

    /// The kind.
    #[must_use]
    pub const fn kind(&self) -> AccountKind {
        self.registration.kind
    }
}

/// An account's master data in force on one date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terms {
    /// The class.
    pub class: Option<Label>,
    /// The open window.
    pub window: Window,
    /// The balance limit.
    pub limit: BalanceLimit,
}

/// The latest version of each account field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestTerms {
    /// The open window.
    pub window: Versioned<Window>,
    /// The balance limit.
    pub limit: Versioned<BalanceLimit>,
    /// The class.
    pub class: Versioned<Option<Label>>,
}

/// One version of a master-data field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versioned<T> {
    /// The value.
    pub value: T,
    /// From when it applies.
    pub effective: Date,
    /// The record that set it.
    pub position: u64,
}

/// A defined period and whether it is sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodView<const P: u8> {
    /// The id.
    pub id: PeriodId,
    /// The first day.
    pub start: Date,
    /// The last day.
    pub end: Date,
    /// The definition's position.
    pub position: u64,
    /// The seal, once the period is sealed.
    pub sealed: Option<SealReceipt<P>>,
}

/// A recorded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryView<const P: u8> {
    /// Its position.
    pub position: u64,
    /// The record.
    pub record: EntryRecord<P>,
}

/// A recorded clearing apply or reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearingView<const P: u8> {
    /// Its position.
    pub position: u64,
    /// The record.
    pub event: ClearingEvent<P>,
    /// For an apply: the position of the reset that released it.
    pub reset_by: Option<u64>,
}

/// One posting as sealing reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Movement<const P: u8> {
    /// The balance it moved.
    pub key: BalanceKey,
    /// Its side.
    pub side: Side,
    /// Its amount.
    pub amount: Amount<P>,
}

/// The Settled balance of one (account, currency, dimension set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice<const P: u8> {
    /// The account.
    pub handle: AccountHandle,
    /// The currency.
    pub currency: Currency,
    /// The exact dimension set of the postings.
    pub dimensions: Dimensions,
    /// Their gross balance.
    pub balance: Balance<P>,
}
