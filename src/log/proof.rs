//! Tree heads and the RFC 6962 inclusion and consistency proofs (in RFC
//! 9162's notation, which describes the same tree), plus the
//! sealed-balance proof that names an account.

use time::Date;

use super::{BalanceProof, Hash, leaf_hash, node_hash};
use crate::model::{
    AccountAction, AccountHandle, AccountKind, AccountPath, Balance, Currency, DecodeError, Layer,
    PeriodId, Record, RecordKind,
};

/// The most hashes in a proof path: a tree of at most 2⁶² leaves needs at
/// most 62 inner and border hashes (63 for a consistency proof's seed).
pub const MAX_PROOF_HASHES: usize = 63;

/// Why a proof, plan or tree operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProofError {
    /// A size or index beyond [`MAX_TREE_SIZE`](super::nodes::MAX_TREE_SIZE).
    #[error("{size} is beyond the largest supported tree")]
    TooLarge {
        /// The size or index.
        size: u64,
    },
    /// A leaf index at or beyond the tree size.
    #[error("leaf {index} is not in a tree of {size}")]
    IndexOutOfRange {
        /// The index.
        index: u64,
        /// The tree size.
        size: u64,
    },
    /// An inclusion proof into the empty tree (vacuous).
    #[error("nothing can be included in the empty tree")]
    EmptyTree,
    /// A consistency proof from the empty tree (vacuous).
    #[error("a consistency proof from the empty tree proves nothing")]
    EmptyOldTree,
    /// A consistency proof whose old size exceeds its new size.
    #[error("old size {old} exceeds new size {new}")]
    OldLargerThanNew {
        /// The old size.
        old: u64,
        /// The new size.
        new: u64,
    },
    /// A size beyond the tree that was asked for it.
    #[error("size {size} is beyond the tree's {tree_size} leaves")]
    BeyondTree {
        /// The size asked for.
        size: u64,
        /// The tree's size.
        tree_size: u64,
    },
    /// The proof is for another tree size than the head.
    #[error("the proof is for size {proof}, the head has size {head}")]
    SizeMismatch {
        /// The proof's size.
        proof: u64,
        /// The head's size.
        head: u64,
    },
    /// The path has the wrong number of hashes for its index and size.
    #[error("the path has {found} hashes, {expected} expected")]
    PathLength {
        /// The required length.
        expected: usize,
        /// The length found.
        found: usize,
    },
    /// The recomputed root differs from the expected one.
    #[error("the proof computes root {computed}, expected {expected}")]
    RootMismatch {
        /// The root the proof should reach.
        expected: Hash,
        /// The root it reaches.
        computed: Hash,
    },
    /// A stored-node batch that does not fit its plan.
    #[error("expected {expected} stored nodes, got {found}")]
    MalformedNodes {
        /// The plan's node count.
        expected: usize,
        /// The count supplied.
        found: usize,
    },
    /// Stored nodes that disagree with the leaves they cover.
    #[error("stored node {index} disagrees with the leaves below it")]
    NodeMismatch {
        /// The stored index of the first bad node.
        index: u64,
    },
    /// A balance proof whose leaf index is not the claimed handle.
    #[error("the proof is for leaf {index}, not handle {handle}")]
    HandleMismatch {
        /// The proof's leaf index (or the register record's handle).
        index: u64,
        /// The claimed handle.
        handle: u32,
    },
    /// A balance proof whose tree size is not the seal's account count.
    #[error("the balance tree has {tree_size} leaves, the seal {accounts} accounts")]
    AccountCountMismatch {
        /// The proof's tree size.
        tree_size: u64,
        /// The seal's account count.
        accounts: u32,
    },
    /// The seal has no tree for the (currency, layer).
    #[error("the seal has no {currency} {layer:?} tree")]
    NoSuchTree {
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
    },
    /// The same (handle, currency, layer) was given two balances.
    #[error("handle {handle} was given two balances in one (currency, layer)")]
    DuplicateBalance {
        /// The handle.
        handle: u32,
    },
    /// The balance proof is missing although the seal has the tree, or names
    /// another (currency, layer, handle) than the sealed-balance proof.
    #[error("the balance proof does not match the claim")]
    BalanceProofMismatch,
    /// A leaf of another kind than the proof needs.
    #[error("expected a {expected:?} record, found {found:?}")]
    UnexpectedRecord {
        /// The kind needed.
        expected: RecordKind,
        /// The kind found.
        found: RecordKind,
    },
    /// An account record that changes an account instead of registering it.
    #[error("the account record is a change, not a registration")]
    NotARegistration,
    /// An account registered after the seal's last covered position.
    #[error("the account is registered at {position}, after the seal's last record {through}")]
    AccountAfterSeal {
        /// The register record's position.
        position: u64,
        /// The seal's `through`.
        through: u64,
    },
    /// Proven leaf bytes that do not decode.
    #[error("the proven leaf does not decode: {0}")]
    Decode(DecodeError),
}

/// A log's size and root: the only thing proofs verify against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TreeHead {
    /// The number of leaves.
    pub size: u64,
    /// The RFC 6962 root.
    pub root: Hash,
}

/// One line, `size 42 root <64 hex digits>` (the whole root, so heads can be
/// compared), honouring width, fill and alignment.
impl core::fmt::Display for TreeHead {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        crate::model::pad(f, &format!("size {} root {}", self.size, self.root))
    }
}

/// Whether bit `i` of `x` is set.
fn bit(x: u64, i: usize) -> bool {
    u32::try_from(i)
        .ok()
        .and_then(|i| x.checked_shr(i))
        .is_some_and(|shifted| shifted & 1 == 1)
}

/// The number of significant bits of `x` (Go's `bits.Len64`).
fn bit_len(x: u64) -> usize {
    x.checked_ilog2()
        .map_or(0, |log| (log as usize).saturating_add(1))
}

/// RFC 9162 §2.1.3.2 decomposition: `(inner, border)` path lengths for leaf
/// `index` in a tree of `size > index` leaves.
fn decompose(index: u64, size: u64) -> (usize, usize) {
    let inner = bit_len(index ^ size.saturating_sub(1));
    let above = u32::try_from(inner)
        .ok()
        .and_then(|inner| index.checked_shr(inner))
        .unwrap_or(0);
    (inner, above.count_ones() as usize)
}

/// Hashes `seed` up the inner part of a path, taking the side from `index`.
fn chain_inner(seed: Hash, path: &[Hash], index: u64) -> Hash {
    path.iter().enumerate().fold(seed, |acc, (i, sibling)| {
        if bit(index, i) {
            node_hash(sibling, &acc)
        } else {
            node_hash(&acc, sibling)
        }
    })
}

/// Hashes `seed` up the inner part, only where it is a right child.
fn chain_inner_right(seed: Hash, path: &[Hash], index: u64) -> Hash {
    path.iter().enumerate().fold(seed, |acc, (i, sibling)| {
        if bit(index, i) {
            node_hash(sibling, &acc)
        } else {
            acc
        }
    })
}

/// Hashes `seed` up the border, where it is always the right child.
fn chain_border_right(seed: Hash, path: &[Hash]) -> Hash {
    path.iter()
        .fold(seed, |acc, sibling| node_hash(sibling, &acc))
}

fn expect_root(expected: Hash, computed: Hash) -> Result<(), ProofError> {
    if expected == computed {
        Ok(())
    } else {
        Err(ProofError::RootMismatch { expected, computed })
    }
}

fn expect_length(expected: usize, found: usize) -> Result<(), ProofError> {
    if expected == found && found <= MAX_PROOF_HASHES {
        Ok(())
    } else {
        Err(ProofError::PathLength { expected, found })
    }
}

/// Proof that leaf `index` is in the tree of `size` leaves (RFC 9162 §2.1.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InclusionProof {
    /// The leaf's position.
    pub index: u64,
    /// The tree size the proof is for.
    pub size: u64,
    /// The sibling hashes, leaf to root.
    pub path: Vec<Hash>,
}

impl InclusionProof {
    /// The root this proof computes for `leaf_hash`, after checking the index
    /// and the exact path length.
    ///
    /// # Errors
    ///
    /// [`ProofError::EmptyTree`], [`ProofError::TooLarge`],
    /// [`ProofError::IndexOutOfRange`] or [`ProofError::PathLength`].
    pub fn root_from(&self, leaf_hash: &Hash) -> Result<Hash, ProofError> {
        if self.size == 0 {
            return Err(ProofError::EmptyTree);
        }
        if self.size > super::nodes::MAX_TREE_SIZE {
            return Err(ProofError::TooLarge { size: self.size });
        }
        if self.index >= self.size {
            return Err(ProofError::IndexOutOfRange {
                index: self.index,
                size: self.size,
            });
        }
        let (inner, border) = decompose(self.index, self.size);
        expect_length(inner.saturating_add(border), self.path.len())?;
        let (inner_path, border_path) = self.path.split_at(inner.min(self.path.len()));
        let root = chain_inner(*leaf_hash, inner_path, self.index);
        Ok(chain_border_right(root, border_path))
    }

    /// Verifies that `leaf_hash` is at [`index`](Self::index) in the tree
    /// `head` describes.
    ///
    /// ```
    /// use doubleentry::log::leaf_hash;
    /// use doubleentry::store::Tree;
    ///
    /// let mut tree = Tree::new();
    /// for leaf in [b"a", b"b", b"c"] {
    ///     tree.push(leaf_hash(leaf))?;
    /// }
    /// let proof = tree.inclusion_proof(1, 3)?;
    /// proof.verify(&leaf_hash(b"b"), &tree.head())?;
    /// assert!(proof.verify(&leaf_hash(b"x"), &tree.head()).is_err());
    /// # Ok::<(), doubleentry::log::ProofError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`ProofError::SizeMismatch`] when the head is for another size, the
    /// errors of [`root_from`](Self::root_from), or
    /// [`ProofError::RootMismatch`].
    pub fn verify(&self, leaf_hash: &Hash, head: &TreeHead) -> Result<(), ProofError> {
        if self.size != head.size {
            return Err(ProofError::SizeMismatch {
                proof: self.size,
                head: head.size,
            });
        }
        expect_root(head.root, self.root_from(leaf_hash)?)
    }

    /// Verifies that `leaf_bytes` are in the log — hashing them here, never
    /// trusting a supplied hash — and decodes them.
    ///
    /// # Errors
    ///
    /// The errors of [`verify`](Self::verify), or [`ProofError::Decode`].
    pub fn verify_record<const P: u8>(
        &self,
        head: &TreeHead,
        leaf_bytes: &[u8],
    ) -> Result<Record<P>, ProofError> {
        self.verify(&leaf_hash(leaf_bytes), head)?;
        Record::decode(leaf_bytes).map_err(ProofError::Decode)
    }
}

/// Proof that the tree of `new_size` leaves extends the one of `old_size`
/// (RFC 9162 §2.1.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConsistencyProof {
    /// The earlier size.
    pub old_size: u64,
    /// The later size.
    pub new_size: u64,
    /// The proof hashes.
    pub path: Vec<Hash>,
}

impl ConsistencyProof {
    /// Verifies that `new` extends `old`.
    ///
    /// ```
    /// use doubleentry::log::leaf_hash;
    /// use doubleentry::store::Tree;
    ///
    /// let mut tree = Tree::new();
    /// for i in 0u8..7 {
    ///     tree.push(leaf_hash(&[i]))?;
    /// }
    /// let old = tree.head_at(3)?;
    /// tree.consistency_proof(3, 7)?.verify(&old, &tree.head())?;
    /// # Ok::<(), doubleentry::log::ProofError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`ProofError::SizeMismatch`] for heads of other sizes;
    /// [`ProofError::EmptyOldTree`]; [`ProofError::OldLargerThanNew`];
    /// [`ProofError::TooLarge`]; [`ProofError::PathLength`] (an equal-size
    /// proof must be empty); [`ProofError::RootMismatch`].
    pub fn verify(&self, old: &TreeHead, new: &TreeHead) -> Result<(), ProofError> {
        for (proof, head) in [(self.old_size, old.size), (self.new_size, new.size)] {
            if proof != head {
                return Err(ProofError::SizeMismatch { proof, head });
            }
        }
        let (m, n) = (self.old_size, self.new_size);
        if m == 0 {
            return Err(ProofError::EmptyOldTree);
        }
        if m > n {
            return Err(ProofError::OldLargerThanNew { old: m, new: n });
        }
        if n > super::nodes::MAX_TREE_SIZE {
            return Err(ProofError::TooLarge { size: n });
        }
        if m == n {
            expect_length(0, self.path.len())?;
            return expect_root(new.root, old.root);
        }
        // RFC 9162 §2.1.4.2, in the inner/border form of transparency-dev.
        let (inner, border) = decompose(m.saturating_sub(1), n);
        let shift = m.trailing_zeros();
        let inner = inner.saturating_sub(shift as usize);
        let complete = m.count_ones() == 1;
        let (seed, path) = if complete {
            (old.root, self.path.as_slice())
        } else {
            match self.path.split_first() {
                Some((seed, rest)) => (*seed, rest),
                None => return expect_length(inner.saturating_add(border).saturating_add(1), 0),
            }
        };
        let seed_len = usize::from(!complete);
        expect_length(
            inner.saturating_add(border).saturating_add(seed_len),
            self.path.len(),
        )?;
        let (inner_path, border_path) = path.split_at(inner.min(path.len()));
        let mask = m.saturating_sub(1).checked_shr(shift).unwrap_or(0);
        let old_root = chain_border_right(chain_inner_right(seed, inner_path, mask), border_path);
        expect_root(old.root, old_root)?;
        let new_root = chain_border_right(chain_inner(seed, inner_path, mask), border_path);
        expect_root(new.root, new_root)
    }
}

/// Proof of one account's sealed balance in one (currency, layer), verifiable
/// against the log head alone: the seal's inclusion, the account's register
/// record's inclusion (which names the account behind the handle), and the
/// balance-tree path (or its absence, which proves a zero balance).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SealedBalanceProof<const P: u8 = 2> {
    /// The seal's leaf bytes.
    #[cfg_attr(feature = "serde", serde(with = "crate::model::serde_hex"))]
    pub seal_leaf: Vec<u8>,
    /// The seal's inclusion in the log.
    pub seal_proof: InclusionProof,
    /// The account's register record's leaf bytes.
    #[cfg_attr(feature = "serde", serde(with = "crate::model::serde_hex"))]
    pub account_leaf: Vec<u8>,
    /// The register record's inclusion in the log.
    pub account_proof: InclusionProof,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The account's handle.
    pub handle: AccountHandle,
    /// The balance-tree path; `None` exactly when the seal has no tree for the
    /// (currency, layer).
    pub balance: Option<BalanceProof<P>>,
}

/// An account's sealed balance proven against a log head — what
/// [`SealedBalanceProof::verify`] and a verified balance bundle
/// ([`Proven::Balance`](crate::transparency::Proven::Balance)) establish:
/// the seal is in the head, the balance is the leaf at the account's handle
/// in the seal's tree, and the registration in the head names the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProvenBalance<const P: u8 = 2> {
    /// The head it is proven against.
    pub head: TreeHead,
    /// The account's path, from its registration.
    pub account: AccountPath,
    /// The account's handle.
    pub handle: AccountHandle,
    /// The account's kind.
    pub kind: AccountKind,
    /// The sealed period's id.
    pub period: PeriodId,
    /// The period's first day.
    pub start: Date,
    /// The period's last day.
    pub end: Date,
    /// The seal's log position.
    pub seal_position: u64,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The gross balance through the period's end; its
    /// [`net`](Balance::net) is derived. A zero balance proven by the
    /// absence of a tree does not bind `currency` and `layer`: it is no
    /// evidence the currency was ever used.
    pub balance: Balance<P>,
}

fn unexpected(expected: RecordKind, found: RecordKind) -> ProofError {
    ProofError::UnexpectedRecord { expected, found }
}

impl<const P: u8> SealedBalanceProof<P> {
    /// Verifies the proof against a log head and returns what it proves.
    ///
    /// # Errors
    ///
    /// A [`ProofError`] for any inclusion, record, handle, position or
    /// balance-tree check that fails.
    pub fn verify(&self, head: &TreeHead) -> Result<ProvenBalance<P>, ProofError> {
        let seal = match self.seal_proof.verify_record::<P>(head, &self.seal_leaf)? {
            Record::Seal(seal) => seal,
            other => return Err(unexpected(RecordKind::Seal, other.kind())),
        };
        let account = match self
            .account_proof
            .verify_record::<P>(head, &self.account_leaf)?
        {
            Record::Account(event) => match event.action {
                AccountAction::Register(account) => account,
                AccountAction::Change { .. } => return Err(ProofError::NotARegistration),
            },
            other => return Err(unexpected(RecordKind::Account, other.kind())),
        };
        let handle = self.handle.get();
        if account.handle != self.handle {
            return Err(ProofError::HandleMismatch {
                index: u64::from(account.handle.get()),
                handle,
            });
        }
        let parts = seal.parts();
        if handle >= parts.accounts {
            return Err(ProofError::IndexOutOfRange {
                index: u64::from(handle),
                size: u64::from(parts.accounts),
            });
        }
        if self.account_proof.index > parts.through {
            return Err(ProofError::AccountAfterSeal {
                position: self.account_proof.index,
                through: parts.through,
            });
        }
        let balance = match (&self.balance, seal.tree(self.currency, self.layer)) {
            (None, None) => Balance::ZERO,
            (Some(proof), Some(_))
                if (proof.currency, proof.layer, proof.handle)
                    == (self.currency, self.layer, self.handle) =>
            {
                proof.verify(&seal)?;
                proof.balance
            }
            _ => return Err(ProofError::BalanceProofMismatch),
        };
        Ok(ProvenBalance {
            head: *head,
            account: account.path,
            handle: self.handle,
            kind: account.kind,
            period: parts.period.clone(),
            start: parts.start,
            end: parts.end,
            seal_position: self.seal_proof.index,
            currency: self.currency,
            layer: self.layer,
            balance,
        })
    }
}
