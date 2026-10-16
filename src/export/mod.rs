//! The GoBD export view: the books as verifiable flat rows (§ 147 Abs. 6
//! AO, GoBD Rz. 94, 165–167).
//!
//! An export is one pass over a log prefix — pinned to a tree head, a
//! signed checkpoint for example — in position order. It emits a
//! [`LeafRow`] for every record, so a recipient rebuilds the tree root and
//! compares it with the checkpoint, and rows for entries (with their
//! postings, Konto/Gegenkonto [`pairs`], documents and rates), account and
//! currency master data, and seals. Every entry rebuilds from its rows to
//! its leaf hash ([`rebuild_entry`]); [`verify`] checks a whole export
//! against its head. Amounts are integers in minor units of the currency's
//! declared exponent, with the exponent beside them: no row holds a
//! formatted decimal, a float or a rounded value.
//!
//! **No file format is in the crate.** DATEV EXTF, a GDPdU `index.xml`
//! with its tables, SAF-T: each is an adapter over these rows plus its own
//! configuration (account numbers, tax keys, client header).
//!
//! ```
//! use doubleentry::export::{ExportRow, ExportScope, verify};
//! use doubleentry::prelude::*;
//!
//! let mut books = Journal::new(LedgerId::new("example.com/acme")?);
//! books.declare_currency(EUR, 2, date!("2026-01-01"))?;
//! books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01"))?;
//! books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
//! books.open("Liabilities:VAT", AccountKind::Liability, date!("2026-01-01"))?;
//! books.define_months(2026, 3..=3)?;
//! books.record(
//!     Entry::new("invoice-1", date!("2026-03-15"))
//!         .debit("Assets:Receivables", amount!("119.00"), EUR)
//!         .credit("Income:Sales", amount!("100.00"), EUR)
//!         .credit("Liabilities:VAT", amount!("19.00"), EUR),
//! )?;
//! let head = books.head();
//! let rows: Vec<ExportRow> = books.export(ExportScope::period("2026-03"), Some(head))?.collect::<Result<_>>()?;
//! let gegenkonten: Vec<_> = rows
//!     .iter()
//!     .filter_map(|row| match row {
//!         ExportRow::Pair(pair) => Some((pair.credit_path.to_string(), pair.minor_units)),
//!         _ => None,
//!     })
//!     .collect();
//! assert_eq!(gegenkonten, [("Income:Sales".to_owned(), 10_000), ("Liabilities:VAT".to_owned(), 1_900)]);
//! verify(rows, &head)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Order
//!
//! Rows follow the log: for each record its [`LeafRow`], then its own rows.
//! An entry: [`EntryRow`], its [`PostingRow`]s by index, its [`PairRow`]s
//! by ordinal, its [`DocumentRow`]s, its [`RateRow`]s. A seal: [`SealRow`],
//! then its [`SealTotalRow`]s by (currency, layer). Account and currency
//! records: one [`AccountRow`] or [`CurrencyRow`]. Genesis, policy, period
//! and clearing records have their leaf row only.
//!
//! # Scope
//!
//! [`ExportScope::All`] exports everything. [`ExportScope::Period`] exports
//! the entries booked in the period (with their child rows) and the
//! period's seal; [`ExportScope::Positions`] the entries and seals at
//! positions in the range. Leaf, account and currency rows are always
//! complete up to the head, so a scoped export verifies like a full one.
//!
//! # Row schemas
//!
//! Field order as listed; `?` marks a nullable field. Dates are calendar
//! dates (ISO 8601 `YYYY-MM-DD`), hashes 32 bytes (adapters render hex),
//! ids 16 bytes, amounts `i128` minor units.
//!
//! | Row | Fields |
//! |---|---|
//! | [`LeafRow`] | `position u64`, `kind` record kind (`u8` 1–8: genesis, account, currency, policy, period, entry, clearing, seal), `leaf_hash [32]` |
//! | [`EntryRow`] | `position u64`, `entry [16]`, `key text`, `booking_date date`, `value_date date`, `captured_at (unix seconds i64, UTC offset minutes i16)?`, `kind text?`, `period text`, `memo_commitment [32]?`, `memo_text text?` (absent once erased), `memo_salt [16]?`, `reverses [16]?`, `hold_action (open, post, void, expire)?`, `hold_target [16]?`, `hold_expires date?`, `post_amount_minor i128?` (a partial post, in the first posting row's exponent), `posting_count u16`, `document_count u16`, `rate_count u16`, `leaf_hash [32]` |
//! | [`PostingRow`] | `entry [16]`, `position u64`, `index u16`, `account u32`, `path text`, `kind (asset, liability, equity, income, expense)`, `class text?`, `side (debit, credit)`, `minor_units i128 > 0`, `exponent u8`, `currency text(3)`, `layer (settled, pending)`, `dimensions [(axis text, value text)]` ascending by axis, `booking_date date`, `value_date date` |
//! | [`PairRow`] | `entry [16]`, `position u64`, `currency text(3)`, `layer`, `ordinal u16`, `debit_index u16`, `debit_account u32`, `debit_path text`, `credit_index u16`, `credit_account u32`, `credit_path text`, `minor_units i128 > 0`, `exponent u8` |
//! | [`DocumentRow`] | `entry [16]`, `position u64`, `ordinal u16`, `doc_type text`, `reference text` (≤ 256 bytes), `date date?`, `sha256 [32]?` |
//! | [`RateRow`] | `entry [16]`, `position u64`, `ordinal u16`, `base text(3)`, `quote text(3)`, `mantissa i128`, `exponent i8` |
//! | [`AccountRow`] | `position u64`, `handle u32`, `path text`, `kind`, `action` (register: `kind`, `class text?`, `open_from date`, `open_to date?`, `limit`; or change: `open_to date?` \| `limit` \| `class text?`), `effective date`, `actor text` |
//! | [`CurrencyRow`] | `position u64`, `code text(3)`, `exponent u8`, `effective date`, `actor text` |
//! | [`SealRow`] | `position u64`, `period text`, `start date`, `end date`, `through u64`, `account_count u32` |
//! | [`SealTotalRow`] | `seal_position u64`, `currency text(3)`, `layer`, `root [32]`, `debit_minor i128`, `credit_minor i128`, `exponent u8`, `postings u64` |
//!
//! Text is UTF-8, NFC. A posting row carries the account's path, kind and
//! class **in force on the entry's booking date** (by effective date); the
//! [`AccountRow`]s are the full history. An amount is `units / 10^(P − e)`
//! for the ledger scale `P` and the currency's exponent `e` in force on the
//! booking date (on a seal's last day for its totals); the rebuild rule is
//! `units = minor × 10^(P − e)`.
//!
//! # Field classes of the target formats
//!
//! Every field class of DATEV EXTF Buchungsstapel, the GDPdU
//! Beschreibungsstandard and the SAF-T general ledger resolves to a view
//! field, a derivation from view fields, or named adapter configuration
//! (the formats' own details are the adapters').
//!
//! | Field class | View source | DATEV EXTF Buchungsstapel | GDPdU / Beschreibungsstandard | SAF-T GL (OECD 2.0) |
//! |---|---|---|---|---|
//! | Amount and side | pair row minor units + exponent; posting row side, minor units, exponent | Umsatz (unsigned) + Soll/Haben-Kennzeichen, from **pair rows** | posting rows; Numeric column, Accuracy = exponent | `Line` DebitAmount / CreditAmount, from **posting rows** |
//! | Konto / Gegenkonto | pair row debit and credit account (handle + path); posting row account | Konto / Gegenkonto — account **numbers** from adapter config (path → SKR03/SKR04) | AlphaNumeric path + handle; ForeignKey to the account table | `AccountID` (adapter config may map path → code) |
//! | Currency, rate | posting row currency; rate rows (base, quote, mantissa, exponent) | WKZ Umsatz; Kurs, Basis-Umsatz from rate rows | ISO code column; rate table | CurrencyCode, CurrencyAmount, ExchangeRate |
//! | Dates | entry row booking date, value date, captured-at; document row date | Belegdatum ← document date; Leistungsdatum ← value date | Date columns (ISO 8601 in the view; format by adapter) | TransactionDate / GLPostingDate ← booking date; SystemEntryDate ← captured-at |
//! | Document | document rows: type, reference, date, SHA-256 | Belegfeld 1 ← reference (adapter picks by type) | document table, ForeignKey entry id | SourceDocumentID ← reference |
//! | Text and identity | entry row memo text (when not erased), kind, entry id, idempotency key, position | Buchungstext ← memo text or kind (truncation by adapter) | AlphaNumeric columns; entry id as primary key | TransactionID ← entry id; Description ← memo text |
//! | Account master (Kontoart, Kontotyp) | posting row kind and class in force on the booking date; account rows (every master-data event: effective, actor, position) | Sachkonten data category by adapter | account table and history table | MasterFiles/GeneralLedgerAccounts: AccountID, AccountType ← kind/class |
//! | Tax keys, cost objects | posting row dimensions (caller-named axes, sorted) | BU-Schlüssel, KOST1/KOST2 ← axes named in adapter config | one column per configured axis | TaxInformation, Analysis ← axes by adapter config |
//! | Reservations | entry row hold action; posting row layer | (adapter: export Settled only, or flag) | layer column | (adapter) |
//! | Reversals | entry row `reverses` | Generalumkehr by adapter | AlphaNumeric column | (adapter) |
//! | Festschreibung / sealing | entry row period; seal and seal-total rows; leaf rows | Festschreibung flag ← always set (an entry is immutable when recorded, Rz. 85) | seal table; hashes as hex | (none; adapter may add a header attestation) |
//! | Period balances | `trial_balance` / `statement` over a period, not an export row | (not part of Buchungsstapel) | balance table from the report | Opening/Closing Debit/CreditBalance from the report |
//! | Header / client | ledger id (genesis leaf), head; adapter config (Berater-/Mandantennummer, fiscal-year start, account length) | EXTF header — adapter config + export scope | `index.xml` written by the adapter from the row schemas above | Header — adapter config |

mod pairs;
mod rows;
mod verify;

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;

use time::Date;

pub use self::pairs::{Pair, pairs};
pub use self::rows::{
    AccountRow, CurrencyRow, DocumentRow, EntryRow, ExportRow, HoldKind, LeafRow, PairRow,
    PostingRow, RateRow, SealRow, SealTotalRow,
};
use self::verify::to_minor;
pub use self::verify::{VerifyError, rebuild_entry, verify, verify_with_precision};
use crate::core::{MemState, MemoPlain};
use crate::error::{Error, IntegrityError, IntegrityKind, QueryError};
pub use crate::ledger::ExportPages;
use crate::model::{
    AccountAction, Currency, EntryId, EntryRecord, HoldAction, LedgerId, PeriodId, PostAmount,
    Record, Seal,
};
use crate::report::accounts::AccountBook;

/// Which entries and seals an export holds (leaf, account and currency rows
/// are always complete up to the head).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExportScope {
    /// Every entry and seal.
    All,
    /// The entries booked in the period with this id, and its seal; an id
    /// no period has is [`QueryError::UnknownPeriod`], one that is no label
    /// [`QueryError::InvalidLabel`].
    Period(String),
    /// The entries and seals at these positions.
    Positions(Range<u64>),
}

impl ExportScope {
    /// The entries booked in period `id`, and its seal.
    #[must_use]
    pub fn period(id: impl AsRef<str>) -> Self {
        Self::Period(id.as_ref().to_owned())
    }
}

/// A scope with its period id checked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    All,
    Period(PeriodId),
    Positions(Range<u64>),
}

/// The export as a pure fold: records in, rows out — what both façades
/// drive, and what a verifier with leaves of its own can run.
///
/// Feed every record of the log in position order from the genesis
/// record, with the memo kept beside each entry.
///
/// ```
/// use doubleentry::export::{ExportRow, ExportScope, Exporter};
/// use doubleentry::{AccountKind, Journal, LedgerId, date, record::Record};
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
/// let mut exporter = Exporter::<2>::new(ExportScope::All)?;
/// let mut rows = Vec::new();
/// for leaf in books.leaves() {
///     let record = Record::<2>::decode(leaf.bytes)?;
///     rows.extend(exporter.feed(leaf.position, &record, leaf.memo)?);
/// }
/// assert!(matches!(rows.last(), Some(ExportRow::Account(account)) if account.path.to_string() == "Assets:Bank"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct Exporter<const P: u8 = 2> {
    scope: Scope,
    ledger: Option<LedgerId>,
    next: u64,
    accounts: AccountBook,
    currencies: BTreeMap<Currency, Vec<(Date, u8)>>,
    /// The defined periods by first day: (id, last day).
    periods: BTreeMap<Date, (PeriodId, Date)>,
}

impl<const P: u8> Exporter<P> {
    /// An exporter of `scope`, before the genesis record.
    ///
    /// # Errors
    ///
    /// [`QueryError::InvalidLabel`] for a period id that is no label.
    pub fn new(scope: ExportScope) -> Result<Self, QueryError> {
        let scope = match scope {
            ExportScope::All => Scope::All,
            ExportScope::Period(id) => Scope::Period(
                PeriodId::new(&id).map_err(|_| QueryError::InvalidLabel { text: id })?,
            ),
            ExportScope::Positions(range) => Scope::Positions(range),
        };
        Ok(Self {
            scope,
            ledger: None,
            next: 0,
            accounts: AccountBook::default(),
            currencies: BTreeMap::new(),
            periods: BTreeMap::new(),
        })
    }

    /// The period a [`ExportScope::Period`] names.
    pub(crate) const fn period(&self) -> Option<&PeriodId> {
        match &self.scope {
            Scope::Period(id) => Some(id),
            Scope::All | Scope::Positions(_) => None,
        }
    }

    /// The rows of the record at `position` — the next one — with the memo
    /// kept beside it (`None`: no memo, or erased).
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] for a position out of order, a log not starting
    /// with its genesis, an entry outside every period or on an unknown
    /// account, or an amount with more decimals than its currency's
    /// exponent — records the rule core would not have written.
    pub fn feed(
        &mut self,
        position: u64,
        record: &Record<P>,
        memo: Option<&MemoPlain>,
    ) -> Result<Vec<ExportRow>, Error> {
        let broken = |kind| Error::from(IntegrityError::new(kind, Some(position)));
        if position != self.next || (position == 0) != matches!(record, Record::Genesis(_)) {
            return Err(broken(IntegrityKind::MissingRecord));
        }
        self.next = position.saturating_add(1);
        let mut rows = vec![ExportRow::Leaf(LeafRow {
            position,
            kind: record.kind(),
            leaf_hash: record.leaf_hash(),
        })];
        match record {
            Record::Genesis(genesis) => self.ledger = Some(genesis.ledger.clone()),
            Record::Account(event) => {
                self.accounts.feed(event);
                let handle = match &event.action {
                    AccountAction::Register(registration) => registration.handle,
                    AccountAction::Change { handle, .. } => *handle,
                };
                let account = self
                    .accounts
                    .get(handle)
                    .ok_or_else(|| broken(IntegrityKind::ReplayMismatch))?;
                rows.push(ExportRow::Account(AccountRow {
                    position,
                    handle,
                    path: account.path().clone(),
                    kind: account.kind(),
                    action: event.action.clone(),
                    effective: event.effective,
                    actor: event.actor.clone(),
                }));
            }
            Record::Currency(declaration) => {
                self.currencies
                    .entry(declaration.code)
                    .or_default()
                    .push((declaration.effective, declaration.exponent));
                rows.push(ExportRow::Currency(CurrencyRow {
                    position,
                    code: declaration.code,
                    exponent: declaration.exponent,
                    effective: declaration.effective,
                    actor: declaration.actor.clone(),
                }));
            }
            Record::Period(period) => {
                self.periods
                    .insert(period.start(), (period.id().clone(), period.end()));
            }
            Record::Entry(entry) => {
                if self.holds_entry(position, entry.parts().booking_date) {
                    self.entry(position, record, entry, memo, &mut rows)
                        .ok_or_else(|| broken(IntegrityKind::ReplayMismatch))?;
                }
            }
            Record::Seal(seal) => {
                if self.holds_seal(position, seal) {
                    self.seal(position, seal, &mut rows)
                        .ok_or_else(|| broken(IntegrityKind::SealMismatch))?;
                }
            }
            Record::Policy(_) | Record::Clearing(_) => {}
        }
        Ok(rows)
    }

    /// Whether the scope holds the entry at `position` booked on `booking`.
    fn holds_entry(&self, position: u64, booking: Date) -> bool {
        match &self.scope {
            Scope::All => true,
            Scope::Positions(range) => range.contains(&position),
            Scope::Period(id) => self
                .period_of(booking)
                .is_some_and(|(period, _)| period == id),
        }
    }

    /// Whether the scope holds the seal at `position`.
    fn holds_seal(&self, position: u64, seal: &Seal<P>) -> bool {
        match &self.scope {
            Scope::All => true,
            Scope::Positions(range) => range.contains(&position),
            Scope::Period(id) => seal.parts().period == *id,
        }
    }

    /// The period containing `date`: (id, last day).
    fn period_of(&self, date: Date) -> Option<(&PeriodId, Date)> {
        let (_, (id, end)) = self.periods.range(..=date).next_back()?;
        (date <= *end).then_some((id, *end))
    }

    /// The exponent of `currency` in force on `date`.
    fn exponent(&self, currency: Currency, date: Date) -> Option<u8> {
        let versions = self.currencies.get(&currency)?;
        versions
            .iter()
            .rev()
            .find(|(effective, _)| *effective <= date)
            .or_else(|| versions.first())
            .map(|(_, exponent)| *exponent)
    }

    /// Appends an entry's rows; `None` when the rule core would not have
    /// written it.
    fn entry(
        &self,
        position: u64,
        record: &Record<P>,
        entry: &EntryRecord<P>,
        memo: Option<&MemoPlain>,
        rows: &mut Vec<ExportRow>,
    ) -> Option<()> {
        let parts = entry.parts();
        let id = entry.id(self.ledger.as_ref()?);
        let booking = parts.booking_date;
        let (period, _) = self.period_of(booking)?;
        let mut children = Vec::new();
        for (index, posting) in (0u16..).zip(&parts.postings) {
            let account = self.accounts.get(posting.handle)?;
            let exponent = self.exponent(posting.currency, booking)?;
            children.push(ExportRow::Posting(PostingRow {
                entry: id,
                position,
                index,
                account: posting.handle,
                path: account.path().clone(),
                kind: account.kind(),
                class: account.class_on(booking).cloned(),
                side: posting.side,
                minor_units: to_minor::<P>(posting.amount.to_minor(), exponent)?,
                exponent,
                currency: posting.currency,
                layer: posting.layer,
                dimensions: posting.dimensions.clone(),
                booking_date: booking,
                value_date: parts.value_date,
            }));
        }
        for pair in pairs(&parts.postings) {
            let debit = parts.postings.get(usize::from(pair.debit))?;
            let credit = parts.postings.get(usize::from(pair.credit))?;
            let exponent = self.exponent(debit.currency, booking)?;
            children.push(ExportRow::Pair(PairRow {
                entry: id,
                position,
                currency: debit.currency,
                layer: debit.layer,
                ordinal: pair.ordinal,
                debit_index: pair.debit,
                debit_account: debit.handle,
                debit_path: self.accounts.get(debit.handle)?.path().clone(),
                credit_index: pair.credit,
                credit_account: credit.handle,
                credit_path: self.accounts.get(credit.handle)?.path().clone(),
                minor_units: to_minor::<P>(pair.amount.to_minor(), exponent)?,
                exponent,
            }));
        }
        for (ordinal, document) in (0u16..).zip(&parts.documents) {
            children.push(ExportRow::Document(DocumentRow {
                entry: id,
                position,
                ordinal,
                doc_type: document.doc_type().clone(),
                reference: document.reference().to_owned(),
                date: document.date(),
                sha256: document.sha256().copied(),
            }));
        }
        for (ordinal, rate) in (0u16..).zip(&parts.rates) {
            children.push(ExportRow::Rate(RateRow {
                entry: id,
                position,
                ordinal,
                base: rate.base(),
                quote: rate.quote(),
                mantissa: rate.mantissa(),
                exponent: rate.exponent(),
            }));
        }
        let (hold_action, hold_target, hold_expires, post_amount) = match parts.hold {
            None => (None, None, None, None),
            Some(HoldAction::Open { expires }) => (Some(HoldKind::Open), None, expires, None),
            Some(HoldAction::Post { hold, amount }) => (
                Some(HoldKind::Post),
                Some(hold),
                None,
                match amount {
                    PostAmount::Full => None,
                    PostAmount::Partial(amount) => Some(amount),
                },
            ),
            Some(HoldAction::Void { hold }) => (Some(HoldKind::Void), Some(hold), None, None),
            Some(HoldAction::Expire { hold }) => (Some(HoldKind::Expire), Some(hold), None, None),
        };
        let post_amount_minor = match post_amount {
            None => None,
            Some(amount) => {
                let exponent = match parts.postings.first() {
                    Some(first) => self.exponent(first.currency, booking)?,
                    None => P,
                };
                Some(to_minor::<P>(amount.to_minor(), exponent)?)
            }
        };
        let memo = memo.filter(|_| parts.memo.is_some());
        rows.push(ExportRow::Entry(EntryRow {
            position,
            entry: id,
            key: parts.key.clone(),
            booking_date: booking,
            value_date: parts.value_date,
            captured_at: parts.captured_at,
            kind: parts.kind.clone(),
            period: period.clone(),
            memo_commitment: parts.memo,
            memo_text: memo.map(|memo| memo.text.clone()),
            memo_salt: memo.map(|memo| memo.salt),
            reverses: parts.reverses,
            hold_action,
            hold_target,
            hold_expires,
            post_amount_minor,
            posting_count: u16::try_from(parts.postings.len()).ok()?,
            document_count: u16::try_from(parts.documents.len()).ok()?,
            rate_count: u16::try_from(parts.rates.len()).ok()?,
            leaf_hash: record.leaf_hash(),
        }));
        rows.append(&mut children);
        Some(())
    }

    /// Appends a seal's rows; `None` when its totals do not convert exactly.
    fn seal(&self, position: u64, seal: &Seal<P>, rows: &mut Vec<ExportRow>) -> Option<()> {
        let parts = seal.parts();
        rows.push(ExportRow::Seal(SealRow {
            position,
            period: parts.period.clone(),
            start: parts.start,
            end: parts.end,
            through: parts.through,
            account_count: parts.accounts,
        }));
        for tree in &parts.trees {
            let exponent = self.exponent(tree.currency, parts.end)?;
            rows.push(ExportRow::SealTotal(SealTotalRow {
                seal_position: position,
                currency: tree.currency,
                layer: tree.layer,
                root: tree.root,
                debit_minor: to_minor::<P>(tree.totals.debit().to_minor(), exponent)?,
                credit_minor: to_minor::<P>(tree.totals.credit().to_minor(), exponent)?,
                exponent,
                postings: tree.totals.postings(),
            }));
        }
        Some(())
    }
}

/// A [`Journal`](crate::Journal)'s export: its rows, one by one
/// ([`Journal::export`](crate::Journal::export)).
#[derive(Debug)]
pub struct Export<'a, const P: u8 = 2> {
    state: &'a MemState<P>,
    exporter: Exporter<P>,
    next: u64,
    bound: u64,
    rows: VecDeque<ExportRow>,
    failed: bool,
}

impl<'a, const P: u8> Export<'a, P> {
    /// The export of the records of `state` below `bound`.
    pub(crate) const fn new(state: &'a MemState<P>, exporter: Exporter<P>, bound: u64) -> Self {
        Self {
            state,
            exporter,
            next: 0,
            bound,
            rows: VecDeque::new(),
            failed: false,
        }
    }

    /// The rows of the next record.
    fn advance(&mut self) -> Result<(), Error> {
        let position = self.next;
        let leaf = self
            .state
            .leaf(position)
            .ok_or_else(|| IntegrityError::new(IntegrityKind::MissingRecord, Some(position)))?;
        let record = Record::<P>::decode(leaf.bytes)
            .map_err(|_| IntegrityError::new(IntegrityKind::Undecodable, Some(position)))?;
        self.rows
            .extend(self.exporter.feed(position, &record, leaf.memo)?);
        self.next = position.saturating_add(1);
        Ok(())
    }
}

impl<const P: u8> Iterator for Export<'_, P> {
    type Item = Result<ExportRow, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.rows.pop_front() {
                return Some(Ok(row));
            }
            if self.failed || self.next >= self.bound {
                return None;
            }
            if let Err(error) = self.advance() {
                self.failed = true;
                return Some(Err(error));
            }
        }
    }
}

/// The id of `record` when it is an entry with a memo: the memos an export
/// reads beside the leaves.
pub(crate) fn with_memo<const P: u8>(ledger: &LedgerId, record: &Record<P>) -> Option<EntryId> {
    match record {
        Record::Entry(entry) if entry.parts().memo.is_some() => Some(entry.id(ledger)),
        _ => None,
    }
}
