//! Regressions for proofs of concept not covered elsewhere. A proof of
//! concept asserted a defect, or — for an attack that found nothing — the
//! sound behaviour; each test here asserts the corrected (or kept) behaviour
//! and names the proof of concept it answers (`// PoC: <fn>`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use doubleentry::store::{ConsistencyPlan, InclusionPlan, MAX_TREE_SIZE, RootPlan, Tree};
use doubleentry::{
    AccountHandle, AccountKind, Amount, Balance, Dimensions, Entry, EntryId, EntryRecord,
    IdempotencyKey, Journal, Label, Layer, LedgerId, PostAmount, Posting, Rate, Rounding, Side,
    TreeHead, amount,
    currency::{EUR, USD},
    date,
    error::AmountError,
    log::{
        BalanceTree, ConsistencyProof, Hash, InclusionProof, ProofError, balance_leaf_bytes,
        build_seal_trees, empty_root, leaf_hash, node_hash, root_of, zero_leaf_hash,
    },
    record::{CurrencyDeclaration, EntryParts, Genesis, PostingRecord, Record},
};

// ── Money at its edges (E-F8, audit round 3 model checks) ────────────────────

/// E-F8 and the round-3 money checks: `MIN` and `MAX` round-trip through
/// `Display`/`parse` at every scale (the range is symmetric, `i128::MIN` is
/// unrepresentable), splits of `MIN` are exact, the seven rounding modes
/// agree at −x.5, and overflow is an error, never a wrap.
#[test]
// PoC: f8_amount_min_edge_cases a3_model_amount_edges
fn amounts_round_trip_and_split_exactly_at_their_edges() {
    fn edges<const P: u8>() {
        for value in [Amount::<P>::MAX, Amount::<P>::MIN, Amount::ZERO] {
            assert_eq!(value.to_string().parse::<Amount<P>>(), Ok(value));
            assert_eq!(Amount::<P>::parse(&value.to_string()), Ok(value));
        }
        assert_eq!(Amount::<P>::MIN.allocate(&[1]), Ok(vec![Amount::<P>::MIN]));
        assert_eq!(Amount::<P>::MIN.negate(), Amount::MAX);
    }
    edges::<0>();
    edges::<2>();
    edges::<18>();
    assert_eq!(
        "-170141183460469231731687303715884105728".parse::<Amount<0>>(),
        Err(AmountError::Overflow)
    );
    assert_eq!(
        Amount::<2>::from_minor(i128::MIN),
        Err(AmountError::Overflow)
    );
    for bad in [
        "\u{0661}", "١.00", "1e2", "+1", " 1", "1.", ".5", "--1", "-",
    ] {
        assert!(bad.parse::<Amount<2>>().is_err(), "{bad:?}");
    }
    assert_eq!("007.50".parse::<Amount<2>>().map(Amount::to_minor), Ok(750));
    let one = Amount::<2>::from_minor(1).unwrap();
    assert_eq!(
        Amount::<2>::MAX.checked_add(one),
        Err(AmountError::Overflow)
    );
    assert_eq!(
        Amount::<2>::MIN.checked_sub(one),
        Err(AmountError::Overflow)
    );
    assert_eq!(Amount::<2>::MIN.checked_mul_int(-1), Ok(Amount::MAX));

    let half: Amount<1> = "-2.5".parse().unwrap();
    let round = |mode| half.rescale::<0>(mode).unwrap().to_minor();
    assert_eq!(round(Rounding::HalfUp), -3);
    assert_eq!(round(Rounding::HalfDown), -2);
    assert_eq!(round(Rounding::HalfEven), -2);
    assert_eq!(round(Rounding::Floor), -3);
    assert_eq!(round(Rounding::Ceiling), -2);
    assert_eq!(round(Rounding::TowardZero), -2);
    assert_eq!(round(Rounding::AwayFromZero), -3);
    let odd: Amount<1> = "-3.5".parse().unwrap();
    assert_eq!(
        odd.rescale::<0>(Rounding::HalfEven).map(Amount::to_minor),
        Ok(-4)
    );

    let parts = Amount::<2>::MAX.allocate(&[u64::MAX - 1, 0, 1]).unwrap();
    assert_eq!(
        Amount::checked_sum(parts.iter().copied()),
        Ok(Amount::<2>::MAX)
    );
    assert!(parts[1].is_zero());
    for parts in [
        Amount::<2>::MIN.allocate(&[1, 1, 1]).unwrap(),
        Amount::<2>::MIN.distribute(65_536).unwrap(),
    ] {
        assert_eq!(
            Amount::checked_sum(parts.iter().copied()),
            Ok(Amount::<2>::MIN)
        );
    }
    let two_cents: Amount = amount!("0.02");
    assert_eq!(
        two_cents.allocate(&[1, 1, 1]).unwrap(),
        [amount!("0.01"), amount!("0.01"), Amount::<2>::ZERO]
    );
}

/// E-F8b and E-F6b: serde round-trips are the identity at the money range's
/// edges and for paths; a path that does not parse is refused on the way in.
#[cfg(feature = "serde")]
#[test]
// PoC: f8b_amount_min_serde_roundtrip f6b_account_path_serde_changes_value
fn serde_round_trips_are_identities_at_the_edges() {
    fn amounts<const P: u8>() {
        for value in [Amount::<P>::MIN, Amount::<P>::MAX] {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<Amount<P>>(&json).unwrap(), value);
        }
    }
    amounts::<0>();
    amounts::<2>();
    amounts::<18>();
    let path = doubleentry::AccountPath::parse("Assets:Cash:Petty").unwrap();
    let json = serde_json::to_string(&path).unwrap();
    assert_eq!(json, "\"Assets:Cash:Petty\"");
    assert_eq!(
        serde_json::from_str::<doubleentry::AccountPath>(&json).unwrap(),
        path
    );
    for refused in [
        "\"Assets::Cash\"",
        "\":Cash\"",
        "\"Cash:\"",
        "\"\"",
        "[\"Assets:Cash\"]",
    ] {
        assert!(
            serde_json::from_str::<doubleentry::AccountPath>(refused).is_err(),
            "{refused}"
        );
    }
}

// ── Proof plans read logarithmically (FC1, audit round 3 log checks) ────────

/// `(⌊log₂ n⌋, popcount(n))` of a tree size `n ≥ 1`.
fn shape(size: u64) -> (usize, usize) {
    (
        (63 - size.leading_zeros()) as usize,
        size.count_ones() as usize,
    )
}

/// FC1: the stored nodes a proof plan fetches (the rows a SQL store reads).
/// The 0.7 documentation's `1 + ⌈log₂ n⌉` is false (3 → 7 reads 5 nodes).
/// The bound that holds, and is reached, for every tree of up to 2¹⁰ leaves
/// and every leaf and old size: inclusion `⌊log₂ n⌋ + popcount(n) − 1`,
/// consistency `⌊log₂ n⌋ + popcount(n)` (reached from n = 4 on), the root
/// `popcount(n)`. The 2¹⁶-leaf tree's paths (the round-3 cost probe,
/// counted rather than timed) are `log₂ n` long.
#[test]
// PoC: fc1_proof_read_bound a3_log_tree_head_and_proofs_are_logarithmic
fn proof_plans_read_at_most_floor_log2_n_plus_popcount_n_nodes() {
    assert_eq!(ConsistencyPlan::new(3, 7).unwrap().ids().len(), 5);
    for size in 1..=1024u64 {
        let (log, ones) = shape(size);
        assert_eq!(
            RootPlan::new(size).unwrap().ids().len(),
            ones,
            "root {size}"
        );
        let inclusion = (0..size)
            .map(|index| InclusionPlan::new(index, size).unwrap().ids().len())
            .max();
        assert_eq!(inclusion, Some(log + ones - 1), "inclusion in {size}");
        let consistency = (1..=size)
            .map(|old| ConsistencyPlan::new(old, size).unwrap().ids().len())
            .max()
            .unwrap();
        assert!(consistency <= log + ones, "consistency in {size}");
        assert!(
            size < 4 || consistency == log + ones,
            "consistency in {size}"
        );
    }
    let size = 1u64 << 16;
    let mut tree = Tree::new();
    for i in 0..size {
        tree.push(leaf_hash(&i.to_le_bytes())).unwrap();
    }
    assert_eq!(tree.inclusion_proof(12_345, size).unwrap().path.len(), 16);
    assert!(tree.consistency_proof(777, size).unwrap().path.len() <= 17);
    assert_eq!(InclusionPlan::new(12_345, size).unwrap().ids().len(), 16);
    assert!(ConsistencyPlan::new(777, size).unwrap().ids().len() <= 17);
}

/// FC1 at scale: sampled leaves and old sizes of a 10⁸-leaf tree, the
/// boundaries around 2²⁶ included, stay within the same bound — 37 nodes
/// for inclusion and 38 for consistency (`⌊log₂ 10⁸⌋ = 26`,
/// `popcount(10⁸) = 12`), both reached — not the 28 the 0.7 documentation
/// stated.
#[test]
// PoC: fc1b_reads_at_100m
fn proof_plans_read_at_most_38_nodes_at_a_hundred_million() {
    let size = 100_000_000u64;
    let (log, ones) = shape(size);
    assert_eq!((log, ones), (26, 12));
    let specials = [0, 1, 2, 3, 67_108_863, 67_108_864, size - 2, size - 1];
    let inclusion = (0..size)
        .step_by(9_973)
        .chain(specials)
        .map(|index| InclusionPlan::new(index, size).unwrap().ids().len())
        .max();
    assert_eq!(inclusion, Some(37));
    let consistency = (1..size)
        .step_by(9_973)
        .chain(specials.map(|s| s.max(1)))
        .map(|old| ConsistencyPlan::new(old, size).unwrap().ids().len())
        .max();
    assert_eq!(consistency, Some(38));
}

// ── The log against an independent RFC 6962 reference (audit round 3) ──────

/// RFC 6962 written from the RFC on raw SHA-256, not the crate's hashing.
mod reference {
    use super::Hash;
    use sha2::{Digest, Sha256};

    pub fn sha(parts: &[&[u8]]) -> Hash {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part);
        }
        Hash::from_bytes(hasher.finalize().into())
    }

    pub fn leaf(data: &[u8]) -> Hash {
        sha(&[&[0], data])
    }

    fn node(left: &Hash, right: &Hash) -> Hash {
        sha(&[&[1], left.as_bytes(), right.as_bytes()])
    }

    fn split(n: usize) -> usize {
        let mut k = 1;
        while k * 2 < n {
            k *= 2;
        }
        k
    }

    pub fn mth(d: &[Hash]) -> Hash {
        match d.len() {
            0 => sha(&[]),
            1 => d[0],
            n => {
                let k = split(n);
                node(&mth(&d[..k]), &mth(&d[k..]))
            }
        }
    }

    pub fn path(m: usize, d: &[Hash]) -> Vec<Hash> {
        let n = d.len();
        if n <= 1 {
            return vec![];
        }
        let k = split(n);
        let (mut p, other) = if m < k {
            (path(m, &d[..k]), mth(&d[k..]))
        } else {
            (path(m - k, &d[k..]), mth(&d[..k]))
        };
        p.push(other);
        p
    }

    pub fn subproof(m: usize, d: &[Hash], complete: bool) -> Vec<Hash> {
        let n = d.len();
        if m == n {
            return if complete { vec![] } else { vec![mth(d)] };
        }
        let k = split(n);
        let (mut p, other) = if m <= k {
            (subproof(m, &d[..k], complete), mth(&d[k..]))
        } else {
            (subproof(m - k, &d[k..], false), mth(&d[..k]))
        };
        p.push(other);
        p
    }

    pub fn leaves(n: usize) -> Vec<Hash> {
        (0..n).map(|i| leaf(&(i as u64).to_le_bytes())).collect()
    }
}

fn tree_of(leaves: &[Hash]) -> Tree {
    let mut tree = Tree::new();
    for leaf in leaves {
        tree.push(*leaf).unwrap();
    }
    tree
}

/// Roots, inclusion and consistency proofs equal the independent reference
/// for every size up to 70, the reference's proofs verify, and a proof never
/// verifies at another index or from another old size.
#[test]
// PoC: a3_log_reference_crosscheck_1_to_70
fn the_log_equals_an_independent_rfc6962_reference_up_to_70() {
    let all = reference::leaves(70);
    let tree = tree_of(&all);
    assert_eq!(empty_root(), reference::sha(&[]));
    assert_eq!(leaf_hash(b"x"), reference::leaf(b"x"));
    for n in 1..=70usize {
        let d = &all[..n];
        let head = tree.head_at(n as u64).unwrap();
        assert_eq!(head.root, reference::mth(d), "root {n}");
        assert_eq!(root_of(d), reference::mth(d));
        for m in 0..n {
            let proof = tree.inclusion_proof(m as u64, n as u64).unwrap();
            assert_eq!(proof.path, reference::path(m, d), "path {m}/{n}");
            proof.verify(&d[m], &head).unwrap();
            for j in (0..n).filter(|j| *j != m) {
                let moved = InclusionProof {
                    index: j as u64,
                    ..proof.clone()
                };
                assert!(moved.verify(&d[m], &head).is_err(), "{m} as {j} of {n}");
            }
        }
        for m in 1..=n {
            let proof = tree.consistency_proof(m as u64, n as u64).unwrap();
            let expected = if m == n {
                vec![]
            } else {
                reference::subproof(m, d, true)
            };
            assert_eq!(proof.path, expected, "consistency {m}->{n}");
            let old = tree.head_at(m as u64).unwrap();
            proof.verify(&old, &head).unwrap();
            let fake = TreeHead {
                size: m as u64,
                root: reference::leaf(b"fake"),
            };
            assert!(proof.verify(&fake, &head).is_err());
            for other in (1..=n).filter(|o| *o != m) {
                let valid = tree.consistency_proof(other as u64, n as u64).unwrap();
                if valid.path != proof.path {
                    let moved = ConsistencyProof {
                        old_size: other as u64,
                        ..proof.clone()
                    };
                    let older = tree.head_at(other as u64).unwrap();
                    assert!(moved.verify(&older, &head).is_err(), "{m}->{n} as {other}");
                }
            }
        }
    }
}

/// Every single-hash replacement, insertion and removal in a proof of a tree
/// of up to 40 leaves is refused, and so are swapped heads.
#[test]
// PoC: a3_log_mutations_rejected_1_to_40
fn every_mutated_proof_up_to_40_is_refused() {
    let all = reference::leaves(40);
    let tree = tree_of(&all);
    let junk = reference::leaf(b"junk");
    for n in 1..=40u64 {
        let head = tree.head_at(n).unwrap();
        for m in 0..n {
            let good = tree.inclusion_proof(m, n).unwrap();
            let leaf = all[m as usize];
            for at in 0..good.path.len() {
                let mut bad = good.clone();
                bad.path[at] = junk;
                assert!(bad.verify(&leaf, &head).is_err());
                let mut bad = good.clone();
                bad.path.remove(at);
                assert!(bad.verify(&leaf, &head).is_err());
            }
            for at in 0..=good.path.len() {
                for extra in [junk, head.root] {
                    let mut bad = good.clone();
                    bad.path.insert(at, extra);
                    assert!(bad.verify(&leaf, &head).is_err());
                }
            }
            let resized = InclusionProof {
                size: n + 1,
                ..good.clone()
            };
            assert!(resized.verify(&leaf, &head).is_err());
        }
        for m in 1..=n {
            let old = tree.head_at(m).unwrap();
            let good = tree.consistency_proof(m, n).unwrap();
            for at in 0..good.path.len() {
                let mut bad = good.clone();
                bad.path[at] = junk;
                assert!(bad.verify(&old, &head).is_err(), "{m}->{n} at {at}");
                let mut bad = good.clone();
                bad.path.remove(at);
                assert!(bad.verify(&old, &head).is_err());
            }
            for at in 0..=good.path.len() {
                for extra in [junk, old.root] {
                    let mut bad = good.clone();
                    bad.path.insert(at, extra);
                    assert!(bad.verify(&old, &head).is_err());
                }
            }
            if m != n {
                let swapped = ConsistencyProof {
                    old_size: n,
                    new_size: m,
                    path: good.path.clone(),
                };
                assert!(swapped.verify(&head, &old).is_err());
            }
        }
    }
}

/// Edge sizes and hostile input are refused with a typed error, never a
/// panic: empty and vacuous proofs, indices at or beyond the size, sizes
/// above 2⁶², a million-hash path (refused by its length, before hashing),
/// plans at hostile sizes, historical sizes beyond the tree, and batches
/// that do not fit their plan.
#[test]
// PoC: a3_log_edge_and_hostile_inputs_are_refused_without_panic
fn edge_and_hostile_proof_inputs_are_refused_without_panic() {
    let l = reference::leaf(b"a");
    let one = TreeHead { size: 1, root: l };
    InclusionProof {
        index: 0,
        size: 1,
        path: vec![],
    }
    .verify(&l, &one)
    .unwrap();
    let empty = TreeHead {
        size: 0,
        root: empty_root(),
    };
    assert_eq!(
        InclusionProof {
            index: 0,
            size: 0,
            path: vec![]
        }
        .verify(&l, &empty),
        Err(ProofError::EmptyTree)
    );
    for new in [&one, &empty] {
        assert_eq!(
            ConsistencyProof {
                old_size: 0,
                new_size: new.size,
                path: vec![]
            }
            .verify(&empty, new),
            Err(ProofError::EmptyOldTree)
        );
    }
    for (index, size) in [(1u64, 1u64), (5, 5), (u64::MAX, 3), (u64::MAX, u64::MAX)] {
        let head = TreeHead { size, root: l };
        let proof = InclusionProof {
            index,
            size,
            path: vec![],
        };
        assert!(proof.verify(&l, &head).is_err());
    }
    for size in [MAX_TREE_SIZE + 1, u64::MAX, u64::MAX - 1] {
        let head = TreeHead { size, root: l };
        for len in [0usize, 62, 63, 64, 128] {
            let proof = InclusionProof {
                index: size - 1,
                size,
                path: vec![l; len],
            };
            assert!(proof.verify(&l, &head).is_err());
            let proof = ConsistencyProof {
                old_size: 1,
                new_size: size,
                path: vec![l; len],
            };
            assert!(proof.verify(&one, &head).is_err());
        }
    }
    let at_max = InclusionProof {
        index: MAX_TREE_SIZE - 1,
        size: MAX_TREE_SIZE,
        path: vec![l; 62],
    };
    assert!(at_max.root_from(&l).is_ok());

    let huge = vec![l; 1_000_000];
    let head8 = TreeHead { size: 8, root: l };
    let head3 = TreeHead { size: 3, root: l };
    let long = InclusionProof {
        index: 3,
        size: 8,
        path: huge.clone(),
    };
    assert!(matches!(
        long.verify(&l, &one),
        Err(ProofError::SizeMismatch { .. })
    ));
    assert!(matches!(
        long.verify(&l, &head8),
        Err(ProofError::PathLength { .. })
    ));
    let long = ConsistencyProof {
        old_size: 3,
        new_size: 8,
        path: huge,
    };
    assert!(matches!(
        long.verify(&head3, &head8),
        Err(ProofError::PathLength { .. })
    ));

    let same = ConsistencyProof {
        old_size: 1,
        new_size: 1,
        path: vec![],
    };
    same.verify(&one, &one).unwrap();
    let other = TreeHead {
        size: 1,
        root: reference::leaf(b"b"),
    };
    assert!(same.verify(&one, &other).is_err());
    let padded = ConsistencyProof {
        path: vec![l],
        ..same
    };
    assert!(padded.verify(&one, &one).is_err());

    for size in [MAX_TREE_SIZE + 1, u64::MAX] {
        assert!(RootPlan::new(size).is_err());
        assert!(InclusionPlan::new(0, size).is_err());
        assert!(ConsistencyPlan::new(1, size).is_err());
    }
    assert!(InclusionPlan::new(u64::MAX, 5).is_err());
    assert!(ConsistencyPlan::new(u64::MAX, 5).is_err());

    let tree = tree_of(&reference::leaves(5));
    assert!(tree.head_at(6).is_err());
    assert!(tree.head_at(u64::MAX).is_err());
    assert!(tree.inclusion_proof(0, 6).is_err());
    assert!(tree.inclusion_proof(5, 5).is_err());
    assert!(tree.consistency_proof(6, 6).is_err());
    assert!(tree.consistency_proof(5, u64::MAX).is_err());
    let plan = InclusionPlan::new(2, 5).unwrap();
    assert!(plan.assemble(&[]).is_err());
    assert!(plan.assemble(&vec![l; plan.ids().len() + 1]).is_err());
}

fn debit(minor: i128) -> Balance {
    Balance::ZERO
        .post(Side::Debit, Amount::from_minor(minor).unwrap())
        .unwrap()
}

fn handle(index: u32) -> AccountHandle {
    AccountHandle::new(index).unwrap()
}

/// Balance trees of 1 to 70 accounts, in four zero patterns, equal the
/// reference over explicit zero leaves (`0x81` ‖ 40 zero bytes), and every
/// handle's proof is the reference path at index = handle, size = accounts.
#[test]
// PoC: a3_log_balance_tree_matches_reference_1_to_70
fn balance_trees_equal_the_reference_over_explicit_zero_leaves() {
    let mut zero = [0u8; 41];
    zero[0] = 0x81;
    assert_eq!(zero_leaf_hash(), reference::leaf(&zero));
    for accounts in 1..=70u32 {
        for pattern in 0..4u32 {
            let mut tree: BalanceTree = BalanceTree::new(EUR, Layer::Settled, accounts);
            let mut explicit = Vec::new();
            for h in 0..accounts {
                let nonzero = match pattern {
                    0 => false,
                    1 => true,
                    2 => h % 3 == 2,
                    _ => h == accounts - 1,
                };
                let balance = if nonzero {
                    debit(i128::from(h) + 1)
                } else {
                    Balance::ZERO
                };
                tree.set(handle(h), balance).unwrap();
                explicit.push(reference::leaf(&balance_leaf_bytes(&balance)));
            }
            assert_eq!(
                tree.root(),
                reference::mth(&explicit),
                "{accounts}/{pattern}"
            );
            for h in 0..accounts {
                let proof = tree.prove(handle(h)).unwrap();
                assert_eq!(proof.path.path, reference::path(h as usize, &explicit));
                assert_eq!(proof.path.size, u64::from(accounts));
                assert_eq!(proof.path.index, u64::from(h));
            }
            assert!(tree.prove(handle(accounts)).is_err());
        }
    }
    let none: BalanceTree = BalanceTree::new(EUR, Layer::Settled, 0);
    assert_eq!(none.root(), empty_root());
    assert!(none.prove(handle(0)).is_err());
}

/// Seal trees refuse a duplicate (handle, currency, layer) and a handle
/// beyond the account count, report a totals overflow instead of wrapping,
/// and bind their size: 4 and 5 all-zero accounts give different roots.
#[test]
// PoC: a3_log_balance_tree_duplicates_and_bounds
fn seal_trees_refuse_duplicates_and_bind_their_size() {
    assert_eq!(
        build_seal_trees(
            3,
            [
                (handle(1), EUR, Layer::Settled, debit(1)),
                (handle(1), EUR, Layer::Settled, debit(2)),
            ]
        )
        .map(|_| ()),
        Err(ProofError::DuplicateBalance { handle: 1 })
    );
    assert!(build_seal_trees(3, [(handle(3), EUR, Layer::Settled, debit(1))]).is_err());
    // A zero balance is an absent one.
    let trees = build_seal_trees(
        3,
        [
            (handle(1), EUR, Layer::Settled, debit(1)),
            (handle(1), EUR, Layer::Settled, Balance::ZERO),
        ],
    )
    .unwrap();
    assert_eq!(trees[0].totals().unwrap(), debit(1));
    let max = Balance::ZERO
        .post(Side::Debit, Amount::from_minor(i128::MAX).unwrap())
        .unwrap();
    let trees = build_seal_trees(
        2,
        [
            (handle(0), EUR, Layer::Settled, max),
            (handle(1), EUR, Layer::Settled, debit(1)),
        ],
    )
    .unwrap();
    assert!(trees[0].totals().is_err());
    assert!(trees[0].head().is_err());
    let four: BalanceTree = BalanceTree::new(EUR, Layer::Settled, 4);
    let five: BalanceTree = BalanceTree::new(EUR, Layer::Settled, 5);
    assert_ne!(four.root(), five.root());
    let pair = node_hash(&zero_leaf_hash(), &zero_leaf_hash());
    assert_eq!(four.root(), node_hash(&pair, &pair));
}

/// A balance tree of `u32::MAX` accounts with two balances set proves any
/// handle against its root: zero subtrees are computed, never stored.
#[test]
// PoC: a3_log_balance_tree_sparse_large_is_cheap
fn a_sparse_balance_tree_of_u32_max_accounts_proves_every_handle() {
    let mut tree: BalanceTree = BalanceTree::new(EUR, Layer::Settled, u32::MAX);
    tree.set(handle(0), debit(1)).unwrap();
    tree.set(handle(u32::MAX - 1), debit(2)).unwrap();
    let root = tree.root();
    for h in [0u32, 1, 12_345, u32::MAX / 2, u32::MAX - 1] {
        let proof = tree.prove(handle(h)).unwrap();
        assert!(proof.path.path.len() <= 32);
        let leaf = leaf_hash(&balance_leaf_bytes(&proof.balance));
        assert_eq!(proof.path.root_from(&leaf).unwrap(), root);
    }
}

/// `Journal` proofs at its edge sizes: the empty head, sizes beyond the head
/// and vacuous or backwards consistency are refused; every historical size
/// proves consistent with the head.
#[test]
// PoC: a3_log_journal_edge_sizes
fn journal_proofs_at_their_edge_sizes() {
    let mut books = Journal::new(LedgerId::new("example.com/audit").unwrap());
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books.define_months(2026, 3..=4).unwrap();
    for i in 0..3 {
        let path = format!("Assets:A{i}");
        books
            .open(path.as_str(), AccountKind::Asset, date!("2026-01-01"))
            .unwrap();
        books
            .record(
                Entry::new(format!("e{i}"), date!("2026-03-15"))
                    .debit(path.as_str(), amount!("1.00"), EUR)
                    .credit("Income:Sales", amount!("1.00"), EUR),
            )
            .unwrap();
    }
    books.seal_period("2026-03").unwrap();
    let head = books.head();
    assert_eq!(
        books.head_at(0).unwrap(),
        TreeHead {
            size: 0,
            root: empty_root()
        }
    );
    assert!(books.head_at(head.size + 1).is_err());
    assert!(books.head_at(u64::MAX).is_err());
    let id = books.entry_by_key("e0").unwrap().id(books.id());
    assert!(books.prove_entry_at(id, 0).is_err());
    assert!(books.prove_entry_at(id, head.size + 1).is_err());
    assert!(books.prove_consistency(0, head.size).is_err());
    assert!(books.prove_consistency(head.size, head.size - 1).is_err());
    assert!(books.prove_consistency(1, u64::MAX).is_err());
    let same = books.prove_consistency(head.size, head.size).unwrap();
    assert!(same.path.is_empty());
    same.verify(&head, &head).unwrap();
    for m in 1..=head.size {
        let old = books.head_at(m).unwrap();
        books
            .prove_consistency(m, head.size)
            .unwrap()
            .verify(&old, &head)
            .unwrap();
    }
}

/// A3 L-2: a node count between two tree sizes is refused naming the count
/// of the next tree size (22 for 20 or 21 nodes: 11 leaves store 19, 12
/// store 22), never a count no tree has.
#[test]
// PoC: a3_log_from_stored_error_reports_meaningless_expected_count
fn from_stored_names_the_next_valid_node_count() {
    let tree = tree_of(&reference::leaves(12));
    assert_eq!(doubleentry::store::stored_count(11), Ok(19));
    assert_eq!(doubleentry::store::stored_count(12), Ok(22));
    for found in [20, 21] {
        let mut nodes = tree.stored().to_vec();
        nodes.truncate(found);
        assert_eq!(
            Tree::from_stored(nodes),
            Err(ProofError::MalformedNodes {
                expected: 22,
                found
            })
        );
    }
    let mut nodes = tree.stored().to_vec();
    nodes.truncate(19);
    assert!(Tree::from_stored(nodes).is_ok());
}

// ── A refused batch needs no rollback (Q-E7) ───────────────────────────────

/// Q-E7: a batch is decided against an overlay of the state and applied
/// only when every draft is accepted, so a batch refused at its second draft
/// (a key conflict, as in the proof of concept) on 5 000 entries of history
/// changes nothing — head, balances and keys — and undoes nothing. The read
/// count of a refusal, independent of history, is the rule core's
/// `qe7_rejection_cost_independent_of_history`.
#[test]
// PoC: qe7_batch_rollback_cost_scales_with_history
fn a_refused_batch_changes_nothing_and_undoes_nothing() {
    let mut books = Journal::new(LedgerId::new("example.com/qe7").unwrap());
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    books.open("Assets:Cash", AccountKind::Asset, on).unwrap();
    books.open("Income:Sales", AccountKind::Income, on).unwrap();
    books.define_months(2026, 1..=12).unwrap();
    let sale = |key: &str, minor: i128| {
        let amount = Amount::from_minor(minor).unwrap();
        Entry::new(key, date!("2026-03-15"))
            .debit("Assets:Cash", amount, EUR)
            .credit("Income:Sales", amount, EUR)
    };
    for n in 0..5_000 {
        books.record(sale(&format!("k{n}"), 100)).unwrap();
    }
    let query = doubleentry::BalanceQuery::all();
    let before = (
        books.head(),
        books.balance("Assets:Cash", EUR, &query).unwrap(),
    );
    let refused = books
        .record_batch([sale("batch-ok", 100), sale("k10", 999)])
        .unwrap_err();
    assert!(
        matches!(
            refused,
            doubleentry::Error::BatchRejected {
                index: 1,
                rejection: doubleentry::Rejection::KeyConflict { .. }
            }
        ),
        "{refused:?}"
    );
    assert_eq!(
        (
            books.head(),
            books.balance("Assets:Cash", EUR, &query).unwrap()
        ),
        before
    );
    assert!(books.entry_by_key("batch-ok").is_none());
    books.audit().unwrap();
}

// ── The model (audit round 3) ────────────────────────────────────────────────

/// A3 M-4: `Rate` ignores a format precision like every other value: the
/// rate is never cut short; width and alignment still apply.
#[test]
// PoC: a3_model_rate_display_truncated_by_precision
fn a_rate_is_never_cut_short_by_a_precision() {
    let rate = Rate::parse(EUR, USD, "1.0873").unwrap();
    assert_eq!(format!("{rate:.3}"), "1.0873");
    assert_eq!(format!("{rate:>8.1}"), "  1.0873");
    assert_eq!(format!("{rate:<8}|"), "1.0873  |");
    let label = Label::new("abcdef").unwrap();
    assert_eq!(format!("{label:.3}"), "abcdef");
}

/// A3 M-2: a currency declaration with an exponent above the precision
/// never reaches a log — the façade refuses it and the decoder refuses its
/// bytes. (The record type itself still encodes one: its fields are public.)
#[test]
// PoC: a3_model_currency_decl_encodes_undecodable
fn an_exponent_above_the_precision_never_reaches_the_log() {
    let mut books = Journal::new(LedgerId::new("example.com/a3").unwrap());
    let head = books.head();
    assert!(books.declare_currency(EUR, 3, date!("2026-01-01")).is_err());
    assert_eq!(books.head(), head);
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    let declaration = |exponent| {
        Record::<2>::Currency(CurrencyDeclaration {
            code: EUR,
            exponent,
            effective: date!("2026-01-01"),
            actor: Label::new("system").unwrap(),
        })
    };
    let two = declaration(2).to_leaf_bytes();
    assert_eq!(Record::<2>::decode(&two).unwrap(), declaration(2));
    assert!(Record::<2>::decode(&declaration(9).to_leaf_bytes()).is_err());
}

/// A3 M-3: `Posting::dimensions` replaces the dimensions and the errors of
/// the `.dimension` calls before it: the draft is valid.
#[test]
// PoC: a3_model_dimensions_setter_keeps_stale_errors
fn setting_dimensions_replaces_earlier_dimension_errors() {
    let dims = Dimensions::new()
        .with(Label::new("project").unwrap(), Label::new("alpha").unwrap())
        .unwrap();
    let posting = Posting::<2>::debit("Assets:Cash", amount!("1.00"), EUR)
        .dimension(" bad", "x")
        .dimensions(dims.clone());
    assert_eq!(posting.get_dimensions(), &dims);
    let draft: Entry = Entry::new("k", date!("2026-01-01"))
        .posting(posting)
        .credit("Income:Sales", amount!("1.00"), EUR);
    assert!(
        draft.build_errors().is_empty(),
        "{:?}",
        draft.build_errors()
    );
    assert!(draft.fingerprint().is_ok());
    // A bad dimension after the set is still reported.
    let late = Posting::<2>::debit("Assets:Cash", amount!("1.00"), EUR)
        .dimensions(dims)
        .dimension(" bad", "x");
    let draft: Entry = Entry::new("k", date!("2026-01-01")).posting(late).credit(
        "Income:Sales",
        amount!("1.00"),
        EUR,
    );
    assert_eq!(draft.build_errors().len(), 1);
}

/// A3 M-5, as documented: `encoded_len` is the leaf length of an ordinary
/// draft; a hold resolution's leaf adds the derived postings, and stays
/// within the leaf limit.
#[test]
// PoC: a3_model_encoded_len_misses_derived_hold_postings
fn encoded_len_is_the_leaf_length_except_for_derived_postings() {
    let mut books = Journal::new(LedgerId::new("example.com/a3").unwrap());
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    for path in ["Liabilities:A", "Liabilities:B"] {
        books
            .open(path, AccountKind::Liability, date!("2026-01-01"))
            .unwrap();
    }
    books.define_months(2026, 11..=11).unwrap();
    let leaf_len = |books: &Journal, id| {
        Record::Entry(books.entry(id).unwrap())
            .to_leaf_bytes()
            .len()
    };
    let plain: Entry = Entry::new("plain", date!("2026-11-19"))
        .debit("Liabilities:A", amount!("1.00"), EUR)
        .credit("Liabilities:B", amount!("1.00"), EUR);
    let plain_len = plain.encoded_len();
    let plain = books.record(plain).unwrap();
    assert_eq!(leaf_len(&books, plain.id), plain_len);
    let hold = books
        .record(
            Entry::open_hold("auth", date!("2026-11-20"))
                .debit("Liabilities:A", amount!("100.00"), EUR)
                .credit("Liabilities:B", amount!("100.00"), EUR),
        )
        .unwrap();
    let post: Entry = Entry::post_hold("cap", date!("2026-11-22"), hold.id, PostAmount::Full);
    let draft_len = post.encoded_len();
    let post = books.record(post).unwrap();
    let len = leaf_len(&books, post.id);
    assert!(len > draft_len, "{len} vs {draft_len}");
    assert!(len <= doubleentry::record::MAX_LEAF_BYTES);
}

/// `EntryId::derive` length-prefixes the ledger and the key: ("a", "bc")
/// and ("ab", "c") never collide.
#[test]
// PoC: a3_model_entry_id_is_length_prefixed
fn entry_ids_length_prefix_ledger_and_key() {
    let a = EntryId::derive(
        &LedgerId::new("a").unwrap(),
        &IdempotencyKey::new("bc").unwrap(),
    );
    let b = EntryId::derive(
        &LedgerId::new("ab").unwrap(),
        &IdempotencyKey::new("c").unwrap(),
    );
    assert_ne!(a, b);
}

/// A draft's fingerprint is of the draft as submitted: naming accounts by
/// path or by handle, or adding a memo, changes it; the leaf length does not
/// depend on how an account is named.
#[test]
// PoC: a3_model_fingerprint_path_vs_handle_differ
fn draft_fingerprints_cover_the_draft_as_given() {
    let by_path: Entry = Entry::new("k", date!("2026-01-01"))
        .debit("A", amount!("1.00"), EUR)
        .credit("B", amount!("1.00"), EUR);
    let by_handle: Entry = Entry::new("k", date!("2026-01-01"))
        .debit(handle(0), amount!("1.00"), EUR)
        .credit(handle(1), amount!("1.00"), EUR);
    assert_ne!(
        by_path.fingerprint().unwrap(),
        by_handle.fingerprint().unwrap()
    );
    let with_memo = by_path.clone().memo("ab");
    assert_ne!(
        with_memo.fingerprint().unwrap(),
        by_path.fingerprint().unwrap()
    );
    assert_eq!(by_path.encoded_len(), by_handle.encoded_len());
}

/// Mutated leaves (bit flips, byte replacements, insertions, removals) never
/// panic the decoder, and whatever decodes re-encodes to exactly its input:
/// one representation per value.
#[test]
// PoC: a3_model_decode_mutation_fuzz_is_canonical
fn mutated_leaves_decode_canonically_or_not_at_all() {
    let dims = Dimensions::new()
        .with(Label::new("b").unwrap(), Label::new("é").unwrap())
        .unwrap()
        .with(Label::new("a").unwrap(), Label::new("x").unwrap())
        .unwrap();
    let posting = |h, side, layer| PostingRecord::<2> {
        handle: handle(h),
        side,
        amount: amount!("1.00"),
        currency: EUR,
        layer,
        dimensions: dims.clone(),
    };
    let entry = EntryRecord::new(EntryParts {
        key: IdempotencyKey::new("k1").unwrap(),
        booking_date: date!("2026-01-01"),
        value_date: date!("2026-01-02"),
        captured_at: Some(doubleentry::CaptureTime::new(1, 60).unwrap()),
        kind: Some(Label::new("inv").unwrap()),
        memo: None,
        documents: vec![
            doubleentry::Document::new("invoice", "R-1")
                .unwrap()
                .dated(date!("2026-01-01")),
        ],
        postings: vec![
            posting(0, Side::Debit, Layer::Settled),
            posting(1, Side::Credit, Layer::Pending),
        ],
        reverses: Some(EntryId::from_bytes([1; 16])),
        hold: None,
        rates: vec![Rate::parse(EUR, USD, "1.25").unwrap()],
    })
    .unwrap();
    let seeds = [
        Record::<2>::Entry(entry).to_leaf_bytes(),
        Record::<2>::Genesis(Genesis {
            ledger: LedgerId::new("x.y/z").unwrap(),
        })
        .to_leaf_bytes(),
    ];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut decoded = 0u32;
    for seed in &seeds {
        for _ in 0..50_000 {
            let mut bytes = seed.clone();
            for _ in 0..1 + next() % 3 {
                let at = (next() as usize) % bytes.len();
                match next() % 4 {
                    0 => bytes[at] ^= 1 << (next() % 8),
                    1 => bytes[at] = next() as u8,
                    2 => {
                        bytes.remove(at);
                        if bytes.is_empty() {
                            bytes.push(0);
                        }
                    }
                    _ => bytes.insert(at, next() as u8),
                }
            }
            if let Ok(record) = Record::<2>::decode(&bytes) {
                decoded += 1;
                assert_eq!(record.to_leaf_bytes(), bytes, "non-canonical: {record:?}");
            }
        }
    }
    assert!(decoded > 0);
}

// ── Every store answers as the `Journal` (Q-S1, Q-S6) ───────────────────────

/// A `Journal` and a `Ledger` over `store` with the same books: EUR, cash,
/// sales, retained earnings and the months of 2026.
#[cfg(feature = "testing")]
async fn parity_books<S: doubleentry::store::Store<2>>(
    store: S,
) -> (Journal, doubleentry::Ledger<S>) {
    use doubleentry::{Ledger, MemoSalt};
    let ledger = LedgerId::new("example.com/poc").unwrap();
    let mut journal = Journal::new(ledger.clone()).with_salt_source(|| MemoSalt([1; 16]));
    let books = Ledger::new(store, ledger)
        .await
        .unwrap()
        .with_salt_source(|| MemoSalt([1; 16]));
    let on = date!("2026-01-01");
    journal.declare_currency(EUR, 2, on).unwrap();
    books.declare_currency(EUR, 2, on).await.unwrap();
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Income:Sales", AccountKind::Income),
        ("Equity:Retained", AccountKind::Equity),
    ] {
        assert_eq!(
            journal.open(path, kind, on).unwrap(),
            books.open(path, kind, on).await.unwrap()
        );
    }
    journal.define_months(2026, 1..=12).unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    (journal, books)
}

/// Q-S1: a batch of an entry and its reversal is recorded in batch order,
/// contiguously: the receipts and the head equal the `Journal`'s, and the
/// log replays in its own order (`audit`). PostgreSQL: `tests/postgres.rs`.
#[cfg(feature = "testing")]
// PoC: q_s1_deferred_sequencer_reorders_a_batch_by_uuid
async fn regress_qs1_batch_order_is_log_order<S: doubleentry::store::Store<2>>(store: S) {
    let (mut journal, books) = parity_books(store).await;
    let original = EntryId::derive(journal.id(), &IdempotencyKey::new("orig").unwrap());
    let batch = || {
        vec![
            Entry::new("orig", date!("2026-03-15"))
                .debit("Assets:Cash", amount!("5.00"), EUR)
                .credit("Income:Sales", amount!("5.00"), EUR),
            Entry::new("rev", date!("2026-03-16"))
                .credit("Assets:Cash", amount!("5.00"), EUR)
                .debit("Income:Sales", amount!("5.00"), EUR)
                .reverses(original),
        ]
    };
    let start = books.head().await.unwrap().size;
    let expected = journal.record_batch(batch()).unwrap();
    let receipts = books.record_batch(batch()).await.unwrap();
    assert_eq!(receipts, expected);
    assert_eq!(
        receipts.iter().map(|r| r.position).collect::<Vec<_>>(),
        [start, start + 1]
    );
    assert_eq!(receipts[0].id, original);
    assert_eq!(books.head().await.unwrap(), journal.head());
    books.audit().await.unwrap();
}

/// Q-S6 (the SQLite and memory baseline of the collation finding): paths and
/// dimension values compare by bytes on every store — the leaf rule, every
/// refusal and year-end closing answer exactly as on the `Journal`.
/// PostgreSQL under `en_US.utf8`: `tests/postgres.rs`.
#[cfg(feature = "testing")]
// PoC: q_s6_baseline_dimension_values_byte_order_on_sqlite_and_memory
async fn regress_qs6_byte_order<S: doubleentry::store::Store<2>>(store: S) {
    let (mut journal, books) = parity_books(store).await;
    let names = ["apple", "émigré", "Network", "Zebra"];
    let on = date!("2026-01-01");
    for name in names {
        let path = format!("Income:{name}");
        assert_eq!(
            journal.open(&path, AccountKind::Income, on).unwrap(),
            books.open(&path, AccountKind::Income, on).await.unwrap()
        );
    }
    // `Income:apple:x` makes `Income:apple` a parent under byte order only.
    journal
        .open("Income:apple:x", AccountKind::Income, on)
        .unwrap();
    books
        .open("Income:apple:x", AccountKind::Income, on)
        .await
        .unwrap();
    for (n, name) in names.iter().enumerate() {
        for value in names {
            let entry = Entry::new(format!("s-{n}-{value}"), date!("2026-02-10"))
                .debit("Assets:Cash", amount!("1.00"), EUR)
                .posting(
                    Posting::credit(format!("Income:{name}"), amount!("1.00"), EUR)
                        .dimension("segment", value),
                );
            let expected = journal
                .record(entry.clone())
                .map_err(|error| error.rejection().map(|r| r.code()));
            let actual = books
                .record(entry)
                .await
                .map_err(|error| error.rejection().map(|r| r.code()));
            assert_eq!(expected, actual, "Income:{name} / {value}");
        }
    }
    let expected = journal
        .close_year("close", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    let actual = books
        .close_year("close", date!("2026-12-31"), "Equity:Retained")
        .await
        .unwrap();
    assert_eq!(expected, actual);
    assert_eq!(books.head().await.unwrap(), journal.head());
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn regress_qs1_qs6_in_memory() {
    regress_qs1_batch_order_is_log_order(doubleentry::store::MemoryStore::new()).await;
    regress_qs6_byte_order(doubleentry::store::MemoryStore::new()).await;
}

#[cfg(all(feature = "testing", feature = "sqlite"))]
#[tokio::test]
async fn regress_qs1_qs6_on_sqlite() {
    use doubleentry::store::{SqlStore, Sqlite};
    let dir = tempfile::tempdir().unwrap();
    let url = |name: &str| format!("sqlite://{}", dir.path().join(name).display());
    let store = SqlStore::<Sqlite>::sqlite(&url("qs1.db")).await.unwrap();
    regress_qs1_batch_order_is_log_order(store).await;
    let store = SqlStore::<Sqlite>::sqlite(&url("qs6.db")).await.unwrap();
    regress_qs6_byte_order(store).await;
}

// ── Wrong stores fail the conformance suite (A3-S8) ──────────────────────────

/// A3-S8: stores that break the read contract fail the suite. Two of the
/// proof of concept's four wrong stores are canaries (`Fault::IgnoresThrough`,
/// `Fault::NoOpenHoldsByAccount` in `tests/memory.rs`); the other two are
/// here.
#[cfg(feature = "testing")]
mod wrong_stores {
    use doubleentry::store::{
        AccountRow, BalanceKey, BalanceRead, Done, EntryRef, Erasure, HoldRead, HoldRow, Leaf,
        LeafRead, LedgerInfo, Memo, MemoryStore, Op, Position, PostingCursor, PostingLine,
        PostingRead, ProjectionPage, ProjectionRead, SealRow, Store, StoreError, StoredEntry,
    };
    use doubleentry::testing::{Config, conformance_with};
    use doubleentry::{
        AccountHandle, AccountRef, Balance, Currency, EntryId, HoldCursor, Layer, Page, PageRead,
        PeriodId, Rejection, log::Hash, record::Genesis,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Defect {
        /// `account_records` always answers an empty page.
        NoAccountRecords,
        /// `read_leaves` returns every kind, whatever `kinds` asks.
        IgnoresKinds,
    }

    #[derive(Clone)]
    struct Wrong {
        inner: MemoryStore,
        defect: Defect,
    }

    impl Store<2> for Wrong {
        async fn create(&self, genesis: &Genesis) -> Result<LedgerInfo, StoreError> {
            self.inner.create(genesis).await
        }
        async fn open(&self) -> Result<LedgerInfo, StoreError> {
            self.inner.open().await
        }
        async fn write(&self, op: &Op<2>) -> Result<Result<Done<2>, Rejection>, StoreError> {
            self.inner.write(op).await
        }
        async fn size(&self) -> Result<u64, StoreError> {
            self.inner.size().await
        }
        async fn read_leaves(&self, read: &LeafRead) -> Result<Page<Leaf, Position>, StoreError> {
            if self.defect == Defect::IgnoresKinds {
                let mut read = read.clone();
                read.kinds = None;
                return self.inner.read_leaves(&read).await;
            }
            self.inner.read_leaves(read).await
        }
        async fn read_nodes(&self, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
            self.inner.read_nodes(ids).await
        }
        async fn seals(&self, through: Option<u64>) -> Result<Vec<SealRow>, StoreError> {
            self.inner.seals(through).await
        }
        async fn accounts(
            &self,
            refs: &[AccountRef],
        ) -> Result<Vec<Option<AccountRow>>, StoreError> {
            self.inner.accounts(refs).await
        }
        async fn account_records(
            &self,
            handle: AccountHandle,
            page: PageRead<Position>,
        ) -> Result<Page<Leaf, Position>, StoreError> {
            let mut found = self.inner.account_records(handle, page).await?;
            if self.defect == Defect::NoAccountRecords {
                found.items.clear();
                found.next = None;
            }
            Ok(found)
        }
        async fn entry(
            &self,
            by: EntryRef<'_>,
            through: Option<u64>,
        ) -> Result<Option<StoredEntry>, StoreError> {
            self.inner.entry(by, through).await
        }
        async fn holds(&self, read: &HoldRead) -> Result<Page<HoldRow<2>, HoldCursor>, StoreError> {
            self.inner.holds(read).await
        }
        async fn clearings(
            &self,
            key: BalanceKey,
            page: PageRead<Position>,
        ) -> Result<Page<Leaf, Position>, StoreError> {
            self.inner.clearings(key, page).await
        }
        async fn projection(&self, read: &ProjectionRead) -> Result<ProjectionPage<2>, StoreError> {
            self.inner.projection(read).await
        }
        async fn seal_nodes(
            &self,
            period: &PeriodId,
            currency: Currency,
            layer: Layer,
            ids: &[(u32, u32)],
        ) -> Result<Option<Vec<Option<Hash>>>, StoreError> {
            self.inner.seal_nodes(period, currency, layer, ids).await
        }
        async fn balances(
            &self,
            read: &BalanceRead,
        ) -> Result<Page<(BalanceKey, Balance), BalanceKey>, StoreError> {
            self.inner.balances(read).await
        }
        async fn postings(
            &self,
            read: &PostingRead,
        ) -> Result<Page<PostingLine<2>, PostingCursor>, StoreError> {
            self.inner.postings(read).await
        }
        async fn memo(&self, id: EntryId) -> Result<Option<Memo>, StoreError> {
            self.inner.memo(id).await
        }
        async fn erase_memo(&self, id: EntryId) -> Result<Erasure, StoreError> {
            self.inner.erase_memo(id).await
        }
    }

    async fn caught(defect: Defect) {
        let failure = conformance_with(
            move || async move {
                Wrong {
                    inner: MemoryStore::new(),
                    defect,
                }
            },
            Config::default(),
        )
        .await
        .expect_err("a wrong store must fail the suite");
        println!("{defect:?} caught by {}", failure.check);
    }

    #[tokio::test(flavor = "multi_thread")]
    // PoC: a3_s8_wrong_stores_pass_the_suite
    async fn a3_s8_a_store_without_account_records_fails_the_suite() {
        caught(Defect::NoAccountRecords).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    // PoC: a3_s8_wrong_stores_pass_the_suite
    async fn a3_s8_a_store_ignoring_leaf_kinds_fails_the_suite() {
        caught(Defect::IgnoresKinds).await;
    }
}
