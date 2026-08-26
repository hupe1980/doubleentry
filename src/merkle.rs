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

use smallvec::SmallVec;

use crate::hash::{Hash, tagged};

pub mod nodes;

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
#[cfg(test)]
fn split_point(n: usize) -> usize {
    debug_assert!(n > 1);
    let mut k = 1usize;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// The Merkle Tree Hash of a run of leaf payloads.
///
/// RFC 6962's definition, transcribed. Nothing in the engine calls it: the log
/// answers from stored nodes in `O(log n)` instead. It is kept, and kept
/// test-only, as the **executable specification** the fast path is checked
/// against — an optimisation with no independent statement of what it is
/// optimising is an optimisation nobody can audit.
///
/// See the `node_equivalence` tests, which compare the two exhaustively over
/// every size and position, and the golden vectors, which pin the answer they
/// agree on.
#[cfg(test)]
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
    /// A consistency proof was requested from the empty tree.
    ///
    /// Every log extends the empty one, so such a proof would be true of any
    /// history at all and would constrain neither the new root nor anything
    /// else. Returning one is worse than refusing: it verifies, and a verifier
    /// who did not know to ask cannot tell it apart from a real check.
    ///
    /// [RFC 9162 §2.1.4.2](https://www.rfc-editor.org/rfc/rfc9162#section-2.1.4.2)
    /// defines consistency only for `0 < m <= n`, and Go's `tlog.ProveTree`
    /// refuses `m < 1` for the same reason. Archive a head with at least one
    /// entry in it.
    #[error("a consistency proof from the empty tree constrains nothing; archive a non-empty head")]
    EmptyOldTree {
        /// The later size the proof was requested against.
        new_size: u64,
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
    /// Verifies this proof against a leaf payload and a published tree head.
    ///
    /// A `true` answer establishes the whole claim, position included:
    /// **`leaf_payload` is the leaf at [`leaf_index`](Self::leaf_index) of the
    /// log of `head.size` entries whose Merkle root is `head.root`**. The shape
    /// of the path is checked as strictly as its contents — a sibling added,
    /// dropped, duplicated or reordered fails — and exactly one labelling of a
    /// given path can verify, so the index a caller reads back afterwards is a
    /// checked number rather than the prover's.
    ///
    /// The head is what makes that last part true, which is why there is no
    /// root-only form of this function. [`leaf_index`](Self::leaf_index) and
    /// [`tree_size`](Self::tree_size) *steer* the walk rather than being checked
    /// by it, and neighbouring pairs steer it identically: against a bare root
    /// the proof for leaf 1 of a two-leaf log is accepted unchanged as the proof
    /// for leaf 2 of three. Pinning the size to a head a verifier already trusts
    /// removes the aliasing, and costs one integer comparison.
    ///
    /// Returns `false` on any inconsistency rather than distinguishing failure
    /// modes: a caller cannot act differently on a malformed proof than on a
    /// forged one.
    #[must_use]
    pub fn verify(&self, leaf_payload: &Hash, head: &TreeHead) -> bool {
        if self.tree_size != head.size || self.leaf_index >= self.tree_size {
            return false;
        }
        let root = &head.root;
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
    /// Verifies that the log at `old` is a prefix of the log at `new`.
    ///
    /// Both ends are (size, root) pairs, and both halves of each are checked, so
    /// the snapshot labels a caller reads back are as trustworthy as the roots.
    ///
    /// Taking heads rather than bare roots is what makes that true.
    /// [`new_size`](Self::new_size) steers the walk and neighbouring values
    /// steer it identically, so against a bare root a one-to-three proof is
    /// accepted as a one-to-four. Comparing both sizes explicitly costs nothing
    /// and spares a verifier deriving that from the index arithmetic.
    ///
    /// # From the empty tree, never
    ///
    /// `old_size == 0` is refused. The statement it makes is true of every
    /// history that has ever existed — each one extends the empty tree — so an
    /// auditor who archived a head *before the first entry* would get `true`
    /// back from a check that examined nothing. Refused at both ends, since the
    /// fields are public and a proof arrives over a wire: see
    /// [`ProofError::EmptyOldTree`].
    ///
    /// ```
    /// # use doubleentry::{MerkleLog, TreeHead, Hash, ProofError};
    /// # let mut log = MerkleLog::new();
    /// # for i in 0..6u64 { log.append(Hash::from_bytes([i as u8; 32])); }
    /// // Refused at construction …
    /// assert!(matches!(
    ///     log.consistency_proof(0),
    ///     Err(ProofError::EmptyOldTree { new_size: 6 })
    /// ));
    ///
    /// // … and refused again on the wire, where the fields are public.
    /// let forged = doubleentry::ConsistencyProof { old_size: 0, new_size: 6, path: vec![] };
    /// let empty = TreeHead { size: 0, root: doubleentry::merkle::empty_root() };
    /// assert!(!forged.verify(&empty, &log.head()));
    ///
    /// // One entry in, a real proof verifies.
    /// let one = log.head_at(1)?;
    /// assert!(log.consistency_proof(1)?.verify(&one, &log.head()));
    /// # Ok::<(), ProofError>(())
    /// ```
    #[must_use]
    pub fn verify(&self, old: &TreeHead, new: &TreeHead) -> bool {
        // A proof from the empty tree is true of every log there has ever been.
        // It is refused rather than answered, so a `true` from this function
        // always means something was checked.
        if self.old_size == 0 {
            return false;
        }
        if self.old_size != old.size || self.new_size != new.size {
            return false;
        }
        let (old_root, new_root) = (&old.root, &new.root);
        if self.old_size > self.new_size {
            return false;
        }
        if self.old_size == self.new_size {
            return self.path.is_empty() && old_root == new_root;
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

    /// Restores a proof from the bytes
    /// [`to_canonical_bytes`](crate::Canonical::to_canonical_bytes) wrote.
    ///
    /// A [`Seal`](crate::Seal) carries one, so a durable backend has to write it
    /// to a column and read it back. The encoding is the crate's own — fixed
    /// width, length-prefixed, one representation per value — rather than a
    /// general-purpose format whose framing is free to change between versions.
    ///
    /// The decode is strict: trailing bytes, a truncated path, or a declared
    /// element count the buffer cannot satisfy are all refused. It cannot check
    /// that the *hashes* are right — that is [`Self::verify`]'s job, and a seal
    /// chain runs it on every link.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedProof`] when the buffer is not exactly one encoded
    /// proof.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, MalformedProof> {
        let mut r = crate::canonical::CanonicalReader::new(bytes);
        let old_size = r.u64().ok_or(MalformedProof)?;
        let new_size = r.u64().ok_or(MalformedProof)?;
        let count = r.u64().ok_or(MalformedProof)?;
        let count = usize::try_from(count).map_err(|_| MalformedProof)?;
        // Bounded by what the buffer can actually hold before allocating, so a
        // hostile count cannot ask for a terabyte of `Vec`.
        if count.saturating_mul(Hash::LEN) > r.remaining() {
            return Err(MalformedProof);
        }
        let mut path = Vec::with_capacity(count);
        for _ in 0..count {
            path.push(Hash::from_bytes(
                r.array::<{ Hash::LEN }>().ok_or(MalformedProof)?,
            ));
        }
        if !r.is_exhausted() {
            return Err(MalformedProof);
        }
        Ok(Self {
            old_size,
            new_size,
            path,
        })
    }
}

impl crate::canonical::Canonical for ConsistencyProof {
    fn encode(&self, w: &mut crate::canonical::CanonicalWriter) {
        w.u64(self.old_size);
        w.u64(self.new_size);
        w.seq(self.path.iter(), |w, h| {
            w.fixed(h.as_bytes());
        });
    }
}

/// A byte buffer is not exactly one encoded [`ConsistencyProof`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a well-formed consistency proof encoding")]
pub struct MalformedProof;

/// The perfect-subtree roots covering a log.
///
/// One node per set bit in the size, so `O(log n)`, and appending merges
/// equal-height neighbours so it is amortised `O(1)`. Folding it right to left
/// gives the root.
///
/// # Not a storage format
///
/// A backend should not persist this: the cover is exactly the nodes a
/// [`nodes::RootPlan`] names, so a store that keeps its tree reads the cover back
/// in `O(log n)` rather than maintaining a second copy that could stop agreeing
/// with the first.
///
/// Its use is the **write** side. The merges
/// [`push_recording`](Self::push_recording) performs are precisely the nodes an
/// append has to store, so a backend gets them without reading anything back.
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

    /// Restores an accumulator from a subtree cover read out of storage.
    ///
    /// `subtrees` must be ordered largest-height first, as
    /// [`MerkleAccumulator::subtrees`] returns them — which is also the order a
    /// [`nodes::RootPlan`] names them in.
    ///
    /// The shape is checked rather than trusted. A log of `size` leaves has
    /// exactly one perfect-subtree decomposition — one subtree per set bit in
    /// `size` — so a row lost, duplicated, reordered, or written against a
    /// different size is a shape error, and this catches it. It cannot catch a
    /// node whose *hash* is wrong; that is what
    /// [`MerkleLog::verify_structure`] is for.
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
        self.push_recording(payload).0
    }

    /// Appends a leaf payload, returning its index **and the nodes to store**.
    ///
    /// The nodes come out in [`nodes`] storage order — the leaf hash, then one
    /// per subtree that just closed — so a backend appends them at consecutive
    /// positions starting at [`nodes::count`] of the previous size. It never has
    /// to read anything back to write them, because the left siblings each merge
    /// needs are exactly what the subtree stack already holds.
    ///
    /// That is the whole reason this is on the accumulator rather than beside
    /// it: the merges are already happening here, and computing them twice is
    /// how two implementations of one tree start to disagree.
    ///
    /// ```
    /// # use doubleentry::{Hash, MerkleAccumulator};
    /// # use doubleentry::merkle::nodes;
    /// let mut accumulator = MerkleAccumulator::new();
    /// // Record 0 closes nothing: one leaf hash.
    /// assert_eq!(accumulator.push_recording(Hash::from_bytes([0; 32])).1.len(), 1);
    /// // Record 1 closes the pair above it: a leaf hash and one interior node.
    /// assert_eq!(accumulator.push_recording(Hash::from_bytes([1; 32])).1.len(), 2);
    /// assert_eq!(nodes::count(accumulator.size()), 3);
    /// ```
    pub fn push_recording(&mut self, payload: Hash) -> (u64, SmallVec<[Hash; 8]>) {
        let index = self.size;
        let mut node = leaf_hash(&payload);
        let mut written = SmallVec::new();
        written.push(node);
        let mut level = 0u8;
        // The left sibling is always the one already on the stack.
        while let Some(&(top_level, top_hash)) = self.subtrees.last() {
            if top_level != level {
                break;
            }
            self.subtrees.pop();
            node = node_hash(&top_hash, &node);
            level = level.saturating_add(1);
            written.push(node);
        }
        self.subtrees.push((level, node));
        self.size = self.size.saturating_add(1);
        (index, written)
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
/// The log holds the tree, not the leaves: every node whose subtree is complete,
/// in [`nodes`] storage order. That is just under two hashes per entry, and it
/// is what makes every operation here logarithmic —
///
/// | Operation | Cost |
/// |---|---|
/// | [`append`](Self::append) | amortised `O(1)` |
/// | [`root`](Self::root), [`head`](Self::head) | `O(log n)` |
/// | [`root_at`](Self::root_at), [`head_at`](Self::head_at) | `O(log n)` |
/// | [`inclusion_proof`](Self::inclusion_proof) and friends | `O(log n)` |
/// | [`consistency_proof`](Self::consistency_proof) and friends | `O(log n)` |
/// | [`verify_structure`](Self::verify_structure) | `O(n)`, audit-time |
///
/// The same [`nodes`] plans drive the durable backends, so a proof from a
/// database and a proof from memory are the same proof — which the equivalence
/// tests in this module pin against an independent replay of the RFC 6962
/// definition.
#[derive(Debug, Clone, Default)]
pub struct MerkleLog {
    /// Every completed node, dense by [`nodes`] storage position.
    nodes: Vec<Hash>,
    /// The perfect-subtree cover, so an append needs no lookup. Derived — it is
    /// exactly the nodes a [`nodes::RootPlan`] names — and
    /// [`MerkleLog::verify_structure`] proves it has not drifted.
    accumulator: MerkleAccumulator,
}

impl MerkleLog {
    /// Creates an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        self.accumulator.size()
    }

    /// True when the log holds no leaves.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The stored nodes, dense by [`nodes`] storage position.
    ///
    /// What a durable backend writes. Position `i` is
    /// [`nodes::split_index`]`(i)`, and the whole slice is what
    /// [`nodes::count`] counts.
    #[must_use]
    pub fn nodes(&self) -> &[Hash] {
        &self.nodes
    }

    /// Appends a leaf payload and returns its index.
    ///
    /// Amortised `O(1)`: the new leaf merges with any equal-sized subtree
    /// already on the stack, which happens once per power of two, and each merge
    /// appends exactly one node.
    pub fn append(&mut self, payload: Hash) -> u64 {
        let (index, written) = self.accumulator.push_recording(payload);
        self.nodes.extend(written);
        index
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
    /// anything, because the batch never returned.
    ///
    /// `O(log n)`: the nodes a size requires are a prefix of the ones it had, so
    /// this is a truncation and a re-read of the cover.
    pub(crate) fn truncate(&mut self, len: u64) {
        if len >= self.len() {
            return;
        }
        let keep = usize::try_from(nodes::count(len)).unwrap_or(usize::MAX);
        self.nodes.truncate(keep);
        match self.cover(len) {
            Some(cover) => self.accumulator = cover,
            // Unreachable: `keep` is exactly the node count `len` requires, and
            // every position in the cover at `len` is below it. Were it ever
            // reached, a log that cannot describe its own size must not go on
            // answering proofs about it — so it empties rather than serving from
            // a stale cover, which is the failure that would not announce
            // itself.
            None => {
                self.nodes.clear();
                self.accumulator = MerkleAccumulator::new();
            }
        }
    }

    /// The subtree cover at `size`, read from the stored nodes.
    ///
    /// `None` when the log does not hold every node the cover names, which for a
    /// `size` the log has reached means the node vector is shorter than its own
    /// size claims.
    ///
    /// The shape needs no checking here, unlike
    /// [`MerkleAccumulator::try_from_parts`]: the heights come from the plan
    /// rather than from storage, so they are the decomposition `size` requires
    /// by construction. What `try_from_parts` guards against is a *backend*
    /// handing over rows, and that check stays where the rows do.
    fn cover(&self, size: u64) -> Option<MerkleAccumulator> {
        let plan = nodes::RootPlan::new(size);
        let subtrees = plan
            .nodes()
            .iter()
            .map(|at| {
                let hash = *self.nodes.get(usize::try_from(*at).ok()?)?;
                Some((nodes::split_index(*at).0, hash))
            })
            .collect::<Option<Vec<_>>>()?;
        MerkleAccumulator::try_from_parts(subtrees, size).ok()
    }

    /// The nodes a plan named, read out of this log.
    ///
    /// Returns `None` for a position the log does not hold, which cannot happen
    /// for a plan built against a size this log has reached.
    fn read(&self, want: &[u64]) -> Option<Vec<Hash>> {
        want.iter()
            .map(|at| self.nodes.get(usize::try_from(*at).ok()?).copied())
            .collect()
    }

    /// The current Merkle Tree Hash.
    #[must_use]
    pub fn root(&self) -> Hash {
        self.accumulator.root()
    }

    /// Checks that every stored node is the hash of the two beneath it.
    ///
    /// The nodes are the log, so this is the log checking itself: for every
    /// interior node, recompute it from its children and compare. It also
    /// confirms the maintained subtree cover still agrees with the nodes it
    /// summarises.
    ///
    /// A single altered node fails here, wherever it sits. What it cannot say is
    /// whether the *leaves* are the right leaves — that is
    /// [`Journal::verify_log`](crate::Journal::verify_log)'s question, and it
    /// answers it by rebuilding from the entries.
    ///
    /// `O(n)`. An audit-time operation.
    #[must_use]
    pub fn verify_structure(&self) -> bool {
        if self.nodes.len() as u64 != nodes::count(self.len()) {
            return false;
        }
        for (at, node) in self.nodes.iter().enumerate() {
            let at = at as u64;
            let (level, index) = nodes::split_index(at);
            if level == 0 {
                continue;
            }
            let (Some(left), Some(right)) = (
                self.nodes.get(
                    usize::try_from(nodes::index_of(level - 1, index * 2)).unwrap_or(usize::MAX),
                ),
                self.nodes.get(
                    usize::try_from(nodes::index_of(level - 1, index * 2 + 1))
                        .unwrap_or(usize::MAX),
                ),
            ) else {
                return false;
            };
            if node_hash(left, right) != *node {
                return false;
            }
        }
        self.cover(self.len())
            .is_some_and(|cover| cover == self.accumulator)
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
    /// `O(log n)`: the perfect-subtree cover at that size, folded.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log.
    pub fn root_at(&self, size: u64) -> Result<Hash, ProofError> {
        self.head_at(size).map(|head| head.root)
    }

    /// The tree head as of an earlier size.
    ///
    /// The pair, not just the root: a size and the root it belongs with are what
    /// [`ConsistencyProof::verify`] needs at each end, and keeping them
    /// together is what stops a caller from pairing a root with the wrong size.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log.
    pub fn head_at(&self, size: u64) -> Result<TreeHead, ProofError> {
        if size > self.len() {
            return Err(ProofError::SizeOutOfRange {
                from: size,
                size: self.len(),
            });
        }
        let plan = nodes::RootPlan::new(size);
        let read = self.read(plan.nodes()).ok_or(ProofError::SizeOutOfRange {
            from: size,
            size: self.len(),
        })?;
        let root = plan
            .assemble(&read)
            .map_err(|_| ProofError::SizeOutOfRange {
                from: size,
                size: self.len(),
            })?;
        Ok(TreeHead { size, root })
    }

    /// Builds an inclusion proof for the leaf at `index`, against the current head.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::IndexOutOfRange`] for a position beyond the log.
    pub fn inclusion_proof(&self, index: u64) -> Result<InclusionProof, ProofError> {
        self.inclusion_proof_at(index, self.len())
    }

    /// Builds an inclusion proof against the head the log had at `size`.
    ///
    /// An auditor archives a head and comes back later, by which time the log
    /// has grown and its current root proves nothing about the head they hold.
    /// This answers the question they can actually ask: *was this entry in the
    /// log as of the head I already have?* Pair it with
    /// [`head_at`](Self::head_at), or with the head they archived — the two must
    /// agree, and [`InclusionProof::verify`] is what says so.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log, and
    /// [`ProofError::IndexOutOfRange`] for a position beyond *that size* — an
    /// entry the log has now but had not yet recorded then.
    pub fn inclusion_proof_at(&self, index: u64, size: u64) -> Result<InclusionProof, ProofError> {
        if size > self.len() {
            return Err(ProofError::SizeOutOfRange {
                from: size,
                size: self.len(),
            });
        }
        let plan = nodes::InclusionPlan::new(index, size)?;
        let read = self
            .read(plan.nodes())
            .ok_or(ProofError::IndexOutOfRange { index, size })?;
        plan.assemble(&read)
            .map_err(|_| ProofError::IndexOutOfRange { index, size })
    }

    /// Builds a consistency proof from an earlier size to the current size.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log, and
    /// [`ProofError::EmptyOldTree`] for `old_size == 0` — a proof from the empty
    /// tree is true of every history and is refused rather than handed out.
    pub fn consistency_proof(&self, old_size: u64) -> Result<ConsistencyProof, ProofError> {
        self.consistency_proof_between(old_size, self.len())
    }

    /// Builds a consistency proof between two sizes the log has passed through.
    ///
    /// The general form: two auditors holding different archived heads, neither
    /// of them current, can be shown that one is a prefix of the other without
    /// either being told the log's present size.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] if `new_size` is beyond the log or
    /// `old_size` is beyond `new_size` — a log cannot be shown to have shrunk —
    /// and [`ProofError::EmptyOldTree`] for `old_size == 0`.
    pub fn consistency_proof_between(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, ProofError> {
        if new_size > self.len() {
            return Err(ProofError::SizeOutOfRange {
                from: new_size,
                size: self.len(),
            });
        }
        let plan = nodes::ConsistencyPlan::new(old_size, new_size)?;
        let read = self.read(plan.nodes()).ok_or(ProofError::SizeOutOfRange {
            from: new_size,
            size: self.len(),
        })?;
        plan.assemble(&read)
            .map_err(|_| ProofError::SizeOutOfRange {
                from: old_size,
                size: new_size,
            })
    }
}

/// `PATH(m, D[n])` from RFC 6962.
///
/// The reference implementation, kept test-only for the reason given on [`mth`].
#[cfg(test)]
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
///
/// The reference implementation, kept test-only for the reason given on [`mth`].
#[cfg(test)]
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
mod node_equivalence {
    //! The stored-node algorithms and the leaf-replay ones must agree exactly.
    //!
    //! [`nodes`] exists so a durable backend can answer a proof from `O(log n)`
    //! rows instead of replaying the whole log. That is only safe if it produces
    //! *the same proof* — a second implementation that drifts is worse than the
    //! `O(n)` one it replaced, because the proofs it hands out would verify
    //! against a root nobody else computes.
    //!
    //! So this compares them exhaustively over every size and position a test
    //! can enumerate, rather than sampling. The golden vectors pin the shared
    //! answer; this pins that there is only one answer.

    use super::*;
    use crate::merkle::nodes::{ConsistencyPlan, InclusionPlan, RootPlan};

    fn payload(i: u64) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&i.to_le_bytes());
        Hash::from_bytes(bytes)
    }

    /// Every node a log of `size` records stores, in storage order.
    ///
    /// Built the way a backend builds it — one append at a time, keeping only
    /// what the accumulator hands back — so this is the write path under test as
    /// well as a fixture.
    fn stored_nodes(size: u64) -> Vec<Hash> {
        let mut accumulator = MerkleAccumulator::new();
        let mut out = Vec::new();
        for i in 0..size {
            out.extend(accumulator.push_recording(payload(i)).1);
        }
        out
    }

    fn read(all: &[Hash], plan: &[u64]) -> Vec<Hash> {
        plan.iter()
            .map(|i| all[usize::try_from(*i).expect("small")])
            .collect()
    }

    #[test]
    fn the_stored_node_count_is_what_the_appends_produced() {
        for size in 0..300u64 {
            assert_eq!(
                stored_nodes(size).len() as u64,
                crate::merkle::nodes::count(size),
                "node count disagreed at size {size}"
            );
        }
    }

    #[test]
    fn roots_agree_at_every_size() {
        let all = stored_nodes(300);
        for size in 0..=300u64 {
            let plan = RootPlan::new(size);
            let from_nodes = plan
                .assemble(&read(&all, plan.nodes()))
                .expect("well formed");
            let from_leaves = mth(&(0..size).map(payload).collect::<Vec<_>>());
            assert_eq!(from_nodes, from_leaves, "root disagreed at size {size}");
        }
    }

    #[test]
    fn inclusion_proofs_agree_at_every_position() {
        let all = stored_nodes(130);
        for size in 1..=130u64 {
            let leaves: Vec<Hash> = (0..size).map(payload).collect();
            for index in 0..size {
                let plan = InclusionPlan::new(index, size).expect("in range");
                let from_nodes = plan
                    .assemble(&read(&all, plan.nodes()))
                    .expect("well formed");

                // RFC 6962's PATH, transcribed, against the same leaves.
                let mut reference = Vec::new();
                build_inclusion_path(
                    usize::try_from(index).expect("small"),
                    &leaves,
                    &mut reference,
                );
                assert_eq!(
                    from_nodes.path, reference,
                    "inclusion path disagreed for leaf {index} of {size}"
                );
            }
        }
    }

    #[test]
    fn consistency_proofs_agree_at_every_pair() {
        let all = stored_nodes(130);
        for new in 1..=130u64 {
            let leaves: Vec<Hash> = (0..new).map(payload).collect();
            for old in 1..=new {
                let plan = ConsistencyPlan::new(old, new).expect("in range");
                let from_nodes = plan
                    .assemble(&read(&all, plan.nodes()))
                    .expect("well formed");

                // RFC 6962's SUBPROOF, transcribed, against the same leaves.
                let mut reference = Vec::new();
                if old < new {
                    build_consistency_path(
                        usize::try_from(old).expect("small"),
                        &leaves,
                        true,
                        &mut reference,
                    );
                }
                assert_eq!(
                    from_nodes.path, reference,
                    "consistency path disagreed from {old} to {new}"
                );
            }
        }
    }

    /// The whole log's proofs, checked against the definition, at every size.
    ///
    /// `MerkleLog` answers from stored nodes, so this ties the two together end
    /// to end rather than only at the plan layer.
    #[test]
    fn the_log_agrees_with_the_definition_at_every_size() {
        let mut log = MerkleLog::new();
        let mut leaves = Vec::new();
        for i in 0..100u64 {
            log.append(payload(i));
            leaves.push(payload(i));
            assert_eq!(log.root(), mth(&leaves), "root diverged at size {}", i + 1);
        }
        for size in 0..=100u64 {
            assert_eq!(
                log.root_at(size).expect("in range"),
                mth(&leaves[..usize::try_from(size).expect("small")]),
                "historical root diverged at size {size}"
            );
        }
    }

    #[test]
    fn a_stored_node_proof_verifies_against_a_replayed_head() {
        // The two halves meeting: a proof assembled from stored nodes, checked
        // against a head computed from leaves.
        let all = stored_nodes(100);
        let log = MerkleLog::from_leaves((0..100u64).map(payload).collect());
        let head = log.head();
        for index in [0u64, 1, 37, 63, 64, 99] {
            let plan = InclusionPlan::new(index, 100).expect("in range");
            let proof = plan
                .assemble(&read(&all, plan.nodes()))
                .expect("well formed");
            assert!(proof.verify(&payload(index), &head), "leaf {index}");
        }
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
        let mut leaves: Vec<Hash> = (0..8u64).map(payload).collect();
        leaves[3] = payload(999);
        let b = MerkleLog::from_leaves(leaves);
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn inclusion_proofs_verify_for_every_leaf_and_size() {
        for n in 1..=33u64 {
            let log = log_of(n);
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                assert!(
                    proof.verify(&payload(i), &log.head()),
                    "inclusion proof failed for leaf {i} of {n}"
                );
            }
        }
    }

    #[test]
    fn inclusion_proof_rejects_wrong_leaf() {
        let log = log_of(8);
        let proof = log.inclusion_proof(3).expect("in range");
        assert!(!proof.verify(&payload(4), &log.head()));
    }

    #[test]
    fn inclusion_proof_rejects_wrong_root() {
        let log = log_of(8);
        let proof = log.inclusion_proof(3).expect("in range");
        let wrong = TreeHead {
            size: log.len(),
            root: payload(0),
        };
        assert!(!proof.verify(&payload(3), &wrong));
    }

    #[test]
    fn inclusion_proof_rejects_tampered_path() {
        let log = log_of(8);
        let mut proof = log.inclusion_proof(3).expect("in range");
        proof.path[0] = payload(12345);
        assert!(!proof.verify(&payload(3), &log.head()));
    }

    /// Every way to deform a proof path without touching the hashes in it:
    /// siblings added, dropped, duplicated, or reordered.
    ///
    /// A verifier that walked the path it was handed and compared only the hash
    /// it arrived at would accept several of these. Padding runs the fold off
    /// the top of the tree, truncation stops it short, and either can be made to
    /// land on a real root if the walk is not tied to the tree's own shape.
    fn shape_mutations(path: &[Hash]) -> Vec<(String, Vec<Hash>)> {
        let mut out = Vec::new();
        for k in 0..=path.len() {
            let mut junk = path.to_vec();
            junk.insert(k, payload(777_777));
            out.push((format!("a junk sibling inserted at {k}"), junk));

            // A duplicated *genuine* node is the sharper case: the padding is
            // then a hash the tree really contains, not a value pulled from
            // nowhere.
            let mut duplicated = path.to_vec();
            if let Some(&real) = path.get(k.min(path.len().saturating_sub(1))) {
                duplicated.insert(k, real);
                out.push((format!("a real sibling duplicated at {k}"), duplicated));
            }
        }
        for k in 0..path.len() {
            out.push((
                format!("truncated to {k} of {}", path.len()),
                path[..k].to_vec(),
            ));

            let mut dropped = path.to_vec();
            dropped.remove(k);
            out.push((format!("sibling {k} dropped"), dropped));
        }
        for k in 1..path.len() {
            let mut swapped = path.to_vec();
            swapped.swap(k - 1, k);
            out.push((format!("siblings {} and {k} swapped", k - 1), swapped));
        }
        // A mutation that happened to reproduce the original proves nothing.
        out.retain(|(_, mutated)| mutated != path);
        out
    }

    #[test]
    fn inclusion_proof_rejects_a_deformed_path() {
        for n in 1..=24u64 {
            let log = log_of(n);
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                for (how, path) in shape_mutations(&proof.path) {
                    let deformed = InclusionProof {
                        path,
                        ..proof.clone()
                    };
                    assert!(
                        !deformed.verify(&payload(i), &log.head()),
                        "leaf {i} of {n} verified with {how}"
                    );
                }
            }
        }
    }

    #[test]
    fn inclusion_proof_rejects_a_relabelled_index() {
        // The index is not carried beside the path, it *drives* it: which side
        // each sibling is folded on comes from the index bits. Replaying a
        // genuine proof at another position therefore cannot be made to work.
        for n in 2..=24u64 {
            let log = log_of(n);
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                for j in (0..n).filter(|j| *j != i) {
                    let moved = InclusionProof {
                        leaf_index: j,
                        ..proof.clone()
                    };
                    assert!(
                        !moved.verify(&payload(i), &log.head()),
                        "leaf {i} of {n} verified when relabelled to index {j}"
                    );
                }
            }
        }
    }

    #[test]
    fn inclusion_proof_rejects_an_index_at_or_beyond_its_tree() {
        // Load-bearing, not a formality. Without the range guard a genuine proof
        // for leaf 0 of eight verifies while claiming index 8 — the fold reaches
        // the same root because the extra index bits shift off the top.
        for n in 1..=16u64 {
            let log = log_of(n);
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                for index in n..n.saturating_add(9) {
                    let past_the_end = InclusionProof {
                        leaf_index: index,
                        ..proof.clone()
                    };
                    assert!(
                        !past_the_end.verify(&payload(i), &log.head()),
                        "leaf {i} of {n} verified at index {index}, past the end of its own tree"
                    );
                }
            }
        }
    }

    #[test]
    fn a_grossly_padded_path_is_refused_rather_than_walked() {
        // The fold stops consuming siblings the moment the tree is exhausted, so
        // an oversized path costs a verifier `O(log n)` work and not `O(path)`.
        let log = log_of(8);
        let proof = log.inclusion_proof(3).expect("in range");
        let bloated = InclusionProof {
            path: proof
                .path
                .iter()
                .copied()
                .chain((0..100_000).map(payload))
                .collect(),
            ..proof
        };
        assert!(!bloated.verify(&payload(3), &log.head()));
    }

    #[test]
    fn an_inclusion_proof_cannot_be_relabelled_outside_its_tree_under_a_head() {
        // Rewriting the index alone fails, and rewriting it past `tree_size` is
        // caught by the range guard. Rewriting *both* together escapes both: for
        // a two-leaf log the proof for leaf 1 folds identically when relabelled
        // as leaf 2 of three, so the fold alone reaches the real root while
        // naming a position that log does not have.
        //
        // What rejects it is the head's size — and nothing else, which is why
        // there is no root-only verification to reach for.
        let log = log_of(2);
        let head = log.head();
        let proof = log.inclusion_proof(1).expect("in range");
        let moved = InclusionProof {
            leaf_index: 2,
            tree_size: 3,
            ..proof.clone()
        };
        assert!(!moved.verify(&payload(1), &head));

        // The fold really does accept it, which a head that agrees with the lie
        // exposes. No such head can be honestly published — a three-leaf tree
        // does not have a two-leaf tree's root — but it shows where the strength
        // of the check comes from.
        let fabricated = TreeHead {
            size: 3,
            root: head.root,
        };
        assert!(moved.verify(&payload(1), &fabricated));

        // Under a head exactly one labelling is accepted, for every leaf of
        // every shape of log.
        for n in 1..=12u64 {
            let log = log_of(n);
            let head = log.head();
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                for size in 1..=16u64 {
                    for index in 0..size {
                        let relabelled = InclusionProof {
                            leaf_index: index,
                            tree_size: size,
                            ..proof.clone()
                        };
                        assert_eq!(
                            relabelled.verify(&payload(i), &head),
                            index == i && size == n,
                            "leaf {i} of {n} under a claimed index {index} of size {size}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_tree_size_is_bound_by_the_head_and_by_nothing_else() {
        let log = log_of(3);
        let head = log.head();
        let proof = log.inclusion_proof(1).expect("in range");
        assert!(proof.verify(&payload(1), &head));

        // Rewriting the size does not always break the recomputation: for this
        // leaf, a claimed size of four folds the same siblings in the same
        // order and arrives at the same root. Only the head's size rejects it.
        let relabelled = InclusionProof {
            tree_size: 4,
            ..proof.clone()
        };
        assert!(!relabelled.verify(&payload(1), &head));
        assert!(relabelled.verify(
            &payload(1),
            &TreeHead {
                size: 4,
                root: head.root
            }
        ));

        // And no size but the true one passes, for any leaf of any log.
        for n in 1..=24u64 {
            let log = log_of(n);
            let head = log.head();
            for i in 0..n {
                let proof = log.inclusion_proof(i).expect("in range");
                assert!(proof.verify(&payload(i), &head));
                for size in (1..=25u64).filter(|s| *s != n) {
                    let relabelled = InclusionProof {
                        tree_size: size,
                        ..proof.clone()
                    };
                    assert!(
                        !relabelled.verify(&payload(i), &head),
                        "leaf {i} of {n} verified under a claimed size of {size}"
                    );
                }
            }
        }
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
            let new_head = log.head();
            for m in 1..=n {
                let old_head = log.head_at(m).expect("in range");
                let proof = log.consistency_proof(m).expect("in range");
                assert!(
                    proof.verify(&old_head, &new_head),
                    "consistency proof failed from {m} to {n}"
                );
            }
        }
    }

    #[test]
    fn consistency_proof_rejects_a_rewritten_prefix() {
        let log = log_of(16);
        let new_head = log.head();
        let proof = log.consistency_proof(8).expect("in range");

        // A log that diverges within the first 8 leaves must not verify.
        let mut tampered: Vec<Hash> = (0..8u64).map(payload).collect();
        tampered[2] = payload(4242);
        let forged_old = TreeHead {
            size: 8,
            root: MerkleLog::from_leaves(tampered).root(),
        };

        assert!(!proof.verify(&forged_old, &new_head));
    }

    #[test]
    fn consistency_proof_rejects_a_deformed_path() {
        for n in 1..=24u64 {
            let log = log_of(n);
            let new_head = log.head();
            for m in 1..=n {
                let old_head = log.head_at(m).expect("in range");
                let proof = log.consistency_proof(m).expect("in range");
                for (how, path) in shape_mutations(&proof.path) {
                    let deformed = ConsistencyProof {
                        path,
                        ..proof.clone()
                    };
                    assert!(
                        !deformed.verify(&old_head, &new_head),
                        "consistency from {m} to {n} verified with {how}"
                    );
                }
            }
        }
    }

    #[test]
    fn verifying_binds_both_snapshot_sizes() {
        // A snapshot is a (size, root) pair, and a verifier that kept the pair
        // together is entitled to trust both halves afterwards.
        for n in 1..=20u64 {
            let log = log_of(n);
            let new_head = log.head();
            for m in 1..=n {
                let old_head = log.head_at(m).expect("in range");
                let proof = log.consistency_proof(m).expect("in range");
                assert!(proof.verify(&old_head, &new_head));

                for size in (0..=21u64).filter(|s| *s != m) {
                    let relabelled = ConsistencyProof {
                        old_size: size,
                        ..proof.clone()
                    };
                    assert!(
                        !relabelled.verify(&old_head, &new_head),
                        "consistency from {m} to {n} verified claiming an old size of {size}"
                    );
                }
                for size in (0..=21u64).filter(|s| *s != n) {
                    let relabelled = ConsistencyProof {
                        new_size: size,
                        ..proof.clone()
                    };
                    assert!(
                        !relabelled.verify(&old_head, &new_head),
                        "consistency from {m} to {n} verified claiming a new size of {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn consistency_verification_refuses_a_backwards_pair_of_heads() {
        // Handing the two heads over in the wrong order is a caller slip rather
        // than an attack, and it has to come back `false`. The guard that makes
        // it do so is load-bearing in a second way: the walk subtracts one from
        // each size, so a `new_size` of zero would underflow before any hashing
        // began. This module turns off the checked-arithmetic lint, which means
        // nothing but this test stands between that guard and a panic.
        let log = log_of(8);
        let later = log.head();
        let earlier = log.head_at(3).expect("in range");
        let proof = log.consistency_proof_between(3, 8).expect("in range");

        // The proof's own labels no longer match the swapped heads …
        assert!(!proof.verify(&later, &earlier));
        // … and relabelling it to agree with them does not help.
        let backwards = ConsistencyProof {
            old_size: 8,
            new_size: 3,
            ..proof
        };
        assert!(!backwards.verify(&later, &earlier));

        // The underflow case exactly: a later log claimed to hold nothing.
        let empty = log.head_at(0).expect("in range");
        let one = log.head_at(1).expect("in range");
        let shrunk = ConsistencyProof {
            old_size: 1,
            new_size: 0,
            ..log.consistency_proof_between(1, 8).expect("in range")
        };
        assert!(!shrunk.verify(&one, &empty));

        // And across every genuine pair of heads in the wrong order.
        for new_size in 0..=12u64 {
            for old_size in (new_size + 1)..=12u64 {
                let old_head = log.head_at(old_size.min(8)).expect("in range");
                let new_head = log.head_at(new_size.min(8)).expect("in range");
                let claim = ConsistencyProof {
                    old_size: old_head.size,
                    new_size: new_head.size,
                    path: Vec::new(),
                };
                if old_head.size > new_head.size {
                    assert!(
                        !claim.verify(&old_head, &new_head),
                        "a log was shown to shrink from {} to {}",
                        old_head.size,
                        new_head.size
                    );
                }
            }
        }
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
    fn a_proof_from_the_empty_tree_is_refused_at_both_ends() {
        let log = log_of(8);

        // Not built. "Every log extends the empty tree" is true of every log
        // there has ever been, so the proof would verify against a history
        // nobody has and its `true` would mean nothing.
        assert!(matches!(
            log.consistency_proof(0),
            Err(ProofError::EmptyOldTree { new_size: 8 })
        ));
        assert!(matches!(
            log.consistency_proof_between(0, 4),
            Err(ProofError::EmptyOldTree { new_size: 4 })
        ));

        // And not verified either, because the fields are public and a proof
        // arrives over a wire. Refusing only at construction would leave the
        // hole open to exactly the party it protects against.
        let empty = log.head_at(0).expect("in range");
        assert_eq!(empty.root, empty_root());
        for path in [vec![], vec![payload(1)]] {
            let forged = ConsistencyProof {
                old_size: 0,
                new_size: 8,
                path,
            };
            assert!(!forged.verify(&empty, &log.head()));
            // Not even against a head that is itself a forgery — which is the
            // whole point: nothing about the newer tree was ever constrained.
            let invented = TreeHead {
                size: 8,
                root: payload(999),
            };
            assert!(!forged.verify(&empty, &invented));
        }
    }

    #[test]
    fn incremental_root_matches_full_recomputation() {
        // The stored tree is derived state; it must agree with the RFC 6962
        // definition at every size, including the ragged ones between powers of
        // two — and every node in it must be the hash of the two beneath it.
        let mut log = MerkleLog::new();
        assert_eq!(log.root(), mth(&[]));
        let mut leaves = Vec::new();
        for i in 0..129u64 {
            log.append(payload(i));
            leaves.push(payload(i));
            assert_eq!(log.root(), mth(&leaves), "diverged at size {}", i + 1);
            assert!(log.verify_structure(), "malformed at size {}", i + 1);
        }
    }

    #[test]
    fn a_single_altered_node_is_caught_wherever_it_sits() {
        // What `verify_structure` buys over comparing roots: the position of the
        // damage is not a place the check has to be pointed at.
        let log = log_of(64);
        assert!(log.verify_structure());
        for at in [0usize, 1, 2, 60, 100, 120] {
            let mut damaged = log.clone();
            damaged.nodes[at] = payload(0xdead);
            assert!(
                !damaged.verify_structure(),
                "an altered node at position {at} went unnoticed"
            );
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
        assert_eq!(built.nodes(), appended.nodes());
        assert!(built.verify_structure());
    }

    #[test]
    fn consistency_with_equal_sizes_is_empty_and_reflexive() {
        let log = log_of(8);
        let head = log.head();
        let proof = log.consistency_proof(8).expect("in range");
        assert!(proof.path.is_empty());
        assert!(proof.verify(&head, &head));

        // Reflexive is not vacuous: at equal sizes there is no path to walk, so
        // the equality of the two roots is the *whole* check. A log that claims
        // it did not grow must actually be the same log.
        let bogus = TreeHead {
            size: 8,
            root: payload(4242),
        };
        assert!(!proof.verify(&bogus, &head));
        assert!(!proof.verify(&head, &bogus));
        let seven_leaves = TreeHead {
            size: 8,
            root: log_of(7).root(),
        };
        assert!(!proof.verify(&seven_leaves, &head));

        // And an empty path is required, not merely produced: a sibling smuggled
        // into a no-op proof is still a deformed proof.
        let padded = ConsistencyProof {
            path: vec![payload(1)],
            ..proof
        };
        assert!(!padded.verify(&head, &head));
    }

    #[test]
    fn a_consistency_proofs_old_size_is_bound_by_the_walk() {
        // Both roots are reconstructed from the one path, and the old size is
        // what decides where that path forks. No other value reproduces the old
        // root, so the field is bound without ever being compared.
        for n in 1..=20u64 {
            let log = log_of(n);
            let new_head = log.head();
            for m in 1..=n {
                let old_head = log.head_at(m).expect("in range");
                let proof = log.consistency_proof(m).expect("in range");
                for size in (0..=24u64).filter(|s| *s != m) {
                    let relabelled = ConsistencyProof {
                        old_size: size,
                        ..proof.clone()
                    };
                    assert!(
                        !relabelled.verify(&old_head, &new_head),
                        "consistency from {m} to {n} verified claiming an old size of {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_consistency_proofs_new_size_is_bound_only_by_the_head() {
        // The new size gets no help from the walk: like an inclusion proof's
        // tree size it steers the fold, and neighbouring values steer it the
        // same way. Stretching a one-to-three proof into a one-to-four still
        // reproduces both roots — so what rejects it is the size comparison
        // against the head, and nothing else.
        let log = log_of(3);
        let old_head = log.head_at(1).expect("in range");
        let new_head = log.head();
        let proof = log.consistency_proof(1).expect("in range");

        let stretched = ConsistencyProof {
            new_size: 4,
            ..proof.clone()
        };
        assert!(!stretched.verify(&old_head, &new_head));

        // Which is to say: hand the walk a head that agrees with the lie and it
        // is satisfied. A fabricated head is not a check on anything, and this
        // is the boundary of what a proof can establish on its own — the head
        // has to come from somewhere the verifier already trusts.
        let fabricated = TreeHead {
            size: 4,
            root: new_head.root,
        };
        assert!(stretched.verify(&old_head, &fabricated));

        // Under both heads, only the true labelling survives — the from-empty
        // proof included, which the walk alone leaves entirely unconstrained.
        for n in 1..=20u64 {
            let log = log_of(n);
            let new_head = log.head();
            for m in 1..=n {
                let old_head = log.head_at(m).expect("in range");
                let proof = log.consistency_proof(m).expect("in range");
                assert!(proof.verify(&old_head, &new_head));
                for size in (0..=24u64).filter(|s| *s != n) {
                    let relabelled = ConsistencyProof {
                        new_size: size,
                        ..proof.clone()
                    };
                    assert!(
                        !relabelled.verify(&old_head, &new_head),
                        "consistency from {m} to {n} verified claiming a new size of {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn inclusion_proofs_verify_against_the_head_they_were_built_for() {
        // An auditor archives a head and comes back later. The log has grown,
        // and its current root says nothing about the head they hold — so the
        // proof has to be built against that head, and must verify under it.
        let log = log_of(40);
        for size in 1..=40u64 {
            let head = log.head_at(size).expect("in range");
            for i in 0..size {
                let proof = log.inclusion_proof_at(i, size).expect("in range");
                assert!(
                    proof.verify(&payload(i), &head),
                    "leaf {i} did not prove against the head at size {size}"
                );
                // The same proof is worthless against any other head, which is
                // what stops one being replayed as the log moves on.
                for other in (1..=40u64).filter(|s| *s != size) {
                    let elsewhere = log.head_at(other).expect("in range");
                    assert!(!proof.verify(&payload(i), &elsewhere));
                }
            }
        }
    }

    #[test]
    fn a_historical_inclusion_proof_matches_one_from_a_shorter_log() {
        // The proof must not depend on entries recorded after the head was
        // published, or an auditor's archived head would be unusable the moment
        // anyone appended.
        let long = log_of(50);
        for size in 1..=25u64 {
            let short = log_of(size);
            for i in 0..size {
                assert_eq!(
                    long.inclusion_proof_at(i, size).expect("in range"),
                    short.inclusion_proof(i).expect("in range"),
                    "historical proof for leaf {i} at size {size} diverged"
                );
            }
        }
    }

    #[test]
    fn a_historical_inclusion_proof_refuses_an_entry_the_log_had_not_reached() {
        let log = log_of(20);
        // Leaf 15 exists now, but not in the log of ten the head describes.
        assert!(matches!(
            log.inclusion_proof_at(15, 10),
            Err(ProofError::IndexOutOfRange {
                index: 15,
                size: 10
            })
        ));
        assert!(matches!(
            log.inclusion_proof_at(0, 21),
            Err(ProofError::SizeOutOfRange { from: 21, size: 20 })
        ));
    }

    #[test]
    fn consistency_proofs_relate_any_two_sizes_the_log_passed_through() {
        // Two auditors, two archived heads, neither of them current: one must
        // still be provably a prefix of the other.
        let log = log_of(33);
        for new_size in 1..=33u64 {
            let new_head = log.head_at(new_size).expect("in range");
            // From 1: a proof from the empty tree is refused, not built.
            for old_size in 1..=new_size {
                let old_head = log.head_at(old_size).expect("in range");
                let proof = log
                    .consistency_proof_between(old_size, new_size)
                    .expect("in range");
                assert!(
                    proof.verify(&old_head, &new_head),
                    "consistency failed from {old_size} to {new_size}"
                );
                // And it agrees with what a log that had stopped there produced.
                assert_eq!(
                    proof,
                    log_of(new_size).consistency_proof(old_size).expect("ok"),
                    "historical proof diverged from {old_size} to {new_size}"
                );
            }
        }
    }

    #[test]
    fn a_log_cannot_be_shown_to_have_shrunk() {
        let log = log_of(20);
        assert!(matches!(
            log.consistency_proof_between(10, 5),
            Err(ProofError::SizeOutOfRange { from: 10, size: 5 })
        ));
        assert!(matches!(
            log.consistency_proof_between(0, 21),
            Err(ProofError::SizeOutOfRange { from: 21, size: 20 })
        ));
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
