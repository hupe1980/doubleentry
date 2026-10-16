//! The eight committed record kinds and their leaf bytes.

use core::fmt;

use time::Date;

use super::entry::{EntryBody, byte_enum};
use super::{
    AccountHandle, AccountPath, Amount, Balance, CaptureTime, Currency, Decode, DecodeError,
    Document, Encode, EntryId, HoldAction, IdempotencyKey, Label, Layer, LedgerId, LedgerPolicy,
    MAX_LEAF_BYTES, MemoCommitment, Rate, Reader, Side, Sink, ValueError, Writer, entry::Entry,
};
use crate::log::{Hash, leaf_hash};
use crate::model::Dimensions;

/// The format tag committed in every genesis record.
pub const FORMAT: &str = "doubleentry/1";

/// The kind byte of a seal balance-tree leaf (never a log record).
pub const BALANCE_LEAF_KIND: u8 = 0x81;

/// What an account is, fixed at registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AccountKind {
    /// Asset (normally debit).
    Asset,
    /// Liability (normally credit).
    Liability,
    /// Equity (normally credit).
    Equity,
    /// Income (normally credit).
    Income,
    /// Expense (normally debit).
    Expense,
}

impl fmt::Display for AccountKind {
    /// `asset`, `liability`, `equity`, `income` or `expense`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        super::encoding::pad(
            f,
            match self {
                Self::Asset => "asset",
                Self::Liability => "liability",
                Self::Equity => "equity",
                Self::Income => "income",
                Self::Expense => "expense",
            },
        )
    }
}

impl AccountKind {
    /// The side on which the account's balance normally stands.
    ///
    /// ```
    /// use doubleentry::{AccountKind, Side};
    ///
    /// assert_eq!(AccountKind::Asset.normal_side(), Side::Debit);
    /// assert_eq!(AccountKind::Income.normal_side(), Side::Credit);
    /// ```
    #[must_use]
    pub const fn normal_side(self) -> Side {
        match self {
            Self::Asset | Self::Expense => Side::Debit,
            Self::Liability | Self::Equity | Self::Income => Side::Credit,
        }
    }
}

/// Which balance an account may not reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum BalanceLimit {
    /// No limit.
    Unlimited,
    /// The account may not end with a net credit balance.
    NoCreditBalance,
    /// The account may not end with a net debit balance.
    NoDebitBalance,
}

byte_enum!(AccountKind, "account kind" { Asset = 0, Liability = 1, Equity = 2, Income = 3, Expense = 4 });
byte_enum!(BalanceLimit, "balance limit" { Unlimited = 0, NoCreditBalance = 1, NoDebitBalance = 2 });

/// The kind byte of a log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// `0x01`.
    Genesis = 1,
    /// `0x02`.
    Account = 2,
    /// `0x03`.
    Currency = 3,
    /// `0x04`.
    Policy = 4,
    /// `0x05`.
    Period = 5,
    /// `0x06`.
    Entry = 6,
    /// `0x07`.
    Clearing = 7,
    /// `0x08`.
    Seal = 8,
}

impl RecordKind {
    /// The kind of a kind byte; `None` for anything but `0x01`–`0x08`.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Genesis,
            2 => Self::Account,
            3 => Self::Currency,
            4 => Self::Policy,
            5 => Self::Period,
            6 => Self::Entry,
            7 => Self::Clearing,
            8 => Self::Seal,
            _ => return None,
        })
    }
}

/// Record `0x01`, position 0: the ledger's id (the C2SP origin). The scale `P`
/// and the format tag `doubleentry/1` are written from the record's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Genesis {
    /// The ledger's id.
    pub ledger: LedgerId,
}

/// Record `0x02`: master data for one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEvent {
    /// What happens to the account.
    pub action: AccountAction,
    /// The date from which the event applies.
    pub effective: Date,
    /// Who made the change.
    pub actor: Label,
}

/// What an [`AccountEvent`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountAction {
    /// Registers a new account.
    Register(AccountRegistration),
    /// Changes one field of an account. Path and kind never change.
    Change {
        /// The account.
        handle: AccountHandle,
        /// The new value.
        change: AccountChange,
    },
}

/// A new account. The handle is committed so one inclusion proof binds
/// handle → path; the rule core refuses a handle that is not the count of
/// earlier registrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRegistration {
    /// The account's handle.
    pub handle: AccountHandle,
    /// The account's path.
    pub path: AccountPath,
    /// The account's kind.
    pub kind: AccountKind,
    /// A caller-defined class.
    pub class: Option<Label>,
    /// The first day the account may be posted to.
    pub open_from: Date,
    /// The last day the account may be posted to.
    pub open_to: Option<Date>,
    /// The balance limit.
    pub limit: BalanceLimit,
}

/// A change to one field of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountChange {
    /// A new last posting day (`None`: open-ended).
    OpenTo(Option<Date>),
    /// A new balance limit.
    Limit(BalanceLimit),
    /// A new class.
    Class(Option<Label>),
}

/// Record `0x03`: a currency the ledger uses, with its exponent. The decoder
/// refuses an exponent above `P`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrencyDeclaration {
    /// The currency.
    pub code: Currency,
    /// Its number of decimal places.
    pub exponent: u8,
    /// The date from which the declaration applies.
    pub effective: Date,
    /// Who declared it.
    pub actor: Label,
}

/// Record `0x04`: a full policy snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvent {
    /// The policy.
    pub policy: LedgerPolicy,
    /// The date from which it applies.
    pub effective: Date,
    /// Who set it.
    pub actor: Label,
}

/// Record `0x05`: a period of the calendar.
///
/// ```
/// use doubleentry::{Label, record::PeriodDefinition};
/// use time::macros::date;
///
/// let march = PeriodDefinition::new(Label::new("2026-03")?, date!(2026 - 03 - 01), date!(2026 - 03 - 31))?;
/// assert_eq!(march.id().as_str(), "2026-03");
/// assert!(PeriodDefinition::new(Label::new("x")?, date!(2026 - 03 - 02), date!(2026 - 03 - 01)).is_err());
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodDefinition {
    id: Label,
    start: Date,
    end: Date,
}

impl PeriodDefinition {
    /// A period from `start` through `end`.
    ///
    /// # Errors
    ///
    /// [`ValueError::OutOfRange`] when `start > end`.
    pub fn new(id: Label, start: Date, end: Date) -> Result<Self, ValueError> {
        if start > end {
            return Err(ValueError::OutOfRange);
        }
        Ok(Self { id, start, end })
    }

    /// The period's id.
    #[must_use]
    pub fn id(&self) -> &Label {
        &self.id
    }

    /// The first day.
    #[must_use]
    pub const fn start(&self) -> Date {
        self.start
    }

    /// The last day.
    #[must_use]
    pub const fn end(&self) -> Date {
        self.end
    }
}

/// A committed posting: the account resolved to its handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingRecord<const P: u8 = 2> {
    /// The account.
    pub handle: AccountHandle,
    /// The side.
    pub side: Side,
    /// The amount.
    pub amount: Amount<P>,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The dimensions.
    pub dimensions: Dimensions,
}

impl<const P: u8> Encode for PostingRecord<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.handle);
        w.value(&self.side);
        w.value(&self.amount);
        w.value(&self.currency);
        w.value(&self.layer);
        w.value(&self.dimensions);
    }
}

impl<const P: u8> Decode for PostingRecord<P> {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        Ok(Self {
            handle: r.value("posting handle")?,
            side: r.value("side")?,
            amount: r.value("posting amount")?,
            currency: r.value("posting currency")?,
            layer: r.value("layer")?,
            dimensions: r.value("dimensions")?,
        })
    }
}

/// The fields of an [`EntryRecord`], before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryParts<const P: u8 = 2> {
    /// The idempotency key (the entry's identity).
    pub key: IdempotencyKey,
    /// The booking date.
    pub booking_date: Date,
    /// The value date.
    pub value_date: Date,
    /// When the caller captured the entry.
    pub captured_at: Option<CaptureTime>,
    /// The entry kind.
    pub kind: Option<Label>,
    /// The memo commitment (text and salt live beside the leaf).
    pub memo: Option<MemoCommitment>,
    /// Source documents (at most [`Entry::MAX_DOCUMENTS`]).
    pub documents: Vec<Document>,
    /// Postings (at most [`Entry::MAX_POSTINGS`]); a posting's index is its
    /// position here.
    pub postings: Vec<PostingRecord<P>>,
    /// The entry this one reverses.
    pub reverses: Option<EntryId>,
    /// The hold action.
    pub hold: Option<HoldAction<P>>,
    /// Rates (at most [`Entry::MAX_RATES`]), strictly ascending by (base, quote).
    pub rates: Vec<Rate>,
}

impl<const P: u8> EntryParts<P> {
    fn body(&self) -> EntryBody<'_, P, &[PostingRecord<P>], MemoCommitment> {
        EntryBody {
            key: self.key.as_str(),
            booking_date: self.booking_date,
            value_date: self.value_date,
            captured_at: self.captured_at.as_ref(),
            kind: self.kind.as_ref(),
            memo: self.memo.as_ref(),
            documents: &self.documents,
            postings: &self.postings,
            reverses: self.reverses.as_ref(),
            hold: self.hold.as_ref(),
            rates: &self.rates,
        }
    }
}

/// Record `0x06`: a committed entry. Built only through [`EntryRecord::new`],
/// which applies the decoder's checks, so every value has one canonical form.
///
/// No id is stored: [`id`](Self::id) derives it from the committed key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRecord<const P: u8 = 2>(EntryParts<P>);

/// Refuses a count above `max`.
fn at_most(found: usize, max: usize) -> Result<(), ValueError> {
    if found > max {
        Err(ValueError::TooLong { max, found })
    } else {
        Ok(())
    }
}

/// Refuses a sequence whose keys are not strictly ascending.
fn strictly_ascending<K: Ord>(keys: impl IntoIterator<Item = K>) -> Result<(), ValueError> {
    let mut previous: Option<K> = None;
    for key in keys {
        if let Some(previous) = &previous {
            match previous.cmp(&key) {
                core::cmp::Ordering::Less => {}
                core::cmp::Ordering::Equal => return Err(ValueError::Duplicate),
                core::cmp::Ordering::Greater => return Err(ValueError::NotSorted),
            }
        }
        previous = Some(key);
    }
    Ok(())
}

/// Refuses a record body whose leaf (kind byte included) exceeds the cap.
fn fits_leaf(body: &impl Encode) -> Result<(), ValueError> {
    let found = body.encoded_len().saturating_add(1);
    if found > MAX_LEAF_BYTES {
        Err(ValueError::TooLarge {
            max: MAX_LEAF_BYTES,
            found,
        })
    } else {
        Ok(())
    }
}

impl<const P: u8> EntryRecord<P> {
    /// Validates an entry record.
    ///
    /// # Errors
    ///
    /// [`ValueError::TooLong`] for too many documents, postings or rates,
    /// [`ValueError::NotSorted`] / [`ValueError::Duplicate`] for rates not
    /// strictly ascending by (base, quote), [`ValueError::TooLarge`] when the
    /// leaf would exceed [`MAX_LEAF_BYTES`].
    pub fn new(parts: EntryParts<P>) -> Result<Self, ValueError> {
        at_most(parts.documents.len(), Entry::<P>::MAX_DOCUMENTS)?;
        at_most(parts.postings.len(), Entry::<P>::MAX_POSTINGS)?;
        at_most(parts.rates.len(), Entry::<P>::MAX_RATES)?;
        strictly_ascending(parts.rates.iter().map(|rate| (rate.base(), rate.quote())))?;
        fits_leaf(&parts.body())?;
        Ok(Self(parts))
    }

    /// The fields.
    #[must_use]
    pub fn parts(&self) -> &EntryParts<P> {
        &self.0
    }

    /// The fields, by value.
    #[must_use]
    pub fn into_parts(self) -> EntryParts<P> {
        self.0
    }

    /// The entry's id in `ledger`, derived from the committed key.
    #[must_use]
    pub fn id(&self, ledger: &LedgerId) -> EntryId {
        EntryId::derive(ledger, &self.0.key)
    }
}

/// One posting of one entry: the entry's id and the posting's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PostingRef {
    /// The entry.
    pub entry: EntryId,
    /// The posting's index in the entry.
    pub index: u16,
}

impl PostingRef {
    /// A reference to posting `index` of `entry`.
    ///
    /// ```
    /// use doubleentry::{EntryId, PostingRef};
    ///
    /// let r = PostingRef::at(EntryId::from_bytes([7; 16]), 3);
    /// assert_eq!(r.index, 3);
    /// ```
    #[must_use]
    pub const fn at(entry: EntryId, index: u16) -> Self {
        Self { entry, index }
    }
}

impl Encode for PostingRef {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.entry);
        w.u16(self.index);
    }
}

impl Decode for PostingRef {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        Ok(Self::at(r.value(field)?, r.u16()?))
    }
}

/// Record `0x07`: open-item matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearingEvent<const P: u8 = 2> {
    /// What happens.
    pub action: ClearingAction<P>,
    /// The date of the clearing.
    pub date: Date,
}

/// What a [`ClearingEvent`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClearingAction<const P: u8 = 2> {
    /// Matches part of a debit posting against part of a credit posting.
    Apply {
        /// The clearing's idempotency key.
        key: IdempotencyKey,
        /// The debit posting.
        debit: PostingRef,
        /// The credit posting.
        credit: PostingRef,
        /// The amount matched.
        amount: Amount<P>,
    },
    /// Undoes an earlier apply, named by its log position.
    Reset {
        /// The reset's idempotency key.
        key: IdempotencyKey,
        /// The log position of the apply record being reset.
        clearing: u64,
    },
}

/// One (currency, layer) of a seal: its balance-tree root and totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeHeadEntry<const P: u8 = 2> {
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The balance tree's root.
    pub root: Hash,
    /// The component-wise sum of every leaf.
    pub totals: Balance<P>,
}

impl<const P: u8> Encode for TreeHeadEntry<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.currency);
        w.value(&self.layer);
        w.value(&self.root);
        w.value(&self.totals);
    }
}

impl<const P: u8> Decode for TreeHeadEntry<P> {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        Ok(Self {
            currency: r.value("tree currency")?,
            layer: r.value("tree layer")?,
            root: r.value("tree root")?,
            totals: r.value("tree totals")?,
        })
    }
}

/// The fields of a [`Seal`], before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealParts<const P: u8 = 2> {
    /// The sealed period.
    pub period: Label,
    /// Its first day.
    pub start: Date,
    /// Its last day.
    pub end: Date,
    /// The log position of the last record the seal covers.
    pub through: u64,
    /// The number of registered accounts at `through`: the size of every
    /// balance tree.
    pub accounts: u32,
    /// One entry per (currency, layer) with postings, strictly ascending.
    pub trees: Vec<TreeHeadEntry<P>>,
}

impl<const P: u8> Encode for SealParts<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.period);
        w.date(self.start);
        w.date(self.end);
        w.u64(self.through);
        w.u32(self.accounts);
        w.seq(&self.trees);
    }
}

/// Record `0x08`: a sealed period, committing per (currency, layer) to a
/// handle-indexed tree of gross balances. Built only through [`Seal::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seal<const P: u8 = 2>(SealParts<P>);

/// Why a seal does not follow an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SealOrderError {
    /// The seal does not start the day after the earlier one ends.
    #[error("the seal does not start the day after the earlier seal ends")]
    NotContiguous,
    /// The seal covers no record beyond the earlier one.
    #[error("the seal covers no record beyond the earlier seal")]
    ThroughNotIncreasing,
    /// Fewer accounts than at the earlier seal.
    #[error("fewer accounts than at the earlier seal")]
    AccountsDecreased,
    /// A (currency, layer) of the earlier seal is missing.
    #[error("the {currency} {layer:?} tree of the earlier seal is missing")]
    TreeMissing {
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
    },
    /// A total component is smaller than at the earlier seal.
    #[error("a {currency} {layer:?} total is smaller than at the earlier seal")]
    TotalDecreased {
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
    },
}

/// One line, `seal of 2026-03 (2026-03-01 to 2026-03-31) through position
/// 40: 6 accounts, 2 balance trees`, honouring width, fill and alignment.
impl<const P: u8> fmt::Display for Seal<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = &self.0;
        let trees = match parts.trees.len() {
            1 => String::from("1 balance tree"),
            count => format!("{count} balance trees"),
        };
        let text = format!(
            "seal of {} ({} to {}) through position {}: {} accounts, {trees}",
            parts.period, parts.start, parts.end, parts.through, parts.accounts
        );
        super::encoding::pad(f, &text)
    }
}

impl<const P: u8> Seal<P> {
    /// Validates a seal.
    ///
    /// # Errors
    ///
    /// [`ValueError::OutOfRange`] when `start > end`, [`ValueError::NotSorted`]
    /// / [`ValueError::Duplicate`] for trees not strictly ascending by
    /// (currency, layer), [`ValueError::TooLarge`] when the leaf would exceed
    /// [`MAX_LEAF_BYTES`].
    pub fn new(parts: SealParts<P>) -> Result<Self, ValueError> {
        if parts.start > parts.end {
            return Err(ValueError::OutOfRange);
        }
        strictly_ascending(parts.trees.iter().map(|tree| (tree.currency, tree.layer)))?;
        fits_leaf(&parts)?;
        Ok(Self(parts))
    }

    /// The fields.
    #[must_use]
    pub fn parts(&self) -> &SealParts<P> {
        &self.0
    }

    /// The tree of one (currency, layer); `None` when nothing was posted there,
    /// which proves every account's balance there is zero.
    #[must_use]
    pub fn tree(&self, currency: Currency, layer: Layer) -> Option<&TreeHeadEntry<P>> {
        self.0
            .trees
            .binary_search_by_key(&(currency, layer), |tree| (tree.currency, tree.layer))
            .ok()
            .and_then(|index| self.0.trees.get(index))
    }

    /// Checks that this seal follows `earlier`: it starts the day after
    /// `earlier` ends, covers more records, has at least as many accounts, and
    /// keeps every (currency, layer) of `earlier` with no total component
    /// smaller.
    ///
    /// # Errors
    ///
    /// The first [`SealOrderError`] found.
    pub fn check_follows(&self, earlier: &Self) -> Result<(), SealOrderError> {
        let (this, earlier) = (&self.0, &earlier.0);
        if earlier.end.next_day() != Some(this.start) {
            return Err(SealOrderError::NotContiguous);
        }
        if this.through <= earlier.through {
            return Err(SealOrderError::ThroughNotIncreasing);
        }
        if this.accounts < earlier.accounts {
            return Err(SealOrderError::AccountsDecreased);
        }
        for before in &earlier.trees {
            let (currency, layer) = (before.currency, before.layer);
            let after = self
                .tree(currency, layer)
                .ok_or(SealOrderError::TreeMissing { currency, layer })?;
            let (a, b) = (&after.totals, &before.totals);
            if a.debit() < b.debit() || a.credit() < b.credit() || a.postings() < b.postings() {
                return Err(SealOrderError::TotalDecreased { currency, layer });
            }
        }
        Ok(())
    }
}

/// One typed leaf of a ledger's log.
///
/// Every record has exactly one byte form: `u8 kind ‖ body`, at most
/// [`MAX_LEAF_BYTES`] bytes. [`decode`](Self::decode) is the strict inverse of
/// [`to_leaf_bytes`](Self::to_leaf_bytes).
///
/// ```
/// use doubleentry::{LedgerId, record::{Genesis, Record}};
///
/// let genesis: Record = Record::Genesis(Genesis { ledger: LedgerId::new("example.com/acme")? });
/// assert_eq!(
///     genesis.leaf_hash().to_string(),
///     "cd615a0dac3171f8c156c2f65bc613bf9d1a5381874de8ca201b508d203e83d5",
/// );
/// assert_eq!(Record::decode(&genesis.to_leaf_bytes()), Ok(genesis));
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Record<const P: u8 = 2> {
    /// `0x01`.
    Genesis(Genesis),
    /// `0x02`.
    Account(AccountEvent),
    /// `0x03`.
    Currency(CurrencyDeclaration),
    /// `0x04`.
    Policy(PolicyEvent),
    /// `0x05`.
    Period(PeriodDefinition),
    /// `0x06`.
    Entry(EntryRecord<P>),
    /// `0x07`.
    Clearing(ClearingEvent<P>),
    /// `0x08`.
    Seal(Seal<P>),
}

impl<const P: u8> Record<P> {
    /// The record's kind.
    #[must_use]
    pub const fn kind(&self) -> RecordKind {
        match self {
            Self::Genesis(_) => RecordKind::Genesis,
            Self::Account(_) => RecordKind::Account,
            Self::Currency(_) => RecordKind::Currency,
            Self::Policy(_) => RecordKind::Policy,
            Self::Period(_) => RecordKind::Period,
            Self::Entry(_) => RecordKind::Entry,
            Self::Clearing(_) => RecordKind::Clearing,
            Self::Seal(_) => RecordKind::Seal,
        }
    }

    /// The leaf bytes: `u8 kind ‖ body`.
    #[must_use]
    pub fn to_leaf_bytes(&self) -> Vec<u8> {
        self.to_bytes()
    }

    /// The RFC 6962 leaf hash of the leaf bytes.
    #[must_use]
    pub fn leaf_hash(&self) -> Hash {
        leaf_hash(&self.to_leaf_bytes())
    }

    /// Decodes leaf bytes, refusing anything that is not one record in its
    /// one canonical form.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] naming what was wrong; the decoder never panics.
    pub fn decode(leaf_bytes: &[u8]) -> Result<Self, DecodeError> {
        if leaf_bytes.len() > MAX_LEAF_BYTES {
            return Err(DecodeError::TooLarge {
                max: MAX_LEAF_BYTES,
                found: leaf_bytes.len(),
            });
        }
        let mut r = Reader::new(leaf_bytes);
        let kind = r.u8()?;
        let record = match RecordKind::from_u8(kind).ok_or(DecodeError::UnknownKind(kind))? {
            RecordKind::Genesis => Self::Genesis(decode_genesis::<P>(&mut r)?),
            RecordKind::Account => Self::Account(decode_account(&mut r)?),
            RecordKind::Currency => Self::Currency(decode_currency::<P>(&mut r)?),
            RecordKind::Policy => Self::Policy(PolicyEvent {
                policy: r.value("policy")?,
                effective: r.date()?,
                actor: r.value("actor")?,
            }),
            RecordKind::Period => {
                let id = r.value("period id")?;
                let (start, end) = (r.date()?, r.date()?);
                Self::Period(PeriodDefinition::new(id, start, end).map_err(|error| {
                    DecodeError::Value {
                        field: "period",
                        error,
                    }
                })?)
            }
            RecordKind::Entry => Self::Entry(decode_entry(&mut r)?),
            RecordKind::Clearing => Self::Clearing(decode_clearing(&mut r)?),
            RecordKind::Seal => Self::Seal(decode_seal(&mut r)?),
        };
        r.finish()?;
        Ok(record)
    }
}

impl<const P: u8> Encode for Record<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.u8(self.kind() as u8);
        match self {
            Self::Genesis(genesis) => {
                w.value(&genesis.ledger);
                w.u8(P);
                w.str(FORMAT);
            }
            Self::Account(event) => {
                match &event.action {
                    AccountAction::Register(account) => {
                        w.u8(0);
                        w.value(&account.handle);
                        w.value(&account.path);
                        w.value(&account.kind);
                        w.opt(account.class.as_ref());
                        w.date(account.open_from);
                        w.opt(account.open_to.as_ref());
                        w.value(&account.limit);
                    }
                    AccountAction::Change { handle, change } => {
                        w.u8(1);
                        w.value(handle);
                        match change {
                            AccountChange::OpenTo(open_to) => {
                                w.u8(0);
                                w.opt(open_to.as_ref());
                            }
                            AccountChange::Limit(limit) => {
                                w.u8(1);
                                w.value(limit);
                            }
                            AccountChange::Class(class) => {
                                w.u8(2);
                                w.opt(class.as_ref());
                            }
                        }
                    }
                }
                w.date(event.effective);
                w.value(&event.actor);
            }
            Self::Currency(declaration) => {
                w.value(&declaration.code);
                w.u8(declaration.exponent);
                w.date(declaration.effective);
                w.value(&declaration.actor);
            }
            Self::Policy(event) => {
                w.value(&event.policy);
                w.date(event.effective);
                w.value(&event.actor);
            }
            Self::Period(period) => {
                w.value(&period.id);
                w.date(period.start);
                w.date(period.end);
            }
            Self::Entry(entry) => w.value(&entry.0.body()),
            Self::Clearing(clearing) => {
                match &clearing.action {
                    ClearingAction::Apply {
                        key,
                        debit,
                        credit,
                        amount,
                    } => {
                        w.u8(0);
                        w.value(key);
                        w.value(debit);
                        w.value(credit);
                        w.value(amount);
                    }
                    ClearingAction::Reset { key, clearing } => {
                        w.u8(1);
                        w.value(key);
                        w.u64(*clearing);
                    }
                }
                w.date(clearing.date);
            }
            Self::Seal(seal) => w.value(&seal.0),
        }
    }
}

fn decode_genesis<const P: u8>(r: &mut Reader<'_>) -> Result<Genesis, DecodeError> {
    let ledger = r.value("ledger")?;
    let precision = r.u8()?;
    if precision != P {
        return Err(DecodeError::PrecisionMismatch {
            expected: P,
            found: precision,
        });
    }
    if r.str("format")? != FORMAT {
        return Err(DecodeError::UnknownFormat);
    }
    Ok(Genesis { ledger })
}

fn decode_account(r: &mut Reader<'_>) -> Result<AccountEvent, DecodeError> {
    let action = match r.u8()? {
        0 => AccountAction::Register(AccountRegistration {
            handle: r.value("handle")?,
            path: r.value("path")?,
            kind: r.value("account kind")?,
            class: r.opt("class", |r| r.value("class"))?,
            open_from: r.date()?,
            open_to: r.opt("open_to", Reader::date)?,
            limit: r.value("balance limit")?,
        }),
        1 => {
            let handle = r.value("handle")?;
            let change = match r.u8()? {
                0 => AccountChange::OpenTo(r.opt("open_to", Reader::date)?),
                1 => AccountChange::Limit(r.value("balance limit")?),
                2 => AccountChange::Class(r.opt("class", |r| r.value("class"))?),
                tag => {
                    return Err(DecodeError::InvalidTag {
                        field: "account field",
                        tag,
                    });
                }
            };
            AccountAction::Change { handle, change }
        }
        tag => {
            return Err(DecodeError::InvalidTag {
                field: "account action",
                tag,
            });
        }
    };
    Ok(AccountEvent {
        action,
        effective: r.date()?,
        actor: r.value("actor")?,
    })
}

fn decode_currency<const P: u8>(r: &mut Reader<'_>) -> Result<CurrencyDeclaration, DecodeError> {
    let code = r.value("currency")?;
    let exponent = r.u8()?;
    if exponent > P {
        return Err(DecodeError::Value {
            field: "exponent",
            error: ValueError::ExponentAboveScale { exponent, scale: P },
        });
    }
    Ok(CurrencyDeclaration {
        code,
        exponent,
        effective: r.date()?,
        actor: r.value("actor")?,
    })
}

/// Reads `seq(x)` of at most `max` elements, each with `read`.
fn decode_seq<T>(
    r: &mut Reader<'_>,
    field: &'static str,
    max: usize,
    mut read: impl FnMut(&mut Reader<'_>) -> Result<T, DecodeError>,
) -> Result<Vec<T>, DecodeError> {
    let count = r.count(field, max)?;
    let mut items = Vec::new();
    for _ in 0..count {
        items.push(read(r)?);
    }
    Ok(items)
}

/// Maps a constructor's refusal of already-checked fields.
fn canonical(field: &'static str) -> impl Fn(ValueError) -> DecodeError {
    move |error| match error {
        ValueError::NotSorted | ValueError::Duplicate => DecodeError::NonCanonical { field },
        error => DecodeError::Value { field, error },
    }
}

fn decode_entry<const P: u8>(r: &mut Reader<'_>) -> Result<EntryRecord<P>, DecodeError> {
    let parts = EntryParts {
        key: r.value("key")?,
        booking_date: r.date()?,
        value_date: r.date()?,
        captured_at: r.opt("captured_at", |r| r.value("captured_at"))?,
        kind: r.opt("kind", |r| r.value("kind"))?,
        memo: r.opt("memo", |r| r.value("memo"))?,
        documents: decode_seq(r, "documents", Entry::<P>::MAX_DOCUMENTS, |r| {
            r.value("document")
        })?,
        postings: decode_seq(r, "postings", Entry::<P>::MAX_POSTINGS, |r| {
            r.value("posting")
        })?,
        reverses: r.opt("reverses", |r| r.value("reverses"))?,
        hold: r.opt("hold", |r| r.value("hold"))?,
        rates: decode_seq(r, "rates", Entry::<P>::MAX_RATES, |r| r.value("rate"))?,
    };
    EntryRecord::new(parts).map_err(canonical("rates"))
}

fn decode_clearing<const P: u8>(r: &mut Reader<'_>) -> Result<ClearingEvent<P>, DecodeError> {
    let action = match r.u8()? {
        0 => ClearingAction::Apply {
            key: r.value("key")?,
            debit: r.value("debit")?,
            credit: r.value("credit")?,
            amount: r.value("amount")?,
        },
        1 => ClearingAction::Reset {
            key: r.value("key")?,
            clearing: r.u64()?,
        },
        tag => {
            return Err(DecodeError::InvalidTag {
                field: "clearing action",
                tag,
            });
        }
    };
    Ok(ClearingEvent {
        action,
        date: r.date()?,
    })
}

fn decode_seal<const P: u8>(r: &mut Reader<'_>) -> Result<Seal<P>, DecodeError> {
    let period = r.value("period id")?;
    let start = r.date()?;
    let end = r.date()?;
    let through = r.u64()?;
    let accounts = r.u32()?;
    // Each tree head takes at least 82 bytes, so the input bounds the count.
    let max_trees = r.remaining().div_euclid(82);
    let trees = decode_seq(r, "trees", max_trees, |r| r.value("tree"))?;
    Seal::new(SealParts {
        period,
        start,
        end,
        through,
        accounts,
        trees,
    })
    .map_err(canonical("trees"))
}
