//! Checkpoints others can cosign and auditors can check, on real ledgers:
//! signing, witnessing, auditing, record and seal bundles, sealed balances
//! and tiles, the verifier property test and the exhaustive bundle
//! deformation. Byte compatibility with Go is `tests/transparency_interop.rs`.
#![cfg(feature = "witness")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::store::{MemoryStore, Tree};
use doubleentry::transparency::{
    AddCheckpoint, Checkpoint, Error as TError, ForkEvidence, KeyName, LogKey, Policy, ProofBundle,
    Proven, SignedCheckpoint, TileId, Update, Verified, Verifier, Vkey, WitnessError, WitnessReply,
};
use doubleentry::{
    AccountKind, BalanceQuery, Currency, Entry, Error, Journal, Layer, Ledger, LedgerId, MemoSalt,
    TreeHead, amount,
    currency::{EUR, USD},
    date,
    error::QueryError,
    log::{ConsistencyProof, Hash, empty_root, leaf_hash},
    record::Record,
};
use sha2::{Digest, Sha256};

fn acme() -> LedgerId {
    LedgerId::new("example.com/ledgers/acme").unwrap()
}

fn key() -> LogKey {
    LogKey::from_seed(&acme(), [7; 32])
}

fn origin() -> KeyName {
    KeyName::from(&acme())
}

/// A ledger with EUR, two accounts and 2026's months, and `entries` sales
/// in January.
fn books(entries: u32) -> Journal {
    let mut books = Journal::new(acme())
        .with_salt_source(|| MemoSalt([1; 16]))
        .as_actor("setup");
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books.define_months(2026, 1..=12).unwrap();
    for n in 0..entries {
        books
            .record(
                Entry::new(format!("sale-{n}"), date!("2026-01-15"))
                    .debit("Assets:Cash", amount!("10.00"), EUR)
                    .credit("Income:Sales", amount!("10.00"), EUR),
            )
            .unwrap();
    }
    books
}

/// A checkpoint of `head` signed by `key`, whatever the head.
fn signed(key: &LogKey, head: TreeHead) -> SignedCheckpoint {
    key.sign(&Checkpoint::new(origin(), head)).unwrap()
}

fn other_root(head: TreeHead) -> TreeHead {
    TreeHead {
        size: head.size,
        root: leaf_hash(b"another history"),
    }
}

// ── US1: sign the ledger's head ─────────────────────────────────────────────

#[test]
fn us1_checkpoint_is_the_head_signed_by_the_ledger_key() {
    let books = books(0);
    let checkpoint = books.checkpoint(&key()).unwrap();
    assert_eq!(checkpoint.checkpoint().head(), books.head());
    assert_eq!(checkpoint.checkpoint().origin(), &origin());
    assert!(checkpoint.checkpoint().extensions().is_empty());
    let text = String::from_utf8(checkpoint.to_bytes()).unwrap();
    assert!(text.starts_with(&format!(
        "example.com/ledgers/acme\n{}\n",
        books.head().size
    )));
    assert!(text.contains("\n\n\u{2014} example.com/ledgers/acme "));
    assert_eq!(key().vkey().key_id(), [0xc3, 0xb6, 0xd9, 0x94]);
    assert!(matches!(
        &checkpoint.verify(&[key().vkey()]).unwrap()[..],
        [Verified::Log { .. }]
    ));
}

#[test]
fn us1_a_key_of_another_ledger_cannot_sign() {
    let other = LogKey::from_seed(&LedgerId::new("example.com/other").unwrap(), [7; 32]);
    assert!(matches!(
        books(0).checkpoint(&other),
        Err(Error::Transparency(TError::KeyNameMismatch { .. }))
    ));
}

#[test]
fn us1_size_zero_needs_the_empty_root() {
    let empty = signed(
        &key(),
        TreeHead {
            size: 0,
            root: empty_root(),
        },
    );
    empty.verify(&[key().vkey()]).unwrap();
    let wrong = signed(
        &key(),
        TreeHead {
            size: 0,
            root: leaf_hash(b"x"),
        },
    );
    assert_eq!(wrong.verify(&[key().vkey()]), Err(TError::EmptyTreeRoot));
}

#[test]
fn us1_verification_ignores_unknown_keys_and_refuses_failing_known_ones() {
    let checkpoint = books(2).checkpoint(&key()).unwrap();
    let stranger = LogKey::from_seed(&acme(), [8; 32]).vkey();
    assert_eq!(
        checkpoint.verify(std::slice::from_ref(&stranger)),
        Err(TError::NoTrustedSignature)
    );
    assert_eq!(
        checkpoint.verify(&[stranger, key().vkey()]).unwrap().len(),
        1
    );
    // One flipped signature bit: the known key fails, the note is refused.
    let mut bytes = checkpoint.to_bytes();
    let at = bytes.len() - 6;
    bytes[at] = if bytes[at] == b'A' { b'B' } else { b'A' };
    if let Ok(tampered) = SignedCheckpoint::parse(&bytes) {
        assert!(matches!(
            tampered.verify(&[key().vkey()]),
            Err(TError::BadSignature { .. })
        ));
    }
}

#[tokio::test]
async fn us1_both_facades_sign_the_same_bytes() {
    let journal = fifty();
    let ledger = mirror(&journal).await;
    assert_eq!(
        ledger.checkpoint(&key()).await.unwrap().to_bytes(),
        journal.checkpoint(&key()).unwrap().to_bytes()
    );
}

// ── US2: collect cosignatures ───────────────────────────────────────────────

#[test]
fn us2_requests_refuse_impossible_shapes() {
    let books = books(12);
    let checkpoint = books.checkpoint(&key()).unwrap();
    let size = checkpoint.checkpoint().head().size;
    let proof = books.prove_consistency(7, size).unwrap();
    let body = AddCheckpoint::new(&checkpoint, 7, Some(&proof)).unwrap();
    assert!(body.to_bytes().starts_with(b"old 7\n"));
    assert_eq!(body.proof(), proof.path.as_slice());
    assert_eq!(
        AddCheckpoint::new(&checkpoint, size + 1, None),
        Err(TError::ProofShape)
    );
    assert_eq!(
        AddCheckpoint::new(&checkpoint, 7, None),
        Err(TError::ProofShape)
    );
    assert_eq!(
        AddCheckpoint::new(&checkpoint, 0, Some(&proof)),
        Err(TError::ProofShape)
    );
    assert_eq!(
        AddCheckpoint::new(&checkpoint, size, Some(&proof)),
        Err(TError::ProofShape)
    );
    let other = books.prove_consistency(6, size).unwrap();
    assert_eq!(
        AddCheckpoint::new(&checkpoint, 7, Some(&other)),
        Err(TError::ProofShape)
    );
    let long = ConsistencyProof {
        old_size: 7,
        new_size: size,
        path: vec![Hash::default(); 64],
    };
    assert_eq!(
        AddCheckpoint::new(&checkpoint, 7, Some(&long)),
        Err(TError::TooManyProofLines)
    );
    let mut body = b"old 7\n".to_vec();
    for _ in 0..64 {
        body.extend_from_slice(b"47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\n");
    }
    body.push(b'\n');
    body.extend_from_slice(&checkpoint.to_bytes());
    assert_eq!(AddCheckpoint::parse(&body), Err(TError::TooManyProofLines));
}

#[cfg(feature = "testing")]
mod witnessing {
    use super::*;
    use doubleentry::testing::TestWitness;
    use doubleentry::transparency::{Next, WitnessSession};

    fn witness() -> TestWitness {
        TestWitness::new("witness.example/w1", [9; 32])
            .unwrap()
            .log(key().vkey())
    }

    /// Runs a session to the end, fetching proofs from `books`.
    fn run(
        session: &mut WitnessSession,
        witness: &mut TestWitness,
        books: &Journal,
    ) -> Result<(doubleentry::transparency::Cosignature, u64, u32), TError> {
        let mut requests = 0;
        loop {
            let proof = session
                .needed_proof()
                .map(|(old, new)| books.prove_consistency(old, new).unwrap());
            let body = session.request(proof.as_ref())?;
            requests += 1;
            let reply = witness.post(&body);
            if let Next::Cosigned {
                cosignature,
                witness_size,
            } = session.receive(reply.status, reply.content_type, &reply.body)?
            {
                return Ok((cosignature, witness_size, requests));
            }
        }
    }

    #[test]
    fn us2_a_session_converges_from_any_old_size() {
        let mut books = books(3);
        let mut witness = witness();
        let first = books.checkpoint(&key()).unwrap();
        let mut session = WitnessSession::new(first.clone(), witness.vkey(), 0).unwrap();
        let (cosignature, size, requests) = run(&mut session, &mut witness, &books).unwrap();
        assert_eq!((size, requests), (first.checkpoint().head().size, 1));
        let cosigned = first.with_cosignature(&cosignature).unwrap();
        assert_eq!(
            cosigned
                .verify(&[key().vkey(), witness.vkey()])
                .unwrap()
                .len(),
            2
        );

        for n in 0..5 {
            books
                .record(
                    Entry::new(format!("more-{n}"), date!("2026-02-01"))
                        .debit("Assets:Cash", amount!("1.00"), EUR)
                        .credit("Income:Sales", amount!("1.00"), EUR),
                )
                .unwrap();
        }
        let next = books.checkpoint(&key()).unwrap();
        // A caller that lost the witness's size starts from 0: 409, then 200.
        let mut session = WitnessSession::new(next.clone(), witness.vkey(), 0).unwrap();
        let (cosignature, size, requests) = run(&mut session, &mut witness, &books).unwrap();
        assert_eq!((size, requests), (next.checkpoint().head().size, 2));
        assert_eq!(cosignature.timestamp(), 2);
        // The same head again: no proof, cosigned with a later timestamp,
        // which replaces the earlier line.
        let mut session = WitnessSession::new(next.clone(), witness.vkey(), size).unwrap();
        assert_eq!(session.needed_proof(), None);
        let (again, _, _) = run(&mut session, &mut witness, &books).unwrap();
        let twice = next
            .with_cosignature(&cosignature)
            .unwrap()
            .with_cosignature(&again)
            .unwrap();
        assert_eq!(twice.signatures().len(), 2);
        assert_eq!(twice.signatures()[1].signature()[..8], 3u64.to_be_bytes());
        assert_eq!(
            session.request(None),
            Err(TError::Witness(WitnessError::SessionFinished))
        );
    }

    #[test]
    fn us2_the_test_witness_answers_like_a_c2sp_witness() {
        let books = books(3);
        let checkpoint = books.checkpoint(&key()).unwrap();
        let body = |old, proof: Option<&ConsistencyProof>| {
            AddCheckpoint::new(&checkpoint, old, proof)
                .unwrap()
                .to_bytes()
        };
        let mut unknown = TestWitness::new("witness.example/w1", [9; 32]).unwrap();
        assert_eq!(unknown.post(&body(0, None)).status, 404);
        let mut stranger = TestWitness::new("witness.example/w1", [9; 32])
            .unwrap()
            .log(LogKey::from_seed(&acme(), [8; 32]).vkey());
        assert_eq!(stranger.post(&body(0, None)).status, 403);
        let mut witness = witness();
        assert_eq!(witness.post(b"old 1\nnot a request").status, 400);
        let stale = witness.post(&body(
            3,
            Some(
                &books
                    .prove_consistency(3, checkpoint.checkpoint().head().size)
                    .unwrap(),
            ),
        ));
        assert_eq!(
            (stale.status, stale.content_type, stale.body.as_slice()),
            (409, Some("text/x.tlog.size"), b"0\n".as_slice())
        );
        assert_eq!(witness.post(&body(0, None)).status, 200);
        assert_eq!(
            witness.head(&origin()),
            Some(checkpoint.checkpoint().head())
        );
        let fork = signed(&key(), other_root(checkpoint.checkpoint().head()));
        let fork_body = AddCheckpoint::new(&fork, fork.checkpoint().head().size, None).unwrap();
        assert_eq!(witness.post(&fork_body.to_bytes()).status, 422);
    }

    #[test]
    fn us2_conflicts_end_the_session() {
        let books = books(12);
        let checkpoint = books.checkpoint(&key()).unwrap();
        let size = checkpoint.checkpoint().head().size;
        let w = witness().vkey();
        let mut ahead = WitnessSession::new(checkpoint.clone(), w.clone(), 0).unwrap();
        ahead.request(None).unwrap();
        let too_big = format!("{}\n", size + 1);
        assert_eq!(
            ahead.receive(409, Some("text/x.tlog.size"), too_big.as_bytes()),
            Err(TError::Witness(WitnessError::WitnessAhead {
                witness: size + 1,
                checkpoint: size
            }))
        );
        let mut session = WitnessSession::new(checkpoint.clone(), w.clone(), 0).unwrap();
        for attempt in 0..3 {
            let proof = session
                .needed_proof()
                .map(|(old, new)| books.prove_consistency(old, new).unwrap());
            session.request(proof.as_ref()).unwrap();
            let reply = session.receive(409, Some("text/x.tlog.size"), b"5\n");
            if attempt < 2 {
                assert_eq!(reply, Ok(Next::Retry));
                assert_eq!(session.needed_proof(), Some((5, size)));
            } else {
                assert_eq!(
                    reply,
                    Err(TError::Witness(WitnessError::TooManyConflicts {
                        sizes: vec![5, 5, 5]
                    }))
                );
            }
        }
        assert!(matches!(
            WitnessSession::new(checkpoint.clone(), key().vkey(), 0),
            Err(TError::WrongKeyType { .. })
        ));
        assert_eq!(
            WitnessSession::new(checkpoint, w, size + 1).err(),
            Some(TError::ProofShape)
        );
    }
}

#[test]
fn us2_replies_ignore_unknown_lines_and_name_the_missing_witness() {
    let checkpoint = books(1).checkpoint(&key()).unwrap();
    let witness: Vkey = "witness.example/w1+24b5d9fe+BP0XJDhaoMdbZPt4zWAvodmR/ev3axPFjtcC6sg16fYY"
        .parse()
        .unwrap();
    let unknown = "\u{2014} other.example/w AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n";
    assert_eq!(
        WitnessReply::read(200, None, unknown.as_bytes(), &checkpoint, &witness),
        Err(TError::Witness(WitnessError::NoCosignature {
            name: witness.name().clone()
        }))
    );
    assert_eq!(
        WitnessReply::read(200, None, b"", &checkpoint, &witness),
        Err(TError::Witness(WitnessError::MalformedReply))
    );
    assert_eq!(
        WitnessReply::read(200, None, b"garbage\n", &checkpoint, &witness),
        Err(TError::Witness(WitnessError::MalformedReply))
    );
}

// ── US3: an auditor follows a ledger ────────────────────────────────────────

#[test]
// PoC: control_rollback_fork_and_beyond_head_refused (kept: rollback and fork refused)
fn us3_verifier_moves_only_forward_through_consistency() {
    let mut books = books(5);
    let at7 = books.checkpoint(&key()).unwrap();
    let mut verifier = Verifier::new().log(key().vkey()).unwrap();
    let proof = books
        .prove_consistency(1, at7.checkpoint().head().size)
        .unwrap();
    assert_eq!(verifier.update(&at7, Some(&proof)), Err(TError::ProofShape));
    assert_eq!(verifier.head(&origin()), None);
    assert_eq!(verifier.update(&at7, None), Ok(Update::First));
    assert_eq!(verifier.update(&at7, None), Ok(Update::Unchanged));
    for n in 0..6 {
        books
            .record(
                Entry::new(format!("next-{n}"), date!("2026-02-01"))
                    .debit("Assets:Cash", amount!("1.00"), EUR)
                    .credit("Income:Sales", amount!("1.00"), EUR),
            )
            .unwrap();
    }
    let at13 = books.checkpoint(&key()).unwrap();
    let (old, new) = (at7.checkpoint().head(), at13.checkpoint().head());
    let good = books.prove_consistency(old.size, new.size).unwrap();
    let mut bad = good.clone();
    bad.path[0] = leaf_hash(b"bad");
    // PoC: A3-T4 — a corrupted proof proves nothing: no fork evidence.
    assert!(matches!(
        verifier.update(&at13, Some(&bad)),
        Err(TError::Proof(_))
    ));
    assert_eq!(verifier.update(&at13, None), Err(TError::ProofShape));
    assert_eq!(verifier.head(&origin()), Some(old));
    assert_eq!(
        verifier.update(&at13, Some(&good)),
        Ok(Update::Advanced { from: old })
    );
    assert_eq!(
        verifier.update(&at7, None),
        Err(TError::Rollback {
            trusted: new,
            offered: old
        })
    );
    let fork = signed(&key(), other_root(new));
    let Err(TError::Fork(evidence)) = verifier.update(&fork, None) else {
        panic!("two roots at one size are a fork");
    };
    assert_eq!(evidence.trusted.as_ref(), Some(&at13));
    assert_eq!(evidence.offered, fork);
    assert_eq!(verifier.heads().collect::<Vec<_>>(), [(&origin(), new)]);

    // Persisted and restored: the pin is trusted, its note unknown.
    let mut restored = Verifier::new()
        .log(key().vkey())
        .unwrap()
        .pin(origin(), old);
    assert_eq!(
        restored.update(&at13, Some(&good)),
        Ok(Update::Advanced { from: old })
    );
    let mut pinned = Verifier::new()
        .log(key().vkey())
        .unwrap()
        .pin(origin(), new);
    let Err(TError::Fork(evidence)) = pinned.update(&fork, None) else {
        panic!("fork");
    };
    assert_eq!(evidence.trusted, None);
}

/// The fields of fork evidence, for comparison.
struct ForkEvidenceView;

impl ForkEvidenceView {
    fn of(
        trusted_head: TreeHead,
        trusted: Option<&SignedCheckpoint>,
        offered: &SignedCheckpoint,
        proof: Option<&ConsistencyProof>,
    ) -> ForkEvidence {
        let mut verifier = Verifier::new().log(key().vkey()).unwrap();
        if let Some(trusted) = trusted {
            verifier.update(trusted, None).unwrap();
        } else {
            verifier = verifier.pin(origin(), trusted_head);
        }
        match verifier.update(offered, proof) {
            Err(TError::Inconsistent(evidence) | TError::Fork(evidence)) => *evidence,
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn us3_unknown_origins_wrong_keys_and_policies_are_refused() {
    let books = books(3);
    let checkpoint = books.checkpoint(&key()).unwrap();
    let mut nobody = Verifier::new();
    assert_eq!(
        nobody.update(&checkpoint, None),
        Err(TError::UnknownOrigin(origin()))
    );
    let mut wrong = Verifier::new()
        .log(LogKey::from_seed(&acme(), [8; 32]).vkey())
        .unwrap();
    assert_eq!(
        wrong.update(&checkpoint, None),
        Err(TError::NoTrustedSignature)
    );
    let w1: Vkey = "witness.example/w1+24b5d9fe+BP0XJDhaoMdbZPt4zWAvodmR/ev3axPFjtcC6sg16fYY"
        .parse()
        .unwrap();
    let policy = Policy::k_of_n(key().vkey(), 1, [("w1".to_owned(), w1)]).unwrap();
    let mut audited = Verifier::from_policy(policy);
    assert!(matches!(
        audited.update(&checkpoint, None),
        Err(TError::QuorumNotMet { .. })
    ));
    assert_eq!(audited.head(&origin()), None);
    let mut size0 = Verifier::new().log(key().vkey()).unwrap();
    let empty = signed(
        &key(),
        TreeHead {
            size: 0,
            root: empty_root(),
        },
    );
    assert_eq!(size0.update(&empty, None), Ok(Update::First));
    assert_eq!(
        size0.update(&checkpoint, None),
        Ok(Update::Advanced {
            from: empty.checkpoint().head()
        })
    );
}

/// RFC 9162 §2.1.4.2, written from the RFC for this test only: the
/// independent check every accepted step is compared with.
fn rfc9162_consistent(m: u64, n: u64, proof: &[Hash], first: &Hash, second: &Hash) -> bool {
    let node = |l: &[u8; 32], r: &[u8; 32]| -> [u8; 32] {
        Sha256::new()
            .chain_update([1u8])
            .chain_update(l)
            .chain_update(r)
            .finalize()
            .into()
    };
    if m == 0 {
        return proof.is_empty();
    }
    if m == n {
        return proof.is_empty() && first == second;
    }
    if m > n || proof.is_empty() {
        return false;
    }
    let mut path: Vec<[u8; 32]> = proof.iter().map(|h| *h.as_bytes()).collect();
    if m.is_power_of_two() {
        path.insert(0, *first.as_bytes());
    }
    let (mut fn_, mut sn) = (m - 1, n - 1);
    while fn_ & 1 == 1 {
        fn_ >>= 1;
        sn >>= 1;
    }
    let (mut fr, mut sr) = (path[0], path[0]);
    for c in &path[1..] {
        if sn == 0 {
            return false;
        }
        if fn_ & 1 == 1 || fn_ == sn {
            fr = node(c, &fr);
            sr = node(c, &sr);
            if fn_ & 1 == 0 {
                while fn_ & 1 == 0 && fn_ != 0 {
                    fn_ >>= 1;
                    sn >>= 1;
                }
            }
        } else {
            sr = node(&sr, c);
        }
        fn_ >>= 1;
        sn >>= 1;
    }
    sn == 0 && &fr == first.as_bytes() && &sr == second.as_bytes()
}

mod property {
    use proptest::prelude::*;

    use super::*;

    const MAX: u64 = 24;

    /// Every checkpoint and proof the sequences draw from, signed once.
    struct Pool {
        honest: Tree,
        forked: Tree,
        /// Honest heads 0..=MAX, signed by the ledger key.
        good: Vec<SignedCheckpoint>,
        /// Heads of another history 1..=MAX, signed by the ledger key.
        fork: Vec<SignedCheckpoint>,
        /// Honest heads signed by another key.
        stranger: Vec<SignedCheckpoint>,
        /// A checkpoint of another log.
        foreign: SignedCheckpoint,
    }

    fn pool() -> &'static Pool {
        static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| {
            let mut honest = Tree::new();
            let mut forked = Tree::new();
            for i in 0..MAX {
                honest
                    .push(leaf_hash(format!("leaf-{i}").as_bytes()))
                    .unwrap();
                forked
                    .push(leaf_hash(format!("fork-{i}").as_bytes()))
                    .unwrap();
            }
            let stranger_key = LogKey::from_seed(&acme(), [8; 32]);
            let other = LedgerId::new("other.example/log").unwrap();
            let other_key = LogKey::from_seed(&other, [7; 32]);
            Pool {
                good: (0..=MAX)
                    .map(|s| signed(&key(), honest.head_at(s).unwrap()))
                    .collect(),
                fork: (1..=MAX)
                    .map(|s| signed(&key(), forked.head_at(s).unwrap()))
                    .collect(),
                stranger: (0..=MAX)
                    .map(|s| signed(&stranger_key, honest.head_at(s).unwrap()))
                    .collect(),
                foreign: other_key
                    .sign(&Checkpoint::new(
                        KeyName::from(&other),
                        honest.head_at(3).unwrap(),
                    ))
                    .unwrap(),
                honest,
                forked,
            }
        })
    }

    /// One step: which checkpoint, which size, which proof.
    #[derive(Debug, Clone, Copy)]
    struct Step {
        checkpoint: u8,
        size: u64,
        proof: u8,
    }

    fn step() -> impl Strategy<Value = Step> {
        (0u8..10, 0..=MAX, 0u8..6).prop_map(|(checkpoint, size, proof)| Step {
            checkpoint,
            size,
            proof,
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 10_000, ..ProptestConfig::default() })]

        /// SC-005: across random sequences of honest growth, forks,
        /// rollbacks, wrong keys and other logs, the trusted head changes
        /// only on a step an independent RFC 9162 check accepts, and every
        /// refused step names its cause.
        #[test]
        fn verifier_moves_only_by_consistency(steps in prop::collection::vec(step(), 1..12)) {
            let pool = pool();
            let mut verifier = Verifier::new().log(key().vkey()).unwrap();
            let mut model: Option<TreeHead> = None;
            for step in steps {
                let size = step.size.max(1);
                let (checkpoint, kind) = match step.checkpoint {
                    0..=5 => (&pool.good[usize::try_from(step.size).unwrap()], "good"),
                    6 | 7 => (&pool.fork[usize::try_from(size - 1).unwrap()], "fork"),
                    8 => (&pool.stranger[usize::try_from(step.size).unwrap()], "stranger"),
                    _ => (&pool.foreign, "foreign"),
                };
                let offered = checkpoint.checkpoint().head();
                let from = model.map_or(0, |head| head.size);
                let proof = match step.proof {
                    0 | 1 => (from > 0 && from < offered.size)
                        .then(|| pool.honest.consistency_proof(from, offered.size).unwrap()),
                    2 => (from > 0 && from < offered.size)
                        .then(|| pool.forked.consistency_proof(from, offered.size).unwrap()),
                    3 => (from > 0 && from < offered.size).then(|| {
                        let mut proof = pool.honest.consistency_proof(from, offered.size).unwrap();
                        proof.path[0] = leaf_hash(b"flipped");
                        proof
                    }),
                    4 => (offered.size > 1)
                        .then(|| pool.honest.consistency_proof(1, offered.size).unwrap()),
                    _ => None,
                };
                let result = verifier.update(checkpoint, proof.as_ref());
                match kind {
                    "stranger" => prop_assert_eq!(result.clone(), Err(TError::NoTrustedSignature)),
                    "foreign" => prop_assert!(matches!(result, Err(TError::UnknownOrigin(_)))),
                    _ => match model {
                        None if proof.is_none() => {
                            prop_assert_eq!(result.clone(), Ok(Update::First));
                            model = Some(offered);
                        }
                        None => prop_assert_eq!(result.clone(), Err(TError::ProofShape)),
                        Some(trusted) if offered.size == trusted.size => {
                            if offered.root == trusted.root {
                                prop_assert_eq!(result.clone(), Ok(Update::Unchanged));
                            } else {
                                prop_assert!(matches!(result, Err(TError::Fork(_))));
                            }
                        }
                        Some(trusted) if offered.size < trusted.size => {
                            prop_assert_eq!(result.clone(), Err(TError::Rollback { trusted, offered }));
                        }
                        Some(trusted) => {
                            let path = proof.as_ref().map(|p| p.path.clone()).unwrap_or_default();
                            let sizes_fit = proof.as_ref().is_none_or(|p| (p.old_size, p.new_size) == (trusted.size, offered.size));
                            if sizes_fit && rfc9162_consistent(trusted.size, offered.size, &path, &trusted.root, &offered.root) {
                                prop_assert_eq!(result.clone(), Ok(Update::Advanced { from: trusted }));
                                model = Some(offered);
                            } else {
                                // Evidence (A3-T4) only for a proof that
                                // reaches the offered root from a prefix
                                // root other than the trusted one; from a
                                // power-of-two size the proof omits that
                                // root, so it shows nothing.
                                let binds_other_prefix = sizes_fit
                                    && !trusted.size.is_power_of_two()
                                    && [&pool.honest, &pool.forked].iter().any(|tree| {
                                        let prefix = tree.head_at(trusted.size).unwrap().root;
                                        prefix != trusted.root
                                            && rfc9162_consistent(trusted.size, offered.size, &path, &prefix, &offered.root)
                                    });
                                if binds_other_prefix {
                                    prop_assert!(matches!(result, Err(TError::Inconsistent(_))), "{:?}", result);
                                } else {
                                    prop_assert!(
                                        matches!(result, Err(TError::Proof(_) | TError::ProofShape)),
                                        "{:?}",
                                        result
                                    );
                                }
                            }
                        }
                    },
                }
                prop_assert_eq!(verifier.head(&origin()), model);
            }
        }
    }
}

// ── US4: prove one entry or one seal ────────────────────────────────────────

/// A ledger with a sealed January.
fn sealed() -> Journal {
    let mut books = books(4);
    books.seal_period("2026-01").unwrap();
    books
        .record(
            Entry::new("february", date!("2026-02-03"))
                .debit("Assets:Cash", amount!("5.00"), EUR)
                .credit("Income:Sales", amount!("5.00"), EUR),
        )
        .unwrap();
    books
}

#[test]
fn us4_an_entry_bundle_names_the_entry() {
    let books = sealed();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let id = books.entry_by_key("sale-2").unwrap().id(&acme());
    let bundle = books.bundle_entry(id, &checkpoint).unwrap();
    let text = String::from_utf8(bundle.to_bytes()).unwrap();
    assert!(text.starts_with("c2sp.org/tlog-proof@v1\nextra "));
    assert!(text.ends_with(&String::from_utf8(checkpoint.to_bytes()).unwrap()));
    assert_eq!(ProofBundle::parse(&bundle.to_bytes()).unwrap(), bundle);
    let mut verifier = Verifier::new().log(key().vkey()).unwrap();
    verifier.update(&checkpoint, None).unwrap();
    let Proven::Record(proven) = verifier.verify(&bundle).unwrap() else {
        panic!("a record");
    };
    assert_eq!(proven.entry_id, Some(id));
    assert_eq!(proven.origin, origin());
    assert!(matches!(proven.record, Record::Entry(_)));
    assert_eq!(
        bundle.verify(&Policy::log_only(key().vkey())).unwrap(),
        Proven::Record(proven)
    );
}

#[test]
fn us4_a_seal_bundle_names_the_period() {
    let books = sealed();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let seal = books
        .leaves()
        .find(|leaf| leaf.bytes[0] == 8)
        .unwrap()
        .position;
    let bundle = books.bundle_record(seal, &checkpoint).unwrap();
    let Proven::Record(proven) = bundle.verify(&Policy::log_only(key().vkey())).unwrap() else {
        panic!("a record");
    };
    let Record::Seal(seal_record) = proven.record else {
        panic!("a seal");
    };
    let parts = seal_record.parts();
    assert_eq!(parts.period.as_str(), "2026-01");
    assert_eq!(
        (parts.start, parts.end),
        (date!("2026-01-01"), date!("2026-01-31"))
    );
    assert_eq!(parts.through, seal - 1);
    assert_eq!(parts.accounts, 2);
    assert_eq!(
        seal_record
            .tree(EUR, Layer::Settled)
            .unwrap()
            .totals
            .debit(),
        amount!("40.00")
    );
}

#[test]
fn us4_bundles_refuse_foreign_and_mismatched_checkpoints() {
    let books = sealed();
    let head = books.head();
    let id = books.entry_by_key("sale-1").unwrap().id(&acme());
    let other = LedgerId::new("example.com/other").unwrap();
    let foreign = LogKey::from_seed(&other, [7; 32])
        .sign(&Checkpoint::new(KeyName::from(&other), head))
        .unwrap();
    assert!(matches!(
        books.bundle_entry(id, &foreign),
        Err(Error::Transparency(TError::ForeignCheckpoint { .. }))
    ));
    let beyond = signed(
        &key(),
        TreeHead {
            size: head.size + 1,
            root: head.root,
        },
    );
    assert!(matches!(
        books.bundle_entry(id, &beyond),
        Err(Error::Transparency(TError::CheckpointMismatch))
    ));
    let rewritten = signed(&key(), other_root(head));
    assert!(matches!(
        books.bundle_entry(id, &rewritten),
        Err(Error::Transparency(TError::CheckpointMismatch))
    ));
    // At an older checkpoint: proven against that older head.
    // Genesis, a currency, two accounts and twelve months, then sale-0.
    let older = signed(&key(), books.head_at(17).unwrap());
    let early = books.entry_by_key("sale-0").unwrap().id(&acme());
    let bundle = books.bundle_entry(early, &older).unwrap();
    assert_eq!(bundle.checkpoint().checkpoint().head().size, 17);
    assert!(matches!(
        bundle.verify_inclusion().unwrap(),
        Proven::Record(_)
    ));
    let late = books.entry_by_key("february").unwrap().id(&acme());
    assert!(matches!(
        books.bundle_entry(late, &older),
        Err(Error::Query(QueryError::UnknownEntry { .. }))
    ));
    assert!(matches!(
        books.bundle_record(17, &older),
        Err(Error::Query(QueryError::PositionBeyondHead { .. }))
    ));
}

#[test]
fn us4_a_verifier_checks_bundles_only_at_its_trusted_head() {
    let mut books = sealed();
    let first = books.checkpoint(&key()).unwrap();
    let id = books.entry_by_key("sale-1").unwrap().id(&acme());
    let mut verifier = Verifier::new().log(key().vkey()).unwrap();
    let bundle = books.bundle_entry(id, &first).unwrap();
    assert!(matches!(
        verifier.verify(&bundle),
        Err(TError::NotAtTrustedHead { trusted: None, .. })
    ));
    verifier.update(&first, None).unwrap();
    verifier.verify(&bundle).unwrap();
    books.define_months(2027, 1..=1).unwrap();
    let next = books.checkpoint(&key()).unwrap();
    let newer = books.bundle_entry(id, &next).unwrap();
    assert_eq!(
        verifier.verify(&newer),
        Err(TError::NotAtTrustedHead {
            trusted: Some(first.checkpoint().head()),
            offered: next.checkpoint().head()
        })
    );
}

#[test]
fn us4_malformed_bundles_are_refused() {
    let books = sealed();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let bundle = books.bundle_record(3, &checkpoint).unwrap().to_bytes();
    let text = String::from_utf8(bundle.clone()).unwrap();
    let size = checkpoint.checkpoint().head().size;
    let beyond = text.replace("index 3\n", &format!("index {size}\n"));
    assert_eq!(
        ProofBundle::parse(beyond.as_bytes())
            .unwrap()
            .verify_inclusion(),
        Err(TError::ProofShape)
    );
    let mut trailing = bundle.clone();
    trailing.extend_from_slice(b"more\n");
    assert!(ProofBundle::parse(&trailing).is_err());
    let no_extra: String = text
        .lines()
        .filter(|line| !line.starts_with("extra "))
        .map(|l| format!("{l}\n"))
        .collect();
    assert!(matches!(
        ProofBundle::parse(no_extra.as_bytes())
            .unwrap()
            .verify_inclusion(),
        Err(TError::Malformed { .. })
    ));
}

/// Every single-bit flip of a bundle fails to parse or to verify (SC-006).
fn assert_deformations_refused(bundle: &[u8], policy: &Policy) {
    let accepted = ProofBundle::parse(bundle).unwrap().verify(policy).unwrap();
    for bit in 0..bundle.len() * 8 {
        let mut bad = bundle.to_vec();
        bad[bit / 8] ^= 1 << (bit % 8);
        if let Ok(parsed) = ProofBundle::parse(&bad)
            && let Ok(proven) = parsed.verify(policy)
        {
            panic!("bit {bit} flipped is still accepted: {proven:?} (was {accepted:?})");
        }
    }
}

#[test]
fn us4_every_single_bit_deformation_of_entry_and_seal_bundles_is_refused() {
    let books = sealed();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let policy = Policy::log_only(key().vkey());
    let id = books.entry_by_key("sale-3").unwrap().id(&acme());
    assert_deformations_refused(
        &books.bundle_entry(id, &checkpoint).unwrap().to_bytes(),
        &policy,
    );
    let seal = books
        .leaves()
        .find(|leaf| leaf.bytes[0] == 8)
        .unwrap()
        .position;
    assert_deformations_refused(
        &books.bundle_record(seal, &checkpoint).unwrap().to_bytes(),
        &policy,
    );
}

// ── US5: prove one sealed balance ───────────────────────────────────────────

const ACCOUNTS: usize = 50;

fn account(i: usize) -> String {
    if i.is_multiple_of(2) {
        format!("Assets:A{i:02}")
    } else {
        format!("Liabilities:L{i:02}")
    }
}

/// SC-007's ledger: 50 accounts, EUR and USD, settled and pending (holds),
/// two sealed months; account 49 never posts.
fn fifty() -> Journal {
    let mut books = Journal::new(acme())
        .with_salt_source(|| MemoSalt([2; 16]))
        .as_actor("setup");
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books.declare_currency(USD, 2, date!("2026-01-01")).unwrap();
    for i in 0..ACCOUNTS {
        let kind = if i % 2 == 0 {
            AccountKind::Asset
        } else {
            AccountKind::Liability
        };
        books.open(account(i), kind, date!("2026-01-01")).unwrap();
    }
    books.define_months(2026, 1..=3).unwrap();
    let mut n = 0;
    for (month, end) in [(1u8, date!("2026-01-31")), (2, date!("2026-02-28"))] {
        let day = end.replace_day(10).unwrap();
        for i in 0..ACCOUNTS - 1 {
            n += 1;
            let currency = if i % 3 == 0 { USD } else { EUR };
            let (debit, credit) = (account(i), account((i + 1) % (ACCOUNTS - 1)));
            books
                .record(
                    Entry::new(format!("m{month}-{n}"), day)
                        .debit(debit.as_str(), amount!("12.34"), currency)
                        .credit(credit.as_str(), amount!("12.34"), currency),
                )
                .unwrap();
            if i % 5 == 0 {
                books
                    .record(
                        Entry::open_hold(format!("hold-{n}"), day)
                            .debit(debit.as_str(), amount!("3.00"), currency)
                            .credit(credit.as_str(), amount!("3.00"), currency),
                    )
                    .unwrap();
            }
        }
        books.seal_period(format!("2026-{month:02}")).unwrap();
    }
    books
}

fn query(end: doubleentry::Date, layer: Layer) -> BalanceQuery {
    match layer {
        Layer::Settled => BalanceQuery::through(end),
        Layer::Pending => BalanceQuery::through(end).pending(),
    }
}

#[test]
fn us5_all_400_sealed_balances_verify_and_name_their_account() {
    let books = fifty();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let mut verifier = Verifier::new().log(key().vkey()).unwrap();
    verifier.update(&checkpoint, None).unwrap();
    let mut proven = 0;
    for (period, end) in [
        ("2026-01", date!("2026-01-31")),
        ("2026-02", date!("2026-02-28")),
    ] {
        for i in 0..ACCOUNTS {
            for currency in [EUR, USD] {
                for layer in [Layer::Settled, Layer::Pending] {
                    let name = account(i);
                    let bundle = books
                        .bundle_balance(name.as_str(), currency, layer, period, &checkpoint)
                        .unwrap();
                    let Proven::Balance(balance) = verifier.verify(&bundle).unwrap() else {
                        panic!("a balance");
                    };
                    assert_eq!(balance.account.to_string(), name);
                    assert_eq!((balance.currency, balance.layer), (currency, layer));
                    assert_eq!(balance.period.as_str(), period);
                    assert_eq!(
                        balance.balance,
                        books
                            .balance(name.as_str(), currency, &query(end, layer))
                            .unwrap(),
                        "{name} {currency} {layer:?} {period}"
                    );
                    let proof = books
                        .prove_balance(name.as_str(), currency, layer, period)
                        .unwrap();
                    assert_eq!(
                        proof.verify(&books.head()).unwrap().balance,
                        balance.balance
                    );
                    proven += 1;
                }
            }
        }
    }
    assert_eq!(proven, 400);
}

#[test]
fn us5_zero_balances_and_missing_trees_verify_as_zero() {
    let mut books = books(1);
    books
        .open("Expenses:Unused", AccountKind::Expense, date!("2026-01-01"))
        .unwrap();
    books.seal_period("2026-01").unwrap();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let policy = Policy::log_only(key().vkey());
    for (account, currency, layer) in [
        ("Expenses:Unused", EUR, Layer::Settled),
        ("Assets:Cash", USD, Layer::Settled),
        ("Assets:Cash", EUR, Layer::Pending),
    ] {
        let bundle = books
            .bundle_balance(account, currency, layer, "2026-01", &checkpoint)
            .unwrap();
        let Proven::Balance(balance) = bundle.verify(&policy).unwrap() else {
            panic!("a balance");
        };
        assert!(balance.balance.is_zero(), "{account} {currency} {layer:?}");
    }
    let proof = books
        .prove_balance("Assets:Cash", USD, Layer::Settled, "2026-01")
        .unwrap();
    assert!(proof.balance.is_none(), "no USD tree: no path");
}

#[test]
fn us5_unsealed_periods_and_late_accounts_are_query_errors() {
    let mut books = books(1);
    books.seal_period("2026-01").unwrap();
    books
        .open("Assets:Late", AccountKind::Asset, date!("2026-02-01"))
        .unwrap();
    assert!(matches!(
        books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-02"),
        Err(Error::Query(QueryError::NotSealed { .. }))
    ));
    assert!(matches!(
        books.prove_balance("Assets:Late", EUR, Layer::Settled, "2026-01"),
        Err(Error::Query(QueryError::UnknownAccount { .. }))
    ));
    assert!(matches!(
        books.prove_balance("Assets:Nobody", EUR, Layer::Settled, "2026-01"),
        Err(Error::Query(QueryError::UnknownAccount { .. }))
    ));
    assert!(matches!(
        books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2027-13"),
        Err(Error::Query(QueryError::UnknownPeriod { .. }))
    ));
    assert!(matches!(
        books.prove_balance("Assets:Cash", EUR, Layer::Settled, " "),
        Err(Error::Query(QueryError::InvalidLabel { .. }))
    ));
}

/// The `extra` layout, decoded by this test from the documentation.
struct Layout {
    seal_leaf: Vec<u8>,
    currency: String,
    layer: u8,
    handle: u32,
    balance: Option<(Vec<u8>, Vec<Hash>)>,
    account_position: u64,
    account_leaf: Vec<u8>,
    account_path: Vec<Hash>,
}

fn decode_layout(extra: &[u8]) -> Layout {
    let mut at = 0;
    let mut take = |n: usize| {
        let out = extra[at..at + n].to_vec();
        at += n;
        out
    };
    fn u64le(b: Vec<u8>) -> u64 {
        u64::from_le_bytes(b.try_into().unwrap())
    }
    let tag_len = u64le(take(8)) as usize;
    assert_eq!(take(tag_len), b"doubleentry/balance-proof/1");
    let n = u64le(take(8)) as usize;
    let seal_leaf = take(n);
    let n = u64le(take(8)) as usize;
    let currency = String::from_utf8(take(n)).unwrap();
    let layer = take(1)[0];
    let handle = u32::from_le_bytes(take(4).try_into().unwrap());
    let balance = match take(1)[0] {
        0 => None,
        _ => {
            let n = u64le(take(8)) as usize;
            let leaf = take(n);
            let count = u64le(take(8)) as usize;
            let path = (0..count)
                .map(|_| Hash::from_bytes(take(32).try_into().unwrap()))
                .collect();
            Some((leaf, path))
        }
    };
    let account_position = u64le(take(8));
    let n = u64le(take(8)) as usize;
    let account_leaf = take(n);
    let count = u64le(take(8)) as usize;
    let account_path = (0..count)
        .map(|_| Hash::from_bytes(take(32).try_into().unwrap()))
        .collect();
    assert_eq!(at, extra.len(), "no trailing bytes");
    Layout {
        seal_leaf,
        currency,
        layer,
        handle,
        balance,
        account_position,
        account_leaf,
        account_path,
    }
}

/// RFC 6962 inclusion by its recursive definition (test-only, independent
/// of the crate's verifier).
fn rfc6962_root(index: u64, size: u64, leaf: Hash, path: &[Hash]) -> Option<Hash> {
    fn node(l: &Hash, r: &Hash) -> Hash {
        Hash::from_bytes(
            Sha256::new()
                .chain_update([1u8])
                .chain_update(l.as_bytes())
                .chain_update(r.as_bytes())
                .finalize()
                .into(),
        )
    }
    if size == 1 {
        return path.is_empty().then_some(leaf);
    }
    let k = 1u64 << (63 - (size - 1).leading_zeros());
    let (last, rest) = path.split_last()?;
    if index < k {
        Some(node(&rfc6962_root(index, k, leaf, rest)?, last))
    } else {
        Some(node(last, &rfc6962_root(index - k, size - k, leaf, rest)?))
    }
}

#[test]
fn us5_balance_bundles_follow_the_documented_layout_and_resist_tampering() {
    let books = fifty();
    let checkpoint = books.checkpoint(&key()).unwrap();
    let head = checkpoint.checkpoint().head();
    let policy = Policy::log_only(key().vkey());
    let bundle = books
        .bundle_balance(account(4), EUR, Layer::Settled, "2026-02", &checkpoint)
        .unwrap();
    let layout = decode_layout(bundle.extra().unwrap());
    assert_eq!(
        (layout.currency.as_str(), layout.layer, layout.handle),
        ("EUR", 0, 4)
    );
    // Each path, checked independently against the root the layout names.
    assert_eq!(
        rfc6962_root(
            bundle.index(),
            head.size,
            leaf_hash(&layout.seal_leaf),
            bundle.path()
        ),
        Some(head.root)
    );
    assert_eq!(
        rfc6962_root(
            layout.account_position,
            head.size,
            leaf_hash(&layout.account_leaf),
            &layout.account_path
        ),
        Some(head.root)
    );
    let Record::Seal(seal) = Record::<2>::decode(&layout.seal_leaf).unwrap() else {
        panic!()
    };
    let (leaf, path) = layout.balance.clone().unwrap();
    assert_eq!(
        rfc6962_root(4, u64::from(seal.parts().accounts), leaf_hash(&leaf), &path),
        Some(seal.tree(EUR, Layer::Settled).unwrap().root)
    );

    // Another account's balance leaf in place of this one's: refused.
    let other = books
        .bundle_balance(account(6), EUR, Layer::Settled, "2026-02", &checkpoint)
        .unwrap();
    let other_layout = decode_layout(other.extra().unwrap());
    let swap = |from: &[u8], to: &[u8]| {
        let text = String::from_utf8(bundle.to_bytes()).unwrap();
        let extra = bundle.extra().unwrap();
        let position = extra.windows(from.len()).position(|w| w == from).unwrap();
        let mut changed = extra.to_vec();
        changed.splice(position..position + from.len(), to.iter().copied());
        let line = |e: &[u8]| format!("extra {}\n", base64(e));
        ProofBundle::parse(text.replace(&line(extra), &line(&changed)).as_bytes()).unwrap()
    };
    let substituted = swap(&leaf, &other_layout.balance.as_ref().unwrap().0);
    assert!(substituted.verify(&policy).is_err());
    let rehandled = swap(&4u32.to_le_bytes()[..], &6u32.to_le_bytes()[..]);
    assert!(rehandled.verify(&policy).is_err());
    let recurrency = swap(b"EUR", b"USD");
    assert!(recurrency.verify(&policy).is_err());
    let reaccount = swap(&layout.account_leaf, &other_layout.account_leaf);
    assert!(reaccount.verify(&policy).is_err());

    // SC-006: three balance bundles, every single-bit flip refused.
    assert_deformations_refused(&bundle.to_bytes(), &policy);
    assert_deformations_refused(
        &books
            .bundle_balance(account(3), USD, Layer::Pending, "2026-01", &checkpoint)
            .unwrap()
            .to_bytes(),
        &policy,
    );
    assert_deformations_refused(
        &books
            .bundle_balance(account(49), EUR, Layer::Settled, "2026-01", &checkpoint)
            .unwrap()
            .to_bytes(),
        &policy,
    );
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ── US6: tiles ──────────────────────────────────────────────────────────────

#[test]
fn us6_entry_bundles_hash_to_their_tiles() {
    let journal = books(300);
    let size = journal.head().size;
    assert!(size > 256);
    for tile in TileId::changed(0, size) {
        let bytes = journal.tile(tile).unwrap();
        assert_eq!(bytes.len(), 32 * usize::from(tile.width()));
        if let Some(entries) = tile.entry_bundle() {
            let bundle = journal.tile_entries(entries).unwrap();
            // Every entry hashes to the matching level-0 tile hash.
            let mut rest = bundle.as_slice();
            let mut i = 0;
            while !rest.is_empty() {
                let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                assert_eq!(
                    leaf_hash(&rest[2..2 + len]).as_bytes(),
                    &bytes[32 * i..32 * i + 32]
                );
                rest = &rest[2 + len..];
                i += 1;
            }
            assert_eq!(i, usize::from(tile.width()));
        }
    }
    let beyond = TileId::hashes(0, 9, 1).unwrap();
    assert!(matches!(
        journal.tile(beyond),
        Err(Error::Query(QueryError::PositionBeyondHead { .. }))
    ));
    let entries = TileId::entries(0, 1).unwrap();
    assert!(matches!(
        journal.tile(entries),
        Err(Error::Transparency(TError::WrongTileKind { .. }))
    ));
    assert!(matches!(
        journal.tile_entries(TileId::hashes(0, 0, 1).unwrap()),
        Err(Error::Transparency(TError::WrongTileKind { .. }))
    ));
}

/// The ledger answers exactly what the journal answers: checkpoints,
/// bundles, tiles and entry bundles, byte for byte.
#[tokio::test]
async fn us6_journal_and_ledger_render_identical_bytes() {
    let journal = fifty();
    let store_books = mirror(&journal).await;
    let size = journal.head().size;
    let checkpoint = journal.checkpoint(&key()).unwrap();
    assert_eq!(store_books.checkpoint(&key()).await.unwrap(), checkpoint);
    for tile in TileId::changed(0, size) {
        assert_eq!(
            store_books.tile(tile).await.unwrap(),
            journal.tile(tile).unwrap()
        );
        if let Some(entries) = tile.entry_bundle() {
            assert_eq!(
                store_books.tile_entries(entries).await.unwrap(),
                journal.tile_entries(entries).unwrap()
            );
        }
    }
    for position in [0, 7, size - 1] {
        assert_eq!(
            store_books
                .bundle_record(position, &checkpoint)
                .await
                .unwrap(),
            journal.bundle_record(position, &checkpoint).unwrap()
        );
    }
    for (i, currency, layer, period) in [
        (3, USD, Layer::Pending, "2026-01"),
        (10, EUR, Layer::Settled, "2026-02"),
        (49, EUR, Layer::Settled, "2026-02"),
    ] {
        assert_eq!(
            store_books
                .bundle_balance(account(i), currency, layer, period, &checkpoint)
                .await
                .unwrap(),
            journal
                .bundle_balance(account(i), currency, layer, period, &checkpoint)
                .unwrap()
        );
        assert_eq!(
            store_books
                .prove_balance(account(i), currency, layer, period)
                .await
                .unwrap(),
            journal
                .prove_balance(account(i), currency, layer, period)
                .unwrap()
        );
    }
    let id = journal.entry_by_key("m2-60").unwrap().id(&acme());
    assert_eq!(
        store_books.bundle_entry(id, &checkpoint).await.unwrap(),
        journal.bundle_entry(id, &checkpoint).unwrap()
    );
}

/// A `Ledger` over a fresh `MemoryStore` holding the journal's records,
/// written through the store's own write path.
async fn mirror(journal: &Journal) -> Ledger<MemoryStore> {
    let store = MemoryStore::new();
    let ledger = Ledger::new(store, acme())
        .await
        .unwrap()
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([2; 16]));
    ledger
        .declare_currency(EUR, 2, date!("2026-01-01"))
        .await
        .unwrap();
    ledger
        .declare_currency(USD, 2, date!("2026-01-01"))
        .await
        .unwrap();
    for i in 0..ACCOUNTS {
        let kind = if i % 2 == 0 {
            AccountKind::Asset
        } else {
            AccountKind::Liability
        };
        ledger
            .open(account(i), kind, date!("2026-01-01"))
            .await
            .unwrap();
    }
    ledger.define_months(2026, 1..=3).await.unwrap();
    let mut n = 0;
    for (month, end) in [(1u8, date!("2026-01-31")), (2, date!("2026-02-28"))] {
        let day = end.replace_day(10).unwrap();
        for i in 0..ACCOUNTS - 1 {
            n += 1;
            let currency: Currency = if i % 3 == 0 { USD } else { EUR };
            let (debit, credit) = (account(i), account((i + 1) % (ACCOUNTS - 1)));
            ledger
                .record(
                    Entry::new(format!("m{month}-{n}"), day)
                        .debit(debit.as_str(), amount!("12.34"), currency)
                        .credit(credit.as_str(), amount!("12.34"), currency),
                )
                .await
                .unwrap();
            if i % 5 == 0 {
                ledger
                    .record(
                        Entry::open_hold(format!("hold-{n}"), day)
                            .debit(debit.as_str(), amount!("3.00"), currency)
                            .credit(credit.as_str(), amount!("3.00"), currency),
                    )
                    .await
                    .unwrap();
            }
        }
        ledger
            .seal_period(format!("2026-{month:02}"))
            .await
            .unwrap();
    }
    assert_eq!(ledger.head().await.unwrap(), journal.head());
    ledger
}

/// A store that counts `read_nodes` calls and the ids they ask for.
#[derive(Clone)]
struct Counting {
    inner: MemoryStore,
    reads: std::sync::Arc<std::sync::Mutex<Vec<Vec<u64>>>>,
}

mod counting {
    use super::Counting;
    use doubleentry::store::*;
    use doubleentry::{
        AccountHandle, AccountRef, Balance, EntryId, HoldCursor, Page, PageRead, Rejection,
        log::Hash, record::Genesis, store::BalanceKey,
    };

    impl Store<2> for Counting {
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
            self.inner.read_leaves(read).await
        }
        async fn read_nodes(&self, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
            self.reads.lock().unwrap().push(ids.to_vec());
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
            self.inner.account_records(handle, page).await
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
}

/// SC-008: a tile is one `read_nodes` of its W nodes, and across every tile
/// of a log no stored node is read twice.
#[tokio::test]
async fn us6_each_tile_is_one_read_of_its_own_nodes() {
    let store = Counting {
        inner: MemoryStore::new(),
        reads: std::sync::Arc::default(),
    };
    let ledger = Ledger::new(store.clone(), acme())
        .await
        .unwrap()
        .as_actor("setup");
    ledger
        .declare_currency(EUR, 2, date!("2026-01-01"))
        .await
        .unwrap();
    ledger
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .await
        .unwrap();
    ledger
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .await
        .unwrap();
    ledger.define_months(2026, 1..=1).await.unwrap();
    for n in 0..600 {
        ledger
            .record(
                Entry::new(format!("sale-{n}"), date!("2026-01-15"))
                    .debit("Assets:Cash", amount!("1.00"), EUR)
                    .credit("Income:Sales", amount!("1.00"), EUR),
            )
            .await
            .unwrap();
    }
    let size = ledger.head().await.unwrap().size;
    store.reads.lock().unwrap().clear();
    let mut seen = std::collections::BTreeSet::new();
    let tiles = TileId::changed(0, size);
    assert!(tiles.len() >= 4);
    for tile in tiles {
        ledger.tile(tile).await.unwrap();
        let reads = std::mem::take(&mut *store.reads.lock().unwrap());
        assert_eq!(reads.len(), 1, "{tile}");
        assert_eq!(reads[0].len(), usize::from(tile.width()));
        for id in &reads[0] {
            assert!(seen.insert(*id), "node {id} read twice");
        }
    }
}

// ── Audit round 3 (2026-10-16): regressions, asserting the fixed behaviour ──

#[cfg(feature = "testing")]
mod audit3 {
    use super::*;
    use doubleentry::testing::TestWitness;
    use doubleentry::transparency::{
        KeyType, MAX_BUNDLE_BYTES, MAX_EXTRA_BYTES, MAX_NOTE_BYTES, MAX_REQUEST_BYTES, Next,
        PolicyRule, Threshold, WitnessSession,
    };

    fn rule(rule: PolicyRule) -> Result<Policy, TError> {
        Err(TError::PolicyRule { line: 0, rule })
    }

    fn tree(leaves: impl IntoIterator<Item = u8>) -> Tree {
        let mut tree = Tree::new();
        for leaf in leaves {
            tree.push(leaf_hash(&[leaf])).unwrap();
        }
        tree
    }

    /// Cosigns `checkpoint` with `witness` (first contact).
    fn cosign(witness: &mut TestWitness, checkpoint: &SignedCheckpoint) -> SignedCheckpoint {
        let mut session = WitnessSession::new(checkpoint.clone(), witness.vkey(), 0).unwrap();
        let reply = witness.post(&session.request(None).unwrap());
        match session
            .receive(reply.status, reply.content_type, &reply.body)
            .unwrap()
        {
            Next::Cosigned { cosignature, .. } => {
                checkpoint.with_cosignature(&cosignature).unwrap()
            }
            other => panic!("{other:?}"),
        }
    }

    // PoC: A3-T1 a3_t1_policy_builder_newline_injects_a_trusted_log — no
    // builder name can add or change a policy line.
    #[test]
    fn a3_t1_policy_builder_names_cannot_inject_lines() {
        let evil = LogKey::from_seed(&LedgerId::new("evil.example/log").unwrap(), [66; 32]);
        let w1 = TestWitness::new("witness.example/w1", [9; 32]).unwrap();
        let injected = format!("w\nlog {}\n#", evil.vkey());
        assert_eq!(
            Policy::builder()
                .log(key().vkey())
                .witness("w", w1.vkey())
                .quorum(&injected)
                .build(),
            rule(PolicyRule::Name)
        );
        assert_eq!(
            Policy::builder()
                .log(key().vkey())
                .witness(&format!("w {}\nquorum none\n#", w1.vkey()), w1.vkey())
                .build(),
            rule(PolicyRule::Name)
        );
        assert_eq!(
            Policy::builder()
                .log(key().vkey())
                .witness("w", w1.vkey())
                .group("g", Threshold::Any, &["w\nquorum none"])
                .quorum("g")
                .build(),
            rule(PolicyRule::Name)
        );
        assert_eq!(
            Policy::k_of_n(key().vkey(), 1, [("w\r".to_owned(), w1.vkey())]),
            rule(PolicyRule::Name)
        );
        // Names without whitespace build exactly what the text says.
        let built = Policy::k_of_n(key().vkey(), 1, [("w".to_owned(), w1.vkey())]).unwrap();
        assert_eq!(built.logs().count(), 1);
        assert_eq!(Policy::parse(&built.to_string()).unwrap(), built);
    }

    // PoC: A3-T2 a3_t2_log_key_reused_as_witness_key_lets_the_log_meet_its_own_quorum
    // — the log's own key is never a witness.
    #[test]
    fn a3_t2_a_witness_key_that_is_the_log_key_is_refused() {
        // The log operator's seed, presented as a witness (0x04).
        let mut sock = TestWitness::new("witness.example/independent", [7; 32])
            .unwrap()
            .log(key().vkey());
        assert_eq!(sock.vkey().public_key(), key().vkey().public_key());
        assert_eq!(
            Policy::k_of_n(key().vkey(), 1, [("w".to_owned(), sock.vkey())]),
            rule(PolicyRule::WitnessIsLog)
        );
        let text = format!(
            "witness w {}\nlog {}\nquorum w\n",
            sock.vkey(),
            key().vkey()
        );
        assert_eq!(
            Policy::parse(&text),
            Err(TError::PolicyRule {
                line: 2,
                rule: PolicyRule::WitnessIsLog
            })
        );
        // Its cosignature does not count towards a quorum of real witnesses.
        let honest = TestWitness::new("witness.example/w1", [9; 32]).unwrap();
        let policy = Policy::k_of_n(key().vkey(), 1, [("w1".to_owned(), honest.vkey())]).unwrap();
        let cosigned = cosign(&mut sock, &signed(&key(), tree(0..3).head()));
        assert!(matches!(
            policy.verify(&cosigned),
            Err(TError::QuorumNotMet { .. })
        ));
        // A 0x04 key on a log line stays accepted (FR-013).
        let as_log = TestWitness::new(acme().as_str(), [5; 32]).unwrap();
        let policy = Policy::parse(&format!("log {}\nquorum none\n", as_log.vkey())).unwrap();
        assert_eq!(
            policy.logs().next().unwrap().key_type(),
            KeyType::CosignatureEd25519
        );
    }

    /// A valid, non-small-order point whose `0x04` key ID under
    /// `witness.example/w1` collides with the honest witness's (ground in
    /// 15 s, audit round 3).
    fn shadow() -> Vkey {
        let hex = "9e8f351f000000005a0000000000000000000000000000000300000000000000";
        let key: [u8; 32] =
            core::array::from_fn(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap());
        Vkey::new(
            KeyName::new("witness.example/w1").unwrap(),
            KeyType::CosignatureEd25519,
            key,
        )
        .unwrap()
    }

    // PoC: A3-T3 a3_t3_ground_key_id_collision_shadows_honest_witness — two
    // keys of one (name, key ID) are refused, never resolved
    // by taking the first.
    #[test]
    fn a3_t3_ambiguous_key_sets_are_refused() {
        let mut honest = TestWitness::new("witness.example/w1", [9; 32])
            .unwrap()
            .log(key().vkey());
        let shadow = shadow();
        assert_eq!(shadow.key_id(), honest.vkey().key_id());
        assert_ne!(shadow.public_key(), honest.vkey().public_key());
        assert_eq!(
            Policy::builder()
                .log(key().vkey())
                .witness("attacker", shadow.clone())
                .witness("honest", honest.vkey())
                .group("g", Threshold::Any, &["honest"])
                .quorum("g")
                .build(),
            rule(PolicyRule::AmbiguousKey)
        );
        let text = format!(
            "log {}\nwitness attacker {shadow}\nwitness honest {}\nquorum honest\n",
            key().vkey(),
            honest.vkey()
        );
        assert_eq!(
            Policy::parse(&text),
            Err(TError::PolicyRule {
                line: 3,
                rule: PolicyRule::AmbiguousKey
            })
        );
        let cosigned = cosign(&mut honest, &signed(&key(), tree(0..3).head()));
        let ambiguous = Err(TError::AmbiguousKey {
            name: shadow.name().clone(),
            key_id: shadow.key_id(),
        });
        assert_eq!(
            cosigned.verify(&[key().vkey(), shadow.clone(), honest.vkey()]),
            ambiguous
        );
        assert_eq!(
            cosigned.note().verify(&[honest.vkey(), shadow.clone()]),
            ambiguous
        );
        // The same key twice is one key.
        assert_eq!(
            cosigned
                .verify(&[key().vkey(), honest.vkey(), honest.vkey()])
                .unwrap()
                .len(),
            2
        );
        // A verifier takes the same log key twice.
        assert!(
            Verifier::new()
                .log(key().vkey())
                .unwrap()
                .log(key().vkey())
                .is_ok()
        );
    }

    // PoC: A3-T3 — a verifier refuses two log keys of one (name, key ID).
    #[test]
    fn a3_t3_a_verifier_refuses_an_ambiguous_log_key() {
        // The shadow point under the honest witness's name doubles as a log
        // key of that name: same (name, key ID) as `honest`, other key.
        let honest = TestWitness::new("witness.example/w1", [9; 32])
            .unwrap()
            .vkey();
        let verifier = Verifier::new().log(honest.clone()).unwrap();
        assert_eq!(
            verifier.log(shadow()).err(),
            Some(TError::AmbiguousKey {
                name: honest.name().clone(),
                key_id: honest.key_id(),
            })
        );
    }

    // PoC: A3-T4 a3_t4_garbage_consistency_proof_is_reported_with_fork_evidence
    // — a corrupted proof is no evidence; a proof that binds the
    // offered root and shows another prefix is.
    #[test]
    fn a3_t4_only_a_binding_proof_is_fork_evidence() {
        let honest = tree(0..13);
        let at7 = signed(&key(), honest.head_at(7).unwrap());
        let at13 = signed(&key(), honest.head());
        let mut verifier = Verifier::new().log(key().vkey()).unwrap();
        verifier.update(&at7, None).unwrap();
        let garbage = ConsistencyProof {
            old_size: 7,
            new_size: 13,
            path: vec![leaf_hash(b"x"); 4],
        };
        assert!(matches!(
            verifier.update(&at13, Some(&garbage)),
            Err(TError::Proof(_))
        ));
        let mut short = honest.consistency_proof(7, 13).unwrap();
        short.path.pop();
        assert!(matches!(
            verifier.update(&at13, Some(&short)),
            Err(TError::Proof(_))
        ));
        let mut shifted = honest.consistency_proof(7, 13).unwrap();
        shifted.old_size = 6;
        assert_eq!(
            verifier.update(&at13, Some(&shifted)),
            Err(TError::ProofShape)
        );

        // The log signs a 13-leaf tree whose first 7 leaves differ, and
        // gives its real proof: it recomputes the signed root, and another
        // root for size 7.
        let forked = tree((0..13).map(|i| if i == 2 { 99 } else { i }));
        let other13 = signed(&key(), forked.head());
        let proof = forked.consistency_proof(7, 13).unwrap();
        let Err(TError::Inconsistent(evidence)) = verifier.update(&other13, Some(&proof)) else {
            panic!("a binding proof of another prefix is evidence");
        };
        assert_eq!(
            *evidence,
            ForkEvidenceView::of(at7.checkpoint().head(), Some(&at7), &other13, Some(&proof))
        );
        // What a third party checks: the proof reaches the offered root
        // from the forked prefix, not from the trusted one.
        let prefix = forked.head_at(7).unwrap();
        assert_ne!(prefix.root, at7.checkpoint().head().root);
        proof.verify(&prefix, &other13.checkpoint().head()).unwrap();
        assert!(
            proof
                .verify(&at7.checkpoint().head(), &other13.checkpoint().head())
                .is_err()
        );
        assert_eq!(verifier.head(&origin()), Some(at7.checkpoint().head()));

        // From a power-of-two size the proof omits the old root, so a fork
        // cannot be shown by it: refused, without evidence.
        let mut from8 = Verifier::new().log(key().vkey()).unwrap();
        from8
            .update(&signed(&key(), honest.head_at(8).unwrap()), None)
            .unwrap();
        assert!(matches!(
            from8.update(&other13, Some(&forked.consistency_proof(8, 13).unwrap())),
            Err(TError::Proof(_))
        ));
    }

    // PoC: A3-T5 a3_t5_session_adopts_decreasing_witness_sizes — a witness
    // that goes backwards is a protocol error.
    #[test]
    fn a3_t5_a_witness_size_going_backwards_ends_the_session() {
        let witness = TestWitness::new("witness.example/w1", [9; 32]).unwrap();
        let mut session =
            WitnessSession::new(signed(&key(), tree(0..10).head()), witness.vkey(), 0).unwrap();
        let size = Some("text/x.tlog.size");
        assert_eq!(session.receive(409, size, b"9\n"), Ok(Next::Retry));
        assert_eq!(
            session.receive(409, size, b"2\n"),
            Err(TError::Witness(WitnessError::WitnessRegressed {
                previous: 9,
                size: 2
            }))
        );
        assert_eq!(session.old(), 9);
        assert_eq!(
            session.receive(409, size, b"9\n"),
            Err(TError::Witness(WitnessError::SessionFinished))
        );
        // The same size again is a retry (counted towards the limit).
        let mut again =
            WitnessSession::new(signed(&key(), tree(0..10).head()), witness.vkey(), 0).unwrap();
        assert_eq!(again.receive(409, size, b"9\n"), Ok(Next::Retry));
        assert_eq!(again.receive(409, size, b"9\n"), Ok(Next::Retry));
    }

    // PoC: A3-T6 a3_t6_bundle_extra_line_has_no_size_limit — every input has
    // a total size limit, checked first.
    #[test]
    fn a3_t6_bundles_requests_and_replies_have_size_limits() {
        const { assert!(MAX_NOTE_BYTES < MAX_REQUEST_BYTES) };
        const { assert!(MAX_REQUEST_BYTES < MAX_BUNDLE_BYTES) };
        let checkpoint = signed(&key(), tree(0..1).head());
        let mut huge = b"c2sp.org/tlog-proof@v1\nextra ".to_vec();
        huge.resize(MAX_BUNDLE_BYTES + 1, b'A');
        assert_eq!(
            ProofBundle::parse(&huge),
            Err(TError::TooLarge {
                limit: MAX_BUNDLE_BYTES
            })
        );
        // An `extra` above its limit, in a bundle below the total one.
        let mut bytes = b"c2sp.org/tlog-proof@v1\nextra ".to_vec();
        bytes.extend(std::iter::repeat_n(b'A', (MAX_EXTRA_BYTES / 3 + 1) * 4));
        bytes.extend_from_slice(b"\nindex 0\n\n");
        bytes.extend_from_slice(&checkpoint.to_bytes());
        assert!(bytes.len() < MAX_BUNDLE_BYTES);
        assert_eq!(
            ProofBundle::parse(&bytes),
            Err(TError::TooLarge {
                limit: MAX_EXTRA_BYTES
            })
        );
        let mut request = b"old 0\n\n".to_vec();
        request.resize(MAX_REQUEST_BYTES + 1, b'x');
        assert_eq!(
            AddCheckpoint::parse(&request),
            Err(TError::TooLarge {
                limit: MAX_REQUEST_BYTES
            })
        );
        let witness = TestWitness::new("witness.example/w1", [9; 32]).unwrap();
        let mut long = vec![b'1'; 64];
        long.push(b'\n');
        assert_eq!(
            WitnessReply::read(
                409,
                Some("text/x.tlog.size"),
                &long,
                &checkpoint,
                &witness.vkey()
            ),
            Err(TError::Witness(WitnessError::MalformedReply))
        );
    }

    // PoC: A3-T8 — what a zero-balance bundle binds, as documented: the
    // seal and the absence of a tree, not the (currency, layer) bytes.
    #[test]
    fn a3_t8_a_zero_balance_bundle_does_not_bind_its_currency() {
        let mut books = books(1);
        books.seal_period("2026-01").unwrap();
        let checkpoint = books.checkpoint(&key()).unwrap();
        let policy = Policy::log_only(key().vkey());
        let rewrite = |bundle: &ProofBundle, from: &[u8], to: &[u8]| {
            let extra = bundle.extra().unwrap();
            let at = extra.windows(from.len()).position(|w| w == from).unwrap();
            let mut changed = extra.to_vec();
            changed.splice(at..at + from.len(), to.iter().copied());
            let line = |e: &[u8]| format!("extra {}\n", base64(e));
            let text = String::from_utf8(bundle.to_bytes()).unwrap();
            ProofBundle::parse(text.replace(&line(extra), &line(&changed)).as_bytes()).unwrap()
        };
        // USD was never declared: the seal has no USD tree, the balance is 0.
        let zero = books
            .bundle_balance("Assets:Cash", USD, Layer::Settled, "2026-01", &checkpoint)
            .unwrap();
        let Proven::Balance(rewritten) = rewrite(&zero, b"USD", b"GBP").verify(&policy).unwrap()
        else {
            panic!("a balance bundle");
        };
        assert_eq!(rewritten.currency.as_str(), "GBP");
        assert_eq!(rewritten.balance, Default::default());
        // A balance from a tree is bound to its (currency, layer).
        let eur = books
            .bundle_balance("Assets:Cash", EUR, Layer::Settled, "2026-01", &checkpoint)
            .unwrap();
        assert!(rewrite(&eur, b"EUR", b"USD").verify(&policy).is_err());
    }

    // PoC: A3-T7 a3_t7_reply_with_thousands_of_duplicate_cosignatures_is_accepted
    // — a 200 reply repeating a line is refused before any
    // signature is checked.
    #[test]
    fn a3_t7_replies_with_repeated_lines_are_refused() {
        let mut witness = TestWitness::new("witness.example/w1", [9; 32])
            .unwrap()
            .log(key().vkey());
        let checkpoint = signed(&key(), tree(0..3).head());
        let reply = witness.post(&AddCheckpoint::new(&checkpoint, 0, None).unwrap().to_bytes());
        assert_eq!(reply.status, 200);
        let read = |body: &[u8]| WitnessReply::read(200, None, body, &checkpoint, &witness.vkey());
        assert!(read(&reply.body).is_ok());
        let malformed = Err(TError::Witness(WitnessError::MalformedReply));
        assert_eq!(read(&reply.body.repeat(2)), malformed);
        assert_eq!(
            read(&reply.body.repeat(1_000_000 / reply.body.len())),
            malformed
        );
        // A repeated line with a broken signature is refused the same way:
        // shape first, signatures after.
        let mut broken = reply.body.clone();
        let at = broken.len() - 3;
        broken[at] = if broken[at] == b'A' { b'B' } else { b'A' };
        assert_eq!(read(&[broken.clone(), broken].concat()), malformed);
    }
}

// ── 024 P1 (post-impl): strict Ed25519 at the signature level ──────────────

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| *c != b'=') {
        acc = (acc << 6) | B64.iter().position(|b| *b == c).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        out.push_str(&"=".repeat(3 - chunk.len()));
    }
    out
}

/// The signed note with its one signature's 64 bytes replaced by `edit`.
fn with_signature(signed: &SignedCheckpoint, edit: impl Fn(&mut [u8; 64])) -> Vec<u8> {
    let text = String::from_utf8(signed.to_bytes()).unwrap();
    let (body, line) = text.trim_end().rsplit_once('\n').unwrap();
    let (prefix, encoded) = line.rsplit_once(' ').unwrap();
    let mut raw = b64_decode(encoded);
    let mut signature: [u8; 64] = raw[4..].try_into().unwrap();
    edit(&mut signature);
    raw[4..].copy_from_slice(&signature);
    format!("{body}\n{prefix} {}\n", b64_encode(&raw)).into_bytes()
}

/// A signature whose S is not reduced (S + ℓ: the same point equation, so a
/// lax verifier would accept it — the malleability strict verification
/// closes) and one whose R is a small-order point (the identity) are both
/// refused as `BadSignature`; the untouched note still verifies.
#[test]
// PoC: control_malleated_s_is_refused (kept: S + ℓ refused)
fn p1_non_canonical_s_and_small_order_r_are_refused() {
    // ℓ = 2^252 + 27742317777372353535851937790883648493, little-endian.
    const L: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let books = books(2);
    let signed = signed(&key(), books.head());
    let keys = [key().vkey()];
    assert!(signed.verify(&keys).is_ok());
    let same = with_signature(&signed, |_| {});
    assert_eq!(same, signed.to_bytes(), "the helper re-encodes faithfully");

    let non_canonical = with_signature(&signed, |signature| {
        let mut carry = 0u16;
        for (s, l) in signature[32..].iter_mut().zip(L) {
            let sum = u16::from(*s) + u16::from(l) + carry;
            *s = sum as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0, "S + ℓ fits 256 bits");
    });
    let small_order_r = with_signature(&signed, |signature| {
        signature[..32].copy_from_slice(&{
            let mut identity = [0u8; 32];
            identity[0] = 1;
            identity
        });
    });
    for bytes in [non_canonical, small_order_r] {
        let note = SignedCheckpoint::parse(&bytes).unwrap();
        assert!(
            matches!(note.verify(&keys), Err(TError::BadSignature { .. })),
            "{:?}",
            note.verify(&keys)
        );
    }
}
