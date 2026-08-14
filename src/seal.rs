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
//! - Against [`Seal::trial_balance_root`], a [`BalanceProof`] that a specific
//!   account held a specific balance in the closing trial balance.
//! - Against [`Seal::accounts_root`], an
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
//! [`Seal::accounts_root`] closes both gaps at once. It pins the handle space to
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

use crate::account::AccountBindingProof;
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::canonical::{Canonical, CanonicalWriter};
use crate::hash::{Hash, tag, tagged};
use crate::merkle::{InclusionProof, MerkleLog, ProofError, TreeHead};
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
    /// Merkle root over the period's closing trial balance.
    ///
    /// The rows it commits to are keyed on account *handles*, so it is only
    /// meaningful together with [`Seal::accounts_root`], which says what those
    /// handles were.
    pub trial_balance_root: Hash,
    /// Merkle root over the handle-to-account bindings in force at sealing.
    ///
    /// A trial-balance leaf names an account by its handle — a dense integer,
    /// cheap to compare and to index by, and meaningless on its own. Without
    /// this field the handles would float: renumbering the registry after the
    /// fact, or re-registering the same paths in a different order, would leave
    /// every seal, every [`BalanceProof`] and the whole chain verifying exactly
    /// as before, while every balance in them silently referred to a different
    /// account. That is precisely the alteration a seal exists to make visible.
    ///
    /// It is also what makes selective disclosure complete. An
    /// [`AccountBindingProof`](crate::account::AccountBindingProof) against this
    /// root turns "handle `#7` held this balance" into "`Assets:Cash` held this
    /// balance", without revealing any other account.
    pub accounts_root: Hash,
    /// Hash of the preceding seal, or `None` for the first.
    pub prev_seal: Option<Hash>,
    /// Hash over every other field.
    pub seal_hash: Hash,
}

impl Seal {
    /// Builds a seal, computing every root and the chaining hash.
    ///
    /// `accounts_root` is an [`AccountRegistry::commitment`](crate::AccountRegistry::commitment)
    /// taken at the same moment as the trial balance. It has to be the registry
    /// the balances were computed against, or the seal commits to handles it
    /// does not explain.
    #[must_use]
    pub fn build<const P: u8>(
        ledger: LedgerId,
        period: PeriodId,
        coverage: PeriodCoverage,
        tree_head: TreeHead,
        trial_balance: &TrialBalance<P>,
        accounts_root: Hash,
        prev_seal: Option<Hash>,
    ) -> Self {
        let trial_balance_root = trial_balance_root(trial_balance);
        let mut seal = Self {
            ledger,
            period,
            first_index: coverage.first_index,
            last_index: coverage.last_index,
            entry_count: coverage.entry_count,
            tree_head,
            trial_balance_root,
            accounts_root,
            prev_seal,
            seal_hash: Hash::from_bytes([0u8; 32]),
        };
        seal.seal_hash = seal.compute_hash();
        seal
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
    trial_balance_root: Hash,
    accounts_root: Hash,
    prev_seal: Option<Hash>,
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
            trial_balance_root: raw.trial_balance_root,
            accounts_root: raw.accounts_root,
            prev_seal: raw.prev_seal,
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
        w.fixed(self.trial_balance_root.as_bytes());
        w.fixed(self.accounts_root.as_bytes());
        w.option(self.prev_seal.as_ref(), |w, v| {
            w.fixed(v.as_bytes());
        });
    }
}

/// Merkle root over a trial balance.
///
/// Shorthand for [`TrialBalanceCommitment::of`] followed by
/// [`TrialBalanceCommitment::root`]. Build the commitment instead when you also
/// want to prove individual rows.
#[must_use]
pub fn trial_balance_root<const P: u8>(tb: &TrialBalance<P>) -> Hash {
    TrialBalanceCommitment::of(tb).root()
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
/// assert!(proof.verify(&commitment.root()));
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
    /// Leaves follow the trial balance's own deterministic order, so the root is
    /// a pure function of the balances and not of how they were accumulated.
    #[must_use]
    pub fn of(tb: &TrialBalance<P>) -> Self {
        let rows: Vec<(BalanceKey, Balance<P>)> = tb.iter().map(|(k, b)| (*k, *b)).collect();
        let leaves = rows.iter().map(|(k, b)| balance_leaf(k, b)).collect();
        Self {
            rows,
            log: MerkleLog::from_leaves(leaves),
        }
    }

    /// The root a seal records.
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
        let index = self.rows.iter().position(|(k, _)| k == key)?;
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
    /// Verifies the claim against a [`Seal::trial_balance_root`].
    ///
    /// Returns `false` on any inconsistency rather than distinguishing failure
    /// modes: a verifier cannot act differently on a malformed proof than on a
    /// forged one.
    #[must_use]
    pub fn verify(&self, trial_balance_root: &Hash) -> bool {
        self.proof
            .verify(&balance_leaf(&self.key, &self.balance), trial_balance_root)
    }

    /// Verifies the claim against a whole seal.
    ///
    /// Establishes what a *handle* held. To learn which account that handle is,
    /// pair this with an
    /// [`AccountBindingProof`](crate::account::AccountBindingProof) against the
    /// same seal's [`accounts_root`](Seal::accounts_root) — or use
    /// [`BalanceProof::verify_naming`], which checks both together.
    #[must_use]
    pub fn verify_against(&self, seal: &Seal) -> bool {
        seal.is_self_consistent() && self.verify(&seal.trial_balance_root)
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
            && binding.verify(&seal.accounts_root)
    }
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
}

/// An ordered chain of period seals.
#[derive(Debug, Clone, Default)]
pub struct SealChain {
    seals: Vec<Seal>,
}

impl SealChain {
    /// Creates an empty chain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    /// Every rule that relates a seal to the one before it.
    ///
    /// Shared by [`SealChain::push`] and [`SealChain::verify`] so that appending
    /// and re-checking cannot drift apart — a chain that accepted a link it
    /// would later reject, or the reverse, would be worse than either rule alone.
    fn check_link(previous: Option<&Seal>, seal: &Seal) -> Result<(), SealChainError> {
        let period = || seal.period.clone();
        if !seal.is_self_consistent() {
            return Err(SealChainError::Tampered { period: period() });
        }
        if let Some(prev) = previous
            && prev.ledger != seal.ledger
        {
            return Err(SealChainError::ForeignLedger {
                period: period(),
                expected: prev.ledger.clone(),
                found: seal.ledger.clone(),
            });
        }
        match (previous, seal.prev_seal) {
            (None, None) => Ok(()),
            (Some(prev), Some(reference)) => {
                if prev.seal_hash != reference {
                    return Err(SealChainError::BrokenChain { period: period() });
                }
                if seal.tree_head.size < prev.tree_head.size {
                    return Err(SealChainError::NonMonotonic { period: period() });
                }
                Ok(())
            }
            _ => Err(SealChainError::MisplacedGenesis { period: period() }),
        }
    }

    /// Appends a seal, checking that it chains to the current head.
    pub fn push(&mut self, seal: Seal) -> Result<(), SealChainError> {
        Self::check_link(self.seals.last(), &seal)?;
        self.seals.push(seal);
        Ok(())
    }

    /// Verifies every seal and every link.
    pub fn verify(&self) -> Result<(), SealChainError> {
        let mut previous: Option<&Seal> = None;
        for seal in &self.seals {
            Self::check_link(previous, seal)?;
            previous = Some(seal);
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
        self.seals.iter().find(|s| s.period == *period)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, AccountRegistry};
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

    /// A registry commitment for the tests that do not exercise bindings.
    ///
    /// Fixed rather than derived: these tests are about the seal preimage and
    /// the chain, and the binding proofs have their own tests below.
    fn accounts_root() -> Hash {
        Hash::from_bytes([0xa0; 32])
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
            head(10, 1),
            &tb(&[(0, 100, 0)]),
            accounts_root(),
            None,
        );
        assert!(seal.is_self_consistent());
        assert_eq!(seal.entry_count, 10);
    }

    #[test]
    fn editing_any_field_invalidates_the_seal() {
        let original = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 9, 10),
            head(10, 1),
            &tb(&[(0, 100, 0)]),
            accounts_root(),
            None,
        );

        let mut altered = original.clone();
        altered.last_index = Some(8);
        assert!(!altered.is_self_consistent());

        let mut retargeted = original.clone();
        retargeted.tree_head = head(11, 1);
        assert!(!retargeted.is_self_consistent());

        let mut restated = original;
        restated.trial_balance_root = Hash::from_bytes([9u8; 32]);
        assert!(!restated.is_self_consistent());
    }

    #[test]
    fn the_trial_balance_root_reflects_the_balances() {
        let a = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::spanning(0, 1, 0),
            head(2, 1),
            &tb(&[(0, 100, 0)]),
            accounts_root(),
            None,
        );
        let b = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::spanning(0, 1, 0),
            head(2, 1),
            &tb(&[(0, 101, 0)]),
            accounts_root(),
            None,
        );
        assert_ne!(a.trial_balance_root, b.trial_balance_root);
    }

    #[test]
    fn gross_totals_are_covered_not_just_the_net() {
        // Both net to zero; a root over nets alone could not tell them apart.
        let quiet = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            head(0, 1),
            &tb(&[(0, 0, 0)]),
            accounts_root(),
            None,
        );
        let busy = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            head(0, 1),
            &tb(&[(0, 500, 500)]),
            accounts_root(),
            None,
        );
        assert_ne!(quiet.trial_balance_root, busy.trial_balance_root);
    }

    #[test]
    fn seals_chain_and_verify() {
        let mut chain = SealChain::new();
        let first = Seal::build(
            lid(),
            pid("2026-01"),
            PeriodCoverage::spanning(0, 4, 0),
            head(5, 1),
            &tb(&[(0, 100, 0)]),
            accounts_root(),
            None,
        );
        chain.push(first.clone()).expect("genesis");

        let second = Seal::build(
            lid(),
            pid("2026-02"),
            PeriodCoverage::spanning(5, 9, 5),
            head(10, 2),
            &tb(&[(0, 200, 0)]),
            accounts_root(),
            Some(first.seal_hash),
        );
        chain.push(second).expect("chains");

        assert_eq!(chain.len(), 2);
        assert!(chain.verify().is_ok());
        assert_eq!(
            chain.get(&pid("2026-01")).map(|s| s.period.clone()),
            Some(pid("2026-01"))
        );
    }

    #[test]
    fn a_seal_that_does_not_reference_the_head_is_refused() {
        let mut chain = SealChain::new();
        let first = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::spanning(0, 0, 1),
            head(1, 1),
            &tb(&[]),
            accounts_root(),
            None,
        );
        chain.push(first).expect("genesis");

        let orphan = Seal::build(
            lid(),
            pid("b"),
            PeriodCoverage::spanning(1, 1, 1),
            head(2, 2),
            &tb(&[]),
            accounts_root(),
            Some(Hash::from_bytes([7u8; 32])),
        );
        assert!(matches!(
            chain.push(orphan),
            Err(SealChainError::BrokenChain { .. })
        ));
    }

    #[test]
    fn only_the_first_seal_may_omit_a_predecessor() {
        let mut chain = SealChain::new();
        chain
            .push(Seal::build(
                lid(),
                pid("a"),
                PeriodCoverage::EMPTY,
                head(1, 1),
                &tb(&[]),
                accounts_root(),
                None,
            ))
            .expect("genesis");

        let second_genesis = Seal::build(
            lid(),
            pid("b"),
            PeriodCoverage::EMPTY,
            head(2, 2),
            &tb(&[]),
            accounts_root(),
            None,
        );
        assert!(matches!(
            chain.push(second_genesis),
            Err(SealChainError::MisplacedGenesis { .. })
        ));
    }

    #[test]
    fn a_first_seal_may_not_claim_a_predecessor() {
        let mut chain = SealChain::new();
        let bogus = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::EMPTY,
            head(1, 1),
            &tb(&[]),
            accounts_root(),
            Some(Hash::from_bytes([3u8; 32])),
        );
        assert!(matches!(
            chain.push(bogus),
            Err(SealChainError::MisplacedGenesis { .. })
        ));
    }

    #[test]
    fn the_tree_may_not_shrink_between_seals() {
        let mut chain = SealChain::new();
        let first = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::spanning(0, 9, 10),
            head(10, 1),
            &tb(&[]),
            accounts_root(),
            None,
        );
        chain.push(first.clone()).expect("genesis");

        let shrunk = Seal::build(
            lid(),
            pid("b"),
            PeriodCoverage::spanning(0, 4, 0),
            head(5, 2),
            &tb(&[]),
            accounts_root(),
            Some(first.seal_hash),
        );
        assert!(matches!(
            chain.push(shrunk),
            Err(SealChainError::NonMonotonic { .. })
        ));
    }

    #[test]
    fn tampering_with_a_sealed_period_is_detected_by_the_chain() {
        let mut chain = SealChain::new();
        let first = Seal::build(
            lid(),
            pid("a"),
            PeriodCoverage::spanning(0, 4, 0),
            head(5, 1),
            &tb(&[(0, 100, 0)]),
            accounts_root(),
            None,
        );
        chain.push(first.clone()).expect("genesis");
        chain
            .push(Seal::build(
                lid(),
                pid("b"),
                PeriodCoverage::spanning(5, 9, 5),
                head(10, 2),
                &tb(&[(0, 200, 0)]),
                accounts_root(),
                Some(first.seal_hash),
            ))
            .expect("chains");
        assert!(chain.verify().is_ok());

        // Restate the first period after the fact.
        let mut tampered = chain;
        if let Some(seal) = tampered.seals.first_mut() {
            seal.trial_balance_root = Hash::from_bytes([0xffu8; 32]);
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
            head(4, 1),
            &trial,
            accounts_root(),
            None,
        );

        let commitment = TrialBalanceCommitment::of(&trial);
        assert_eq!(commitment.root(), seal.trial_balance_root);
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
            head(2, 1),
            &trial,
            accounts_root(),
            None,
        );
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
            head(2, 1),
            &march,
            accounts_root(),
            None,
        );
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
        // `accounts_root` it did: the trial balance root is keyed on handles, so
        // re-registering the same paths in a different order produced a byte-
        // identical seal that now meant something else entirely.
        let trial = tb(&[(0, 100, 0), (1, 0, 100)]);
        let seal = Seal::build(
            lid(),
            pid("2026-03"),
            PeriodCoverage::spanning(0, 1, 2),
            head(2, 1),
            &trial,
            registry().commitment(),
            None,
        );

        let mut renumbered = AccountRegistry::new();
        for path in ["Income:Sales", "Assets:Cash"] {
            renumbered
                .register_path(path, date!(2026 - 01 - 01))
                .expect("registers");
        }

        // Same paths, same balances, same everything the old seal covered …
        assert_eq!(trial_balance_root(&trial), seal.trial_balance_root);
        // … but the handles they hang off are different, and the seal says so.
        assert_ne!(renumbered.commitment(), seal.accounts_root);
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
            head(2, 1),
            &trial,
            accounts.commitment(),
            None,
        );

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
        assert_eq!(binding.account().path.to_string(), "Assets:Cash");

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
            head(2, 1),
            &trial,
            accounts_root(),
            None,
        );
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
        proof.record.id = AccountId::from_index(1);
        assert!(!proof.verify(&root));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_deserialised_seal_is_checked_against_its_own_hash() {
        let seal = Seal::build(
            lid(),
            pid("p"),
            PeriodCoverage::EMPTY,
            head(1, 1),
            &tb(&[(0, 100, 0)]),
            accounts_root(),
            None,
        );
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
            head(0, 1),
            &tb(&[]),
            accounts_root(),
            None,
        );
        assert_eq!(seal.entry_count, 0);
        assert!(seal.is_self_consistent());
    }
}
