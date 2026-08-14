//! Content hashing.
//!
//! Every hash the engine produces is domain-separated: the tag identifying *what*
//! is being hashed is mixed in before the payload. Two different structures can
//! therefore never collide by encoding to the same bytes, and a value hashed for
//! one purpose is not a valid hash for another.

use core::fmt;

/// A 256-bit BLAKE3 digest.
///
/// Displayed, serialised, and parsed as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash([u8; 32]);

#[cfg(feature = "serde")]
impl serde::Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        Self::parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

impl Hash {
    /// Width of a digest in bytes.
    pub const LEN: usize = 32;

    /// Wraps raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parses a 64-character lowercase or uppercase hex string.
    pub fn parse_hex(s: &str) -> Result<Self, ParseHashError> {
        if s.len() != 64 {
            return Err(ParseHashError::Length { got: s.len() });
        }
        let mut out = [0u8; 32];
        for (slot, pair) in out.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
            let [hi, lo] = pair else {
                return Err(ParseHashError::InvalidDigit);
            };
            *slot = hex_val(*hi)?.wrapping_shl(4) | hex_val(*lo)?;
        }
        Ok(Self(out))
    }

    /// Hashes `payload` under a caller-chosen domain separation tag.
    ///
    /// This is the same construction the engine uses for its own digests, with
    /// the tag length-prefixed so `domain || payload` is unambiguous. It is
    /// public because a caller needs it: [`DocumentRef::new`](crate::DocumentRef::new)
    /// takes the content hash of a source document, and the alternative to
    /// offering a construction is every caller inventing one — usually a bare
    /// SHA-256 with no domain separation, which is exactly the mistake this
    /// module exists to avoid.
    ///
    /// Pick a domain that names *your* document type, not this crate's. Tags
    /// beginning `doubleentry/` are reserved for the engine, and reusing one
    /// would let a document hash be mistaken for an entry hash.
    ///
    /// ```
    /// # use doubleentry::{DocumentRef, Hash};
    /// let pdf = b"%PDF-1.7 ...";
    /// let document = DocumentRef::new("INV-2026-0001", Hash::digest(b"acme/invoice/v1", pdf))?;
    /// assert!(document.is_verifiable());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn digest(domain: &[u8], payload: &[u8]) -> Self {
        tagged(domain, payload)
    }

    /// Renders as a 64-character lowercase hex string.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            // Two hex digits per byte; `write!` to a String cannot fail.
            s.push(hex_digit(b >> 4));
            s.push(hex_digit(b & 0x0f));
        }
        s
    }
}

const fn hex_digit(nibble: u8) -> char {
    match nibble & 0x0f {
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

const fn hex_val(c: u8) -> Result<u8, ParseHashError> {
    match c {
        b'0'..=b'9' => Ok(c.wrapping_sub(b'0')),
        b'a'..=b'f' => Ok(c.wrapping_sub(b'a').wrapping_add(10)),
        b'A'..=b'F' => Ok(c.wrapping_sub(b'A').wrapping_add(10)),
        _ => Err(ParseHashError::InvalidDigit),
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

/// Failure parsing a [`struct@Hash`] from hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseHashError {
    /// The input was not exactly 64 characters.
    #[error("expected 64 hex characters, got {got}")]
    Length {
        /// Length actually supplied.
        got: usize,
    },
    /// The input contained a character outside `[0-9a-fA-F]`.
    #[error("invalid hex digit")]
    InvalidDigit,
}

/// Domain separation tags.
///
/// Every tag is a distinct byte prefix. The encoding version is folded into the
/// entry tag so that a change to the canonical encoding produces a different
/// hash rather than silently reinterpreting old bytes.
pub(crate) mod tag {
    /// Canonically encoded entry, encoding version 1.
    pub(crate) const ENTRY_V1: &[u8] = b"doubleentry/entry/v1";
    /// Account path.
    pub(crate) const ACCOUNT_V1: &[u8] = b"doubleentry/account/v1";
    /// Period seal.
    pub(crate) const SEAL_V1: &[u8] = b"doubleentry/seal/v1";
    /// One row of a trial-balance commitment.
    pub(crate) const TRIAL_BALANCE_V1: &[u8] = b"doubleentry/trialbalance/v1";
    /// One handle-to-account binding in the registry commitment.
    pub(crate) const ACCOUNT_BINDING_V1: &[u8] = b"doubleentry/accountbinding/v1";
}

/// The prefix every tag this crate uses starts with.
///
/// Callers of [`Hash::digest`] should stay out of this namespace so a document
/// digest can never be mistaken for one of the engine's own.
pub const RESERVED_DOMAIN_PREFIX: &[u8] = b"doubleentry/";

/// Hashes `payload` under a domain separation `tag`.
#[must_use]
pub(crate) fn tagged(tag: &[u8], payload: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    // Length-prefix the tag so `tag || payload` is unambiguous.
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag);
    hasher.update(payload);
    Hash(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let h = tagged(b"test", b"payload");
        let parsed = Hash::parse_hex(&h.to_hex()).expect("valid hex");
        assert_eq!(h, parsed);
    }

    #[test]
    fn hex_is_lowercase_and_64_chars() {
        let h = tagged(b"test", b"payload");
        let s = h.to_hex();
        assert_eq!(s.len(), 64);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(matches!(
            Hash::parse_hex("abcd"),
            Err(ParseHashError::Length { got: 4 })
        ));
    }

    #[test]
    fn parse_rejects_non_hex() {
        let bad = "z".repeat(64);
        assert!(matches!(
            Hash::parse_hex(&bad),
            Err(ParseHashError::InvalidDigit)
        ));
    }

    #[test]
    fn parse_accepts_uppercase() {
        let h = tagged(b"test", b"payload");
        let upper = h.to_hex().to_uppercase();
        assert_eq!(Hash::parse_hex(&upper).expect("valid"), h);
    }

    #[test]
    fn different_tags_produce_different_hashes() {
        assert_ne!(tagged(b"a", b"payload"), tagged(b"b", b"payload"));
    }

    #[test]
    fn tag_length_prefix_prevents_boundary_collision() {
        // Without the length prefix, ("ab", "c") and ("a", "bc") would collide.
        assert_ne!(tagged(b"ab", b"c"), tagged(b"a", b"bc"));
    }

    #[test]
    fn the_public_digest_is_the_engines_own_construction() {
        assert_eq!(
            Hash::digest(b"acme/doc/v1", b"x"),
            tagged(b"acme/doc/v1", b"x")
        );
    }

    #[test]
    fn every_engine_tag_lives_under_the_reserved_prefix() {
        // A caller told to stay out of this namespace can only do so if the
        // engine actually stays inside it.
        for t in [
            tag::ENTRY_V1,
            tag::ACCOUNT_V1,
            tag::SEAL_V1,
            tag::TRIAL_BALANCE_V1,
            tag::ACCOUNT_BINDING_V1,
        ] {
            assert!(
                t.starts_with(RESERVED_DOMAIN_PREFIX),
                "{:?} escapes the reserved prefix",
                std::str::from_utf8(t)
            );
        }
    }
}
