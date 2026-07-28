//! Canonical binary encoding.
//!
//! Hashes are only meaningful if the bytes fed to them are reproducible. This
//! module defines a fixed, field-ordered, length-prefixed encoding with exactly
//! one representation per value.
//!
//! The rules:
//!
//! - Integers are little-endian, fixed width. No varints, no leading-zero
//!   ambiguity.
//! - Variable-length data carries a `u32` byte-length prefix, so no concatenation
//!   of two fields can be reinterpreted as a different split.
//! - `Option` is a `0x00` / `0x01` discriminant followed by the payload when set.
//! - Sequences carry a `u32` element count and are written in the order the
//!   producer chose; the producer is responsible for that order being canonical.
//!
//! General-purpose serialisation formats are deliberately not used here. Their
//! map ordering, integer widths, and string escaping are free to change between
//! versions, which would silently change every hash in a ledger.

/// Appends canonically encoded values to a byte buffer.
#[derive(Debug, Default)]
pub struct CanonicalWriter {
    buf: Vec<u8>,
}

impl CanonicalWriter {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Creates a writer with room for `cap` bytes.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Writes a single byte, typically a discriminant.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    /// Writes a `u16`.
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes a `u32`.
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes a `u64`.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes an `i64`.
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes a `u128`.
    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes a bool as `0x00` or `0x01`.
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.buf.push(u8::from(v));
        self
    }

    /// Writes a length-prefixed byte string.
    ///
    /// Inputs longer than `u32::MAX` are truncated in the prefix only in
    /// theory; no field in this crate approaches that bound, and the callers
    /// that build entries enforce much smaller limits.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        let len = u32::try_from(v.len()).unwrap_or(u32::MAX);
        self.u32(len);
        self.buf.extend_from_slice(v);
        self
    }

    /// Writes a length-prefixed UTF-8 string.
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    /// Writes a fixed-width byte array without a length prefix.
    ///
    /// Only valid for widths fixed by the schema, such as a digest or a UUID.
    pub fn fixed(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// Writes an optional value: `0x00`, or `0x01` followed by `f`.
    pub fn option<T: ?Sized>(&mut self, v: Option<&T>, f: impl FnOnce(&mut Self, &T)) -> &mut Self {
        match v {
            None => {
                self.u8(0);
            }
            Some(inner) => {
                self.u8(1);
                f(self, inner);
            }
        }
        self
    }

    /// Writes a counted sequence.
    pub fn seq<T>(
        &mut self,
        items: impl ExactSizeIterator<Item = T>,
        mut f: impl FnMut(&mut Self, T),
    ) -> &mut Self {
        let len = u32::try_from(items.len()).unwrap_or(u32::MAX);
        self.u32(len);
        for item in items {
            f(self, item);
        }
        self
    }

    /// Consumes the writer and returns the encoded bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// The bytes written so far.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// A value with a canonical byte encoding.
pub trait Canonical {
    /// Appends this value's canonical encoding to `w`.
    fn encode(&self, w: &mut CanonicalWriter);

    /// Returns this value's canonical encoding as a fresh buffer.
    #[must_use]
    fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut w = CanonicalWriter::new();
        self.encode(&mut w);
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_are_little_endian_fixed_width() {
        let mut w = CanonicalWriter::new();
        w.u32(1);
        assert_eq!(w.finish(), vec![1, 0, 0, 0]);
    }

    #[test]
    fn length_prefix_disambiguates_concatenation() {
        let mut a = CanonicalWriter::new();
        a.str("ab").str("c");

        let mut b = CanonicalWriter::new();
        b.str("a").str("bc");

        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn none_and_empty_are_distinct() {
        let mut none = CanonicalWriter::new();
        none.option(Option::<&str>::None, |w, v: &str| {
            w.str(v);
        });

        let mut empty = CanonicalWriter::new();
        empty.option(Some(""), |w, v: &str| {
            w.str(v);
        });

        assert_ne!(none.finish(), empty.finish());
    }

    #[test]
    fn sequence_counts_are_written() {
        let mut w = CanonicalWriter::new();
        w.seq([1u8, 2, 3].into_iter(), |w, v| {
            w.u8(v);
        });
        assert_eq!(w.finish(), vec![3, 0, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn empty_sequence_encodes_as_zero_count() {
        let mut w = CanonicalWriter::new();
        w.seq(core::iter::empty::<u8>(), |w, v| {
            w.u8(v);
        });
        assert_eq!(w.finish(), vec![0, 0, 0, 0]);
    }

    #[test]
    fn encoding_is_deterministic() {
        let build = || {
            let mut w = CanonicalWriter::new();
            w.u64(42)
                .str("hello")
                .bool(true)
                .option(Some(&7u8), |w, v| {
                    w.u8(*v);
                });
            w.finish()
        };
        assert_eq!(build(), build());
    }
}
