//! Standard base64, because the note format is specified in it.
//!
//! Hand-rolled for the same reason the hex codecs in [`hash`](crate::hash) and
//! [`entry`](crate::entry) are: it is forty lines, it is exercised by every
//! round-trip test in this module, and a dependency here would be a dependency
//! in the dependency tree of everyone who never touches a witness.
//!
//! The standard alphabet with padding — RFC 4648 §4 — which is what Go's
//! `base64.StdEncoding` produces and therefore what a C2SP note contains.
//! Decoding is strict: the alphabet, the padding position and the total length
//! are all checked, so exactly one encoding of a given byte string is accepted.
//! A permissive decoder would let two different note texts carry the same root,
//! and a signature covers the text.

// Shifts and masks over a `u32` assembled from three `u8`s. Every shift is a
// literal below 24 and every operand is bounded by construction, so none of this
// can wrap or panic; routing it through `checked_shl` would return an `Option`
// that is never `None` and obscure the correspondence with the RFC.
#![allow(clippy::arithmetic_side_effects)]

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const PAD: char = '=';

/// Encodes `input` as standard base64 with padding.
pub(crate) fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3).saturating_mul(4));
    for chunk in input.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let packed =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));
        let symbol = |shift: u32| {
            let index = ((packed >> shift) & 0x3f) as usize;
            ALPHABET.get(index).copied().map_or(PAD, char::from)
        };
        out.push(symbol(18));
        out.push(symbol(12));
        out.push(if b1.is_some() { symbol(6) } else { PAD });
        out.push(if b2.is_some() { symbol(0) } else { PAD });
    }
    out
}

/// The value of one alphabet character, or `None` for anything else.
const fn value_of(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => (c - b'A') as u32,
        b'a'..=b'z' => (c - b'a') as u32 + 26,
        b'0'..=b'9' => (c - b'0') as u32 + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

/// Decodes standard base64 with padding, strictly.
///
/// Returns `None` for a length that is not a multiple of four, a character
/// outside the alphabet, padding anywhere but the last one or two positions, or
/// a final group whose unused bits are not zero. That last one matters: without
/// it, several distinct texts decode to the same bytes, and a note's signature
/// covers the text rather than the bytes.
pub(crate) fn decode(input: &str) -> Option<Vec<u8>> {
    // A length that is not a positive multiple of four leaves a remainder or no
    // groups at all, so the two length rules are one check on the split.
    let (groups, remainder) = input.as_bytes().as_chunks::<4>();
    if groups.is_empty() || !remainder.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(groups.len() * 3);
    let final_group = groups.len() - 1;
    for (at, &[c0, c1, c2, c3]) in groups.iter().enumerate() {
        let last = at == final_group;
        // Padding is only ever in the final group, and only in the last two
        // positions. Anywhere else it is a second encoding of the same bytes.
        let pad = usize::from(c2 == b'=') + usize::from(c3 == b'=');
        if pad > 0 && (!last || c2 == b'=' && c3 != b'=') {
            return None;
        }
        let v0 = value_of(c0)?;
        let v1 = value_of(c1)?;
        let v2 = if c2 == b'=' { 0 } else { value_of(c2)? };
        let v3 = if c3 == b'=' { 0 } else { value_of(c3)? };
        let packed = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;

        // The bits the padding stands in for must be zero, or two texts decode
        // alike.
        let unused_mask = match pad {
            1 => 0x0000_00ff,
            2 => 0x0000_ffff,
            _ => 0,
        };
        if packed & unused_mask != 0 {
            return None;
        }

        out.push(((packed >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((packed >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((packed & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length_up_to_a_few_groups() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(37)).collect();
            let text = encode(&bytes);
            if len == 0 {
                assert_eq!(text, "");
                continue;
            }
            assert_eq!(
                decode(&text).as_deref(),
                Some(bytes.as_slice()),
                "len {len}"
            );
        }
    }

    #[test]
    fn matches_the_rfc_4648_test_vectors() {
        for (plain, encoded) in [
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded).as_deref(), Some(plain.as_bytes()));
        }
    }

    #[test]
    fn refuses_a_second_encoding_of_the_same_bytes() {
        // `Zg==` is "f". `Zh==` would decode to the same byte with non-zero
        // padding bits, so a lax decoder accepts two texts for one value — and a
        // note's signature covers the text.
        assert_eq!(decode("Zg=="), Some(b"f".to_vec()));
        assert_eq!(decode("Zh=="), None);
        assert_eq!(decode("Zm8="), Some(b"fo".to_vec()));
        assert_eq!(decode("Zm9="), None);
    }

    #[test]
    fn refuses_malformed_input() {
        for bad in ["Zg=", "Zg", "Z===", "Zm9v!", "Zg==Zg==", "=Zm9", "Zm=v"] {
            assert_eq!(decode(bad), None, "accepted {bad:?}");
        }
    }
}
