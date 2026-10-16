//! Interoperability with the Go reference implementations, in both
//! directions.
//!
//! **Go → crate**: `tests/vectors/c2sp/` holds vectors `interop/c2sp/gen`
//! wrote with `golang.org/x/mod/sumdb/{note,tlog}`,
//! `transparency-dev/{formats,merkle}` and the stock
//! `transparency-dev/witness`. Every test here reads them — no Go needed —
//! and checks the crate decides exactly what the vector records. Where the
//! crate is deliberately stricter than Go (or, for ML-DSA-44, fails
//! closed), the vector records both verdicts.
//!
//! **crate → Go**: `emit` (ignored; `just interop` runs it) writes
//! checkpoints, request bodies, bundles, policies and tiles of a real ledger
//! to `target/interop/`, which `interop/c2sp/check` verifies with Go and
//! feeds to the stock witness; `witness_roundtrip` (ignored, run after the
//! check) verifies the cosignatures the stock witness returned.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use doubleentry::store::Tree;
use doubleentry::transparency::{
    AddCheckpoint, Checkpoint, Error, KeyName, Policy, ProofBundle, SignedCheckpoint, TileId, Vkey,
};
use doubleentry::{
    TreeHead,
    log::{ConsistencyProof, Hash, InclusionProof, leaf_hash},
};
use sha2::{Digest, Sha256};

// ── Reading vectors ─────────────────────────────────────────────────────────

/// One vector file: its input and expected fields, in order.
struct Vector {
    path: PathBuf,
    input: Vec<(String, String)>,
    expect: Vec<(String, String)>,
}

impl Vector {
    fn read(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap();
        let (mut input, mut expect) = (Vec::new(), Vec::new());
        let mut section = "";
        for (i, line) in text.lines().enumerate() {
            match line {
                "# input" | "# expect" => section = line,
                _ if i == 0 && line.starts_with("# ") => {}
                _ => {
                    let (key, value) = line
                        .split_once(": ")
                        .unwrap_or((line.trim_end_matches(':'), ""));
                    let field = (key.to_owned(), value.to_owned());
                    if section == "# input" {
                        input.push(field);
                    } else {
                        expect.push(field);
                    }
                }
            }
        }
        Self {
            path: path.to_owned(),
            input,
            expect,
        }
    }

    fn find<'a>(fields: &'a [(String, String)], key: &str, path: &Path) -> &'a str {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("{}: no field {key}", path.display()))
    }

    fn input(&self, key: &str) -> &str {
        Self::find(&self.input, key, &self.path)
    }

    fn expect(&self, key: &str) -> &str {
        Self::find(&self.expect, key, &self.path)
    }

    /// A field of either section.
    fn field(&self, key: &str) -> &str {
        if self.input.iter().any(|(k, _)| k == key) {
            self.input(key)
        } else {
            self.expect(key)
        }
    }

    fn input_bytes(&self, key: &str) -> Vec<u8> {
        base64(self.input(key))
    }

    fn expect_bytes(&self, key: &str) -> Vec<u8> {
        base64(self.expect(key))
    }

    fn expect_all(&self, key: &str) -> Vec<&str> {
        self.expect
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn name(&self) -> String {
        self.path.display().to_string()
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/c2sp")
}

/// Every vector in a directory, by file name.
fn vectors(dir: &str) -> Vec<Vector> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(root().join(dir))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no vectors in {dir}");
    paths.iter().map(|path| Vector::read(path)).collect()
}

/// Standard base64 with padding (test-side, lenient on nothing).
fn base64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let value = |c: u8| ALPHABET.iter().position(|a| *a == c).unwrap() as u32;
    let mut out = Vec::new();
    for group in text.as_bytes().chunks(4) {
        let n = group.iter().filter(|c| **c != b'=').count();
        let mut acc = 0u32;
        for (i, c) in group.iter().enumerate() {
            if *c != b'=' {
                acc |= value(*c) << (18 - 6 * i);
            }
        }
        out.extend_from_slice(&acc.to_be_bytes()[1..n]);
    }
    out
}

fn hash(text: &str) -> Hash {
    Hash::from_bytes(base64(text).try_into().unwrap())
}

fn hashes(text: &str) -> Vec<Hash> {
    text.split_whitespace().map(hash).collect()
}

/// The variant an error is, as the vectors name it.
fn cause(error: &Error) -> String {
    use doubleentry::transparency::WitnessError as W;
    match error {
        Error::Malformed { what, .. } => format!("Malformed:{what:?}"),
        Error::UnsupportedKeyType(byte) => format!("UnsupportedKeyType:{byte}"),
        Error::PolicyRule { rule, .. } => format!("PolicyRule:{rule:?}"),
        Error::DuplicateSignature { .. } => "DuplicateSignature".into(),
        Error::BadSignature { .. } => "BadSignature".into(),
        Error::QuorumNotMet { .. } => "QuorumNotMet".into(),
        Error::Witness(W::Conflict { size }) => format!("Conflict:{size}"),
        Error::Witness(W::Unexpected { status }) => format!("Unexpected:{status}"),
        Error::Witness(witness) => format!("{witness:?}"),
        other => format!("{other:?}"),
    }
}

// ── The manifest ────────────────────────────────────────────────────────────

/// Every vector file is the one Go wrote: its SHA-256 is in the MANIFEST,
/// and the MANIFEST names no other file.
#[test]
fn every_vector_matches_the_manifest() {
    let manifest = std::fs::read_to_string(root().join("MANIFEST")).unwrap();
    let mut listed = BTreeMap::new();
    for line in manifest.lines() {
        let (digest, path) = line.split_once("  ").unwrap();
        listed.insert(path.to_owned(), digest.to_owned());
    }
    let mut found = Vec::new();
    let mut stack = vec![root()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().unwrap() != "MANIFEST" {
                found.push(path);
            }
        }
    }
    assert_eq!(found.len(), listed.len(), "files not in the MANIFEST");
    for path in found {
        let relative = path
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let digest: [u8; 32] = Sha256::digest(std::fs::read(&path).unwrap()).into();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(listed.get(&relative), Some(&hex), "{relative}");
    }
}

// ── Notes, checkpoints and keys ─────────────────────────────────────────────

#[cfg(feature = "witness")]
fn acme() -> doubleentry::LedgerId {
    doubleentry::LedgerId::new("example.com/ledgers/acme").unwrap()
}

/// Go's checkpoints parse and re-render byte for byte; signed by the crate
/// from the same seed, they are the same bytes (SC-002).
#[test]
fn notes_checkpoints_are_byte_identical() {
    for name in ["size-0", "size-1", "size-13", "size-70000", "extensions"] {
        let v = Vector::read(&root().join(format!("notes/checkpoint-{name}.txt")));
        let note = base64(v.field("note_b64"));
        let signed = SignedCheckpoint::parse(&note).unwrap();
        assert_eq!(signed.to_bytes(), note, "{name}");
        assert_eq!(v.expect("go"), "accept", "{name}");
        if name == "extensions" {
            assert_eq!(signed.checkpoint().extensions(), ["ext one", "ext two"]);
            continue;
        }
        let head = TreeHead {
            size: v.input("size").parse().unwrap(),
            root: hash(v.input("root")),
        };
        let checkpoint = Checkpoint::new(KeyName::new(v.input("origin")).unwrap(), head);
        assert_eq!(signed.checkpoint(), &checkpoint);
        assert_eq!(checkpoint.text(), signed.note().text());
        #[cfg(feature = "witness")]
        {
            let key = doubleentry::transparency::LogKey::from_seed(&acme(), [7; 32]);
            assert_eq!(key.sign(&checkpoint).unwrap().to_bytes(), note, "{name}");
            assert_eq!(key.vkey().to_string(), v.expect("vkey"));
        }
    }
    let example = Vector::read(&root().join("notes/signed-note-example.txt"));
    let note = doubleentry::transparency::Note::parse(&example.input_bytes("note_b64")).unwrap();
    assert_eq!(note.to_bytes(), example.input_bytes("note_b64"));
}

/// Every Go note verifies with its vkey; extension lines are covered by the
/// log signature.
#[cfg(feature = "witness")]
#[test]
fn notes_verify_with_their_vkeys() {
    use doubleentry::transparency::Note;
    for v in vectors("notes") {
        let key: Vkey = v.field("vkey").parse().unwrap();
        let note = Note::parse(&base64(v.field("note_b64"))).unwrap();
        assert_eq!(note.verify(&[key]).unwrap().len(), 1, "{}", v.name());
    }
}

/// Every malformed note is refused with its own cause; where Go accepts
/// (lax base64, duplicates dropped, non-canonical numbers, short roots,
/// empty lines) the crate is stricter, never laxer.
#[test]
fn notes_malformed_are_refused_with_their_cause() {
    let mut count = 0;
    for v in vectors("notes/malformed") {
        count += 1;
        let parsed = SignedCheckpoint::parse(&v.input_bytes("note_b64"));
        let crate_verdict = parsed.as_ref().map_or_else(cause, |_| "accept".to_owned());
        assert_eq!(crate_verdict, v.expect("crate"), "{}", v.name());
        if v.expect("go") == "reject" {
            assert!(
                parsed.is_err(),
                "Go refuses {} but the crate accepts",
                v.name()
            );
        }
    }
    assert!(count >= 10, "{count} malformed notes");
}

/// Verifier keys parse or are refused as recorded.
#[test]
// PoC: a3_t8_uppercase_key_id_refused (kept: upper-case key IDs refused; Go accepts)
fn keys_parse_as_recorded() {
    let mut count = 0;
    for v in vectors("keys") {
        count += 1;
        let parsed = v.input("vkey").parse::<Vkey>();
        let expected = match v.expect("crate") {
            "WeakKey" if !cfg!(feature = "witness") => "Ed25519",
            other => other,
        };
        let verdict = match &parsed {
            Ok(key) => format!("{:?}", key.key_type()),
            Err(error) => cause(error),
        };
        assert_eq!(verdict, expected, "{}", v.name());
        if let Ok(key) = parsed {
            assert_eq!(key.to_string(), v.input("vkey"), "{}", v.name());
        }
    }
    assert!(count >= 6);
}

// ── Trees and proofs ────────────────────────────────────────────────────────

fn leaf(i: u64) -> Hash {
    leaf_hash(format!("leaf-{i}").as_bytes())
}

fn tree(size: u64) -> Tree {
    let mut tree = Tree::new();
    for i in 0..size {
        tree.push(leaf(i)).unwrap();
    }
    tree
}

/// Flips bit `bit` of a hash.
fn flipped(hash: &Hash, bit: usize) -> Hash {
    let mut bytes = *hash.as_bytes();
    bytes[bit / 8] ^= 1 << (bit % 8);
    Hash::from_bytes(bytes)
}

/// Go's inclusion proofs: (proof, leaf hash, head).
fn inclusion_vectors(big: &Tree) -> Vec<(InclusionProof, Hash, TreeHead)> {
    let inclusion = Vector::read(&root().join("trees/inclusion.txt"));
    inclusion
        .expect_all("proof")
        .into_iter()
        .map(|line| {
            let mut parts = line.split(' ');
            let index: u64 = parts.next().unwrap().parse().unwrap();
            let size: u64 = parts.next().unwrap().parse().unwrap();
            let path = parts.map(hash).collect();
            (
                InclusionProof { index, size, path },
                leaf(index),
                big.head_at(size).unwrap(),
            )
        })
        .collect()
}

/// Go's consistency proofs: (proof, old head, new head).
fn consistency_vectors(big: &Tree) -> Vec<(ConsistencyProof, TreeHead, TreeHead)> {
    let consistency = Vector::read(&root().join("trees/consistency.txt"));
    consistency
        .expect_all("proof")
        .into_iter()
        .map(|line| {
            let mut parts = line.split(' ');
            let old_size: u64 = parts.next().unwrap().parse().unwrap();
            let new_size: u64 = parts.next().unwrap().parse().unwrap();
            let path = parts.map(hash).collect();
            let heads = (
                big.head_at(old_size).unwrap(),
                big.head_at(new_size).unwrap(),
            );
            (
                ConsistencyProof {
                    old_size,
                    new_size,
                    path,
                },
                heads.0,
                heads.1,
            )
        })
        .collect()
}

/// Runs `check` over `items` on every core.
fn on_all_cores<T: Sync>(items: &[T], check: impl Fn(&T) + Sync) {
    let cores = std::thread::available_parallelism().map_or(4, usize::from);
    std::thread::scope(|scope| {
        for chunk in items.chunks(items.len().div_ceil(cores)) {
            let check = &check;
            scope.spawn(move || chunk.iter().for_each(check));
        }
    });
}

/// Roots, and inclusion and consistency proofs for every size pair up to 64
/// and at 70 000: the crate verifies Go's and builds the same.
#[test]
fn trees_proofs_verify_and_build_identically() {
    let big = tree(70_000);
    let roots = Vector::read(&root().join("trees/roots.txt"));
    for line in roots.expect_all("root") {
        let (size, expected) = line.split_once(' ').unwrap();
        let size: u64 = size.parse().unwrap();
        assert_eq!(
            big.head_at(size).unwrap().root,
            hash(expected),
            "root {size}"
        );
    }
    let inclusion = inclusion_vectors(&big);
    assert_eq!(inclusion.len(), 64 * 65 / 2 + 4);
    for (proof, leaf, head) in &inclusion {
        proof.verify(leaf, head).unwrap();
        assert_eq!(
            &big.inclusion_proof(proof.index, proof.size).unwrap(),
            proof
        );
    }
    let consistency = consistency_vectors(&big);
    assert_eq!(consistency.len(), 64 * 65 / 2 + 3);
    for (proof, old, new) in &consistency {
        proof.verify(old, new).unwrap();
        assert_eq!(
            &big.consistency_proof(proof.old_size, proof.new_size)
                .unwrap(),
            proof
        );
    }
}

/// Every single-bit flip of any hash, the index or the size of every
/// inclusion-proof vector is refused (SC-006).
#[test]
fn trees_inclusion_proofs_resist_every_single_bit_deformation() {
    let big = tree(70_000);
    on_all_cores(&inclusion_vectors(&big), |(proof, leaf, head)| {
        let mut bad = proof.clone();
        for at in 0..proof.path.len() {
            for bit in 0..256 {
                bad.path[at] = flipped(&proof.path[at], bit);
                assert!(bad.verify(leaf, head).is_err());
            }
            bad.path[at] = proof.path[at];
        }
        for bit in 0..64 {
            bad.index = proof.index ^ (1 << bit);
            assert!(bad.verify(leaf, head).is_err());
            bad.index = proof.index;
            bad.size = proof.size ^ (1 << bit);
            assert!(bad.verify(leaf, head).is_err());
            bad.size = proof.size;
        }
    });
}

/// Every single-bit flip of any hash or either size of every
/// consistency-proof vector is refused (SC-006).
#[test]
fn trees_consistency_proofs_resist_every_single_bit_deformation() {
    let big = tree(70_000);
    on_all_cores(&consistency_vectors(&big), |(proof, old, new)| {
        let mut bad = proof.clone();
        for at in 0..proof.path.len() {
            for bit in 0..256 {
                bad.path[at] = flipped(&proof.path[at], bit);
                assert!(bad.verify(old, new).is_err());
            }
            bad.path[at] = proof.path[at];
        }
        for bit in 0..64 {
            bad.old_size = proof.old_size ^ (1 << bit);
            assert!(bad.verify(old, new).is_err());
            bad.old_size = proof.old_size;
            bad.new_size = proof.new_size ^ (1 << bit);
            assert!(bad.verify(old, new).is_err());
            bad.new_size = proof.new_size;
        }
    });
}

// ── Witness protocol ────────────────────────────────────────────────────────

/// The crate builds the exact bodies Go formats and the stock witness
/// accepted, and parses them back.
#[test]
fn witness_requests_are_byte_identical() {
    let mut count = 0;
    for v in vectors("witness/requests") {
        count += 1;
        let signed = SignedCheckpoint::parse(&v.input_bytes("checkpoint_b64")).unwrap();
        let old: u64 = v.input("old").parse().unwrap();
        let path = hashes(v.input("proof"));
        let size = signed.checkpoint().head().size;
        let proof = (!path.is_empty()).then_some(ConsistencyProof {
            old_size: old,
            new_size: size,
            path,
        });
        let body = AddCheckpoint::new(&signed, old, proof.as_ref()).unwrap();
        assert_eq!(body.to_bytes(), v.expect_bytes("body_b64"), "{}", v.name());
        assert_eq!(AddCheckpoint::parse(&body.to_bytes()).unwrap(), body);
        assert_eq!(v.expect("status"), "200", "{}", v.name());
    }
    assert_eq!(count, 6);
}

/// Every reply the stock witness sent, and every one the protocol allows,
/// maps to its outcome.
#[cfg(feature = "witness")]
#[test]
fn witness_replies_map_to_their_outcomes() {
    use doubleentry::transparency::WitnessReply;
    let mut count = 0;
    for v in vectors("witness/replies") {
        count += 1;
        let request = AddCheckpoint::parse(&v.input_bytes("request_b64")).unwrap();
        let witness: Vkey = v.input("witness").parse().unwrap();
        let content_type = Some(v.input("content_type")).filter(|value| !value.is_empty());
        let status: u16 = v.input("status").parse().unwrap();
        let read = WitnessReply::read(
            status,
            content_type,
            &v.input_bytes("body_b64"),
            request.checkpoint(),
            &witness,
        );
        let outcome = read.as_ref().map_or_else(cause, |_| "Cosigned".to_owned());
        assert_eq!(outcome, v.expect("crate"), "{}", v.name());
    }
    assert!(count >= 9, "{count} replies");
}

/// Go's cosignatures verify (extension lines included), and attaching one
/// gives the bytes Go's `note.Sign` gives.
#[cfg(feature = "witness")]
#[test]
fn witness_cosignatures_verify_and_attach_like_go() {
    use doubleentry::transparency::{Verified, WitnessReply};
    let log = doubleentry::transparency::LogKey::from_seed(&acme(), [7; 32]).vkey();
    let mut count = 0;
    for v in vectors("witness/cosignatures") {
        count += 1;
        let signed = SignedCheckpoint::parse(&v.input_bytes("checkpoint_b64")).unwrap();
        let witness: Vkey = v.input("witness").parse().unwrap();
        let line = format!("{}\n", v.expect("line"));
        let cosignature =
            WitnessReply::read(200, None, line.as_bytes(), &signed, &witness).unwrap();
        assert_eq!(cosignature.timestamp().to_string(), v.expect("timestamp"));
        assert_eq!(cosignature.to_line(), line);
        let cosigned = signed.with_cosignature(&cosignature).unwrap();
        assert_eq!(
            cosigned.to_bytes(),
            v.expect_bytes("cosigned_b64"),
            "{}",
            v.name()
        );
        let verified = cosigned.verify(&[log.clone(), witness.clone()]).unwrap();
        assert!(matches!(
            verified[..],
            [Verified::Log { .. }, Verified::Cosigner(_)]
        ));
        // A tampered line from the witness key refuses the reply. The
        // character changed sits inside the signature's base64, away from
        // the padding, so the line keeps its length and shape: only the
        // Ed25519 check can fail (024 P2, post-impl).
        let mut tampered = line.into_bytes();
        let at = tampered.len() - 12;
        tampered[at] = if tampered[at] == b'A' { b'B' } else { b'A' };
        assert!(matches!(
            WitnessReply::read(200, None, &tampered, &signed, &witness),
            Err(doubleentry::transparency::Error::Witness(
                doubleentry::transparency::WitnessError::BadCosignature { .. }
            ))
        ));
    }
    assert_eq!(count, 4);
}

// ── Policies ────────────────────────────────────────────────────────────────

fn policy_vectors() -> BTreeMap<String, Vector> {
    vectors("policies")
        .into_iter()
        .map(|v| {
            let name = v.path.file_stem().unwrap().to_string_lossy().into_owned();
            (name, v)
        })
        .collect()
}

/// Every policy parses (or is refused) as formats decides — except the
/// recorded divergences, where the crate refuses what formats accepts
/// (audit round 3: a witness key that is the log key, two keys of one
/// (name, key ID)); the crate writes the canonical form formats writes, and
/// it parses back equal.
#[test]
fn policy_parse_verdicts_equal_go() {
    let policies = policy_vectors();
    assert!(policies.len() >= 10);
    let mut divergent = 0;
    for (name, v) in &policies {
        let text = String::from_utf8(v.input_bytes("policy_b64")).unwrap();
        let parsed = Policy::parse(&text);
        let verdict = parsed.as_ref().map_or_else(cause, |_| "accept".to_owned());
        assert_eq!(verdict, v.expect("crate"), "{name}");
        if v.expect.iter().any(|(k, _)| k == "divergence") {
            // Stricter, never laxer.
            assert!(parsed.is_err() && v.expect("go") == "accept", "{name}");
            divergent += 1;
            continue;
        }
        assert_eq!(parsed.is_ok(), v.expect("go") == "accept", "{name}");
        if let Ok(policy) = parsed {
            let marshal = String::from_utf8(v.expect_bytes("marshal_b64")).unwrap();
            assert_eq!(policy.to_string(), marshal, "{name}");
            assert_eq!(Policy::parse(&policy.to_string()).unwrap(), policy);
        }
    }
    assert_eq!(divergent, 2);
    // formats lets the log meet its own quorum, and refuses every
    // checkpoint the ambiguous pair signs: the crate refuses both policies.
    assert_eq!(
        policies["witness-is-log"].expect("go_verify_cosigned"),
        "accept"
    );
    assert_eq!(
        policies["ambiguous-key-id"].expect("go_verify_cosigned"),
        "reject"
    );
}

/// Every (policy, checkpoint) verdict equals `TLogPolicy.Verify`, except
/// the recorded ML-DSA-44 divergence (SC-004).
#[cfg(feature = "witness")]
#[test]
fn policy_verify_verdicts_equal_go() {
    let policies = policy_vectors();
    let mut count = 0;
    for v in vectors("policies/verdicts") {
        count += 1;
        let text =
            String::from_utf8(policies[v.input("policy")].input_bytes("policy_b64")).unwrap();
        let policy = Policy::parse(&text).unwrap();
        let checkpoint = SignedCheckpoint::parse(&v.input_bytes("checkpoint_b64")).unwrap();
        let result = policy.verify(&checkpoint);
        let verdict = result.as_ref().map_or_else(cause, |_| "accept".to_owned());
        assert_eq!(verdict, v.expect("crate"), "{}", v.name());
        let diverges = v.expect.iter().any(|(k, _)| k == "divergence");
        assert_eq!(
            result.is_ok() == (v.expect("go") == "accept"),
            !diverges,
            "{}",
            v.name()
        );
    }
    assert!(count >= 16, "{count} verdicts");
}

// ── Bundles ─────────────────────────────────────────────────────────────────

/// Go's bundles parse and re-render byte for byte (the long-extra one too,
/// which Go's own line scanner cannot read) and their paths verify.
#[test]
fn bundles_round_trip_and_verify() {
    let mut count = 0;
    for v in vectors("bundles") {
        count += 1;
        let bytes = v.expect_bytes("bundle_b64");
        let bundle = ProofBundle::parse(&bytes).unwrap();
        assert_eq!(bundle.to_bytes(), bytes, "{}", v.name());
        let extra = v.input_bytes("extra_b64");
        assert_eq!(bundle.extra().unwrap_or_default(), extra.as_slice());
        if v.name().ends_with("long-extra.txt") {
            assert_eq!(v.expect("go_unmarshal"), "reject");
            continue;
        }
        let head = bundle.checkpoint().checkpoint().head();
        let proof = InclusionProof {
            index: bundle.index(),
            size: head.size,
            path: bundle.path().to_vec(),
        };
        proof.verify(&leaf(5), &head).unwrap();
        assert_eq!(v.expect("go_inclusion"), "accept");
    }
    assert_eq!(count, 3);
}

/// Every single-bit deformation of Go's bundles is refused by a verifier
/// holding the one-of-one policy (log and witness `w1`) (SC-006).
#[cfg(feature = "witness")]
#[test]
fn bundles_resist_every_single_bit_deformation() {
    let policy =
        String::from_utf8(policy_vectors()["one-of-one"].input_bytes("policy_b64")).unwrap();
    let policy = Policy::parse(&policy).unwrap();
    let accepts = |bytes: &[u8]| -> bool {
        let Ok(bundle) = ProofBundle::parse(bytes) else {
            return false;
        };
        if policy.verify(bundle.checkpoint()).is_err() {
            return false;
        }
        let head = bundle.checkpoint().checkpoint().head();
        let proof = InclusionProof {
            index: bundle.index(),
            size: head.size,
            path: bundle.path().to_vec(),
        };
        bundle.extra().is_none_or(|extra| extra == b"leaf-5")
            && proof.verify(&leaf(5), &head).is_ok()
    };
    for name in ["index-5-extra", "index-5-no-extra"] {
        let bytes =
            Vector::read(&root().join(format!("bundles/{name}.txt"))).expect_bytes("bundle_b64");
        assert!(accepts(&bytes), "{name}");
        for bit in 0..bytes.len() * 8 {
            let mut bad = bytes.clone();
            bad[bit / 8] ^= 1 << (bit % 8);
            assert!(!accepts(&bad), "{name} accepts a flip of bit {bit}");
        }
    }
}

// ── Tiles ───────────────────────────────────────────────────────────────────

/// Tile lists, paths and bytes equal Go's for six sizes; entry bundles are
/// the length-prefixed leaves and hash to their level-0 tile.
#[test]
fn tiles_equal_go() {
    let big = tree(70_000);
    let stored = big.stored();
    let mut count = 0;
    for v in vectors("tiles") {
        count += 1;
        let size: u64 = v.input("size").parse().unwrap();
        let paths =
            |tiles: Vec<TileId>| tiles.iter().map(TileId::path).collect::<Vec<_>>().join(" ");
        assert_eq!(paths(TileId::changed(0, size)), v.expect("changed"));
        assert_eq!(
            paths(TileId::changed(size / 2, size)),
            v.expect("changed_half")
        );
        for line in v.expect_all("tile") {
            let (path, digest) = line.split_once(' ').unwrap();
            let tile = TileId::parse(path).unwrap();
            let bytes: Vec<u8> = tile
                .node_ids(size)
                .unwrap()
                .iter()
                .flat_map(|id| stored[*id as usize].as_bytes().to_vec())
                .collect();
            let sum: [u8; 32] = Sha256::digest(&bytes).into();
            let hex: String = sum.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, digest, "{path} at {size}");
        }
        for line in v.expect_all("entries") {
            let (path, digest) = line.split_once(' ').unwrap();
            let tile = TileId::parse(path).unwrap();
            let mut bundle = Vec::new();
            for position in tile.leaf_range(size).unwrap() {
                let leaf = format!("leaf-{position}");
                bundle.extend_from_slice(&(leaf.len() as u16).to_be_bytes());
                bundle.extend_from_slice(leaf.as_bytes());
            }
            let sum: [u8; 32] = Sha256::digest(&bundle).into();
            let hex: String = sum.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, digest, "{path} at {size}");
        }
    }
    assert_eq!(count, 6);
    // Each stored node is read at most once across all tiles of 70 000.
    let mut seen = std::collections::BTreeSet::new();
    for tile in TileId::changed(0, 70_000) {
        for id in tile.node_ids(70_000).unwrap() {
            assert!(seen.insert(id), "node {id} read twice");
        }
    }
}

// ── crate → Go ──────────────────────────────────────────────────────────────

#[cfg(all(feature = "witness", feature = "testing"))]
mod emitted {
    use super::*;

    /// Where `emit` writes and `interop/c2sp/check` reads.
    fn interop_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/interop")
    }
    use doubleentry::transparency::{LogKey, WitnessReply};
    use doubleentry::{
        AccountKind, Entry, Journal, Layer, amount,
        currency::{EUR, USD},
        date,
    };

    /// The ledger the emitted artefacts are of: 10 batches of entries, two
    /// sealed months, checkpoints after each batch.
    pub(super) fn ledger() -> (Journal, Vec<SignedCheckpoint>, LogKey) {
        let key = LogKey::from_seed(&acme(), [7; 32]);
        let mut books = Journal::new(acme())
            .with_salt_source(|| doubleentry::MemoSalt([3; 16]))
            .as_actor("interop");
        books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
        books.declare_currency(USD, 2, date!("2026-01-01")).unwrap();
        for path in [
            "Assets:Cash",
            "Assets:Bank",
            "Income:Sales",
            "Expenses:Rent",
        ] {
            let kind = match path.split(':').next().unwrap() {
                "Assets" => AccountKind::Asset,
                "Income" => AccountKind::Income,
                _ => AccountKind::Expense,
            };
            books.open(path, kind, date!("2026-01-01")).unwrap();
        }
        books.define_months(2026, 1..=3).unwrap();
        let mut checkpoints = Vec::new();
        let mut n = 0;
        for batch in 0..10u32 {
            for _ in 0..30 {
                n += 1;
                let day = date!("2026-01-01")
                    .replace_day(1 + (n % 28) as u8)
                    .unwrap()
                    .replace_month(time::Month::try_from(1 + (batch / 4) as u8).unwrap())
                    .unwrap();
                let currency = if n % 3 == 0 { USD } else { EUR };
                books
                    .record(
                        Entry::new(format!("interop-{n}"), day)
                            .debit("Assets:Cash", amount!("10.00"), currency)
                            .credit("Income:Sales", amount!("10.00"), currency)
                            .memo("interop"),
                    )
                    .unwrap();
            }
            if batch == 4 {
                books.seal_period("2026-01").unwrap();
            }
            if batch == 8 {
                books.seal_period("2026-02").unwrap();
            }
            checkpoints.push(books.checkpoint(&key).unwrap());
        }
        (books, checkpoints, key)
    }

    pub(super) fn write(path: &str, bytes: &[u8]) {
        let full = interop_dir().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, bytes).unwrap();
    }

    pub(super) fn emit() {
        let _ = std::fs::remove_dir_all(interop_dir());
        let (books, checkpoints, key) = ledger();
        write("vkey.txt", format!("{}\n", key.vkey()).as_bytes());
        let mut old = 0;
        for (i, signed) in checkpoints.iter().enumerate() {
            write(&format!("checkpoints/{i:02}.note"), &signed.to_bytes());
            let size = signed.checkpoint().head().size;
            let proof = (old != 0).then(|| books.prove_consistency(old, size).unwrap());
            let body = AddCheckpoint::new(signed, old, proof.as_ref()).unwrap();
            write(&format!("requests/{i:02}.body"), &body.to_bytes());
            old = size;
        }
        let last = checkpoints.last().unwrap();
        // A stale `old`, a checkpoint by another key, a corrupted proof.
        write(
            "stale.body",
            &AddCheckpoint::new(last, 0, None).unwrap().to_bytes(),
        );
        let foreign = LogKey::from_seed(&acme(), [8; 32])
            .sign(last.checkpoint())
            .unwrap();
        write(
            "foreign.body",
            &AddCheckpoint::new(&foreign, 0, None).unwrap().to_bytes(),
        );
        // A corrupted proof from the witness's size to a later head.
        let mut later = books.clone();
        later
            .record(
                Entry::new("interop-later", date!("2026-03-31"))
                    .debit("Assets:Bank", amount!("1.00"), EUR)
                    .credit("Income:Sales", amount!("1.00"), EUR),
            )
            .unwrap();
        let beyond = later.checkpoint(&key).unwrap();
        let (from, to) = (
            last.checkpoint().head().size,
            beyond.checkpoint().head().size,
        );
        let mut proof = later.prove_consistency(from, to).unwrap();
        proof.path[0] = Hash::from_bytes([0xAB; 32]);
        write(
            "corrupt.body",
            &AddCheckpoint::new(&beyond, from, Some(&proof))
                .unwrap()
                .to_bytes(),
        );
        // Bundles at the last checkpoint.
        let entry = books.entry_by_key("interop-17").unwrap();
        let id = entry.id(&acme());
        write(
            "bundles/entry.tlog-proof",
            &books.bundle_entry(id, last).unwrap().to_bytes(),
        );
        let seal = books.bundle_record(
            books
                .leaves()
                .find(|leaf| leaf.bytes[0] == 8)
                .unwrap()
                .position,
            last,
        );
        write("bundles/seal.tlog-proof", &seal.unwrap().to_bytes());
        for (name, currency) in [("eur", EUR), ("usd", USD)] {
            let bundle = books
                .bundle_balance("Assets:Cash", currency, Layer::Settled, "2026-02", last)
                .unwrap();
            write(
                &format!("bundles/balance-{name}.tlog-proof"),
                &bundle.to_bytes(),
            );
        }
        // Policies the crate renders.
        let witness =
            doubleentry::testing::TestWitness::new("witness.example/w1", [9; 32]).unwrap();
        write(
            "policies/log-only.txt",
            Policy::log_only(key.vkey()).to_string().as_bytes(),
        );
        let one = Policy::k_of_n(key.vkey(), 1, [("w1".to_owned(), witness.vkey())]).unwrap();
        write("policies/one-of-one.txt", one.to_string().as_bytes());
        // Tiles and entry bundles at the last checkpoint.
        let size = last.checkpoint().head().size;
        for tile in TileId::changed(0, size) {
            write(&tile.path(), &books.tile(tile).unwrap());
            if let Some(entries) = tile.entry_bundle() {
                write(&entries.path(), &books.tile_entries(entries).unwrap());
            }
        }
        write("checkpoint", &last.to_bytes());
    }

    /// The stock witness's replies to the emitted requests verify.
    pub(super) fn roundtrip() {
        let (_, checkpoints, key) = ledger();
        let witness: Vkey = std::fs::read_to_string(interop_dir().join("witness.txt"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for (i, signed) in checkpoints.iter().enumerate() {
            let reply = std::fs::read(interop_dir().join(format!("replies/{i:02}.body"))).unwrap();
            let cosignature = WitnessReply::read(200, None, &reply, signed, &witness).unwrap();
            let cosigned = signed.with_cosignature(&cosignature).unwrap();
            assert_eq!(
                cosigned
                    .verify(&[key.vkey(), witness.clone()])
                    .unwrap()
                    .len(),
                2
            );
        }
    }
}

/// Writes crate-made artefacts to `target/interop/` for the Go checker.
#[cfg(all(feature = "witness", feature = "testing"))]
#[test]
#[ignore = "run by `just interop` before the Go checker"]
fn emit() {
    emitted::emit();
}

/// Verifies the cosignatures the stock witness returned to the checker.
#[cfg(all(feature = "witness", feature = "testing"))]
#[test]
#[ignore = "run by `just interop` after the Go checker"]
fn witness_roundtrip() {
    emitted::roundtrip();
}
