//! Witness cosigning — an independent check on a **split view**.
//!
//! Inclusion and consistency proofs answer everything an auditor can ask *about
//! the history they were shown*. They cannot answer whether it is the history
//! everybody else was shown, and no proof can: each party only ever sees one
//! view. An operator who can serve two heads can serve head `H₁` to the tax
//! authority and `H₂` to the bank, with a complete, internally consistent log
//! behind each, and hand both parties proofs that verify perfectly.
//!
//! Every other guarantee in this crate is relative to a head, and the head is
//! what was forked.
//!
//! # What a witness does
//!
//! A witness is a party that is not the operator. It remembers the last head it
//! accepted for a log, and only ever moves forward:
//!
//! 1. It is told out of band which log to track and from which head —
//!    [`Witness::trust`]. There is no trust-on-first-use: an operator who could
//!    introduce a witness to a log could introduce it to the fork.
//! 2. A later head must come with a [`ConsistencyProof`] from the head it holds.
//!    No proof, or a bad one, and the head is refused.
//! 3. **A second history at a size it has already accepted is refused** —
//!    [`WitnessError::Fork`], and it never signs. The fork is not something the
//!    witness reports afterwards; it is something it declines to take part in.
//! 4. Having accepted, it cosigns — see [`Cosigner`].
//!
//! A verifier then refuses any head not cosigned by the witnesses *it* chose. To
//! show that verifier a forked history you must compromise those witnesses too,
//! and they are deliberately not yours.
//!
//! A witness never sees an entry, a balance, an account or a period — only an
//! origin, a size and a root. That is what lets it be somebody with no business
//! seeing your books. It does not make anything *available*, either: it stops a
//! forked history being believed, not a log being withheld.
//!
//! # Interoperating, and no HTTP
//!
//! The wire format is [C2SP `signed-note`](https://c2sp.org/signed-note) around a
//! [`tlog-checkpoint`](https://c2sp.org/tlog-checkpoint) body, cosigned per
//! [`tlog-cosignature`](https://c2sp.org/tlog-cosignature) — what Certificate
//! Transparency, the Go checksum database, Sigsum and sigstore speak. A witness
//! is only worth having if it is somebody else's, and a bespoke format could
//! only be cosigned by software this crate ships.
//!
//! [C2SP `tlog-witness`](https://c2sp.org/tlog-witness) puts this behind
//! `POST /add-checkpoint`. That transport is not here: it is a request body, a
//! response body and four status codes, and an async runtime plus a TLS stack is
//! a large thing to inflict on every user who never touches a witness.
//! [`AddCheckpoint`] gives you the bytes.
//!
//! ```
//! # use doubleentry::witness::{AddCheckpoint, MemoryWitnessStore, Origin, SignedTreeHead, Witness};
//! # use doubleentry::{Hash, MerkleLog, TreeHead};
//! # let mut log = MerkleLog::new();
//! # for i in 0..8u64 { log.append(Hash::from_bytes([i as u8; 32])); }
//! # let origin = Origin::new("example.com/ledgers/acme")?;
//! // A head the witness was told to trust, out of band.
//! let earlier = log.head_at(3)?;
//! let mut witness = Witness::new(MemoryWitnessStore::new());
//! witness.trust(origin.clone(), earlier)?;
//!
//! // Later: the log has grown, and the operator has to prove it extends.
//! let now = log.head();
//! let proof = log.consistency_proof_between(earlier.size, now.size)?;
//! let request = AddCheckpoint::new(SignedTreeHead::new(origin.clone(), now), proof);
//!
//! let accepted = witness.offer(&origin, now, Some(request.proof()))?;
//! assert!(accepted.advanced);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Determinism
//!
//! A cosignature covers a timestamp, and this crate reads no clock, so the
//! timestamp is an argument — [`Cosigner::cosign`] takes it. Pass
//! `SystemTime::now()` at the call site, where it is visible.

use std::collections::BTreeMap;

use crate::hash::Hash;
use crate::merkle::{ConsistencyProof, TreeHead};

mod base64;
mod note;

#[cfg(feature = "witness")]
#[cfg_attr(docsrs, doc(cfg(feature = "witness")))]
pub mod signing;

pub use note::{KeyName, NoteError, NoteSignature, Origin, SignedTreeHead};

#[cfg(feature = "witness")]
pub use signing::Cosigner;

/// Why a witness refused a head.
///
/// Every variant is a refusal to sign. A witness that answered any of these with
/// a signature would be worth exactly nothing, because the signature is the only
/// thing anyone downstream looks at.
///
/// The mapping to [C2SP `tlog-witness`](https://c2sp.org/tlog-witness) HTTP
/// status codes is given per variant, for a caller putting this behind the
/// standard endpoint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WitnessError {
    /// The witness does not track this log. *(404)*
    ///
    /// There is no trust-on-first-use, and that is the point. A witness that
    /// adopted whatever head it was first shown could be introduced to the fork
    /// as easily as to the log, by the same operator, and would then cosign the
    /// fork with complete sincerity. Which log to watch, and from which head, is
    /// a decision made out of band — see [`Witness::trust`].
    #[error("this witness does not track {origin}")]
    Untracked {
        /// The log offered.
        origin: Origin,
    },

    /// A **second history at a size already accepted**. *(409)*
    ///
    /// The attack this module exists for. A Merkle root determines its own tree,
    /// so a second root at one size is not a stale read or a race — it is a
    /// second history, and somebody is being shown books that are not these
    /// books.
    ///
    /// The refusal is permanent: no later head could repair the claim, because
    /// this witness has already vouched for the other one. Resolving it is an
    /// operational matter, and a loud one.
    #[error(
        "{origin} offered a second history at size {size}: this witness has \
         already signed {known}, and was shown {offered}"
    )]
    Fork {
        /// The log.
        origin: Origin,
        /// The size both histories claim.
        size: u64,
        /// The root this witness accepted, and will not contradict.
        known: Hash,
        /// The root it was just shown.
        offered: Hash,
    },

    /// The head offered is **smaller** than the one held. *(409)*
    ///
    /// A log does not shrink. Usually a stale replica or a reordered request;
    /// occasionally a restore from a backup taken before entries this witness
    /// has already vouched for, which is a real incident.
    #[error("{origin} offered {offered} entries, behind the {known} this witness holds")]
    Shrunk {
        /// The log.
        origin: Origin,
        /// The size this witness holds.
        known: u64,
        /// The size offered.
        offered: u64,
    },

    /// A later head arrived without a consistency proof. *(422)*
    #[error("{origin} offered {to} entries with no consistency proof from {from}")]
    MissingProof {
        /// The log.
        origin: Origin,
        /// The size the proof had to start at.
        from: u64,
        /// The size offered.
        to: u64,
    },

    /// The consistency proof did not verify. *(422)*
    ///
    /// The offered log is not an extension of the one this witness holds. Same
    /// severity as [`Fork`](Self::Fork) and usually the same cause — it is what a
    /// fork looks like when it is discovered at a *later* size rather than at the
    /// same one.
    #[error("{origin} offered {to} entries that do not extend the {from} this witness holds")]
    BadProof {
        /// The log.
        origin: Origin,
        /// The size held.
        from: u64,
        /// The size offered.
        to: u64,
    },

    /// [`Witness::trust`] was called for a log already being tracked.
    ///
    /// Refused rather than treated as a re-initialisation. Re-pointing a witness
    /// at a new starting head would discard the state that makes it a witness —
    /// it is exactly the move an operator needs to make a fork acceptable, and it
    /// must not be reachable by calling the ordinary setup function twice.
    #[error("this witness already tracks {origin} at {size} entries")]
    AlreadyTracked {
        /// The log.
        origin: Origin,
        /// The size already held.
        size: u64,
    },

    /// The signed head's origin is not the one it was offered under.
    #[error("note names {found}, not {expected}")]
    OriginMismatch {
        /// The origin the caller asked about.
        expected: Origin,
        /// The origin the note carries.
        found: Origin,
    },
}

/// A head a witness accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    /// The head now held for the log.
    pub head: TreeHead,
    /// False when this was a re-offer of the head already held.
    ///
    /// Re-offering is not an error — an at-least-once delivery path will do it,
    /// and a witness that refused would be unusable behind one. It is reported
    /// so a caller can tell "you moved me forward" from "I was already here",
    /// which is the difference between a log that is growing and one that has
    /// stalled.
    pub advanced: bool,
}

/// Where a witness keeps the last head it accepted per log.
///
/// One row per log — an origin, a size and a root. That is the whole state, and
/// its smallness is the point: a witness is meant to be cheap enough that
/// somebody with no stake in your books will run one.
///
/// **Durability is the security property.** A witness whose state does not
/// survive a restart is a witness that can be made to forget a fork by being
/// restarted, which is not a difficult thing for an operator to arrange. Anything
/// that persists will do — a file, a table, a row in the ledger's own database —
/// but it has to persist.
///
/// [`MemoryWitnessStore`] does not, and says so.
pub trait WitnessStore {
    /// The head last accepted for `origin`, if it is tracked.
    fn head(&self, origin: &Origin) -> Option<TreeHead>;

    /// Records `head` as the one now accepted for `origin`.
    ///
    /// Called only after [`Witness`] has decided the move is legal, so an
    /// implementation stores rather than judges. It must be durable **before it
    /// returns**: a witness that signs a head it has not yet committed can be
    /// restarted into signing a different one at the same size.
    fn store(&mut self, origin: &Origin, head: TreeHead);

    /// Every tracked log and its head, in origin order.
    fn tracked(&self) -> Vec<(Origin, TreeHead)>;
}

/// A [`WitnessStore`] that keeps state in memory.
///
/// For tests, and for a witness whose state is reloaded from somewhere else at
/// start-up. **Not for a witness that matters on its own**: a restart forgets
/// every head, and a witness with no memory of what it signed will cosign a fork
/// without hesitation. See [`WitnessStore`].
#[derive(Debug, Clone, Default)]
pub struct MemoryWitnessStore {
    heads: BTreeMap<Origin, TreeHead>,
}

impl MemoryWitnessStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds a store from persisted rows.
    ///
    /// The reload path: read your table, hand it here, and the witness resumes
    /// with the state it had.
    pub fn from_heads(heads: impl IntoIterator<Item = (Origin, TreeHead)>) -> Self {
        Self {
            heads: heads.into_iter().collect(),
        }
    }
}

impl WitnessStore for MemoryWitnessStore {
    fn head(&self, origin: &Origin) -> Option<TreeHead> {
        self.heads.get(origin).copied()
    }

    fn store(&mut self, origin: &Origin, head: TreeHead) {
        self.heads.insert(origin.clone(), head);
    }

    fn tracked(&self) -> Vec<(Origin, TreeHead)> {
        self.heads.iter().map(|(o, h)| (o.clone(), *h)).collect()
    }
}

/// An independent party that will vouch for one history and no other.
///
/// See the [module documentation](self) for what this is for. The rules, in
/// full, are [`Witness::offer`].
///
/// This type decides; it does not sign. Signing needs a key and therefore
/// cryptography, which lives behind the `witness` feature — wrap this in a
/// [`Cosigner`] to get a signature out.
#[derive(Debug, Clone)]
pub struct Witness<S> {
    store: S,
}

impl<S: WitnessStore> Witness<S> {
    /// Creates a witness over `store`.
    #[must_use]
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Begins tracking `origin`, starting from `head`.
    ///
    /// The out-of-band step, and it has to be out of band. Everything else this
    /// type does is relative to a head it already holds, so the *first* head is
    /// the one thing it cannot check — it is the root of the witness's trust in
    /// the same way a published head is the root of an auditor's. Get it from
    /// the same place you would get a certificate fingerprint: a contract, a
    /// printed statement, a channel the log operator does not control.
    ///
    /// Starting from a head with at least one entry is strongly preferable.
    /// From size 0 the first offer cannot carry a consistency proof — every log
    /// extends the empty one, so this crate refuses to build such a proof at all
    /// (see [`ConsistencyProof::verify`]) — and the witness has no choice but to
    /// take the first non-empty head on trust.
    ///
    /// # Errors
    ///
    /// Returns [`WitnessError::AlreadyTracked`] if the log is already tracked.
    /// Re-pointing an existing witness at a new starting head is exactly how an
    /// operator would launder a fork, so it is not something the setup function
    /// will do by being called twice.
    pub fn trust(&mut self, origin: Origin, head: TreeHead) -> Result<(), WitnessError> {
        if let Some(existing) = self.store.head(&origin) {
            return Err(WitnessError::AlreadyTracked {
                origin,
                size: existing.size,
            });
        }
        self.store.store(&origin, head);
        Ok(())
    }

    /// The head last accepted for `origin`.
    #[must_use]
    pub fn head(&self, origin: &Origin) -> Option<TreeHead> {
        self.store.head(origin)
    }

    /// Every log this witness tracks, and where it has got to.
    #[must_use]
    pub fn tracked(&self) -> Vec<(Origin, TreeHead)> {
        self.store.tracked()
    }

    /// The store, for a caller that needs to persist or inspect it.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Offers a head, and decides whether this witness will stand behind it.
    ///
    /// The rules, in the order they are applied:
    ///
    /// | Situation | Answer |
    /// |---|---|
    /// | Log not tracked | [`Untracked`](WitnessError::Untracked) |
    /// | Same size, same root | accepted, `advanced: false` |
    /// | **Same size, different root** | [`Fork`](WitnessError::Fork) — refused, permanently |
    /// | Smaller size | [`Shrunk`](WitnessError::Shrunk) |
    /// | Larger size, no proof | [`MissingProof`](WitnessError::MissingProof) |
    /// | Larger size, proof does not verify | [`BadProof`](WitnessError::BadProof) |
    /// | Larger size, proof verifies | accepted, `advanced: true` |
    ///
    /// The third row is the one worth running a witness for — see
    /// [`WitnessError::Fork`]; the rest is hygiene.
    ///
    /// State advances **before** anything is signed, so a crash between the two
    /// loses a signature rather than leaving one behind for a head the witness
    /// has no memory of. A `proof` is accepted and ignored in the equal-size
    /// case, because an at-least-once caller will resend one.
    ///
    /// # Errors
    ///
    /// Returns the [`WitnessError`] named in the table.
    pub fn offer(
        &mut self,
        origin: &Origin,
        head: TreeHead,
        proof: Option<&ConsistencyProof>,
    ) -> Result<Accepted, WitnessError> {
        let known = self
            .store
            .head(origin)
            .ok_or_else(|| WitnessError::Untracked {
                origin: origin.clone(),
            })?;

        if head.size == known.size {
            // A root determines its own tree, so a second root at one size is a
            // second history — not a race, not a stale read. This witness has
            // already vouched for the other one and will not contradict itself.
            if head.root != known.root {
                return Err(WitnessError::Fork {
                    origin: origin.clone(),
                    size: head.size,
                    known: known.root,
                    offered: head.root,
                });
            }
            return Ok(Accepted {
                head: known,
                advanced: false,
            });
        }

        if head.size < known.size {
            return Err(WitnessError::Shrunk {
                origin: origin.clone(),
                known: known.size,
                offered: head.size,
            });
        }

        // Growing. The operator has to show the new tree extends the old one,
        // and "extends" is a proof rather than an assertion.
        //
        // From an empty starting head there is nothing to prove: every log
        // extends the empty tree, and this crate refuses to build or verify such
        // a proof precisely because it would say nothing. The first non-empty
        // head is taken on trust, which is why `trust` asks for a non-empty one.
        if known.size == 0 {
            self.store.store(origin, head);
            return Ok(Accepted {
                head,
                advanced: true,
            });
        }

        let proof = proof.ok_or_else(|| WitnessError::MissingProof {
            origin: origin.clone(),
            from: known.size,
            to: head.size,
        })?;
        if !proof.verify(&known, &head) {
            return Err(WitnessError::BadProof {
                origin: origin.clone(),
                from: known.size,
                to: head.size,
            });
        }

        // Durable before signed: a crash here loses a signature, which is
        // recoverable, rather than leaving one behind for a head this witness
        // has no memory of, which is not.
        self.store.store(origin, head);
        Ok(Accepted {
            head,
            advanced: true,
        })
    }

    /// Offers a head that arrived as a note, checking the origin matches.
    ///
    /// The form a [C2SP `tlog-witness`](https://c2sp.org/tlog-witness) endpoint
    /// takes: a request carries the origin *inside* the signed body, so the
    /// endpoint has to confirm it is the one the route names rather than trusting
    /// either alone.
    ///
    /// # Errors
    ///
    /// Returns [`WitnessError::OriginMismatch`] when the note names a different
    /// log, and otherwise whatever [`Self::offer`] returns.
    pub fn offer_note(
        &mut self,
        origin: &Origin,
        note: &SignedTreeHead,
        proof: Option<&ConsistencyProof>,
    ) -> Result<Accepted, WitnessError> {
        if note.origin != *origin {
            return Err(WitnessError::OriginMismatch {
                expected: origin.clone(),
                found: note.origin.clone(),
            });
        }
        self.offer(origin, note.head, proof)
    }
}

/// A request to add a head to a witness, and the reply.
///
/// The body of [C2SP `tlog-witness`](https://c2sp.org/tlog-witness)'s
/// `POST /add-checkpoint`, as bytes. **The transport is not here** — see the
/// [module documentation](self) for why — so this is what you hand to whatever
/// HTTP client you already use:
///
/// ```
/// # use doubleentry::witness::{AddCheckpoint, Origin, SignedTreeHead};
/// # use doubleentry::{Hash, MerkleLog, TreeHead};
/// # fn post(_url: &str, _body: &[u8]) -> Vec<u8> { Vec::new() }
/// # let mut log = MerkleLog::new();
/// # for i in 0..8u64 { log.append(Hash::from_bytes([i as u8; 32])); }
/// # let origin = Origin::new("example.com/ledgers/acme")?;
/// let head = log.head();
/// let proof = log.consistency_proof_between(3, head.size)?;
/// let request = AddCheckpoint::new(SignedTreeHead::new(origin, head), proof);
///
/// let _body: Vec<u8> = request.to_body();     // POST this to /add-checkpoint
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// The body is the note, then one base64 line per consistency-proof hash — the
/// format the specification defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddCheckpoint {
    note: SignedTreeHead,
    proof: ConsistencyProof,
}

impl AddCheckpoint {
    /// Builds a request offering `note`, proven consistent by `proof`.
    #[must_use]
    pub fn new(note: SignedTreeHead, proof: ConsistencyProof) -> Self {
        Self { note, proof }
    }

    /// The head being offered.
    #[must_use]
    pub fn note(&self) -> &SignedTreeHead {
        &self.note
    }

    /// The proof that it extends the head the witness holds.
    #[must_use]
    pub fn proof(&self) -> &ConsistencyProof {
        &self.proof
    }

    /// The request body: the note, then one base64 line per proof hash.
    #[must_use]
    pub fn to_body(&self) -> Vec<u8> {
        let mut out = self.note.to_note();
        for hash in &self.proof.path {
            out.push_str(&base64::encode(hash.as_bytes()));
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Parses a request body, given the sizes the proof relates.
    ///
    /// The sizes are arguments because the wire format does not carry them: the
    /// new size is in the note, and the old size is *the witness's own state*.
    /// That is deliberate in the specification and worth preserving — a proof
    /// whose starting size came from the request could be aimed at a head the
    /// witness never accepted.
    ///
    /// # Errors
    ///
    /// Returns [`NoteError`] when the note or a proof line is malformed.
    pub fn parse_body(body: &[u8], old_size: u64) -> Result<Self, NoteError> {
        let text = core::str::from_utf8(body).map_err(|_| NoteError::MalformedRoot)?;
        let (note_text, rest) = split_after_signatures(text);
        let note = SignedTreeHead::parse(note_text)?;
        let mut path = Vec::new();
        for line in rest.split('\n').filter(|l| !l.is_empty()) {
            let bytes = base64::decode(line).ok_or(NoteError::MalformedRoot)?;
            let array: [u8; Hash::LEN] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| NoteError::MalformedRoot)?;
            path.push(Hash::from_bytes(array));
        }
        Ok(Self {
            proof: ConsistencyProof {
                old_size,
                new_size: note.head.size,
                path,
            },
            note,
        })
    }
}

/// Splits a request body into its note and the proof lines that follow it.
///
/// A note is body, blank line, then signature lines; the proof hashes begin at
/// the first line that is not a signature.
fn split_after_signatures(text: &str) -> (&str, &str) {
    let Some(separator) = text.find("\n\n") else {
        return (text, "");
    };
    // Past the blank line, signatures run until a line that is not one.
    let mut at = separator.saturating_add(2);
    for line in text.get(at..).unwrap_or_default().split_inclusive('\n') {
        if !line.starts_with('\u{2014}') {
            break;
        }
        at = at.saturating_add(line.len());
    }
    (
        text.get(..at).unwrap_or(text),
        text.get(at..).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merkle::MerkleLog;

    fn origin() -> Origin {
        Origin::new("example.com/ledgers/acme").expect("valid")
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

    fn witness_at(log: &MerkleLog, size: u64) -> Witness<MemoryWitnessStore> {
        let mut witness = Witness::new(MemoryWitnessStore::new());
        witness
            .trust(origin(), log.head_at(size).expect("in range"))
            .expect("first time");
        witness
    }

    #[test]
    fn an_untracked_log_is_refused_rather_than_adopted() {
        let log = log_of(8);
        let mut witness = Witness::new(MemoryWitnessStore::new());
        assert!(matches!(
            witness.offer(&origin(), log.head(), None),
            Err(WitnessError::Untracked { .. })
        ));
    }

    #[test]
    fn a_growing_log_is_accepted_when_it_proves_it_extends() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 3);
        let proof = log.consistency_proof_between(3, 8).expect("in range");
        let accepted = witness
            .offer(&origin(), log.head(), Some(&proof))
            .expect("extends");
        assert!(accepted.advanced);
        assert_eq!(witness.head(&origin()), Some(log.head()));
    }

    #[test]
    fn growth_without_a_proof_is_refused() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 3);
        assert!(matches!(
            witness.offer(&origin(), log.head(), None),
            Err(WitnessError::MissingProof { from: 3, to: 8, .. })
        ));
    }

    /// The rule the whole module exists for.
    #[test]
    fn a_second_history_at_the_same_size_is_refused() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 8);

        // A different history that happens to be the same length. Every proof
        // inside it verifies; it is simply not the log this witness signed.
        let mut forked = log_of(7);
        forked.append(Hash::from_bytes([0xff; 32]));
        assert_eq!(forked.len(), log.len());
        assert_ne!(forked.root(), log.root());

        let refusal = witness.offer(&origin(), forked.head(), None);
        assert!(
            matches!(refusal, Err(WitnessError::Fork { size: 8, .. })),
            "got {refusal:?}"
        );
        // And the witness has not moved: it still stands behind the first one.
        assert_eq!(witness.head(&origin()), Some(log.head()));
    }

    /// The same fork, discovered one size later. Caught by the proof instead.
    #[test]
    fn a_fork_that_has_moved_on_is_caught_by_the_proof() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 8);

        let mut forked = log_of(7);
        forked.append(Hash::from_bytes([0xff; 32]));
        forked.append(Hash::from_bytes([0xfe; 32]));

        let proof = forked.consistency_proof_between(8, 9).expect("in range");
        assert!(matches!(
            witness.offer(&origin(), forked.head(), Some(&proof)),
            Err(WitnessError::BadProof { from: 8, to: 9, .. })
        ));
    }

    #[test]
    fn re_offering_the_head_already_held_is_not_an_error() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 8);
        let accepted = witness.offer(&origin(), log.head(), None).expect("same");
        assert!(
            !accepted.advanced,
            "an at-least-once caller resends; refusing would make the witness unusable"
        );
    }

    #[test]
    fn a_shrinking_log_is_refused() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 8);
        assert!(matches!(
            witness.offer(&origin(), log.head_at(4).expect("in range"), None),
            Err(WitnessError::Shrunk {
                known: 8,
                offered: 4,
                ..
            })
        ));
    }

    #[test]
    fn a_witness_cannot_be_re_pointed_by_calling_trust_again() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 8);
        let mut forked = log_of(7);
        forked.append(Hash::from_bytes([0xff; 32]));
        assert!(
            matches!(
                witness.trust(origin(), forked.head()),
                Err(WitnessError::AlreadyTracked { size: 8, .. })
            ),
            "re-pointing is how an operator would launder a fork"
        );
    }

    #[test]
    fn from_an_empty_head_the_first_growth_is_taken_on_trust() {
        let log = log_of(8);
        let mut witness = Witness::new(MemoryWitnessStore::new());
        witness
            .trust(origin(), log.head_at(0).expect("in range"))
            .expect("first time");
        // No proof is possible: every log extends the empty tree.
        let accepted = witness.offer(&origin(), log.head(), None).expect("accepts");
        assert!(accepted.advanced);
        // But from here on it is a real witness.
        let mut forked = log_of(7);
        forked.append(Hash::from_bytes([0xff; 32]));
        assert!(matches!(
            witness.offer(&origin(), forked.head(), None),
            Err(WitnessError::Fork { .. })
        ));
    }

    #[test]
    fn a_note_naming_another_log_is_refused() {
        let log = log_of(8);
        let mut witness = witness_at(&log, 8);
        let other = Origin::new("example.com/ledgers/other").expect("valid");
        let note = SignedTreeHead::new(other, log.head());
        assert!(matches!(
            witness.offer_note(&origin(), &note, None),
            Err(WitnessError::OriginMismatch { .. })
        ));
    }

    #[test]
    fn a_request_body_round_trips() {
        let log = log_of(8);
        let proof = log.consistency_proof_between(3, 8).expect("in range");
        let request = AddCheckpoint::new(SignedTreeHead::new(origin(), log.head()), proof.clone());
        let body = request.to_body();
        let parsed = AddCheckpoint::parse_body(&body, 3).expect("parses");
        assert_eq!(parsed.note(), request.note());
        assert_eq!(parsed.proof(), &proof);
    }

    #[test]
    fn a_request_body_with_signatures_round_trips() {
        let log = log_of(8);
        let proof = log.consistency_proof_between(3, 8).expect("in range");
        let mut note = SignedTreeHead::new(origin(), log.head());
        note.add_signature(NoteSignature {
            name: KeyName::new("operator.example").expect("valid"),
            key_hash: [1, 2, 3, 4],
            value: vec![5u8; 64],
        });
        let request = AddCheckpoint::new(note.clone(), proof.clone());
        let parsed = AddCheckpoint::parse_body(&request.to_body(), 3).expect("parses");
        assert_eq!(parsed.note(), &note, "signature lines are not proof lines");
        assert_eq!(parsed.proof(), &proof);
    }

    #[test]
    fn a_reloaded_store_resumes_where_it_left_off() {
        let log = log_of(8);
        let witness = witness_at(&log, 8);
        // What a durable store would have written, read back.
        let rows = witness.tracked();
        let mut reloaded = Witness::new(MemoryWitnessStore::from_heads(rows));

        let mut forked = log_of(7);
        forked.append(Hash::from_bytes([0xff; 32]));
        assert!(
            matches!(
                reloaded.offer(&origin(), forked.head(), None),
                Err(WitnessError::Fork { .. })
            ),
            "a witness that forgets across a restart can be restarted into signing a fork"
        );
    }
}
