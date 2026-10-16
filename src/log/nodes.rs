//! Stored-node numbering and proof plans, shared by the in-memory [`Tree`]
//! and by stores.
//!
//! Nodes use Go `tlog`'s numbering: a node is stored once its subtree is
//! complete, in the order those subtrees complete, so a tree of `n` leaves
//! stores `2n − popcount(n)` nodes and stored nodes never change. A proof is
//! planned (`ids()`: the stored nodes it needs), fetched in one batch, and
//! assembled — the same pure code for every backend, at any historical size.
//!
//! Every function accepts sizes up to [`MAX_TREE_SIZE`] and refuses larger
//! ones with [`ProofError::TooLarge`]: no wrapping, no unbounded loop.
//!
//! [`Tree`]: crate::store::Tree

use super::{ConsistencyProof, Hash, InclusionProof, ProofError, empty_root, node_hash};

/// The largest tree size (2⁶²): every node index then fits in a `u64`.
pub const MAX_TREE_SIZE: u64 = 1 << 62;

fn too_large(size: u64) -> ProofError {
    ProofError::TooLarge { size }
}

fn check_size(size: u64) -> Result<(), ProofError> {
    if size > MAX_TREE_SIZE {
        Err(too_large(size))
    } else {
        Ok(())
    }
}

/// The number of stored nodes of a tree of `size` leaves: `2n − popcount(n)`.
///
/// # Errors
///
/// [`ProofError::TooLarge`] above [`MAX_TREE_SIZE`].
pub fn stored_count(size: u64) -> Result<u64, ProofError> {
    check_size(size)?;
    size.checked_mul(2)
        .and_then(|double| double.checked_sub(u64::from(size.count_ones())))
        .ok_or_else(|| too_large(size))
}

/// The stored index of the node at `level` (leaves are level 0) covering the
/// `index`-th complete subtree of that level.
///
/// # Errors
///
/// [`ProofError::TooLarge`] when the node lies beyond a tree of
/// [`MAX_TREE_SIZE`] leaves.
pub fn stored_index(level: u8, index: u64) -> Result<u64, ProofError> {
    // The node's last leaf is `(index + 1) · 2^level − 1`; it lies in the tree.
    let width = 1u64
        .checked_shl(u32::from(level))
        .filter(|width| *width <= MAX_TREE_SIZE)
        .ok_or_else(|| too_large(index))?;
    let end = index
        .checked_add(1)
        .and_then(|count| count.checked_mul(width))
        .filter(|end| *end <= MAX_TREE_SIZE)
        .ok_or_else(|| too_large(index))?;
    // Σ_k ⌊m / 2^k⌋ = 2m − popcount(m) for m = end − 1, plus the level.
    let m = end.checked_sub(1).ok_or_else(|| too_large(index))?;
    m.checked_mul(2)
        .and_then(|double| double.checked_sub(u64::from(m.count_ones())))
        .and_then(|sum| sum.checked_add(u64::from(level)))
        .ok_or_else(|| too_large(index))
}

/// The inverse of [`stored_index`]: `(level, index)` of a stored node.
///
/// # Errors
///
/// [`ProofError::TooLarge`] for a stored index beyond a tree of
/// [`MAX_TREE_SIZE`] leaves.
pub fn split_stored(stored: u64) -> Result<(u8, u64), ProofError> {
    if stored >= stored_count(MAX_TREE_SIZE)? {
        return Err(too_large(stored));
    }
    // Leaf n is stored at 2n − popcount(n) ≤ stored, so n ≥ stored / 2, and
    // each later leaf adds 1 + trailing_zeros(n + 1) nodes: at most 63 steps.
    let mut leaf = stored.div_euclid(2);
    let mut at = stored_index(0, leaf)?;
    loop {
        let next_leaf = leaf.checked_add(1).ok_or_else(|| too_large(stored))?;
        let next = at
            .checked_add(1)
            .and_then(|n| n.checked_add(u64::from(next_leaf.trailing_zeros())))
            .ok_or_else(|| too_large(stored))?;
        if next > stored {
            break;
        }
        leaf = next_leaf;
        at = next;
    }
    let level = stored.checked_sub(at).ok_or_else(|| too_large(stored))?;
    let level = u8::try_from(level).map_err(|_| too_large(stored))?;
    Ok((level, leaf.checked_shr(u32::from(level)).unwrap_or(0)))
}

/// The tree size whose stored-node count is `count`; `None` when no size has
/// exactly that many nodes.
///
/// # Errors
///
/// [`ProofError::TooLarge`] above the node count of [`MAX_TREE_SIZE`].
pub fn size_from_stored_count(count: u64) -> Result<Option<u64>, ProofError> {
    if count > stored_count(MAX_TREE_SIZE)? {
        return Err(too_large(count));
    }
    // stored_count is strictly increasing: binary search for the largest size
    // whose count does not exceed `count`.
    let (mut low, mut high) = (0u64, MAX_TREE_SIZE);
    while low < high {
        let mid = low.saturating_add(high.saturating_sub(low).div_ceil(2));
        if stored_count(mid)? <= count {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    Ok((stored_count(low)? == count).then_some(low))
}

/// The largest power of two ≤ `n`, for `n ≥ 1` (`n = 0` gives 0).
fn floor_power_of_two(n: u64) -> u64 {
    n.checked_ilog2()
        .map_or(0, |exponent| 1u64.rotate_left(exponent))
}

/// The left subtree width of a range of `n ≥ 2` leaves: the largest power of
/// two strictly below `n`.
fn split(n: u64) -> u64 {
    floor_power_of_two(n.saturating_sub(1))
}

/// The stored ids of the maximal complete subtrees covering `[lo, hi)`, left
/// to right.
fn subtree_ids(lo: u64, hi: u64, ids: &mut Vec<u64>) -> Result<(), ProofError> {
    let mut lo = lo;
    while lo < hi {
        let width = floor_power_of_two(hi.saturating_sub(lo));
        let level = width.trailing_zeros();
        let level_u8 = u8::try_from(level).map_err(|_| too_large(hi))?;
        ids.push(stored_index(level_u8, lo.checked_shr(level).unwrap_or(0))?);
        lo = lo.checked_add(width).ok_or_else(|| too_large(hi))?;
    }
    Ok(())
}

/// How many stored nodes [`subtree_ids`] uses for `[lo, hi)`.
fn subtree_len(lo: u64, hi: u64) -> usize {
    // One node per set bit of the width, since each piece is the largest
    // power of two that fits.
    hi.saturating_sub(lo).count_ones() as usize
}

/// Folds the stored nodes of `[lo, hi)` (as planned by [`subtree_ids`]) into
/// the range's root, returning it and the unused hashes; `None` when the
/// batch is too short.
fn subtree_hash(lo: u64, hi: u64, hashes: &[Hash]) -> Option<(Hash, &[Hash])> {
    let (used, rest) = hashes.split_at_checked(subtree_len(lo, hi))?;
    let (last, others) = used.split_last()?;
    let root = others
        .iter()
        .rev()
        .fold(*last, |right, left| node_hash(left, &right));
    Some((root, rest))
}

/// Plans the inclusion path of leaf `index` in `[lo, hi)` (RFC 6962 PATH).
fn inclusion_ids(lo: u64, hi: u64, index: u64, ids: &mut Vec<u64>) -> Result<(), ProofError> {
    let n = hi.saturating_sub(lo);
    if n <= 1 {
        return Ok(());
    }
    let mid = lo.checked_add(split(n)).ok_or_else(|| too_large(hi))?;
    if index < mid {
        inclusion_ids(lo, mid, index, ids)?;
        subtree_ids(mid, hi, ids)
    } else {
        subtree_ids(lo, mid, ids)?;
        inclusion_ids(mid, hi, index, ids)
    }
}

fn inclusion_path<'a>(
    lo: u64,
    hi: u64,
    index: u64,
    hashes: &'a [Hash],
    path: &mut Vec<Hash>,
) -> Option<&'a [Hash]> {
    let n = hi.saturating_sub(lo);
    if n <= 1 {
        return Some(hashes);
    }
    let mid = lo.checked_add(split(n))?;
    let (sibling, rest) = if index < mid {
        let rest = inclusion_path(lo, mid, index, hashes, path)?;
        subtree_hash(mid, hi, rest)?
    } else {
        let (sibling, rest) = subtree_hash(lo, mid, hashes)?;
        (sibling, inclusion_path(mid, hi, index, rest, path)?)
    };
    path.push(sibling);
    Some(rest)
}

/// Plans the consistency proof from `old` leaves within `[lo, hi)`
/// (RFC 6962 SUBPROOF; `complete` while the old tree is a complete subtree).
fn consistency_ids(
    lo: u64,
    hi: u64,
    old: u64,
    complete: bool,
    ids: &mut Vec<u64>,
) -> Result<(), ProofError> {
    if old == hi {
        return if complete {
            Ok(())
        } else {
            subtree_ids(lo, hi, ids)
        };
    }
    let mid = lo
        .checked_add(split(hi.saturating_sub(lo)))
        .ok_or_else(|| too_large(hi))?;
    if old <= mid {
        consistency_ids(lo, mid, old, complete, ids)?;
        subtree_ids(mid, hi, ids)
    } else {
        subtree_ids(lo, mid, ids)?;
        consistency_ids(mid, hi, old, false, ids)
    }
}

fn consistency_path<'a>(
    lo: u64,
    hi: u64,
    old: u64,
    complete: bool,
    hashes: &'a [Hash],
    path: &mut Vec<Hash>,
) -> Option<&'a [Hash]> {
    if old == hi {
        if complete {
            return Some(hashes);
        }
        let (root, rest) = subtree_hash(lo, hi, hashes)?;
        path.push(root);
        return Some(rest);
    }
    let mid = lo.checked_add(split(hi.saturating_sub(lo)))?;
    let (sibling, rest) = if old <= mid {
        let rest = consistency_path(lo, mid, old, complete, hashes, path)?;
        subtree_hash(mid, hi, rest)?
    } else {
        let (sibling, rest) = subtree_hash(lo, mid, hashes)?;
        (sibling, consistency_path(mid, hi, old, false, rest, path)?)
    };
    path.push(sibling);
    Some(rest)
}

/// The error for a node batch that does not match its plan.
fn malformed(ids: &[u64], hashes: &[Hash]) -> ProofError {
    ProofError::MalformedNodes {
        expected: ids.len(),
        found: hashes.len(),
    }
}

/// Refuses a node batch whose length is not the plan's.
fn expect_len(ids: &[u64], hashes: &[Hash]) -> Result<(), ProofError> {
    if ids.len() == hashes.len() {
        Ok(())
    } else {
        Err(malformed(ids, hashes))
    }
}

/// The stored nodes appending one leaf reads and the nodes it completes:
/// the leaf itself, then one node per subtree it closes.
///
/// ```
/// use doubleentry::log::leaf_hash;
/// use doubleentry::store::Tree;
/// use doubleentry::store::AppendPlan;
///
/// let mut tree = Tree::new();
/// for i in 0u8..3 {
///     tree.push(leaf_hash(&[i]))?;
/// }
/// // Leaf 3 closes the subtrees [2, 4) and [0, 4).
/// let plan = AppendPlan::new(3)?;
/// let siblings: Vec<_> = plan.ids().iter().map(|id| tree.stored()[*id as usize]).collect();
/// let completed = plan.assemble(leaf_hash(&[3]), &siblings)?;
/// tree.push(leaf_hash(&[3]))?;
/// assert_eq!(completed.len(), 3);
/// for (id, hash) in completed {
///     assert_eq!(tree.stored()[id as usize], hash);
/// }
/// # Ok::<(), doubleentry::log::ProofError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendPlan {
    first: u64,
    ids: Vec<u64>,
}

impl AppendPlan {
    /// Plans appending the leaf at `position` (the tree's current size).
    ///
    /// # Errors
    ///
    /// [`ProofError::TooLarge`] when the tree would exceed [`MAX_TREE_SIZE`].
    pub fn new(position: u64) -> Result<Self, ProofError> {
        let size = position.checked_add(1).ok_or_else(|| too_large(position))?;
        check_size(size)?;
        // Each trailing one bit of the position closes one more subtree; its
        // left half is the stored node of the subtree before it.
        let ids = (0..position.trailing_ones())
            .map(|level| {
                let level = u8::try_from(level).map_err(|_| too_large(size))?;
                let sibling = position
                    .checked_shr(u32::from(level))
                    .and_then(|index| index.checked_sub(1))
                    .ok_or_else(|| too_large(size))?;
                stored_index(level, sibling)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            first: stored_count(position)?,
            ids,
        })
    }

    /// The stored node indices to fetch (the left siblings, lowest first).
    #[must_use]
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// The nodes the append stores, as (stored index, hash) in numbering
    /// order: the leaf, then each completed subtree.
    ///
    /// # Errors
    ///
    /// [`ProofError::MalformedNodes`] when the batch does not match the plan.
    pub fn assemble(
        &self,
        leaf_hash: Hash,
        siblings: &[Hash],
    ) -> Result<Vec<(u64, Hash)>, ProofError> {
        expect_len(&self.ids, siblings)?;
        let mut stored = Vec::with_capacity(siblings.len().saturating_add(1));
        stored.push((self.first, leaf_hash));
        let mut current = leaf_hash;
        let mut id = self.first;
        for sibling in siblings {
            current = node_hash(sibling, &current);
            id = id.checked_add(1).ok_or_else(|| too_large(id))?;
            stored.push((id, current));
        }
        Ok(stored)
    }
}

/// The stored nodes the root of a tree of `size` leaves is folded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootPlan {
    size: u64,
    ids: Vec<u64>,
}

impl RootPlan {
    /// Plans the root of the tree of `size` leaves.
    ///
    /// # Errors
    ///
    /// [`ProofError::TooLarge`] above [`MAX_TREE_SIZE`].
    pub fn new(size: u64) -> Result<Self, ProofError> {
        check_size(size)?;
        let mut ids = Vec::new();
        subtree_ids(0, size, &mut ids)?;
        Ok(Self { size, ids })
    }

    /// The stored node indices to fetch, in order.
    #[must_use]
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// Folds the fetched hashes (in [`ids`](Self::ids) order) into the root.
    ///
    /// # Errors
    ///
    /// [`ProofError::MalformedNodes`] when the batch does not match the plan.
    pub fn assemble(&self, hashes: &[Hash]) -> Result<Hash, ProofError> {
        expect_len(&self.ids, hashes)?;
        if self.size == 0 {
            return Ok(empty_root());
        }
        subtree_hash(0, self.size, hashes)
            .map(|(root, _)| root)
            .ok_or_else(|| malformed(&self.ids, hashes))
    }
}

/// The stored nodes an inclusion proof is assembled from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InclusionPlan {
    index: u64,
    size: u64,
    ids: Vec<u64>,
}

impl InclusionPlan {
    /// Plans the inclusion proof of leaf `index` in the tree of `size` leaves.
    ///
    /// # Errors
    ///
    /// [`ProofError::TooLarge`], [`ProofError::EmptyTree`] for size 0, or
    /// [`ProofError::IndexOutOfRange`].
    pub fn new(index: u64, size: u64) -> Result<Self, ProofError> {
        check_size(size)?;
        if size == 0 {
            return Err(ProofError::EmptyTree);
        }
        if index >= size {
            return Err(ProofError::IndexOutOfRange { index, size });
        }
        let mut ids = Vec::new();
        inclusion_ids(0, size, index, &mut ids)?;
        Ok(Self { index, size, ids })
    }

    /// The stored node indices to fetch, in order.
    #[must_use]
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// Folds the fetched hashes into the proof.
    ///
    /// # Errors
    ///
    /// [`ProofError::MalformedNodes`] when the batch does not match the plan.
    pub fn assemble(&self, hashes: &[Hash]) -> Result<InclusionProof, ProofError> {
        expect_len(&self.ids, hashes)?;
        let mut path = Vec::new();
        inclusion_path(0, self.size, self.index, hashes, &mut path)
            .ok_or_else(|| malformed(&self.ids, hashes))?;
        Ok(InclusionProof {
            index: self.index,
            size: self.size,
            path,
        })
    }
}

/// The stored nodes a consistency proof is assembled from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyPlan {
    old: u64,
    new: u64,
    ids: Vec<u64>,
}

impl ConsistencyPlan {
    /// Plans the proof that the tree of `new` leaves extends the one of `old`.
    ///
    /// # Errors
    ///
    /// [`ProofError::TooLarge`], [`ProofError::EmptyOldTree`] for `old = 0`
    /// (a vacuous proof), or [`ProofError::OldLargerThanNew`].
    pub fn new(old: u64, new: u64) -> Result<Self, ProofError> {
        check_size(new)?;
        if old == 0 {
            return Err(ProofError::EmptyOldTree);
        }
        if old > new {
            return Err(ProofError::OldLargerThanNew { old, new });
        }
        let mut ids = Vec::new();
        if old < new {
            consistency_ids(0, new, old, true, &mut ids)?;
        }
        Ok(Self { old, new, ids })
    }

    /// The stored node indices to fetch, in order.
    #[must_use]
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// Folds the fetched hashes into the proof.
    ///
    /// # Errors
    ///
    /// [`ProofError::MalformedNodes`] when the batch does not match the plan.
    pub fn assemble(&self, hashes: &[Hash]) -> Result<ConsistencyProof, ProofError> {
        expect_len(&self.ids, hashes)?;
        let mut path = Vec::new();
        if self.old < self.new {
            consistency_path(0, self.new, self.old, true, hashes, &mut path)
                .ok_or_else(|| malformed(&self.ids, hashes))?;
        }
        Ok(ConsistencyProof {
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
    fn the_numbering_matches_go_tlog() {
        let expected = [
            (0, 0, 0),
            (0, 1, 1),
            (1, 0, 2),
            (0, 2, 3),
            (0, 3, 4),
            (1, 1, 5),
            (2, 0, 6),
        ];
        for (level, index, stored) in expected {
            assert_eq!(stored_index(level, index), Ok(stored));
        }
    }

    #[test]
    fn stored_index_and_split_stored_round_trip() {
        for size in 1..=1024u64 {
            for level in 0..=10u8 {
                let width = 1u64 << level;
                for index in 0..size / width {
                    let stored = stored_index(level, index).unwrap();
                    assert_eq!(split_stored(stored), Ok((level, index)));
                    assert!(stored < stored_count(size).unwrap());
                }
            }
        }
        for stored in 0..4096 {
            let (level, index) = split_stored(stored).unwrap();
            assert_eq!(stored_index(level, index), Ok(stored));
        }
    }

    #[test]
    fn stored_count_is_two_n_minus_popcount_and_counts_appends() {
        let mut running = 0u64;
        for n in 0..4096u64 {
            assert_eq!(stored_count(n), Ok(2 * n - u64::from(n.count_ones())));
            assert_eq!(stored_count(n), Ok(running));
            running += 1 + u64::from((n + 1).trailing_zeros());
        }
    }

    #[test]
    fn size_from_stored_count_inverts_and_refuses_between_sizes() {
        for size in 1..600u64 {
            let count = stored_count(size).unwrap();
            assert_eq!(size_from_stored_count(count), Ok(Some(size)));
            for between in stored_count(size - 1).unwrap() + 1..count {
                assert_eq!(size_from_stored_count(between), Ok(None));
            }
        }
        assert_eq!(size_from_stored_count(0), Ok(Some(0)));
    }

    #[test]
    // PoC: f11_nodes_helpers_panic_on_extreme_sizes
    fn every_function_is_total_at_and_above_the_bound() {
        let top = stored_count(MAX_TREE_SIZE).unwrap();
        assert_eq!(top, 2 * MAX_TREE_SIZE - 1);
        assert_eq!(size_from_stored_count(top), Ok(Some(MAX_TREE_SIZE)));
        assert_eq!(split_stored(top - 1), Ok((62, 0)));
        assert_eq!(stored_index(62, 0), Ok(top - 1));
        assert!(RootPlan::new(MAX_TREE_SIZE).is_ok());
        assert!(InclusionPlan::new(MAX_TREE_SIZE - 1, MAX_TREE_SIZE).is_ok());
        assert!(ConsistencyPlan::new(MAX_TREE_SIZE - 1, MAX_TREE_SIZE).is_ok());
        for size in [MAX_TREE_SIZE + 1, u64::MAX] {
            let refused: Result<(), _> = Err(ProofError::TooLarge { size });
            assert_eq!(stored_count(size).map(|_| ()), refused);
            assert_eq!(RootPlan::new(size).map(|_| ()), refused);
            assert_eq!(InclusionPlan::new(0, size).map(|_| ()), refused);
            assert_eq!(ConsistencyPlan::new(1, size).map(|_| ()), refused);
        }
        assert!(split_stored(top).is_err());
        assert!(split_stored(u64::MAX).is_err());
        assert!(size_from_stored_count(top + 1).is_err());
        assert!(size_from_stored_count(u64::MAX).is_err());
        assert!(stored_index(63, 0).is_err());
        assert!(stored_index(0, MAX_TREE_SIZE).is_err());
        assert!(stored_index(0, u64::MAX).is_err());
    }
}
