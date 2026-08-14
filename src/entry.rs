//! Entries — atomic, balanced sets of postings.
//!
//! # Balanced by construction
//!
//! [`Entry`] carries a type-state parameter. A draft is freely editable and
//! proves nothing; sealing it runs every invariant and yields an
//! [`Entry<Balanced, P>`]. The balanced form has private fields and no public
//! constructor, and the marker types sit behind a sealed trait, so nothing
//! outside this crate can produce one by any route other than validation.
//!
//! The guarantee this provides is worth stating precisely, because it is easy to
//! overclaim. Whether a set of postings balances is a property of runtime values,
//! so no type system short of dependent types decides it at compile time. What
//! the type state gives is that **an unbalanced entry cannot be represented as a
//! validated one**: every API that persists, exports, or commits to an entry
//! accepts only the balanced form.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use smallvec::SmallVec;
use time::Date;
use uuid::Uuid;

use crate::account::{AccountId, AccountRegistry, encode_date};
use crate::balance::Balance;
use crate::canonical::{Canonical, CanonicalWriter};
use crate::dimensions::{DimensionError, Label};
use crate::hash::{Hash, tag, tagged};
use crate::money::{Amount, Currency, MoneyError};
use crate::period::{PeriodCalendar, PeriodState};
use crate::posting::Posting;
use crate::serde_support::validating_string_serde;

/// Maximum characters in an entry description.
pub const MAX_DESCRIPTION_LEN: usize = 512;

/// Maximum bytes in an idempotency key.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

/// Maximum number of postings one entry may carry.
///
/// Not an arbitrary bound. A posting is addressed by its position within its
/// entry, and that position is narrowed twice on the way out of the engine: to a
/// `u16` in [`PostingRef`](crate::PostingRef), and to a `SMALLINT` in the
/// reference SQL schema. `i16::MAX` is the largest index both can carry, so an
/// entry beyond it could not be referenced by a clearing, could not be written to
/// a posting row, and could not appear correctly in a statement.
///
/// Enforced at validation rather than truncated at the boundary, because a
/// truncated position is a posting silently pointing at the wrong movement.
pub const MAX_POSTINGS: usize = i16::MAX as usize;

/// Number of postings held inline before spilling to the heap.
const INLINE_POSTINGS: usize = 4;

mod sealed {
    /// Restricts the entry type-state to the markers defined in this crate.
    pub trait State {}
}

/// Type state for an entry still under construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Draft;

/// Type state for an entry that has passed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balanced;

impl sealed::State for Draft {}
impl sealed::State for Balanced {}

/// Identifier for an entry.
///
/// Time-ordered identifiers keep insertion local in a B-tree index. The engine
/// never generates one itself: identity is supplied by the caller so that a
/// replay produces byte-identical output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EntryId(Uuid);

impl EntryId {
    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Generates a fresh time-ordered identifier.
    ///
    /// Provided for callers that have no identifier of their own. It reads the
    /// clock, so it is not part of the deterministic path; the engine never
    /// calls it.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7()) // purity-exempt: identity, not ledger state
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// A caller-supplied key that makes recording an entry idempotent.
///
/// Two submissions carrying the same key are the same logical transaction.
/// Whether that is a safe replay or a conflict is decided by comparing content
/// hashes, not by comparing keys.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyKey(Vec<u8>);

#[cfg(feature = "serde")]
impl serde::Serialize for IdempotencyKey {
    /// Serialised as lowercase hex, since keys are arbitrary bytes.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for IdempotencyKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        Self::parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

impl IdempotencyKey {
    /// Wraps raw key bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, EntryFieldError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(EntryFieldError::EmptyIdempotencyKey);
        }
        if bytes.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(EntryFieldError::IdempotencyKeyTooLong);
        }
        Ok(Self(bytes))
    }

    /// The key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Renders the key as lowercase hex.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(self.0.len().saturating_mul(2));
        for b in &self.0 {
            out.push(nibble_to_hex(b >> 4));
            out.push(nibble_to_hex(b & 0x0f));
        }
        out
    }

    /// Parses a key from lowercase or uppercase hex.
    pub fn parse_hex(s: &str) -> Result<Self, EntryFieldError> {
        if s.len() & 1 == 1 {
            return Err(EntryFieldError::MalformedIdempotencyKey);
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        for pair in s.as_bytes().chunks_exact(2) {
            let [hi, lo] = pair else {
                return Err(EntryFieldError::MalformedIdempotencyKey);
            };
            let (hi, lo) = (hex_to_nibble(*hi)?, hex_to_nibble(*lo)?);
            out.push(hi.wrapping_shl(4) | lo);
        }
        Self::new(out)
    }
}

const fn nibble_to_hex(n: u8) -> char {
    match n & 0x0f {
        0 => '0',
        1 => '1',
        2 => '2',
        3 => '3',
        4 => '4',
        5 => '5',
        6 => '6',
        7 => '7',
        8 => '8',
        9 => '9',
        10 => 'a',
        11 => 'b',
        12 => 'c',
        13 => 'd',
        14 => 'e',
        _ => 'f',
    }
}

const fn hex_to_nibble(c: u8) -> Result<u8, EntryFieldError> {
    match c {
        b'0'..=b'9' => Ok(c.wrapping_sub(b'0')),
        b'a'..=b'f' => Ok(c.wrapping_sub(b'a').wrapping_add(10)),
        b'A'..=b'F' => Ok(c.wrapping_sub(b'A').wrapping_add(10)),
        _ => Err(EntryFieldError::MalformedIdempotencyKey),
    }
}

/// A free-text description of an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Description(String);

validating_string_serde!(Description);

impl Description {
    /// Validates and wraps a description.
    pub fn new(s: impl Into<String>) -> Result<Self, EntryFieldError> {
        let s = s.into();
        if s.chars().count() > MAX_DESCRIPTION_LEN {
            return Err(EntryFieldError::DescriptionTooLong);
        }
        if s.chars().any(|c| c.is_control() && c != '\n') {
            return Err(EntryFieldError::DescriptionControlCharacter);
        }
        Ok(Self(s))
    }

    /// The description text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who recorded an entry, and on whose behalf.
///
/// Recorded on every entry and folded into its hash. An audit trail that cannot
/// say who made a booking is not an audit trail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Provenance {
    /// The acting principal.
    pub actor: Option<Label>,
    /// The system the entry originated in.
    pub source: Option<Label>,
    /// Correlation identifier tying this entry to an external process.
    pub correlation: Option<Label>,
}

impl Provenance {
    /// Empty provenance.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Sets the acting principal.
    pub fn with_actor(mut self, actor: &str) -> Result<Self, DimensionError> {
        self.actor = Some(Label::new(actor)?);
        Ok(self)
    }

    /// Sets the originating system.
    pub fn with_source(mut self, source: &str) -> Result<Self, DimensionError> {
        self.source = Some(Label::new(source)?);
        Ok(self)
    }

    /// Sets the correlation identifier.
    pub fn with_correlation(mut self, correlation: &str) -> Result<Self, DimensionError> {
        self.correlation = Some(Label::new(correlation)?);
        Ok(self)
    }
}

impl Canonical for Provenance {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.option(self.actor.as_ref(), |w, v| v.encode(w));
        w.option(self.source.as_ref(), |w, v| v.encode(w));
        w.option(self.correlation.as_ref(), |w, v| v.encode(w));
    }
}

/// A reference to the source document behind an entry.
///
/// Binding the document's content hash into the entry makes the link
/// tamper-evident: a document swapped after the fact no longer matches the
/// booking that cites it.
///
/// The hash is optional because the alternative is worse. Systems routinely
/// book against a document they hold only an identifier for — an invoice number
/// arriving on a message bus, a payment reference from a bank statement — and a
/// mandatory hash pushes those callers into inventing one, which produces a
/// commitment that looks cryptographic and verifies nothing. Leaving it absent
/// says exactly what is true: the entry names a document without vouching for
/// its contents. [`Self::is_verifiable`] distinguishes the two, and the presence
/// of the hash is itself part of the canonical encoding, so an unhashed
/// reference cannot later be passed off as a hashed one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DocumentRef {
    /// Identifier of the document in its own system.
    pub id: Label,
    /// Content hash of the document, when the citing system holds one.
    pub content_hash: Option<Hash>,
}

impl DocumentRef {
    /// Creates a document reference bound to the document's contents.
    ///
    /// # Errors
    ///
    /// Returns an error if `id` is not a valid label.
    pub fn new(id: &str, content_hash: Hash) -> Result<Self, DimensionError> {
        Ok(Self {
            id: Label::new(id)?,
            content_hash: Some(content_hash),
        })
    }

    /// Creates a reference that names a document without committing to it.
    ///
    /// Use when the identifier is all the citing system has. The entry records
    /// which document it relates to; it does not detect a change to that
    /// document.
    ///
    /// # Errors
    ///
    /// Returns an error if `id` is not a valid label.
    pub fn unverified(id: &str) -> Result<Self, DimensionError> {
        Ok(Self {
            id: Label::new(id)?,
            content_hash: None,
        })
    }

    /// True when the reference carries a content hash.
    #[must_use]
    pub const fn is_verifiable(&self) -> bool {
        self.content_hash.is_some()
    }
}

impl Canonical for DocumentRef {
    fn encode(&self, w: &mut CanonicalWriter) {
        self.id.encode(w);
        w.option(self.content_hash.as_ref(), |w, h| {
            w.fixed(h.as_bytes());
        });
    }
}

/// Failure constructing a bounded entry field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntryFieldError {
    /// The idempotency key was empty.
    #[error("idempotency key is empty")]
    EmptyIdempotencyKey,
    /// The idempotency key exceeded [`MAX_IDEMPOTENCY_KEY_LEN`].
    #[error("idempotency key exceeds {MAX_IDEMPOTENCY_KEY_LEN} bytes")]
    IdempotencyKeyTooLong,
    /// The idempotency key was not valid hex.
    #[error("idempotency key is not valid hex")]
    MalformedIdempotencyKey,
    /// The description exceeded [`MAX_DESCRIPTION_LEN`].
    #[error("description exceeds {MAX_DESCRIPTION_LEN} characters")]
    DescriptionTooLong,
    /// The description contained a control character other than a newline.
    #[error("description contains a control character")]
    DescriptionControlCharacter,
}

/// Which currencies a ledger accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurrencyPolicy {
    /// Any currency, balanced independently per currency.
    #[default]
    Multi,
    /// Exactly one currency.
    Single(Currency),
}

/// Ledger-wide validation settings.
///
/// Everything here is off by default: a policy the caller did not ask for is a
/// rule they will discover by having a valid booking rejected.
#[derive(Debug, Clone, Default)]
pub struct LedgerPolicy {
    /// Accepted currencies.
    pub currency: CurrencyPolicy,
    /// Dimension axes every posting must carry.
    ///
    /// Set this where the books cannot be kept without an attribution — a
    /// regulated activity, a mandate, a fund — so that an unattributed posting
    /// is rejected at the door rather than silently landing outside every
    /// grouping a report knows about. Discovering it later means restating.
    ///
    /// The engine checks presence, never the value: which values are legal is a
    /// question about your business, and one this crate has no way to answer.
    pub required_dimensions: Vec<Label>,
    /// Largest permitted gap in days between booking date and value date.
    pub max_value_date_drift_days: Option<i64>,
}

impl LedgerPolicy {
    /// A policy that constrains nothing.
    #[must_use]
    pub fn permissive() -> Self {
        Self::default()
    }

    /// Restricts the ledger to one currency.
    #[must_use]
    pub fn in_currency(mut self, currency: Currency) -> Self {
        self.currency = CurrencyPolicy::Single(currency);
        self
    }

    /// Requires `axis` on every posting.
    #[must_use]
    pub fn requiring(mut self, axis: Label) -> Self {
        if !self.required_dimensions.contains(&axis) {
            self.required_dimensions.push(axis);
        }
        self
    }

    /// Bounds how far a value date may sit from its booking date.
    #[must_use]
    pub fn with_max_value_date_drift(mut self, days: i64) -> Self {
        self.max_value_date_drift_days = Some(days);
        self
    }
}

/// Everything validation needs to check an entry.
#[derive(Debug, Clone, Copy)]
pub struct SealContext<'a> {
    /// The accounts the entry may reference.
    pub accounts: &'a AccountRegistry,
    /// The period calendar governing booking dates.
    pub calendar: &'a PeriodCalendar,
    /// Ledger-wide policy.
    pub policy: &'a LedgerPolicy,
}

/// A single reason an entry failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ValidationError {
    /// Fewer than two postings.
    #[error("entry has {count} posting(s); double-entry requires at least two")]
    TooFewPostings {
        /// Number of postings supplied.
        count: usize,
    },
    /// More postings than a posting position can address.
    #[error("entry has {count} postings; at most {MAX_POSTINGS} can be addressed")]
    TooManyPostings {
        /// Number of postings supplied.
        count: usize,
    },
    /// A posting carried a zero amount.
    #[error("posting {index} has a zero amount")]
    ZeroAmount {
        /// Index of the offending posting.
        index: usize,
    },
    /// A posting carried a negative amount.
    #[error("posting {index} has a negative amount; the side is carried by the direction")]
    NegativeAmount {
        /// Index of the offending posting.
        index: usize,
    },
    /// A posting referenced an account that is not registered.
    #[error("posting {index} references unregistered account {account}")]
    UnknownAccount {
        /// Index of the offending posting.
        index: usize,
        /// The account referenced.
        account: AccountId,
    },
    /// A posting targeted an aggregation node rather than a leaf.
    #[error("posting {index} targets account {account}, which has descendants")]
    NonLeafAccount {
        /// Index of the offending posting.
        index: usize,
        /// The account referenced.
        account: AccountId,
    },
    /// A posting fell outside its account's open window.
    #[error("posting {index} targets account {account}, not open on {on}")]
    AccountNotOpen {
        /// Index of the offending posting.
        index: usize,
        /// The account referenced.
        account: AccountId,
        /// The booking date.
        on: Date,
    },
    /// Debits and credits did not match for a currency.
    ///
    /// Amounts are reported in minor units at `scale`, so the variant stays
    /// independent of the entry's compile-time precision.
    #[error(
        "{currency} is unbalanced at scale {scale}: debits {debits_minor}, \
         credits {credits_minor} (difference {difference_minor})"
    )]
    Unbalanced {
        /// The currency that failed to balance.
        currency: Currency,
        /// Gross debits, in minor units.
        debits_minor: i64,
        /// Gross credits, in minor units.
        credits_minor: i64,
        /// Debits less credits, in minor units.
        difference_minor: i64,
        /// Decimal places the minor units are expressed in.
        scale: u8,
    },
    /// The booking date fell in a period that no longer accepts postings.
    #[error("booking date {on} falls in a {state} period")]
    ClosedPeriod {
        /// The booking date.
        on: Date,
        /// The state of the period covering it.
        state: PeriodState,
    },
    /// A currency outside the ledger's policy was used.
    #[error("currency {currency} is not permitted by this ledger")]
    CurrencyNotAllowed {
        /// The offending currency.
        currency: Currency,
    },
    /// A posting lacked a dimension the ledger's policy requires.
    #[error("posting {index} has no {axis} dimension")]
    MissingDimension {
        /// Index of the offending posting.
        index: usize,
        /// The axis the policy requires.
        axis: Label,
    },
    /// The value date was too far from the booking date.
    #[error("value date {value} is more than {max_days} days from booking date {booking}")]
    ValueDateDrift {
        /// The booking date.
        booking: Date,
        /// The value date.
        value: Date,
        /// The configured maximum.
        max_days: i64,
    },
    /// Accumulating the postings overflowed.
    #[error("summing {currency} overflowed")]
    Overflow {
        /// The currency being accumulated.
        currency: Currency,
    },
}

/// Every reason an entry failed validation.
///
/// Validation reports all violations rather than the first. A caller repairing a
/// batch import should not discover its problems one round trip at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// The individual violations.
    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.0
    }

    /// Number of violations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always false: this type is only constructed with at least one violation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True when any violation matches `predicate`.
    #[must_use]
    pub fn any(&self, predicate: impl Fn(&ValidationError) -> bool) -> bool {
        self.0.iter().any(predicate)
    }
}

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "entry validation failed with {} error(s)", self.0.len())?;
        for e in &self.0 {
            write!(f, "\n  - {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// An atomic, balanced collection of postings.
///
/// See the [module documentation](self) for what the `S` type parameter
/// guarantees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<S: sealed::State, const P: u8> {
    id: EntryId,
    idempotency_key: IdempotencyKey,
    booking_date: Date,
    value_date: Date,
    description: Description,
    postings: SmallVec<[Posting<P>; INLINE_POSTINGS]>,
    kind: Option<Label>,
    provenance: Provenance,
    document: Option<DocumentRef>,
    reverses: Option<EntryId>,
    original_booking_date: Option<Date>,
    _state: PhantomData<S>,
}

impl<const P: u8> Entry<Draft, P> {
    /// Starts a new draft entry.
    ///
    /// The value date defaults to the booking date.
    #[must_use]
    pub fn new(id: EntryId, idempotency_key: IdempotencyKey, booking_date: Date) -> Self {
        Self {
            id,
            idempotency_key,
            booking_date,
            value_date: booking_date,
            description: Description::default(),
            postings: SmallVec::new(),
            kind: None,
            provenance: Provenance::none(),
            document: None,
            reverses: None,
            original_booking_date: None,
            _state: PhantomData,
        }
    }

    /// Adds a posting.
    #[must_use]
    pub fn post(mut self, posting: Posting<P>) -> Self {
        self.postings.push(posting);
        self
    }

    /// Adds a debit to `account`.
    #[must_use]
    pub fn debit(self, account: AccountId, amount: Amount<P>, currency: Currency) -> Self {
        self.post(Posting::debit(account, amount, currency))
    }

    /// Adds a credit to `account`.
    #[must_use]
    pub fn credit(self, account: AccountId, amount: Amount<P>, currency: Currency) -> Self {
        self.post(Posting::credit(account, amount, currency))
    }

    /// Sets the value date.
    #[must_use]
    pub fn with_value_date(mut self, date: Date) -> Self {
        self.value_date = date;
        self
    }

    /// Sets the description.
    #[must_use]
    pub fn with_description(mut self, description: Description) -> Self {
        self.description = description;
        self
    }

    /// Tags the entry with a caller-defined kind.
    ///
    /// Entry-level, unlike [`Dimensions`](crate::Dimensions), which describe a single posting. What
    /// an entry *is* — an invoice, a payment, an advance, a correction — belongs
    /// to the whole entry, and putting it on the postings would repeat it and
    /// permit two postings of one entry to disagree.
    ///
    /// Opaque to the engine: it is stored, hashed, indexed, and grouped by,
    /// never interpreted. No vocabulary ships with this crate.
    #[must_use]
    pub fn with_kind(mut self, kind: Label) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Sets the provenance.
    #[must_use]
    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Attaches a source document reference.
    #[must_use]
    pub fn with_document(mut self, document: DocumentRef) -> Self {
        self.document = Some(document);
        self
    }

    /// Marks this entry as reversing `original`, preserving its booking date.
    #[must_use]
    pub fn reversing(mut self, original: EntryId, original_booking_date: Date) -> Self {
        self.reverses = Some(original);
        self.original_booking_date = Some(original_booking_date);
        self
    }

    /// Adopts a draft as validated on the evidence of its content hash.
    ///
    /// For a backend rehydrating an entry **it wrote itself**. Re-running
    /// [`Entry::seal`] would be wrong here: validation is against the ledger's
    /// *current* accounts, calendar, and policy, so a historical entry would
    /// start failing the day its account closed or its period sealed — even
    /// though it was valid when written and must stay readable forever.
    ///
    /// Comparing the content hash is both safer and cheaper. It proves the bytes
    /// are exactly what passed validation originally, which re-validation does
    /// not: re-validation would accept a *different* entry that also happens to
    /// balance. A mismatch means the row was altered underneath the engine, and
    /// is reported rather than returned.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the reconstructed entry does not hash to
    /// `expected`.
    pub fn adopt_verified(self, expected: Hash) -> Result<Entry<Balanced, P>, IntegrityError> {
        let adopted = Entry::<Balanced, P> {
            id: self.id,
            idempotency_key: self.idempotency_key,
            booking_date: self.booking_date,
            value_date: self.value_date,
            description: self.description,
            postings: self.postings,
            kind: self.kind,
            provenance: self.provenance,
            document: self.document,
            reverses: self.reverses,
            original_booking_date: self.original_booking_date,
            _state: PhantomData,
        };
        let actual = adopted.content_hash();
        if actual == expected {
            Ok(adopted)
        } else {
            Err(IntegrityError { expected, actual })
        }
    }

    /// Runs every invariant and produces a validated entry.
    ///
    /// # Errors
    ///
    /// Returns every violation found, not merely the first.
    pub fn seal(self, ctx: &SealContext<'_>) -> Result<Entry<Balanced, P>, ValidationErrors> {
        let mut errors = Vec::new();

        if self.postings.len() < 2 {
            errors.push(ValidationError::TooFewPostings {
                count: self.postings.len(),
            });
        }
        if self.postings.len() > MAX_POSTINGS {
            errors.push(ValidationError::TooManyPostings {
                count: self.postings.len(),
            });
        }

        // Per-currency gross totals; ordered so error reporting is deterministic.
        let mut totals: BTreeMap<Currency, Balance<P>> = BTreeMap::new();

        for (index, posting) in self.postings.iter().enumerate() {
            if posting.amount.is_zero() {
                errors.push(ValidationError::ZeroAmount { index });
            }
            if posting.amount.is_negative() {
                errors.push(ValidationError::NegativeAmount { index });
            }

            match ctx.accounts.get(posting.account) {
                None => errors.push(ValidationError::UnknownAccount {
                    index,
                    account: posting.account,
                }),
                Some(account) => {
                    if !ctx.accounts.is_leaf(posting.account) {
                        errors.push(ValidationError::NonLeafAccount {
                            index,
                            account: posting.account,
                        });
                    }
                    if !account.is_open_on(self.booking_date) {
                        errors.push(ValidationError::AccountNotOpen {
                            index,
                            account: posting.account,
                            on: self.booking_date,
                        });
                    }
                }
            }

            if let CurrencyPolicy::Single(allowed) = ctx.policy.currency
                && posting.currency != allowed
            {
                errors.push(ValidationError::CurrencyNotAllowed {
                    currency: posting.currency,
                });
            }

            for axis in &ctx.policy.required_dimensions {
                if !posting.dimensions.contains(axis.as_str()) {
                    errors.push(ValidationError::MissingDimension {
                        index,
                        axis: axis.clone(),
                    });
                }
            }

            let balance = totals.entry(posting.currency).or_default();
            if balance.add(posting.direction, posting.amount).is_err() {
                errors.push(ValidationError::Overflow {
                    currency: posting.currency,
                });
            }
        }

        for (currency, balance) in &totals {
            if !balance.is_balanced() {
                let debits_minor = balance.debits.to_minor();
                let credits_minor = balance.credits.to_minor();
                errors.push(ValidationError::Unbalanced {
                    currency: *currency,
                    debits_minor,
                    credits_minor,
                    difference_minor: debits_minor.saturating_sub(credits_minor),
                    scale: P,
                });
            }
        }

        let state = ctx.calendar.state_on(self.booking_date);
        if !state.accepts_postings() {
            errors.push(ValidationError::ClosedPeriod {
                on: self.booking_date,
                state,
            });
        }

        if let Some(max_days) = ctx.policy.max_value_date_drift_days {
            let booking = i64::from(self.booking_date.to_julian_day());
            let value = i64::from(self.value_date.to_julian_day());
            let drift = value.saturating_sub(booking).saturating_abs();
            if drift > max_days {
                errors.push(ValidationError::ValueDateDrift {
                    booking: self.booking_date,
                    value: self.value_date,
                    max_days,
                });
            }
        }

        if errors.is_empty() {
            Ok(Entry {
                id: self.id,
                idempotency_key: self.idempotency_key,
                booking_date: self.booking_date,
                value_date: self.value_date,
                description: self.description,
                postings: self.postings,
                kind: self.kind,
                provenance: self.provenance,
                document: self.document,
                reverses: self.reverses,
                original_booking_date: self.original_booking_date,
                _state: PhantomData,
            })
        } else {
            Err(ValidationErrors(errors))
        }
    }
}

impl<S: sealed::State, const P: u8> Entry<S, P> {
    /// The content hash of these bytes, whatever state the entry is in.
    ///
    /// Deliberately not public. The hash of a *draft* proves only that the bytes
    /// are what they are; publishing it would hand a caller the one input
    /// [`Entry::adopt_verified`] needs, turning a check that the bytes already
    /// passed validation into a check that they hash to their own hash.
    ///
    /// Inside the crate it has one honest use: deciding idempotency before
    /// validation runs, so a safe retry cannot trip a rule the original
    /// submission already passed.
    pub(crate) fn digest(&self) -> Hash {
        tagged(tag::ENTRY_V1, &self.to_canonical_bytes())
    }

    /// The entry's identifier.
    #[must_use]
    pub fn id(&self) -> EntryId {
        self.id
    }

    /// The idempotency key.
    #[must_use]
    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    /// The booking date.
    #[must_use]
    pub fn booking_date(&self) -> Date {
        self.booking_date
    }

    /// The value date.
    #[must_use]
    pub fn value_date(&self) -> Date {
        self.value_date
    }

    /// The description.
    #[must_use]
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// The postings.
    #[must_use]
    pub fn postings(&self) -> &[Posting<P>] {
        &self.postings
    }

    /// The caller-defined kind, if one was set.
    #[must_use]
    pub fn kind(&self) -> Option<&Label> {
        self.kind.as_ref()
    }

    /// Who recorded the entry.
    #[must_use]
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// The source document, if one was attached.
    #[must_use]
    pub fn document(&self) -> Option<&DocumentRef> {
        self.document.as_ref()
    }

    /// The entry this one reverses, if any.
    #[must_use]
    pub fn reverses(&self) -> Option<EntryId> {
        self.reverses
    }

    /// The booking date of the entry being reversed.
    ///
    /// Set when a correction to a sealed period is booked in a later open
    /// period; the original date is retained so the correction can still be
    /// attributed to the period it economically belongs to.
    #[must_use]
    pub fn original_booking_date(&self) -> Option<Date> {
        self.original_booking_date
    }
}

impl<const P: u8> Entry<Balanced, P> {
    /// The entry's content hash.
    ///
    /// Covers everything semantic, including provenance, dimensions, and the
    /// document reference. The identifier is deliberately excluded: it is
    /// storage metadata, and two submissions of the same logical transaction
    /// must hash identically for idempotency to be decidable.
    ///
    /// The consequence is worth being explicit about, since it bounds what
    /// [`Entry::adopt_verified`] establishes: the hash binds an entry's
    /// *contents*, not which row they were read out of. Two entries cannot
    /// nonetheless share one — the idempotency key is inside the preimage and
    /// unique across the ledger — so the hash still identifies an entry in
    /// practice. It is the key, not the identifier, that makes it so.
    #[must_use]
    pub fn content_hash(&self) -> Hash {
        self.digest()
    }

    /// Gross debit and credit totals per currency.
    ///
    /// Every currency present is balanced; this reports the volume that moved.
    pub fn totals(&self) -> Result<BTreeMap<Currency, Balance<P>>, MoneyError> {
        let mut out: BTreeMap<Currency, Balance<P>> = BTreeMap::new();
        for posting in &self.postings {
            out.entry(posting.currency)
                .or_default()
                .add(posting.direction, posting.amount)?;
        }
        Ok(out)
    }

    /// Builds a draft that reverses this entry.
    ///
    /// Every posting's side is flipped and its magnitude preserved, so no
    /// arithmetic is performed and no overflow is possible. The reversal must
    /// still be sealed, which is where the target period is checked: a
    /// correction to a sealed period books into a later open one, never into
    /// the sealed period itself.
    ///
    /// Provenance and kind are deliberately **not** inherited. A correction is a
    /// new act by whoever made it, and it is not an invoice merely because the
    /// entry it reverses was one. Set both on the returned draft.
    #[must_use]
    pub fn reverse(&self, id: EntryId, key: IdempotencyKey, booking_date: Date) -> Entry<Draft, P> {
        let mut draft = Entry::<Draft, P>::new(id, key, booking_date)
            .with_description(self.description.clone())
            .reversing(self.id, self.booking_date);
        for posting in &self.postings {
            draft = draft.post(posting.inverted());
        }
        draft
    }
}

/// Storage returned an entry whose bytes do not match the hash recorded with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stored entry does not match its recorded content hash")]
pub struct IntegrityError {
    /// The hash the store recorded when the entry was written.
    pub expected: Hash,
    /// The hash of what the store just returned.
    pub actual: Hash,
}

/// Wire form of an entry.
///
/// Serialising borrows; deserialising owns. Both use the same field set so the
/// representation round-trips exactly.
#[cfg(feature = "serde")]
#[derive(serde::Serialize)]
struct EntryRef<'a, const P: u8> {
    id: &'a EntryId,
    idempotency_key: &'a IdempotencyKey,
    booking_date: &'a Date,
    value_date: &'a Date,
    description: &'a Description,
    postings: &'a [Posting<P>],
    kind: &'a Option<Label>,
    provenance: &'a Provenance,
    document: &'a Option<DocumentRef>,
    reverses: &'a Option<EntryId>,
    original_booking_date: &'a Option<Date>,
}

#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct EntryOwned<const P: u8> {
    id: EntryId,
    idempotency_key: IdempotencyKey,
    booking_date: Date,
    value_date: Date,
    description: Description,
    postings: Vec<Posting<P>>,
    kind: Option<Label>,
    provenance: Provenance,
    document: Option<DocumentRef>,
    reverses: Option<EntryId>,
    original_booking_date: Option<Date>,
}

#[cfg(feature = "serde")]
impl<S: sealed::State, const P: u8> serde::Serialize for Entry<S, P> {
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        EntryRef::<P> {
            id: &self.id,
            idempotency_key: &self.idempotency_key,
            booking_date: &self.booking_date,
            value_date: &self.value_date,
            description: &self.description,
            postings: &self.postings,
            kind: &self.kind,
            provenance: &self.provenance,
            document: &self.document,
            reverses: &self.reverses,
            original_booking_date: &self.original_booking_date,
        }
        .serialize(serializer)
    }
}

/// Deserialisation always produces a [`Draft`], never a [`Balanced`] entry.
///
/// The balanced type is a witness that validation ran. A witness that can be
/// read off a wire is not a witness — a peer could assert it for postings that
/// do not balance, against accounts that do not exist, in a period that is
/// closed. Received entries are therefore drafts, and the receiver re-runs
/// [`Entry::seal`] against its own accounts, calendar, and policy.
///
/// Round-tripping is lossless: sealing a deserialised draft that was valid
/// reproduces the same content hash.
#[cfg(feature = "serde")]
impl<'de, const P: u8> serde::Deserialize<'de> for Entry<Draft, P> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = EntryOwned::<P>::deserialize(d)?;
        Ok(Self {
            id: raw.id,
            idempotency_key: raw.idempotency_key,
            booking_date: raw.booking_date,
            value_date: raw.value_date,
            description: raw.description,
            postings: SmallVec::from_vec(raw.postings),
            kind: raw.kind,
            provenance: raw.provenance,
            document: raw.document,
            reverses: raw.reverses,
            original_booking_date: raw.original_booking_date,
            _state: PhantomData,
        })
    }
}

/// The encoding does not depend on the type state: a draft and the balanced
/// entry it seals into are the same bytes. Only the *use* of those bytes
/// differs, which is why [`Entry::content_hash`] is offered on the balanced form
/// alone.
impl<S: sealed::State, const P: u8> Canonical for Entry<S, P> {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.u8(P);
        w.bytes(self.idempotency_key.as_bytes());
        encode_date(w, self.booking_date);
        encode_date(w, self.value_date);
        w.str(self.description.as_str());
        w.seq(self.postings.iter(), |w, p| p.encode(w));
        w.option(self.kind.as_ref(), |w, v| v.encode(w));
        self.provenance.encode(w);
        w.option(self.document.as_ref(), |w, v| v.encode(w));
        w.option(self.reverses.as_ref(), |w, id| {
            w.fixed(id.as_uuid().as_bytes());
        });
        w.option(self.original_booking_date.as_ref(), |w, d| {
            encode_date(w, *d);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, AccountPath};
    use crate::dimensions::Dimensions;
    use crate::period::{Period, PeriodId};
    use crate::posting::Layer;
    use time::macros::date;

    type Eur = Amount<2>;

    struct Fixture {
        accounts: AccountRegistry,
        calendar: PeriodCalendar,
        policy: LedgerPolicy,
        cash: AccountId,
        revenue: AccountId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut accounts = AccountRegistry::new();
            let cash = accounts
                .register_path("Assets:Cash", date!(2026 - 01 - 01))
                .expect("registers");
            let revenue = accounts
                .register_path("Income:Sales", date!(2026 - 01 - 01))
                .expect("registers");
            Self {
                accounts,
                calendar: PeriodCalendar::new(),
                policy: LedgerPolicy::default(),
                cash,
                revenue,
            }
        }

        fn ctx(&self) -> SealContext<'_> {
            SealContext {
                accounts: &self.accounts,
                calendar: &self.calendar,
                policy: &self.policy,
            }
        }

        fn draft(&self) -> Entry<Draft, 2> {
            Entry::new(
                EntryId::generate(),
                IdempotencyKey::new(b"key-1".to_vec()).expect("valid"),
                date!(2026 - 03 - 15),
            )
        }

        fn balanced(&self) -> Entry<Balanced, 2> {
            self.draft()
                .debit(self.cash, Eur::from_minor(1000), Currency::EUR)
                .credit(self.revenue, Eur::from_minor(1000), Currency::EUR)
                .seal(&self.ctx())
                .expect("balances")
        }
    }

    #[test]
    fn a_balanced_entry_seals() {
        let f = Fixture::new();
        let entry = f.balanced();
        assert_eq!(entry.postings().len(), 2);
        let totals = entry.totals().expect("no overflow");
        assert!(totals[&Currency::EUR].is_balanced());
    }

    #[test]
    fn an_unbalanced_entry_is_rejected() {
        let f = Fixture::new();
        let err = f
            .draft()
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(999), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("must not balance");
        assert!(err.any(|e| matches!(e, ValidationError::Unbalanced { .. })));
    }

    #[test]
    fn a_single_posting_is_not_double_entry() {
        let f = Fixture::new();
        let err = f
            .draft()
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("too few postings");
        assert!(err.any(|e| matches!(e, ValidationError::TooFewPostings { count: 1 })));
    }

    #[test]
    fn zero_and_negative_amounts_are_rejected() {
        let f = Fixture::new();
        let err = f
            .draft()
            .debit(f.cash, Eur::ZERO, Currency::EUR)
            .credit(f.revenue, Eur::from_minor(-5), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("invalid amounts");
        assert!(err.any(|e| matches!(e, ValidationError::ZeroAmount { index: 0 })));
        assert!(err.any(|e| matches!(e, ValidationError::NegativeAmount { index: 1 })));
    }

    #[test]
    fn all_violations_are_reported_at_once() {
        let f = Fixture::new();
        let ghost = AccountId::from_index(999);
        let err = f
            .draft()
            .debit(ghost, Eur::ZERO, Currency::EUR)
            .credit(f.revenue, Eur::from_minor(500), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("multiple problems");

        // A caller repairing a batch should see every problem in one pass.
        assert!(err.any(|e| matches!(e, ValidationError::ZeroAmount { index: 0 })));
        assert!(err.any(|e| matches!(e, ValidationError::UnknownAccount { index: 0, .. })));
        assert!(err.any(|e| matches!(e, ValidationError::Unbalanced { .. })));
        assert_eq!(err.len(), 3, "expected exactly three violations, got {err}");
    }

    #[test]
    fn an_entry_of_only_zero_postings_is_not_reported_as_unbalanced() {
        // Zero on both sides balances; the real defects are the zero amounts.
        let f = Fixture::new();
        let err = f
            .draft()
            .debit(f.cash, Eur::ZERO, Currency::EUR)
            .credit(f.revenue, Eur::ZERO, Currency::EUR)
            .seal(&f.ctx())
            .expect_err("zero amounts");
        assert!(!err.any(|e| matches!(e, ValidationError::Unbalanced { .. })));
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn unknown_accounts_are_rejected() {
        let f = Fixture::new();
        let ghost = AccountId::from_index(999);
        let err = f
            .draft()
            .debit(ghost, Eur::from_minor(10), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(10), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("unknown account");
        assert!(err.any(|e| matches!(e, ValidationError::UnknownAccount { index: 0, .. })));
    }

    #[test]
    fn posting_to_an_aggregation_node_is_rejected() {
        let mut f = Fixture::new();
        let parent = f
            .accounts
            .register_path("Assets", date!(2026 - 01 - 01))
            .expect("registers");
        let err = f
            .draft()
            .debit(parent, Eur::from_minor(10), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(10), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("non-leaf account");
        assert!(err.any(|e| matches!(e, ValidationError::NonLeafAccount { index: 0, .. })));
    }

    #[test]
    fn postings_outside_the_account_window_are_rejected() {
        let mut f = Fixture::new();
        let closed = f
            .accounts
            .register(
                Account::new(
                    AccountPath::parse("Assets:Old").expect("valid"),
                    date!(2026 - 01 - 01),
                )
                .closing_on(date!(2026 - 02 - 01)),
            )
            .expect("registers");
        let err = f
            .draft()
            .debit(closed, Eur::from_minor(10), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(10), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("account closed");
        assert!(err.any(|e| matches!(e, ValidationError::AccountNotOpen { index: 0, .. })));
    }

    #[test]
    fn sealed_periods_reject_postings() {
        let mut f = Fixture::new();
        let id = PeriodId::new("2026-03").expect("valid");
        f.calendar
            .define(
                Period::new(id.clone(), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
                    .expect("valid range"),
            )
            .expect("defines");
        f.calendar
            .transition(&id, PeriodState::Closing)
            .expect("ok");
        f.calendar.transition(&id, PeriodState::Sealed).expect("ok");

        let err = f
            .draft()
            .debit(f.cash, Eur::from_minor(10), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(10), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("period sealed");
        assert!(err.any(|e| matches!(
            e,
            ValidationError::ClosedPeriod {
                state: PeriodState::Sealed,
                ..
            }
        )));
    }

    #[test]
    fn each_currency_balances_independently() {
        let mut f = Fixture::new();
        let fx = f
            .accounts
            .register_path("Assets:FX", date!(2026 - 01 - 01))
            .expect("registers");

        // EUR balances, USD balances: a legitimate cross-currency booking.
        let entry = f
            .draft()
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .debit(fx, Eur::from_minor(500), Currency::USD)
            .credit(f.revenue, Eur::from_minor(500), Currency::USD)
            .seal(&f.ctx());
        assert!(entry.is_ok(), "per-currency balance should succeed");
    }

    #[test]
    fn a_currency_that_does_not_balance_is_named() {
        let mut f = Fixture::new();
        let fx = f
            .accounts
            .register_path("Assets:FX", date!(2026 - 01 - 01))
            .expect("registers");
        let err = f
            .draft()
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .debit(fx, Eur::from_minor(500), Currency::USD)
            .seal(&f.ctx())
            .expect_err("USD is unbalanced");
        assert!(err.any(|e| matches!(
            e,
            ValidationError::Unbalanced {
                currency: Currency::USD,
                ..
            }
        )));
    }

    #[test]
    fn single_currency_policy_rejects_others() {
        let mut f = Fixture::new();
        f.policy.currency = CurrencyPolicy::Single(Currency::EUR);
        let err = f
            .draft()
            .debit(f.cash, Eur::from_minor(10), Currency::USD)
            .credit(f.revenue, Eur::from_minor(10), Currency::USD)
            .seal(&f.ctx())
            .expect_err("USD not permitted");
        assert!(err.any(|e| matches!(
            e,
            ValidationError::CurrencyNotAllowed {
                currency: Currency::USD
            }
        )));
    }

    #[test]
    fn dimensions_can_be_required_on_every_posting() {
        let mut f = Fixture::new();
        let activity = Label::new("activity").expect("valid");
        f.policy = f.policy.clone().requiring(activity.clone());

        let err = f
            .draft()
            .debit(f.cash, Eur::from_minor(10), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(10), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("activity missing");
        assert_eq!(
            err.errors()
                .iter()
                .filter(|e| matches!(e, ValidationError::MissingDimension { .. }))
                .count(),
            2
        );

        let dims = Dimensions::none()
            .with(activity, Label::new("Network").expect("valid"))
            .expect("fits");
        let ok = f
            .draft()
            .post(
                Posting::debit(f.cash, Eur::from_minor(10), Currency::EUR)
                    .with_dimensions(dims.clone()),
            )
            .post(
                Posting::credit(f.revenue, Eur::from_minor(10), Currency::EUR)
                    .with_dimensions(dims),
            )
            .seal(&f.ctx());
        assert!(ok.is_ok());
    }

    #[test]
    fn a_posting_carrying_the_wrong_axis_does_not_satisfy_the_policy() {
        let mut f = Fixture::new();
        f.policy = f
            .policy
            .clone()
            .requiring(Label::new("activity").expect("valid"));

        let dims = Dimensions::none()
            .with(
                Label::new("segment").expect("valid"),
                Label::new("Electricity").expect("valid"),
            )
            .expect("fits");
        let err = f
            .draft()
            .post(
                Posting::debit(f.cash, Eur::from_minor(10), Currency::EUR)
                    .with_dimensions(dims.clone()),
            )
            .post(
                Posting::credit(f.revenue, Eur::from_minor(10), Currency::EUR)
                    .with_dimensions(dims),
            )
            .seal(&f.ctx())
            .expect_err("wrong axis");
        assert!(err.any(|e| matches!(e, ValidationError::MissingDimension { .. })));
    }

    #[test]
    fn value_date_drift_can_be_bounded() {
        let mut f = Fixture::new();
        f.policy.max_value_date_drift_days = Some(5);
        let err = f
            .draft()
            .with_value_date(date!(2026 - 04 - 15))
            .debit(f.cash, Eur::from_minor(10), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(10), Currency::EUR)
            .seal(&f.ctx())
            .expect_err("drift too large");
        assert!(err.any(|e| matches!(e, ValidationError::ValueDateDrift { .. })));
    }

    #[test]
    fn content_hash_is_stable_across_identical_entries() {
        let f = Fixture::new();
        let a = f.balanced();
        let b = f.balanced();
        // Different identifiers, same logical transaction.
        assert_ne!(a.id(), b.id());
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn content_hash_changes_with_any_semantic_field() {
        let f = Fixture::new();
        let base = f.balanced();

        let different_amount = f
            .draft()
            .debit(f.cash, Eur::from_minor(1001), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1001), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        assert_ne!(base.content_hash(), different_amount.content_hash());

        let different_description = f
            .draft()
            .with_description(Description::new("something else").expect("valid"))
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        assert_ne!(base.content_hash(), different_description.content_hash());

        let different_provenance = f
            .draft()
            .with_provenance(Provenance::none().with_actor("auditor").expect("valid"))
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        assert_ne!(base.content_hash(), different_provenance.content_hash());
    }

    #[test]
    fn swapping_debit_and_credit_changes_the_hash() {
        let f = Fixture::new();
        let a = f.balanced();
        let b = f
            .draft()
            .credit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .debit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn reversal_flips_every_side_and_keeps_the_reference() {
        let f = Fixture::new();
        let original = f.balanced();
        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"reversal-1".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("reversal balances");

        assert_eq!(reversal.reverses(), Some(original.id()));
        assert_eq!(
            reversal.original_booking_date(),
            Some(original.booking_date())
        );
        assert_eq!(reversal.booking_date(), date!(2026 - 04 - 01));

        for (orig, rev) in original.postings().iter().zip(reversal.postings()) {
            assert_eq!(rev.direction, orig.direction.inverse());
            assert_eq!(rev.amount, orig.amount);
            assert_eq!(rev.account, orig.account);
        }
    }

    #[test]
    fn reversal_of_a_sealed_period_books_into_an_open_one() {
        let mut f = Fixture::new();
        let original = f.balanced();

        let march = PeriodId::new("2026-03").expect("valid");
        f.calendar
            .define(
                Period::new(march.clone(), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
                    .expect("valid range"),
            )
            .expect("defines");
        f.calendar
            .transition(&march, PeriodState::Closing)
            .expect("ok");
        f.calendar
            .transition(&march, PeriodState::Sealed)
            .expect("ok");

        // Booking the reversal back into March is refused.
        let refused = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"r1".to_vec()).expect("valid"),
                date!(2026 - 03 - 20),
            )
            .seal(&f.ctx());
        assert!(refused.is_err());

        // Booking it into April succeeds and retains the original date.
        let accepted = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"r2".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("books into an open period");
        assert_eq!(
            accepted.original_booking_date(),
            Some(date!(2026 - 03 - 15))
        );
    }

    #[test]
    fn a_reversal_nets_the_original_to_zero() {
        let f = Fixture::new();
        let original = f.balanced();
        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"r".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("balances");

        let mut tb = crate::balance::TrialBalance::<2>::new();
        for p in original.postings().iter().chain(reversal.postings()) {
            tb.apply(p).expect("no overflow");
        }
        for (_, balance) in tb.iter() {
            assert!(balance.is_balanced(), "every account must net to zero");
        }
    }

    #[test]
    fn a_rehydrated_entry_must_match_its_recorded_hash() {
        let f = Fixture::new();
        let original = f.balanced();
        let hash = original.content_hash();

        // Reconstructing the same bytes is accepted.
        let rebuilt = f
            .draft()
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .adopt_verified(hash)
            .expect("matches");
        assert_eq!(rebuilt.content_hash(), hash);

        // A row altered underneath the engine is refused, even though the
        // altered entry balances perfectly well on its own.
        let tampered = f
            .draft()
            .debit(f.cash, Eur::from_minor(1001), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1001), Currency::EUR)
            .adopt_verified(hash)
            .expect_err("must not be adopted");
        assert_eq!(tampered.expected, hash);
        assert_ne!(tampered.actual, hash);
    }

    #[test]
    fn adoption_does_not_consult_the_current_ledger_state() {
        // An entry stays readable after its account closes: validation is about
        // what may be written next, not about what was written.
        let mut f = Fixture::new();
        let original = f.balanced();
        let hash = original.content_hash();

        f.accounts
            .close(f.cash, date!(2026 - 01 - 31))
            .expect("closes");

        // Sealing now fails …
        assert!(
            f.draft()
                .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
                .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
                .seal(&f.ctx())
                .is_err()
        );
        // … but the historical entry still rehydrates.
        assert!(
            f.draft()
                .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
                .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
                .adopt_verified(hash)
                .is_ok()
        );
    }

    #[test]
    fn the_posting_count_is_bounded_by_what_a_position_can_address() {
        // A posting is addressed by a `u16` in `PostingRef` and a `SMALLINT` in
        // the reference schema. Beyond `MAX_POSTINGS` those would truncate, and
        // a truncated position is a clearing or a statement line silently
        // pointing at the wrong movement — so validation refuses the entry.
        let f = Fixture::new();
        let mut draft = f.draft();
        for _ in 0..=MAX_POSTINGS {
            draft = draft.debit(f.cash, Eur::from_minor(1), Currency::EUR);
        }
        // Balance it, so `TooManyPostings` is the only thing left to report.
        draft = draft.credit(
            f.revenue,
            Eur::from_minor(MAX_POSTINGS as i64 + 1),
            Currency::EUR,
        );

        let err = draft.seal(&f.ctx()).expect_err("too many postings");
        assert!(err.any(|e| matches!(e, ValidationError::TooManyPostings { .. })));
    }

    #[test]
    fn an_entry_at_the_posting_bound_is_accepted() {
        let f = Fixture::new();
        let mut draft = f.draft();
        for _ in 0..MAX_POSTINGS - 1 {
            draft = draft.debit(f.cash, Eur::from_minor(1), Currency::EUR);
        }
        draft = draft.credit(
            f.revenue,
            Eur::from_minor(MAX_POSTINGS as i64 - 1),
            Currency::EUR,
        );
        assert_eq!(
            draft.seal(&f.ctx()).expect("at the bound").postings().len(),
            MAX_POSTINGS
        );
    }

    #[test]
    fn field_bounds_are_enforced() {
        assert_eq!(
            IdempotencyKey::new(Vec::new()),
            Err(EntryFieldError::EmptyIdempotencyKey)
        );
        assert_eq!(
            IdempotencyKey::new(vec![0u8; MAX_IDEMPOTENCY_KEY_LEN + 1]),
            Err(EntryFieldError::IdempotencyKeyTooLong)
        );
        assert_eq!(
            Description::new("x".repeat(MAX_DESCRIPTION_LEN + 1)),
            Err(EntryFieldError::DescriptionTooLong)
        );
        assert_eq!(
            Description::new("bad\u{7}"),
            Err(EntryFieldError::DescriptionControlCharacter)
        );
        assert!(Description::new("multi\nline").is_ok());
    }

    #[test]
    fn document_reference_is_covered_by_the_hash() {
        let f = Fixture::new();
        let base = f.balanced();
        let with_doc = f
            .draft()
            .with_document(
                DocumentRef::new("INV-2026-0001", crate::hash::tagged(b"doc", b"content"))
                    .expect("valid"),
            )
            .debit(f.cash, Eur::from_minor(1000), Currency::EUR)
            .credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
            .seal(&f.ctx())
            .expect("balances");
        assert_ne!(base.content_hash(), with_doc.content_hash());
    }

    #[test]
    fn pending_postings_are_distinguished_from_settled() {
        let f = Fixture::new();
        let settled = f.balanced();
        let pending = f
            .draft()
            .post(
                Posting::debit(f.cash, Eur::from_minor(1000), Currency::EUR)
                    .in_layer(Layer::Pending),
            )
            .post(
                Posting::credit(f.revenue, Eur::from_minor(1000), Currency::EUR)
                    .in_layer(Layer::Pending),
            )
            .seal(&f.ctx())
            .expect("balances");
        assert_ne!(settled.content_hash(), pending.content_hash());
    }
}
