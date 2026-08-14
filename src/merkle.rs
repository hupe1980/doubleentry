//! Append-only Merkle log with inclusion and consistency proofs.
//!
//! The tree structure, proof construction, and proof verification follow the
//! append-only log described in RFC 6962 and RFC 9162 (Certificate
//! Transparency), with BLAKE3 in place of SHA-256 and an additional domain
//! separation tag on every node. Because the hash function differs, the
//! published Certificate Transparency test vectors do not apply; the golden
//! vectors committed with this crate serve the same purpose.
//!
//! Two properties matter, and they are why this structure is used rather than a
//! chained hash over entries:
//!
//! - **Concurrency.** A leaf hash depends only on its own entry, so leaves can be
//!   computed in parallel by unrelated writers. Only the sequencing step is
//!   ordered, and it is cheap.
//! - **Selective disclosure.** An inclusion proof establishes that one entry sits
//!   at one index under a published root, in `O(log n)` hashes, without revealing
//!   any other entry. A chained hash cannot do this.
//!
//! A consistency proof establishes that one tree is a prefix of another — that
//! the log was appended to and never rewritten.

// The index arithmetic below is bounded by the recursion invariants (`k < n`,
// `m < n`), and slices are split at indices derived from those bounds. Checked
// arithmetic here would obscure the correspondence with the published algorithms
// without making them safer.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use crate::hash::{Hash, tagged};

const TAG_EMPTY: &[u8] = b"doubleentry/merkle/empty/v1";
const TAG_LEAF: &[u8] = b"doubleentry/merkle/leaf/v1";
const TAG_NODE: &[u8] = b"doubleentry/merkle/node/v1";

/// The root hash of an empty log.
#[must_use]
pub fn empty_root() -> Hash {
    tagged(TAG_EMPTY, &[])
}

/// Hashes a leaf's payload into a leaf node.
#[must_use]
pub fn leaf_hash(payload: &Hash) -> Hash {
    tagged(TAG_LEAF, payload.as_bytes())
}

/// Combines two child hashes into an interior node.
#[must_use]
fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut buf = [0u8; Hash::LEN * 2];
    buf[..Hash::LEN].copy_from_slice(left.as_bytes());
    buf[Hash::LEN..].copy_from_slice(right.as_bytes());
    tagged(TAG_NODE, &buf)
}

/// The largest power of two strictly less than `n`, for `n > 1`.
fn split_point(n: usize) -> usize {
    debug_assert!(n > 1);
    let mut k = 1usize;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// The Merkle Tree Hash of a run of leaf payloads.
fn mth(leaves: &[Hash]) -> Hash {
    match leaves.len() {
        0 => empty_root(),
        1 => leaf_hash(&leaves[0]),
        n => {
            let k = split_point(n);
            node_hash(&mth(&leaves[..k]), &mth(&leaves[k..]))
        }
    }
}

/// A commitment to the log at a given size.
///
/// Publishing a tree head — to an auditor, a timestamping service, or an
/// append-only public location — is what turns detectable tampering into
/// tampering detectable *by someone else*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TreeHead {
    /// Number of leaves committed to.
    pub size: u64,
    /// Merkle Tree Hash over those leaves.
    pub root: Hash,
}

/// Failure constructing a proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProofError {
    /// The requested leaf index is at or beyond the current log size.
    #[error("leaf index {index} out of range for log of size {size}")]
    IndexOutOfRange {
        /// Index requested.
        index: u64,
        /// Current log size.
        size: u64,
    },
    /// A consistency proof was requested against a size larger than the log.
    #[error("cannot prove consistency from size {from} against log of size {size}")]
    SizeOutOfRange {
        /// Earlier size requested.
        from: u64,
        /// Current log size.
        size: u64,
    },
}

/// Persisted accumulator state does not describe a log of the claimed size.
///
/// Separate from [`ProofError`] because it is not a proof failure: it means the
/// rows a backend read back are not the rows it wrote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "accumulator state is not a valid cover of {size} leaves: \
     expected subtree heights {expected:?}, found {found:?}"
)]
pub struct MalformedAccumulator {
    /// Number of leaves the state claims to summarise.
    pub size: u64,
    /// The heights a log of that size must decompose into, largest first.
    pub expected: Vec<u8>,
    /// The heights actually supplied.
    pub found: Vec<u8>,
}

/// Proof that a leaf sits at a given index under a given root.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InclusionProof {
    /// Index of the leaf being proven.
    pub leaf_index: u64,
    /// Size of the tree the proof is against.
    pub tree_size: u64,
    /// Sibling hashes from the leaf upward.
    pub path: Vec<Hash>,
}

impl InclusionProof {
    /// Verifies this proof against a leaf payload and an expected root.
    ///
    /// Returns `false` on any inconsistency rather than distinguishing failure
    /// modes: a caller cannot act differently on a malformed proof than on a
    /// forged one.
    #[must_use]
    pub fn verify(&self, leaf_payload: &Hash, root: &Hash) -> bool {
        if self.leaf_index >= self.tree_size {
            return false;
        }
        let mut fname = self.leaf_index;
        let mut sname = self.tree_size - 1;
        let mut acc = leaf_hash(leaf_payload);

        for p in &self.path {
            if sname == 0 {
                return false;
            }
            if fname & 1 == 1 || fname == sname {
                acc = node_hash(p, &acc);
                while fname & 1 == 0 && fname != 0 {
                    fname >>= 1;
                    sname >>= 1;
                }
            } else {
                acc = node_hash(&acc, p);
            }
            fname >>= 1;
            sname >>= 1;
        }

        sname == 0 && acc == *root
    }
}

/// Proof that an earlier tree is a prefix of a later one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConsistencyProof {
    /// Size of the earlier tree.
    pub old_size: u64,
    /// Size of the later tree.
    pub new_size: u64,
    /// Hashes needed to relate the two roots.
    pub path: Vec<Hash>,
}

impl ConsistencyProof {
    /// Verifies that `old_root` at `old_size` is a prefix of `new_root` at `new_size`.
    #[must_use]
    pub fn verify(&self, old_root: &Hash, new_root: &Hash) -> bool {
        if self.old_size > self.new_size {
            return false;
        }
        if self.old_size == self.new_size {
            return self.path.is_empty() && old_root == new_root;
        }
        if self.old_size == 0 {
            // Every tree is consistent with the empty tree, but the verifier must
            // still confirm that the root it was handed *is* the empty root.
            // Accepting any value here would let a caller "prove" consistency
            // with a history that never existed.
            return self.path.is_empty() && *old_root == empty_root();
        }

        // A proof for a tree whose size is an exact power of two omits the old
        // root, because it is a complete subtree the verifier already holds.
        let old_is_power_of_two = self.old_size & (self.old_size - 1) == 0;
        let mut iter = self.path.iter();
        let seed = if old_is_power_of_two {
            *old_root
        } else {
            match iter.next() {
                Some(h) => *h,
                None => return false,
            }
        };

        let mut fname = self.old_size - 1;
        let mut sname = self.new_size - 1;
        while fname & 1 == 1 {
            fname >>= 1;
            sname >>= 1;
        }

        let mut fr = seed;
        let mut sr = seed;

        for p in iter {
            if sname == 0 {
                return false;
            }
            if fname & 1 == 1 || fname == sname {
                fr = node_hash(p, &fr);
                sr = node_hash(p, &sr);
                while fname & 1 == 0 && fname != 0 {
                    fname >>= 1;
                    sname >>= 1;
                }
            } else {
                sr = node_hash(&sr, p);
            }
            fname >>= 1;
            sname >>= 1;
        }

        sname == 0 && fr == *old_root && sr == *new_root
    }
}

/// The perfect-subtree roots covering a log, without the leaves.
///
/// A [`MerkleLog`] keeps every leaf so it can build proofs. A durable backend
/// does not want that in memory, and does not need it to answer "what is the
/// root now" — that depends only on this stack, which holds one node per set bit
/// in the size. Appending merges equal-height neighbours, so it is amortised
/// `O(1)` and the whole structure is `O(log n)`.
///
/// Persisting it lets a backend record the tree head alongside each entry and
/// answer head queries in constant time, instead of rebuilding the tree from
/// every content hash it has ever stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MerkleAccumulator {
    subtrees: Vec<(u8, Hash)>,
    size: u64,
}

impl MerkleAccumulator {
    /// Creates an empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The subtree heights a log of `size` leaves must decompose into, largest
    /// first — one per set bit, from the most significant down.
    fn cover_of(size: u64) -> Vec<u8> {
        (0..u64::BITS)
            .rev()
            .filter(|bit| size & (1u64 << bit) != 0)
            .filter_map(|bit| u8::try_from(bit).ok())
            .collect()
    }

    /// Restores an accumulator from persisted state.
    ///
    /// `subtrees` must be ordered largest-height first, as
    /// [`MerkleAccumulator::subtrees`] returns them.
    ///
    /// The shape is checked rather than trusted. A log of `size` leaves has
    /// exactly one perfect-subtree decomposition — one subtree per set bit in
    /// `size` — so a row lost, duplicated, reordered, or written against a
    /// different size is a shape error, and this catches it. It cannot catch a
    /// node whose *hash* is wrong; that is what [`MerkleLog::verify_incremental_state`]
    /// is for, and what recording the root alongside each entry lets a backend
    /// cross-check cheaply.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedAccumulator`] when the heights supplied are not the
    /// decomposition `size` requires.
    pub fn try_from_parts(
        subtrees: Vec<(u8, Hash)>,
        size: u64,
    ) -> Result<Self, MalformedAccumulator> {
        let expected = Self::cover_of(size);
        let found: Vec<u8> = subtrees.iter().map(|(height, _)| *height).collect();
        if found == expected {
            Ok(Self { subtrees, size })
        } else {
            Err(MalformedAccumulator {
                size,
                expected,
                found,
            })
        }
    }

    /// The perfect-subtree roots, largest first.
    #[must_use]
    pub fn subtrees(&self) -> &[(u8, Hash)] {
        &self.subtrees
    }

    /// Number of leaves accumulated.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// True when nothing has been accumulated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Appends a leaf payload and returns its index.
    pub fn push(&mut self, payload: Hash) -> u64 {
        let index = self.size;
        let mut node = leaf_hash(&payload);
        let mut level = 0u8;
        // The left sibling is always the one already on the stack.
        while let Some(&(top_level, top_hash)) = self.subtrees.last() {
            if top_level != level {
                break;
            }
            self.subtrees.pop();
            node = node_hash(&top_hash, &node);
            level = level.saturating_add(1);
        }
        self.subtrees.push((level, node));
        self.size = self.size.saturating_add(1);
        index
    }

    /// The current Merkle Tree Hash.
    ///
    /// Folds the perfect subtrees right to left, which reproduces the
    /// left-perfect split the tree hash is defined by.
    #[must_use]
    pub fn root(&self) -> Hash {
        let mut iter = self.subtrees.iter().rev();
        let Some(&(_, seed)) = iter.next() else {
            return empty_root();
        };
        let mut acc = seed;
        for &(_, left) in iter {
            acc = node_hash(&left, &acc);
        }
        acc
    }

    /// The current tree head.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        TreeHead {
            size: self.size,
            root: self.root(),
        }
    }
}

/// An append-only log of leaf payloads.
///
/// Leaves are the content hashes of whatever the log commits to; the log itself
/// does not interpret them.
///
/// # Cost
///
/// The log keeps the roots of the perfect subtrees covering its leaves, so
/// appending is amortised `O(1)` and reading the current root is `O(log n)`
/// rather than a full recomputation. Historical roots and proofs are derived
/// from the stored leaves and cost `O(n)`; they are audit-time operations, not
/// write-path ones.
#[derive(Debug, Clone, Default)]
pub struct MerkleLog {
    leaves: Vec<Hash>,
    /// Derived state: [`MerkleLog::root`] folds it, and
    /// [`MerkleLog::verify_incremental_state`] proves it still agrees with a
    /// recomputation from the leaves.
    accumulator: MerkleAccumulator,
}

impl MerkleLog {
    /// Creates an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            leaves: Vec::new(),
            accumulator: MerkleAccumulator::new(),
        }
    }

    /// Builds a log from existing leaf payloads, in order.
    #[must_use]
    pub fn from_leaves(leaves: Vec<Hash>) -> Self {
        let mut log = Self::new();
        for leaf in leaves {
            log.append(leaf);
        }
        log
    }

    /// Number of leaves.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.leaves.len() as u64
    }

    /// True when the log holds no leaves.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// The leaf payloads, in log order.
    #[must_use]
    pub fn leaves(&self) -> &[Hash] {
        &self.leaves
    }

    /// Appends a leaf payload and returns its index.
    ///
    /// Amortised `O(1)`: the new leaf merges with any equal-sized subtree
    /// already on the stack, which happens once per power of two.
    pub fn append(&mut self, payload: Hash) -> u64 {
        self.leaves.push(payload);
        self.accumulator.push(payload)
    }

    /// The perfect-subtree state, for a backend that wants to persist it.
    #[must_use]
    pub fn accumulator(&self) -> &MerkleAccumulator {
        &self.accumulator
    }

    /// Drops every leaf from `len` onward and rebuilds the subtree state.
    ///
    /// Deliberately not public: a log that can be shortened is not append-only.
    /// It exists so an in-memory journal can undo a batch that turned out to be
    /// unacceptable partway through — leaves that were never committed to
    /// anything, because the batch never returned. `O(n)`, and only on that
    /// path.
    pub(crate) fn truncate(&mut self, len: u64) {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len >= self.leaves.len() {
            return;
        }
        self.leaves.truncate(len);
        self.accumulator = MerkleAccumulator::new();
        for leaf in &self.leaves {
            self.accumulator.push(*leaf);
        }
    }

    /// The current Merkle Tree Hash.
    ///
    /// Folds the perfect subtrees right to left, which reproduces the
    /// left-perfect split the tree hash is defined by.
    #[must_use]
    pub fn root(&self) -> Hash {
        self.accumulator.root()
    }

    /// Recomputes the root from the stored leaves and compares it with the
    /// incrementally maintained one.
    ///
    /// The incremental subtree stack is derived state; this proves it has not
    /// drifted from the leaves it claims to summarise.
    #[must_use]
    pub fn verify_incremental_state(&self) -> bool {
        self.root() == mth(&self.leaves)
    }

    /// The current tree head.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        TreeHead {
            size: self.len(),
            root: self.root(),
        }
    }

    /// The Merkle Tree Hash over the first `size` leaves.
    ///
    /// Used to reconstruct a historical root without replaying the log.
    pub fn root_at(&self, size: u64) -> Result<Hash, ProofError> {
        let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
        if size_usize > self.leaves.len() {
            return Err(ProofError::SizeOutOfRange {
                from: size,
                size: self.len(),
            });
        }
        Ok(mth(&self.leaves[..size_usize]))
    }

    /// Builds an inclusion proof for the leaf at `index`.
    pub fn inclusion_proof(&self, index: u64) -> Result<InclusionProof, ProofError> {
        let n = self.leaves.len();
        let m = usize::try_from(index).unwrap_or(usize::MAX);
        if m >= n {
            return Err(ProofError::IndexOutOfRange {
                index,
                size: self.len(),
            });
        }
        let mut path = Vec::new();
        build_inclusion_path(m, &self.leaves, &mut path);
        Ok(InclusionProof {
            leaf_index: index,
            tree_size: self.len(),
            path,
        })
    }

    /// Builds a consistency proof from an earlier size to the current size.
    pub fn consistency_proof(&self, old_size: u64) -> Result<ConsistencyProof, ProofError> {
        let n = self.leaves.len();
        let m = usize::try_from(old_size).unwrap_or(usize::MAX);
        if m > n {
            return Err(ProofError::SizeOutOfRange {
                from: old_size,
                size: self.len(),
            });
        }
        let mut path = Vec::new();
        if m > 0 && m < n {
            build_consistency_path(m, &self.leaves, true, &mut path);
        }
        Ok(ConsistencyProof {
            old_size,
            new_size: self.len(),
            path,
        })
    }
}

/// `PATH(m, D[n])` from RFC 6962.
fn build_inclusion_path(m: usize, leaves: &[Hash], out: &mut Vec<Hash>) {
    let n = leaves.len();
    if n <= 1 {
        return;
    }
    let k = split_point(n);
    if m < k {
        build_inclusion_path(m, &leaves[..k], out);
        out.push(mth(&leaves[k..]));
    } else {
        build_inclusion_path(m - k, &leaves[k..], out);
        out.push(mth(&leaves[..k]));
    }
}

/// `SUBPROOF(m, D[n], b)` from RFC 6962.
fn build_consistency_path(m: usize, leaves: &[Hash], b: bool, out: &mut Vec<Hash>) {
    let n = leaves.len();
    if m == n {
        if !b {
            out.push(mth(leaves));
        }
        return;
    }
    let k = split_point(n);
    if m <= k {
        build_consistency_path(m, &leaves[..k], b, out);
        out.push(mth(&leaves[k..]));
    } else {
        build_consistency_path(m - k, &leaves[k..], false, out);
        out.push(mth(&leaves[..k]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(i: u64) -> Hash {
        tagged(b"test/leaf", &i.to_le_bytes())
    }

    fn log_of(n: u64) -> MerkleLog {
        let mut log = MerkleLog::new();
        for i in 0..n {
            log.append(payload(i));
        }
        log
    }

    #[test]
    fn empty_log_has_empty_root() {
        assert_eq!(MerkleLog::new().root(), empty_root());
        assert_eq!(MerkleLog::new().head().size, 0);
    }

    #[test]
    fn single_leaf_root_is_leaf_hash() {
        let mut log = MerkleLog::new();
        log.append(payload(0));
        assert_eq!(log.root(), leaf_hash(&payload(0)));
    }

    #[test]
    fn append_returns_sequential_indices() {
        let mut log = MerkleLog::new();
        assert_eq!(log.append(payload(0)), 0);
        assert_eq!(log.append(payload(1)), 1);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn root_changes_when_a_leaf_changes() {
        let a = log_of(8);
        let mut leaves = a.leaves().to_vec();
        leaves[3] = payload(999);
        let b = MerkleLog::from_leaves(leaves);
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn inclusion_proofs_verify_for_every_leaf_and_size() {
        for n in 1..=33u64 {
            let log = log_of(n);
            let root = log.root();
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                assert!(
                    proof.verify(&payload(i), &root),
                    "inclusion proof failed for leaf {i} of {n}"
                );
            }
        }
    }

    #[test]
    fn inclusion_proof_rejects_wrong_leaf() {
        let log = log_of(8);
        let root = log.root();
        let proof = log.inclusion_proof(3).expect("in range");
        assert!(!proof.verify(&payload(4), &root));
    }

    #[test]
    fn inclusion_proof_rejects_wrong_root() {
        let log = log_of(8);
        let proof = log.inclusion_proof(3).expect("in range");
        assert!(!proof.verify(&payload(3), &payload(0)));
    }

    #[test]
    fn inclusion_proof_rejects_tampered_path() {
        let log = log_of(8);
        let root = log.root();
        let mut proof = log.inclusion_proof(3).expect("in range");
        proof.path[0] = payload(12345);
        assert!(!proof.verify(&payload(3), &root));
    }

    #[test]
    fn inclusion_proof_out_of_range_is_an_error() {
        let log = log_of(4);
        assert!(matches!(
            log.inclusion_proof(4),
            Err(ProofError::IndexOutOfRange { index: 4, size: 4 })
        ));
    }

    #[test]
    fn inclusion_path_is_logarithmic() {
        let log = log_of(1024);
        let proof = log.inclusion_proof(500).expect("in range");
        assert_eq!(proof.path.len(), 10);
    }

    #[test]
    fn consistency_proofs_verify_for_every_prefix() {
        for n in 1..=33u64 {
            let log = log_of(n);
            let new_root = log.root();
            for m in 0..=n {
                let old_root = log.root_at(m).expect("in range");
                let proof = log.consistency_proof(m).expect("in range");
                assert!(
                    proof.verify(&old_root, &new_root),
                    "consistency proof failed from {m} to {n}"
                );
            }
        }
    }

    #[test]
    fn consistency_proof_rejects_a_rewritten_prefix() {
        let log = log_of(16);
        let new_root = log.root();
        let proof = log.consistency_proof(8).expect("in range");

        // A log that diverges within the first 8 leaves must not verify.
        let mut tampered = log.leaves()[..8].to_vec();
        tampered[2] = payload(4242);
        let forged_old_root = MerkleLog::from_leaves(tampered).root();

        assert!(!proof.verify(&forged_old_root, &new_root));
    }

    #[test]
    fn consistency_proof_rejects_growing_backwards() {
        let log = log_of(8);
        assert!(matches!(
            log.consistency_proof(9),
            Err(ProofError::SizeOutOfRange { from: 9, size: 8 })
        ));
    }

    #[test]
    fn consistency_from_the_empty_tree_requires_the_empty_root() {
        let log = log_of(8);
        let proof = log.consistency_proof(0).expect("in range");

        // The empty tree is a prefix of everything …
        assert!(proof.verify(&empty_root(), &log.root()));
        // … but a claimed history that was never empty must not verify.
        assert!(!proof.verify(&payload(1), &log.root()));
        assert!(!proof.verify(&log.root(), &log.root()));
    }

    #[test]
    fn incremental_root_matches_full_recomputation() {
        // The subtree stack is derived state; it must agree with the definition
        // at every size, including the ragged ones between powers of two.
        let mut log = MerkleLog::new();
        assert_eq!(log.root(), mth(&[]));
        for i in 0..129u64 {
            log.append(payload(i));
            assert_eq!(log.root(), mth(log.leaves()), "diverged at size {}", i + 1);
            assert!(log.verify_incremental_state());
        }
    }

    #[test]
    fn subtree_count_is_the_population_count_of_the_size() {
        // One perfect subtree per set bit in the leaf count.
        for n in 1u64..64 {
            let log = log_of(n);
            assert_eq!(
                log.accumulator().subtrees().len() as u32,
                n.count_ones(),
                "size {n}"
            );
        }
    }

    #[test]
    fn the_accumulator_matches_the_log_at_every_size() {
        // A backend persists only the accumulator, so it must agree with a full
        // log at every size — including the ragged ones between powers of two.
        let mut acc = MerkleAccumulator::new();
        let mut log = MerkleLog::new();
        for i in 0..80u64 {
            acc.push(payload(i));
            log.append(payload(i));
            assert_eq!(acc.root(), log.root(), "diverged at size {}", i + 1);
            assert_eq!(acc.size(), log.len());
        }
    }

    #[test]
    fn an_accumulator_restores_from_persisted_parts() {
        let mut original = MerkleAccumulator::new();
        for i in 0..37u64 {
            original.push(payload(i));
        }

        // Round-trip through the representation a backend would store.
        let restored =
            MerkleAccumulator::try_from_parts(original.subtrees().to_vec(), original.size())
                .expect("well-formed");
        assert_eq!(restored.root(), original.root());

        // And it keeps accumulating correctly from there.
        let mut a = original.clone();
        let mut b = restored;
        for i in 37..50u64 {
            a.push(payload(i));
            b.push(payload(i));
        }
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn restoring_an_accumulator_checks_its_shape() {
        let mut original = MerkleAccumulator::new();
        for i in 0..37u64 {
            original.push(payload(i));
        }
        let parts = original.subtrees().to_vec();

        // 37 = 0b100101, so the cover is heights [5, 2, 0] and nothing else.
        assert_eq!(
            parts.iter().map(|(h, _)| *h).collect::<Vec<_>>(),
            vec![5, 2, 0]
        );

        // A row lost between the database and the process.
        let mut short = parts.clone();
        short.pop();
        assert!(matches!(
            MerkleAccumulator::try_from_parts(short, 37),
            Err(MalformedAccumulator { size: 37, .. })
        ));

        // Rows returned in the wrong order — a missing ORDER BY.
        let mut shuffled = parts.clone();
        shuffled.reverse();
        assert!(MerkleAccumulator::try_from_parts(shuffled, 37).is_err());

        // The right rows against the wrong size.
        assert!(MerkleAccumulator::try_from_parts(parts, 36).is_err());

        // And the empty state is well-formed at size zero only.
        assert!(MerkleAccumulator::try_from_parts(Vec::new(), 0).is_ok());
        assert!(MerkleAccumulator::try_from_parts(Vec::new(), 1).is_err());
    }

    #[test]
    fn a_restored_accumulator_is_checked_at_every_size() {
        let mut acc = MerkleAccumulator::new();
        for i in 0..200u64 {
            acc.push(payload(i));
            let restored = MerkleAccumulator::try_from_parts(acc.subtrees().to_vec(), acc.size())
                .expect("its own state must round-trip");
            assert_eq!(restored.root(), acc.root());
        }
    }

    #[test]
    fn an_empty_accumulator_is_the_empty_root() {
        let acc = MerkleAccumulator::new();
        assert!(acc.is_empty());
        assert_eq!(acc.root(), empty_root());
        assert_eq!(acc.head().size, 0);
    }

    #[test]
    fn from_leaves_and_repeated_append_agree() {
        let leaves: Vec<Hash> = (0..37).map(payload).collect();
        let built = MerkleLog::from_leaves(leaves.clone());
        let mut appended = MerkleLog::new();
        for l in leaves {
            appended.append(l);
        }
        assert_eq!(built.root(), appended.root());
        assert!(built.verify_incremental_state());
    }

    #[test]
    fn consistency_with_equal_sizes_is_empty_and_reflexive() {
        let log = log_of(8);
        let root = log.root();
        let proof = log.consistency_proof(8).expect("in range");
        assert!(proof.path.is_empty());
        assert!(proof.verify(&root, &root));
    }

    #[test]
    fn root_at_reconstructs_historical_roots() {
        let log = log_of(20);
        for m in 0..=20u64 {
            assert_eq!(log.root_at(m).expect("in range"), log_of(m).root());
        }
    }

    #[test]
    fn split_point_is_largest_power_of_two_below() {
        assert_eq!(split_point(2), 1);
        assert_eq!(split_point(3), 2);
        assert_eq!(split_point(4), 2);
        assert_eq!(split_point(5), 4);
        assert_eq!(split_point(1024), 512);
        assert_eq!(split_point(1025), 1024);
    }

    #[test]
    fn leaf_and_node_hashing_are_domain_separated() {
        // A leaf must not be confusable with an interior node of the same bytes.
        let l = payload(0);
        assert_ne!(leaf_hash(&l), node_hash(&l, &l));
        assert_ne!(leaf_hash(&l), empty_root());
    }
}
