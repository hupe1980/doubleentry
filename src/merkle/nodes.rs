//! Permanent node numbering, and proofs built from `O(log n)` reads of it.
//!
//! # Why interior nodes are stored
//!
//! A proof is `O(log n)` hashes, but producing one from *leaves alone* means
//! rebuilding every interior node on the way — `O(n)` time and, for a durable
//! backend, `O(n)` memory, since every content hash in the ledger has to be read
//! back and held at once.
//!
//! A node whose subtree is complete can never change in an append-only log, so
//! it is written once and read back forever; only the ragged right edge is
//! folded at read time. Storage is `2n − popcount(n)` hashes — just under two
//! per entry — and a proof reads at most `1 + ⌈log₂ n⌉` of them.
//!
//! # The numbering
//!
//! Nodes are numbered in the order they *become complete*, which is the order an
//! append-only log can write them:
//!
//! ```text
//!            6                 level 2
//!      2          5            level 1
//!   0     1    3     4         level 0  (leaf hashes)
//!  rec0  rec1 rec2  rec3
//! ```
//!
//! Adding record `n` appends `1 + trailing_zeros(n + 1)` nodes — its leaf hash,
//! then one per subtree that just closed — so a backend keeps them in one table
//! with a monotonic integer key and no updates, ever.
//!
//! This is [Go `tlog`'s numbering](https://pkg.go.dev/golang.org/x/mod/sumdb/tlog#StoredHashIndex)
//! exactly, so the layout is one other transparency-log tooling understands.
//!
//! # Plan, read, assemble
//!
//! Reading is split in two because storage is asynchronous and this crate is
//! not: [`InclusionPlan::new`] says *which* nodes are needed, the caller fetches
//! them however it likes, and [`InclusionPlan::assemble`] folds them into a
//! proof. Both halves are pure, so the in-memory [`MerkleLog`](super::MerkleLog)
//! runs the same two functions a SQL backend does — which is what stops the two
//! from disagreeing.

// Index arithmetic bounded by the recursion invariants (`lo < hi`, `lo <= n <
// hi`), with every shift below 64 by construction. Checked arithmetic here would
// obscure the correspondence with the published algorithms without making them
// safer, exactly as in the parent module.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use crate::hash::Hash;

use super::{ProofError, node_hash};

/// How many nodes a log of `size` records stores.
///
/// Exactly `2·size − popcount(size)`: every record contributes its leaf hash,
/// plus one interior node for each subtree that closes when it lands, and the
/// set bits of `size` are precisely the subtrees still open.
///
/// Just under two hashes per entry, which is what a backend should size its
/// storage against.
#[must_use]
pub fn count(size: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    size.saturating_mul(2)
        .saturating_sub(u64::from(size.count_ones()))
}

/// The storage position of the node at `level` covering records starting at
/// `index · 2^level`.
///
/// Level 0 is a leaf hash; level `L` node `i` covers records
/// `[i·2^L, (i+1)·2^L)`.
#[must_use]
pub fn index_of(level: u8, index: u64) -> u64 {
    // A level-L node is written immediately after the level-(L−1) node numbered
    // 2i+1, so walk down to the leaf that closes this subtree.
    let mut n = index;
    for _ in 0..level {
        n = n.saturating_mul(2).saturating_add(1);
    }
    // Leaf n sits at n + n/2 + n/4 + … — its own position plus one for every
    // subtree that closed before it.
    let mut at = 0u64;
    while n > 0 {
        at = at.saturating_add(n);
        n >>= 1;
    }
    at.saturating_add(u64::from(level))
}

/// The `(level, index)` a storage position names.
///
/// The exact inverse of [`index_of`], which the round-trip test pins.
#[must_use]
pub fn split_index(stored: u64) -> (u8, u64) {
    let (level, record) = level_and_record(stored);
    // `index_of` is level-relative: the level-L node holding record `r` is the
    // one numbered `r >> L`, since it covers 2^L consecutive records.
    (level, record >> u32::from(level))
}

/// The level of a storage position, and the record whose arrival wrote it.
///
/// Adding a record writes its leaf hash and then one node per subtree it just
/// closed, all attributed to that record — so this is how a backend turns "the
/// highest position I hold" into "how many records I hold".
fn level_and_record(stored: u64) -> (u8, u64) {
    // Leaf positions grow by at least one per record, so the leaf at or before
    // `stored` is no further back than `stored / 2`.
    let mut record = stored / 2;
    let mut at = index_of(0, record);
    loop {
        // Record n contributes 1 + trailing_zeros(n + 1) nodes.
        let next = at + 1 + u64::from((record + 1).trailing_zeros());
        if next > stored {
            break;
        }
        record += 1;
        at = next;
    }
    // Everything above that leaf position is an interior node, one per level.
    (u8::try_from(stored - at).unwrap_or(u8::MAX), record)
}

/// How many records a store holding `stored` nodes describes.
///
/// The inverse of [`count`], which is strictly increasing, so the answer is
/// unique. `None` when no size produces exactly that many nodes — a record
/// writes all of its nodes or none of them, so a count that lands between two
/// sizes means a partially applied append, which a transaction is supposed to
/// make impossible.
#[must_use]
pub fn size_from_count(stored: u64) -> Option<u64> {
    if stored == 0 {
        return Some(0);
    }
    // count(size) >= size, so the size is somewhere at or below the node count.
    let (mut low, mut high) = (0u64, stored);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if count(mid) <= stored {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    (count(low) == stored).then_some(low)
}

/// The largest power of two strictly less than `n`, and its exponent.
///
/// The split every algorithm here recurses on: a tree of `n` leaves is a perfect
/// left subtree of this size and a shorter right one.
fn split(n: u64) -> (u64, u8) {
    let mut level = 0u8;
    while level < 63 && (1u64 << (level + 1)) < n {
        level += 1;
    }
    (1u64 << level, level)
}

/// Appends the storage positions covering records `[lo, hi)`, largest first.
///
/// A range that is not itself a perfect subtree decomposes into several, exactly
/// as a log's size decomposes into its set bits.
fn subtree_indices(lo: u64, hi: u64, need: &mut Vec<u64>) {
    let mut lo = lo;
    while lo < hi {
        let (width, level) = split(hi - lo + 1);
        need.push(index_of(level, lo >> level));
        lo += width;
    }
}

/// Folds the hashes [`subtree_indices`] asked for into one subtree root.
///
/// Returns the root and the hashes left over, so callers can consume a plan's
/// output left to right in one pass.
fn subtree_hash(lo: u64, hi: u64, hashes: &[Hash]) -> Option<(Hash, &[Hash])> {
    let mut wanted = 0usize;
    let mut at = lo;
    while at < hi {
        let (width, _) = split(hi - at + 1);
        wanted = wanted.checked_add(1)?;
        at += width;
    }
    let (used, rest) = hashes.split_at_checked(wanted)?;
    // Right to left: the decomposition is largest-subtree-first, and the tree
    // hash folds the shorter right side into the taller left one.
    let mut folded = *used.last()?;
    for left in used.iter().rev().skip(1) {
        folded = node_hash(left, &folded);
    }
    Some((folded, rest))
}

/// Which nodes prove that leaf `n` is in the subtree over records `[lo, hi)`.
fn inclusion_indices(lo: u64, hi: u64, n: u64, need: &mut Vec<u64>) {
    if lo + 1 == hi {
        return;
    }
    let (width, _) = split(hi - lo);
    if n < lo + width {
        inclusion_indices(lo, lo + width, n, need);
        subtree_indices(lo + width, hi, need);
    } else {
        subtree_indices(lo, lo + width, need);
        inclusion_indices(lo + width, hi, n, need);
    }
}

/// Folds those nodes into the sibling path, deepest sibling first.
fn inclusion_path<'a>(
    lo: u64,
    hi: u64,
    n: u64,
    hashes: &'a [Hash],
    out: &mut Vec<Hash>,
) -> Option<&'a [Hash]> {
    if lo + 1 == hi {
        return Some(hashes);
    }
    let (width, _) = split(hi - lo);
    let (sibling, rest) = if n < lo + width {
        let rest = inclusion_path(lo, lo + width, n, hashes, out)?;
        subtree_hash(lo + width, hi, rest)?
    } else {
        let (sibling, rest) = subtree_hash(lo, lo + width, hashes)?;
        (sibling, inclusion_path(lo + width, hi, n, rest, out)?)
    };
    out.push(sibling);
    Some(rest)
}

/// Which nodes relate the subtree over `[lo, n)` to the one over `[lo, hi)`.
fn consistency_indices(lo: u64, hi: u64, n: u64, need: &mut Vec<u64>) {
    if n == hi {
        // The old tree ends exactly here. At the very left it is the whole
        // subtree and the verifier already holds its root; anywhere else it has
        // to be named.
        if lo != 0 {
            subtree_indices(lo, hi, need);
        }
        return;
    }
    let (width, _) = split(hi - lo);
    if n <= lo + width {
        consistency_indices(lo, lo + width, n, need);
        subtree_indices(lo + width, hi, need);
    } else {
        subtree_indices(lo, lo + width, need);
        consistency_indices(lo + width, hi, n, need);
    }
}

/// Folds those nodes into a consistency path.
fn consistency_path<'a>(
    lo: u64,
    hi: u64,
    n: u64,
    hashes: &'a [Hash],
    out: &mut Vec<Hash>,
) -> Option<&'a [Hash]> {
    if n == hi {
        if lo == 0 {
            return Some(hashes);
        }
        let (root, rest) = subtree_hash(lo, hi, hashes)?;
        out.push(root);
        return Some(rest);
    }
    let (width, _) = split(hi - lo);
    let (sibling, rest) = if n <= lo + width {
        let rest = consistency_path(lo, lo + width, n, hashes, out)?;
        subtree_hash(lo + width, hi, rest)?
    } else {
        let (sibling, rest) = subtree_hash(lo, lo + width, hashes)?;
        (sibling, consistency_path(lo + width, hi, n, rest, out)?)
    };
    out.push(sibling);
    Some(rest)
}

/// The nodes a caller supplied are not the ones a plan asked for.
///
/// Wrong count, wrong order, or a position the store could not produce. It is
/// not a proof failure — nothing has been proven or disproven — it means the
/// rows a backend read back are not the rows its plan named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected {expected} nodes for this plan, got {found}")]
pub struct MalformedNodes {
    /// How many nodes the plan asked for.
    pub expected: usize,
    /// How many were supplied.
    pub found: usize,
}

/// Checks the count before folding, so a short slice is named rather than
/// producing a silently truncated proof.
fn expect_len(plan: &[u64], nodes: &[Hash]) -> Result<(), MalformedNodes> {
    if plan.len() == nodes.len() {
        Ok(())
    } else {
        Err(MalformedNodes {
            expected: plan.len(),
            found: nodes.len(),
        })
    }
}

/// Which stored nodes are needed to reproduce the root at `size`.
///
/// The perfect-subtree cover — one node per set bit in `size`, largest first —
/// which is the same decomposition
/// [`MerkleAccumulator`](super::MerkleAccumulator) keeps in memory. A backend
/// that stores nodes therefore needs no separate accumulator table: the cover is
/// a lookup of `O(log n)` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootPlan {
    size: u64,
    need: Vec<u64>,
}

impl RootPlan {
    /// Plans the root of a log of `size` records.
    #[must_use]
    pub fn new(size: u64) -> Self {
        let mut need = Vec::new();
        subtree_indices(0, size, &mut need);
        Self { size, need }
    }

    /// The storage positions to read, in the order [`Self::assemble`] wants them.
    #[must_use]
    pub fn nodes(&self) -> &[u64] {
        &self.need
    }

    /// Folds the nodes into the root.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedNodes`] when `nodes` is not what [`Self::nodes`] asked
    /// for.
    pub fn assemble(&self, nodes: &[Hash]) -> Result<Hash, MalformedNodes> {
        expect_len(&self.need, nodes)?;
        if self.size == 0 {
            return Ok(super::empty_root());
        }
        subtree_hash(0, self.size, nodes)
            .map(|(root, _)| root)
            .ok_or(MalformedNodes {
                expected: self.need.len(),
                found: nodes.len(),
            })
    }
}

/// Which stored nodes prove leaf `index` sits under the root at `size`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InclusionPlan {
    index: u64,
    size: u64,
    need: Vec<u64>,
}

impl InclusionPlan {
    /// Plans an inclusion proof for leaf `index` against a tree of `size`.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::IndexOutOfRange`] when the leaf is at or beyond
    /// `size`.
    pub fn new(index: u64, size: u64) -> Result<Self, ProofError> {
        if index >= size {
            return Err(ProofError::IndexOutOfRange { index, size });
        }
        let mut need = Vec::new();
        inclusion_indices(0, size, index, &mut need);
        Ok(Self { index, size, need })
    }

    /// The storage positions to read, in the order [`Self::assemble`] wants them.
    #[must_use]
    pub fn nodes(&self) -> &[u64] {
        &self.need
    }

    /// Folds the nodes into a proof.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedNodes`] when `nodes` is not what [`Self::nodes`] asked
    /// for.
    pub fn assemble(&self, nodes: &[Hash]) -> Result<super::InclusionProof, MalformedNodes> {
        expect_len(&self.need, nodes)?;
        let mut path = Vec::new();
        inclusion_path(0, self.size, self.index, nodes, &mut path).ok_or(MalformedNodes {
            expected: self.need.len(),
            found: nodes.len(),
        })?;
        Ok(super::InclusionProof {
            leaf_index: self.index,
            tree_size: self.size,
            path,
        })
    }
}

/// Which stored nodes prove the log at `old` is a prefix of the log at `new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyPlan {
    old: u64,
    new: u64,
    need: Vec<u64>,
}

impl ConsistencyPlan {
    /// Plans a consistency proof from `old` to `new`.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::EmptyOldTree`] for `old == 0` — a proof from the
    /// empty tree is true of every history and is refused rather than built —
    /// and [`ProofError::SizeOutOfRange`] when `old > new`.
    pub fn new(old: u64, new: u64) -> Result<Self, ProofError> {
        if old == 0 {
            return Err(ProofError::EmptyOldTree { new_size: new });
        }
        if old > new {
            return Err(ProofError::SizeOutOfRange {
                from: old,
                size: new,
            });
        }
        let mut need = Vec::new();
        if old < new {
            consistency_indices(0, new, old, &mut need);
        }
        Ok(Self { old, new, need })
    }

    /// The storage positions to read, in the order [`Self::assemble`] wants them.
    #[must_use]
    pub fn nodes(&self) -> &[u64] {
        &self.need
    }

    /// Folds the nodes into a proof.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedNodes`] when `nodes` is not what [`Self::nodes`] asked
    /// for.
    pub fn assemble(&self, nodes: &[Hash]) -> Result<super::ConsistencyProof, MalformedNodes> {
        expect_len(&self.need, nodes)?;
        let mut path = Vec::new();
        if self.old < self.new {
            consistency_path(0, self.new, self.old, nodes, &mut path).ok_or(MalformedNodes {
                expected: self.need.len(),
                found: nodes.len(),
            })?;
        }
        Ok(super::ConsistencyProof {
            old_size: self.old,
            new_size: self.new,
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_numbering_round_trips() {
        for stored in 0..2_000u64 {
            let (level, index) = split_index(stored);
            assert_eq!(
                index_of(level, index),
                stored,
                "position {stored} split to ({level}, {index}) and back to something else"
            );
        }
    }

    #[test]
    fn the_numbering_matches_the_documented_layout() {
        // The four-leaf tree drawn in the module docs.
        assert_eq!(index_of(0, 0), 0);
        assert_eq!(index_of(0, 1), 1);
        assert_eq!(index_of(1, 0), 2);
        assert_eq!(index_of(0, 2), 3);
        assert_eq!(index_of(0, 3), 4);
        assert_eq!(index_of(1, 1), 5);
        assert_eq!(index_of(2, 0), 6);
    }

    #[test]
    fn appending_a_record_writes_one_node_per_closed_subtree() {
        // `count` is a closed form; this is the incremental statement it has to
        // agree with, which is what a backend actually does on append.
        let mut running = 0u64;
        for n in 0..4_000u64 {
            assert_eq!(count(n), running, "count disagreed at {n}");
            running += 1 + u64::from((n + 1).trailing_zeros());
        }
    }

    #[test]
    fn storage_is_just_under_two_hashes_per_record() {
        for size in [1u64, 2, 3, 7, 8, 1_000, 1_048_576] {
            let nodes = count(size);
            // Strictly under 2n, and short of it by exactly the set bits of n —
            // which is at most 64 however large the log gets.
            assert!(nodes < 2 * size, "size {size} stored {nodes}");
            assert_eq!(
                2 * size - nodes,
                u64::from(size.count_ones()),
                "size {size} stored {nodes}"
            );
        }
    }

    #[test]
    fn the_node_count_inverts_to_the_size() {
        for size in 0..5_000u64 {
            assert_eq!(
                size_from_count(count(size)),
                Some(size),
                "count did not invert at size {size}"
            );
        }
    }

    #[test]
    fn a_count_between_two_sizes_is_refused() {
        // A record writes all of its nodes or none of them, so this can only
        // mean a partially applied append — which a transaction rules out, and
        // which is worth naming rather than rounding to the nearest size.
        for size in 1..500u64 {
            let full = count(size);
            let previous = count(size - 1);
            for between in (previous + 1)..full {
                assert_eq!(
                    size_from_count(between),
                    None,
                    "{between} nodes is between sizes {} and {size}",
                    size - 1
                );
            }
        }
    }

    #[test]
    fn the_last_position_names_the_last_record() {
        // How a backend turns `MAX(node_index)` into a log length.
        for size in 1..2_000u64 {
            let top = count(size) - 1;
            assert_eq!(
                super::level_and_record(top).1 + 1,
                size,
                "the highest position did not name the last record at size {size}"
            );
        }
    }

    #[test]
    fn a_proof_reads_a_logarithmic_number_of_nodes() {
        // The claim in the module docs, and the whole reason this exists.
        for size in [2u64, 16, 1_024, 1_048_576] {
            let bound = usize::try_from(size.ilog2()).expect("small") + 2;
            let plan = InclusionPlan::new(size / 3, size).expect("in range");
            assert!(
                plan.nodes().len() <= bound,
                "inclusion at size {size} read {} nodes, over the bound of {bound}",
                plan.nodes().len()
            );
            let plan = ConsistencyPlan::new(size / 3 + 1, size).expect("in range");
            assert!(
                plan.nodes().len() <= bound,
                "consistency at size {size} read {} nodes, over the bound of {bound}",
                plan.nodes().len()
            );
        }
    }

    #[test]
    fn a_plan_names_the_count_it_needs_rather_than_truncating() {
        let plan = InclusionPlan::new(3, 8).expect("in range");
        let short = vec![Hash::from_bytes([0; 32]); plan.nodes().len() - 1];
        assert!(matches!(
            plan.assemble(&short),
            Err(MalformedNodes { found, .. }) if found == short.len()
        ));
        let long = vec![Hash::from_bytes([0; 32]); plan.nodes().len() + 1];
        assert!(plan.assemble(&long).is_err());
    }

    #[test]
    fn a_proof_from_the_empty_tree_is_not_planned() {
        assert!(matches!(
            ConsistencyPlan::new(0, 8),
            Err(ProofError::EmptyOldTree { new_size: 8 })
        ));
    }

    #[test]
    fn a_leaf_beyond_the_tree_is_not_planned() {
        assert!(matches!(
            InclusionPlan::new(8, 8),
            Err(ProofError::IndexOutOfRange { index: 8, size: 8 })
        ));
    }

    #[test]
    fn a_shrinking_consistency_proof_is_not_planned() {
        assert!(matches!(
            ConsistencyPlan::new(9, 8),
            Err(ProofError::SizeOutOfRange { from: 9, size: 8 })
        ));
    }
}
