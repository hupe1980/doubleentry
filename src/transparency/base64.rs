//! Canonical base64 (RFC 4648 §4, standard alphabet, `=` padding), the
//! encoding every C2SP text format uses.
//!
//! Encoding is canonical by construction. Decoding refuses everything that
//! is not the canonical encoding of some bytes: characters outside the
//! alphabet, a length that is not a multiple of four, padding anywhere but
//! the last one or two positions of the last group, and a final group whose
//! unused bits are not zero (RFC 4648 §3.5). Two texts never decode to the
//! same bytes — which matters, because a signature covers the text.
//!
//! Every shift is by a constant below the operand's width, so the
//! `wrapping_*` forms are exact.

/// The 64 symbols, in value order.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The symbol of a six-bit value (`value & 0x3f` is taken first).
fn symbol(value: u8) -> char {
    let index = usize::from(value & 0x3f);
    ALPHABET.get(index).map_or('=', |byte| char::from(*byte))
}

/// The six-bit value of one symbol; `None` outside the alphabet.
const fn value_of(symbol: u8) -> Option<u8> {
    Some(match symbol {
        b'A'..=b'Z' => symbol.wrapping_sub(b'A'),
        b'a'..=b'z' => symbol.wrapping_sub(b'a').wrapping_add(26),
        b'0'..=b'9' => symbol.wrapping_sub(b'0').wrapping_add(52),
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

/// Encodes one group of one to three bytes as four symbols.
fn encode_group(b0: u8, b1: Option<u8>, b2: Option<u8>) -> [char; 4] {
    let (c1, c2) = (b1.unwrap_or(0), b2.unwrap_or(0));
    [
        symbol(b0.wrapping_shr(2)),
        symbol((b0 & 0x03).wrapping_shl(4) | c1.wrapping_shr(4)),
        b1.map_or('=', |_| {
            symbol((c1 & 0x0f).wrapping_shl(2) | c2.wrapping_shr(6))
        }),
        b2.map_or('=', |_| symbol(c2 & 0x3f)),
    ]
}

/// Decodes one group of four symbols into one to three bytes (`None`
/// marks a byte the padding stands in for), refusing a non-canonical group.
/// Padding is accepted here only where `last` allows it.
fn decode_group(group: [u8; 4], last: bool) -> Option<[Option<u8>; 3]> {
    let [s0, s1, s2, s3] = group;
    let (v0, v1) = (value_of(s0)?, value_of(s1)?);
    match (s2, s3) {
        (b'=', b'=') if last => {
            // One byte: the low four bits of the second symbol are unused.
            (v1 & 0x0f == 0).then_some([Some(v0.wrapping_shl(2) | v1.wrapping_shr(4)), None, None])
        }
        (_, b'=') if last => {
            let v2 = value_of(s2)?;
            // Two bytes: the low two bits of the third symbol are unused.
            (v2 & 0x03 == 0).then_some([
                Some(v0.wrapping_shl(2) | v1.wrapping_shr(4)),
                Some(v1.wrapping_shl(4) | v2.wrapping_shr(2)),
                None,
            ])
        }
        _ => {
            let (v2, v3) = (value_of(s2)?, value_of(s3)?);
            Some([
                Some(v0.wrapping_shl(2) | v1.wrapping_shr(4)),
                Some(v1.wrapping_shl(4) | v2.wrapping_shr(2)),
                Some(v2.wrapping_shl(6) | v3),
            ])
        }
    }
}

/// The canonical encoding of `input`.
pub(crate) fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3).saturating_mul(4));
    for chunk in input.chunks(3) {
        if let Some((&b0, rest)) = chunk.split_first() {
            out.extend(encode_group(
                b0,
                rest.first().copied(),
                rest.get(1).copied(),
            ));
        }
    }
    out
}

/// The bytes `text` canonically encodes; `None` for any other text. The
/// empty text is the encoding of no bytes.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let (groups, remainder) = text.as_bytes().as_chunks::<4>();
    if !remainder.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(groups.len().saturating_mul(3));
    let count = groups.len();
    for (at, group) in groups.iter().enumerate() {
        let last = at.saturating_add(1) == count;
        out.extend(decode_group(*group, last)?.into_iter().flatten());
    }
    Some(out)
}

/// The 32 bytes `text` canonically encodes; `None` for any other text.
pub(crate) fn decode_hash(text: &str) -> Option<[u8; 32]> {
    decode(text)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_text(group: [char; 4]) -> [u8; 4] {
        group.map(|c| c as u8)
    }

    #[test]
    fn every_three_byte_group_round_trips() {
        for value in 0u32..1 << 24 {
            let [_, b0, b1, b2] = value.to_be_bytes();
            let text = group_text(encode_group(b0, Some(b1), Some(b2)));
            assert_eq!(
                decode_group(text, true),
                Some([Some(b0), Some(b1), Some(b2)]),
                "{value:06x}"
            );
            assert_eq!(decode_group(text, false), decode_group(text, true));
        }
    }

    #[test]
    fn every_final_group_is_canonical_or_refused() {
        // Every 4-symbol text with padding: accepted exactly when it is the
        // encoding of what it decodes to.
        for a in ALPHABET {
            for b in ALPHABET {
                let one = [*a, *b, b'=', b'='];
                match decode_group(one, true) {
                    Some([Some(byte), None, None]) => {
                        assert_eq!(group_text(encode_group(byte, None, None)), one);
                    }
                    None => assert!(value_of(*b).unwrap() & 0x0f != 0),
                    other => panic!("{other:?}"),
                }
                assert_eq!(decode_group(one, false), None, "padding before the end");
                for c in ALPHABET {
                    let two = [*a, *b, *c, b'='];
                    match decode_group(two, true) {
                        Some([Some(x), Some(y), None]) => {
                            assert_eq!(group_text(encode_group(x, Some(y), None)), two);
                        }
                        None => assert!(value_of(*c).unwrap() & 0x03 != 0),
                        other => panic!("{other:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn matches_the_rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
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
    fn refuses_malformed_text() {
        for bad in [
            "Zg=", "Zg", "Z===", "Zm9v!", "Zg==Zg==", "=Zm9", "Zm=v", "Zh==", "Zm9=", "Zm9v\n",
            "Zm 9", "====",
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
        assert_eq!(decode_hash("Zm9v"), None);
    }
}
