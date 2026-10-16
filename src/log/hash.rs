//! RFC 6962 SHA-256 hashing and the crate's tagged hashes.

use core::fmt;
use core::str::FromStr;

use sha2::{Digest, Sha256};

use crate::model::{Decode, DecodeError, Encode, Reader, Sink, ValueError, Writer, hex};

/// The tag of the [`EntryId`](crate::EntryId) derivation.
pub(crate) const TAG_ENTRY_ID: &str = "doubleentry/1/entry-id";
/// The tag of the [`MemoCommitment`](crate::record::MemoCommitment).
pub(crate) const TAG_MEMO: &str = "doubleentry/1/memo";
/// The tag of the draft [`Fingerprint`](crate::record::Fingerprint).
pub(crate) const TAG_FINGERPRINT: &str = "doubleentry/1/fingerprint";

/// A SHA-256 digest: a leaf hash, a node hash or a root.
///
/// `Display` prints 64 lowercase hex digits; `FromStr` accepts 64 hex digits
/// of either case.
///
/// ```
/// use doubleentry::{log::{Hash, empty_root}};
///
/// let root: Hash = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855".parse()?;
/// assert_eq!(root, empty_root());
/// assert_eq!(root.to_string(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Wraps 32 raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32 raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex::write(f, &self.0)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

impl FromStr for Hash {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        hex::parse(text).map(Self)
    }
}

impl Encode for Hash {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.fixed(&self.0);
    }
}

impl Decode for Hash {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        r.array().map(Self)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::model::serde_str::deserialize(deserializer)
    }
}

/// SHA-256 over the concatenation of `parts` — the one place a digest is
/// computed, so tests can count invocations.
fn sha256(parts: &[&[u8]]) -> Hash {
    #[cfg(test)]
    counter::bump();
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    Hash(hasher.finalize().into())
}

/// The RFC 6962 leaf hash: `SHA-256(0x00 ‖ leaf_bytes)`.
///
/// ```
/// use doubleentry::log::leaf_hash;
///
/// assert_eq!(
///     leaf_hash(b"").to_string(),
///     "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
/// );
/// ```
#[must_use]
pub fn leaf_hash(leaf_bytes: &[u8]) -> Hash {
    sha256(&[&[0x00], leaf_bytes])
}

/// The RFC 6962 interior node hash: `SHA-256(0x01 ‖ left ‖ right)`.
#[must_use]
pub fn node_hash(left: &Hash, right: &Hash) -> Hash {
    sha256(&[&[0x01], &left.0, &right.0])
}

/// The root of the empty tree: `SHA-256("")`.
#[must_use]
pub fn empty_root() -> Hash {
    sha256(&[])
}

/// The RFC 6962 Merkle Tree Hash of a sequence of leaf hashes (§2.1: split at
/// the largest power of two below `n`).
///
/// A direct, recursive definition — the reference the incremental
/// [`Tree`](crate::store::Tree) and the balance trees are tested against.
///
/// ```
/// use doubleentry::{log::{leaf_hash, node_hash, root_of}};
///
/// let (a, b) = (leaf_hash(b"a"), leaf_hash(b"b"));
/// assert_eq!(root_of(&[a, b]), node_hash(&a, &b));
/// ```
#[must_use]
pub fn root_of(leaf_hashes: &[Hash]) -> Hash {
    match leaf_hashes {
        [] => empty_root(),
        [leaf] => *leaf,
        _ => {
            let split = largest_power_of_two_below(leaf_hashes.len());
            let (left, right) = leaf_hashes.split_at(split.min(leaf_hashes.len()));
            node_hash(&root_of(left), &root_of(right))
        }
    }
}

/// The largest power of two strictly below `n`, for `n ≥ 2`.
fn largest_power_of_two_below(n: usize) -> usize {
    n.saturating_sub(1)
        .checked_ilog2()
        .map_or(1, |exponent| 1usize.rotate_left(exponent))
}

/// `tagged(tag, payload) = SHA-256(u64_le(len(tag)) ‖ tag ‖ payload)`.
///
/// The first preimage byte is the tag length (2–255 for every tag here), so a
/// tagged preimage never starts with the RFC 6962 `0x00`/`0x01` prefixes.
pub(crate) fn tagged(tag: &'static str, payload: &[u8]) -> Hash {
    let len = u64::try_from(tag.len()).unwrap_or(u64::MAX);
    sha256(&[&len.to_le_bytes(), tag.as_bytes(), payload])
}

/// A per-thread count of SHA-256 invocations, for the balance-tree cost bound.
#[cfg(test)]
pub(crate) mod counter {
    use std::cell::Cell;

    thread_local! {
        static HASHES: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn bump() {
        HASHES.with(|count| count.set(count.get() + 1));
    }

    /// Runs `f` and returns its result with the number of hashes it computed.
    pub(crate) fn count<T>(f: impl FnOnce() -> T) -> (T, u64) {
        let before = HASHES.with(Cell::get);
        let result = f();
        (result, HASHES.with(Cell::get) - before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6962_constants() {
        assert_eq!(
            empty_root().to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            leaf_hash(b"").to_string(),
            "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d"
        );
    }

    #[test]
    fn node_preimage_starts_with_one() {
        let (l, r) = (leaf_hash(b"l"), leaf_hash(b"r"));
        let mut hasher = Sha256::new();
        hasher.update([0x01]);
        hasher.update(l.0);
        hasher.update(r.0);
        assert_eq!(node_hash(&l, &r), Hash(hasher.finalize().into()));
    }

    #[test]
    fn tagged_preimages_start_with_the_tag_length() {
        for tag in [TAG_ENTRY_ID, TAG_MEMO, TAG_FINGERPRINT] {
            assert!((2..=255).contains(&tag.len()), "{tag}");
            let mut hasher = Sha256::new();
            hasher.update((tag.len() as u64).to_le_bytes());
            hasher.update(tag.as_bytes());
            hasher.update(b"payload");
            assert_eq!(tagged(tag, b"payload"), Hash(hasher.finalize().into()));
        }
    }

    #[test]
    fn hex_round_trips_in_either_case() {
        let hash = leaf_hash(b"x");
        assert_eq!(hash.to_string().parse::<Hash>(), Ok(hash));
        assert_eq!(hash.to_string().to_uppercase().parse::<Hash>(), Ok(hash));
        assert_eq!("00".parse::<Hash>(), Err(ValueError::InvalidHex));
        assert_eq!(format!("{hash:?}"), format!("Hash({hash})"));
    }

    #[test]
    fn split_points_are_the_largest_power_of_two_below() {
        for (n, k) in [
            (2, 1),
            (3, 2),
            (4, 2),
            (5, 4),
            (8, 4),
            (9, 8),
            (1 << 20, 1 << 19),
        ] {
            assert_eq!(largest_power_of_two_below(n), k, "n = {n}");
        }
    }
}
