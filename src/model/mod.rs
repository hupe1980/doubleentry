//! Values and records: everything that is committed to the log.
//!
//! This module owns every committed byte. A value is validated once, by its
//! constructor, and has exactly one canonical byte form; a [`Record`] is one
//! typed leaf of the ledger's log.
//!
//! # The decoder and the rule core
//!
//! [`Record::decode`] (and every record constructor) refuses anything that is
//! not a valid *value* in its one canonical form: bad tags, truncation,
//! trailing bytes, invalid or non-NFC text, out-of-range dates, counts above
//! the format maximums, unsorted or duplicate keyed sequences, non-normalised
//! rates, gross balances that violate their invariant, and a genesis whose
//! scale differs from the decoding `P`. Business rules — at least two postings,
//! positive amounts, balance per (currency, layer), handle sequencing,
//! references, periods, limits — belong to the rule core, which replays the
//! log in `audit()`.
//!
//! # Format reference (`doubleentry/1`)
//!
//! `leaf_bytes = u8 kind ‖ body`, at most [`MAX_LEAF_BYTES`] bytes.
//!
//! | Primitive | Bytes |
//! |---|---|
//! | `u8 u16 u32 u64 i8 i16 i32 i64 i128` | fixed width, little-endian |
//! | `str(x)` | `u64` byte length ‖ UTF-8 bytes |
//! | `opt(x)` | `0x00` \| `0x01 ‖ x` |
//! | `seq(x)` | `u64` count ‖ elements |
//! | `Date` | `i32` Julian day number (`2026-01-01` = 2 461 042) |
//! | `Amount` | `i128` minor units |
//! | `Currency`, `Label`, `AccountPath` (joined with `:`), `LedgerId`, `IdempotencyKey` | `str` |
//! | `EntryId` / hashes, commitments | 16 / 32 raw bytes |
//! | `Side` / `Layer` | `Debit 0` `Credit 1` / `Settled 0` `Pending 1` |
//! | `AccountKind` | `Asset 0` `Liability 1` `Equity 2` `Income 3` `Expense 4` |
//! | `BalanceLimit` | `Unlimited 0` `NoCreditBalance 1` `NoDebitBalance 2` |
//! | `Dimensions` | `seq(str(axis) ‖ str(value))`, ascending axis |
//! | `CaptureTime` | `i64` unix seconds ‖ `i16` UTC offset minutes |
//! | `Document` | `str(type) ‖ str(reference) ‖ opt(Date) ‖ opt([32])` |
//! | `Rate` | `str(base) ‖ str(quote) ‖ i128 mantissa ‖ i8 exponent` (normalised) |
//! | `HoldAction` | `0 Open: opt(Date)` \| `1 Post: [16] ‖ (0 Full \| 1 ‖ i128)` \| `2 Void: [16]` \| `3 Expire: [16]` |
//! | `LedgerPolicy` | `seq(str(label))` ascending ‖ `opt(u32)` |
//! | `Balance` | `i128 debit ‖ i128 credit ‖ u64 postings` |
//! | `PostingRef` | `[16] entry ‖ u16 index` |
//!
//! | Kind | Body |
//! |---|---|
//! | `0x01` Genesis | `str(ledger) ‖ u8 P ‖ str("doubleentry/1")` |
//! | `0x02` Account | `u8 action ‖ body ‖ Date effective ‖ str(actor)`; register `0`: `u32 handle ‖ str(path) ‖ u8 kind ‖ opt(str(class)) ‖ Date open_from ‖ opt(Date open_to) ‖ u8 limit`; change `1`: `u32 handle ‖ u8 field ‖ value` (`0` open_to `opt(Date)`, `1` limit `u8`, `2` class `opt(str)`) |
//! | `0x03` Currency | `str(code) ‖ u8 exponent ‖ Date effective ‖ str(actor)` |
//! | `0x04` Policy | `LedgerPolicy ‖ Date effective ‖ str(actor)` |
//! | `0x05` Period | `str(id) ‖ Date start ‖ Date end` |
//! | `0x06` Entry | `str(key) ‖ Date booking ‖ Date value ‖ opt(CaptureTime) ‖ opt(str(kind)) ‖ opt([32] memo) ‖ seq(Document) ‖ seq(Posting) ‖ opt([16] reverses) ‖ opt(HoldAction) ‖ seq(Rate)`; Posting = `u32 handle ‖ u8 side ‖ i128 amount ‖ str(currency) ‖ u8 layer ‖ Dimensions` |
//! | `0x07` Clearing | `u8 action ‖ body ‖ Date date`; apply `0`: `str(key) ‖ PostingRef ‖ PostingRef ‖ i128 amount`; reset `1`: `str(key) ‖ u64 position` |
//! | `0x08` Seal | `str(period) ‖ Date start ‖ Date end ‖ u64 through ‖ u32 accounts ‖ seq(str(currency) ‖ u8 layer ‖ [32] root ‖ Balance)`, ascending by (currency, layer) |
//! | `0x81` | balance-tree leaf `Balance` (never a log record) |
//!
//! Tagged hashes outside the tree are `SHA-256(u64 len(tag) ‖ tag ‖ payload)`
//! with the tags `doubleentry/1/entry-id`, `doubleentry/1/memo` and
//! `doubleentry/1/fingerprint`; the first preimage byte is the tag length
//! (2–255), so no tagged preimage equals an RFC 6962 leaf or node preimage.

mod amount;
mod currency;
mod dimensions;
mod encoding;
mod entry;
mod ids;
mod label;
mod path;
mod policy;
mod record;

pub use amount::{Amount, AmountError, Balance, Rounding};
pub use currency::{
    AUD, BHD, BRL, CAD, CHF, CNY, CZK, Currency, DKK, EUR, GBP, HKD, HUF, INR, JPY, KRW, KWD, MXN,
    NOK, NZD, PLN, SEK, SGD, TRY, USD, ZAR,
};
pub use dimensions::Dimensions;
pub use encoding::{DecodeError, MAX_LEAF_BYTES};
pub use entry::{
    AccountRef, BuildError, CaptureTime, Document, Entry, Field, Fingerprint, HoldAction, Layer,
    MemoCommitment, MemoSalt, PostAmount, Posting, Rate, Side,
};
pub use ids::{AccountHandle, EntryId, IdempotencyKey, LedgerId};
pub use label::{Label, PeriodId};
pub use path::AccountPath;
pub use policy::LedgerPolicy;
pub use record::{
    AccountAction, AccountChange, AccountEvent, AccountKind, AccountRegistration,
    BALANCE_LEAF_KIND, BalanceLimit, ClearingAction, ClearingEvent, CurrencyDeclaration,
    EntryParts, EntryRecord, FORMAT, Genesis, PeriodDefinition, PolicyEvent, PostingRecord,
    PostingRef, Record, RecordKind, Seal, SealOrderError, SealParts, TreeHeadEntry,
};

pub(crate) use encoding::{Decode, Encode, Reader, Sink, Writer, hex, pad};
pub(crate) use entry::record_fingerprint;

use unicode_normalization::UnicodeNormalization;

/// Serde through `Display` / `FromStr`: a value is read back only through its
/// validating constructor.
#[cfg(feature = "serde")]
pub(crate) mod serde_str {
    use core::fmt::Display;
    use core::str::FromStr;

    pub(crate) fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        let text = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Serde of raw bytes (leaf bytes) as lowercase hex text.
#[cfg(feature = "serde")]
pub(crate) mod serde_hex {
    pub(crate) fn serialize<S: serde::Serializer>(
        bytes: &[u8],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        struct Hex<'a>(&'a [u8]);
        impl core::fmt::Display for Hex<'_> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                super::hex::write(f, self.0)
            }
        }
        serializer.collect_str(&Hex(bytes))
    }

    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        use serde::de::Error as _;
        let text = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
        super::hex::parse_vec(&text).ok_or_else(|| D::Error::custom(ValueError::InvalidHex))
    }

    use super::ValueError;
}

/// Why a value could not be constructed.
///
/// One error type for every value constructor; each variant names the rule
/// that was violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ValueError {
    /// The value is empty (or a path has an empty segment).
    #[error("the value is empty")]
    Empty,
    /// The value is longer than its limit.
    #[error("the value is too long: {found} exceeds the maximum of {max}")]
    TooLong {
        /// The limit (characters, bytes or elements, as documented on the type).
        max: usize,
        /// The length found.
        found: usize,
    },
    /// The text starts or ends with whitespace.
    #[error("the text starts or ends with whitespace")]
    EdgeWhitespace,
    /// The text contains a control character.
    #[error("the text contains a control character")]
    ControlCharacter,
    /// The text contains an invisible character: a Unicode default-ignorable
    /// code point (zero-width space, joiner, byte-order mark, bidirectional
    /// override, …) or a line or paragraph separator, so two different names
    /// would print alike.
    #[error("the text contains an invisible character {found:?}")]
    InvisibleCharacter {
        /// The first offending character.
        found: char,
    },
    /// An account path segment contains the separator `:`.
    #[error("an account path segment contains the separator ':'")]
    SeparatorInSegment,
    /// An account path has more segments than allowed.
    #[error("the account path is deeper than {max} segments")]
    TooDeep {
        /// The maximum depth.
        max: usize,
    },
    /// A character outside the value's alphabet.
    #[error("invalid character {found:?}")]
    InvalidCharacter {
        /// The first offending character.
        found: char,
    },
    /// A dimension axis was given a second value.
    #[error("the dimension axis already has a value")]
    DuplicateAxis,
    /// More dimension axes than allowed.
    #[error("more than {} dimension axes", Dimensions::MAX_AXES)]
    TooManyDimensions,
    /// A number outside the value's range.
    #[error("the value is out of range")]
    OutOfRange,
    /// A value that must be positive is zero or negative.
    #[error("the value must be positive")]
    NotPositive,
    /// A rate whose base and quote currency are the same.
    #[error("a rate needs two different currencies")]
    SameCurrency,
    /// Text that is not the expected number of hexadecimal digits.
    #[error("invalid hexadecimal text")]
    InvalidHex,
    /// A sequence that must be in ascending order is not.
    #[error("the sequence is not in ascending order")]
    NotSorted,
    /// A sequence that must not repeat an element does.
    #[error("the sequence contains a duplicate")]
    Duplicate,
    /// A currency exponent above the ledger's scale `P`.
    #[error("currency exponent {exponent} exceeds the ledger scale {scale}")]
    ExponentAboveScale {
        /// The declared exponent.
        exponent: u8,
        /// The scale `P`.
        scale: u8,
    },
    /// A record whose leaf bytes would exceed [`MAX_LEAF_BYTES`].
    #[error("the record would encode to {found} bytes, more than the maximum of {max}")]
    TooLarge {
        /// [`MAX_LEAF_BYTES`].
        max: usize,
        /// The encoded size.
        found: usize,
    },
}

impl From<core::convert::Infallible> for ValueError {
    fn from(never: core::convert::Infallible) -> Self {
        match never {}
    }
}

/// Whether `c` prints as nothing: a Unicode `Default_Ignorable_Code_Point`
/// (Unicode 16, `DerivedCoreProperties.txt`) or a line or paragraph
/// separator (`Zl`, `Zp`).
pub(crate) const fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Validates text as a label-like value: NFC-normalised, 1–`max_chars`
/// Unicode scalar values, no whitespace at either edge, no control character
/// and no invisible character ([`is_invisible`]).
pub(crate) fn normalize_label(text: &str, max_chars: usize) -> Result<String, ValueError> {
    let normalized: String = text.nfc().collect();
    if normalized.is_empty() {
        return Err(ValueError::Empty);
    }
    let chars = normalized.chars().count();
    if chars > max_chars {
        return Err(ValueError::TooLong {
            max: max_chars,
            found: chars,
        });
    }
    if normalized.starts_with(char::is_whitespace) || normalized.ends_with(char::is_whitespace) {
        return Err(ValueError::EdgeWhitespace);
    }
    if normalized.chars().any(char::is_control) {
        return Err(ValueError::ControlCharacter);
    }
    if let Some(found) = normalized.chars().find(|c| is_invisible(*c)) {
        return Err(ValueError::InvisibleCharacter { found });
    }
    Ok(normalized)
}

/// Whether text is already in Unicode NFC (the decoder's check: a leaf has
/// exactly one valid representation).
pub(crate) fn is_nfc(text: &str) -> bool {
    unicode_normalization::is_nfc(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfd_text_is_normalised_to_nfc() {
        assert_eq!(
            normalize_label("Caf\u{65}\u{301}", 64).unwrap(),
            "Caf\u{e9}"
        );
        assert!(is_nfc("Caf\u{e9}"));
        assert!(!is_nfc("Caf\u{65}\u{301}"));
    }

    #[test]
    fn label_rules_are_checked_in_order() {
        assert_eq!(normalize_label("", 64), Err(ValueError::Empty));
        assert_eq!(
            normalize_label(&"a".repeat(64), 64).map(|s| s.len()),
            Ok(64)
        );
        assert_eq!(
            normalize_label(&"a".repeat(65), 64),
            Err(ValueError::TooLong { max: 64, found: 65 })
        );
        assert_eq!(normalize_label(" a", 64), Err(ValueError::EdgeWhitespace));
        assert_eq!(normalize_label("a ", 64), Err(ValueError::EdgeWhitespace));
        assert_eq!(
            normalize_label("a\u{7}b", 64),
            Err(ValueError::ControlCharacter)
        );
        // Length counts scalar values after normalisation, not bytes.
        assert!(normalize_label(&"é".repeat(64), 64).is_ok());
        // PoC: A3-M-1 — invisible characters, at an edge or inside.
        for (text, found) in [
            ("Ca\u{200B}sh", '\u{200B}'),
            ("Cash\u{FEFF}", '\u{FEFF}'),
            ("Ca\u{200D}sh", '\u{200D}'),
            ("\u{202E}hsaC", '\u{202E}'),
            ("Ca\u{2028}sh", '\u{2028}'),
            ("Ca\u{00AD}sh", '\u{00AD}'),
            ("Ca\u{E0041}sh", '\u{E0041}'),
        ] {
            assert_eq!(
                normalize_label(text, 64),
                Err(ValueError::InvisibleCharacter { found }),
                "{text:?}"
            );
        }
    }
}
