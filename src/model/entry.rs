//! What a caller submits: the [`Entry`] draft and the values an entry carries.
//!
//! # Personal data
//!
//! The memo is the **only** free-text field of an entry. It is committed as a
//! salted hash ([`MemoCommitment`]); its text and salt live beside the leaf and
//! can be erased (GDPR Art. 17) without changing any leaf byte, hash or proof.
//! Every other field — key, kind, account paths, dimensions, document
//! references — is committed in clear and **must not carry personal data**.

use core::fmt;

use time::Date;

use super::{
    AccountHandle, AccountPath, Amount, Currency, Decode, DecodeError, Dimensions, Encode, EntryId,
    EntryParts, IdempotencyKey, Label, LedgerId, PostingRecord, Reader, Sink, ValueError, Writer,
    hex,
};
use crate::log::hash::{TAG_FINGERPRINT, TAG_MEMO, tagged};

/// The side of a posting. Magnitudes are positive; the side is never a sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Side {
    /// Debit.
    Debit,
    /// Credit.
    Credit,
}

impl fmt::Display for Side {
    /// `debit` or `credit`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        super::encoding::pad(
            f,
            match self {
                Self::Debit => "debit",
                Self::Credit => "credit",
            },
        )
    }
}

impl fmt::Display for Layer {
    /// `settled` or `pending`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        super::encoding::pad(
            f,
            match self {
                Self::Settled => "settled",
                Self::Pending => "pending",
            },
        )
    }
}

impl Side {
    /// The other side.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Debit => Self::Credit,
            Self::Credit => Self::Debit,
        }
    }
}

/// The balancing layer beside the currency: settled movements, or pending
/// ones.
///
/// Only holds write the Pending layer: [`Entry::open_hold`] reserves an
/// amount with two Pending postings, and its one resolution
/// ([`Entry::post_hold`], [`Entry::void_hold`], [`Entry::expire_hold`])
/// releases them with Pending postings the rule core derives (plus Settled
/// ones for a post). An account's Pending balance is therefore exactly what
/// its open holds reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Layer {
    /// Settled: every booking.
    Settled,
    /// Pending: what open holds reserve.
    Pending,
}

macro_rules! byte_enum {
    ($ty:ident, $field:literal { $($variant:ident = $byte:literal),* $(,)? }) => {
        impl Encode for $ty {
            fn encode<S: Sink>(&self, w: &mut Writer<S>) {
                w.u8(match self { $(Self::$variant => $byte),* });
            }
        }

        impl Decode for $ty {
            fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
                match r.u8()? {
                    $($byte => Ok(Self::$variant),)*
                    tag => Err(DecodeError::InvalidTag { field: $field, tag }),
                }
            }
        }
    };
}
pub(crate) use byte_enum;

byte_enum!(Side, "side" { Debit = 0, Credit = 1 });
byte_enum!(Layer, "layer" { Settled = 0, Pending = 1 });

/// When the caller says an entry was captured: Unix seconds and the UTC
/// offset in minutes — a caller's claim, never read from a clock.
///
/// ```
/// use doubleentry::CaptureTime;
///
/// assert!(CaptureTime::new(1_773_568_800, 60).is_ok());
/// assert!(CaptureTime::new(1_773_568_800, 1081).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaptureTime {
    unix_seconds: i64,
    utc_offset_minutes: i16,
}

impl CaptureTime {
    const SECONDS: core::ops::RangeInclusive<i64> = -62_135_596_800..=253_402_300_799;
    const OFFSET: core::ops::RangeInclusive<i16> = -1080..=1080;

    /// A capture time within the years 0001–9999 with an offset within ±18 h.
    ///
    /// # Errors
    ///
    /// [`ValueError::OutOfRange`] outside those bounds.
    pub fn new(unix_seconds: i64, utc_offset_minutes: i16) -> Result<Self, ValueError> {
        if Self::SECONDS.contains(&unix_seconds) && Self::OFFSET.contains(&utc_offset_minutes) {
            Ok(Self {
                unix_seconds,
                utc_offset_minutes,
            })
        } else {
            Err(ValueError::OutOfRange)
        }
    }

    /// Seconds since 1970-01-01T00:00:00Z.
    #[must_use]
    pub const fn unix_seconds(self) -> i64 {
        self.unix_seconds
    }

    /// The local offset from UTC, in minutes.
    #[must_use]
    pub const fn utc_offset_minutes(self) -> i16 {
        self.utc_offset_minutes
    }
}

impl Encode for CaptureTime {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.i64(self.unix_seconds);
        w.i16(self.utc_offset_minutes);
    }
}

impl Decode for CaptureTime {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        let seconds = r.i64()?;
        let offset = r.i16()?;
        Self::new(seconds, offset).map_err(|error| DecodeError::Value { field, error })
    }
}

/// A source document an entry is based on (an invoice, a receipt, a bank
/// statement line).
///
/// ```
/// use doubleentry::Document;
/// use time::macros::date;
///
/// let invoice = Document::new("invoice", "RE-2026-0001")?.dated(date!(2026 - 03 - 15));
/// assert_eq!(invoice.reference(), "RE-2026-0001");
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Document {
    doc_type: Label,
    reference: String,
    date: Option<Date>,
    sha256: Option<[u8; 32]>,
}

impl Document {
    /// The most bytes in a reference.
    pub const MAX_REFERENCE_BYTES: usize = 256;

    /// A document of a type, by its reference.
    ///
    /// # Errors
    ///
    /// The type's [`Label`] error; for the reference [`ValueError::Empty`],
    /// [`ValueError::TooLong`] above 256 bytes or [`ValueError::ControlCharacter`].
    pub fn new(
        doc_type: impl TryInto<Label, Error: Into<ValueError>>,
        reference: impl Into<String>,
    ) -> Result<Self, ValueError> {
        let doc_type = doc_type.try_into().map_err(Into::into)?;
        let reference = reference.into();
        if reference.is_empty() {
            return Err(ValueError::Empty);
        }
        if reference.len() > Self::MAX_REFERENCE_BYTES {
            return Err(ValueError::TooLong {
                max: Self::MAX_REFERENCE_BYTES,
                found: reference.len(),
            });
        }
        if reference.chars().any(char::is_control) {
            return Err(ValueError::ControlCharacter);
        }
        Ok(Self {
            doc_type,
            reference,
            date: None,
            sha256: None,
        })
    }

    /// With the document's date.
    #[must_use]
    pub fn dated(self, date: Date) -> Self {
        Self {
            date: Some(date),
            ..self
        }
    }

    /// With the SHA-256 of the document's bytes.
    #[must_use]
    pub fn with_sha256(self, digest: [u8; 32]) -> Self {
        Self {
            sha256: Some(digest),
            ..self
        }
    }

    /// The document type.
    #[must_use]
    pub fn doc_type(&self) -> &Label {
        &self.doc_type
    }

    /// The reference.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// The document's date.
    #[must_use]
    pub fn date(&self) -> Option<Date> {
        self.date
    }

    /// The SHA-256 of the document's bytes.
    #[must_use]
    pub fn sha256(&self) -> Option<&[u8; 32]> {
        self.sha256.as_ref()
    }
}

impl Encode for Document {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.doc_type);
        w.str(&self.reference);
        w.opt(self.date.as_ref());
        w.opt(self.sha256.as_ref());
    }
}

impl Decode for Document {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        let doc_type: Label = r.value(field)?;
        let reference = r.str(field)?;
        let date = r.opt(field, Reader::date)?;
        let sha256 = r.opt(field, Reader::array)?;
        let document =
            Self::new(doc_type, reference).map_err(|error| DecodeError::Value { field, error })?;
        Ok(Self {
            date,
            sha256,
            ..document
        })
    }
}

/// An exchange rate recorded on an entry: `mantissa × 10^exponent` units of
/// `quote` per unit of `base`. Data only — the crate converts nothing.
///
/// The rate is normalised at construction (the mantissa is not a multiple of
/// ten), so equal rates have one form.
///
/// ```
/// use doubleentry::{Rate, currency::{EUR, USD}};
///
/// let rate = Rate::parse(EUR, USD, "1.0873")?;
/// assert_eq!((rate.mantissa(), rate.exponent()), (10873, -4));
/// assert_eq!(Rate::new(EUR, USD, 108_730, -5)?, rate);
/// assert_eq!(rate.to_string(), "1.0873");
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rate {
    base: Currency,
    quote: Currency,
    mantissa: i128,
    exponent: i8,
}

impl Rate {
    /// A rate, normalised.
    ///
    /// # Errors
    ///
    /// [`ValueError::SameCurrency`], [`ValueError::NotPositive`] for a
    /// mantissa ≤ 0, or [`ValueError::OutOfRange`] when normalising pushes
    /// the exponent above `i8::MAX`.
    pub fn new(
        base: Currency,
        quote: Currency,
        mantissa: i128,
        exponent: i8,
    ) -> Result<Self, ValueError> {
        if base == quote {
            return Err(ValueError::SameCurrency);
        }
        if mantissa <= 0 {
            return Err(ValueError::NotPositive);
        }
        let (mut mantissa, mut exponent) = (mantissa, exponent);
        while mantissa.checked_rem(10) == Some(0) {
            mantissa = mantissa.checked_div(10).ok_or(ValueError::OutOfRange)?;
            exponent = exponent.checked_add(1).ok_or(ValueError::OutOfRange)?;
        }
        Ok(Self {
            base,
            quote,
            mantissa,
            exponent,
        })
    }

    /// A rate from decimal text: digits with at most one `.`, no sign.
    ///
    /// # Errors
    ///
    /// [`ValueError::Empty`], [`ValueError::InvalidCharacter`],
    /// [`ValueError::NotPositive`] for zero, [`ValueError::OutOfRange`] when
    /// the significant digits exceed `i128` or the exponent exceeds `i8`, and
    /// the errors of [`new`](Self::new).
    pub fn parse(base: Currency, quote: Currency, decimal: &str) -> Result<Self, ValueError> {
        let (integer, fraction) = decimal.split_once('.').unwrap_or((decimal, ""));
        if integer.is_empty() || (decimal.contains('.') && fraction.is_empty()) {
            return Err(ValueError::Empty);
        }
        if let Some(found) = integer
            .chars()
            .chain(fraction.chars())
            .find(|c| !c.is_ascii_digit())
        {
            return Err(ValueError::InvalidCharacter { found });
        }
        let digits = format!("{integer}{fraction}");
        let significant = digits.trim_end_matches('0');
        let trailing = digits.len().saturating_sub(significant.len());
        let exponent = i64::try_from(trailing)
            .ok()
            .and_then(|trailing| trailing.checked_sub(i64::try_from(fraction.len()).ok()?))
            .and_then(|exponent| i8::try_from(exponent).ok())
            .ok_or(ValueError::OutOfRange)?;
        let significant = significant.trim_start_matches('0');
        if significant.is_empty() {
            return Err(ValueError::NotPositive);
        }
        let mantissa = significant
            .parse::<i128>()
            .map_err(|_| ValueError::OutOfRange)?;
        Self::new(base, quote, mantissa, exponent)
    }

    /// The currency being priced.
    #[must_use]
    pub const fn base(&self) -> Currency {
        self.base
    }

    /// The currency the price is in.
    #[must_use]
    pub const fn quote(&self) -> Currency {
        self.quote
    }

    /// The significant digits.
    #[must_use]
    pub const fn mantissa(&self) -> i128 {
        self.mantissa
    }

    /// The power of ten.
    #[must_use]
    pub const fn exponent(&self) -> i8 {
        self.exponent
    }
}

impl fmt::Display for Rate {
    /// The rate as plain decimal text (no exponent notation).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.mantissa.to_string();
        let shift = usize::from(self.exponent.unsigned_abs());
        let text = if self.exponent >= 0 {
            format!("{digits}{}", "0".repeat(shift))
        } else {
            let padded = format!(
                "{}{digits}",
                "0".repeat(shift.saturating_add(1).saturating_sub(digits.len()))
            );
            let (integer, fraction) = padded.split_at(padded.len().saturating_sub(shift));
            format!("{integer}.{fraction}")
        };
        super::encoding::pad(f, &text)
    }
}

impl Encode for Rate {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.base);
        w.value(&self.quote);
        w.i128(self.mantissa);
        w.i8(self.exponent);
    }
}

impl Decode for Rate {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        let base = r.value(field)?;
        let quote = r.value(field)?;
        let mantissa = r.i128()?;
        let exponent = r.i8()?;
        let rate = Self::new(base, quote, mantissa, exponent)
            .map_err(|error| DecodeError::Value { field, error })?;
        if rate.mantissa == mantissa {
            Ok(rate)
        } else {
            Err(DecodeError::NonCanonical { field })
        }
    }
}

/// How an entry acts on a hold: open one, or resolve an open one.
///
/// A hold reserves an amount on the Pending layer; its resolutions carry no
/// postings of their own — the rule core derives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum HoldAction<const P: u8 = 2> {
    /// Open a hold, optionally expiring on a date.
    Open {
        /// The first day the hold may be expired.
        expires: Option<Date>,
    },
    /// Settle a hold, fully or partly (the remainder is released).
    Post {
        /// The hold being settled.
        hold: EntryId,
        /// How much of it.
        amount: PostAmount<P>,
    },
    /// Release a hold.
    Void {
        /// The hold being released.
        hold: EntryId,
    },
    /// Release a hold that reached its expiry date.
    Expire {
        /// The hold being expired.
        hold: EntryId,
    },
}

/// How much of a hold a [`HoldAction::Post`] settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum PostAmount<const P: u8 = 2> {
    /// All of it.
    Full,
    /// Part of it.
    Partial(Amount<P>),
}

impl<const P: u8> Encode for HoldAction<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        match self {
            Self::Open { expires } => {
                w.u8(0);
                w.opt(expires.as_ref());
            }
            Self::Post { hold, amount } => {
                w.u8(1);
                w.value(hold);
                match amount {
                    PostAmount::Full => w.u8(0),
                    PostAmount::Partial(amount) => {
                        w.u8(1);
                        w.value(amount);
                    }
                }
            }
            Self::Void { hold } => {
                w.u8(2);
                w.value(hold);
            }
            Self::Expire { hold } => {
                w.u8(3);
                w.value(hold);
            }
        }
    }
}

impl<const P: u8> Decode for HoldAction<P> {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        match r.u8()? {
            0 => Ok(Self::Open {
                expires: r.opt(field, Reader::date)?,
            }),
            1 => {
                let hold = r.value(field)?;
                let amount = match r.u8()? {
                    0 => PostAmount::Full,
                    1 => PostAmount::Partial(r.value(field)?),
                    tag => {
                        return Err(DecodeError::InvalidTag {
                            field: "post amount",
                            tag,
                        });
                    }
                };
                Ok(Self::Post { hold, amount })
            }
            2 => Ok(Self::Void {
                hold: r.value(field)?,
            }),
            3 => Ok(Self::Expire {
                hold: r.value(field)?,
            }),
            tag => Err(DecodeError::InvalidTag { field, tag }),
        }
    }
}

/// The 16-byte salt of a memo commitment, kept beside the leaf with the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoSalt(pub [u8; 16]);

/// A memo's commitment: `SHA-256(u64 len(tag) ‖ "doubleentry/1/memo" ‖ salt ‖ str(text))`.
///
/// Only the commitment enters the leaf. Erasing the text and the salt
/// changes no leaf byte, and the salt keeps a low-entropy memo from being
/// guessed back from the commitment.
///
/// ```
/// use doubleentry::{MemoSalt, record::MemoCommitment};
///
/// let salt = MemoSalt([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
/// let commitment = MemoCommitment::compute(&salt, "Rechnung 4711");
/// assert_eq!(
///     commitment.to_string(),
///     "d1f4b79422abbf28c679a18d6487c3faafdd151b1f1fc0fc941d2dabd3e47dbc",
/// );
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoCommitment([u8; 32]);

impl MemoCommitment {
    /// Commits to a memo text under a salt.
    #[must_use]
    pub fn compute(salt: &MemoSalt, text: &str) -> Self {
        let mut payload = Writer::new(Vec::new());
        payload.fixed(&salt.0);
        payload.str(text);
        Self(*tagged(TAG_MEMO, &payload.into_inner()).as_bytes())
    }

    /// A commitment from its 32 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Whether a memo may not hold `c`: a control character other than tab, line
/// feed and carriage return.
fn is_memo_control(c: char) -> bool {
    c.is_control() && !matches!(c, '\t' | '\n' | '\r')
}

/// A tagged SHA-256 of a draft's content, which decides idempotency: a retry
/// under a used key whose fingerprint differs is a conflict.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// A fingerprint from its 32 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

macro_rules! hex_bytes {
    ($($ty:ident),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                hex::write(f, &self.0)
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($ty), "({})"), self)
            }
        }
    )*};
}

hex_bytes!(MemoCommitment, Fingerprint);

impl Encode for MemoCommitment {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.fixed(&self.0);
    }
}

impl Decode for MemoCommitment {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        r.array().map(Self)
    }
}

/// How a draft names an account: by path, or by handle.
///
/// A ledger compares a retry with accounts resolved to handles, so a retry
/// may name an account either way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AccountRef {
    /// By path; resolved by the rule core.
    Path(AccountPath),
    /// By handle.
    Handle(AccountHandle),
}

impl From<AccountPath> for AccountRef {
    fn from(path: AccountPath) -> Self {
        Self::Path(path)
    }
}

impl From<&AccountPath> for AccountRef {
    fn from(path: &AccountPath) -> Self {
        Self::Path(path.clone())
    }
}

impl From<AccountHandle> for AccountRef {
    fn from(handle: AccountHandle) -> Self {
        Self::Handle(handle)
    }
}

impl TryFrom<&str> for AccountRef {
    type Error = ValueError;

    fn try_from(path: &str) -> Result<Self, Self::Error> {
        AccountPath::parse(path).map(Self::Path)
    }
}

impl TryFrom<String> for AccountRef {
    type Error = ValueError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        AccountPath::parse(&path).map(Self::Path)
    }
}

/// One movement in a draft: an account, a side, a positive amount, a
/// currency and optional dimensions.
///
/// Builder arguments that fail validation do not panic and need no `?`:
/// the error is kept and reported by [`Entry::build_errors`] once the posting
/// is added to an entry. The layer is not a builder choice (see [`Layer`]).
///
/// ```
/// use doubleentry::{Entry, Posting, amount, currency::EUR};
/// use time::macros::date;
///
/// let draft: Entry = Entry::new("invoice-2026-0002", date!(2026 - 03 - 15))
///     .posting(Posting::debit("Expenses:Travel", amount!("80.00"), EUR).dimension("project", "alpha"))
///     .credit("Assets:Cash", amount!("80.00"), EUR);
/// assert!(draft.build_errors().is_empty());
/// ```
///
/// There is no public layer setter:
///
/// ```compile_fail
/// use doubleentry::{Posting, amount, currency::EUR};
/// let _ = Posting::<2>::debit("Assets:Cash", amount!("1.00"), EUR).pending();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Posting<const P: u8 = 2> {
    account: Result<AccountRef, ValueError>,
    side: Side,
    amount: Amount<P>,
    currency: Currency,
    layer: Layer,
    dimensions: Dimensions,
    dimension_errors: Vec<ValueError>,
}

impl<const P: u8> Posting<P> {
    fn new<A>(account: A, side: Side, amount: Amount<P>, currency: Currency) -> Self
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        Self {
            account: account.try_into().map_err(Into::into),
            side,
            amount,
            currency,
            layer: Layer::Settled,
            dimensions: Dimensions::new(),
            dimension_errors: Vec::new(),
        }
    }

    /// A debit posting.
    pub fn debit<A>(account: A, amount: Amount<P>, currency: Currency) -> Self
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        Self::new(account, Side::Debit, amount, currency)
    }

    /// A credit posting.
    pub fn credit<A>(account: A, amount: Amount<P>, currency: Currency) -> Self
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        Self::new(account, Side::Credit, amount, currency)
    }

    /// With a dimension value; an invalid label, a second value for an axis
    /// or a ninth axis is reported by [`Entry::build_errors`].
    #[must_use]
    pub fn dimension<X, Y>(mut self, axis: X, value: Y) -> Self
    where
        X: TryInto<Label>,
        X::Error: Into<ValueError>,
        Y: TryInto<Label>,
        Y::Error: Into<ValueError>,
    {
        let added = axis
            .try_into()
            .map_err(Into::into)
            .and_then(|axis| Ok((axis, value.try_into().map_err(Into::into)?)))
            .and_then(|(axis, value)| self.dimensions.clone().with(axis, value));
        match added {
            Ok(dimensions) => self.dimensions = dimensions,
            Err(error) => self.dimension_errors.push(error),
        }
        self
    }

    /// With a whole set of dimensions, replacing any added before — and the
    /// errors of those additions.
    #[must_use]
    pub fn dimensions(self, dimensions: Dimensions) -> Self {
        Self {
            dimensions,
            dimension_errors: Vec::new(),
            ..self
        }
    }

    /// The same posting on the Pending layer, for a hold's postings.
    #[must_use]
    pub(crate) fn pending(self) -> Self {
        Self {
            layer: Layer::Pending,
            ..self
        }
    }

    /// The account; `None` when the account argument was invalid.
    #[must_use]
    pub fn account(&self) -> Option<&AccountRef> {
        self.account.as_ref().ok()
    }

    /// The side.
    #[must_use]
    pub const fn side(&self) -> Side {
        self.side
    }

    /// The amount.
    #[must_use]
    pub const fn amount(&self) -> Amount<P> {
        self.amount
    }

    /// The currency.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// The layer.
    #[must_use]
    pub const fn get_layer(&self) -> Layer {
        self.layer
    }

    /// The dimensions.
    #[must_use]
    pub const fn get_dimensions(&self) -> &Dimensions {
        &self.dimensions
    }

    /// The posting's fields after the account, shared by every form.
    fn encode_tail<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.side);
        w.value(&self.amount);
        w.value(&self.currency);
        w.value(&self.layer);
        w.value(&self.dimensions);
    }
}

/// Which field of a draft a [`BuildError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Field {
    /// The idempotency key.
    Key,
    /// The entry kind.
    Kind,
    /// The memo text.
    Memo,
    /// A posting's account (by posting index).
    Posting(usize),
    /// A dimension of a posting (by posting index).
    Dimension {
        /// The posting's index.
        posting: usize,
    },
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key => f.write_str("key"),
            Self::Kind => f.write_str("kind"),
            Self::Memo => f.write_str("memo"),
            Self::Posting(index) => write!(f, "posting {index} account"),
            Self::Dimension { posting } => write!(f, "posting {posting} dimension"),
        }
    }
}

/// A builder argument of a draft that failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("{field}: {error}")]
#[non_exhaustive]
pub struct BuildError {
    /// The field.
    pub field: Field,
    /// Why its value was refused.
    pub error: ValueError,
}

/// An entry draft: what a caller submits to be recorded.
///
/// Builders are infallible: an invalid key, kind, account path, dimension or
/// memo is kept as a [`BuildError`] (see [`build_errors`](Self::build_errors))
/// and reported when the draft is recorded. `P` is inferred from the amounts
/// or fixed by the type (`Entry` alone means `Entry<2>`).
///
/// ```
/// use doubleentry::{Entry, amount, currency::EUR, error::Field};
/// use time::macros::date;
///
/// let draft: Entry = Entry::new("invoice-2026-0001", date!(2026 - 03 - 15))
///     .debit("Assets:Cash", amount!("1190.00"), EUR)
///     .credit("Income:Sales", amount!("1190.00"), EUR)
///     .memo("Rechnung 4711");
/// assert!(draft.fingerprint().is_ok());
///
/// let broken: Entry = Entry::new("", date!(2026 - 03 - 15)).debit("Assets::Cash", amount!("1.00"), EUR);
/// let fields: Vec<Field> = broken.build_errors().iter().map(|e| e.field).collect();
/// assert_eq!(fields, [Field::Key, Field::Posting(0)]);
/// ```
///
/// A hold action is not a builder choice (holds have their own constructors):
///
/// ```compile_fail
/// use doubleentry::{Entry, record::HoldAction};
/// let _ = Entry::<2>::new("k", time::macros::date!(2026 - 01 - 01))
///     .hold_action(HoldAction::Open { expires: None });
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry<const P: u8 = 2> {
    key: Result<IdempotencyKey, ValueError>,
    booking_date: Date,
    value_date: Option<Date>,
    captured_at: Option<CaptureTime>,
    kind: Option<Label>,
    memo: Option<(String, Option<MemoSalt>)>,
    documents: Vec<Document>,
    postings: Vec<Posting<P>>,
    reverses: Option<EntryId>,
    hold: Option<HoldAction<P>>,
    rates: Vec<Rate>,
    errors: Vec<BuildError>,
}

impl<const P: u8> Entry<P> {
    /// The most postings in one entry.
    pub const MAX_POSTINGS: usize = 1024;
    /// The most documents on one entry.
    pub const MAX_DOCUMENTS: usize = 16;
    /// The most rates on one entry.
    pub const MAX_RATES: usize = 16;
    /// The most bytes of memo text.
    pub const MAX_MEMO_BYTES: usize = 4096;

    /// A draft under an idempotency key, booked on a date.
    pub fn new<K>(key: K, booking_date: Date) -> Self
    where
        K: TryInto<IdempotencyKey>,
        K::Error: Into<ValueError>,
    {
        let key = key.try_into().map_err(Into::into);
        let errors = key
            .as_ref()
            .err()
            .map(|error| BuildError {
                field: Field::Key,
                error: *error,
            })
            .into_iter()
            .collect();
        Self {
            key,
            booking_date,
            value_date: None,
            captured_at: None,
            kind: None,
            memo: None,
            documents: Vec::new(),
            postings: Vec::new(),
            reverses: None,
            hold: None,
            rates: Vec::new(),
            errors,
        }
    }

    /// Records (or clears) the build error of a field that can be set again.
    fn set_error(&mut self, field: Field, error: Option<ValueError>) {
        self.errors.retain(|existing| existing.field != field);
        if let Some(error) = error {
            self.errors.push(BuildError { field, error });
        }
    }

    /// With a value date (defaults to the booking date).
    #[must_use]
    pub fn value_date(self, date: Date) -> Self {
        Self {
            value_date: Some(date),
            ..self
        }
    }

    /// With the time the caller captured the entry.
    #[must_use]
    pub fn captured_at(self, at: CaptureTime) -> Self {
        Self {
            captured_at: Some(at),
            ..self
        }
    }

    /// With an entry kind (`"invoice"`, `"payment"`, …).
    #[must_use]
    pub fn kind<L>(mut self, kind: L) -> Self
    where
        L: TryInto<Label>,
        L::Error: Into<ValueError>,
    {
        match kind.try_into().map_err(Into::into) {
            Ok(kind) => {
                self.kind = Some(kind);
                self.set_error(Field::Kind, None);
            }
            Err(error) => {
                self.kind = None;
                self.set_error(Field::Kind, Some(error));
            }
        }
        self
    }

    /// With a memo: 1–4 096 bytes, tabs and line breaks allowed, no other
    /// control character (an invalid memo is rejected as 108 `InvalidMemo`).
    /// Its salt is supplied when the entry is recorded. The memo is the only
    /// free-text field (see the module docs).
    #[must_use]
    pub fn memo(self, text: impl Into<String>) -> Self {
        self.set_memo(text.into(), None)
    }

    /// With a memo and the salt of its commitment.
    #[must_use]
    pub fn memo_salted(self, text: impl Into<String>, salt: MemoSalt) -> Self {
        self.set_memo(text.into(), Some(salt))
    }

    fn set_memo(mut self, text: String, salt: Option<MemoSalt>) -> Self {
        let error = if text.is_empty() {
            Some(ValueError::Empty)
        } else if text.len() > Self::MAX_MEMO_BYTES {
            Some(ValueError::TooLong {
                max: Self::MAX_MEMO_BYTES,
                found: text.len(),
            })
        } else if text.chars().any(is_memo_control) {
            Some(ValueError::ControlCharacter)
        } else {
            None
        };
        self.set_error(Field::Memo, error);
        self.memo = Some((text, salt));
        self
    }

    /// With a source document.
    #[must_use]
    pub fn document(mut self, document: Document) -> Self {
        self.documents.push(document);
        self
    }

    /// With a debit posting.
    #[must_use]
    pub fn debit<A>(self, account: A, amount: Amount<P>, currency: Currency) -> Self
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.posting(Posting::debit(account, amount, currency))
    }

    /// With a credit posting.
    #[must_use]
    pub fn credit<A>(self, account: A, amount: Amount<P>, currency: Currency) -> Self
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.posting(Posting::credit(account, amount, currency))
    }

    /// With a posting built by [`Posting`]. On a hold
    /// ([`open_hold`](Self::open_hold)) the posting is Pending, on every
    /// other draft Settled.
    #[must_use]
    pub fn posting(mut self, posting: Posting<P>) -> Self {
        let posting = if matches!(self.hold, Some(HoldAction::Open { .. })) {
            posting.pending()
        } else {
            posting
        };
        let index = self.postings.len();
        if let Err(error) = &posting.account {
            self.errors.push(BuildError {
                field: Field::Posting(index),
                error: *error,
            });
        }
        for error in &posting.dimension_errors {
            self.errors.push(BuildError {
                field: Field::Dimension { posting: index },
                error: *error,
            });
        }
        self.postings.push(posting);
        self
    }

    /// A hold: a reservation of one amount from one account to another,
    /// resolved later exactly once. Add exactly one debit and one credit of
    /// the same amount and currency; on this draft they are Pending postings
    /// (anything else is refused with code 140). Optionally it
    /// [`expires`](Self::expires).
    ///
    /// The hold's id is the id of its entry ([`Recorded::id`](crate::Recorded)),
    /// which its resolution names. While it is open, its amount counts against
    /// the balance limits of both accounts.
    ///
    /// ```
    /// use doubleentry::{Entry, Layer, amount, date, currency::EUR};
    ///
    /// let hold: Entry = Entry::open_hold("auth-4711", date!("2026-11-20"))
    ///     .expires(date!("2026-11-30"))
    ///     .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
    ///     .credit("Liabilities:Card:clearing", amount!("100.00"), EUR);
    /// assert!(hold.postings().iter().all(|p| p.get_layer() == Layer::Pending));
    /// ```
    pub fn open_hold<K>(key: K, booking_date: Date) -> Self
    where
        K: TryInto<IdempotencyKey>,
        K::Error: Into<ValueError>,
    {
        Self::new(key, booking_date).hold_action(HoldAction::Open { expires: None })
    }

    /// The first day a hold may be expired ([`expire_hold`](Self::expire_hold))
    /// and the first day it can no longer be posted. It must lie after the
    /// hold's booking date (code 141). Only a hold expires: on any other draft
    /// this changes nothing.
    ///
    /// ```
    /// use doubleentry::{Entry, date, record::HoldAction};
    ///
    /// let hold: Entry = Entry::open_hold("auth-4711", date!("2026-11-20")).expires(date!("2026-11-30"));
    /// assert_eq!(hold.get_hold(), Some(&HoldAction::Open { expires: Some(date!("2026-11-30")) }));
    /// let entry: Entry = Entry::new("sale-1", date!("2026-11-20")).expires(date!("2026-11-30"));
    /// assert_eq!(entry.get_hold(), None);
    /// ```
    #[must_use]
    pub fn expires(self, on: Date) -> Self {
        match self.hold {
            Some(HoldAction::Open { .. }) => {
                self.hold_action(HoldAction::Open { expires: Some(on) })
            }
            _ => self,
        }
    }

    /// Posts (captures) the hold `hold`: all of it, or part with the rest
    /// released in the same entry. The draft carries no postings — the rule
    /// core derives them from the hold: the two Pending legs released, then
    /// the posted amount settled between the hold's two accounts. A post must
    /// be booked before the hold's expiry date.
    ///
    /// ```
    /// use doubleentry::{AccountKind, Entry, Journal, Layer, LedgerId, PostAmount, amount, date, currency::EUR};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/payments")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.define_months(2026, 11..=11)?;
    /// let hold = books.record(
    ///     Entry::open_hold("auth-4711", date!("2026-11-20"))
    ///         .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
    ///         .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
    /// )?;
    /// let capture = books.record(Entry::post_hold(
    ///     "capture-4711",
    ///     date!("2026-11-22"),
    ///     hold.id,
    ///     PostAmount::Partial(amount!("60.00")),
    /// ))?;
    /// let postings = books.entry(capture.id).map(|entry| entry.parts().postings.clone()).unwrap_or_default();
    /// let legs: Vec<_> = postings.iter().map(|p| (p.layer, p.side, p.amount)).collect();
    /// use doubleentry::Side::{Credit, Debit};
    /// assert_eq!(legs, [
    ///     (Layer::Pending, Credit, amount!("100.00")), // the reservation released
    ///     (Layer::Pending, Debit, amount!("100.00")),
    ///     (Layer::Settled, Debit, amount!("60.00")),   // the capture settled
    ///     (Layer::Settled, Credit, amount!("60.00")),
    /// ]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn post_hold<K>(key: K, booking_date: Date, hold: EntryId, amount: PostAmount<P>) -> Self
    where
        K: TryInto<IdempotencyKey>,
        K::Error: Into<ValueError>,
    {
        Self::new(key, booking_date).hold_action(HoldAction::Post { hold, amount })
    }

    /// Voids (releases) the hold `hold` on any day from its booking date on,
    /// even after its expiry date or after one of its accounts closed. The
    /// core derives the two Pending release postings.
    ///
    /// ```
    /// use doubleentry::{Entry, EntryId, date, record::HoldAction};
    ///
    /// let hold = EntryId::from_bytes([7; 16]);
    /// let void: Entry = Entry::void_hold("void-4711", date!("2026-11-21"), hold);
    /// assert_eq!(void.get_hold(), Some(&HoldAction::Void { hold }));
    /// assert!(void.postings().is_empty());
    /// ```
    pub fn void_hold<K>(key: K, booking_date: Date, hold: EntryId) -> Self
    where
        K: TryInto<IdempotencyKey>,
        K::Error: Into<ValueError>,
    {
        Self::new(key, booking_date).hold_action(HoldAction::Void { hold })
    }

    /// Expires the hold `hold`: releases it like a void, booked on or after
    /// its expiry date. Nothing expires by itself — a scheduler lists the
    /// due holds (`holds_due`) and records an expiry for each.
    ///
    /// ```
    /// use doubleentry::{Entry, EntryId, date, record::HoldAction};
    ///
    /// let hold = EntryId::from_bytes([7; 16]);
    /// let expiry: Entry = Entry::expire_hold("expire-4711", date!("2026-11-30"), hold);
    /// assert_eq!(expiry.get_hold(), Some(&HoldAction::Expire { hold }));
    /// ```
    pub fn expire_hold<K>(key: K, booking_date: Date, hold: EntryId) -> Self
    where
        K: TryInto<IdempotencyKey>,
        K::Error: Into<ValueError>,
    {
        Self::new(key, booking_date).hold_action(HoldAction::Expire { hold })
    }

    /// With a hold action: only the hold constructors set one.
    fn hold_action(self, action: HoldAction<P>) -> Self {
        Self {
            hold: Some(action),
            ..self
        }
    }

    /// Marks the draft as the reversal of an earlier entry.
    #[must_use]
    pub fn reverses(self, target: EntryId) -> Self {
        Self {
            reverses: Some(target),
            ..self
        }
    }

    /// With an exchange rate; rates are kept sorted by (base, quote) and a
    /// second rate for the same pair replaces the first.
    #[must_use]
    pub fn rate(mut self, rate: Rate) -> Self {
        let pair = |r: &Rate| (r.base, r.quote);
        match self.rates.binary_search_by_key(&pair(&rate), pair) {
            Ok(index) => {
                if let Some(slot) = self.rates.get_mut(index) {
                    *slot = rate;
                }
            }
            Err(index) => self.rates.insert(index, rate),
        }
        self
    }

    /// The idempotency key; `None` when it was invalid.
    #[must_use]
    pub fn key(&self) -> Option<&IdempotencyKey> {
        self.key.as_ref().ok()
    }

    /// The booking date.
    #[must_use]
    pub const fn booking_date(&self) -> Date {
        self.booking_date
    }

    /// The value date (the booking date unless set).
    #[must_use]
    pub fn get_value_date(&self) -> Date {
        self.value_date.unwrap_or(self.booking_date)
    }

    /// The capture time.
    #[must_use]
    pub const fn get_captured_at(&self) -> Option<CaptureTime> {
        self.captured_at
    }

    /// The entry kind.
    #[must_use]
    pub const fn get_kind(&self) -> Option<&Label> {
        self.kind.as_ref()
    }

    /// The memo text.
    #[must_use]
    pub fn memo_text(&self) -> Option<&str> {
        self.memo.as_ref().map(|(text, _)| text.as_str())
    }

    /// The memo salt, when the caller supplied one.
    #[must_use]
    pub fn memo_salt(&self) -> Option<&MemoSalt> {
        self.memo.as_ref().and_then(|(_, salt)| salt.as_ref())
    }

    /// The documents, in the order added.
    #[must_use]
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// The postings, in the order added.
    #[must_use]
    pub fn postings(&self) -> &[Posting<P>] {
        &self.postings
    }

    /// The entry this one reverses.
    #[must_use]
    pub const fn get_reverses(&self) -> Option<EntryId> {
        self.reverses
    }

    /// The hold action.
    #[must_use]
    pub const fn get_hold(&self) -> Option<&HoldAction<P>> {
        self.hold.as_ref()
    }

    /// The rates, sorted by (base, quote).
    #[must_use]
    pub fn rates(&self) -> &[Rate] {
        &self.rates
    }

    /// The builder arguments that failed validation, in the order given.
    #[must_use]
    pub fn build_errors(&self) -> &[BuildError] {
        &self.errors
    }

    /// The length of the entry leaf this draft becomes, without allocating,
    /// to check against [`MAX_LEAF_BYTES`](crate::record::MAX_LEAF_BYTES)
    /// before submitting. Meaningful only without build errors. A hold
    /// resolution's derived postings (two to four) are not counted.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let placeholder = MemoCommitment([0; 32]);
        self.body(
            DraftPostings {
                postings: &self.postings,
                form: AccountForm::Handle,
            },
            self.memo.as_ref().map(|_| &placeholder),
        )
        .encoded_len()
        .saturating_add(1)
    }

    /// The draft's [`Fingerprint`]: a tagged SHA-256 of the entry body as
    /// submitted — accounts as given (path or handle), the memo as text, no
    /// salt — so it depends on nothing but the draft.
    ///
    /// # Errors
    ///
    /// The first [`BuildError`], while any exists.
    pub fn fingerprint(&self) -> Result<Fingerprint, BuildError> {
        if let Some(error) = self.errors.first() {
            return Err(*error);
        }
        let body = self.body(
            DraftPostings {
                postings: &self.postings,
                form: AccountForm::AsGiven,
            },
            self.memo.as_ref().map(|(text, _)| text.as_str()),
        );
        Ok(Fingerprint(
            *tagged(TAG_FINGERPRINT, &body.to_bytes()).as_bytes(),
        ))
    }

    /// The fingerprint a ledger keeps for this draft once its accounts are
    /// resolved to `handles` (one per posting, in order): the
    /// [`fingerprint`](Self::fingerprint) of the same draft naming every
    /// account by its handle, so an audit can recompute it from the leaf and
    /// memo text.
    ///
    /// # Errors
    ///
    /// The first [`BuildError`], while any exists, or a posting count other
    /// than `handles.len()` (as a posting error).
    pub(crate) fn resolved_fingerprint(
        &self,
        handles: &[AccountHandle],
    ) -> Result<Fingerprint, BuildError> {
        if let Some(error) = self.errors.first() {
            return Err(*error);
        }
        if handles.len() != self.postings.len() {
            return Err(BuildError {
                field: Field::Posting(handles.len().min(self.postings.len())),
                error: ValueError::Empty,
            });
        }
        let postings: Vec<PostingRecord<P>> = self
            .postings
            .iter()
            .zip(handles)
            .map(|(posting, handle)| PostingRecord {
                handle: *handle,
                side: posting.side,
                amount: posting.amount,
                currency: posting.currency,
                layer: posting.layer,
                dimensions: posting.dimensions.clone(),
            })
            .collect();
        let body = EntryBody {
            key: self.key.as_ref().map_or("", IdempotencyKey::as_str),
            booking_date: self.booking_date,
            value_date: self.get_value_date(),
            captured_at: self.captured_at.as_ref(),
            kind: self.kind.as_ref(),
            memo: self.memo.as_ref().map(|(text, _)| text.as_str()),
            documents: &self.documents,
            postings: ByHandle(&postings),
            reverses: self.reverses.as_ref(),
            hold: self.hold.as_ref(),
            rates: &self.rates,
        };
        Ok(Fingerprint(
            *tagged(TAG_FINGERPRINT, &body.to_bytes()).as_bytes(),
        ))
    }

    /// The id the entry will have in `ledger`; `None` while the key is invalid.
    #[must_use]
    pub fn id(&self, ledger: &LedgerId) -> Option<EntryId> {
        self.key().map(|key| EntryId::derive(ledger, key))
    }

    fn body<'a, M: Encode + ?Sized>(
        &'a self,
        postings: DraftPostings<'a, P>,
        memo: Option<&'a M>,
    ) -> EntryBody<'a, P, DraftPostings<'a, P>, M> {
        EntryBody {
            key: self.key.as_ref().map_or("", IdempotencyKey::as_str),
            booking_date: self.booking_date,
            value_date: self.get_value_date(),
            captured_at: self.captured_at.as_ref(),
            kind: self.kind.as_ref(),
            memo,
            documents: &self.documents,
            postings,
            reverses: self.reverses.as_ref(),
            hold: self.hold.as_ref(),
            rates: &self.rates,
        }
    }
}

/// The fingerprint a ledger keeps for an entry recorded as `parts`, with
/// `postings` as its draft carried them (none for a hold resolution, whose
/// postings are derived) and `memo` its memo text: the draft's fingerprint
/// with every account named by handle (see [`Entry::resolved_fingerprint`]).
pub(crate) fn record_fingerprint<const P: u8>(
    parts: &EntryParts<P>,
    postings: &[PostingRecord<P>],
    memo: Option<&str>,
) -> Fingerprint {
    let body = EntryBody {
        key: parts.key.as_str(),
        booking_date: parts.booking_date,
        value_date: parts.value_date,
        captured_at: parts.captured_at.as_ref(),
        kind: parts.kind.as_ref(),
        memo,
        documents: &parts.documents,
        postings: ByHandle(postings),
        reverses: parts.reverses.as_ref(),
        hold: parts.hold.as_ref(),
        rates: &parts.rates,
    };
    Fingerprint(*tagged(TAG_FINGERPRINT, &body.to_bytes()).as_bytes())
}

/// Postings named by handle, as a draft naming every account by its handle
/// writes them for its fingerprint: `0x01 ‖ u32 handle ‖ side ‖ amount ‖
/// currency ‖ layer ‖ dimensions` each.
struct ByHandle<'a, const P: u8>(&'a [PostingRecord<P>]);

impl<const P: u8> Encode for ByHandle<'_, P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.len(self.0.len());
        for posting in self.0 {
            w.u8(1);
            w.value(&posting.handle);
            w.value(&posting.side);
            w.value(&posting.amount);
            w.value(&posting.currency);
            w.value(&posting.layer);
            w.value(&posting.dimensions);
        }
    }
}

/// How a draft posting's account is written.
#[derive(Clone, Copy)]
enum AccountForm {
    /// As submitted: `0x00 ‖ str(path)` or `0x01 ‖ u32 handle` (fingerprint).
    AsGiven,
    /// As the leaf will hold it: a 4-byte handle (length only).
    Handle,
}

/// A draft's postings in one of their encoded forms.
struct DraftPostings<'a, const P: u8> {
    postings: &'a [Posting<P>],
    form: AccountForm,
}

impl<const P: u8> Encode for DraftPostings<'_, P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.len(self.postings.len());
        for posting in self.postings {
            match (self.form, &posting.account) {
                (AccountForm::Handle, _) => w.u32(0),
                (AccountForm::AsGiven, Ok(AccountRef::Path(path))) => {
                    w.u8(0);
                    w.value(path);
                }
                (AccountForm::AsGiven, Ok(AccountRef::Handle(handle))) => {
                    w.u8(1);
                    w.value(handle);
                }
                // Unreachable: a fingerprint is refused while build errors exist.
                (AccountForm::AsGiven, Err(_)) => {}
            }
            posting.encode_tail(w);
        }
    }
}

/// The fields of an entry body in their committed order, shared by
/// the entry record and by the draft's fingerprint and length.
pub(crate) struct EntryBody<'a, const P: u8, Ps, M: ?Sized> {
    pub(crate) key: &'a str,
    pub(crate) booking_date: Date,
    pub(crate) value_date: Date,
    pub(crate) captured_at: Option<&'a CaptureTime>,
    pub(crate) kind: Option<&'a Label>,
    pub(crate) memo: Option<&'a M>,
    pub(crate) documents: &'a [Document],
    pub(crate) postings: Ps,
    pub(crate) reverses: Option<&'a EntryId>,
    pub(crate) hold: Option<&'a HoldAction<P>>,
    pub(crate) rates: &'a [Rate],
}

impl<const P: u8, Ps: Encode, M: Encode + ?Sized> Encode for EntryBody<'_, P, Ps, M> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.str(self.key);
        w.date(self.booking_date);
        w.date(self.value_date);
        w.opt(self.captured_at);
        w.opt(self.kind);
        w.opt(self.memo);
        w.seq(self.documents);
        w.value(&self.postings);
        w.opt(self.reverses);
        w.opt(self.hold);
        w.seq(self.rates);
    }
}

#[cfg(feature = "serde")]
mod serde_impl {
    //! Drafts and entry values travel as plain data; deserialising re-runs
    //! every constructor, so a value read back keeps its invariants.

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use time::Date;

    use super::{
        AccountHandle, AccountPath, AccountRef, Amount, CaptureTime, Currency, Dimensions,
        Document, Entry, EntryId, HoldAction, IdempotencyKey, Label, Layer, MemoSalt, Posting,
        Rate, Side, hex,
    };

    impl Serialize for AccountRef {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Self::Path(path) => path.serialize(serializer),
                Self::Handle(handle) => handle.serialize(serializer),
            }
        }
    }

    impl<'de> Deserialize<'de> for AccountRef {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum Wire {
                Handle(AccountHandle),
                Path(AccountPath),
            }
            Ok(match Wire::deserialize(deserializer)? {
                Wire::Handle(handle) => Self::Handle(handle),
                Wire::Path(path) => Self::Path(path),
            })
        }
    }

    impl Serialize for MemoSalt {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(&HexBytes(&self.0))
        }
    }

    impl<'de> Deserialize<'de> for MemoSalt {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let text = String::deserialize(deserializer)?;
            hex::parse(&text).map(Self).map_err(D::Error::custom)
        }
    }

    struct HexBytes<'a>(&'a [u8]);

    impl core::fmt::Display for HexBytes<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            hex::write(f, self.0)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename = "CaptureTime")]
    struct CaptureTimeWire {
        unix_seconds: i64,
        utc_offset_minutes: i16,
    }

    impl Serialize for CaptureTime {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            CaptureTimeWire {
                unix_seconds: self.unix_seconds,
                utc_offset_minutes: self.utc_offset_minutes,
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for CaptureTime {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let wire = CaptureTimeWire::deserialize(deserializer)?;
            Self::new(wire.unix_seconds, wire.utc_offset_minutes).map_err(D::Error::custom)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename = "Document")]
    struct DocumentWire {
        doc_type: Label,
        reference: String,
        date: Option<Date>,
        sha256: Option<String>,
    }

    impl Serialize for Document {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            DocumentWire {
                doc_type: self.doc_type.clone(),
                reference: self.reference.clone(),
                date: self.date,
                sha256: self.sha256.map(|digest| HexBytes(&digest).to_string()),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Document {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let wire = DocumentWire::deserialize(deserializer)?;
            let mut document =
                Self::new(wire.doc_type, wire.reference).map_err(D::Error::custom)?;
            if let Some(date) = wire.date {
                document = document.dated(date);
            }
            if let Some(digest) = wire.sha256 {
                document = document.with_sha256(hex::parse(&digest).map_err(D::Error::custom)?);
            }
            Ok(document)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename = "Rate")]
    struct RateWire {
        base: Currency,
        quote: Currency,
        rate: String,
    }

    impl Serialize for Rate {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            RateWire {
                base: self.base,
                quote: self.quote,
                rate: self.to_string(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Rate {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let wire = RateWire::deserialize(deserializer)?;
            Self::parse(wire.base, wire.quote, &wire.rate).map_err(D::Error::custom)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename = "Posting")]
    struct PostingWire<const P: u8> {
        account: AccountRef,
        side: Side,
        amount: Amount<P>,
        currency: Currency,
        layer: Layer,
        #[serde(default)]
        dimensions: Dimensions,
    }

    impl<const P: u8> Serialize for Posting<P> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let account = self.account.clone().map_err(serde::ser::Error::custom)?;
            if let Some(error) = self.dimension_errors.first() {
                return Err(serde::ser::Error::custom(error));
            }
            PostingWire {
                account,
                side: self.side,
                amount: self.amount,
                currency: self.currency,
                layer: self.layer,
                dimensions: self.dimensions.clone(),
            }
            .serialize(serializer)
        }
    }

    impl<'de, const P: u8> Deserialize<'de> for Posting<P> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let wire = PostingWire::<P>::deserialize(deserializer)?;
            Ok(Self {
                layer: wire.layer,
                ..Self::new(wire.account, wire.side, wire.amount, wire.currency)
                    .dimensions(wire.dimensions)
            })
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename = "Entry")]
    struct EntryWire<const P: u8> {
        key: IdempotencyKey,
        booking_date: Date,
        value_date: Date,
        captured_at: Option<CaptureTime>,
        kind: Option<Label>,
        memo: Option<String>,
        memo_salt: Option<MemoSalt>,
        #[serde(default)]
        documents: Vec<Document>,
        postings: Vec<Posting<P>>,
        reverses: Option<EntryId>,
        hold: Option<HoldAction<P>>,
        #[serde(default)]
        rates: Vec<Rate>,
    }

    impl<const P: u8> Serialize for Entry<P> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if let Some(error) = self.errors.first() {
                return Err(serde::ser::Error::custom(error));
            }
            let key = self.key.clone().map_err(serde::ser::Error::custom)?;
            EntryWire {
                key,
                booking_date: self.booking_date,
                value_date: self.get_value_date(),
                captured_at: self.captured_at,
                kind: self.kind.clone(),
                memo: self.memo_text().map(str::to_owned),
                memo_salt: self.memo_salt().copied(),
                documents: self.documents.clone(),
                postings: self.postings.clone(),
                reverses: self.reverses,
                hold: self.hold,
                rates: self.rates.clone(),
            }
            .serialize(serializer)
        }
    }

    impl<'de, const P: u8> Deserialize<'de> for Entry<P> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let wire = EntryWire::<P>::deserialize(deserializer)?;
            let mut entry = Self::new(wire.key, wire.booking_date).value_date(wire.value_date);
            entry.captured_at = wire.captured_at;
            entry.kind = wire.kind;
            entry.reverses = wire.reverses;
            entry.hold = wire.hold;
            entry = match (wire.memo, wire.memo_salt) {
                (Some(text), Some(salt)) => entry.memo_salted(text, salt),
                (Some(text), None) => entry.memo(text),
                (None, _) => entry,
            };
            entry = wire.documents.into_iter().fold(entry, Self::document);
            entry = wire.postings.into_iter().fold(entry, Self::posting);
            entry = wire.rates.into_iter().fold(entry, Self::rate);
            match entry.errors.first() {
                Some(error) => Err(D::Error::custom(error)),
                None => Ok(entry),
            }
        }
    }
}
