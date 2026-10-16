//! The row kinds of the export view (field lists: the module documentation).

use time::Date;

use crate::log::Hash;
use crate::model::{
    AccountAction, AccountHandle, AccountKind, AccountPath, CaptureTime, Currency, Dimensions,
    EntryId, IdempotencyKey, Label, Layer, MemoCommitment, MemoSalt, PeriodId, RecordKind, Side,
};

/// One row of the export view.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExportRow {
    /// Every record's leaf hash.
    Leaf(LeafRow),
    /// An entry.
    Entry(EntryRow),
    /// One posting of the entry before it.
    Posting(PostingRow),
    /// One Konto/Gegenkonto pair of the entry before it.
    Pair(PairRow),
    /// One document of the entry before it.
    Document(DocumentRow),
    /// One exchange rate of the entry before it.
    Rate(RateRow),
    /// An account master-data event.
    Account(AccountRow),
    /// A currency declaration.
    Currency(CurrencyRow),
    /// A seal.
    Seal(SealRow),
    /// One (currency, layer) of the seal before it.
    SealTotal(SealTotalRow),
}

/// A record of the log: what the tree root is rebuilt from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeafRow {
    /// The record's position.
    pub position: u64,
    /// The record's kind (its first leaf byte: `kind as u8`).
    pub kind: RecordKind,
    /// The RFC 6962 leaf hash of its leaf bytes.
    pub leaf_hash: Hash,
}

/// What an entry does to a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HoldKind {
    /// It opens one.
    Open,
    /// It settles one, fully or partly.
    Post,
    /// It releases one.
    Void,
    /// It releases one that expired.
    Expire,
}

/// An entry: every field of its record but the postings, documents and
/// rates (their own rows), with its id, period and memo.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntryRow {
    /// The record's position.
    pub position: u64,
    /// The entry's id.
    pub entry: EntryId,
    /// Its idempotency key.
    pub key: IdempotencyKey,
    /// The booking date.
    pub booking_date: Date,
    /// The value date.
    pub value_date: Date,
    /// When it was captured, if recorded.
    pub captured_at: Option<CaptureTime>,
    /// The caller's kind (`invoice`, `payment`, …).
    pub kind: Option<Label>,
    /// The period of its booking date.
    pub period: PeriodId,
    /// The memo's commitment, when the entry has a memo.
    pub memo_commitment: Option<MemoCommitment>,
    /// The memo's text, unless erased.
    pub memo_text: Option<String>,
    /// The commitment's salt, unless erased.
    pub memo_salt: Option<MemoSalt>,
    /// The entry it reverses.
    pub reverses: Option<EntryId>,
    /// What it does to a hold.
    pub hold_action: Option<HoldKind>,
    /// The hold it resolves.
    pub hold_target: Option<EntryId>,
    /// The day an opened hold may be expired.
    pub hold_expires: Option<Date>,
    /// The amount a partial post settles, in minor units of the exponent of
    /// the entry's first posting row (the ledger scale without postings);
    /// `None` for a full post.
    pub post_amount_minor: Option<i128>,
    /// Its number of posting rows.
    pub posting_count: u16,
    /// Its number of document rows.
    pub document_count: u16,
    /// Its number of rate rows.
    pub rate_count: u16,
    /// Its leaf hash (as its leaf row's).
    pub leaf_hash: Hash,
}

/// One posting, with the master data in force on the booking date.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostingRow {
    /// The entry.
    pub entry: EntryId,
    /// The entry's position.
    pub position: u64,
    /// The posting's index in the entry.
    pub index: u16,
    /// The account.
    pub account: AccountHandle,
    /// Its path.
    pub path: AccountPath,
    /// Its kind (Kontoart).
    pub kind: AccountKind,
    /// Its class (Kontotyp) in force on the booking date.
    pub class: Option<Label>,
    /// The side.
    pub side: Side,
    /// The amount in minor units of `exponent` (positive).
    pub minor_units: i128,
    /// The currency's declared exponent on the booking date.
    pub exponent: u8,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The dimensions, ascending by axis.
    pub dimensions: Dimensions,
    /// The booking date.
    pub booking_date: Date,
    /// The value date.
    pub value_date: Date,
}

/// One Konto/Gegenkonto pair: an amount moved from a credit posting to a
/// debit posting of the same entry, currency and layer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PairRow {
    /// The entry.
    pub entry: EntryId,
    /// The entry's position.
    pub position: u64,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The pair's number in the entry, from 0.
    pub ordinal: u16,
    /// The debit posting's index.
    pub debit_index: u16,
    /// The debit account (Konto).
    pub debit_account: AccountHandle,
    /// Its path.
    pub debit_path: AccountPath,
    /// The credit posting's index.
    pub credit_index: u16,
    /// The credit account (Gegenkonto).
    pub credit_account: AccountHandle,
    /// Its path.
    pub credit_path: AccountPath,
    /// The amount in minor units of `exponent` (positive).
    pub minor_units: i128,
    /// The currency's declared exponent on the booking date.
    pub exponent: u8,
}

/// One document of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DocumentRow {
    /// The entry.
    pub entry: EntryId,
    /// The entry's position.
    pub position: u64,
    /// The document's number in the entry, from 0.
    pub ordinal: u16,
    /// Its type.
    pub doc_type: Label,
    /// Its reference (at most 256 bytes).
    pub reference: String,
    /// Its date.
    pub date: Option<Date>,
    /// The SHA-256 of its bytes.
    pub sha256: Option<[u8; 32]>,
}

/// One exchange rate of an entry: `mantissa × 10^exponent` units of
/// `quote` per unit of `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RateRow {
    /// The entry.
    pub entry: EntryId,
    /// The entry's position.
    pub position: u64,
    /// The rate's number in the entry, from 0.
    pub ordinal: u16,
    /// The base currency.
    pub base: Currency,
    /// The quote currency.
    pub quote: Currency,
    /// The normalised mantissa.
    pub mantissa: i128,
    /// The decimal exponent.
    pub exponent: i8,
}

/// One account master-data event (GoBD Rz. 111: the history, not only the
/// state).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountRow {
    /// The record's position.
    pub position: u64,
    /// The account.
    pub handle: AccountHandle,
    /// Its path.
    pub path: AccountPath,
    /// Its kind.
    pub kind: AccountKind,
    /// The registration, or the field changed and its new value.
    pub action: AccountAction,
    /// From when the event applies.
    pub effective: Date,
    /// Who made it.
    pub actor: Label,
}

/// One currency declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CurrencyRow {
    /// The record's position.
    pub position: u64,
    /// The currency.
    pub code: Currency,
    /// Its number of decimal places.
    pub exponent: u8,
    /// From when it applies.
    pub effective: Date,
    /// Who declared it.
    pub actor: Label,
}

/// A seal: a period closed for good (Festschreibung).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealRow {
    /// The record's position.
    pub position: u64,
    /// The period.
    pub period: PeriodId,
    /// Its first day.
    pub start: Date,
    /// Its last day.
    pub end: Date,
    /// The log position of the last record the seal covers.
    pub through: u64,
    /// The number of accounts its balance trees hold.
    pub account_count: u32,
}

/// One (currency, layer) of a seal: its balance tree's root and totals.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealTotalRow {
    /// The seal's position.
    pub seal_position: u64,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The balance tree's root.
    pub root: Hash,
    /// The debit total in minor units of `exponent`.
    pub debit_minor: i128,
    /// The credit total in minor units of `exponent`.
    pub credit_minor: i128,
    /// The currency's declared exponent on the period's last day.
    pub exponent: u8,
    /// The number of postings totalled.
    pub postings: u64,
}
