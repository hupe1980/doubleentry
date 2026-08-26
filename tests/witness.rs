//! The split-view attack, and the witness that refuses to participate in it.
//!
//! Every other guarantee in this crate is relative to a tree head. An operator
//! who can serve two heads can serve two histories, hand each party proofs that
//! verify perfectly against the head *they* were given, and nothing inside
//! either view can tell. These tests build that attack against a real journal
//! and check that a witness is what stops it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use doubleentry::period::LedgerId;
use doubleentry::witness::{
    AddCheckpoint, MemoryWitnessStore, Origin, SignedTreeHead, Witness, WitnessError,
};
use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal, MerkleLog, TreeHead};
use time::macros::date;

type Eur = Amount<2>;

fn origin() -> Origin {
    Origin::new("example.com/ledgers/acme-gmbh").expect("valid")
}

/// Index of the one entry the two histories disagree about.
///
/// Early in the log on purpose: a restatement at the *end* would leave every
/// earlier prefix identical, and the interesting case is the one where a later
/// consistency proof has to notice a change buried behind it.
const RESTATED: u32 = 3;

/// A journal with `n` ordinary entries, where entry [`RESTATED`] is booked for
/// `restated_minor` rather than the usual amount.
///
/// The two histories below differ in exactly that one figure, which is what
/// makes the attack realistic: an operator does not rewrite the books, it
/// restates a number and keeps everything else.
fn books(n: u32, restated_minor: i64) -> Journal<2> {
    let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh").expect("valid"));
    let cash = journal
        .register_path("Assets:Cash", date!(2026 - 01 - 01))
        .expect("registers");
    let revenue = journal
        .register_path("Income:Sales", date!(2026 - 01 - 01))
        .expect("registers");

    for i in 0..n {
        let minor = if i == RESTATED {
            restated_minor
        } else {
            10_000
        };
        journal
            .record(
                Entry::new(
                    EntryId::from_uuid(uuid::Uuid::from_u128(u128::from(i))),
                    IdempotencyKey::new(format!("entry-{i}").into_bytes()).expect("valid"),
                    date!(2026 - 03 - 15),
                )
                .debit(cash, Eur::from_minor(minor), Currency::EUR)
                .credit(revenue, Eur::from_minor(minor), Currency::EUR),
            )
            .expect("records");
    }
    journal
}

fn log_of(journal: &Journal<2>) -> MerkleLog {
    MerkleLog::from_leaves(
        journal
            .entries()
            .iter()
            .map(doubleentry::Entry::content_hash)
            .collect(),
    )
}

/// The attack, and the fact that proofs alone do not see it.
///
/// This test asserts the *weakness*. It is here so the guarantee the next test
/// establishes is measured against something real rather than against a claim.
#[test]
fn two_histories_are_each_internally_perfect() {
    let honest = books(8, 10_000);
    let restated = books(8, 99_900);

    assert_eq!(honest.len(), restated.len());
    assert_ne!(honest.head().root, restated.head().root);

    // Both journals verify completely, on their own terms.
    for journal in [&honest, &restated] {
        assert!(journal.verify_log());
        assert!(journal.verify_balances().expect("no overflow"));
        assert!(journal.verify_balanced().expect("no overflow"));

        // And every inclusion proof inside each one checks out.
        let head = journal.head();
        for (index, entry) in journal.entries().iter().enumerate() {
            let proof = journal
                .prove_inclusion(doubleentry::LogIndex::new(index as u64))
                .expect("in range");
            assert!(proof.verify(&entry.content_hash(), &head));
        }
    }

    // An auditor shown one of them, and only one, has no question they can ask
    // that distinguishes it from the other. That is the gap a witness fills.
}

/// The same attack, against a witness.
#[test]
fn a_witness_refuses_the_second_history_at_the_same_size() {
    let honest = books(8, 10_000);
    let restated = books(8, 99_900);

    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness
        .trust(origin(), honest.head())
        .expect("first time we hear about this log");

    let refusal = witness.offer(&origin(), restated.head(), None);
    match refusal {
        Err(WitnessError::Fork {
            size,
            known,
            offered,
            ..
        }) => {
            assert_eq!(size, 8);
            assert_eq!(known, honest.head().root);
            assert_eq!(offered, restated.head().root);
        }
        other => panic!("expected a fork to be named, got {other:?}"),
    }

    // And it did not move: it still stands behind the history it vouched for.
    assert_eq!(witness.head(&origin()), Some(honest.head()));
}

/// A fork discovered later is caught by the proof rather than by the size.
#[test]
fn a_witness_refuses_a_history_that_does_not_extend_what_it_signed() {
    let honest = books(8, 10_000);
    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness.trust(origin(), honest.head()).expect("first time");

    // The operator restates an earlier figure and carries on booking, so by the
    // time the witness is asked again the sizes no longer collide.
    let restated = books(12, 99_900);
    let proof = log_of(&restated)
        .consistency_proof_between(8, 12)
        .expect("in range");

    // The proof is genuine *within the restated history* …
    assert!(proof.verify(
        &log_of(&restated).head_at(8).expect("in range"),
        &restated.head()
    ));
    // … and says nothing about the one the witness actually signed.
    assert!(matches!(
        witness.offer(&origin(), restated.head(), Some(&proof)),
        Err(WitnessError::BadProof {
            from: 8,
            to: 12,
            ..
        })
    ));
}

/// The honest path, end to end, in the shape a deployment would use it.
#[test]
fn the_honest_path_advances_the_witness_and_cosigns() {
    let mut journal = books(8, 10_000);
    let published = journal.head();

    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness.trust(origin(), published).expect("first time");

    // Time passes; the books grow.
    let cash = journal
        .accounts()
        .id_of(&"Assets:Cash".parse().expect("valid"))
        .expect("registered");
    let revenue = journal
        .accounts()
        .id_of(&"Income:Sales".parse().expect("valid"))
        .expect("registered");
    journal
        .record(
            Entry::new(
                EntryId::generate(),
                IdempotencyKey::new(b"later".to_vec()).expect("valid"),
                date!(2026 - 03 - 20),
            )
            .debit(cash, Eur::from_minor(4_200), Currency::EUR)
            .credit(revenue, Eur::from_minor(4_200), Currency::EUR),
        )
        .expect("records");

    let now = journal.head();
    let proof = journal
        .prove_consistency_between(published.size, now.size)
        .expect("in range");

    // What the operator would POST to /add-checkpoint, and what the witness
    // would read out of the body.
    let request = AddCheckpoint::new(SignedTreeHead::new(origin(), now), proof);
    let body = request.to_body();
    let received = AddCheckpoint::parse_body(&body, published.size).expect("parses");

    let accepted = witness
        .offer_note(&origin(), received.note(), Some(received.proof()))
        .expect("extends what the witness holds");
    assert!(accepted.advanced);
    assert_eq!(witness.head(&origin()), Some(now));

    // A replay of the same request is a no-op, not an error: an at-least-once
    // delivery path will send it twice.
    let again = witness
        .offer_note(&origin(), received.note(), Some(received.proof()))
        .expect("idempotent");
    assert!(!again.advanced);
}

/// Growth without a proof is refused, however plausible the head looks.
#[test]
fn a_witness_will_not_take_growth_on_trust() {
    let journal = books(8, 10_000);
    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness
        .trust(origin(), journal.head_at(4).expect("in range"))
        .expect("first time");

    assert!(matches!(
        witness.offer(&origin(), journal.head(), None),
        Err(WitnessError::MissingProof { from: 4, to: 8, .. })
    ));
}

/// A witness that forgot could be restarted into signing a fork.
#[test]
fn witness_state_is_what_makes_it_a_witness() {
    let honest = books(8, 10_000);
    let restated = books(8, 99_900);

    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness.trust(origin(), honest.head()).expect("first time");

    // What a durable store would have written.
    let persisted = witness.tracked();
    assert_eq!(persisted, vec![(origin(), honest.head())]);

    // Reloaded, it still refuses.
    let mut reloaded = Witness::new(MemoryWitnessStore::from_heads(persisted));
    assert!(matches!(
        reloaded.offer(&origin(), restated.head(), None),
        Err(WitnessError::Fork { .. })
    ));

    // Had it come back empty, it would have adopted the fork without hesitation
    // — which is why `WitnessStore` insists on durability.
    let mut amnesiac = Witness::new(MemoryWitnessStore::new());
    amnesiac
        .trust(origin(), restated.head())
        .expect("nothing to contradict");
    assert_eq!(amnesiac.head(&origin()), Some(restated.head()));
}

/// The whole arrangement: a verifier that requires a cosignature is not
/// showable a forked history.
#[cfg(feature = "witness")]
#[test]
fn a_verifier_requiring_a_cosignature_cannot_be_shown_a_fork() {
    use doubleentry::witness::KeyName;
    use doubleentry::witness::signing::{Cosigner, SigningKey};

    const AT: u64 = 1_800_000_000;

    let honest = books(8, 10_000);
    let restated = books(8, 99_900);

    // The witness — somebody who is not the operator — with its own key.
    let key = SigningKey::from_seed(KeyName::new("auditor.example").expect("valid"), [7u8; 32]);
    let public = key.verifying_key();
    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness.trust(origin(), honest.head()).expect("first time");
    let mut cosigner = Cosigner::new(witness, key);

    // The honest head gets a signature.
    let signature = cosigner
        .cosign(&origin(), honest.head(), None, AT)
        .expect("the head it already holds");

    let mut published = SignedTreeHead::new(origin(), honest.head());
    published.add_signature(signature);

    // A third party, holding only the public key, checks the published note.
    let parsed = SignedTreeHead::parse(&published.to_note()).expect("parses");
    let line = parsed.signatures.first().expect("one signature");
    assert_eq!(
        public.verify_cosignature(&parsed.body(), line),
        Some(AT),
        "the cosignature has to survive the text it travels in"
    );

    // The forked head gets nothing at all — not a bad signature, no bytes.
    assert!(matches!(
        cosigner.cosign(&origin(), restated.head(), None, AT),
        Err(WitnessError::Fork { .. })
    ));

    // So the operator cannot assemble a note for the fork that this verifier
    // would accept: the only cosignature in existence is over the other root.
    let forged = SignedTreeHead {
        origin: origin(),
        head: restated.head(),
        extensions: Vec::new(),
        signatures: parsed.signatures.clone(),
    };
    assert_eq!(
        public.verify_cosignature(&forged.body(), line),
        None,
        "a cosignature lifted onto another head does not verify"
    );
}

/// A cosignature is over one head and cannot be moved to another size either.
#[cfg(feature = "witness")]
#[test]
fn a_cosignature_does_not_transfer_across_sizes() {
    use doubleentry::witness::KeyName;
    use doubleentry::witness::signing::{Cosigner, SigningKey};

    let journal = books(8, 10_000);
    let key = SigningKey::from_seed(KeyName::new("auditor.example").expect("valid"), [3u8; 32]);
    let public = key.verifying_key();

    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness
        .trust(origin(), journal.head_at(4).expect("in range"))
        .expect("first time");
    let mut cosigner = Cosigner::new(witness, key);

    let proof = journal.prove_consistency_between(4, 8).expect("in range");
    let signature = cosigner
        .cosign(&origin(), journal.head(), Some(&proof), 1_800_000_000)
        .expect("extends");

    let at_eight = SignedTreeHead::new(origin(), journal.head()).body();
    let at_four = SignedTreeHead::new(origin(), journal.head_at(4).expect("in range")).body();
    assert!(public.verify_cosignature(&at_eight, &signature).is_some());
    assert!(public.verify_cosignature(&at_four, &signature).is_none());
}

/// An origin is part of the signed body, so a head cannot be moved between logs.
#[cfg(feature = "witness")]
#[test]
fn a_cosignature_does_not_transfer_across_logs() {
    use doubleentry::witness::KeyName;
    use doubleentry::witness::signing::{Cosigner, SigningKey};

    let journal = books(8, 10_000);
    let key = SigningKey::from_seed(KeyName::new("auditor.example").expect("valid"), [5u8; 32]);
    let public = key.verifying_key();

    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness.trust(origin(), journal.head()).expect("first time");
    let mut cosigner = Cosigner::new(witness, key);
    let signature = cosigner
        .cosign(&origin(), journal.head(), None, 1_800_000_000)
        .expect("the head it holds");

    let other = Origin::new("example.com/ledgers/someone-else").expect("valid");
    let elsewhere = SignedTreeHead::new(other, journal.head()).body();
    assert_eq!(
        public.verify_cosignature(&elsewhere, &signature),
        None,
        "two entities' books can have identical roots; the origin is what separates them"
    );
}

/// A witness tracks logs independently, and one log's growth is not another's.
#[test]
fn logs_are_tracked_independently() {
    let a = Origin::new("example.com/ledgers/a").expect("valid");
    let b = Origin::new("example.com/ledgers/b").expect("valid");
    let books_a = books(8, 10_000);
    let books_b = books(8, 99_900);

    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness.trust(a.clone(), books_a.head()).expect("first");
    witness.trust(b.clone(), books_b.head()).expect("first");

    assert_eq!(witness.head(&a), Some(books_a.head()));
    assert_eq!(witness.head(&b), Some(books_b.head()));

    // Identical roots at identical sizes, under different origins, are simply
    // two logs — but offering one's root under the other's name is a fork.
    assert!(matches!(
        witness.offer(&a, books_b.head(), None),
        Err(WitnessError::Fork { .. })
    ));
}

/// An empty starting head cannot be proven against, and the type says so.
#[test]
fn a_witness_started_from_nothing_takes_the_first_head_on_trust() {
    let journal = books(8, 10_000);
    let mut witness = Witness::new(MemoryWitnessStore::new());
    witness
        .trust(
            origin(),
            TreeHead {
                size: 0,
                root: doubleentry::merkle::empty_root(),
            },
        )
        .expect("first time");

    // No proof exists to demand: every log extends the empty tree, so this crate
    // refuses to build one at all.
    assert!(matches!(
        journal.prove_consistency(0),
        Err(doubleentry::ProofError::EmptyOldTree { .. })
    ));

    // The first non-empty head is therefore taken on trust — which is precisely
    // why `trust` asks for a head with entries in it.
    assert!(
        witness
            .offer(&origin(), journal.head(), None)
            .expect("accepts")
            .advanced
    );

    // From there it is a real witness again.
    assert!(matches!(
        witness.offer(&origin(), books(8, 99_900).head(), None),
        Err(WitnessError::Fork { .. })
    ));
}
/// The C2SP wire format, pinned — and cross-checked against Go.
///
/// Interoperating is the *entire* justification for using a specified format
/// rather than inventing one: a witness is only worth having if it is somebody
/// else's, and a bespoke encoding could only ever be cosigned by software this
/// crate ships. So these bytes are not merely "what we produce today" — the
/// values below were verified against Go's `crypto/ed25519` and the key-hash
/// construction from `golang.org/x/mod/sumdb/note`:
///
/// ```text
/// go pubkey          = [25 127 107 35 ...]        ← matches
/// go keyhash 0x01    = [243 123 2 127]            ← matches
/// go keyhash 0x04    = [135 175 99 210]           ← matches
/// ed25519.Verify(pub, cosignature message, sig) = true
/// ```
///
/// A change here is a change to what other implementations will accept, which is
/// a different and much larger thing than a change to an internal encoding.
#[cfg(feature = "witness")]
#[test]
fn the_c2sp_wire_format_is_unchanged() {
    use doubleentry::Hash;
    use doubleentry::witness::KeyName;
    use doubleentry::witness::SignedTreeHead;
    use doubleentry::witness::signing::SigningKey;

    let head = TreeHead {
        size: 4812,
        root: Hash::from_bytes([0x2a; 32]),
    };
    let origin = Origin::new("example.com/ledgers/acme-gmbh").expect("valid");
    let sth = SignedTreeHead::new(origin, head);

    // The body: origin, canonical decimal size, standard base64 root, each
    // newline-terminated. This is what a signature covers.
    assert_eq!(
        sth.body(),
        "example.com/ledgers/acme-gmbh\n4812\nKioqKioqKioqKioqKioqKioqKioqKioqKioqKioqKio=\n"
    );

    let key = SigningKey::from_seed(KeyName::new("witness.example").expect("valid"), [42u8; 32]);
    let public = key.verifying_key();

    // Ed25519 from a fixed seed — the same key Go derives.
    assert_eq!(
        public.to_bytes(),
        [
            25, 127, 107, 35, 225, 108, 133, 50, 198, 171, 200, 56, 250, 205, 94, 167, 137, 190,
            12, 118, 178, 146, 3, 52, 3, 155, 250, 139, 61, 54, 141, 97
        ]
    );

    // SHA-256(name || '\n' || alg || pubkey)[..4]. The algorithm byte is inside,
    // so one key has two selectors and a note signature cannot be presented as a
    // cosignature.
    assert_eq!(public.note_key_hash(), [243, 123, 2, 127]);
    assert_eq!(public.cosignature_key_hash(), [135, 175, 99, 210]);
    assert_ne!(public.note_key_hash(), public.cosignature_key_hash());

    // The whole note, byte for byte. `ed25519.Verify` in Go accepts this
    // signature over `cosignature/v1\ntime 1800000000\n` + the body.
    let mut signed = sth.clone();
    signed.add_signature(key.cosign_body(&sth.body(), 1_800_000_000));
    assert_eq!(
        signed.to_note(),
        "example.com/ledgers/acme-gmbh\n\
         4812\n\
         KioqKioqKioqKioqKioqKioqKioqKioqKioqKioqKio=\n\
         \n\
         \u{2014} witness.example h69j0gAAAABrSdIA7Hg6V4WP+wDB6CebuLVhwtaBKodoGLoBDCJKA8JW09Lc\
         IBScooZ5utolWCQC56pYHpyBXzbeku+spdHhNLpyCQ==\n"
    );

    // And it round-trips, because the text is the thing that is signed.
    let parsed = SignedTreeHead::parse(&signed.to_note()).expect("parses");
    assert_eq!(parsed, signed);
    assert_eq!(
        public.verify_cosignature(
            &parsed.body(),
            parsed.signatures.first().expect("one signature")
        ),
        Some(1_800_000_000)
    );
}
