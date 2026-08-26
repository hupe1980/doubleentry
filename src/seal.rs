//! Period seals.
//!
//! Sealing a period commits to three things at once: **which entries** the log
//! held when it closed, **what they add up to**, and **which accounts those
//! totals are for**. All three are Merkle roots, so a third party holding
//! nothing but a seal can later be shown a fact about the period without being
//! given its contents:
//!
//! - Against [`Seal::tree_head`], an [`InclusionProof`]
//!   that a specific entry was in the log the seal closed over.
//! - Against [`Seal::trial_balance`], a [`BalanceProof`] that a specific
//!   account held a specific balance in the closing trial balance.
//! - Against [`Seal::accounts`], an
//!   [`AccountBindingProof`] that a specific
//!   handle is a specific account path.
//!
//! All three are `O(log n)` and reveal nothing else. The second is the reason a
//! seal commits to a Merkle root over the balances rather than to a flat digest
//! of them: a digest can only be checked by whoever holds every balance, which
//! is precisely the party an auditor is trying not to have to trust.
//!
//! # Why the third root exists
//!
//! A trial-balance leaf names its account by handle — a dense integer, chosen so
//! comparisons and lookups are cheap. On its own that makes a balance proof a
//! statement about an integer: an auditor learns that handle `#7` held a
//! balance and must take the operator's word for what `#7` is. Worse, nothing
//! would stop the operator changing the answer afterwards. Re-registering the
//! same paths in a different order renumbers every handle, and every seal, every
//! balance proof and the whole chain would go on verifying byte for byte while
//! each balance quietly referred to a different account — precisely the
//! alteration a seal exists to expose.
//!
//! [`Seal::accounts`] closes both gaps at once. It pins the handle space to
//! the paths it meant at the moment of sealing, and it lets a balance be
//! *named*: [`BalanceProof::verify_naming`] checks a balance and its account
//! binding against one seal, disclosing nothing about any other account.
//!
//! # What "belongs to the period" means
//!
//! The tree head is the whole log at the moment of sealing, not the period's
//! entries alone — entries are appended in recording order, not booking-date
//! order, so a period's entries need not be contiguous. An inclusion proof
//! therefore establishes *this entry was in the log the seal closed over*, and
//! [`Seal::entry_count`] and [`Seal::index_span`] describe how much of that log
//! the period accounts for. The closing balances are the stronger statement, and
//! they are exact: they fold every entry booked on or before the period's last
//! day and nothing else.
//!
//! Seals chain: each carries the hash of its predecessor. Removing or reordering
//! a sealed period breaks every seal after it.
//!
//! What a seal detects is *alteration*, not *access*. Preventing writes is the
//! storage layer's job; making a write recognisable afterwards is this one's.

use std::collections::{BTreeMap, BTreeSet};

use crate::account::AccountBindingProof;
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::canonical::{Canonical, CanonicalWriter};
use crate::hash::{Hash, tag, tagged};
use crate::merkle::{
    ConsistencyProof, InclusionProof, MerkleLog, ProofError, TreeHead, empty_root,
};
use crate::period::{LedgerId, PeriodId};

/// What a period turned out to contain.
///
/// Grouped rather than passed as three loose numbers: `first_index` and
/// `last_index` are both `Option<u64>` and `entry_count` is a bare `u64`, so as
/// positional arguments nothing but discipline keeps them in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeriodCoverage {
    /// Log index of the first entry the period covers, if it covers any.
    ///
    /// Entries are appended in recording order, not booking-date order, so a
    /// period's entries need not be contiguous. This is the smallest index
    /// belonging to the period, not a claim that everything above it does.
    pub first_index: Option<u64>,
    /// Log index of the last entry the period covers, if it covers any.
    pub last_index: Option<u64>,
    /// How many entries the period actually contains.
    ///
    /// Carried rather than derived from the index span, which may enclose
    /// entries belonging to other periods.
    pub entry_count: u64,
}

impl PeriodCoverage {
    /// A period that covers nothing.
    pub const EMPTY: Self = Self {
        first_index: None,
        last_index: None,
        entry_count: 0,
    };

    /// Coverage of a contiguous run of entries.
    #[must_use]
    pub fn spanning(first: u64, last: u64, entry_count: u64) -> Self {
        Self {
            first_index: Some(first),
            last_index: Some(last),
            entry_count,
        }
    }
}

/// A commitment to the closing state of one accounting period.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Seal {
    /// The ledger whose books this seal covers.
    ///
    /// Named inside the hash, not alongside it. Two ledgers can hold
    /// structurally identical entries — the same amounts, accounts and dates —
    /// and would then produce identical tree heads and identical trial balance
    /// roots. Without the ledger in the preimage their seals would be
    /// byte-identical, and a seal handed to an auditor would not say whose
    /// books it attests to. With it, a seal is evidence about one entity or it
    /// does not verify at all.
    pub ledger: LedgerId,
    /// The period being sealed.
    pub period: PeriodId,
    /// Log index of the first entry the period covers, if it covers any.
    ///
    /// Entries are appended in recording order, not booking-date order, so a
    /// period's entries need not be contiguous. This is the smallest index
    /// belonging to the period, not a claim that everything above it does.
    pub first_index: Option<u64>,
    /// Log index of the last entry the period covers, if it covers any.
    pub last_index: Option<u64>,
    /// How many entries the period actually contains.
    ///
    /// Stored rather than derived from the index span, which may enclose
    /// entries belonging to other periods.
    pub entry_count: u64,
    /// The journal's tree head at the moment of sealing.
    pub tree_head: TreeHead,
    /// Merkle head over the period's closing trial balance.
    ///
    /// The rows it commits to are keyed on account *handles*, so it is only
    /// meaningful together with [`Seal::accounts`], which says what those
    /// handles were.
    ///
    /// A head rather than a bare root: its size is the number of balance rows
    /// the period closed with, and it is half of what a [`BalanceProof`] is
    /// checked against.
    pub trial_balance: TreeHead,
    /// Merkle head over the handle-to-account bindings in force at sealing.
    ///
    /// What pins the handle space a [`Seal::trial_balance`] leaf is keyed on —
    /// see [Why the third root exists](self#why-the-third-root-exists) — and
    /// what [`SealChain::verify_against_accounts`] checks it against.
    ///
    /// It is also what makes selective disclosure complete: an
    /// [`AccountBindingProof`] against this head turns "handle `#7` held this
    /// balance" into "`Assets:Cash` held this balance", revealing no other
    /// account.
    pub accounts: TreeHead,
    /// Hash of the preceding seal, or `None` for the first.
    pub prev_seal: Option<Hash>,
    /// Proof that the predecessor's log is a **prefix** of this seal's log.
    ///
    /// What turns "these commitments are in order" into "this history was only
    /// ever appended to", checkable by a recipient holding nothing but the
    /// seals. See [`SealChainError::NotAPrefix`] for what it catches.
    ///
    /// `None` in exactly two cases: the genesis seal, and one following a
    /// predecessor whose tree was **empty** — no proof from the empty tree
    /// exists, so its root is checked against
    /// [`crate::merkle::empty_root`] instead. Any other `None` is
    /// [`SealChainError::MissingConsistency`], so the field cannot be dropped to
    /// make a rewritten history verify.
    pub prev_consistency: Option<ConsistencyProof>,
    /// Hash over every other field.
    ///
    /// Covers [`prev_consistency`](Self::prev_consistency) too, so the proof
    /// cannot be swapped for one relating a different pair of trees.
    pub seal_hash: Hash,
}

impl Seal {
    /// Builds a seal from a log, computing every root, the chaining hash and the
    /// consistency proof.
    ///
    /// `accounts` is an [`AccountRegistry::commitment`](crate::AccountRegistry::commitment)
    /// taken at the same moment as the trial balance. It has to be the registry
    /// the balances were computed against, or the seal commits to handles it
    /// does not explain.
    ///
    /// `previous` is the seal this one chains onto. Both the predecessor's hash
    /// and the consistency proof are derived from it, rather than passed
    /// separately, because a hash paired with a proof of a different pair of
    /// trees is the one combination that must not be expressible.
    ///
    /// A backend that reads its tree from storage rather than holding it uses
    /// [`Seal::from_parts`], which checks what this derives.
    ///
    /// # Errors
    ///
    /// Returns [`SealChainError::NotAPrefix`] when `log` cannot produce a proof
    /// from the predecessor's tree size — the log is shorter than a seal already
    /// committed to, or does not extend it — and the other link errors from
    /// [`Seal::from_parts`].
    pub fn build<const P: u8>(
        ledger: LedgerId,
        period: PeriodId,
        coverage: PeriodCoverage,
        log: &MerkleLog,
        trial_balance: &TrialBalance<P>,
        accounts: TreeHead,
        previous: Option<&Self>,
    ) -> Result<Self, SealChainError> {
        let tree_head = log.head();
        let prev_consistency = match previous {
            // A proof from the empty tree is refused by construction, and there
            // is nothing it could add: every log extends the empty one.
            Some(prev) if prev.tree_head.size > 0 => Some(
                log.consistency_proof_between(prev.tree_head.size, tree_head.size)
                    .map_err(|_| SealChainError::NotAPrefix {
                        period: period.clone(),
                    })?,
            ),
            _ => None,
        };
        Self::from_parts(
            ledger,
            period,
            coverage,
            tree_head,
            prev_consistency,
            trial_balance,
            accounts,
            previous,
        )
    }

    /// Builds a seal from parts a backend has already read out of storage.
    ///
    /// The durable counterpart to [`Seal::build`]. A SQL backend does not hold
    /// its tree in memory — it reads `O(log n)` nodes to produce a head and
    /// another `O(log n)` to produce a consistency proof — so it arrives with
    /// the pieces rather than with a log to derive them from.
    ///
    /// **The pieces are checked, not trusted.** [`Seal::build`] guarantees the
    /// head and the proof came from one tree by construction; here that has to
    /// be re-established, so the proof is verified against both heads before
    /// anything is hashed. A backend that read from two different trees is
    /// refused at the point of sealing rather than when somebody verifies the
    /// chain.
    ///
    /// # Errors
    ///
    /// Returns the [`SealChainError`] the parts violate:
    /// [`NotAPrefix`](SealChainError::NotAPrefix) when the proof does not relate
    /// the two heads, [`MissingConsistency`](SealChainError::MissingConsistency)
    /// when a chained seal has none, and
    /// [`UnexpectedConsistency`](SealChainError::UnexpectedConsistency) when one
    /// is supplied where no proof can exist.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts<const P: u8>(
        ledger: LedgerId,
        period: PeriodId,
        coverage: PeriodCoverage,
        tree_head: TreeHead,
        prev_consistency: Option<ConsistencyProof>,
        trial_balance: &TrialBalance<P>,
        accounts: TreeHead,
        previous: Option<&Self>,
    ) -> Result<Self, SealChainError> {
        // The same rule the chain will apply, from the same function, so a seal
        // that could never chain is never written in the first place.
        check_extends(&period, previous, &tree_head, prev_consistency.as_ref())?;

        let trial_balance = trial_balance_head(trial_balance);
        let mut seal = Self {
            ledger,
            period,
            first_index: coverage.first_index,
            last_index: coverage.last_index,
            entry_count: coverage.entry_count,
            tree_head,
            trial_balance,
            accounts,
            prev_seal: previous.map(|p| p.seal_hash),
            prev_consistency,
            seal_hash: Hash::from_bytes([0u8; 32]),
        };
        seal.seal_hash = seal.compute_hash();
        Ok(seal)
    }

    /// Recomputes the hash over every field except `seal_hash` itself.
    #[must_use]
    pub fn compute_hash(&self) -> Hash {
        let mut w = CanonicalWriter::new();
        self.encode(&mut w);
        tagged(tag::SEAL_V1, &w.finish())
    }

    /// True when `seal_hash` matches the seal's own contents.
    #[must_use]
    pub fn is_self_consistent(&self) -> bool {
        self.compute_hash() == self.seal_hash
    }

    /// The span of log indices the period's entries fall within.
    #[must_use]
    pub fn index_span(&self) -> Option<(u64, u64)> {
        match (self.first_index, self.last_index) {
            (Some(first), Some(last)) if last >= first => Some((first, last)),
            _ => None,
        }
    }
}

/// Wire form of a seal.
///
/// Separate from [`Seal`] so deserialisation can re-check the seal hash before
/// handing back a value. Every field is public and mutable, so a `Seal` read off
/// a wire without that check would be a commitment that commits to nothing.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct SealOwned {
    ledger: LedgerId,
    period: PeriodId,
    first_index: Option<u64>,
    last_index: Option<u64>,
    entry_count: u64,
    tree_head: TreeHead,
    trial_balance: TreeHead,
    accounts: TreeHead,
    prev_seal: Option<Hash>,
    prev_consistency: Option<ConsistencyProof>,
    seal_hash: Hash,
}

/// A deserialised seal is checked against its own hash before it is returned.
///
/// The same rule the rest of the crate follows: an invariant that holds for a
/// constructed value must hold for one read back, or the type guarantees
/// nothing. Here the invariant is the whole point of the type — `seal_hash` is
/// what an auditor archives, and a `Seal` whose fields no longer hash to it is
/// evidence of tampering, not a value to hand to a caller who may never think to
/// call [`Seal::is_self_consistent`].
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Seal {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = SealOwned::deserialize(d)?;
        let seal = Self {
            ledger: raw.ledger,
            period: raw.period,
            first_index: raw.first_index,
            last_index: raw.last_index,
            entry_count: raw.entry_count,
            tree_head: raw.tree_head,
            trial_balance: raw.trial_balance,
            accounts: raw.accounts,
            prev_seal: raw.prev_seal,
            prev_consistency: raw.prev_consistency,
            seal_hash: raw.seal_hash,
        };
        if seal.is_self_consistent() {
            Ok(seal)
        } else {
            Err(serde::de::Error::custom(
                "seal does not match its own contents",
            ))
        }
    }
}

impl Canonical for Seal {
    /// Encodes every field except `seal_hash`, which is derived from this.
    fn encode(&self, w: &mut CanonicalWriter) {
        self.ledger.encode(w);
        self.period.encode(w);
        w.option(self.first_index.as_ref(), |w, v| {
            w.u64(*v);
        });
        w.option(self.last_index.as_ref(), |w, v| {
            w.u64(*v);
        });
        w.u64(self.entry_count);
        w.u64(self.tree_head.size);
        w.fixed(self.tree_head.root.as_bytes());
        w.u64(self.trial_balance.size);
        w.fixed(self.trial_balance.root.as_bytes());
        w.u64(self.accounts.size);
        w.fixed(self.accounts.root.as_bytes());
        w.option(self.prev_seal.as_ref(), |w, v| {
            w.fixed(v.as_bytes());
        });
        // Inside the preimage, so the proof cannot be swapped for one relating a
        // different pair of trees while the seal hash goes on matching.
        w.option(self.prev_consistency.as_ref(), |w, v| v.encode(w));
    }
}

/// Merkle head over a trial balance.
///
/// Shorthand for [`TrialBalanceCommitment::of`] followed by
/// [`TrialBalanceCommitment::head`]. Build the commitment instead when you also
/// want to prove individual rows.
#[must_use]
pub fn trial_balance_head<const P: u8>(tb: &TrialBalance<P>) -> TreeHead {
    TrialBalanceCommitment::of(tb).head()
}

/// The leaf a single trial-balance row hashes to.
///
/// One leaf per `(account, currency, layer) → (debits, credits)` row. Both gross
/// totals are covered, not the net: two accounts that net to zero — one quiet,
/// one with heavy offsetting turnover — must not produce the same commitment.
/// The scale is covered too, so the same minor units at a different precision
/// are a different balance rather than a coincidence.
#[must_use]
pub fn balance_leaf<const P: u8>(key: &BalanceKey, balance: &Balance<P>) -> Hash {
    let mut w = CanonicalWriter::new();
    w.u32(key.account.index());
    w.fixed(key.currency.as_bytes());
    w.u8(key.layer.discriminant());
    w.u8(P);
    w.i64(balance.debits.to_minor());
    w.i64(balance.credits.to_minor());
    tagged(tag::TRIAL_BALANCE_V1, &w.finish())
}

/// A Merkle commitment to a trial balance, able to prove individual rows.
///
/// A seal stores only [`Self::root`]. Rebuild the commitment from the same trial
/// balance to answer a proof request; it is a pure function of the balances, so
/// the rebuild is exact or the root does not match and nothing can be proven.
///
/// ```
/// # use doubleentry::{Amount, BalanceKey, Currency, Layer, Posting, TrialBalance};
/// # use doubleentry::account::AccountId;
/// # use doubleentry::seal::TrialBalanceCommitment;
/// # type Eur = Amount<2>;
/// # let cash = AccountId::from_index(0);
/// # let revenue = AccountId::from_index(1);
/// let mut tb = TrialBalance::<2>::new();
/// tb.apply(&Posting::debit(cash, Eur::parse("1190.00")?, Currency::EUR))?;
/// tb.apply(&Posting::credit(revenue, Eur::parse("1190.00")?, Currency::EUR))?;
///
/// let commitment = TrialBalanceCommitment::of(&tb);
/// let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };
/// let proof = commitment.prove(&key).expect("cash is in the trial balance");
///
/// // An auditor holding only the seal's root and this one balance can check it.
/// assert!(proof.verify(&commitment.head()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct TrialBalanceCommitment<const P: u8> {
    rows: Vec<(BalanceKey, Balance<P>)>,
    log: MerkleLog,
}

impl<const P: u8> TrialBalanceCommitment<P> {
    /// Commits to a trial balance.
    ///
    /// Leaves follow the trial balance's own deterministic order — ascending
    /// [`BalanceKey`] — so the root is a pure function of the balances and not
    /// of how they were accumulated, and [`prove`](Self::prove) can find a row
    /// by binary search.
    #[must_use]
    pub fn of(tb: &TrialBalance<P>) -> Self {
        let rows: Vec<(BalanceKey, Balance<P>)> = tb.iter().map(|(k, b)| (*k, *b)).collect();
        let leaves = rows.iter().map(|(k, b)| balance_leaf(k, b)).collect();
        Self {
            rows,
            log: MerkleLog::from_leaves(leaves),
        }
    }

    /// The head a seal records.
    ///
    /// Size and root together: the size is the number of rows in the trial
    /// balance, and a [`BalanceProof`] is checked against both.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        self.log.head()
    }

    /// The root alone, for a caller that only wants to compare commitments.
    #[must_use]
    pub fn root(&self) -> Hash {
        self.log.root()
    }

    /// Number of rows committed to.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when the trial balance was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Proves that `key` held the balance this commitment recorded for it.
    ///
    /// Returns `None` when the key is absent. That is not the same as a balance
    /// of zero: an account with no postings has no row, so there is nothing to
    /// prove about it and a proof must not be manufactured.
    #[must_use]
    pub fn prove(&self, key: &BalanceKey) -> Option<BalanceProof<P>> {
        // Rows come out of a `TrialBalance` in key order, so this is a binary
        // search rather than a scan. It matters: an auditor asking about a
        // hundred accounts out of a hundred thousand should pay `O(k log n)`,
        // not `O(k · n)`.
        let index = self.rows.binary_search_by(|(k, _)| k.cmp(key)).ok()?;
        let (key, balance) = self.rows.get(index).copied()?;
        let proof = self.log.inclusion_proof(index as u64).ok()?;
        Some(BalanceProof {
            key,
            balance,
            proof,
        })
    }

    /// Proves every row, in commitment order.
    ///
    /// # Errors
    ///
    /// Returns a [`ProofError`] only if the log and the row list have diverged,
    /// which would be a bug in this crate.
    pub fn prove_all(&self) -> Result<Vec<BalanceProof<P>>, ProofError> {
        self.rows
            .iter()
            .enumerate()
            .map(|(index, (key, balance))| {
                Ok(BalanceProof {
                    key: *key,
                    balance: *balance,
                    proof: self.log.inclusion_proof(index as u64)?,
                })
            })
            .collect()
    }
}

/// Proof that one account's balance is the one a seal committed to.
///
/// Self-contained: it carries the claim as well as the path, so a verifier needs
/// nothing but this and the root out of the seal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceProof<const P: u8> {
    /// What the balance is for.
    pub key: BalanceKey,
    /// The balance being claimed.
    pub balance: Balance<P>,
    /// Path from the balance's leaf up to the trial-balance root.
    pub proof: InclusionProof,
}

impl<const P: u8> BalanceProof<P> {
    /// Verifies the claim against a [`Seal::trial_balance`] head.
    ///
    /// Returns `false` on any inconsistency rather than distinguishing failure
    /// modes: a verifier cannot act differently on a malformed proof than on a
    /// forged one.
    #[must_use]
    pub fn verify(&self, trial_balance: &TreeHead) -> bool {
        self.proof
            .verify(&balance_leaf(&self.key, &self.balance), trial_balance)
    }

    /// Verifies the claim against a whole seal.
    ///
    /// Establishes what a *handle* held. To learn which account that handle is,
    /// pair this with an
    /// [`AccountBindingProof`] against the
    /// same seal's [`accounts`](Seal::accounts) head — or use
    /// [`BalanceProof::verify_naming`], which checks both together.
    #[must_use]
    pub fn verify_against(&self, seal: &Seal) -> bool {
        seal.is_self_consistent() && self.verify(&seal.trial_balance)
    }

    /// Verifies the balance *and* the account it belongs to, against one seal.
    ///
    /// The complete claim an auditor wants: this account, this balance, this
    /// period, checkable from a seal and two `O(log n)` paths, disclosing
    /// nothing else. Fails unless the binding proof names the same handle the
    /// balance is for — otherwise a genuine balance for one account could be
    /// presented under another account's name.
    #[must_use]
    pub fn verify_naming(&self, binding: &AccountBindingProof, seal: &Seal) -> bool {
        self.verify_against(seal)
            && binding.id() == self.key.account
            && binding.verify(&seal.accounts)
    }
}

/// What a period's seal can say about one account.
///
/// Three answers, and only the first carries a proof. The other two are answers
/// rather than failures, which is why they are here and not in
/// [`SealedBalanceError`]: the caller asked a well-formed question about books
/// that are intact, and the honest reply is "nothing".
///
/// That distinction is load-bearing for a generic caller. A
/// [`LedgerStore`](crate::LedgerStore)'s error type is the *backend's*, and it
/// is only required to be `From<SealedBalanceError>` — there is no route back,
/// so a "nothing to prove" case on the error path is unreachable through
/// `S: LedgerStore<P>` and would have to be handled per backend.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SealedBalanceOutcome<const P: u8> {
    /// The account held this balance when the period closed, provably.
    Proven(Box<SealedBalance<P>>),
    /// The account was registered by then and has no row in the closing trial
    /// balance.
    ///
    /// Not a balance of zero. An account with no postings has no row, and a
    /// proof of one must not be manufactured.
    NoRow,
    /// The account had not been registered when the period sealed.
    ///
    /// A seal names the handles the registry had issued by then, so one
    /// onboarded afterwards is not an account it can speak about at all.
    NotYetRegistered,
}

impl<const P: u8> SealedBalanceOutcome<P> {
    /// The proof, if there is one.
    #[must_use]
    pub fn proven(&self) -> Option<&SealedBalance<P>> {
        match self {
            Self::Proven(balance) => Some(balance),
            _ => None,
        }
    }

    /// Takes the proof, if there is one.
    #[must_use]
    pub fn into_proven(self) -> Option<SealedBalance<P>> {
        match self {
            Self::Proven(balance) => Some(*balance),
            _ => None,
        }
    }

    /// True when there is nothing to prove, for either reason.
    ///
    /// The common case a caller wants once: a report renders a blank either
    /// way. Match the variants when the two need to read differently — "no
    /// activity" and "not on the books yet" are different sentences.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        !matches!(self, Self::Proven(_))
    }
}

/// A sealed balance, named, with everything needed to check it.
///
/// The complete answer to *what did this account close a period at, and says
/// who?* — a seal, an `O(log n)` path to one trial-balance row, and an
/// `O(log n)` path binding that row's handle to an account path. It discloses
/// nothing about any other account, balance or entry, and it serialises, because
/// the whole point is handing it to someone who does not have the books.
///
/// Assemble it with [`Journal::prove_sealed_balance`](crate::Journal::prove_sealed_balance)
/// or [`LedgerStore::prove_sealed_balance`](crate::LedgerStore::prove_sealed_balance),
/// never by hand — see [`SealedBalance::assemble`] for what they check on the
/// way. [`Self::verify`] re-checks the lot from scratch, which is what a
/// recipient does, since they did not build it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SealedBalance<const P: u8> {
    /// The seal the claim is against.
    pub seal: Seal,
    /// The balance, and its path to [`Seal::trial_balance`].
    pub balance: BalanceProof<P>,
    /// The handle-to-path binding, and its path to [`Seal::accounts`].
    pub binding: AccountBindingProof,
}

impl<const P: u8> SealedBalance<P> {
    /// Builds the claim from a seal, the period's closing balances, and the
    /// registry — checking every part on the way.
    ///
    /// The single place the recipe lives, so the in-memory journal and every
    /// durable backend assemble it identically rather than keeping copies that
    /// can drift.
    ///
    /// Three things are checked, and the first is the one that matters:
    ///
    /// 1. `closing` reproduces [`Seal::trial_balance`]. A commitment the caller
    ///    just computed proves nothing until it matches the one on record —
    ///    skip this and the resulting proof is internally consistent and
    ///    evidence of nothing. A mismatch is [`SealedBalanceError::Restated`].
    /// 2. The handle was issued by the time the period sealed, so the binding is
    ///    proven at [`Seal::accounts`]`.size` rather than against the registry
    ///    as it stands now.
    /// 3. That binding verifies under [`Seal::accounts`].
    ///
    /// The two "nothing to prove" answers come back as
    /// [`SealedBalanceOutcome`] variants rather than errors — see there for why
    /// that matters to a generic caller.
    ///
    /// # Errors
    ///
    /// Returns [`SealedBalanceError`] only when something is actually wrong:
    /// the books no longer reproduce the seal, or the registry was renumbered.
    pub fn assemble(
        seal: Seal,
        closing: &TrialBalance<P>,
        accounts: &crate::account::AccountRegistry,
        key: BalanceKey,
    ) -> Result<SealedBalanceOutcome<P>, SealedBalanceError> {
        let commitment = TrialBalanceCommitment::of(closing);
        if commitment.head() != seal.trial_balance {
            return Err(SealedBalanceError::Restated {
                period: seal.period,
            });
        }
        let Some(binding) = accounts.prove_binding_at(key.account, seal.accounts.size) else {
            return Ok(SealedBalanceOutcome::NotYetRegistered);
        };
        if !binding.verify(&seal.accounts) {
            return Err(SealedBalanceError::RegistryMismatch {
                period: seal.period,
            });
        }
        Ok(match commitment.prove(&key) {
            Some(balance) => SealedBalanceOutcome::Proven(Box::new(Self {
                seal,
                balance,
                binding,
            })),
            None => SealedBalanceOutcome::NoRow,
        })
    }

    /// Re-checks the whole claim against the seal it carries.
    ///
    /// Equivalent to [`BalanceProof::verify_naming`]; offered here so a
    /// recipient does not have to know which of the three pieces to feed to
    /// which verifier.
    ///
    /// Note what this does *not* establish: that the seal is one you should
    /// trust. A seal is self-consistent by construction, so check it against a
    /// chain you already hold — [`SealChain::verify`] — or against a copy
    /// archived when the period closed.
    #[must_use]
    pub fn verify(&self) -> bool {
        self.balance.verify_naming(&self.binding, &self.seal)
    }

    /// The account path the balance belongs to.
    #[must_use]
    pub fn path(&self) -> &crate::account::AccountPath {
        self.binding.path()
    }
}

/// Failure proving a sealed balance.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SealedBalanceError {
    /// No seal exists for the period.
    #[error("period {period} has not been sealed")]
    NotSealed {
        /// The period asked about.
        period: PeriodId,
    },
    /// The period the seal names is no longer defined in the calendar.
    #[error("period {period} is sealed but no longer defined")]
    UndefinedPeriod {
        /// The period asked about.
        period: PeriodId,
    },
    /// The rebuilt trial balance does not match what the seal committed to.
    ///
    /// The books have changed since the period was sealed, or the seal did not
    /// come from them. Either way there is nothing honest to prove, so no proof
    /// is returned — one against a locally recomputed commitment would be
    /// internally consistent and evidence of nothing.
    #[error("the rebuilt closing balance does not match the seal for period {period}")]
    Restated {
        /// The period asked about.
        period: PeriodId,
    },
    /// The account bindings do not reproduce the seal's `accounts` head.
    ///
    /// The registry was renumbered — re-registering the same paths in a
    /// different order repoints every handle the seal's balances are keyed on.
    #[error("the account bindings do not match the seal for period {period}")]
    RegistryMismatch {
        /// The period asked about.
        period: PeriodId,
    },
}

/// Failure verifying a chain of seals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SealChainError {
    /// A seal's stored hash did not match its contents.
    #[error("seal for period {period} does not match its own contents")]
    Tampered {
        /// The offending period.
        period: PeriodId,
    },
    /// A seal did not reference its predecessor.
    #[error("seal for period {period} does not chain to its predecessor")]
    BrokenChain {
        /// The offending period.
        period: PeriodId,
    },
    /// A seal named a different ledger than the chain it was offered to.
    #[error("seal for period {period} belongs to ledger {found}, not {expected}")]
    ForeignLedger {
        /// The offending period.
        period: PeriodId,
        /// The ledger the chain covers.
        expected: LedgerId,
        /// The ledger the seal names.
        found: LedgerId,
    },
    /// Two seals in the chain claim the same period.
    ///
    /// A period is sealed once. A second seal for it would give two different
    /// commitments to one period's closing balances, and nothing in the chain
    /// says which of them the books mean.
    #[error("period {period} is sealed more than once in this chain")]
    DuplicatePeriod {
        /// The repeated period.
        period: PeriodId,
    },
    /// The first seal claimed a predecessor, or a later one claimed none.
    #[error("seal for period {period} has an unexpected predecessor reference")]
    MisplacedGenesis {
        /// The offending period.
        period: PeriodId,
    },
    /// Tree heads did not grow monotonically.
    #[error("seal for period {period} does not extend the previous tree")]
    NonMonotonic {
        /// The offending period.
        period: PeriodId,
    },
    /// A seal did not carry the consistency proof its position requires.
    ///
    /// Only the genesis seal, and one following a predecessor whose tree was
    /// empty, may omit it — see [`Seal::prev_consistency`]. Anywhere else a
    /// missing proof is a chain that cannot say the log was appended to rather
    /// than rewritten, and it is refused instead of being verified to a weaker
    /// standard.
    #[error(
        "seal for period {period} carries no consistency proof from its \
         predecessor's tree of {previous_size} entries"
    )]
    MissingConsistency {
        /// The offending period.
        period: PeriodId,
        /// The predecessor's tree size, which the proof should have started at.
        previous_size: u64,
    },
    /// A seal carried a consistency proof where none is admissible.
    ///
    /// The genesis seal has no predecessor to be consistent with, and a
    /// predecessor whose tree was empty admits no proof at all. A value here is
    /// therefore evidence about nothing, and accepting it would let a chain look
    /// better checked than it is.
    #[error("seal for period {period} carries a consistency proof it cannot have one for")]
    UnexpectedConsistency {
        /// The offending period.
        period: PeriodId,
    },
    /// A seal's log is not a prefix of its predecessor's — the history was
    /// rewritten between the two closes.
    ///
    /// **The rule the rest of the chain exists to support.** Hash-chaining the
    /// seals proves they were issued in order and none was edited; it says
    /// nothing about the entries underneath. Two seals claiming tree sizes 100
    /// and 200 with unrelated roots satisfy every other check here. This is what
    /// refuses them.
    #[error("seal for period {period} does not prove its log extends its predecessor's")]
    NotAPrefix {
        /// The offending period.
        period: PeriodId,
    },
    /// A predecessor claimed an empty tree whose root is not the empty root.
    #[error("seal for period {period} follows a zero-size tree that is not the empty tree")]
    NotTheEmptyTree {
        /// The offending period.
        period: PeriodId,
    },
    /// A seal's tree head is not the head the log had at that size.
    ///
    /// Only [`SealChain::verify_against_log`] can raise this, because only it
    /// holds the log. The chain itself is internally consistent — it simply
    /// describes a different history from the one in front of you.
    #[error("seal for period {period} commits to root {expected}, but this log held {found}")]
    HeadMismatch {
        /// The offending period.
        period: PeriodId,
        /// The root the seal committed to.
        expected: Hash,
        /// The root the log actually had at that size.
        found: Hash,
    },
    /// A seal commits to more entries than the log holds.
    ///
    /// Entries were removed after the period sealed, or the chain belongs to
    /// other books. Either way the seal cannot be checked against this log.
    #[error(
        "seal for period {period} commits to {claimed} entries, but the log holds \
         only {holds}"
    )]
    BeyondTheLog {
        /// The offending period.
        period: PeriodId,
        /// How many entries the seal commits to.
        claimed: u64,
        /// How many the log actually holds.
        holds: u64,
    },
    /// The account registry shrank between two seals.
    ///
    /// An [`AccountRegistry`](crate::AccountRegistry) only ever grows: handles
    /// are dense positions and are never reissued, because a reused one would
    /// repoint every posting row and every sealed balance that names it. A later
    /// seal committing to *fewer* bindings than an earlier one is therefore not
    /// a state the engine can reach — it is a registry that was rebuilt from a
    /// truncated set, which is exactly the renumbering
    /// [`Seal::accounts`] exists to expose.
    #[error(
        "seal for period {period} commits to {found} account bindings, fewer than \
         the {expected} its predecessor did"
    )]
    ShrunkenRegistry {
        /// The offending period.
        period: PeriodId,
        /// How many bindings the predecessor committed to.
        expected: u64,
        /// How many this seal commits to.
        found: u64,
    },
    /// A seal commits to more account bindings than the registry ever issued.
    ///
    /// The registry counterpart of [`BeyondTheLog`](Self::BeyondTheLog):
    /// accounts were removed after the period sealed, or the chain belongs to
    /// other books.
    #[error(
        "seal for period {period} commits to {claimed} account bindings, but the \
         registry holds only {holds}"
    )]
    BeyondTheRegistry {
        /// The offending period.
        period: PeriodId,
        /// How many bindings the seal commits to.
        claimed: u64,
        /// How many the registry actually holds.
        holds: u64,
    },
    /// A seal's account commitment is not the one the registry produces.
    ///
    /// **The renumbering [`Seal::accounts`] exists to expose.** A trial-balance
    /// leaf names its account by *handle*, so re-registering the same paths in a
    /// different order leaves every seal and every balance proof verifying byte
    /// for byte while each balance refers to a different account.
    ///
    /// Nothing about the seals themselves can see it, so only
    /// [`SealChain::verify_against_accounts`] raises this.
    #[error(
        "seal for period {period} commits to account bindings {expected}, but the \
         registry's first {size} handles hash to {found}"
    )]
    AccountsRebound {
        /// The offending period.
        period: PeriodId,
        /// How many bindings the seal commits to.
        size: u64,
        /// The commitment the seal recorded.
        expected: Hash,
        /// The commitment the registry actually produces at that size.
        found: Hash,
    },
}

/// Whether `tree_head` provably extends `previous`, given the proof offered.
///
/// The one place the rule lives: [`Seal::from_parts`] runs it before writing a
/// seal and [`SealChain::check_link`] before accepting one, so a chain cannot
/// refuse a link it would itself have produced.
fn check_extends(
    period: &PeriodId,
    previous: Option<&Seal>,
    tree_head: &TreeHead,
    proof: Option<&ConsistencyProof>,
) -> Result<(), SealChainError> {
    let at = || period.clone();
    match (previous, proof) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(SealChainError::UnexpectedConsistency { period: at() }),
        (Some(prev), proof) => match (prev.tree_head.size, proof) {
            (0, None) => {
                if prev.tree_head.root == empty_root() {
                    Ok(())
                } else {
                    Err(SealChainError::NotTheEmptyTree { period: at() })
                }
            }
            (0, Some(_)) => Err(SealChainError::UnexpectedConsistency { period: at() }),
            (previous_size, None) => Err(SealChainError::MissingConsistency {
                period: at(),
                previous_size,
            }),
            (_, Some(proof)) => {
                if proof.verify(&prev.tree_head, tree_head) {
                    Ok(())
                } else {
                    Err(SealChainError::NotAPrefix { period: at() })
                }
            }
        },
    }
}

/// An ordered chain of period seals covering one ledger.
///
/// The chain names the ledger it covers, so the **first** seal is checked as
/// strictly as every later one. Without that, a chain of length one would accept
/// a seal from any books at all, and the ledger identity folded into the seal
/// hash — the whole reason it is in the preimage — would only start being
/// enforced from the second period onward.
#[derive(Debug, Clone)]
pub struct SealChain {
    ledger: LedgerId,
    seals: Vec<Seal>,
    /// Period to position in `seals`, so the one-seal-per-period rule and
    /// [`SealChain::get`] both cost a lookup rather than a scan. A ledger on
    /// daily periods reaches thousands of seals within a decade, and scanning
    /// made [`SealChain::verify`] quadratic in exactly the case an auditor runs
    /// it.
    periods: BTreeMap<PeriodId, usize>,
}

impl SealChain {
    /// Creates an empty chain for one ledger.
    #[must_use]
    pub fn new(ledger: LedgerId) -> Self {
        Self {
            ledger,
            seals: Vec::new(),
            periods: BTreeMap::new(),
        }
    }

    /// Rebuilds a chain from stored seals, checking every link.
    ///
    /// What a backend uses on the way out: seals read back from a table are
    /// rows, not evidence, until the chain has accepted them.
    ///
    /// # Errors
    ///
    /// Returns the first [`SealChainError`] that does not hold.
    pub fn from_seals(
        ledger: LedgerId,
        seals: impl IntoIterator<Item = Seal>,
    ) -> Result<Self, SealChainError> {
        let mut chain = Self::new(ledger);
        for seal in seals {
            chain.push(seal)?;
        }
        Ok(chain)
    }

    /// The ledger this chain covers.
    #[must_use]
    pub fn ledger(&self) -> &LedgerId {
        &self.ledger
    }

    /// The hash of the most recent seal.
    #[must_use]
    pub fn head(&self) -> Option<Hash> {
        self.seals.last().map(|s| s.seal_hash)
    }

    /// The most recent seal.
    #[must_use]
    pub fn last(&self) -> Option<&Seal> {
        self.seals.last()
    }

    /// Every rule a seal must satisfy to belong after `previous`.
    ///
    /// Shared by [`SealChain::push`] and [`SealChain::verify`] so that appending
    /// and re-checking cannot drift apart — a chain that accepted a link it
    /// would later reject, or the reverse, would be worse than either rule alone.
    ///
    /// `already_sealed` answers whether a period is in the chain already. It is
    /// a predicate rather than a set so that appending can consult the index it
    /// maintains while verifying consults one it rebuilds — verification that
    /// trusted the cache would only be checking that the cache agreed with
    /// itself, and appending that rebuilt the set each time would be quadratic.
    fn check_link(
        ledger: &LedgerId,
        already_sealed: impl Fn(&PeriodId) -> bool,
        previous: Option<&Seal>,
        seal: &Seal,
    ) -> Result<(), SealChainError> {
        let period = || seal.period.clone();
        if !seal.is_self_consistent() {
            return Err(SealChainError::Tampered { period: period() });
        }
        if seal.ledger != *ledger {
            return Err(SealChainError::ForeignLedger {
                period: period(),
                expected: ledger.clone(),
                found: seal.ledger.clone(),
            });
        }
        if already_sealed(&seal.period) {
            return Err(SealChainError::DuplicatePeriod { period: period() });
        }
        match (previous, seal.prev_seal) {
            (None, None) => check_extends(
                &seal.period,
                None,
                &seal.tree_head,
                seal.prev_consistency.as_ref(),
            ),
            (Some(prev), Some(reference)) => {
                if prev.seal_hash != reference {
                    return Err(SealChainError::BrokenChain { period: period() });
                }
                if seal.tree_head.size < prev.tree_head.size {
                    return Err(SealChainError::NonMonotonic { period: period() });
                }
                // The registry only ever grows, so a later seal committing to
                // fewer bindings means the handles its balances are keyed on
                // were renumbered underneath the chain.
                if seal.accounts.size < prev.accounts.size {
                    return Err(SealChainError::ShrunkenRegistry {
                        period: period(),
                        expected: prev.accounts.size,
                        found: seal.accounts.size,
                    });
                }
                // The check the rest of this function exists to support: the
                // predecessor's log is a *prefix* of this one's. Everything
                // above establishes that the seals are in order and unedited,
                // and nothing above looks at the entries they commit to.
                check_extends(
                    &seal.period,
                    previous,
                    &seal.tree_head,
                    seal.prev_consistency.as_ref(),
                )
            }
            _ => Err(SealChainError::MisplacedGenesis { period: period() }),
        }
    }

    /// Appends a seal, checking that it chains to the current head.
    ///
    /// # Errors
    ///
    /// Returns the [`SealChainError`] the seal violates, having appended nothing.
    pub fn push(&mut self, seal: Seal) -> Result<(), SealChainError> {
        Self::check_link(
            &self.ledger,
            |period| self.periods.contains_key(period),
            self.seals.last(),
            &seal,
        )?;
        self.periods.insert(seal.period.clone(), self.seals.len());
        self.seals.push(seal);
        Ok(())
    }

    /// Verifies every seal's **account commitment** against a registry.
    ///
    /// The registry half of [`verify_against_log`](Self::verify_against_log),
    /// and needed for the same reason: [`SealChain::verify`] takes seals alone,
    /// so a complete chain can be assembled over a chart of accounts nobody
    /// holds.
    ///
    /// This is what catches **renumbering**. A trial-balance leaf names its
    /// account by handle, so re-registering the same paths in a different order
    /// repoints every sealed balance while every hash in the chain goes on
    /// matching. [`Seal::accounts`] is the commitment that exposes it; this is
    /// the comparison.
    ///
    /// A registry only ever grows, so each seal is checked against the
    /// registry's commitment *at the size the seal recorded*, not its current
    /// one.
    ///
    /// # Errors
    ///
    /// Returns [`SealChainError::BeyondTheRegistry`] when a seal names more
    /// bindings than the registry holds, and
    /// [`SealChainError::AccountsRebound`] when the two commitments differ.
    pub fn verify_against_accounts(
        &self,
        accounts: &crate::account::AccountRegistry,
    ) -> Result<(), SealChainError> {
        self.verify()?;
        for seal in &self.seals {
            let Some(actual) = accounts.commitment_at(seal.accounts.size) else {
                return Err(SealChainError::BeyondTheRegistry {
                    period: seal.period.clone(),
                    claimed: seal.accounts.size,
                    holds: accounts.len() as u64,
                });
            };
            if actual != seal.accounts {
                return Err(SealChainError::AccountsRebound {
                    period: seal.period.clone(),
                    size: seal.accounts.size,
                    expected: seal.accounts.root,
                    found: actual.root,
                });
            }
        }
        Ok(())
    }

    /// Verifies every seal and every link.
    ///
    /// Linear in the number of seals: the periods seen so far are carried along
    /// rather than rescanned at each position. They are re-derived from the
    /// seals rather than read from the index [`SealChain::push`] maintains —
    /// verification that trusted its own cache would only be checking that the
    /// cache agreed with itself.
    ///
    /// # Errors
    ///
    /// Returns the first [`SealChainError`] that does not hold.
    pub fn verify(&self) -> Result<(), SealChainError> {
        let mut seen: BTreeSet<PeriodId> = BTreeSet::new();
        let mut previous: Option<&Seal> = None;
        for seal in &self.seals {
            Self::check_link(&self.ledger, |period| seen.contains(period), previous, seal)?;
            seen.insert(seal.period.clone());
            previous = Some(seal);
        }
        Ok(())
    }

    /// Verifies the chain, and then that it describes **this** log.
    ///
    /// [`verify`](Self::verify) is what a recipient runs: it takes seals alone
    /// and establishes that they are in order, unedited, and that each period's
    /// tree extends the one before it. What it cannot establish is that the tree
    /// they describe is the tree you are holding — a complete, internally
    /// consistent chain can be built over a history nobody has.
    ///
    /// This is the operator's counterpart. For each seal it recomputes the log's
    /// head at that seal's size and compares. Passing means every seal in the
    /// chain is a commitment to a prefix of this log, at the size it claims.
    ///
    /// Run it after a restore, after a migration, and before handing a chain to
    /// anyone: it is the one check that ties the evidence to the books.
    ///
    /// `O(n·m)` for `m` seals over an `n`-entry log, since each head is a
    /// recomputation. That is an audit-time cost, not a write-path one.
    ///
    /// # Errors
    ///
    /// Returns the first [`SealChainError`] that does not hold, including
    /// [`SealChainError::HeadMismatch`] when a seal's tree head is not the head
    /// this log had at that size, and
    /// [`SealChainError::BeyondTheLog`] when a seal commits to
    /// more entries than the log holds.
    pub fn verify_against_log(&self, log: &MerkleLog) -> Result<(), SealChainError> {
        self.verify()?;
        for seal in &self.seals {
            let actual =
                log.head_at(seal.tree_head.size)
                    .map_err(|_| SealChainError::BeyondTheLog {
                        period: seal.period.clone(),
                        claimed: seal.tree_head.size,
                        holds: log.len(),
                    })?;
            if actual != seal.tree_head {
                return Err(SealChainError::HeadMismatch {
                    period: seal.period.clone(),
                    expected: seal.tree_head.root,
                    found: actual.root,
                });
            }
        }
        Ok(())
    }

    /// The seals, oldest first.
    #[must_use]
    pub fn seals(&self) -> &[Seal] {
        &self.seals
    }

    /// Number of seals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seals.len()
    }

    /// True when nothing has been sealed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seals.is_empty()
    }

    /// The seal covering a period, if it has been sealed.
    #[must_use]
    pub fn get(&self, period: &PeriodId) -> Option<&Seal> {
        self.seals.get(*self.periods.get(period)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, AccountRegistry};
    use crate::merkle::MerkleLog;
    use crate::money::{Amount, Currency};
    use crate::posting::{Direction, Layer};
    use time::macros::date;

    type Eur = Amount<2>;

    fn lid() -> LedgerId {
        LedgerId::new("test-ledger").expect("valid")
    }

    fn pid(s: &str) -> PeriodId {
        PeriodId::new(s).expect("valid")
    }

    fn head(size: u64, byte: u8) -> TreeHead {
        TreeHead {
            size,
            root: Hash::from_bytes([byte; 32]),
        }
    }

    /// A real log of `n` leaves.
    ///
    /// Seals are built over a log rather than a bare head, because each one
    /// derives a consistency proof from its predecessor's tree — so a chain can
    /// only be assembled over leaves that actually exist. Invented roots would
    /// have no prefixes to prove anything about.
    fn log_of(n: u64) -> MerkleLog {
        let mut log = MerkleLog::new();
        for i in 0..n {
            let mut bytes = [0u8; 32];
            // A distinct leaf per position, so no two prefixes share a root.
            if let Some(slot) = bytes.get_mut(..8) {
                slot.copy_from_slice(&i.to_le_bytes());
            }
            log.append(Hash::from_bytes(bytes));
        }
        log
    }

    /// A seal edited after the fact and re-hashed so it is self-consistent.
    ///
    /// What an attacker with write access to the seal table actually produces:
    /// `is_self_consistent` passes, because they recomputed the hash. It is the
    /// *chain* rules that have to catch these, which is what the tests using
    /// this assert.
    fn forged(seal: &Seal, edit: impl FnOnce(&mut Seal)) -> Seal {
        let mut forged = seal.clone();
        edit(&mut forged);
        forged.seal_hash = forged.compute_hash();
        forged
    }

    /// A registry commitment for the tests that do not exercise bindings.
    ///
    /// Fixed rather than derived: these tests are about the seal preimage and
    /// the chain, and the binding proofs have their own tests below.
    fn accounts_head() -> TreeHead {
        TreeHead {
            size: 2,
            root: Hash::from_bytes([0xa0; 32]),
        }
    }

    /// A registry with two accounts, and the commitment over it.
    fn registry() -> AccountRegistry {
        let mut r = AccountRegistry::new();
        for path in ["Assets:Cash", "Income:Sales"] {
            r.register_path(path, date!(2026 - 01 - 01))
                .expect("registers");
        }
        r
    }

    fn tb(entries: &[(u32, i64, i64)]) -> TrialBalance<2> {
        let mut tb = TrialBalance::<2>::new();
        for (account, debit, credit) in entries {
            let key = BalanceKey {
                account: AccountId::from_index(*account),
                currency: Currency::EUR,
                layer: Layer::Settled,
            };
            let mut balance = Balance::<2>::ZERO;
            balance
                .add(Direction::Debit, Eur::from_minor(*debit))
                .expect("ok");
            balance
                .add(Direction::Credit, Eur::from_minor(*credit))
                .expect("ok");
            tb.set(key, balance);
        }
        tb
    }

    #[test]
    fn a_seal_hashes_its_own_contents() {
        let seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 9, 10),
            &log_of(10),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        assert!(seal.is_self_consistent());
        assert_eq!(seal.entry_count, 10);
    }

    #[test]
    fn editing_any_field_invalidates_the_seal() {
        let original = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 9, 10),
            &log_of(10),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");

        let mut altered = original.clone();
        altered.last_index = Some(8);
        assert!(!altered.is_self_consistent());

        let mut retargeted = original.clone();
        retargeted.tree_head = head(11, 1);
        assert!(!retargeted.is_self_consistent());

        let mut restated = original;
        restated.trial_balance.root = Hash::from_bytes([9u8; 32]);
        assert!(!restated.is_self_consistent());
    }

    #[test]
    fn the_trial_balance_head_reflects_the_balances() {
        let a = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::spanning(0, 1, 0),
            &log_of(2),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        let b = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::spanning(0, 1, 0),
            &log_of(2),
            &tb(&[(0, 101, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        assert_ne!(a.trial_balance, b.trial_balance);
    }

    #[test]
    fn gross_totals_are_covered_not_just_the_net() {
        // Both net to zero; a root over nets alone could not tell them apart.
        let quiet = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            &log_of(0),
            &tb(&[(0, 0, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        let busy = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            &log_of(0),
            &tb(&[(0, 500, 500)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        assert_ne!(quiet.trial_balance, busy.trial_balance);
    }

    #[test]
    fn seals_chain_and_verify() {
        let mut chain = SealChain::new(lid());
        let first = Seal::build(
            lid(),
            pid("2026-01"),
            PeriodCoverage::spanning(0, 4, 0),
            &log_of(5),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        chain.push(first.clone()).expect("genesis");

        let second = Seal::build(
            lid(),
            pid("2026-02"),
            PeriodCoverage::spanning(5, 9, 5),
            &log_of(10),
            &tb(&[(0, 200, 0)]),
            accounts_head(),
            Some(&first),
        )
        .expect("builds");
        chain.push(second).expect("chains");

        assert_eq!(chain.len(), 2);
        assert!(chain.verify().is_ok());
        assert_eq!(
            chain.get(&pid("2026-01")).map(|s| s.period.clone()),
            Some(pid("2026-01"))
        );
    }

    #[test]
    fn a_chain_refuses_a_foreign_seal_even_as_its_first() {
        // The ledger is in the seal preimage precisely so a seal says whose
        // books it attests to. A chain that only compared a seal against its
        // predecessor would not start enforcing that until the second period —
        // so the very first seal of a foreign ledger would be accepted.
        let mut chain = SealChain::new(lid());
        let foreign = Seal::build(
            LedgerId::new("someone-else").expect("valid"),
            pid("2026-01"),
            PeriodCoverage::EMPTY,
            &log_of(1),
            &tb(&[]),
            accounts_head(),
            None,
        )
        .expect("builds");
        assert!(matches!(
            chain.push(foreign),
            Err(SealChainError::ForeignLedger { .. })
        ));
        assert!(chain.is_empty(), "a refused seal appends nothing");
        assert_eq!(chain.ledger(), &lid());
    }

    #[test]
    fn one_period_may_not_be_sealed_twice_in_a_chain() {
        // Two commitments to one period's closing balances, with nothing in the
        // chain saying which the books mean.
        let first = Seal::build(
            lid(),
            pid("2026-01"),
            PeriodCoverage::spanning(0, 4, 5),
            &log_of(5),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        let restated = Seal::build(
            lid(),
            pid("2026-01"),
            PeriodCoverage::spanning(0, 4, 5),
            &log_of(5),
            &tb(&[(0, 999, 0)]),
            accounts_head(),
            Some(&first),
        )
        .expect("builds");

        let mut chain = SealChain::new(lid());
        chain.push(first).expect("genesis");
        assert!(matches!(
            chain.push(restated),
            Err(SealChainError::DuplicatePeriod { .. })
        ));
    }

    #[test]
    fn from_seals_rebuilds_and_re_checks_a_stored_chain() {
        let first = Seal::build(
            lid(),
            pid("2026-01"),
            PeriodCoverage::EMPTY,
            &log_of(1),
            &tb(&[]),
            accounts_head(),
            None,
        )
        .expect("builds");
        let second = Seal::build(
            lid(),
            pid("2026-02"),
            PeriodCoverage::EMPTY,
            &log_of(2),
            &tb(&[]),
            accounts_head(),
            Some(&first),
        )
        .expect("builds");

        let chain = SealChain::from_seals(lid(), [first.clone(), second.clone()]).expect("chains");
        assert_eq!(chain.len(), 2);
        assert!(chain.verify().is_ok());

        // Out of order is not a chain, and neither is the wrong ledger.
        assert!(SealChain::from_seals(lid(), [second, first.clone()]).is_err());
        assert!(
            SealChain::from_seals(LedgerId::new("elsewhere").expect("valid"), [first]).is_err()
        );
    }

    #[test]
    fn a_seal_that_does_not_reference_the_head_is_refused() {
        let mut chain = SealChain::new(lid());
        let first = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::spanning(0, 0, 1),
            &log_of(1),
            &tb(&[]),
            accounts_head(),
            None,
        )
        .expect("builds");
        chain.push(first.clone()).expect("genesis");

        // A seal that chains onto something that is not the head: built legally
        // against `first`, then re-pointed and re-hashed, so it is internally
        // perfect and belongs to a chain nobody has.
        let orphan = forged(
            &Seal::build(
                lid(),
                pid("b"),
                PeriodCoverage::spanning(1, 1, 1),
                &log_of(2),
                &tb(&[]),
                accounts_head(),
                Some(&first),
            )
            .expect("builds"),
            |s| s.prev_seal = Some(Hash::from_bytes([7u8; 32])),
        );
        assert!(orphan.is_self_consistent(), "re-hashed, as a forger would");
        assert!(matches!(
            chain.push(orphan),
            Err(SealChainError::BrokenChain { .. })
        ));
    }

    #[test]
    fn only_the_first_seal_may_omit_a_predecessor() {
        let mut chain = SealChain::new(lid());
        chain
            .push(
                Seal::build(
                    lid(),
                    pid("a"),
                    PeriodCoverage::EMPTY,
                    &log_of(1),
                    &tb(&[]),
                    accounts_head(),
                    None,
                )
                .expect("builds"),
            )
            .expect("genesis");

        let second_genesis = Seal::build(
            lid(),
            pid("b"),
            PeriodCoverage::EMPTY,
            &log_of(2),
            &tb(&[]),
            accounts_head(),
            None,
        )
        .expect("builds");
        assert!(matches!(
            chain.push(second_genesis),
            Err(SealChainError::MisplacedGenesis { .. })
        ));
    }

    #[test]
    fn a_first_seal_may_not_claim_a_predecessor() {
        let mut chain = SealChain::new(lid());
        let bogus = forged(
            &Seal::build(
                lid(),
                pid("a"),
                PeriodCoverage::EMPTY,
                &log_of(1),
                &tb(&[]),
                accounts_head(),
                None,
            )
            .expect("builds"),
            |s| s.prev_seal = Some(Hash::from_bytes([3u8; 32])),
        );
        assert!(matches!(
            chain.push(bogus),
            Err(SealChainError::MisplacedGenesis { .. })
        ));
    }

    #[test]
    fn the_tree_may_not_shrink_between_seals() {
        let mut chain = SealChain::new(lid());
        let first = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::spanning(0, 9, 10),
            &log_of(10),
            &tb(&[]),
            accounts_head(),
            None,
        )
        .expect("builds");
        chain.push(first.clone()).expect("genesis");

        // A shrunken tree cannot be *built* — the log has no such prefix to
        // prove — so this is the forged form: a legal seal edited down and
        // re-hashed, which is what someone rewriting the seal table produces.
        let shrunk = forged(
            &Seal::build(
                lid(),
                pid("b"),
                PeriodCoverage::spanning(0, 4, 0),
                &log_of(10),
                &tb(&[]),
                accounts_head(),
                Some(&first),
            )
            .expect("builds"),
            |s| s.tree_head = head(5, 2),
        );
        assert!(matches!(
            chain.push(shrunk),
            Err(SealChainError::NonMonotonic { .. })
        ));
    }

    #[test]
    fn the_account_registry_may_not_shrink_between_seals() {
        // Handles are dense positions and are never reissued, so a registry only
        // grows. A later seal committing to fewer bindings is a registry rebuilt
        // from a truncated set — which renumbers the handles every earlier
        // balance is keyed on, while every seal hash still checks out.
        let mut chain = SealChain::new(lid());
        let first = Seal::build(
            lid(),
            pid("2026-01"),
            PeriodCoverage::spanning(0, 4, 5),
            &log_of(5),
            &tb(&[(0, 100, 0)]),
            TreeHead {
                size: 12,
                root: Hash::from_bytes([0xa0; 32]),
            },
            None,
        )
        .expect("builds");
        chain.push(first.clone()).expect("genesis");

        let renumbered = Seal::build(
            lid(),
            pid("2026-02"),
            PeriodCoverage::spanning(5, 9, 5),
            &log_of(10),
            &tb(&[(0, 200, 0)]),
            TreeHead {
                size: 9,
                root: Hash::from_bytes([0xa1; 32]),
            },
            Some(&first),
        )
        .expect("builds");
        assert!(matches!(
            chain.push(renumbered),
            Err(SealChainError::ShrunkenRegistry {
                expected: 12,
                found: 9,
                ..
            })
        ));

        // Growing, or holding steady, is ordinary.
        for size in [12, 13] {
            let grown = Seal::build(
                lid(),
                pid("2026-02"),
                PeriodCoverage::spanning(5, 9, 5),
                &log_of(10),
                &tb(&[(0, 200, 0)]),
                TreeHead {
                    size,
                    root: Hash::from_bytes([0xa2; 32]),
                },
                Some(&first),
            )
            .expect("builds");
            let mut fresh = SealChain::new(lid());
            fresh.push(first.clone()).expect("genesis");
            fresh.push(grown).expect("a registry that grew");
            assert!(fresh.verify().is_ok());
        }
    }

    #[test]
    fn tampering_with_a_sealed_period_is_detected_by_the_chain() {
        let mut chain = SealChain::new(lid());
        let first = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::spanning(0, 4, 0),
            &log_of(5),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        chain.push(first.clone()).expect("genesis");
        chain
            .push(
                Seal::build(
                    lid(),
                    pid("b"),
                    PeriodCoverage::spanning(5, 9, 5),
                    &log_of(10),
                    &tb(&[(0, 200, 0)]),
                    accounts_head(),
                    Some(&first),
                )
                .expect("builds"),
            )
            .expect("chains");
        assert!(chain.verify().is_ok());

        // Restate the first period after the fact.
        let mut tampered = chain;
        if let Some(seal) = tampered.seals.first_mut() {
            seal.trial_balance.root = Hash::from_bytes([0xffu8; 32]);
        }
        assert!(matches!(
            tampered.verify(),
            Err(SealChainError::Tampered { .. })
        ));
    }

    #[test]
    fn a_balance_can_be_proven_against_a_seal_alone() {
        let trial = tb(&[(0, 119_000, 0), (1, 0, 119_000), (2, 500, 500)]);
        let seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 3, 4),
            &log_of(4),
            &trial,
            accounts_head(),
            None,
        )
        .expect("builds");

        let commitment = TrialBalanceCommitment::of(&trial);
        assert_eq!(commitment.head(), seal.trial_balance);
        assert_eq!(commitment.len(), 3);

        // Every row proves, against nothing but the seal.
        for proof in commitment.prove_all().expect("well-formed") {
            assert!(proof.verify_against(&seal), "{:?} must prove", proof.key);
        }
    }

    #[test]
    fn a_restated_balance_does_not_prove() {
        let trial = tb(&[(0, 119_000, 0), (1, 0, 119_000)]);
        let seal = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            &log_of(2),
            &trial,
            accounts_head(),
            None,
        )
        .expect("builds");
        let key = BalanceKey {
            account: AccountId::from_index(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let mut proof = TrialBalanceCommitment::of(&trial)
            .prove(&key)
            .expect("row exists");
        assert!(proof.verify_against(&seal));

        // Claiming a different number under the same path.
        proof.balance.debits = Eur::from_minor(119_001);
        assert!(!proof.verify_against(&seal));

        // Claiming the same number for a different account.
        let mut retargeted = TrialBalanceCommitment::of(&trial)
            .prove(&key)
            .expect("row exists");
        retargeted.key.account = AccountId::from_index(7);
        assert!(!retargeted.verify_against(&seal));
    }

    #[test]
    fn an_account_with_no_row_cannot_be_proven_to_be_zero() {
        // Absence is not a zero balance, and a proof of it must not be invented.
        let trial = tb(&[(0, 100, 0)]);
        let absent = BalanceKey {
            account: AccountId::from_index(9),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        assert!(TrialBalanceCommitment::of(&trial).prove(&absent).is_none());
    }

    #[test]
    fn a_proof_does_not_carry_across_seals() {
        let march = tb(&[(0, 100, 0), (1, 0, 100)]);
        let april = tb(&[(0, 250, 0), (1, 0, 250)]);
        let march_seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::EMPTY,
            &log_of(2),
            &march,
            accounts_head(),
            None,
        )
        .expect("builds");
        let key = BalanceKey {
            account: AccountId::from_index(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let april_proof = TrialBalanceCommitment::of(&april)
            .prove(&key)
            .expect("row exists");
        assert!(!april_proof.verify_against(&march_seal));
    }

    #[test]
    fn a_seal_binds_the_handles_its_balances_are_keyed_on() {
        // Renumbering the registry must not leave the seal verifying. Before
        // `accounts` head it did: the trial balance is keyed on handles, so
        // re-registering the same paths in a different order produced a byte-
        // identical seal that now meant something else entirely.
        let trial = tb(&[(0, 100, 0), (1, 0, 100)]);
        let seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 1, 2),
            &log_of(2),
            &trial,
            registry().commitment(),
            None,
        )
        .expect("builds");

        let mut renumbered = AccountRegistry::new();
        for path in ["Income:Sales", "Assets:Cash"] {
            renumbered
                .register_path(path, date!(2026 - 01 - 01))
                .expect("registers");
        }

        // Same paths, same balances, same everything the old seal covered …
        assert_eq!(trial_balance_head(&trial), seal.trial_balance);
        // … but the handles they hang off are different, and the seal says so.
        assert_ne!(renumbered.commitment(), seal.accounts);

        // And the difference is *checked*, not merely available to check. Before
        // `verify_against_accounts` nothing compared the two on any routine
        // path: the chain verified, the log verified, `audit` passed, and the
        // renumbering surfaced only if somebody happened to ask for a sealed
        // balance.
        let mut chain = SealChain::new(lid());
        chain.push(seal.clone()).expect("chains");

        chain
            .verify_against_accounts(&registry())
            .expect("the registry the seal was taken against");

        assert!(matches!(
            chain.verify_against_accounts(&renumbered),
            Err(SealChainError::AccountsRebound { size: 2, .. })
        ));
    }

    #[test]
    fn a_seal_is_checked_against_the_registry_as_it_stood_then() {
        // The registry grows after a period seals — that is the normal case, and
        // a check against its *current* commitment would fail for every seal but
        // the last. It has to compare against the prefix the seal named.
        let trial = tb(&[(0, 100, 0), (1, 0, 100)]);
        let seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 1, 2),
            &log_of(2),
            &trial,
            registry().commitment(),
            None,
        )
        .expect("builds");
        let mut chain = SealChain::new(lid());
        chain.push(seal).expect("chains");

        let mut grown = registry();
        grown
            .register_path("Expense:Rent", date!(2026 - 01 - 01))
            .expect("registers");
        assert_ne!(grown.commitment().size, 2, "the registry really did grow");

        chain
            .verify_against_accounts(&grown)
            .expect("growth is not tampering");

        // Losing bindings is, though: a seal cannot be checked against a
        // registry that never reached the size it names.
        let mut truncated = AccountRegistry::new();
        truncated
            .register_path("Assets:Cash", date!(2026 - 01 - 01))
            .expect("registers");
        assert!(matches!(
            chain.verify_against_accounts(&truncated),
            Err(SealChainError::BeyondTheRegistry {
                claimed: 2,
                holds: 1,
                ..
            })
        ));
    }

    #[test]
    fn a_balance_proof_can_name_the_account_it_is_about() {
        let accounts = registry();
        let cash = accounts
            .id_of(&crate::account::AccountPath::parse("Assets:Cash").expect("valid"))
            .expect("registered");

        let trial = tb(&[(cash.index(), 119_000, 0), (1, 0, 119_000)]);
        let seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 1, 2),
            &log_of(2),
            &trial,
            accounts.commitment(),
            None,
        )
        .expect("builds");

        let key = BalanceKey {
            account: cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let balance = TrialBalanceCommitment::of(&trial)
            .prove(&key)
            .expect("row exists");
        let binding = accounts.prove_binding(cash).expect("registered");

        // The complete claim: this account, this balance, this seal.
        assert!(balance.verify_naming(&binding, &seal));
        assert_eq!(binding.path().to_string(), "Assets:Cash");

        // A binding for a *different* handle must not launder a real balance
        // under the wrong account's name.
        let other = accounts
            .prove_binding(AccountId::from_index(1))
            .expect("registered");
        assert!(!balance.verify_naming(&other, &seal));

        // And a genuine binding proves nothing against a seal from a registry
        // that never contained it.
        let foreign = Seal::build(
            lid(),
            pid("2026-04"),
            PeriodCoverage::EMPTY,
            &log_of(2),
            &trial,
            accounts_head(),
            None,
        )
        .expect("builds");
        assert!(!balance.verify_naming(&binding, &foreign));
    }

    #[test]
    fn a_binding_proof_cannot_be_replayed_at_another_handle() {
        let accounts = registry();
        let root = accounts.commitment();
        let mut proof = accounts
            .prove_binding(AccountId::from_index(0))
            .expect("registered");
        assert!(proof.verify(&root));

        // Claiming the same account sits at a different position.
        proof.id = AccountId::from_index(1);
        assert!(!proof.verify(&root));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_deserialised_seal_is_checked_against_its_own_hash() {
        let seal = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            &log_of(1),
            &tb(&[(0, 100, 0)]),
            accounts_head(),
            None,
        )
        .expect("builds");
        let json = serde_json::to_string(&seal).expect("serialises");
        assert_eq!(
            serde_json::from_str::<Seal>(&json).expect("round-trips"),
            seal
        );

        // A field edited on the wire must not deserialise at all — a caller who
        // never thinks to call `is_self_consistent` still cannot be handed a
        // commitment that commits to nothing.
        let forged = json.replace("\"entry_count\":0", "\"entry_count\":99");
        assert_ne!(forged, json, "the test must actually alter the payload");
        assert!(serde_json::from_str::<Seal>(&forged).is_err());
    }

    #[test]
    fn an_empty_period_seals_with_no_entries() {
        let seal = Seal::build(
            lid(),
            pid("quiet"),
            PeriodCoverage::EMPTY,
            &log_of(0),
            &tb(&[]),
            accounts_head(),
            None,
        )
        .expect("builds");
        assert_eq!(seal.entry_count, 0);
        assert!(seal.is_self_consistent());
    }
}
