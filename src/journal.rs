//! The in-memory journal.
//!
//! A journal holds validated entries in append order, maintains the Merkle log
//! that commits to them, and enforces idempotency and reversal rules. It has no
//! storage of its own: it is the reference implementation of the engine's
//! semantics, the substrate for deterministic testing, and the thing a
//! persistence layer is expected to agree with.
//!
//! # Idempotency
//!
//! Recording is keyed by [`IdempotencyKey`]. Submitting the same key twice with
//! byte-identical content is a no-op that returns the original outcome, which is
//! what makes a retry safe across an at-least-once delivery path. Submitting the
//! same key with different content is a conflict and is refused — never a silent
//! overwrite, and never a second entry.

use std::collections::BTreeMap;

use time::Date;

use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::checkpoint::{AssertionOutcome, BalanceAssertion, Checkpoint, CheckpointError};
use crate::clearing::{
    Clearing, ClearingError, ClearingId, ClearingRegister, OpenItem, PostingLookup, PostingRef,
};
use crate::entry::{Balanced, Entry, EntryId, IdempotencyKey, SealContext, ValidationErrors};
use crate::hash::Hash;
use crate::merkle::{ConsistencyProof, InclusionProof, MerkleLog, ProofError, TreeHead};
use crate::money::{Currency, MoneyError};
use crate::period::{LedgerId, PeriodCalendar, PeriodId, PeriodState};
use crate::posting::Layer;
use crate::seal::{PeriodCoverage, Seal, SealChain, SealChainError};

/// Position of an entry in the journal.
///
/// Ordering is by log index rather than by timestamp: a wall clock is neither
/// monotonic nor agreed between writers, and the index is both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LogIndex(u64);

impl LogIndex {
    /// Wraps a raw index.
    #[must_use]
    pub const fn new(index: u64) -> Self {
        Self(index)
    }

    /// The underlying index.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for LogIndex {
    fn from(index: u64) -> Self {
        Self(index)
    }
}

impl std::fmt::Display for LogIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The result of recording an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    /// The entry's identifier.
    pub id: EntryId,
    /// Where it sits in the log, once it has been sequenced.
    ///
    /// `None` means the entry is durable and idempotency-checked but not yet
    /// assigned a position — a backend that sequences out of band returns this
    /// until its sequencer has run. Backends that sequence inline always return
    /// `Some`.
    ///
    /// The distinction is not an implementation leak. It is the difference
    /// between *recorded* and *committed to*: an entry with no index is safe
    /// from loss but cannot yet be proven to sit anywhere in particular.
    pub index: Option<LogIndex>,
    /// Its content hash.
    pub content_hash: Hash,
    /// False when an identical submission had already been recorded.
    pub is_new: bool,
}

impl Recorded {
    /// The log position, or an error naming the entry that has none yet.
    ///
    /// # Errors
    ///
    /// Returns [`NotSequenced`] when the entry has not been assigned a position.
    pub fn require_index(&self) -> Result<LogIndex, NotSequenced> {
        self.index.ok_or(NotSequenced { id: self.id })
    }
}

/// An entry was expected to have a log position and did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("entry {id} has been recorded but not yet sequenced")]
pub struct NotSequenced {
    /// The entry without a position.
    pub id: EntryId,
}

/// Failure recording an entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum JournalError {
    /// The entry did not pass validation.
    #[error(transparent)]
    Invalid(#[from] ValidationErrors),

    /// The idempotency key was reused with different content.
    #[error("idempotency key already used by entry {existing} with different content")]
    IdempotencyConflict {
        /// The entry already holding the key.
        existing: EntryId,
        /// Content hash of the stored entry.
        stored: Hash,
        /// Content hash of the submission.
        submitted: Hash,
    },

    /// The entry identifier was already used.
    #[error("entry {id} is already recorded")]
    DuplicateId {
        /// The offending identifier.
        id: EntryId,
    },

    /// A reversal referenced an entry that is not in the journal.
    #[error("cannot reverse unknown entry {id}")]
    UnknownOriginal {
        /// The referenced identifier.
        id: EntryId,
    },

    /// The referenced entry has already been reversed.
    #[error("entry {id} has already been reversed by {by}")]
    AlreadyReversed {
        /// The entry being reversed.
        id: EntryId,
        /// The reversal already on file.
        by: EntryId,
    },

    /// A reversal was aimed at another reversal.
    #[error("entry {id} is itself a reversal and cannot be reversed")]
    ReversalOfReversal {
        /// The offending identifier.
        id: EntryId,
    },

    /// An entry claiming to reverse another does not actually invert it.
    ///
    /// Postings must correspond one-to-one *in order*, which is what
    /// [`Entry::reverse`] produces. Building a reversal by hand and reordering
    /// its postings is refused rather than matched heuristically: a rule that
    /// guesses is a rule an auditor cannot check.
    #[error("entry claiming to reverse {id} does not invert its postings")]
    NotAnInversion {
        /// The entry it claims to reverse.
        id: EntryId,
    },

    /// Accumulating balances overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),

    /// A clearing was refused.
    #[error(transparent)]
    Clearing(#[from] ClearingError),

    /// The period is not ready to be sealed.
    #[error("period {period} is {state}; only a closing period can be sealed")]
    PeriodNotClosing {
        /// The period.
        period: PeriodId,
        /// Its current state.
        state: PeriodState,
    },

    /// The period is not defined in the calendar.
    #[error("period {period} is not defined")]
    UnknownPeriod {
        /// The missing period.
        period: PeriodId,
    },

    /// The seal did not chain onto the existing ones.
    #[error(transparent)]
    Seal(#[from] SealChainError),
}

/// An append-only journal of validated entries.
#[derive(Debug, Clone)]
pub struct Journal<const P: u8> {
    ledger: LedgerId,
    entries: Vec<Entry<Balanced, P>>,
    log: MerkleLog,
    by_id: BTreeMap<EntryId, LogIndex>,
    by_key: BTreeMap<IdempotencyKey, (EntryId, LogIndex, Hash)>,
    reversed_by: BTreeMap<EntryId, EntryId>,
    seals: SealChain,
    clearings: ClearingRegister<P>,
}

impl<const P: u8> Journal<P> {
    /// Creates an empty journal for one ledger.
    ///
    /// A journal is one entity's books, not a shared table: the ledger it names
    /// is bound into every seal it produces, so seals from two journals can
    /// never be mistaken for one chain.
    #[must_use]
    pub fn new(ledger: LedgerId) -> Self {
        Self {
            ledger,
            entries: Vec::new(),
            log: MerkleLog::new(),
            by_id: BTreeMap::new(),
            by_key: BTreeMap::new(),
            reversed_by: BTreeMap::new(),
            seals: SealChain::new(),
            clearings: ClearingRegister::new(),
        }
    }

    /// Validates a draft and records it.
    pub fn seal_and_record(
        &mut self,
        draft: crate::entry::Entry<crate::entry::Draft, P>,
        ctx: &SealContext<'_>,
    ) -> Result<Recorded, JournalError> {
        let entry = draft.seal(ctx)?;
        self.record(entry)
    }

    /// Records an already-validated entry.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::IdempotencyConflict`] when the key has been used
    /// for different content, and the reversal errors when the entry violates
    /// the correction rules.
    pub fn record(&mut self, entry: Entry<Balanced, P>) -> Result<Recorded, JournalError> {
        let content_hash = entry.content_hash();

        // Idempotency is settled before anything else: a safe retry must not be
        // able to trip a later rule that the original submission already passed.
        if let Some((existing_id, existing_index, stored)) =
            self.by_key.get(entry.idempotency_key())
        {
            if *stored == content_hash {
                return Ok(Recorded {
                    id: *existing_id,
                    index: Some(*existing_index),
                    content_hash,
                    is_new: false,
                });
            }
            return Err(JournalError::IdempotencyConflict {
                existing: *existing_id,
                stored: *stored,
                submitted: content_hash,
            });
        }

        if self.by_id.contains_key(&entry.id()) {
            return Err(JournalError::DuplicateId { id: entry.id() });
        }

        if let Some(original) = entry.reverses() {
            let Some(original_index) = self.by_id.get(&original) else {
                return Err(JournalError::UnknownOriginal { id: original });
            };
            if let Some(existing) = self.reversed_by.get(&original) {
                return Err(JournalError::AlreadyReversed {
                    id: original,
                    by: *existing,
                });
            }
            let position = usize::try_from(original_index.get()).unwrap_or(usize::MAX);
            let Some(target) = self.entries.get(position) else {
                return Err(JournalError::UnknownOriginal { id: original });
            };
            if target.reverses().is_some() {
                return Err(JournalError::ReversalOfReversal { id: original });
            }
            if !is_inversion_of(target, &entry) {
                return Err(JournalError::NotAnInversion { id: original });
            }
            self.reversed_by.insert(original, entry.id());
        }

        let index = LogIndex(self.log.append(content_hash));
        self.by_id.insert(entry.id(), index);
        self.by_key.insert(
            entry.idempotency_key().clone(),
            (entry.id(), index, content_hash),
        );
        let id = entry.id();
        self.entries.push(entry);

        Ok(Recorded {
            id,
            index: Some(index),
            content_hash,
            is_new: true,
        })
    }

    /// Number of recorded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, in log order.
    #[must_use]
    pub fn entries(&self) -> &[Entry<Balanced, P>] {
        &self.entries
    }

    /// The entry at a log index.
    #[must_use]
    pub fn at(&self, index: LogIndex) -> Option<&Entry<Balanced, P>> {
        self.entries
            .get(usize::try_from(index.get()).unwrap_or(usize::MAX))
    }

    /// The entry with a given identifier.
    #[must_use]
    pub fn get(&self, id: EntryId) -> Option<&Entry<Balanced, P>> {
        self.by_id.get(&id).and_then(|i| self.at(*i))
    }

    /// The reversal of an entry, if one has been recorded.
    #[must_use]
    pub fn reversal_of(&self, id: EntryId) -> Option<EntryId> {
        self.reversed_by.get(&id).copied()
    }

    /// The current tree head.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        self.log.head()
    }

    /// The tree head as of an earlier size.
    pub fn head_at(&self, size: u64) -> Result<TreeHead, ProofError> {
        Ok(TreeHead {
            size,
            root: self.log.root_at(size)?,
        })
    }

    /// Proves that the entry at `index` is committed to by the current head.
    pub fn prove_inclusion(&self, index: LogIndex) -> Result<InclusionProof, ProofError> {
        self.log.inclusion_proof(index.get())
    }

    /// Proves that the log at `old_size` is a prefix of the current log.
    pub fn prove_consistency(&self, old_size: u64) -> Result<ConsistencyProof, ProofError> {
        self.log.consistency_proof(old_size)
    }

    /// Folds the journal into a trial balance.
    ///
    /// Passing `through` restricts the fold to a prefix of the log, which is how
    /// a historical view is reconstructed: not "the balance on a date" but "the
    /// balance as the journal stood after `through` entries".
    pub fn trial_balance(&self, through: Option<LogIndex>) -> Result<TrialBalance<P>, MoneyError> {
        let limit = through.map_or(self.entries.len(), |i| {
            usize::try_from(i.get().saturating_add(1)).unwrap_or(usize::MAX)
        });
        let mut tb = TrialBalance::new();
        for entry in self.entries.iter().take(limit) {
            for posting in entry.postings() {
                tb.apply(posting)?;
            }
        }
        Ok(tb)
    }

    /// The balance of one account, currency, and layer.
    pub fn balance(
        &self,
        key: &BalanceKey,
        through: Option<LogIndex>,
    ) -> Result<Balance<P>, MoneyError> {
        Ok(self.trial_balance(through)?.get_or_zero(key))
    }

    /// Checks that debits equal credits across the whole journal.
    ///
    /// Every entry balances individually, so this must hold; running it is a
    /// direct test that the fold and the invariant have not drifted apart.
    pub fn verify_balanced(&self) -> Result<bool, MoneyError> {
        let tb = self.trial_balance(None)?;
        for currency in tb.currencies() {
            for layer in [Layer::Settled, Layer::Pending] {
                if !tb.totals(currency, layer)?.is_balanced() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Recomputes the Merkle log from the stored entries and compares it.
    ///
    /// Checks three things, because they can fail independently: the log commits
    /// to as many leaves as there are entries, those leaves are the entries'
    /// current content hashes, and the incrementally maintained subtree state
    /// still agrees with a full recomputation from the definition.
    #[must_use]
    pub fn verify_log(&self) -> bool {
        if self.log.len() != self.entries.len() as u64 {
            return false;
        }
        if !self.log.verify_incremental_state() {
            return false;
        }
        let recomputed =
            MerkleLog::from_leaves(self.entries.iter().map(Entry::content_hash).collect());
        recomputed.root() == self.log.root()
    }

    /// Currencies present in the journal, in deterministic order.
    pub fn currencies(&self) -> Result<Vec<Currency>, MoneyError> {
        Ok(self.trial_balance(None)?.currencies())
    }

    // ── statements ──────────────────────────────────────────────────────────

    /// Every posting touching `key`, in log order, with the running balance
    /// after each.
    ///
    /// This is the account statement a reader actually wants: a trial balance
    /// says where an account ended up, and says nothing about how it got there.
    pub fn statement(
        &self,
        key: &BalanceKey,
    ) -> Result<Vec<crate::storage::StatementLine<P>>, MoneyError> {
        let mut running = Balance::ZERO;
        let mut out = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            for (index, posting) in entry.postings().iter().enumerate() {
                if posting.account != key.account
                    || posting.currency != key.currency
                    || posting.layer != key.layer
                {
                    continue;
                }
                running.add(posting.direction, posting.amount)?;
                out.push(crate::storage::StatementLine {
                    index: LogIndex(i as u64),
                    posting: PostingRef::new(entry.id(), u16::try_from(index).unwrap_or(u16::MAX)),
                    booking_date: entry.booking_date(),
                    direction: posting.direction,
                    amount: posting.amount,
                    running,
                    kind: entry.kind().cloned(),
                });
            }
        }
        Ok(out)
    }

    /// References to every posting on `key`, in log order.
    #[must_use]
    pub fn postings_on(&self, key: &BalanceKey) -> Vec<PostingRef> {
        let mut out = Vec::new();
        for entry in &self.entries {
            for (index, posting) in entry.postings().iter().enumerate() {
                if posting.account == key.account
                    && posting.currency == key.currency
                    && posting.layer == key.layer
                {
                    out.push(PostingRef::new(
                        entry.id(),
                        u16::try_from(index).unwrap_or(u16::MAX),
                    ));
                }
            }
        }
        out
    }

    // ── clearing ────────────────────────────────────────────────────────────

    /// Records that a set of postings offset one another.
    pub fn clear(&mut self, clearing: Clearing<P>) -> Result<(), JournalError> {
        // Split the borrow: the register needs to read postings while mutating
        // its own state, so resolve against a snapshot of the entry index.
        let lookup = PostingIndex {
            by_id: &self.by_id,
            entries: &self.entries,
        };
        self.clearings.clear(clearing, &lookup)?;
        Ok(())
    }

    /// Releases a clearing, reopening the items it had assigned.
    pub fn reset_clearing(&mut self, id: ClearingId, on: Date) -> Result<(), JournalError> {
        self.clearings.reset(id, on)?;
        Ok(())
    }

    /// The clearing register.
    #[must_use]
    pub fn clearings(&self) -> &ClearingRegister<P> {
        &self.clearings
    }

    /// Postings on `key` with something still outstanding.
    pub fn open_items(&self, key: &BalanceKey) -> Result<Vec<OpenItem<P>>, JournalError> {
        let candidates = self.postings_on(key);
        let lookup = PostingIndex {
            by_id: &self.by_id,
            entries: &self.entries,
        };
        Ok(self.clearings.open_items(candidates, &lookup)?)
    }

    // ── periods and seals ───────────────────────────────────────────────────

    /// Smallest index range containing the period's entries, and how many there
    /// are.
    ///
    /// Entries are appended in recording order, not booking-date order, so a
    /// period's entries need not be contiguous — hence the separate count.
    fn index_span(&self, start: Date, end: Date) -> (Option<u64>, Option<u64>, u64) {
        let mut first = None;
        let mut last = None;
        let mut count = 0u64;
        for (i, entry) in self.entries.iter().enumerate() {
            let date = entry.booking_date();
            if date >= start && date <= end {
                let index = i as u64;
                first.get_or_insert(index);
                last = Some(index);
                count = count.saturating_add(1);
            }
        }
        (first, last, count)
    }

    /// Folds every entry booked on or before `end` into a trial balance.
    ///
    /// This is what a period's *closing* balance means: cumulative through the
    /// period's last day. Folding the whole journal instead would pull in
    /// entries booked into later periods, which is wrong whenever a period is
    /// sealed after the next one has begun — the normal case.
    pub fn trial_balance_through_date(&self, end: Date) -> Result<TrialBalance<P>, MoneyError> {
        let mut tb = TrialBalance::new();
        for entry in self.entries.iter().filter(|e| e.booking_date() <= end) {
            for posting in entry.postings() {
                tb.apply(posting)?;
            }
        }
        Ok(tb)
    }

    /// Seals a closing period, committing to its entries and closing balances.
    ///
    /// The period must already be in [`PeriodState::Closing`]: stopping new
    /// postings is a separate, earlier decision, so that verification runs
    /// against a set that can no longer grow underneath it.
    ///
    /// On success the calendar advances the period to [`PeriodState::Sealed`]
    /// and the seal is appended to the chain.
    pub fn seal_period(
        &mut self,
        period: &PeriodId,
        calendar: &mut PeriodCalendar,
    ) -> Result<Seal, JournalError> {
        let Some(definition) = calendar.get(period) else {
            return Err(JournalError::UnknownPeriod {
                period: period.clone(),
            });
        };
        if definition.state != PeriodState::Closing {
            return Err(JournalError::PeriodNotClosing {
                period: period.clone(),
                state: definition.state,
            });
        }

        let (first_index, last_index, entry_count) =
            self.index_span(definition.start, definition.end);
        let closing = self.trial_balance_through_date(definition.end)?;
        let seal = Seal::build(
            self.ledger.clone(),
            period.clone(),
            PeriodCoverage {
                first_index,
                last_index,
                entry_count,
            },
            self.head(),
            &closing,
            self.seals.head(),
        );

        self.seals.push(seal.clone())?;
        calendar
            .transition(period, PeriodState::Sealed)
            .map_err(|_| JournalError::UnknownPeriod {
                period: period.clone(),
            })?;
        Ok(seal)
    }

    /// The ledger these books belong to.
    #[must_use]
    pub fn ledger(&self) -> &LedgerId {
        &self.ledger
    }

    /// The chain of seals recorded so far.
    #[must_use]
    pub fn seals(&self) -> &SealChain {
        &self.seals
    }

    /// Verifies every seal and every link between them.
    pub fn verify_seals(&self) -> Result<(), SealChainError> {
        self.seals.verify()
    }

    // ── checkpoints and assertions ──────────────────────────────────────────

    /// Takes a checkpoint of one balance at the current log position.
    pub fn checkpoint(&self, key: &BalanceKey) -> Result<Checkpoint<P>, MoneyError> {
        let through = self.entries.len().checked_sub(1).map(|i| i as u64);
        Ok(Checkpoint::new(
            *key,
            through,
            self.balance(key, through.map(LogIndex))?,
            self.head(),
        ))
    }

    /// Re-derives a checkpoint from the journal and compares it.
    ///
    /// Checks the tree head as well as the balance: a checkpoint that matches
    /// numerically but was taken against a different history is stale, and
    /// silently trusting it would carry a stale balance forward.
    pub fn verify_checkpoint(&self, checkpoint: &Checkpoint<P>) -> Result<(), CheckpointError> {
        if let Some(index) = checkpoint.through_index
            && index >= self.len() as u64
        {
            return Err(CheckpointError::IndexOutOfRange { index });
        }

        let size = checkpoint.through_index.map_or(0, |i| i.saturating_add(1));
        let head = self
            .head_at(size)
            .map_err(|_| CheckpointError::IndexOutOfRange { index: size })?;
        if head != checkpoint.tree_head {
            return Err(CheckpointError::HeadMismatch);
        }

        let actual = self.balance(&checkpoint.key, checkpoint.through_index.map(LogIndex))?;
        if actual == checkpoint.balance {
            Ok(())
        } else {
            Err(CheckpointError::BalanceMismatch)
        }
    }

    /// Evaluates a balance assertion against the journal.
    pub fn check_assertion(
        &self,
        assertion: &BalanceAssertion<P>,
    ) -> Result<AssertionOutcome<P>, MoneyError> {
        let actual = self.balance(&assertion.key, assertion.at.map(LogIndex))?;
        assertion.check(&actual)
    }
}

/// Resolves posting references against the journal's entries.
struct PostingIndex<'a, const P: u8> {
    by_id: &'a BTreeMap<EntryId, LogIndex>,
    entries: &'a [Entry<Balanced, P>],
}

impl<const P: u8> PostingLookup<P> for PostingIndex<'_, P> {
    fn posting(&self, reference: PostingRef) -> Option<&crate::posting::Posting<P>> {
        let index = self.by_id.get(&reference.entry)?;
        let entry = self
            .entries
            .get(usize::try_from(index.get()).unwrap_or(usize::MAX))?;
        entry.postings().get(usize::from(reference.index))
    }
}

/// True when `candidate` is exactly `original` with every side flipped.
///
/// A reversal that does not invert would leave the original marked as reversed
/// while the amounts failed to net, which is worse than no reversal tracking at
/// all — the ledger would assert a correction it did not make.
fn is_inversion_of<const P: u8>(
    original: &Entry<Balanced, P>,
    candidate: &Entry<Balanced, P>,
) -> bool {
    let (a, b) = (original.postings(), candidate.postings());
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(o, r)| {
            r.account == o.account
                && r.amount == o.amount
                && r.currency == o.currency
                && r.layer == o.layer
                && r.dimensions == o.dimensions
                && r.direction == o.direction.inverse()
        })
}

#[cfg(test)]
mod tests {
    /// The ledger these tests keep their books in.
    fn test_ledger() -> LedgerId {
        LedgerId::new("test-ledger").expect("valid")
    }

    use super::*;
    use crate::account::{AccountId, AccountRegistry};
    use crate::entry::{Description, Draft, LedgerPolicy};
    use crate::money::Amount;
    use crate::period::PeriodCalendar;
    use time::macros::date;

    type Eur = Amount<2>;

    struct Fixture {
        accounts: AccountRegistry,
        calendar: PeriodCalendar,
        policy: LedgerPolicy,
        cash: AccountId,
        revenue: AccountId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut accounts = AccountRegistry::new();
            let cash = accounts
                .register_path("Assets:Cash", date!(2026 - 01 - 01))
                .expect("registers");
            let revenue = accounts
                .register_path("Income:Sales", date!(2026 - 01 - 01))
                .expect("registers");
            Self {
                accounts,
                calendar: PeriodCalendar::new(),
                policy: LedgerPolicy::default(),
                cash,
                revenue,
            }
        }

        fn ctx(&self) -> SealContext<'_> {
            SealContext {
                accounts: &self.accounts,
                calendar: &self.calendar,
                policy: &self.policy,
            }
        }

        fn draft(&self, key: &[u8], minor: i64) -> Entry<Draft, 2> {
            Entry::new(
                EntryId::generate(),
                IdempotencyKey::new(key.to_vec()).expect("valid"),
                date!(2026 - 03 - 15),
            )
            .debit(self.cash, Eur::from_minor(minor), Currency::EUR)
            .credit(self.revenue, Eur::from_minor(minor), Currency::EUR)
        }

        fn sealed(&self, key: &[u8], minor: i64) -> Entry<Balanced, 2> {
            self.draft(key, minor).seal(&self.ctx()).expect("balances")
        }
    }

    #[test]
    fn records_entries_in_order() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let a = j.record(f.sealed(b"a", 100)).expect("records");
        let b = j.record(f.sealed(b"b", 200)).expect("records");
        assert_eq!(a.index.expect("sequenced inline").get(), 0);
        assert_eq!(b.index.expect("sequenced inline").get(), 1);
        assert_eq!(j.len(), 2);
        assert!(a.is_new && b.is_new);
    }

    #[test]
    fn an_identical_resubmission_is_a_no_op() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let first = j.record(f.sealed(b"same-key", 100)).expect("records");

        // A different entry identifier, but the same logical transaction.
        let replay = j.record(f.sealed(b"same-key", 100)).expect("replays");
        assert!(!replay.is_new);
        assert_eq!(replay.id, first.id);
        assert_eq!(replay.index, first.index);
        assert_eq!(j.len(), 1, "a replay must not append");
    }

    #[test]
    fn the_same_key_with_different_content_is_a_conflict() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"key", 100)).expect("records");
        let err = j
            .record(f.sealed(b"key", 999))
            .expect_err("must not overwrite");
        assert!(matches!(err, JournalError::IdempotencyConflict { .. }));
        assert_eq!(j.len(), 1, "a conflict must not append");
    }

    #[test]
    fn duplicate_entry_ids_are_refused() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let entry = f.sealed(b"k1", 100);
        let id = entry.id();
        j.record(entry).expect("records");

        let clash = Entry::<Draft, 2>::new(
            id,
            IdempotencyKey::new(b"k2".to_vec()).expect("valid"),
            date!(2026 - 03 - 15),
        )
        .debit(f.cash, Eur::from_minor(1), Currency::EUR)
        .credit(f.revenue, Eur::from_minor(1), Currency::EUR)
        .seal(&f.ctx())
        .expect("balances");

        assert!(matches!(
            j.record(clash),
            Err(JournalError::DuplicateId { .. })
        ));
    }

    #[test]
    fn every_recorded_entry_can_be_proven_included() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        for i in 0..12i64 {
            let key = format!("k{i}");
            j.record(f.sealed(key.as_bytes(), 100 + i))
                .expect("records");
        }
        let head = j.head();
        for (i, entry) in j.entries().iter().enumerate() {
            let index = LogIndex(i as u64);
            let proof = j.prove_inclusion(index).expect("in range");
            assert!(
                proof.verify(&entry.content_hash(), &head.root),
                "entry {i} must be provably included"
            );
        }
    }

    #[test]
    fn growth_is_provably_append_only() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        for i in 0..5i64 {
            j.record(f.sealed(format!("a{i}").as_bytes(), 10 + i))
                .expect("records");
        }
        let early = j.head();

        for i in 0..7i64 {
            j.record(f.sealed(format!("b{i}").as_bytes(), 20 + i))
                .expect("records");
        }
        let later = j.head();

        let proof = j.prove_consistency(early.size).expect("in range");
        assert!(proof.verify(&early.root, &later.root));
    }

    #[test]
    fn a_reversal_requires_a_known_original() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let orphan = f.sealed(b"orphan", 100);
        let reversal = orphan
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("balances");
        assert!(matches!(
            j.record(reversal),
            Err(JournalError::UnknownOriginal { .. })
        ));
    }

    #[test]
    fn an_entry_can_only_be_reversed_once() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let original = f.sealed(b"orig", 100);
        j.record(original.clone()).expect("records");

        let first = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev1".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("balances");
        j.record(first).expect("records the reversal");

        let second = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev2".to_vec()).expect("valid"),
                date!(2026 - 04 - 02),
            )
            .seal(&f.ctx())
            .expect("balances");
        assert!(matches!(
            j.record(second),
            Err(JournalError::AlreadyReversed { .. })
        ));
    }

    #[test]
    fn a_reversal_cannot_itself_be_reversed() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let original = f.sealed(b"orig", 100);
        j.record(original.clone()).expect("records");

        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("balances");
        let reversal_id = reversal.id();
        j.record(reversal.clone()).expect("records");

        let double = reversal
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev-rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 02),
            )
            .seal(&f.ctx())
            .expect("balances");
        assert!(matches!(
            j.record(double),
            Err(JournalError::ReversalOfReversal { id }) if id == reversal_id
        ));
    }

    #[test]
    fn the_journal_always_balances() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        for i in 0..6i64 {
            j.record(f.sealed(format!("k{i}").as_bytes(), 10 + i))
                .expect("records");
        }
        assert!(j.verify_balanced().expect("no overflow"));
        assert!(j.verify_log());
    }

    #[test]
    fn a_reversal_nets_the_original_out() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let original = f.sealed(b"orig", 4200);
        j.record(original.clone()).expect("records");

        let before = j
            .balance(
                &BalanceKey {
                    account: f.cash,
                    currency: Currency::EUR,
                    layer: Layer::Settled,
                },
                None,
            )
            .expect("no overflow");
        assert_eq!(before.debits, Eur::from_minor(4200));

        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&f.ctx())
            .expect("balances");
        j.record(reversal).expect("records");

        let after = j
            .balance(
                &BalanceKey {
                    account: f.cash,
                    currency: Currency::EUR,
                    layer: Layer::Settled,
                },
                None,
            )
            .expect("no overflow");

        // Net is zero, but both gross totals survive: the reversal is visible.
        assert_eq!(after.signed_net().expect("ok"), Eur::ZERO);
        assert_eq!(after.debits, Eur::from_minor(4200));
        assert_eq!(after.credits, Eur::from_minor(4200));
    }

    #[test]
    fn a_prefix_fold_reconstructs_an_earlier_state() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        j.record(f.sealed(b"b", 200)).expect("records");
        j.record(f.sealed(b"c", 300)).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        assert_eq!(
            j.balance(&key, Some(LogIndex(0))).expect("ok").debits,
            Eur::from_minor(100)
        );
        assert_eq!(
            j.balance(&key, Some(LogIndex(1))).expect("ok").debits,
            Eur::from_minor(300)
        );
        assert_eq!(
            j.balance(&key, None).expect("ok").debits,
            Eur::from_minor(600)
        );
    }

    #[test]
    fn a_historical_head_matches_the_log_as_it_stood() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        let snapshot = j.head();
        j.record(f.sealed(b"b", 200)).expect("records");

        let reconstructed = j.head_at(snapshot.size).expect("in range");
        assert_eq!(reconstructed, snapshot);
        assert_ne!(j.head(), snapshot);
    }

    #[test]
    fn seal_and_record_rejects_an_invalid_draft() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let bad = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"bad".to_vec()).expect("valid"),
            date!(2026 - 03 - 15),
        )
        .debit(f.cash, Eur::from_minor(100), Currency::EUR)
        .credit(f.revenue, Eur::from_minor(99), Currency::EUR);

        assert!(matches!(
            j.seal_and_record(bad, &f.ctx()),
            Err(JournalError::Invalid(_))
        ));
        assert!(j.is_empty());
    }

    fn march(calendar: &mut PeriodCalendar) -> PeriodId {
        let id = PeriodId::new("2026-03").expect("valid");
        calendar
            .define(
                crate::period::Period::new(
                    id.clone(),
                    date!(2026 - 03 - 01),
                    date!(2026 - 03 - 31),
                )
                .expect("valid range"),
            )
            .expect("defines");
        id
    }

    #[test]
    fn sealing_requires_a_closing_period() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let mut calendar = PeriodCalendar::new();
        let id = march(&mut calendar);
        j.record(f.sealed(b"a", 100)).expect("records");

        // Open is not enough: postings must be stopped before verification runs.
        assert!(matches!(
            j.seal_period(&id, &mut calendar),
            Err(JournalError::PeriodNotClosing {
                state: PeriodState::Open,
                ..
            })
        ));

        calendar.transition(&id, PeriodState::Closing).expect("ok");
        let seal = j.seal_period(&id, &mut calendar).expect("seals");
        assert!(seal.is_self_consistent());
        assert_eq!(seal.entry_count, 1);
        assert_eq!(
            calendar.state_on(date!(2026 - 03 - 15)),
            PeriodState::Sealed
        );
    }

    #[test]
    fn sealing_an_unknown_period_is_an_error() {
        let mut j = Journal::<2>::new(test_ledger());
        let mut calendar = PeriodCalendar::new();
        let ghost = PeriodId::new("nope").expect("valid");
        assert!(matches!(
            j.seal_period(&ghost, &mut calendar),
            Err(JournalError::UnknownPeriod { .. })
        ));
    }

    #[test]
    fn consecutive_seals_chain_and_verify() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let mut calendar = PeriodCalendar::new();

        let march_id = march(&mut calendar);
        j.record(f.sealed(b"a", 100)).expect("records");
        calendar
            .transition(&march_id, PeriodState::Closing)
            .expect("ok");
        let first = j.seal_period(&march_id, &mut calendar).expect("seals");

        let april_id = PeriodId::new("2026-04").expect("valid");
        calendar
            .define(
                crate::period::Period::new(
                    april_id.clone(),
                    date!(2026 - 04 - 01),
                    date!(2026 - 04 - 30),
                )
                .expect("valid range"),
            )
            .expect("defines");
        calendar
            .transition(&april_id, PeriodState::Closing)
            .expect("ok");
        let second = j.seal_period(&april_id, &mut calendar).expect("seals");

        assert_eq!(first.prev_seal, None);
        assert_eq!(second.prev_seal, Some(first.seal_hash));
        assert_eq!(j.seals().len(), 2);
        assert!(j.verify_seals().is_ok());
    }

    #[test]
    fn a_seal_excludes_entries_booked_into_later_periods() {
        // Sealing March in April is the normal case; April must not leak into
        // March's closing balance.
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let mut calendar = PeriodCalendar::new();
        let id = march(&mut calendar);

        j.record(f.sealed(b"march", 100)).expect("records");

        let april = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"april".to_vec()).expect("valid"),
            date!(2026 - 04 - 10),
        )
        .debit(f.cash, Eur::from_minor(900), Currency::EUR)
        .credit(f.revenue, Eur::from_minor(900), Currency::EUR)
        .seal(&f.ctx())
        .expect("balances");
        j.record(april).expect("records");

        calendar.transition(&id, PeriodState::Closing).expect("ok");
        let seal = j.seal_period(&id, &mut calendar).expect("seals");

        assert_eq!(seal.entry_count, 1, "only the March entry belongs to March");

        let march_only = j
            .trial_balance_through_date(date!(2026 - 03 - 31))
            .expect("ok");
        assert_eq!(
            seal.trial_balance_root,
            crate::seal::trial_balance_root(&march_only)
        );
        assert_ne!(
            seal.trial_balance_root,
            crate::seal::trial_balance_root(&j.trial_balance(None).expect("ok")),
            "the whole-journal balance must not be what was sealed"
        );
    }

    #[test]
    fn a_seal_commits_to_the_balances_at_that_moment() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let mut calendar = PeriodCalendar::new();
        let id = march(&mut calendar);

        j.record(f.sealed(b"a", 100)).expect("records");
        calendar.transition(&id, PeriodState::Closing).expect("ok");
        let seal = j.seal_period(&id, &mut calendar).expect("seals");

        // A later entry cannot retroactively change what the seal committed to.
        let later = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"later".to_vec()).expect("valid"),
            date!(2026 - 04 - 05),
        )
        .debit(f.cash, Eur::from_minor(200), Currency::EUR)
        .credit(f.revenue, Eur::from_minor(200), Currency::EUR)
        .seal(&f.ctx())
        .expect("balances");
        j.record(later).expect("records");

        let recomputed = crate::seal::trial_balance_root(&j.trial_balance(None).expect("ok"));
        assert_ne!(recomputed, seal.trial_balance_root);
        assert!(seal.is_self_consistent());
    }

    #[test]
    fn a_checkpoint_round_trips_against_the_journal() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        j.record(f.sealed(b"b", 250)).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let cp = j.checkpoint(&key).expect("no overflow");
        assert_eq!(cp.balance.debits, Eur::from_minor(350));
        assert!(j.verify_checkpoint(&cp).is_ok());
    }

    #[test]
    fn a_prefix_checkpoint_stays_valid_as_the_log_grows() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let stale = j.checkpoint(&key).expect("ok");

        // The log grows. The checkpoint still describes a real prefix, and its
        // pinned head lets it be re-derived exactly, so it remains valid.
        j.record(f.sealed(b"b", 250)).expect("records");
        assert!(j.verify_checkpoint(&stale).is_ok());

        // A restated balance is caught.
        let mut restated = stale;
        restated.balance.debits = Eur::from_minor(9_999);
        assert!(matches!(
            j.verify_checkpoint(&restated),
            Err(CheckpointError::BalanceMismatch)
        ));

        // So is a checkpoint claiming a history the log never had.
        let mut forged = stale;
        forged.tree_head.root = crate::Hash::from_bytes([0xabu8; 32]);
        assert!(matches!(
            j.verify_checkpoint(&forged),
            Err(CheckpointError::HeadMismatch)
        ));
    }

    #[test]
    fn a_checkpoint_beyond_the_log_is_rejected() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let mut cp = j.checkpoint(&key).expect("ok");
        cp.through_index = Some(99);
        assert!(matches!(
            j.verify_checkpoint(&cp),
            Err(CheckpointError::IndexOutOfRange { index: 99 })
        ));
    }

    #[test]
    fn balance_assertions_catch_divergence() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        j.record(f.sealed(b"b", 250)).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };

        let holds = BalanceAssertion::net(key, Eur::from_minor(350));
        assert!(j.check_assertion(&holds).expect("ok").held());

        let wrong = BalanceAssertion::net(key, Eur::from_minor(300));
        let outcome = j.check_assertion(&wrong).expect("ok");
        assert!(!outcome.held());
        assert!(matches!(
            outcome,
            AssertionOutcome::Failed {
                difference,
                ..
            } if difference == Eur::from_minor(50)
        ));
    }

    #[test]
    fn an_assertion_can_target_an_earlier_log_position() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        j.record(f.sealed(b"b", 250)).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let earlier = BalanceAssertion::net(key, Eur::from_minor(100)).at_index(0);
        assert!(j.check_assertion(&earlier).expect("ok").held());
    }

    #[test]
    fn an_entry_claiming_a_reversal_it_does_not_perform_is_refused() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let original = f.sealed(b"orig", 1000);
        j.record(original.clone()).expect("records");

        // Names the original, but the postings do not invert it.
        let forged = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"forged".to_vec()).expect("valid"),
            date!(2026 - 04 - 01),
        )
        .reversing(original.id(), original.booking_date())
        .debit(f.revenue, Eur::from_minor(1), Currency::EUR)
        .credit(f.cash, Eur::from_minor(1), Currency::EUR)
        .seal(&f.ctx())
        .expect("balances on its own");

        assert!(matches!(
            j.record(forged),
            Err(JournalError::NotAnInversion { .. })
        ));
        assert_eq!(j.reversal_of(original.id()), None, "must stay unreversed");
        assert_eq!(j.len(), 1);
    }

    #[test]
    fn a_statement_shows_movements_and_the_running_balance() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");
        j.record(f.sealed(b"b", 250)).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        let lines = j.statement(&key).expect("no overflow");
        assert_eq!(lines.len(), 2);

        let first = lines.first().expect("present");
        assert_eq!(first.amount, Eur::from_minor(100));
        assert_eq!(first.direction, crate::posting::Direction::Debit);
        assert_eq!(first.running.debits, Eur::from_minor(100));

        let second = lines.get(1).expect("present");
        assert_eq!(second.running.debits, Eur::from_minor(350));
        assert_eq!(second.booking_date, date!(2026 - 03 - 15));
    }

    #[test]
    fn a_statement_ignores_other_accounts_and_layers() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        j.record(f.sealed(b"a", 100)).expect("records");

        let pending = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"p".to_vec()).expect("valid"),
            date!(2026 - 03 - 16),
        )
        .post(
            crate::posting::Posting::debit(f.cash, Eur::from_minor(500), Currency::EUR)
                .in_layer(Layer::Pending),
        )
        .post(
            crate::posting::Posting::credit(f.revenue, Eur::from_minor(500), Currency::EUR)
                .in_layer(Layer::Pending),
        )
        .seal(&f.ctx())
        .expect("balances");
        j.record(pending).expect("records");

        let settled = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        assert_eq!(j.statement(&settled).expect("ok").len(), 1);

        let reserved = BalanceKey {
            layer: Layer::Pending,
            ..settled
        };
        assert_eq!(j.statement(&reserved).expect("ok").len(), 1);
    }

    #[test]
    fn open_items_track_invoices_against_payments() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());

        // An invoice raises a receivable; a payment settles part of it.
        let invoice = f.sealed(b"invoice", 1000);
        j.record(invoice.clone()).expect("records");

        let payment = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(f.cash, Eur::from_minor(400), Currency::EUR)
        .debit(f.revenue, Eur::from_minor(400), Currency::EUR)
        .seal(&f.ctx())
        .expect("balances");
        let payment_id = payment.id();
        j.record(payment).expect("records");

        let key = BalanceKey {
            account: f.cash,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };

        // Before clearing, both postings are open.
        assert_eq!(j.open_items(&key).expect("ok").len(), 2);

        j.clear(crate::clearing::Clearing {
            id: crate::clearing::ClearingId::generate(),
            account: f.cash,
            currency: Currency::EUR,
            cleared_on: date!(2026 - 03 - 20),
            items: vec![
                crate::clearing::ClearedItem {
                    posting: crate::clearing::PostingRef::new(invoice.id(), 0),
                    applied: Eur::from_minor(400),
                },
                crate::clearing::ClearedItem {
                    posting: crate::clearing::PostingRef::new(payment_id, 0),
                    applied: Eur::from_minor(400),
                },
            ],
        })
        .expect("clears");

        // The payment is fully applied; the invoice keeps its remainder open.
        let open = j.open_items(&key).expect("ok");
        assert_eq!(open.len(), 1);
        let item = open.first().expect("present");
        assert_eq!(item.residual, Eur::from_minor(600));
        assert_eq!(j.clearings().len(), 1);
    }

    #[test]
    fn clearing_does_not_change_any_balance() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let invoice = f.sealed(b"invoice", 1000);
        j.record(invoice.clone()).expect("records");
        let payment = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(f.cash, Eur::from_minor(1000), Currency::EUR)
        .debit(f.revenue, Eur::from_minor(1000), Currency::EUR)
        .seal(&f.ctx())
        .expect("balances");
        let payment_id = payment.id();
        j.record(payment).expect("records");

        let before = j.trial_balance(None).expect("ok");
        j.clear(crate::clearing::Clearing {
            id: crate::clearing::ClearingId::generate(),
            account: f.cash,
            currency: Currency::EUR,
            cleared_on: date!(2026 - 03 - 20),
            items: vec![
                crate::clearing::ClearedItem {
                    posting: crate::clearing::PostingRef::new(invoice.id(), 0),
                    applied: Eur::from_minor(1000),
                },
                crate::clearing::ClearedItem {
                    posting: crate::clearing::PostingRef::new(payment_id, 0),
                    applied: Eur::from_minor(1000),
                },
            ],
        })
        .expect("clears");

        // Clearing is an assignment, not a movement.
        assert_eq!(before, j.trial_balance(None).expect("ok"));
        assert!(j.verify_log());
    }

    #[test]
    fn description_does_not_affect_ordering_or_balance() {
        let f = Fixture::new();
        let mut j = Journal::<2>::new(test_ledger());
        let entry = f
            .draft(b"k", 500)
            .with_description(Description::new("annotated").expect("valid"))
            .seal(&f.ctx())
            .expect("balances");
        j.record(entry).expect("records");
        assert!(j.verify_balanced().expect("ok"));
    }
}
