//! What the read primitives of a [`Store`](super::Store) take and return.

use core::ops::RangeInclusive;

use time::Date;

use super::StoreError;
use crate::core::{BalanceKey, ResolutionKind};
use crate::log::Hash;
use crate::model::{
    AccountHandle, AccountKind, AccountPath, Amount, Balance, Currency, Dimensions, EntryId,
    Fingerprint, IdempotencyKey, Label, LedgerId, MemoSalt, PeriodId, PostingRef, Record,
    RecordKind, Side,
};

/// A log position: the index of a leaf.
pub type Position = u64;

/// The identity of a stored ledger, as [`create`](super::Store::create) and
/// [`open`](super::Store::open) report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerInfo {
    /// The ledger's id (its genesis record's, the C2SP origin).
    pub ledger: LedgerId,
    /// The scale `P` its amounts are stored at.
    pub precision: u8,
    /// The number of records.
    pub size: u64,
    /// The leaf hash of the genesis record.
    pub genesis: Hash,
}

/// One leaf of the log, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// Its position.
    pub position: Position,
    /// Its record kind (the first leaf byte).
    pub kind: RecordKind,
    /// The leaf bytes: `kind ‖ canonical(record)`, at most 65 535 bytes.
    pub bytes: Vec<u8>,
}

impl Leaf {
    /// The record the leaf encodes, decoded strictly.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::Integrity`](super::StoreErrorKind::Integrity) when
    /// the bytes are not one canonical record of scale `P`.
    pub fn record<const P: u8>(&self) -> Result<Record<P>, StoreError> {
        Record::decode(&self.bytes)
            .ok()
            .filter(|record| record.kind() == self.kind)
            .ok_or_else(|| {
                StoreError::integrity("decode", "a leaf does not decode", Some(self.position))
            })
    }
}

/// One page of a paged read.
///
/// Every page is read in one read snapshot: `size` is the log size that
/// snapshot saw, `through` the bound the page honours (positions below it).
/// Pass `next` as the next request's `after` and keep `through` to page over
/// one log prefix while the ledger grows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T, C> {
    /// The items, in the read's order.
    pub items: Vec<T>,
    /// The cursor after the last item, when more may follow.
    pub next: Option<C>,
    /// The log size the read saw.
    pub size: u64,
    /// The bound applied: only records below this position were read.
    pub through: u64,
}

/// A page request: an exclusive cursor, a limit and an optional bound.
///
/// ```
/// use doubleentry::PageRead;
///
/// let first: PageRead<u64> = PageRead::first(100);
/// let next = first.clone().after(42).through(1_000);
/// assert_eq!((next.after, next.limit, next.through), (Some(42), 100, Some(1_000)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRead<C> {
    /// Start after this cursor (`None`: from the beginning).
    pub after: Option<C>,
    /// The most items to return, `1..=1000`.
    pub limit: u32,
    /// Read only records below this position (`None`: the size the read
    /// sees). A bound above that size is clamped.
    pub through: Option<u64>,
}

impl<C> PageRead<C> {
    /// The largest page.
    pub const MAX_LIMIT: u32 = 1000;

    /// The first page of at most `limit` items.
    #[must_use]
    pub const fn first(limit: u32) -> Self {
        Self {
            after: None,
            limit,
            through: None,
        }
    }

    /// The page after `cursor`.
    #[must_use]
    pub fn after(self, cursor: C) -> Self {
        Self {
            after: Some(cursor),
            ..self
        }
    }

    /// The same request, bounded to the records below `through`.
    #[must_use]
    pub fn through(self, through: u64) -> Self {
        Self {
            through: Some(through),
            ..self
        }
    }

    /// The limit, checked.
    pub(crate) fn checked_limit(&self, operation: &'static str) -> Result<u32, StoreError> {
        checked_limit(self.limit, operation)
    }
}

/// A page limit, checked to lie in `1..=1000`.
pub(crate) fn checked_limit(limit: u32, operation: &'static str) -> Result<u32, StoreError> {
    if (1..=PageRead::<()>::MAX_LIMIT).contains(&limit) {
        Ok(limit)
    } else {
        Err(StoreError::backend_message(operation, "limit out of range"))
    }
}

/// The bound a read applies: `through` clamped to the size it sees.
pub(crate) fn bound(through: Option<u64>, size: u64) -> u64 {
    through.map_or(size, |through| through.min(size))
}

/// A request for leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafRead {
    /// The first position.
    pub from: Position,
    /// Read only leaves below this position (`None`: the size the read
    /// sees).
    pub through: Option<u64>,
    /// Only leaves of these kinds (`None`: every kind).
    pub kinds: Option<Vec<RecordKind>>,
    /// The most leaves to return, `1..=1000`.
    pub limit: u32,
}

/// A sealed period as the seal index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealRow {
    /// The period.
    pub period: PeriodId,
    /// The seal record's position.
    pub position: Position,
    /// The seal record.
    pub leaf: Leaf,
}

/// A registered account as the account index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    /// Its handle.
    pub handle: AccountHandle,
    /// Its path.
    pub path: AccountPath,
    /// Its kind.
    pub kind: AccountKind,
    /// The position of its register record.
    pub position: Position,
}

/// How [`entry`](super::Store::entry) finds an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryRef<'a> {
    /// By its id.
    Id(EntryId),
    /// By its idempotency key.
    Key(&'a IdempotencyKey),
}

/// A recorded entry as the entry index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    /// Its position.
    pub position: Position,
    /// Its id.
    pub id: EntryId,
    /// Its idempotency key.
    pub key: IdempotencyKey,
    /// The fingerprint of the draft it was recorded from; `None` once its
    /// memo was erased.
    pub fingerprint: Option<Fingerprint>,
    /// The entry record.
    pub leaf: Leaf,
}

/// A request for holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HoldRead {
    /// One hold by the id of the entry that opened it.
    ById {
        /// The hold's id.
        id: EntryId,
        /// Read only holds opened below this position.
        through: Option<u64>,
    },
    /// The open holds on one account (on either side), by position.
    OpenByAccount {
        /// The account.
        handle: AccountHandle,
        /// The page; cursors carry no expiry date.
        page: PageRead<HoldCursor>,
    },
    /// The open holds that expire on or before a date, by (expiry, position).
    Due {
        /// The date.
        on_or_before: Date,
        /// The page.
        page: PageRead<HoldCursor>,
    },
}

/// A hold page cursor: (expiry date, position); the date is `None` for
/// [`HoldRead::OpenByAccount`].
pub type HoldCursor = (Option<Date>, Position);

/// A hold as the hold index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldRow<const P: u8> {
    /// The id of the entry that opened it.
    pub id: EntryId,
    /// That entry's position.
    pub position: Position,
    /// Its booking date.
    pub booking: Date,
    /// Its expiry date, if any.
    pub expires: Option<Date>,
    /// The account debited.
    pub debit: AccountHandle,
    /// The account credited.
    pub credit: AccountHandle,
    /// The currency.
    pub currency: Currency,
    /// The amount held.
    pub amount: Amount<P>,
    /// The entry that resolved it and how; `None` while open.
    pub resolution: Option<(EntryId, ResolutionKind)>,
}

/// Which balances a [`BalanceRead`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BalanceScope {
    /// The maintained balances now, in key order.
    Current,
    /// The balances after every record below a position, in key order.
    At(Position),
    /// The leaves of a sealed period's balance trees: each account's balance
    /// through the period's end, in (currency, layer, handle) order.
    Sealed(PeriodId),
}

/// A request for balances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceRead {
    /// Which balances.
    pub scope: BalanceScope,
    /// These keys, in this order, a key without postings reading as the zero
    /// balance (`None`: every non-zero balance, paged).
    pub keys: Option<Vec<BalanceKey>>,
    /// The page (with explicit keys only `through` applies).
    pub page: PageRead<BalanceKey>,
}

/// The order of a postings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostingOrder {
    /// By (position, posting index): log order.
    Position,
    /// By (booking date, position, posting index): statement order.
    Booking,
}

/// A request for the postings of one balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingRead {
    /// The balance.
    pub key: BalanceKey,
    /// The order.
    pub order: PostingOrder,
    /// Only postings booked in this range.
    pub booking: Option<RangeInclusive<Date>>,
    /// Only postings with a value date in this range.
    pub value: Option<RangeInclusive<Date>>,
    /// Only postings carrying every one of these (axis, value) pairs (at most
    /// eight).
    pub dimensions: Vec<(Label, Label)>,
    /// The page.
    pub page: PageRead<PostingCursor>,
}

impl PostingRead {
    /// The most dimension filters in one request.
    pub const MAX_DIMENSIONS: usize = 8;
}

/// A postings page cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PostingCursor {
    /// The booking date (for [`PostingOrder::Booking`] only).
    pub booking: Option<Date>,
    /// The entry's position.
    pub position: Position,
    /// The posting's index in its entry.
    pub index: u16,
}

/// One posting as a statement reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingLine<const P: u8> {
    /// The entry's position.
    pub position: Position,
    /// The posting's index in its entry.
    pub index: u16,
    /// The entry's id.
    pub entry: EntryId,
    /// The side.
    pub side: Side,
    /// The amount (positive).
    pub amount: Amount<P>,
    /// The booking date.
    pub booking: Date,
    /// The value date.
    pub value: Date,
    /// The dimensions.
    pub dimensions: Dimensions,
    /// The balance's gross totals after this posting.
    pub balance_after: Balance<P>,
    /// The amount cleared against this posting, net of resets.
    pub cleared: Amount<P>,
}

impl<const P: u8> PostingLine<P> {
    /// The posting as a clearing names it.
    #[must_use]
    pub const fn posting_ref(&self) -> PostingRef {
        PostingRef::at(self.entry, self.index)
    }
}

/// An entry's memo as stored beside its leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Memo {
    /// The entry has no memo.
    None,
    /// The memo's text and the salt of its commitment.
    Kept {
        /// The text.
        text: String,
        /// The salt.
        salt: MemoSalt,
    },
    /// The memo was erased; its commitment stays in the leaf.
    Erased,
}

/// What [`erase_memo`](super::Store::erase_memo) did. None of the outcomes
/// is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Erasure {
    /// The memo's text and salt, and the entry's draft fingerprint, are gone.
    Erased,
    /// The memo had been erased before.
    AlreadyErased,
    /// The entry has no memo.
    NoMemo,
    /// No entry has the id.
    UnknownEntry,
}

/// A page from at most `limit + 1` rows in read order, each with its cursor:
/// a row beyond the limit only says that more follow.
pub(crate) fn paged<T, C>(
    mut rows: Vec<(C, T)>,
    limit: u32,
    size: u64,
    through: u64,
) -> Page<T, C> {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let more = rows.len() > limit;
    rows.truncate(limit);
    let mut next = None;
    let items = rows
        .into_iter()
        .map(|(cursor, item)| {
            next = Some(cursor);
            item
        })
        .collect();
    Page {
        items,
        next: if more { next } else { None },
        size,
        through,
    }
}
