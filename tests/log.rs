//! The log: RFC 6962 proofs, and sealed balances proven and named.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::store::Tree;
use doubleentry::{
    AccountHandle, AccountKind, Amount, Balance, BalanceLimit, EntryRecord, IdempotencyKey, Label,
    Layer, LedgerId, Side, TreeHead,
    currency::{EUR, USD},
    log::{
        BalanceTree, ConsistencyProof, Hash, InclusionProof, ProofError, SealedBalanceProof,
        build_seal_trees, empty_root, leaf_hash, root_of,
    },
    record::{
        AccountAction, AccountChange, AccountEvent, AccountRegistration, CurrencyDeclaration,
        EntryParts, Genesis, PeriodDefinition, PostingRecord, Record, RecordKind, Seal, SealParts,
    },
};
use time::macros::date;

// ── US4: standard proofs ──────────────────────────────────────────────────────

fn h(text: &str) -> Hash {
    text.parse().unwrap()
}

fn rfc6962_leaves() -> Vec<Hash> {
    let inputs: [&[u8]; 8] = [
        b"",
        &[0x00],
        &[0x10],
        &[0x20, 0x21],
        &[0x30, 0x31],
        &[0x40, 0x41, 0x42, 0x43],
        &[0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57],
        &[
            0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d,
            0x6e, 0x6f,
        ],
    ];
    inputs.iter().map(|input| leaf_hash(input)).collect()
}

const ROOTS: [&str; 9] = [
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
    "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
    "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77",
    "d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7",
    "4e3bbb1f7b478dcfe71fb631631519a3bca12c9aefca1612bfce4c13a86264d4",
    "76e67dadbcdf1e10e1b74ddc608abd2f98dfb16fbce75277b5232a127f2087ef",
    "ddb89be403809e325750d3d263cd78929c2942b7942a34b77e122c9594a74c8c",
    "5dc9da79a70659a9ad559cb701ded9a2ab9d823aad2f4960cfe370eff4604328",
];

fn inclusion_vectors() -> Vec<(u64, u64, Vec<Hash>)> {
    vec![
        (
            0,
            8,
            vec![
                h("96a296d224f285c67bee93c30f8a309157f0daa35dc5b87e410b78630a09cfc7"),
                h("5f083f0a1a33ca076a95279832580db3e0ef4584bdff1f54c8a360f50de3031e"),
                h("6b47aaf29ee3c2af9af889bc1fb9254dabd31177f16232dd6aab035ca39bf6e4"),
            ],
        ),
        (
            5,
            8,
            vec![
                h("bc1a0643b12e4d2d7c77918f44e0f4f79a838b6cf9ec5b5c283e1f4d88599e6b"),
                h("ca854ea128ed050b41b35ffc1b87b8eb2bde461e9e3b5596ece6b9d5975a0ae0"),
                h("d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7"),
            ],
        ),
        (
            2,
            3,
            vec![h(
                "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
            )],
        ),
        (
            1,
            5,
            vec![
                h("6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d"),
                h("5f083f0a1a33ca076a95279832580db3e0ef4584bdff1f54c8a360f50de3031e"),
                h("bc1a0643b12e4d2d7c77918f44e0f4f79a838b6cf9ec5b5c283e1f4d88599e6b"),
            ],
        ),
    ]
}

fn consistency_vectors() -> Vec<(u64, u64, Vec<Hash>)> {
    vec![
        (
            1,
            8,
            vec![
                h("96a296d224f285c67bee93c30f8a309157f0daa35dc5b87e410b78630a09cfc7"),
                h("5f083f0a1a33ca076a95279832580db3e0ef4584bdff1f54c8a360f50de3031e"),
                h("6b47aaf29ee3c2af9af889bc1fb9254dabd31177f16232dd6aab035ca39bf6e4"),
            ],
        ),
        (
            6,
            8,
            vec![
                h("0ebc5d3437fbe2db158b9f126a1d118e308181031d0a949f8dededebc558ef6a"),
                h("ca854ea128ed050b41b35ffc1b87b8eb2bde461e9e3b5596ece6b9d5975a0ae0"),
                h("d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7"),
            ],
        ),
        (
            2,
            5,
            vec![
                h("5f083f0a1a33ca076a95279832580db3e0ef4584bdff1f54c8a360f50de3031e"),
                h("bc1a0643b12e4d2d7c77918f44e0f4f79a838b6cf9ec5b5c283e1f4d88599e6b"),
            ],
        ),
    ]
}

fn tree_of(leaves: &[Hash]) -> Tree {
    let mut tree = Tree::new();
    for leaf in leaves {
        tree.push(*leaf).unwrap();
    }
    tree
}

/// 64 distinct leaves: the eight reference leaves, then more.
fn leaves_64() -> Vec<Hash> {
    let mut leaves = rfc6962_leaves();
    leaves.extend((8u8..64).map(|i| leaf_hash(&[0xee, i])));
    leaves
}

#[test]
fn us4_rfc6962_roots() {
    let leaves = rfc6962_leaves();
    for (n, root) in ROOTS.iter().enumerate() {
        assert_eq!(
            tree_of(&leaves[..n]).head(),
            TreeHead {
                size: n as u64,
                root: h(root)
            }
        );
        assert_eq!(root_of(&leaves[..n]), h(root));
    }
    assert_eq!(empty_root(), h(ROOTS[0]));
}

#[test]
fn us4_published_proofs_at_exact_and_historical_sizes() {
    let leaves = rfc6962_leaves();
    let big = tree_of(&leaves_64());
    for (index, size, path) in inclusion_vectors() {
        let exact = tree_of(&leaves[..size as usize]);
        let expected = InclusionProof { index, size, path };
        assert_eq!(exact.inclusion_proof(index, size).unwrap(), expected);
        assert_eq!(big.inclusion_proof(index, size).unwrap(), expected);
        expected
            .verify(&leaves[index as usize], &big.head_at(size).unwrap())
            .unwrap();
    }
    for (old_size, new_size, path) in consistency_vectors() {
        let exact = tree_of(&leaves[..new_size as usize]);
        let expected = ConsistencyProof {
            old_size,
            new_size,
            path,
        };
        assert_eq!(
            exact.consistency_proof(old_size, new_size).unwrap(),
            expected
        );
        assert_eq!(big.consistency_proof(old_size, new_size).unwrap(), expected);
        expected
            .verify(
                &big.head_at(old_size).unwrap(),
                &big.head_at(new_size).unwrap(),
            )
            .unwrap();
    }
}

/// RFC 6962's split point: the largest power of two smaller than `n`.
fn largest_power_of_two_below(n: usize) -> usize {
    let mut k = 1;
    while k * 2 < n {
        k *= 2;
    }
    k
}

/// RFC 6962 §2.1.1 `PATH(m, D[n])`, written from the RFC.
fn reference_path(m: usize, leaves: &[Hash]) -> Vec<Hash> {
    let n = leaves.len();
    if n <= 1 {
        return vec![];
    }
    let k = largest_power_of_two_below(n);
    if m < k {
        let mut path = reference_path(m, &leaves[..k]);
        path.push(root_of(&leaves[k..]));
        path
    } else {
        let mut path = reference_path(m - k, &leaves[k..]);
        path.push(root_of(&leaves[..k]));
        path
    }
}

/// RFC 6962 §2.1.2 `SUBPROOF(m, D[n], b)`.
fn reference_subproof(m: usize, leaves: &[Hash], complete: bool) -> Vec<Hash> {
    let n = leaves.len();
    if m == n {
        return if complete {
            vec![]
        } else {
            vec![root_of(leaves)]
        };
    }
    let k = largest_power_of_two_below(n);
    if m <= k {
        let mut proof = reference_subproof(m, &leaves[..k], complete);
        proof.push(root_of(&leaves[k..]));
        proof
    } else {
        let mut proof = reference_subproof(m - k, &leaves[k..], false);
        proof.push(root_of(&leaves[..k]));
        proof
    }
}

#[test]
fn us4_every_proof_up_to_64_equals_the_recursive_reference() {
    let leaves = leaves_64();
    let tree = tree_of(&leaves);
    for n in 1..=64usize {
        assert_eq!(tree.head_at(n as u64).unwrap().root, root_of(&leaves[..n]));
        for i in 0..n {
            let proof = tree.inclusion_proof(i as u64, n as u64).unwrap();
            assert_eq!(
                proof.path,
                reference_path(i, &leaves[..n]),
                "inclusion {i}/{n}"
            );
            proof
                .verify(&leaves[i], &tree.head_at(n as u64).unwrap())
                .unwrap();
        }
        for m in 1..=n {
            let proof = tree.consistency_proof(m as u64, n as u64).unwrap();
            let expected = if m == n {
                vec![]
            } else {
                reference_subproof(m, &leaves[..n], true)
            };
            assert_eq!(proof.path, expected, "consistency {m}->{n}");
            proof
                .verify(
                    &tree.head_at(m as u64).unwrap(),
                    &tree.head_at(n as u64).unwrap(),
                )
                .unwrap();
        }
    }
    assert_eq!(tree.head_at(0).unwrap().root, empty_root());
}

fn flipped(path: &[Hash], at: usize, bit: u8) -> Vec<Hash> {
    let mut path = path.to_vec();
    let mut bytes = *path[at].as_bytes();
    bytes[usize::from(bit / 8)] ^= 1 << (bit % 8);
    path[at] = Hash::from_bytes(bytes);
    path
}

#[test]
fn us4_every_deformed_proof_fails() {
    let leaves = leaves_64();
    let tree = tree_of(&leaves);
    for n in 1..=32u64 {
        let head = tree.head_at(n).unwrap();
        for i in 0..n {
            let leaf = leaves[i as usize];
            let good = tree.inclusion_proof(i, n).unwrap();
            for at in 0..good.path.len() {
                for bit in [0u8, 7, 128, 255] {
                    let bad = InclusionProof {
                        path: flipped(&good.path, at, bit),
                        ..good.clone()
                    };
                    assert!(bad.verify(&leaf, &head).is_err());
                }
                let mut removed = good.clone();
                removed.path.remove(at);
                assert!(removed.verify(&leaf, &head).is_err());
            }
            let mut added = good.clone();
            added.path.push(leaf);
            assert!(added.verify(&leaf, &head).is_err());
            for index in [i.wrapping_sub(1), i + 1] {
                let shifted = InclusionProof {
                    index,
                    ..good.clone()
                };
                assert!(
                    shifted.verify(&leaf, &head).is_err(),
                    "index {index} of {n}"
                );
            }
            for size in [n - 1, n + 1] {
                let resized = InclusionProof {
                    size,
                    ..good.clone()
                };
                assert!(resized.verify(&leaf, &head).is_err());
                if (1..=64).contains(&size) {
                    assert!(resized.verify(&leaf, &tree.head_at(size).unwrap()).is_err());
                }
            }
            let wrong_head = TreeHead {
                size: n,
                root: leaf_hash(b"wrong"),
            };
            assert!(good.verify(&leaf, &wrong_head).is_err());
        }
        for m in 1..=n {
            let old = tree.head_at(m).unwrap();
            let good = tree.consistency_proof(m, n).unwrap();
            for at in 0..good.path.len() {
                let bad = ConsistencyProof {
                    path: flipped(&good.path, at, 3),
                    ..good.clone()
                };
                assert!(bad.verify(&old, &head).is_err());
                let mut removed = good.clone();
                removed.path.remove(at);
                assert!(removed.verify(&old, &head).is_err());
            }
            let mut added = good.clone();
            added.path.push(head.root);
            assert!(added.verify(&old, &head).is_err());
            if m > 1 {
                let older = tree.head_at(m - 1).unwrap();
                let shifted = ConsistencyProof {
                    old_size: m - 1,
                    ..good.clone()
                };
                assert!(
                    shifted.verify(&older, &head).is_err(),
                    "{m}->{n} as {}->{n}",
                    m - 1
                );
            }
            if n < 64 && m < n {
                let newer = tree.head_at(n + 1).unwrap();
                let shifted = ConsistencyProof {
                    new_size: n + 1,
                    ..good.clone()
                };
                assert!(shifted.verify(&old, &newer).is_err());
            }
            let wrong_old = TreeHead {
                size: m,
                root: leaf_hash(b"wrong"),
            };
            assert!(good.verify(&wrong_old, &head).is_err());
        }
    }
}

#[test]
fn us4_vacuous_and_malformed_proofs_are_refused() {
    let leaves = leaves_64();
    let tree = tree_of(&leaves);
    let empty = TreeHead {
        size: 0,
        root: empty_root(),
    };
    let into_empty = InclusionProof {
        index: 0,
        size: 0,
        path: vec![],
    };
    assert_eq!(
        into_empty.verify(&leaves[0], &empty),
        Err(ProofError::EmptyTree)
    );
    assert_eq!(
        Tree::new().inclusion_proof(0, 0),
        Err(ProofError::EmptyTree)
    );

    let head = tree.head_at(8).unwrap();
    let from_empty = ConsistencyProof {
        old_size: 0,
        new_size: 8,
        path: vec![],
    };
    assert_eq!(
        from_empty.verify(&empty, &head),
        Err(ProofError::EmptyOldTree)
    );
    assert_eq!(tree.consistency_proof(0, 8), Err(ProofError::EmptyOldTree));

    let backwards = ConsistencyProof {
        old_size: 8,
        new_size: 4,
        path: vec![],
    };
    assert_eq!(
        backwards.verify(&head, &tree.head_at(4).unwrap()),
        Err(ProofError::OldLargerThanNew { old: 8, new: 4 })
    );
    let same_with_path = ConsistencyProof {
        old_size: 8,
        new_size: 8,
        path: vec![head.root],
    };
    assert_eq!(
        same_with_path.verify(&head, &head),
        Err(ProofError::PathLength {
            expected: 0,
            found: 1
        })
    );
    let same = ConsistencyProof {
        old_size: 8,
        new_size: 8,
        path: vec![],
    };
    same.verify(&head, &head).unwrap();
    let other = TreeHead {
        size: 8,
        root: leaf_hash(b"fork"),
    };
    assert_eq!(
        same.verify(&head, &other),
        Err(ProofError::RootMismatch {
            expected: other.root,
            computed: head.root
        })
    );

    let proof = tree.inclusion_proof(3, 8).unwrap();
    assert_eq!(
        proof.verify(&leaves[3], &tree.head_at(9).unwrap()),
        Err(ProofError::SizeMismatch { proof: 8, head: 9 })
    );
    let wrong = TreeHead {
        size: 8,
        root: leaf_hash(b"wrong"),
    };
    assert_eq!(
        proof.verify(&leaves[3], &wrong),
        Err(ProofError::RootMismatch {
            expected: wrong.root,
            computed: head.root
        })
    );
    let too_long = InclusionProof {
        path: vec![leaves[0]; 64],
        ..proof.clone()
    };
    assert!(matches!(
        too_long.verify(&leaves[3], &head),
        Err(ProofError::PathLength { found: 64, .. })
    ));
    let huge = InclusionProof {
        index: 0,
        size: u64::MAX,
        path: vec![],
    };
    assert_eq!(
        huge.verify(
            &leaves[0],
            &TreeHead {
                size: u64::MAX,
                root: head.root
            }
        ),
        Err(ProofError::TooLarge { size: u64::MAX })
    );
}

#[test]
fn us4_verify_record_hashes_the_bytes_itself() {
    let a: Record = Record::Genesis(Genesis {
        ledger: LedgerId::new("example.com/a").unwrap(),
    });
    let b: Record = Record::Genesis(Genesis {
        ledger: LedgerId::new("example.com/b").unwrap(),
    });
    let tree = tree_of(&[a.leaf_hash(), b.leaf_hash()]);
    let proof = tree.inclusion_proof(0, 2).unwrap();
    assert_eq!(
        proof.verify_record::<2>(&tree.head(), &a.to_leaf_bytes()),
        Ok(a.clone())
    );
    assert!(matches!(
        proof.verify_record::<2>(&tree.head(), &b.to_leaf_bytes()),
        Err(ProofError::RootMismatch { .. })
    ));
    // Included bytes that are not a record of this scale are refused by the decoder.
    assert!(matches!(
        proof.verify_record::<6>(&tree.head(), &a.to_leaf_bytes()),
        Err(ProofError::Decode(_))
    ));
}

#[test]
fn us4_head_at_every_historical_size() {
    let leaves = leaves_64();
    let tree = tree_of(&leaves);
    for k in 0..=64 {
        assert_eq!(tree.head_at(k as u64).unwrap().root, root_of(&leaves[..k]));
    }
}

// ── US5: sealed balances, proven and named ────────────────────────────────────

fn handle(index: u32) -> AccountHandle {
    AccountHandle::new(index).unwrap()
}

fn label(text: &str) -> Label {
    Label::new(text).unwrap()
}

fn debit(minor: i128) -> Balance {
    Balance::ZERO
        .post(Side::Debit, Amount::from_minor(minor).unwrap())
        .unwrap()
}

fn seal_with(accounts: u32, trees: &[&BalanceTree], through: u64) -> Seal {
    Seal::new(SealParts {
        period: label("2026-03"),
        start: date!(2026 - 03 - 01),
        end: date!(2026 - 03 - 31),
        through,
        accounts,
        trees: trees.iter().map(|tree| tree.head().unwrap()).collect(),
    })
    .unwrap()
}

#[test]
// PoC: qe2_assemble_masks_a_shrunken_registry_as_not_yet_registered
fn us5_sparse_trees_equal_explicit_trees_and_prove_every_handle() {
    let mut tree = BalanceTree::new(EUR, Layer::Settled, 1000);
    let used: Vec<u32> = (0..10).map(|i| i * 97 + 7).collect();
    for (i, index) in used.iter().enumerate() {
        tree.set(handle(*index), debit(i as i128 + 1)).unwrap();
    }
    let explicit: Vec<Hash> = (0..1000u32)
        .map(|index| {
            let balance = used
                .iter()
                .position(|u| *u == index)
                .map_or(Balance::ZERO, |i| debit(i as i128 + 1));
            leaf_hash(&doubleentry::log::balance_leaf_bytes(&balance))
        })
        .collect();
    assert_eq!(tree.root(), root_of(&explicit));
    let seal = seal_with(1000, &[&tree], 10);
    for index in 0..1000 {
        let proof = tree.prove(handle(index)).unwrap();
        proof.verify(&seal).unwrap();
        assert_eq!(proof.balance.is_zero(), !used.contains(&index));
    }
    assert_eq!(
        tree.totals().unwrap().debit().to_minor(),
        (1..=10).sum::<i128>()
    );

    // Index ≠ handle, size ≠ accounts, and a missing tree are refused.
    let proof = tree.prove(handle(7)).unwrap();
    let claimed = doubleentry::log::BalanceProof {
        handle: handle(8),
        ..proof.clone()
    };
    assert_eq!(
        claimed.verify(&seal),
        Err(ProofError::HandleMismatch {
            index: 7,
            handle: 8
        })
    );
    let resized = doubleentry::log::BalanceProof {
        path: InclusionProof {
            size: 999,
            ..proof.path.clone()
        },
        ..proof.clone()
    };
    assert_eq!(
        resized.verify(&seal),
        Err(ProofError::AccountCountMismatch {
            tree_size: 999,
            accounts: 1000
        })
    );
    let pending = doubleentry::log::BalanceProof {
        layer: Layer::Pending,
        ..proof.clone()
    };
    assert_eq!(
        pending.verify(&seal),
        Err(ProofError::NoSuchTree {
            currency: EUR,
            layer: Layer::Pending
        })
    );
    let inflated = doubleentry::log::BalanceProof {
        balance: debit(2),
        ..proof
    };
    assert!(matches!(
        inflated.verify(&seal),
        Err(ProofError::RootMismatch { .. })
    ));
    assert_eq!(
        tree.prove(handle(1000)).map(|_| ()),
        Err(ProofError::IndexOutOfRange {
            index: 1000,
            size: 1000
        })
    );
}

#[test]
fn us5_seal_trees_are_built_per_currency_and_layer() {
    let trees = build_seal_trees(
        4,
        [
            (handle(1), USD, Layer::Settled, debit(5)),
            (handle(0), EUR, Layer::Pending, debit(1)),
            (handle(2), EUR, Layer::Settled, debit(2)),
            (handle(3), EUR, Layer::Settled, Balance::ZERO),
        ],
    )
    .unwrap();
    let keys: Vec<_> = trees.iter().map(|t| (t.currency(), t.layer())).collect();
    assert_eq!(
        keys,
        [
            (EUR, Layer::Settled),
            (EUR, Layer::Pending),
            (USD, Layer::Settled)
        ]
    );
    assert!(trees.iter().all(|t| t.accounts() == 4));
    assert_eq!(
        build_seal_trees(4, [(handle(4), EUR, Layer::Settled, debit(1))]).map(|_| ()),
        Err(ProofError::IndexOutOfRange { index: 4, size: 4 })
    );
    assert_eq!(
        build_seal_trees(
            4,
            [
                (handle(1), EUR, Layer::Settled, debit(1)),
                (handle(1), EUR, Layer::Settled, debit(2))
            ]
        )
        .map(|_| ()),
        Err(ProofError::DuplicateBalance { handle: 1 })
    );
}

fn register(index: u32, path: &str, kind: AccountKind) -> Record {
    Record::Account(AccountEvent {
        action: AccountAction::Register(AccountRegistration {
            handle: handle(index),
            path: path.parse().unwrap(),
            kind,
            class: None,
            open_from: date!(2026 - 01 - 01),
            open_to: None,
            limit: BalanceLimit::Unlimited,
        }),
        effective: date!(2026 - 01 - 01),
        actor: label("setup"),
    })
}

struct Books {
    tree: Tree,
    leaves: Vec<Vec<u8>>,
    balances: Vec<BalanceTree>,
}

/// genesis, two registers, EUR, period, one entry, seal, then a late
/// register and a change record.
fn books() -> Books {
    let cash = debit(119_000);
    let sales = Balance::ZERO
        .post(Side::Credit, Amount::from_minor(119_000).unwrap())
        .unwrap();
    let balances = build_seal_trees(
        2,
        [
            (handle(0), EUR, Layer::Settled, cash),
            (handle(1), EUR, Layer::Settled, sales),
        ],
    )
    .unwrap();
    let records: Vec<Record> = vec![
        Record::Genesis(Genesis {
            ledger: LedgerId::new("example.com/acme").unwrap(),
        }),
        register(0, "Assets:Cash", AccountKind::Asset),
        register(1, "Income:Sales", AccountKind::Income),
        Record::Currency(CurrencyDeclaration {
            code: EUR,
            exponent: 2,
            effective: date!(2026 - 01 - 01),
            actor: label("setup"),
        }),
        Record::Period(
            PeriodDefinition::new(
                label("2026-03"),
                date!(2026 - 03 - 01),
                date!(2026 - 03 - 31),
            )
            .unwrap(),
        ),
        Record::Entry(
            EntryRecord::new(EntryParts {
                key: IdempotencyKey::new("invoice-2026-0001").unwrap(),
                booking_date: date!(2026 - 03 - 15),
                value_date: date!(2026 - 03 - 15),
                captured_at: None,
                kind: None,
                memo: None,
                documents: vec![],
                postings: [(0, Side::Debit), (1, Side::Credit)]
                    .into_iter()
                    .map(|(index, side)| PostingRecord {
                        handle: handle(index),
                        side,
                        amount: Amount::from_minor(119_000).unwrap(),
                        currency: EUR,
                        layer: Layer::Settled,
                        dimensions: doubleentry::Dimensions::new(),
                    })
                    .collect(),
                reverses: None,
                hold: None,
                rates: vec![],
            })
            .unwrap(),
        ),
        Record::Seal(seal_with(2, &balances.iter().collect::<Vec<_>>(), 5)),
        register(2, "Expenses:Late", AccountKind::Expense),
        Record::Account(AccountEvent {
            action: AccountAction::Change {
                handle: handle(0),
                change: AccountChange::OpenTo(None),
            },
            effective: date!(2026 - 04 - 01),
            actor: label("setup"),
        }),
    ];
    let leaves: Vec<Vec<u8>> = records.iter().map(Record::to_leaf_bytes).collect();
    let tree = tree_of(
        &leaves
            .iter()
            .map(|leaf| leaf_hash(leaf))
            .collect::<Vec<_>>(),
    );
    Books {
        tree,
        leaves,
        balances,
    }
}

const SEAL: u64 = 6;

fn sealed_proof(books: &Books, account: u64, index: u32, layer: Layer) -> SealedBalanceProof {
    let size = books.tree.size();
    SealedBalanceProof {
        seal_leaf: books.leaves[SEAL as usize].clone(),
        seal_proof: books.tree.inclusion_proof(SEAL, size).unwrap(),
        account_leaf: books.leaves[account as usize].clone(),
        account_proof: books.tree.inclusion_proof(account, size).unwrap(),
        currency: EUR,
        layer,
        handle: handle(index),
        balance: match layer {
            Layer::Settled => Some(books.balances[0].prove(handle(index)).unwrap()),
            Layer::Pending => None,
        },
    }
}

#[test]
fn us5_a_sealed_balance_is_proven_and_named() {
    let books = books();
    let head = books.tree.head();
    for (account, index, path, kind, net) in [
        (1, 0, "Assets:Cash", AccountKind::Asset, 119_000),
        (2, 1, "Income:Sales", AccountKind::Income, -119_000),
    ] {
        let verified = sealed_proof(&books, account, index, Layer::Settled)
            .verify(&head)
            .unwrap();
        assert_eq!(verified.account.to_string(), path);
        assert_eq!(verified.kind, kind);
        assert_eq!(verified.period.as_str(), "2026-03");
        assert_eq!(
            (verified.start, verified.end),
            (date!(2026 - 03 - 01), date!(2026 - 03 - 31))
        );
        assert_eq!(verified.seal_position, SEAL);
        assert_eq!(verified.balance.net().to_minor(), net);
        assert_eq!(verified.balance.postings(), 1);
        // No EUR/Pending tree: the seal alone proves a zero balance.
        let zero = sealed_proof(&books, account, index, Layer::Pending)
            .verify(&head)
            .unwrap();
        assert_eq!(zero.balance, Balance::ZERO);
    }
}

#[test]
fn us5_sealed_balance_proofs_refuse_what_they_cannot_name() {
    let books = books();
    let head = books.tree.head();
    let size = books.tree.size();
    let good = sealed_proof(&books, 1, 0, Layer::Settled);

    // Another account's register under this handle.
    let swapped = SealedBalanceProof {
        account_leaf: books.leaves[2].clone(),
        account_proof: books.tree.inclusion_proof(2, size).unwrap(),
        ..good.clone()
    };
    assert_eq!(
        swapped.verify(&head),
        Err(ProofError::HandleMismatch {
            index: 1,
            handle: 0
        })
    );

    // A change record, or another kind, as the account leaf.
    let change = SealedBalanceProof {
        account_leaf: books.leaves[8].clone(),
        account_proof: books.tree.inclusion_proof(8, size).unwrap(),
        ..good.clone()
    };
    assert_eq!(change.verify(&head), Err(ProofError::NotARegistration));
    let currency = SealedBalanceProof {
        account_leaf: books.leaves[3].clone(),
        account_proof: books.tree.inclusion_proof(3, size).unwrap(),
        ..good.clone()
    };
    assert_eq!(
        currency.verify(&head),
        Err(ProofError::UnexpectedRecord {
            expected: RecordKind::Account,
            found: RecordKind::Currency
        })
    );

    // A register after the seal.
    let late = SealedBalanceProof {
        account_leaf: books.leaves[7].clone(),
        account_proof: books.tree.inclusion_proof(7, size).unwrap(),
        handle: handle(2),
        balance: None,
        layer: Layer::Pending,
        ..good.clone()
    };
    assert_eq!(
        late.verify(&head),
        Err(ProofError::IndexOutOfRange { index: 2, size: 2 })
    );

    // A seal leaf that is not a seal.
    let not_a_seal = SealedBalanceProof {
        seal_leaf: books.leaves[5].clone(),
        seal_proof: books.tree.inclusion_proof(5, size).unwrap(),
        ..good.clone()
    };
    assert_eq!(
        not_a_seal.verify(&head),
        Err(ProofError::UnexpectedRecord {
            expected: RecordKind::Seal,
            found: RecordKind::Entry
        })
    );

    // Either inclusion against a wrong head.
    let wrong = TreeHead {
        size,
        root: leaf_hash(b"wrong"),
    };
    assert!(matches!(
        good.verify(&wrong),
        Err(ProofError::RootMismatch { .. })
    ));
    let other_size = books.tree.head_at(size - 1).unwrap();
    assert!(matches!(
        good.verify(&other_size),
        Err(ProofError::SizeMismatch { .. })
    ));

    // The balance proof omitted although the seal has the tree, or for another handle.
    let omitted = SealedBalanceProof {
        balance: None,
        ..good.clone()
    };
    assert_eq!(omitted.verify(&head), Err(ProofError::BalanceProofMismatch));
    let mismatched = SealedBalanceProof {
        balance: Some(books.balances[0].prove(handle(1)).unwrap()),
        ..good.clone()
    };
    assert_eq!(
        mismatched.verify(&head),
        Err(ProofError::BalanceProofMismatch)
    );
    good.verify(&head).unwrap();
}

#[test]
fn us5_a_register_after_through_is_refused() {
    // A seal whose `through` precedes the register record of a handle it counts.
    let register0 = register(0, "Assets:Cash", AccountKind::Asset);
    let seal: Record = Record::Seal(seal_with(1, &[], 0));
    let leaves = [seal.to_leaf_bytes(), register0.to_leaf_bytes()];
    let tree = tree_of(
        &leaves
            .iter()
            .map(|leaf| leaf_hash(leaf))
            .collect::<Vec<_>>(),
    );
    let proof: SealedBalanceProof = SealedBalanceProof {
        seal_leaf: leaves[0].clone(),
        seal_proof: tree.inclusion_proof(0, 2).unwrap(),
        account_leaf: leaves[1].clone(),
        account_proof: tree.inclusion_proof(1, 2).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
        handle: handle(0),
        balance: None,
    };
    assert_eq!(
        proof.verify(&tree.head()),
        Err(ProofError::AccountAfterSeal {
            position: 1,
            through: 0
        })
    );
}

#[test]
fn us5_seal_order_is_checked() {
    let base = |start, end, through, accounts, total: i128| -> Seal {
        let mut tree = BalanceTree::new(EUR, Layer::Settled, accounts);
        tree.set(handle(0), debit(total)).unwrap();
        Seal::new(SealParts {
            period: label("p"),
            start,
            end,
            through,
            accounts,
            trees: vec![tree.head().unwrap()],
        })
        .unwrap()
    };
    let march = base(date!(2026 - 03 - 01), date!(2026 - 03 - 31), 10, 2, 5);
    let april = base(date!(2026 - 04 - 01), date!(2026 - 04 - 30), 20, 3, 7);
    april.check_follows(&march).unwrap();
    use doubleentry::record::SealOrderError as E;
    assert_eq!(
        base(date!(2026 - 04 - 02), date!(2026 - 04 - 30), 20, 3, 7).check_follows(&march),
        Err(E::NotContiguous)
    );
    assert_eq!(
        base(date!(2026 - 04 - 01), date!(2026 - 04 - 30), 10, 3, 7).check_follows(&march),
        Err(E::ThroughNotIncreasing)
    );
    assert_eq!(
        base(date!(2026 - 04 - 01), date!(2026 - 04 - 30), 20, 1, 7).check_follows(&march),
        Err(E::AccountsDecreased)
    );
    assert_eq!(
        base(date!(2026 - 04 - 01), date!(2026 - 04 - 30), 20, 3, 4).check_follows(&march),
        Err(E::TotalDecreased {
            currency: EUR,
            layer: Layer::Settled
        })
    );
    let empty = Seal::new(SealParts {
        period: label("p"),
        start: date!(2026 - 04 - 01),
        end: date!(2026 - 04 - 30),
        through: 20,
        accounts: 3,
        trees: vec![],
    })
    .unwrap();
    assert_eq!(
        empty.check_follows(&march),
        Err(E::TreeMissing {
            currency: EUR,
            layer: Layer::Settled
        })
    );
}
