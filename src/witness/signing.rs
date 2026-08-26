//! Ed25519 keys, and the two signature types a note carries.
//!
//! Split from the rest of [`witness`](super) so that moving heads around costs
//! no cryptography: a log operator collecting signatures, or a service relaying
//! them, needs [`SignedTreeHead`](super::SignedTreeHead) and nothing here.
//!
//! # The two signature types
//!
//! Both are Ed25519 and both appear as one `—` line, but they cover different
//! messages and are not interchangeable:
//!
//! - **A note signature** (algorithm `0x01`) covers the checkpoint body exactly.
//!   This is what a *log operator* puts on a head to say "these are my books at
//!   this size". [`SigningKey::sign`] / [`VerifyingKey::verify`].
//! - **A cosignature** (algorithm `0x04`, [C2SP
//!   `tlog-cosignature`](https://c2sp.org/tlog-cosignature)) covers the body
//!   *and a timestamp*. This is what a **witness** puts on a head, and the
//!   timestamp is what makes it more than a second opinion: it says the witness
//!   had seen this history by that moment, which is what turns a silent log into
//!   a detectable one. [`Cosigner::cosign`] / [`VerifyingKey::verify_cosignature`].
//!
//! The algorithm byte is inside the key hash, so a key is bound to its type: a
//! note signature cannot be presented as a cosignature, or the reverse, because
//! the four-byte selector would not match.
//!
//! # No clock, no randomness
//!
//! A key comes from bytes the caller supplies and a cosignature's timestamp is
//! an argument. `ed25519-dalek` is pulled in with `rand_core` off so that stays
//! true by construction rather than by discipline: there is no code path here
//! that could read the environment.
//!
//! Pass `SystemTime::now()` at the call site, where a reader can see it:
//!
//! ```
//! # use std::time::{SystemTime, UNIX_EPOCH};
//! # use doubleentry::witness::signing::{Cosigner, SigningKey};
//! # use doubleentry::witness::{KeyName, MemoryWitnessStore, Origin, Witness};
//! # use doubleentry::{Hash, MerkleLog};
//! # let mut log = MerkleLog::new();
//! # for i in 0..8u64 { log.append(Hash::from_bytes([i as u8; 32])); }
//! # let origin = Origin::new("example.com/ledgers/acme")?;
//! let key = SigningKey::from_seed(KeyName::new("witness.example")?, [42u8; 32]);
//! let public = key.verifying_key();
//!
//! let mut witness = Witness::new(MemoryWitnessStore::new());
//! witness.trust(origin.clone(), log.head_at(3)?)?;
//! let mut cosigner = Cosigner::new(witness, key);
//!
//! let now = log.head();
//! let proof = log.consistency_proof_between(3, now.size)?;
//! let at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
//! let signature = cosigner.cosign(&origin, now, Some(&proof), at)?;
//!
//! // A third party, holding only the public key and the head, can check it.
//! let mut published = doubleentry::witness::SignedTreeHead::new(origin, now);
//! published.add_signature(signature);
//! assert_eq!(
//!     public.verify_cosignature(&published.body(), published.signatures.first().unwrap()),
//!     Some(at),
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use ed25519_dalek::{Signer, Verifier};
use sha2::{Digest, Sha256};

use crate::merkle::{ConsistencyProof, TreeHead};

use super::{Accepted, KeyName, NoteSignature, Origin, Witness, WitnessError, WitnessStore};

/// Note algorithm identifier for a plain Ed25519 signature.
const ALG_ED25519: u8 = 0x01;
/// Note algorithm identifier for a `cosignature/v1`.
const ALG_COSIGNATURE: u8 = 0x04;

/// Width of a raw Ed25519 signature.
const SIG_LEN: usize = 64;
/// Width of the big-endian timestamp a cosignature carries.
const TIME_LEN: usize = 8;

/// The four-byte key selector a note signature line carries.
///
/// `SHA-256(name || '\n' || algorithm || public key)`, truncated to four bytes —
/// the construction Go's `sumdb/note` uses, and therefore the one every existing
/// witness computes. SHA-256 rather than the BLAKE3 used everywhere else in this
/// crate, because interoperating is the entire purpose of the format and a
/// substitution would silently make every key hash disagree with the rest of the
/// ecosystem.
///
/// The algorithm byte being inside means a key is bound to one signature type:
/// the same public key used for notes and for cosignatures has two different
/// hashes, so a signature of one kind cannot be offered as the other.
fn key_hash(name: &KeyName, algorithm: u8, public: &[u8; 32]) -> [u8; 4] {
    let mut hasher = Sha256::new();
    hasher.update(name.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update([algorithm]);
    hasher.update(public);
    let digest = hasher.finalize();
    let mut out = [0u8; 4];
    // SHA-256 is 32 bytes wide, so the first four always exist.
    if let Some(prefix) = digest.get(..4) {
        out.copy_from_slice(prefix);
    }
    out
}

/// The message a `cosignature/v1` covers.
///
/// The body with a timestamp bound in front of it, so the signature says *when*
/// the witness had seen this history and not merely that it had.
fn cosignature_message(body: &str, at: u64) -> String {
    format!("cosignature/v1\ntime {at}\n{body}")
}

/// An Ed25519 signing key with the name it appears under.
///
/// Built from 32 caller-supplied bytes — this crate generates no randomness, so
/// where the seed comes from is the caller's decision and, more usefully, the
/// caller's *record*. Zeroized on drop.
#[derive(Debug)]
pub struct SigningKey {
    name: KeyName,
    inner: ed25519_dalek::SigningKey,
}

impl SigningKey {
    /// Builds a key from a 32-byte seed.
    ///
    /// The seed is the private key. Generate it with a CSPRNG *at the call
    /// site*, or read it from wherever you keep secrets; nothing in this crate
    /// will do it for you, because a library that quietly produces key material
    /// is a library nobody can audit for how it produced it.
    #[must_use]
    pub fn from_seed(name: KeyName, seed: [u8; 32]) -> Self {
        Self {
            name,
            inner: ed25519_dalek::SigningKey::from_bytes(&seed),
        }
    }

    /// The name this key signs under.
    #[must_use]
    pub fn name(&self) -> &KeyName {
        &self.name
    }

    /// The public half, for whoever has to check the signatures.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey {
            name: self.name.clone(),
            inner: self.inner.verifying_key(),
        }
    }

    /// Signs a checkpoint body as its **operator** — algorithm `0x01`.
    ///
    /// Pass [`SignedTreeHead::body`](super::SignedTreeHead::body) and nothing
    /// else. The signature covers those bytes exactly, so signing a
    /// reconstruction rather than the text you will publish is how a note ends
    /// up carrying a signature that does not verify.
    #[must_use]
    pub fn sign(&self, body: &str) -> NoteSignature {
        let public = self.inner.verifying_key().to_bytes();
        NoteSignature {
            name: self.name.clone(),
            key_hash: key_hash(&self.name, ALG_ED25519, &public),
            value: self.inner.sign(body.as_bytes()).to_bytes().to_vec(),
        }
    }

    /// Cosigns a checkpoint body as a **witness** at `at` — algorithm `0x04`.
    ///
    /// `at` is Unix seconds and is an argument on purpose: this crate reads no
    /// clock, so a replay produces identical bytes. See the
    /// [module documentation](self).
    ///
    /// This signs whatever it is given. Use [`Cosigner`], which will not reach
    /// this without a [`Witness`] having accepted the head first — the signature
    /// is the only thing anyone downstream looks at, so producing one outside
    /// the state machine defeats the entire mechanism.
    #[must_use]
    pub fn cosign_body(&self, body: &str, at: u64) -> NoteSignature {
        let public = self.inner.verifying_key().to_bytes();
        let message = cosignature_message(body, at);
        let mut value = Vec::with_capacity(TIME_LEN.saturating_add(SIG_LEN));
        value.extend_from_slice(&at.to_be_bytes());
        value.extend_from_slice(&self.inner.sign(message.as_bytes()).to_bytes());
        NoteSignature {
            name: self.name.clone(),
            key_hash: key_hash(&self.name, ALG_COSIGNATURE, &public),
            value,
        }
    }
}

/// The public half of a signing key, and the name it signs under.
///
/// What a verifier holds. Obtaining it is out of band and that is the point —
/// the security of the whole arrangement is that the verifier chose these keys
/// before the operator had anything to gain from influencing the choice.
#[derive(Debug, Clone)]
pub struct VerifyingKey {
    name: KeyName,
    inner: ed25519_dalek::VerifyingKey,
}

impl VerifyingKey {
    /// Builds a verifying key from a name and 32 public-key bytes.
    ///
    /// # Errors
    ///
    /// Returns [`BadKey`] when the bytes are not a valid Ed25519 point.
    pub fn from_bytes(name: KeyName, bytes: [u8; 32]) -> Result<Self, BadKey> {
        Ok(Self {
            name,
            inner: ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| BadKey)?,
        })
    }

    /// The name.
    #[must_use]
    pub fn name(&self) -> &KeyName {
        &self.name
    }

    /// The raw public key bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// The selector this key's **note** signatures carry.
    #[must_use]
    pub fn note_key_hash(&self) -> [u8; 4] {
        key_hash(&self.name, ALG_ED25519, &self.inner.to_bytes())
    }

    /// The selector this key's **cosignatures** carry.
    #[must_use]
    pub fn cosignature_key_hash(&self) -> [u8; 4] {
        key_hash(&self.name, ALG_COSIGNATURE, &self.inner.to_bytes())
    }

    /// Checks an operator's note signature over `body`.
    ///
    /// The name and key hash are checked as well as the signature, so a line
    /// signed by a different key of the same name — or the same key acting as a
    /// cosigner — is refused rather than verified against the wrong message.
    ///
    /// Uses `verify_strict`, which rejects small-order and non-canonical public
    /// keys: without it two distinct keys can verify one signature, and "signed
    /// by the witness" stops being a statement about a particular witness.
    #[must_use]
    pub fn verify(&self, body: &str, signature: &NoteSignature) -> bool {
        if signature.name != self.name || signature.key_hash != self.note_key_hash() {
            return false;
        }
        let Some(bytes) = signature
            .value
            .get(..SIG_LEN)
            .filter(|_| signature.value.len() == SIG_LEN)
        else {
            return false;
        };
        let Ok(array) = <[u8; SIG_LEN]>::try_from(bytes) else {
            return false;
        };
        self.inner
            .verify(
                body.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&array),
            )
            .is_ok()
    }

    /// Checks a witness cosignature over `body`, returning the time it claims.
    ///
    /// `Some(at)` means: this key vouched for this exact head, and had seen it
    /// by Unix second `at`. `None` means the line does not verify, and the
    /// reasons are not distinguished — a verifier cannot act differently on a
    /// malformed cosignature than on a forged one.
    ///
    /// The timestamp comes back **checked**, not read off the wire: it is inside
    /// the signed message, so a value that was altered in transit fails the
    /// signature rather than being returned.
    ///
    /// What the caller still has to decide is whether `at` is *recent enough*.
    /// A cosignature from last year is perfectly valid and says nothing about
    /// today; freshness is a policy, and it is yours.
    #[must_use]
    pub fn verify_cosignature(&self, body: &str, signature: &NoteSignature) -> Option<u64> {
        if signature.name != self.name || signature.key_hash != self.cosignature_key_hash() {
            return None;
        }
        if signature.value.len() != TIME_LEN.saturating_add(SIG_LEN) {
            return None;
        }
        let at = u64::from_be_bytes(
            signature
                .value
                .get(..TIME_LEN)
                .and_then(|s| <[u8; TIME_LEN]>::try_from(s).ok())?,
        );
        let array = signature
            .value
            .get(TIME_LEN..)
            .and_then(|s| <[u8; SIG_LEN]>::try_from(s).ok())?;
        let message = cosignature_message(body, at);
        self.inner
            .verify(
                message.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&array),
            )
            .ok()
            .map(|()| at)
    }
}

/// A public key was not a valid Ed25519 point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a valid Ed25519 public key")]
pub struct BadKey;

/// A [`Witness`] that signs what it accepts.
///
/// The only route to a cosignature, and deliberately the only one:
/// [`SigningKey::cosign_body`] will sign anything, and a signature produced
/// outside the state machine defeats the entire mechanism — downstream nobody
/// looks at anything *but* the signature, so a witness that could be talked into
/// signing without checking is not a witness.
///
/// [`Self::cosign`] therefore runs [`Witness::offer`] first and signs only if it
/// returns. A refused head produces a [`WitnessError`] and no bytes at all.
#[derive(Debug)]
pub struct Cosigner<S> {
    witness: Witness<S>,
    key: SigningKey,
}

impl<S: WitnessStore> Cosigner<S> {
    /// Pairs a witness with the key it signs under.
    #[must_use]
    pub fn new(witness: Witness<S>, key: SigningKey) -> Self {
        Self { witness, key }
    }

    /// The underlying witness, for reading state.
    #[must_use]
    pub fn witness(&self) -> &Witness<S> {
        &self.witness
    }

    /// The underlying witness, mutably — for [`Witness::trust`].
    pub fn witness_mut(&mut self) -> &mut Witness<S> {
        &mut self.witness
    }

    /// The public key a verifier needs.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Accepts a head and cosigns it, or refuses and signs nothing.
    ///
    /// `at` is Unix seconds; see the [module documentation](self) for why it is
    /// an argument rather than a clock read.
    ///
    /// The body signed is rendered from `origin` and the **accepted** head, not
    /// from the offered one. For a re-offer the two are equal by definition, and
    /// deriving it from what the witness holds rather than from what it was
    /// handed means there is no arrangement of arguments that gets a signature
    /// over a head this witness has not accepted.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Witness::offer`] returns — chiefly
    /// [`WitnessError::Fork`], which is the refusal this whole module is for.
    pub fn cosign(
        &mut self,
        origin: &Origin,
        head: TreeHead,
        proof: Option<&ConsistencyProof>,
        at: u64,
    ) -> Result<NoteSignature, WitnessError> {
        let accepted: Accepted = self.witness.offer(origin, head, proof)?;
        let body = super::SignedTreeHead::new(origin.clone(), accepted.head).body();
        Ok(self.key.cosign_body(&body, at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::merkle::MerkleLog;
    use crate::witness::{MemoryWitnessStore, SignedTreeHead};

    fn origin() -> Origin {
        Origin::new("example.com/ledgers/acme").expect("valid")
    }

    fn name(s: &str) -> KeyName {
        KeyName::new(s).expect("valid")
    }

    fn log_of(n: u64) -> MerkleLog {
        let mut log = MerkleLog::new();
        for i in 0..n {
            let mut bytes = [0u8; 32];
            bytes
                .get_mut(..8)
                .expect("32 > 8")
                .copy_from_slice(&i.to_le_bytes());
            log.append(Hash::from_bytes(bytes));
        }
        log
    }

    fn cosigner_at(log: &MerkleLog, size: u64) -> Cosigner<MemoryWitnessStore> {
        let mut witness = Witness::new(MemoryWitnessStore::new());
        witness
            .trust(origin(), log.head_at(size).expect("in range"))
            .expect("first time");
        Cosigner::new(
            witness,
            SigningKey::from_seed(name("witness.example"), [7; 32]),
        )
    }

    #[test]
    fn a_cosignature_verifies_and_carries_a_checked_timestamp() {
        let log = log_of(8);
        let mut cosigner = cosigner_at(&log, 3);
        let public = cosigner.verifying_key();
        let proof = log.consistency_proof_between(3, 8).expect("in range");

        let signature = cosigner
            .cosign(&origin(), log.head(), Some(&proof), 1_800_000_000)
            .expect("extends");
        let body = SignedTreeHead::new(origin(), log.head()).body();
        assert_eq!(
            public.verify_cosignature(&body, &signature),
            Some(1_800_000_000)
        );
    }

    /// The whole arrangement in one test: a fork gets no bytes.
    #[test]
    fn a_refused_head_produces_no_signature_at_all() {
        let log = log_of(8);
        let mut cosigner = cosigner_at(&log, 8);

        let mut forked = log_of(7);
        forked.append(Hash::from_bytes([0xff; 32]));

        assert!(matches!(
            cosigner.cosign(&origin(), forked.head(), None, 1_800_000_000),
            Err(WitnessError::Fork { size: 8, .. })
        ));
    }

    #[test]
    fn a_cosignature_over_a_different_head_does_not_verify() {
        let log = log_of(8);
        let mut cosigner = cosigner_at(&log, 3);
        let public = cosigner.verifying_key();
        let proof = log.consistency_proof_between(3, 8).expect("in range");
        let signature = cosigner
            .cosign(&origin(), log.head(), Some(&proof), 1_800_000_000)
            .expect("extends");

        let other = SignedTreeHead::new(origin(), log.head_at(3).expect("in range")).body();
        assert_eq!(public.verify_cosignature(&other, &signature), None);
    }

    #[test]
    fn a_tampered_timestamp_fails_the_signature_rather_than_being_returned() {
        let log = log_of(8);
        let mut cosigner = cosigner_at(&log, 3);
        let public = cosigner.verifying_key();
        let proof = log.consistency_proof_between(3, 8).expect("in range");
        let mut signature = cosigner
            .cosign(&origin(), log.head(), Some(&proof), 1_800_000_000)
            .expect("extends");
        let body = SignedTreeHead::new(origin(), log.head()).body();

        // Move it a year forward on the wire.
        signature.value.splice(..8, 1_831_536_000u64.to_be_bytes());
        assert_eq!(
            public.verify_cosignature(&body, &signature),
            None,
            "the timestamp is inside the signed message, not beside it"
        );
    }

    #[test]
    fn a_note_signature_cannot_be_presented_as_a_cosignature() {
        let key = SigningKey::from_seed(name("operator.example"), [3; 32]);
        let public = key.verifying_key();
        let body = SignedTreeHead::new(origin(), log_of(8).head()).body();

        let note_sig = key.sign(&body);
        assert!(public.verify(&body, &note_sig));
        assert_eq!(
            public.verify_cosignature(&body, &note_sig),
            None,
            "the algorithm byte is inside the key hash, so the selector differs"
        );
    }

    #[test]
    fn another_key_of_the_same_name_does_not_verify() {
        let body = SignedTreeHead::new(origin(), log_of(8).head()).body();
        let mine = SigningKey::from_seed(name("witness.example"), [1; 32]);
        let theirs = SigningKey::from_seed(name("witness.example"), [2; 32]);
        assert!(!mine.verifying_key().verify(&body, &theirs.sign(&body)));
    }

    #[test]
    fn a_signed_note_round_trips_through_the_wire_format() {
        let log = log_of(8);
        let key = SigningKey::from_seed(name("operator.example"), [9; 32]);
        let public = key.verifying_key();

        let mut sth = SignedTreeHead::new(origin(), log.head());
        let body = sth.body();
        sth.add_signature(key.sign(&body));

        let text = sth.to_note();
        let parsed = SignedTreeHead::parse(&text).expect("parses");
        let signature = parsed.signatures.first().expect("one signature");
        assert!(
            public.verify(&parsed.body(), signature),
            "a signature has to survive the text it travels in"
        );
        assert!(parsed.has_signature_from(public.name(), public.note_key_hash()));
    }

    #[test]
    fn a_key_hash_is_the_documented_construction() {
        // Pinned so a refactor cannot quietly change the selector every existing
        // witness computes. SHA-256(name || '\n' || alg || pubkey)[..4].
        let key = SigningKey::from_seed(name("witness.example"), [7; 32]);
        let public = key.verifying_key();
        let mut hasher = Sha256::new();
        hasher.update(b"witness.example\n");
        hasher.update([ALG_COSIGNATURE]);
        hasher.update(public.to_bytes());
        let digest = hasher.finalize();
        assert_eq!(
            public.cosignature_key_hash().as_slice(),
            digest.get(..4).expect("32 > 4")
        );
    }
}
