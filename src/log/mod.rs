//! The log as an independent verifier sees it: RFC 6962 SHA-256 hashing,
//! inclusion and consistency proofs against a [`TreeHead`],
//! the seal [`BalanceTree`]s and their proofs, and the leaves a journal is
//! replayed from. Bookkeeping code rarely names these: the façades' `prove_*`
//! methods return the proofs, which verify themselves.
//!
//! One tree, the plain C2SP / RFC 6962 one: `leaf_hash = SHA-256(0x00 ‖ leaf)`,
//! `node = SHA-256(0x01 ‖ left ‖ right)`, empty root `SHA-256("")`. A record's
//! position is its leaf index — one numbering for everything — so stock
//! witnesses, monitors and `transparency-dev/merkle` verify the same proofs.
//!
//! Every proof verifies against a [`TreeHead`] (size and root), never a bare
//! root, and every proof can be produced at any earlier tree size. Vacuous
//! proofs — inclusion into the empty tree, consistency from it — are refused.

pub(crate) mod balance_tree;
pub(crate) mod hash;
pub(crate) mod nodes;
pub(crate) mod proof;
pub(crate) mod tree;

pub use balance_tree::{
    BalanceProof, BalanceTree, balance_leaf_bytes, build_seal_trees, zero_leaf_hash,
};
pub use hash::{Hash, empty_root, leaf_hash, node_hash, root_of};
pub(crate) use proof::TreeHead;
pub use proof::{
    ConsistencyProof, InclusionProof, MAX_PROOF_HASHES, ProofError, ProvenBalance,
    SealedBalanceProof,
};
pub(crate) use tree::Tree;

pub use crate::core::ReplayLeaf;
pub use crate::model::BALANCE_LEAF_KIND;
