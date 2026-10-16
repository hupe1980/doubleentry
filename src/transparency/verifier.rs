//! The auditor's [`Verifier`]: a trusted head per log that moves only
//! through a consistency proof.

use std::collections::BTreeMap;

use super::Error;
use super::bundle::{ProofBundle, Proven};
use super::checkpoint::SignedCheckpoint;
use super::error::ForkEvidence;
use super::note::KeyName;
use super::policy::Policy;
use super::signing::{Vkey, unambiguous};
use crate::log::{ConsistencyProof, ProofError, TreeHead};

/// What an accepted [`Verifier::update`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Update {
    /// The first trusted head for the origin (trust on first use).
    First,
    /// The same head again.
    Unchanged,
    /// The head moved forward from `from` through a valid consistency
    /// proof.
    Advanced {
        /// The previous trusted head.
        from: TreeHead,
    },
}

/// One log the verifier follows.
#[derive(Debug, Clone, Default)]
struct Followed {
    keys: Vec<Vkey>,
    head: Option<TreeHead>,
    /// The signed checkpoint behind `head`, for evidence; `None` for a pin.
    note: Option<SignedCheckpoint>,
}

/// An auditor's view of one or more ledgers: per origin, the log keys and
/// the trusted head, and optionally a witness policy every checkpoint must
/// meet.
///
/// A head is trusted first by a pin (or a restored head) or on first use of
/// a validly signed checkpoint offered without a proof. After that it moves
/// only forward, through an RFC 9162 consistency proof; two roots at one
/// size are a fork, a smaller size a rollback — refused. A proof that does
/// not verify is refused as [`Error::Proof`]; only a proof that binds the
/// offered root and shows another prefix is [`Error::Inconsistent`], with
/// evidence (see [`ForkEvidence`]).
///
/// ```
/// use doubleentry::transparency::{Checkpoint, KeyName, LogKey, Update, Verifier};
/// use doubleentry::{Journal, LedgerId, date, currency::EUR};
///
/// let ledger = LedgerId::new("example.com/ledgers/acme")?;
/// let key = LogKey::from_seed(&ledger, [7; 32]);
/// let mut books = Journal::new(ledger.clone());
/// let first = books.checkpoint(&key)?;
///
/// let mut verifier = Verifier::new().log(key.vkey())?;
/// assert_eq!(verifier.update(&first, None)?, Update::First);
///
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// let next = books.checkpoint(&key)?;
/// let (old, new) = (first.checkpoint().head(), next.checkpoint().head());
/// let proof = books.prove_consistency(old.size, new.size)?;
/// assert_eq!(verifier.update(&next, Some(&proof))?, Update::Advanced { from: old });
/// assert_eq!(verifier.head(&KeyName::from(&ledger)), Some(new));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct Verifier {
    logs: BTreeMap<KeyName, Followed>,
    policy: Option<Policy>,
}

impl Verifier {
    /// A verifier that follows no log yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A verifier that follows the policy's logs (each log key's name is an
    /// origin) and requires every checkpoint to meet the policy.
    #[must_use]
    pub fn from_policy(policy: Policy) -> Self {
        let mut verifier = Self::new();
        for key in policy.logs() {
            // A policy holds no two keys of one (name, key ID).
            verifier.add(key.clone());
        }
        verifier.policy = Some(policy);
        verifier
    }

    /// Adds a log key; its name is the origin it signs for. Adding the
    /// same key again changes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::AmbiguousKey`] for another key with the same name and key
    /// ID: a signature line could be checked against either, so the set is
    /// refused rather than the first key taken.
    pub fn log(mut self, key: Vkey) -> Result<Self, Error> {
        if let Some(followed) = self.logs.get(key.name()) {
            unambiguous(followed.keys.iter().chain([&key]))?;
        }
        self.add(key);
        Ok(self)
    }

    /// Adds a key known not to be ambiguous.
    fn add(&mut self, key: Vkey) {
        let followed = self.logs.entry(key.name().clone()).or_default();
        if !followed.keys.contains(&key) {
            followed.keys.push(key);
        }
    }

    /// Pins (or restores) the trusted head of an origin.
    #[must_use]
    pub fn pin(mut self, origin: KeyName, head: TreeHead) -> Self {
        let followed = self.logs.entry(origin).or_default();
        followed.head = Some(head);
        followed.note = None;
        self
    }

    /// The trusted head of an origin.
    #[must_use]
    pub fn head(&self, origin: &KeyName) -> Option<TreeHead> {
        self.logs.get(origin).and_then(|followed| followed.head)
    }

    /// Every trusted head, by origin — what an auditor persists.
    pub fn heads(&self) -> impl Iterator<Item = (&KeyName, TreeHead)> {
        self.logs
            .iter()
            .filter_map(|(origin, followed)| Some((origin, followed.head?)))
    }

    /// Offers a signed checkpoint and the consistency proof from the trusted
    /// head to it (`None` on first use, at the same size, or from size 0).
    ///
    /// Signatures (and the policy, if set) are checked first; nothing
    /// changes unless the whole update is accepted.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownOrigin`]; the errors of
    /// [`SignedCheckpoint::verify`] and [`Policy::verify`];
    /// [`Error::ProofShape`] for a proof on first use, a missing proof where
    /// one is needed, a non-empty proof from size 0, or a proof whose sizes
    /// are not the two heads'; [`Error::Proof`] for a proof that does not
    /// verify; [`Error::Fork`]; [`Error::Rollback`]; or
    /// [`Error::Inconsistent`] when the proof binds the offered root and
    /// shows a prefix other than the trusted head.
    pub fn update(
        &mut self,
        checkpoint: &SignedCheckpoint,
        proof: Option<&ConsistencyProof>,
    ) -> Result<Update, Error> {
        let origin = checkpoint.checkpoint().origin();
        let followed = self
            .logs
            .get(origin)
            .filter(|followed| !followed.keys.is_empty())
            .ok_or_else(|| Error::UnknownOrigin(origin.clone()))?;
        checkpoint.verify(&followed.keys)?;
        if let Some(policy) = &self.policy {
            policy.verify(checkpoint)?;
        }
        let offered = checkpoint.checkpoint().head();
        let evidence = |trusted: TreeHead| {
            Box::new(ForkEvidence {
                trusted_head: trusted,
                trusted: followed.note.clone(),
                offered: checkpoint.clone(),
                proof: proof.cloned(),
            })
        };
        let update = match followed.head {
            None if proof.is_none() => Update::First,
            None => return Err(Error::ProofShape),
            Some(trusted) if offered.size == trusted.size => {
                if offered.root == trusted.root {
                    Update::Unchanged
                } else {
                    return Err(Error::Fork(evidence(trusted)));
                }
            }
            Some(trusted) if offered.size < trusted.size => {
                return Err(Error::Rollback { trusted, offered });
            }
            Some(trusted) => match consistency(trusted, offered, proof) {
                Consistency::Proven => Update::Advanced { from: trusted },
                Consistency::Unproven(error) => return Err(error),
                Consistency::Conflict => return Err(Error::Inconsistent(evidence(trusted))),
            },
        };
        if let Some(followed) = self.logs.get_mut(origin)
            && (update != Update::Unchanged || followed.note.is_none())
        {
            // The trusted head moves, or a pinned head gains its signed
            // checkpoint as evidence.
            followed.head = Some(offered);
            followed.note = Some(checkpoint.clone());
        }
        Ok(update)
    }

    /// Verifies a bundle against the trusted head of its origin, by hashes:
    /// the head was verified when it was trusted.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownOrigin`]; [`Error::NotAtTrustedHead`] for a bundle
    /// at any other head (advance first, or ask for a bundle at the trusted
    /// head); the errors of
    /// [`ProofBundle::verify_inclusion`].
    pub fn verify<const P: u8>(&self, bundle: &ProofBundle<P>) -> Result<Proven<P>, Error> {
        let checkpoint = bundle.checkpoint().checkpoint();
        let origin = checkpoint.origin();
        let followed = self
            .logs
            .get(origin)
            .ok_or_else(|| Error::UnknownOrigin(origin.clone()))?;
        let offered = checkpoint.head();
        if followed.head != Some(offered) {
            return Err(Error::NotAtTrustedHead {
                trusted: followed.head,
                offered,
            });
        }
        bundle.verify_inclusion()
    }
}

/// What a consistency proof between two signed heads shows.
enum Consistency {
    /// `offered` extends `trusted`.
    Proven,
    /// Nothing: the proof is missing, misshapen or does not recompute the
    /// offered root — anyone on the path could have corrupted it.
    Unproven(Error),
    /// The proof recomputes the offered root exactly, and for the trusted
    /// size a root other than the trusted one: the offered tree's prefix is
    /// not the trusted tree (`ForkEvidence`).
    Conflict,
}

/// Whether `proof` connects `trusted` to the larger `offered` head. From
/// size 0 every tree is consistent, and the proof is empty.
fn consistency(
    trusted: TreeHead,
    offered: TreeHead,
    proof: Option<&ConsistencyProof>,
) -> Consistency {
    let Some(proof) = proof else {
        return if trusted.size == 0 {
            Consistency::Proven
        } else {
            Consistency::Unproven(Error::ProofShape)
        };
    };
    if (proof.old_size, proof.new_size) != (trusted.size, offered.size) {
        return Consistency::Unproven(Error::ProofShape);
    }
    if trusted.size == 0 {
        return if proof.path.is_empty() {
            Consistency::Proven
        } else {
            Consistency::Unproven(Error::ProofShape)
        };
    }
    match proof.verify(&trusted, &offered) {
        Ok(()) => Consistency::Proven,
        // The trusted root failed first. Whether the proof binds the
        // offered root is decided by running it again from the root it
        // computed for the trusted size: every proof hash feeds the offered
        // root, so a proof that recomputes it carries the offered tree's
        // real nodes, and its prefix root is the one computed.
        Err(ProofError::RootMismatch { expected, computed })
            if expected == trusted.root && computed != trusted.root =>
        {
            let prefix = TreeHead {
                size: trusted.size,
                root: computed,
            };
            if proof.verify(&prefix, &offered).is_ok() {
                Consistency::Conflict
            } else {
                Consistency::Unproven(Error::Proof(ProofError::RootMismatch {
                    expected,
                    computed,
                }))
            }
        }
        Err(error) => Consistency::Unproven(Error::Proof(error)),
    }
}
