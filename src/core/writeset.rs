//! What the core decides: an [`Outcome`], and for an append the [`WriteSet`]
//! a store persists atomically.

use time::Date;

use super::holds::HoldEffect;
use super::state::{BalanceKey, Window};
use crate::error::Rejection;
use crate::model::{
    AccountHandle, AccountRegistration, Amount, Balance, BalanceLimit, Currency, Dimensions, Entry,
    EntryId, Fingerprint, IdempotencyKey, Label, Layer, LedgerPolicy, MemoSalt, PeriodId,
    PostingRef, Record, Seal, Side,
};

/// The core's decision on an operation it accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome<const P: u8> {
    /// Append exactly one record, with everything a store maintains for it.
    Append(Box<WriteSet<P>>),
    /// The operation is already in effect (a replayed entry or clearing, an
    /// identical re-declaration, an already sealed period): write nothing and
    /// return this.
    Unchanged(Done<P>),
    /// Append these records, in order: the new entries of an atomic batch
    /// ([`Op::Batch`](super::Op::Batch)), each decided against the state
    /// the ones before it leave. Persist all of them in one transaction or
    /// none.
    Batch {
        /// One write set per new entry, by position.
        writes: Vec<WriteSet<P>>,
        /// [`Done::Batch`]: every draft's receipt, replays included.
        done: Done<P>,
    },
}

impl<const P: u8> Outcome<P> {
    /// What the façade returns, whether or not anything is written.
    #[must_use]
    pub const fn done(&self) -> &Done<P> {
        match self {
            Self::Append(write) => &write.done,
            Self::Unchanged(done) | Self::Batch { done, .. } => done,
        }
    }
}

impl<const P: u8> From<WriteSet<P>> for Outcome<P> {
    fn from(write: WriteSet<P>) -> Self {
        Self::Append(Box::new(write))
    }
}

/// What an accepted operation returns to its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Done<const P: u8> {
    /// The genesis record exists.
    Genesis,
    /// An entry was recorded (or replayed).
    Entry(Recorded),
    /// An account is registered under this handle.
    Account(AccountHandle),
    /// An account change, currency declaration or policy is in effect.
    Changed,
    /// A period is defined.
    Period(PeriodId),
    /// A period is sealed.
    Sealed(SealReceipt<P>),
    /// A clearing or its reset is recorded at this position.
    Cleared(u64),
    /// The year-end closing drafts, to be recorded in order (nothing was
    /// written).
    Closing(Vec<Entry<P>>),
    /// Every draft of a batch recorded (or replayed), in order.
    Batch(Vec<Recorded>),
    /// A batch refused at draft `index`; nothing of it was written.
    BatchRejected {
        /// The refused draft's index in the batch.
        index: usize,
        /// Why it was refused.
        rejection: Rejection,
    },
}

/// The receipt of a recorded entry.
///
/// `replayed` is `true` exactly when the same draft had been recorded before:
/// nothing was appended and `id` and `position` are the original's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Recorded {
    /// The entry's id, derived from the ledger and the idempotency key.
    pub id: EntryId,
    /// The entry's position in the log.
    pub position: u64,
    /// Whether this was a retry of an entry already recorded.
    pub replayed: bool,
}

/// One line, `entry <id> at position 21` (and `, replayed` for a retry),
/// honouring width, fill and alignment.
impl core::fmt::Display for Recorded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let replayed = if self.replayed { ", replayed" } else { "" };
        let text = format!("entry {} at position {}{replayed}", self.id, self.position);
        crate::model::pad(f, &text)
    }
}

/// One line, `2026-03 sealed at position 40`, honouring width, fill and
/// alignment.
impl<const P: u8> core::fmt::Display for SealReceipt<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = format!("{} sealed at position {}", self.period, self.position);
        crate::model::pad(f, &text)
    }
}

impl Recorded {
    pub(crate) const fn new(id: EntryId, position: u64, replayed: bool) -> Self {
        Self {
            id,
            position,
            replayed,
        }
    }
}

/// The receipt of a sealed period.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealReceipt<const P: u8 = 2> {
    /// The period.
    pub period: PeriodId,
    /// The seal record's position.
    pub position: u64,
    /// The seal record.
    pub seal: Seal<P>,
}

/// Everything a store writes for one record, applied completely or not at
/// all.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriteSet<const P: u8> {
    /// Where the record goes: the view's size when the core decided. A store
    /// refuses a write set whose position is not its size.
    pub position: u64,
    /// The record.
    pub record: Record<P>,
    /// The record's leaf bytes.
    pub leaf: Vec<u8>,
    /// Memo text and salt, stored beside the leaf (erasable).
    pub memo: Option<MemoPlain>,
    /// What the operation returns.
    pub done: Done<P>,
    /// One row per posting, in record order.
    pub postings: Vec<PostingRow<P>>,
    /// One delta per touched balance, ascending by key.
    pub balances: Vec<BalanceDelta<P>>,
    /// The index rows to maintain, in a fixed order per operation.
    pub index: Vec<IndexRow<P>>,
    /// The hold index change: a hold opened or resolved.
    pub hold: Option<HoldEffect<P>>,
    /// The amounts open holds reserve after this record, for each (account,
    /// currency) the hold change touches, ascending by account.
    pub outstanding: Vec<Outstanding<P>>,
}

impl<const P: u8> WriteSet<P> {
    /// A write set for a record that touches no balance.
    pub(crate) fn of(position: u64, record: Record<P>, done: Done<P>) -> Self {
        let leaf = record.to_leaf_bytes();
        Self {
            position,
            record,
            leaf,
            memo: None,
            done,
            postings: Vec::new(),
            balances: Vec::new(),
            index: Vec::new(),
            hold: None,
            outstanding: Vec::new(),
        }
    }

    /// With an index row.
    pub(crate) fn with(mut self, row: IndexRow<P>) -> Self {
        self.index.push(row);
        self
    }
}

/// What open holds reserve on one (account, currency): a maintained value
/// a store keeps beside the hold index and an audit recomputes. The rule
/// core computes it for every hold and resolution
/// ([`WriteSet::outstanding`]); a store writes it as given.
///
/// ```
/// use doubleentry::store::Outstanding;
/// use doubleentry::{AccountHandle, Amount, amount, currency::EUR};
///
/// let row: Outstanding<2> = Outstanding {
///     handle: AccountHandle::new(0).ok_or("no handle")?,
///     currency: EUR,
///     debits: amount!("30.00"),
///     credits: Amount::ZERO,
/// };
/// assert!(!row.is_zero());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outstanding<const P: u8> {
    /// The account.
    pub handle: AccountHandle,
    /// The currency.
    pub currency: Currency,
    /// The sum of the open holds debiting the account.
    pub debits: Amount<P>,
    /// The sum of the open holds crediting the account.
    pub credits: Amount<P>,
}

impl<const P: u8> Outstanding<P> {
    /// Whether no open hold reserves anything here.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.debits.is_zero() && self.credits.is_zero()
    }
}

/// A memo's text and the salt of its commitment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoPlain {
    /// The salt.
    pub salt: MemoSalt,
    /// The text.
    pub text: String,
}

/// One posting as a store keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostingRow<const P: u8> {
    /// The posting's index in its entry.
    pub index: u16,
    /// The balance it moves.
    pub key: BalanceKey,
    /// Its side.
    pub side: Side,
    /// Its amount (positive).
    pub amount: Amount<P>,
    /// The entry's booking date.
    pub booking: Date,
    /// The entry's value date.
    pub value: Date,
    /// The posting's dimensions.
    pub dimensions: Dimensions,
    /// The key's gross balance after this posting (the entry's postings
    /// applied in order).
    pub balance_after: Balance<P>,
}

/// What one record adds to one maintained balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BalanceDelta<const P: u8> {
    /// The balance.
    pub key: BalanceKey,
    /// The gross debit, credit and posting count to add.
    pub delta: Balance<P>,
}

/// One version of an account's master data.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountTerm {
    /// The open window.
    Window(Window),
    /// The balance limit.
    Limit(BalanceLimit),
    /// The class.
    Class(Option<Label>),
}

/// A row of an index a store maintains beside the log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexRow<const P: u8> {
    /// An entry's key: idempotency and id → position.
    EntryKey {
        /// The key.
        key: IdempotencyKey,
        /// The derived id.
        id: EntryId,
        /// The entry's position.
        position: u64,
        /// The draft's fingerprint (side data: `None` when unknown).
        fingerprint: Option<Fingerprint>,
    },
    /// A reversal link (at most one per target).
    Reversal {
        /// The reversed entry.
        target: EntryId,
        /// The reversing entry.
        by: EntryId,
    },
    /// A registered account.
    Account {
        /// The registration as recorded.
        registration: AccountRegistration,
        /// The register record's position.
        position: u64,
    },
    /// One version of one account field.
    AccountVersion {
        /// The account.
        handle: AccountHandle,
        /// The new value.
        term: AccountTerm,
        /// From when it applies.
        effective: Date,
        /// The record's position.
        position: u64,
    },
    /// One version of a currency declaration.
    CurrencyVersion {
        /// The currency.
        code: Currency,
        /// Its exponent.
        exponent: u8,
        /// From when it applies.
        effective: Date,
        /// The record's position.
        position: u64,
    },
    /// One version of the ledger policy.
    PolicyVersion {
        /// The policy.
        policy: LedgerPolicy,
        /// From when it applies.
        effective: Date,
        /// The record's position.
        position: u64,
    },
    /// A defined period.
    Period {
        /// Its id.
        id: PeriodId,
        /// Its first day.
        start: Date,
        /// Its last day.
        end: Date,
        /// The record's position.
        position: u64,
    },
    /// A sealed period; the watermark becomes its end.
    Sealed {
        /// The period.
        period: PeriodId,
        /// The seal record's position.
        position: u64,
    },
    /// The non-zero leaves of one seal balance tree, ascending by handle,
    /// kept so a sealed balance can be proven without refolding the log.
    SealTree {
        /// The period.
        period: PeriodId,
        /// The tree's currency.
        currency: Currency,
        /// The tree's layer.
        layer: Layer,
        /// Every handle whose leaf is not the zero balance.
        leaves: Vec<(AccountHandle, Balance<P>)>,
    },
    /// A clearing record's key (apply or reset).
    ClearingKey {
        /// The key.
        key: IdempotencyKey,
        /// The record's position.
        position: u64,
    },
    /// A change of the amount cleared against a posting.
    Applied {
        /// The posting.
        posting: PostingRef,
        /// Added by an apply, subtracted (negative) by a reset.
        delta: Amount<P>,
    },
    /// A clearing was reset.
    ClearingReset {
        /// The apply record's position.
        clearing: u64,
        /// The reset record's position.
        position: u64,
    },
}
