//! Seal balance trees: one RFC 6962 tree per (currency, layer) whose leaf `i`
//! is account handle `i`'s gross balance.
//!
//! Every account has a leaf — the canonical zero leaf when it has no postings
//! — so a leaf's index binds it to its handle, two rows for one account are
//! impossible, and "no balance" is provable. Trees are built from the
//! non-zero leaves only, using precomputed zero-subtree hashes, in
//! `O((k + 1) · log n)` hashes for `k` non-zero leaves.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::{Hash, InclusionProof, ProofError, empty_root, leaf_hash, node_hash};
use crate::model::{
    AccountHandle, AmountError, BALANCE_LEAF_KIND, Balance, Currency, Layer, Seal, TreeHeadEntry,
};

/// The leaf bytes of a balance: `0x81 ‖ i128 debit ‖ i128 credit ‖ u64 postings`.
///
/// ```
/// use doubleentry::{Balance, log::balance_leaf_bytes};
///
/// let zero = balance_leaf_bytes(&Balance::<2>::ZERO);
/// assert_eq!(zero[0], 0x81);
/// assert!(zero[1..].iter().all(|b| *b == 0));
/// ```
#[must_use]
pub fn balance_leaf_bytes<const P: u8>(balance: &Balance<P>) -> [u8; 41] {
    let bytes = core::iter::once(BALANCE_LEAF_KIND)
        .chain(balance.debit().to_minor().to_le_bytes())
        .chain(balance.credit().to_minor().to_le_bytes())
        .chain(balance.postings().to_le_bytes());
    let mut leaf = [0u8; 41];
    for (slot, byte) in leaf.iter_mut().zip(bytes) {
        *slot = byte;
    }
    leaf
}

/// The leaf hash of an account without postings.
#[must_use]
pub fn zero_leaf_hash() -> Hash {
    zero_subtree(0)
}

/// `Z₀ … Z₆₂`: the roots of all-zero subtrees of `2^k` leaves.
fn zeros() -> &'static [Hash; 63] {
    static ZEROS: OnceLock<[Hash; 63]> = OnceLock::new();
    ZEROS.get_or_init(|| {
        let mut next = leaf_hash(&balance_leaf_bytes(&Balance::<0>::ZERO));
        core::array::from_fn(|_| {
            let current = next;
            next = node_hash(&current, &current);
            current
        })
    })
}

/// The root of an all-zero subtree of `2^level` leaves.
fn zero_subtree(level: u32) -> Hash {
    match usize::try_from(level)
        .ok()
        .and_then(|level| zeros().get(level))
    {
        Some(hash) => *hash,
        // Beyond the table (never for `u32` account counts): fold directly.
        None => (0..level).fold(zero_subtree(0), |hash, _| node_hash(&hash, &hash)),
    }
}

/// The largest power of two strictly below `n`, for `n ≥ 2`.
fn split(n: u32) -> u32 {
    n.saturating_sub(1)
        .checked_ilog2()
        .map_or(0, |exponent| 1u32.rotate_left(exponent))
}

/// The hashes of the aligned complete subtrees that contain a non-zero leaf,
/// by `(level, index)`: everything else is a zero subtree.
type Nodes = BTreeMap<(u32, u32), Hash>;

/// One (currency, layer) of a seal: `accounts` leaves, the non-zero ones kept.
///
/// ```
/// use doubleentry::{AccountHandle, Balance, Layer, Side, amount, currency::EUR, log::BalanceTree};
///
/// let mut tree: BalanceTree = BalanceTree::new(EUR, Layer::Settled, 3);
/// let cash = Balance::ZERO.post(Side::Debit, amount!("1190.00"))?;
/// let sales = Balance::ZERO.post(Side::Credit, amount!("1190.00"))?;
/// tree.set(AccountHandle::new(0).unwrap(), cash)?;
/// tree.set(AccountHandle::new(2).unwrap(), sales)?;
/// assert_eq!(
///     tree.root().to_string(),
///     "61a2dffcc9633717adfac15db9f40c2f9390bfab45181a7e2dad078cf25451e8",
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct BalanceTree<const P: u8 = 2> {
    currency: Currency,
    layer: Layer,
    accounts: u32,
    leaves: BTreeMap<u32, Balance<P>>,
    /// Built once on the first root or proof after a change, in
    /// `O(k · log accounts)` hashes; every proof then costs `O(log accounts)`.
    nodes: OnceLock<Nodes>,
}

impl<const P: u8> PartialEq for BalanceTree<P> {
    fn eq(&self, other: &Self) -> bool {
        (self.currency, self.layer, self.accounts, &self.leaves)
            == (other.currency, other.layer, other.accounts, &other.leaves)
    }
}

impl<const P: u8> Eq for BalanceTree<P> {}

impl<const P: u8> core::fmt::Debug for BalanceTree<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BalanceTree")
            .field("currency", &self.currency)
            .field("layer", &self.layer)
            .field("accounts", &self.accounts)
            .field("leaves", &self.leaves)
            .finish()
    }
}

impl<const P: u8> BalanceTree<P> {
    /// An all-zero tree of `accounts` leaves.
    #[must_use]
    pub const fn new(currency: Currency, layer: Layer, accounts: u32) -> Self {
        Self {
            currency,
            layer,
            accounts,
            leaves: BTreeMap::new(),
            nodes: OnceLock::new(),
        }
    }

    /// Sets a handle's balance; a zero balance removes it.
    ///
    /// # Errors
    ///
    /// [`ProofError::IndexOutOfRange`] for a handle not below `accounts`.
    pub fn set(&mut self, handle: AccountHandle, balance: Balance<P>) -> Result<(), ProofError> {
        let index = handle.get();
        if index >= self.accounts {
            return Err(ProofError::IndexOutOfRange {
                index: u64::from(index),
                size: u64::from(self.accounts),
            });
        }
        if balance.is_zero() {
            self.leaves.remove(&index);
        } else {
            self.leaves.insert(index, balance);
        }
        self.nodes.take();
        Ok(())
    }

    /// The currency.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// The layer.
    #[must_use]
    pub const fn layer(&self) -> Layer {
        self.layer
    }

    /// The number of leaves.
    #[must_use]
    pub const fn accounts(&self) -> u32 {
        self.accounts
    }

    /// The RFC 6962 root over all `accounts` leaves.
    #[must_use]
    pub fn root(&self) -> Hash {
        if self.accounts == 0 {
            return empty_root();
        }
        let nodes = self.nodes();
        range_root(0, self.accounts, &mut |level, index| {
            lookup(nodes, level, index)
        })
    }

    /// The component-wise sum of every leaf.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] when a total leaves its range.
    pub fn totals(&self) -> Result<Balance<P>, AmountError> {
        self.leaves
            .values()
            .try_fold(Balance::ZERO, |sum, balance| sum.checked_add(balance))
    }

    /// The seal entry for this tree: currency, layer, root and totals.
    ///
    /// # Errors
    ///
    /// As [`totals`](Self::totals).
    pub fn head(&self) -> Result<TreeHeadEntry<P>, AmountError> {
        Ok(TreeHeadEntry {
            currency: self.currency,
            layer: self.layer,
            root: self.root(),
            totals: self.totals()?,
        })
    }

    /// The proof of one handle's balance (the zero leaf for an account
    /// without postings).
    ///
    /// # Errors
    ///
    /// [`ProofError::IndexOutOfRange`] for a handle not below `accounts`.
    pub fn prove(&self, handle: AccountHandle) -> Result<BalanceProof<P>, ProofError> {
        let index = handle.get();
        if index >= self.accounts {
            return Err(ProofError::IndexOutOfRange {
                index: u64::from(index),
                size: u64::from(self.accounts),
            });
        }
        let mut path = Vec::new();
        let nodes = self.nodes();
        walk_path(0, self.accounts, index, &mut path, &mut |level, at| {
            lookup(nodes, level, at)
        });
        Ok(BalanceProof {
            currency: self.currency,
            layer: self.layer,
            handle,
            balance: self.leaves.get(&index).copied().unwrap_or(Balance::ZERO),
            path: InclusionProof {
                index: u64::from(index),
                size: u64::from(self.accounts),
                path,
            },
        })
    }

    /// The aligned non-zero subtrees, built bottom-up from the non-zero
    /// leaves: one hash per node that has a non-zero leaf below it and lies
    /// inside the tree.
    fn nodes(&self) -> &Nodes {
        self.nodes.get_or_init(|| {
            let mut nodes: Nodes = self
                .leaves
                .iter()
                .map(|(index, balance)| ((0, *index), leaf_hash(&balance_leaf_bytes(balance))))
                .collect();
            let mut level_nodes: Vec<u32> = self.leaves.keys().copied().collect();
            for level in 0..u32::BITS {
                let width = 2u64.pow(level.saturating_add(1));
                let mut parents: Vec<u32> = level_nodes.iter().map(|index| index / 2).collect();
                parents.dedup();
                // Only nodes whose whole range lies inside the tree are used.
                parents.retain(|parent| {
                    u64::from(*parent).saturating_add(1).saturating_mul(width)
                        <= u64::from(self.accounts)
                });
                for parent in &parents {
                    let child = |index: u32| {
                        nodes
                            .get(&(level, index))
                            .copied()
                            .unwrap_or_else(|| zero_subtree(level))
                    };
                    // A retained parent lies inside the tree, so its children
                    // are below `accounts` and the products cannot saturate.
                    let left = parent.saturating_mul(2);
                    let hash = node_hash(&child(left), &child(left.saturating_add(1)));
                    nodes.insert((level.saturating_add(1), *parent), hash);
                }
                if parents.is_empty() {
                    break;
                }
                level_nodes = parents;
            }
            nodes
        })
    }

    /// Every non-zero leaf, by handle.
    pub(crate) fn leaves(&self) -> impl Iterator<Item = (u32, Balance<P>)> + '_ {
        self.leaves
            .iter()
            .map(|(index, balance)| (*index, *balance))
    }

    /// One handle's leaf (zero without postings).
    pub(crate) fn leaf(&self, handle: AccountHandle) -> Balance<P> {
        self.leaves
            .get(&handle.get())
            .copied()
            .unwrap_or(Balance::ZERO)
    }

    /// The stored nodes of the tree: the hash of every aligned subtree that
    /// lies inside it and holds a non-zero leaf, by `(level, index)`
    /// (level 0: the non-zero leaves). Every other aligned subtree is a zero
    /// subtree, so these are all a proof needs ([`proof_nodes`]).
    pub(crate) fn stored_nodes(&self) -> impl Iterator<Item = (u32, u32, Hash)> + '_ {
        self.nodes()
            .iter()
            .map(|((level, index), hash)| (*level, *index, *hash))
    }

    /// The tree of `accounts` leaves known only by the stored nodes a proof
    /// of `handle` reads ([`proof_nodes`]) and that handle's balance: what
    /// [`prove`](Self::prove) needs to prove it in `O(log accounts)`.
    pub(crate) fn from_nodes(
        currency: Currency,
        layer: Layer,
        accounts: u32,
        leaf: (AccountHandle, Balance<P>),
        nodes: impl IntoIterator<Item = ((u32, u32), Hash)>,
    ) -> Self {
        let mut leaves = BTreeMap::new();
        if !leaf.1.is_zero() {
            leaves.insert(leaf.0.get(), leaf.1);
        }
        Self {
            currency,
            layer,
            accounts,
            leaves,
            nodes: OnceLock::from(nodes.into_iter().collect::<Nodes>()),
        }
    }
}

/// A stored node, or the zero subtree of its level when none is stored.
fn lookup(nodes: &Nodes, level: u32, index: u32) -> Hash {
    nodes
        .get(&(level, index))
        .copied()
        .unwrap_or_else(|| zero_subtree(level))
}

/// The root of leaves `[lo, lo + n)`, `n ≥ 1`, from the aligned subtrees
/// `node(level, index)` answers. Power-of-two ranges are aligned in RFC
/// 6962's decomposition, so they are looked up; only the ragged right edge
/// is hashed here.
fn range_root(lo: u32, n: u32, node: &mut dyn FnMut(u32, u32) -> Hash) -> Hash {
    if n.is_power_of_two() {
        let level = n.trailing_zeros();
        return node(level, lo.checked_shr(level).unwrap_or(0));
    }
    let k = split(n);
    let left = range_root(lo, k, node);
    let right = range_root(lo.saturating_add(k), n.saturating_sub(k), node);
    node_hash(&left, &right)
}

/// The RFC 6962 inclusion path of `index` within `[lo, lo + n)`.
fn walk_path(
    lo: u32,
    n: u32,
    index: u32,
    out: &mut Vec<Hash>,
    node: &mut dyn FnMut(u32, u32) -> Hash,
) {
    if n <= 1 {
        return;
    }
    let k = split(n);
    let mid = lo.saturating_add(k);
    if index < mid {
        walk_path(lo, k, index, out, node);
        out.push(range_root(mid, n.saturating_sub(k), node));
    } else {
        walk_path(mid, n.saturating_sub(k), index, out, node);
        out.push(range_root(lo, k, node));
    }
}

/// The aligned subtrees, `(level, index)`, a proof of `handle` in a tree of
/// `accounts` leaves reads: `O(log² accounts)` at most.
pub(crate) fn proof_nodes(accounts: u32, handle: AccountHandle) -> Vec<(u32, u32)> {
    let mut ids = Vec::new();
    let mut path = Vec::new();
    walk_path(0, accounts, handle.get(), &mut path, &mut |level, index| {
        ids.push((level, index));
        zero_subtree(level)
    });
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Proof of one handle's balance in one (currency, layer) of a seal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceProof<const P: u8 = 2> {
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The account's handle.
    pub handle: AccountHandle,
    /// The account's gross balance.
    pub balance: Balance<P>,
    /// The leaf's path in the balance tree.
    pub path: InclusionProof,
}

impl<const P: u8> BalanceProof<P> {
    /// Verifies the balance against the seal's tree for its (currency, layer):
    /// the leaf index is the handle, the tree size is the seal's account
    /// count, and the path reaches the committed root.
    ///
    /// # Errors
    ///
    /// [`ProofError::NoSuchTree`], [`ProofError::HandleMismatch`],
    /// [`ProofError::AccountCountMismatch`], or the errors of
    /// [`InclusionProof::root_from`] and [`ProofError::RootMismatch`].
    pub fn verify(&self, seal: &Seal<P>) -> Result<(), ProofError> {
        let tree = seal
            .tree(self.currency, self.layer)
            .ok_or(ProofError::NoSuchTree {
                currency: self.currency,
                layer: self.layer,
            })?;
        if self.path.index != u64::from(self.handle.get()) {
            return Err(ProofError::HandleMismatch {
                index: self.path.index,
                handle: self.handle.get(),
            });
        }
        let accounts = seal.parts().accounts;
        if self.path.size != u64::from(accounts) {
            return Err(ProofError::AccountCountMismatch {
                tree_size: self.path.size,
                accounts,
            });
        }
        let computed = self
            .path
            .root_from(&leaf_hash(&balance_leaf_bytes(&self.balance)))?;
        if computed == tree.root {
            Ok(())
        } else {
            Err(ProofError::RootMismatch {
                expected: tree.root,
                computed,
            })
        }
    }
}

/// Builds one tree per (currency, layer) that has a non-zero balance, in
/// ascending (currency, layer) order — the trees a seal commits to.
///
/// ```
/// use doubleentry::{AccountHandle, Balance, Layer, Side, amount, currency::EUR, log::build_seal_trees};
///
/// let cash: Balance = Balance::ZERO.post(Side::Debit, amount!("10.00"))?;
/// let trees = build_seal_trees(2, [(AccountHandle::new(0).unwrap(), EUR, Layer::Settled, cash)])?;
/// assert_eq!(trees.len(), 1);
/// assert_eq!(trees[0].totals()?, cash);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// [`ProofError::IndexOutOfRange`] for a handle not below `accounts`;
/// [`ProofError::DuplicateBalance`] for a (handle, currency, layer) given twice.
pub fn build_seal_trees<const P: u8>(
    accounts: u32,
    balances: impl IntoIterator<Item = (AccountHandle, Currency, Layer, Balance<P>)>,
) -> Result<Vec<BalanceTree<P>>, ProofError> {
    let mut trees: BTreeMap<(Currency, Layer), BalanceTree<P>> = BTreeMap::new();
    for (handle, currency, layer, balance) in balances {
        if balance.is_zero() {
            continue;
        }
        let tree = trees
            .entry((currency, layer))
            .or_insert_with(|| BalanceTree::new(currency, layer, accounts));
        if tree.leaves.contains_key(&handle.get()) {
            return Err(ProofError::DuplicateBalance {
                handle: handle.get(),
            });
        }
        tree.set(handle, balance)?;
    }
    Ok(trees.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::hash::counter;
    use crate::log::root_of;
    use crate::model::{Amount, EUR, SealParts, Side};

    fn balance(minor: i128) -> Balance {
        Balance::ZERO
            .post(Side::Debit, Amount::from_minor(minor).unwrap())
            .unwrap()
    }

    #[test]
    fn a_million_accounts_with_a_thousand_balances_take_under_50000_hashes() {
        let accounts = 1_000_000u32;
        let mut tree = BalanceTree::new(EUR, Layer::Settled, accounts);
        let handles: Vec<u32> = (0..1000u32).map(|i| i * 997 + 3).collect();
        for (i, handle) in handles.iter().enumerate() {
            tree.set(AccountHandle::new(*handle).unwrap(), balance(i as i128 + 1))
                .unwrap();
        }
        zeros();
        let (root, hashes) = counter::count(|| tree.root());
        assert!(hashes < 50_000, "{hashes} hashes");

        let head = tree.head().unwrap();
        let seal = Seal::new(SealParts {
            period: crate::model::Label::new("p").unwrap(),
            start: time::macros::date!(2026 - 01 - 01),
            end: time::macros::date!(2026 - 01 - 31),
            through: 1,
            accounts,
            trees: vec![head],
        })
        .unwrap();
        assert_eq!(seal.tree(EUR, Layer::Settled).unwrap().root, root);
        let zero_handles = (0..1000u32)
            .map(|i| i * 991 + 1)
            .filter(|h| !handles.contains(h));
        for handle in handles.iter().copied().chain(zero_handles) {
            let proof = tree.prove(AccountHandle::new(handle).unwrap()).unwrap();
            proof.verify(&seal).unwrap();
        }
    }

    #[test]
    fn sparse_roots_equal_the_explicit_tree() {
        for accounts in 1..=40u32 {
            let mut tree = BalanceTree::<2>::new(EUR, Layer::Settled, accounts);
            let mut explicit = Vec::new();
            for handle in 0..accounts {
                let b = if handle % 3 == 1 {
                    balance(i128::from(handle))
                } else {
                    Balance::ZERO
                };
                tree.set(AccountHandle::new(handle).unwrap(), b).unwrap();
                explicit.push(leaf_hash(&balance_leaf_bytes(&b)));
            }
            assert_eq!(tree.root(), root_of(&explicit), "accounts = {accounts}");
        }
    }
}
