//! The in-memory RFC 6962 log.

use super::nodes::{
    AppendPlan, ConsistencyPlan, InclusionPlan, MAX_TREE_SIZE, RootPlan, size_from_stored_count,
    stored_count, stored_index,
};
use super::{ConsistencyProof, Hash, InclusionProof, ProofError, TreeHead};

/// The node count of the smallest tree that stores more than `count` nodes
/// (what a count between two tree sizes falls short of).
fn next_stored_count(count: u64) -> Result<usize, ProofError> {
    // `stored_count(n) = 2n − popcount(n)` is strictly increasing and at
    // least `2n − 64`: the answer is within 33 sizes of `count / 2`, and a
    // size above `MAX_TREE_SIZE` ends the search with `TooLarge`.
    let mut size = count >> 1;
    loop {
        let stored = stored_count(size)?;
        if stored > count {
            return Ok(usize::try_from(stored).unwrap_or(usize::MAX));
        }
        size = size.saturating_add(1);
    }
}

/// An append-only RFC 6962 tree of leaf hashes, kept as its stored nodes in
/// Go `tlog` order.
///
/// Appending is amortised `O(1)` hashes; heads and proofs are `O(log n)` at
/// **any historical size**, because stored nodes never change. Stores keep the
/// same nodes ([`push_with`](Self::push_with) visits each new one) and run the
/// same plans.
///
/// ```
/// use doubleentry::store::Tree;
/// use doubleentry::{log::{leaf_hash, root_of}};
///
/// let leaves: Vec<_> = (0u8..5).map(|i| leaf_hash(&[i])).collect();
/// let mut tree = Tree::new();
/// for leaf in &leaves {
///     tree.push(*leaf)?;
/// }
/// assert_eq!(tree.head().root, root_of(&leaves));
/// assert_eq!(tree.head_at(3)?.root, root_of(&leaves[..3]));
/// tree.inclusion_proof(2, 3)?.verify(&leaves[2], &tree.head_at(3)?)?;
/// # Ok::<(), doubleentry::log::ProofError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    nodes: Vec<Hash>,
    size: u64,
}

impl Tree {
    /// The empty tree.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nodes: Vec::new(),
            size: 0,
        }
    }

    /// Rebuilds a tree from its stored nodes.
    ///
    /// # Errors
    ///
    /// [`ProofError::MalformedNodes`] when the count is not that of any tree
    /// size, [`ProofError::NodeMismatch`] when an interior node disagrees with
    /// the leaves below it.
    pub fn from_stored(nodes: Vec<Hash>) -> Result<Self, ProofError> {
        let count =
            u64::try_from(nodes.len()).map_err(|_| ProofError::TooLarge { size: u64::MAX })?;
        let Some(size) = size_from_stored_count(count)? else {
            return Err(ProofError::MalformedNodes {
                expected: next_stored_count(count)?,
                found: nodes.len(),
            });
        };
        let mut rebuilt = Self::new();
        for index in 0..size {
            let leaf = Self::node(&nodes, stored_index(0, index)?)?;
            rebuilt.push(leaf)?;
        }
        if let Some(index) = rebuilt.nodes.iter().zip(&nodes).position(|(a, b)| a != b) {
            return Err(ProofError::NodeMismatch {
                index: u64::try_from(index).unwrap_or(u64::MAX),
            });
        }
        Ok(rebuilt)
    }

    /// The number of leaves.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Appends a leaf hash and returns its position.
    ///
    /// # Errors
    ///
    /// [`ProofError::TooLarge`] beyond [`MAX_TREE_SIZE`] leaves.
    pub fn push(&mut self, leaf_hash: Hash) -> Result<u64, ProofError> {
        self.push_with(leaf_hash, |_, _| {})
    }

    /// Appends a leaf hash, calling `visit(stored_index, hash)` for every node
    /// this completes (the leaf first), in numbering order — what a store
    /// persists.
    ///
    /// # Errors
    ///
    /// [`ProofError::TooLarge`] beyond [`MAX_TREE_SIZE`] leaves.
    pub fn push_with(
        &mut self,
        leaf_hash: Hash,
        mut visit: impl FnMut(u64, &Hash),
    ) -> Result<u64, ProofError> {
        let position = self.size;
        let size = position
            .checked_add(1)
            .filter(|size| *size <= MAX_TREE_SIZE)
            .ok_or(ProofError::TooLarge { size: position })?;
        let plan = AppendPlan::new(position)?;
        let siblings = self.fetch(plan.ids())?;
        for (index, hash) in plan.assemble(leaf_hash, &siblings)? {
            visit(index, &hash);
            self.nodes.push(hash);
        }
        self.size = size;
        Ok(position)
    }

    /// The stored nodes, in numbering order.
    #[must_use]
    pub fn stored(&self) -> &[Hash] {
        &self.nodes
    }

    /// The current head.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        // The current size is always within the bound, and its plan uses only
        // stored nodes, so this cannot fail; the empty root is its own head.
        self.head_at(self.size).unwrap_or(TreeHead {
            size: 0,
            root: super::empty_root(),
        })
    }

    /// The head the tree had at `size` leaves.
    ///
    /// # Errors
    ///
    /// [`ProofError::BeyondTree`] for a size beyond the tree.
    pub fn head_at(&self, size: u64) -> Result<TreeHead, ProofError> {
        self.check_size(size)?;
        let plan = RootPlan::new(size)?;
        let root = plan.assemble(&self.fetch(plan.ids())?)?;
        Ok(TreeHead { size, root })
    }

    /// The inclusion proof of leaf `index` in the tree as it was at `size`
    /// leaves.
    ///
    /// # Errors
    ///
    /// [`ProofError::BeyondTree`], and the errors of [`InclusionPlan::new`].
    pub fn inclusion_proof(&self, index: u64, size: u64) -> Result<InclusionProof, ProofError> {
        self.check_size(size)?;
        let plan = InclusionPlan::new(index, size)?;
        plan.assemble(&self.fetch(plan.ids())?)
    }

    /// The proof that the tree at `new_size` extends the tree at `old_size`.
    ///
    /// # Errors
    ///
    /// [`ProofError::BeyondTree`], and the errors of [`ConsistencyPlan::new`].
    pub fn consistency_proof(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, ProofError> {
        self.check_size(new_size)?;
        let plan = ConsistencyPlan::new(old_size, new_size)?;
        plan.assemble(&self.fetch(plan.ids())?)
    }

    fn check_size(&self, size: u64) -> Result<(), ProofError> {
        if size > self.size {
            Err(ProofError::BeyondTree {
                size,
                tree_size: self.size,
            })
        } else {
            Ok(())
        }
    }

    fn node(nodes: &[Hash], index: u64) -> Result<Hash, ProofError> {
        usize::try_from(index)
            .ok()
            .and_then(|index| nodes.get(index))
            .copied()
            .ok_or(ProofError::MalformedNodes {
                expected: usize::try_from(index)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1),
                found: nodes.len(),
            })
    }

    fn fetch(&self, ids: &[u64]) -> Result<Vec<Hash>, ProofError> {
        ids.iter().map(|id| Self::node(&self.nodes, *id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{leaf_hash, nodes::stored_count, root_of};

    #[test]
    fn push_with_visits_every_new_node_in_order() {
        let mut tree = Tree::new();
        let mut seen = Vec::new();
        for i in 0u8..20 {
            tree.push_with(leaf_hash(&[i]), |index, hash| seen.push((index, *hash)))
                .unwrap();
            assert_eq!(seen.len() as u64, stored_count(tree.size()).unwrap());
        }
        for (expected, (index, hash)) in seen.iter().enumerate() {
            assert_eq!(*index, expected as u64);
            assert_eq!(tree.stored()[expected], *hash);
        }
    }

    #[test]
    fn from_stored_rebuilds_and_refuses_bad_covers() {
        let mut tree = Tree::new();
        let leaves: Vec<_> = (0u8..13).map(|i| leaf_hash(&[i])).collect();
        for leaf in &leaves {
            tree.push(*leaf).unwrap();
        }
        let rebuilt = Tree::from_stored(tree.stored().to_vec()).unwrap();
        assert_eq!(rebuilt.head(), tree.head());
        assert_eq!(rebuilt.head().root, root_of(&leaves));

        // 21 nodes lie between the covers of 11 (19) and 12 (22) leaves.
        let mut short = tree.stored().to_vec();
        short.truncate(21);
        assert!(matches!(
            Tree::from_stored(short),
            Err(ProofError::MalformedNodes { .. })
        ));

        let mut forged = tree.stored().to_vec();
        forged[2] = leaf_hash(b"forged");
        assert_eq!(
            Tree::from_stored(forged),
            Err(ProofError::NodeMismatch { index: 2 })
        );
    }

    #[test]
    fn sizes_beyond_the_tree_are_refused() {
        let mut tree = Tree::new();
        tree.push(leaf_hash(b"a")).unwrap();
        assert_eq!(
            tree.head_at(2),
            Err(ProofError::BeyondTree {
                size: 2,
                tree_size: 1
            })
        );
        assert!(tree.inclusion_proof(0, 2).is_err());
        assert!(tree.consistency_proof(1, 2).is_err());
        assert_eq!(Tree::new().head().root, crate::log::empty_root());
    }
}
