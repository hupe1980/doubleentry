//! The canonical writer and reader: one byte form per value, never serde.
//!
//! Every committed value implements [`Encode`], written once against a
//! [`Sink`]: a `Vec<u8>` collects the bytes, a [`Count`] only measures them,
//! so a size check never allocates and never disagrees with the encoding.
//! [`Reader`] is the strict inverse: it refuses truncation, trailing bytes,
//! invalid UTF-8, unknown tags and over-long counts before allocating.

use core::fmt;

use time::Date;

use super::ValueError;

/// The largest leaf a record may encode to, in bytes (kind byte included).
///
/// The tlog-tiles entry-bundle bound. A constructor refuses a larger record
/// ([`ValueError::TooLarge`]) and [`Record::decode`](super::Record::decode)
/// refuses longer input before reading it.
pub const MAX_LEAF_BYTES: usize = 65_535;

/// Why leaf bytes could not be decoded into a record.
///
/// The decoder never panics and never allocates more than the input length
/// implies; every refusal names what was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The input ended before a field was complete.
    #[error("truncated: {needed} more bytes needed, {remaining} remaining")]
    Truncated {
        /// Bytes the field needs.
        needed: u64,
        /// Bytes left in the input.
        remaining: u64,
    },
    /// Bytes are left over after the record.
    #[error("{count} trailing bytes after the record")]
    TrailingBytes {
        /// How many bytes are left.
        count: u64,
    },
    /// The kind byte is not a log record kind.
    #[error("unknown record kind 0x{0:02x}")]
    UnknownKind(u8),
    /// A tag byte (option, action, field, side, …) has no meaning.
    #[error("invalid tag 0x{tag:02x} for {field}")]
    InvalidTag {
        /// The field being decoded.
        field: &'static str,
        /// The byte found.
        tag: u8,
    },
    /// Text that is not UTF-8.
    #[error("{field} is not valid UTF-8")]
    InvalidUtf8 {
        /// The field being decoded.
        field: &'static str,
    },
    /// Text that is not in Unicode NFC.
    #[error("{field} is not in Unicode NFC")]
    NotNfc {
        /// The field being decoded.
        field: &'static str,
    },
    /// A Julian day outside the supported date range.
    #[error("Julian day {julian_day} is not a supported date")]
    InvalidDate {
        /// The day number found.
        julian_day: i32,
    },
    /// A count above the format's maximum.
    #[error("{field} has {found} elements, more than the maximum of {max}")]
    TooMany {
        /// The field being decoded.
        field: &'static str,
        /// The maximum.
        max: u64,
        /// The count found.
        found: u64,
    },
    /// A keyed sequence that is not strictly ascending, or a value that is
    /// not in its one canonical form.
    #[error("{field} is not in canonical form")]
    NonCanonical {
        /// The field being decoded.
        field: &'static str,
    },
    /// A field that its value constructor refuses.
    #[error("{field}: {error}")]
    Value {
        /// The field being decoded.
        field: &'static str,
        /// Why the constructor refused it.
        error: ValueError,
    },
    /// A genesis record whose scale differs from the decoding type's `P`.
    #[error("the ledger has scale {found}, the reader expects {expected}")]
    PrecisionMismatch {
        /// The decoding type's `P`.
        expected: u8,
        /// The scale in the genesis record.
        found: u8,
    },
    /// A genesis record whose format tag is not `doubleentry/1`.
    #[error("unknown format tag")]
    UnknownFormat,
    /// Input longer than [`MAX_LEAF_BYTES`].
    #[error("{found} bytes exceed the maximum leaf size of {max}")]
    TooLarge {
        /// [`MAX_LEAF_BYTES`].
        max: usize,
        /// The input length.
        found: usize,
    },
}

/// Where a [`Writer`] puts bytes.
pub(crate) trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// A sink that only counts, saturating at `usize::MAX`.
#[derive(Debug, Default)]
pub(crate) struct Count(pub(crate) usize);

impl Sink for Count {
    fn put(&mut self, bytes: &[u8]) {
        self.0 = self.0.saturating_add(bytes.len());
    }
}

/// The canonical writer.
pub(crate) struct Writer<S> {
    sink: S,
}

/// A `usize` length or count as the format's `u64` (lossless on every
/// supported target, where `usize` has at most 64 bits).
fn wide(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

impl<S: Sink> Writer<S> {
    pub(crate) const fn new(sink: S) -> Self {
        Self { sink }
    }

    pub(crate) fn into_inner(self) -> S {
        self.sink
    }

    pub(crate) fn fixed(&mut self, bytes: &[u8]) {
        self.sink.put(bytes);
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.fixed(&[value]);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn i8(&mut self, value: i8) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn i16(&mut self, value: i16) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.fixed(&value.to_le_bytes());
    }

    pub(crate) fn i128(&mut self, value: i128) {
        self.fixed(&value.to_le_bytes());
    }

    /// A `u64` length or count prefix.
    pub(crate) fn len(&mut self, len: usize) {
        self.u64(wide(len));
    }

    /// `str(x)`: `u64` byte length ‖ bytes.
    pub(crate) fn str(&mut self, text: &str) {
        self.len(text.len());
        self.fixed(text.as_bytes());
    }

    /// `Date`: the `i32` Julian day number.
    pub(crate) fn date(&mut self, date: Date) {
        self.i32(date.to_julian_day());
    }

    pub(crate) fn value<T: Encode + ?Sized>(&mut self, value: &T) {
        value.encode(self);
    }

    /// `opt(x)`: `0x00` | `0x01 ‖ x`.
    pub(crate) fn opt<T: Encode + ?Sized>(&mut self, value: Option<&T>) {
        match value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                value.encode(self);
            }
        }
    }

    /// `seq(x)`: `u64` count ‖ elements.
    pub(crate) fn seq<T: Encode>(&mut self, items: &[T]) {
        self.len(items.len());
        for item in items {
            item.encode(self);
        }
    }
}

/// A value with one canonical byte form.
pub(crate) trait Encode {
    fn encode<S: Sink>(&self, w: &mut Writer<S>);

    /// The encoded length, measured without allocating.
    fn encoded_len(&self) -> usize {
        let mut w = Writer::new(Count::default());
        self.encode(&mut w);
        w.into_inner().0
    }

    /// The encoding, in one allocation of the exact length.
    fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new(Vec::with_capacity(self.encoded_len()));
        self.encode(&mut w);
        w.into_inner()
    }
}

impl Encode for Date {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.date(*self);
    }
}

impl Encode for str {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.str(self);
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        (**self).encode(w);
    }
}

/// A slice encodes as `seq(x)`.
impl<T: Encode> Encode for [T] {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.seq(self);
    }
}

impl Encode for u32 {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.u32(*self);
    }
}

impl<const N: usize> Encode for [u8; N] {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.fixed(self);
    }
}

/// A value that can be read back from its canonical form.
pub(crate) trait Decode: Sized {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError>;
}

/// The strict canonical reader.
pub(crate) struct Reader<'a> {
    input: &'a [u8],
}

macro_rules! read_int {
    ($($name:ident: $ty:ty),* $(,)?) => {
        $(
            pub(crate) fn $name(&mut self) -> Result<$ty, DecodeError> {
                self.array().map(<$ty>::from_le_bytes)
            }
        )*
    };
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(input: &'a [u8]) -> Self {
        Self { input }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.input.len()
    }

    fn truncated(&self, needed: u64) -> DecodeError {
        DecodeError::Truncated {
            needed,
            remaining: wide(self.input.len()),
        }
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let (head, rest) = self
            .input
            .split_at_checked(len)
            .ok_or_else(|| self.truncated(wide(len)))?;
        self.input = rest;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (head, rest) = self
            .input
            .split_first_chunk::<N>()
            .ok_or_else(|| self.truncated(wide(N)))?;
        self.input = rest;
        Ok(*head)
    }

    read_int!(u8: u8, u16: u16, u32: u32, u64: u64, i8: i8, i16: i16, i32: i32, i64: i64, i128: i128);

    /// `str(x)` as UTF-8, refusing a length beyond the remaining input before
    /// touching it.
    pub(crate) fn str(&mut self, field: &'static str) -> Result<&'a str, DecodeError> {
        let len = self.u64()?;
        let len = usize::try_from(len)
            .ok()
            .filter(|len| *len <= self.input.len())
            .ok_or_else(|| self.truncated(len))?;
        core::str::from_utf8(self.take(len)?).map_err(|_| DecodeError::InvalidUtf8 { field })
    }

    /// `str(x)` that must already be in NFC.
    pub(crate) fn nfc_str(&mut self, field: &'static str) -> Result<&'a str, DecodeError> {
        let text = self.str(field)?;
        if super::is_nfc(text) {
            Ok(text)
        } else {
            Err(DecodeError::NotNfc { field })
        }
    }

    pub(crate) fn date(&mut self) -> Result<Date, DecodeError> {
        let julian_day = self.i32()?;
        Date::from_julian_day(julian_day).map_err(|_| DecodeError::InvalidDate { julian_day })
    }

    pub(crate) fn value<T: Decode>(&mut self, field: &'static str) -> Result<T, DecodeError> {
        T::decode(self, field)
    }

    /// `opt(x)`: refuses any tag other than `0x00` and `0x01`.
    pub(crate) fn opt<T>(
        &mut self,
        field: &'static str,
        read: impl FnOnce(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<Option<T>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => read(self).map(Some),
            tag => Err(DecodeError::InvalidTag { field, tag }),
        }
    }

    /// A `u64` count, refused above `max` before anything is allocated.
    pub(crate) fn count(&mut self, field: &'static str, max: usize) -> Result<usize, DecodeError> {
        let found = self.u64()?;
        usize::try_from(found)
            .ok()
            .filter(|count| *count <= max)
            .ok_or(DecodeError::TooMany {
                field,
                max: wide(max),
                found,
            })
    }

    /// Refuses trailing bytes.
    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes {
                count: wide(self.input.len()),
            })
        }
    }
}

impl Decode for Date {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        r.date()
    }
}

impl<const N: usize> Decode for [u8; N] {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        r.array()
    }
}

/// Writes `text` honouring the formatter's width, fill and alignment (left
/// by default, as for strings) and ignoring its precision: a value is never
/// cut short.
pub(crate) fn pad(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    if f.precision().is_none() {
        return f.pad(text);
    }
    let missing = f.width().unwrap_or(0).saturating_sub(text.chars().count());
    let (before, after) = match f.align() {
        Some(fmt::Alignment::Right) => (missing, 0),
        Some(fmt::Alignment::Center) => (missing / 2, missing.saturating_sub(missing / 2)),
        Some(fmt::Alignment::Left) | None => (0, missing),
    };
    let fill = f.fill();
    for _ in 0..before {
        f.write_fmt(format_args!("{fill}"))?;
    }
    f.write_str(text)?;
    for _ in 0..after {
        f.write_fmt(format_args!("{fill}"))?;
    }
    Ok(())
}

/// Lowercase hexadecimal for identifiers and hashes.
pub(crate) mod hex {
    use core::fmt;

    use super::ValueError;

    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    /// Writes `bytes` as lowercase hex.
    pub(crate) fn write(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
        let text: String = bytes
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .filter_map(|nibble| DIGITS.get(usize::from(nibble)).map(|d| char::from(*d)))
            .collect();
        super::pad(f, &text)
    }

    fn nibble(digit: u8) -> Option<u8> {
        match digit {
            b'0'..=b'9' => digit.checked_sub(b'0'),
            b'a'..=b'f' => digit.checked_sub(b'a' - 10),
            b'A'..=b'F' => digit.checked_sub(b'A' - 10),
            _ => None,
        }
    }

    /// Parses an even number of hex digits of either case.
    #[cfg(feature = "serde")]
    pub(crate) fn parse_vec(text: &str) -> Option<Vec<u8>> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| match pair {
                [high, low] => Some((nibble(*high)? << 4) | nibble(*low)?),
                _ => None,
            })
            .collect()
    }

    /// Parses exactly `2 N` hex digits of either case.
    pub(crate) fn parse<const N: usize>(text: &str) -> Result<[u8; N], ValueError> {
        let digits = text.as_bytes();
        if digits.len() != N.saturating_mul(2) {
            return Err(ValueError::InvalidHex);
        }
        let mut out = [0u8; N];
        let (pairs, _) = digits.as_chunks::<2>();
        for (slot, [high, low]) in out.iter_mut().zip(pairs) {
            let (Some(high), Some(low)) = (nibble(*high), nibble(*low)) else {
                return Err(ValueError::InvalidHex);
            };
            *slot = (high << 4) | low;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use time::macros::date;

    use super::*;

    fn bytes(write: impl FnOnce(&mut Writer<Vec<u8>>)) -> Vec<u8> {
        let mut w = Writer::new(Vec::new());
        write(&mut w);
        w.into_inner()
    }

    #[test]
    fn every_primitive_round_trips() {
        let encoded = bytes(|w| {
            w.u8(0xab);
            w.u16(0xbeef);
            w.u32(0xdead_beef);
            w.u64(u64::MAX - 1);
            w.i8(-2);
            w.i16(-300);
            w.i32(-70_000);
            w.i64(i64::MIN);
            w.i128(i128::MAX);
            w.str("Café");
            w.opt::<str>(None);
            w.opt(Some("x"));
            w.date(date!(2026 - 01 - 01));
        });
        let mut r = Reader::new(&encoded);
        assert_eq!(r.u8().unwrap(), 0xab);
        assert_eq!(r.u16().unwrap(), 0xbeef);
        assert_eq!(r.u32().unwrap(), 0xdead_beef);
        assert_eq!(r.u64().unwrap(), u64::MAX - 1);
        assert_eq!(r.i8().unwrap(), -2);
        assert_eq!(r.i16().unwrap(), -300);
        assert_eq!(r.i32().unwrap(), -70_000);
        assert_eq!(r.i64().unwrap(), i64::MIN);
        assert_eq!(r.i128().unwrap(), i128::MAX);
        assert_eq!(r.str("text").unwrap(), "Café");
        assert_eq!(r.opt("opt", |r| r.str("x")).unwrap(), None);
        assert_eq!(r.opt("opt", |r| r.str("x")).unwrap(), Some("x"));
        assert_eq!(r.date().unwrap(), date!(2026 - 01 - 01));
        r.finish().unwrap();
    }

    #[test]
    fn integers_are_little_endian_and_dates_are_julian_days() {
        assert_eq!(bytes(|w| w.u32(1)), [1, 0, 0, 0]);
        assert_eq!(bytes(|w| w.str("ab")), [2, 0, 0, 0, 0, 0, 0, 0, b'a', b'b']);
        // 2026-01-01 is Julian day 2 461 042.
        assert_eq!(
            bytes(|w| w.date(date!(2026 - 01 - 01))),
            [0x72, 0x8d, 0x25, 0x00]
        );
    }

    #[test]
    fn the_count_sink_measures_without_storing() {
        let mut w = Writer::new(Count::default());
        w.str("hello");
        w.i128(0);
        assert_eq!(w.into_inner().0, 8 + 5 + 16);
    }

    #[test]
    fn truncation_and_trailing_bytes_are_refused() {
        assert_eq!(
            Reader::new(&[1, 2, 3]).u32(),
            Err(DecodeError::Truncated {
                needed: 4,
                remaining: 3
            })
        );
        assert_eq!(
            Reader::new(&[1]).finish(),
            Err(DecodeError::TrailingBytes { count: 1 })
        );
    }

    #[test]
    fn a_huge_string_length_is_refused_without_allocating() {
        let encoded = bytes(|w| w.u64(u64::MAX));
        assert_eq!(
            Reader::new(&encoded).str("text"),
            Err(DecodeError::Truncated {
                needed: u64::MAX,
                remaining: 0
            })
        );
        let encoded = bytes(|w| w.u64(10));
        assert!(matches!(
            Reader::new(&encoded).str("text"),
            Err(DecodeError::Truncated { needed: 10, .. })
        ));
    }

    #[test]
    fn invalid_utf8_and_non_nfc_text_are_refused() {
        let encoded = bytes(|w| {
            w.len(1);
            w.u8(0xff);
        });
        assert_eq!(
            Reader::new(&encoded).str("label"),
            Err(DecodeError::InvalidUtf8 { field: "label" })
        );
        let encoded = bytes(|w| w.str("Cafe\u{301}"));
        assert_eq!(
            Reader::new(&encoded).nfc_str("label"),
            Err(DecodeError::NotNfc { field: "label" })
        );
    }

    #[test]
    fn an_option_tag_other_than_zero_or_one_is_refused() {
        assert_eq!(
            Reader::new(&[2]).opt("memo", |r| r.u8()),
            Err(DecodeError::InvalidTag {
                field: "memo",
                tag: 2
            })
        );
    }

    #[test]
    fn a_julian_day_outside_the_date_range_is_refused() {
        let encoded = bytes(|w| w.i32(i32::MAX));
        assert_eq!(
            Reader::new(&encoded).date(),
            Err(DecodeError::InvalidDate {
                julian_day: i32::MAX
            })
        );
    }

    #[test]
    fn counts_above_the_maximum_are_refused() {
        let encoded = bytes(|w| w.u64(9));
        assert_eq!(
            Reader::new(&encoded).count("dimensions", 8),
            Err(DecodeError::TooMany {
                field: "dimensions",
                max: 8,
                found: 9
            })
        );
    }

    #[test]
    fn hex_parses_either_case_and_prints_lowercase() {
        assert_eq!(hex::parse::<2>("aBcD"), Ok([0xab, 0xcd]));
        assert_eq!(hex::parse::<2>("abc"), Err(ValueError::InvalidHex));
        assert_eq!(hex::parse::<2>("abcg"), Err(ValueError::InvalidHex));
        struct Show([u8; 2]);
        impl core::fmt::Display for Show {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                hex::write(f, &self.0)
            }
        }
        assert_eq!(Show([0xab, 0x01]).to_string(), "ab01");
    }
}
