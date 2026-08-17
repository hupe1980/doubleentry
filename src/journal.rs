//! The in-memory journal — the engine's reference implementation.
//!
//! A journal is one entity's books: its accounts, its calendar, its policy, its
//! entries in append order, the Merkle log that commits to them, its seals and
//! its clearings. It holds all of that together because every one of those
//! pieces is needed to decide whether the next booking is legal, and splitting
//! them across the caller's own variables is how they drift apart.
//!
//! It has no storage. It is the semantics a [`LedgerStore`](crate::LedgerStore)
//! is expected to agree with, the substrate for deterministic testing, and — for
//! a process that does not need durability — a usable ledger on its own.
//!
//! # Idempotency
//!
//! Recording is keyed by [`IdempotencyKey`]. Submitting the same key twice with
//! byte-identical content is a no-op that returns the original outcome, which is
//! what makes a retry safe across an at-least-once delivery path. Submitting the
//! same key with different content is a conflict and is refused — never a silent
//! overwrite, and never a second entry.
//!
//! # Cost
//!
//! Balances and per-account posting lists are maintained as entries arrive, so
//! reading the current trial balance, one account's balance, or one account's
//! statement costs what the answer costs and not what the history costs. Asking
//! for a *historical* state — a balance as of an earlier log position — replays
//! that account's postings up to the position, which is linear in the postings
//! on that account rather than in the journal.
//!
//! Proofs and whole-log verification are `O(n)` by nature. They are audit-time
//! operations, not write-path ones.

use std::collections::{BTreeMap, BTreeSet};

use time::Date;

use crate::account::{AccountId, AccountRecord, AccountRegistry, BalanceLimit};
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::checkpoint::{
    AssertAt, AssertionOutcome, BalanceAssertion, Checkpoint, CheckpointError,
};
use crate::clearing::{
    Clearing, ClearingError, ClearingId, ClearingRegister, OpenItem, PostingLookup,
    PostingPosition, PostingRef,
};
use crate::entry::{
    Balanced, Draft, Entry, EntryId, IdempotencyKey, LedgerPolicy, SealContext, ValidationErrors,
};
use crate::hash::Hash;
use crate::merkle::{ConsistencyProof, InclusionProof, MerkleLog, ProofError, TreeHead};
use crate::money::{Currency, MoneyError};
use crate::period::{LedgerId, Period, PeriodCalendar, PeriodError, PeriodId, PeriodState};
use crate::posting::{Layer, Posting};
use crate::seal::{
    PeriodCoverage, Seal, SealChain, SealChainError, SealedBalance, SealedBalanceError,
    SealedBalanceOutcome,
};
use crate::storage::StatementLine;

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

    /// The entry would leave an account on a side its limit forbids.
    ///
    /// The net is reported in minor units at `scale`, so the variant stays
    /// independent of the ledger's compile-time precision.
    #[error(
        "{account} in {currency} ({layer}) would net to {net_minor} at scale \
         {scale}, which its {limit} limit forbids"
    )]
    LimitBreached {
        /// The account whose limit would be breached.
        account: AccountId,
        /// The currency the limit was breached in.
        currency: Currency,
        /// The layer the limit was breached in.
        layer: Layer,
        /// The limit in force.
        limit: BalanceLimit,
        /// The signed net the entry would leave, debit positive, in minor units.
        net_minor: i64,
        /// Decimal places the minor units are expressed in.
        scale: u8,
    },

    /// Accumulating balances overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),

    /// A clearing was refused.
    #[error(transparent)]
    Clearing(#[from] ClearingError),

    /// The calendar refused a period operation.
    ///
    /// Covers the sealing preconditions too — that the period is defined, is
    /// closing, and is next in date order. Those are questions about the
    /// calendar, and [`PeriodCalendar::check_sealable`] is the one place they
    /// are answered.
    #[error(transparent)]
    Period(#[from] PeriodError),

    /// The seal did not chain onto the existing ones.
    #[error(transparent)]
    Seal(#[from] SealChainError),

    /// A sealed balance could not be proven.
    #[error(transparent)]
    SealedBalance(#[from] SealedBalanceError),
}

/// Where one posting sits: which entry, and which posting within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Site {
    index: LogIndex,
    posting: u16,
}

/// An append-only journal of validated entries.
///
/// See the [module documentation](self) for what it owns and why.
#[derive(Debug, Clone)]
pub struct Journal<const P: u8> {
    ledger: LedgerId,
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,

    entries: Vec<Entry<Balanced, P>>,
    log: MerkleLog,
    by_id: BTreeMap<EntryId, LogIndex>,
    by_key: BTreeMap<IdempotencyKey, (EntryId, LogIndex, Hash)>,
    reversed_by: BTreeMap<EntryId, EntryId>,

    /// Current balances, maintained as entries arrive.
    balances: TrialBalance<P>,
    /// Where each balance key's postings are, in log order.
    sites: BTreeMap<BalanceKey, Vec<Site>>,

    seals: SealChain,
    clearings: ClearingRegister<P>,
}

impl<const P: u8> Journal<P> {
    /// Creates an empty journal for one ledger.
    ///
    /// A journal is one entity's books, not a shared table: the ledger it names
    /// is bound into every seal it produces, so seals from two journals can
    /// never be mistaken for one chain.
    ///
    /// It starts with no accounts, no periods, and a policy that constrains
    /// nothing. Register accounts before posting to them; define periods when
    /// you want to be able to seal them.
    #[must_use]
    pub fn new(ledger: LedgerId) -> Self {
        Self {
            seals: SealChain::new(ledger.clone()),
            ledger,
            accounts: AccountRegistry::new(),
            calendar: PeriodCalendar::new(),
            policy: LedgerPolicy::default(),
            entries: Vec::new(),
            log: MerkleLog::new(),
            by_id: BTreeMap::new(),
            by_key: BTreeMap::new(),
            reversed_by: BTreeMap::new(),
            balances: TrialBalance::new(),
            sites: BTreeMap::new(),
            clearings: ClearingRegister::new(),
        }
    }

    /// Sets the ledger-wide policy.
    ///
    /// Applies to what is recorded next. It does not re-validate what is already
    /// recorded, and it must not: an entry that was legal when written stays
    /// readable forever.
    #[must_use]
    pub fn with_policy(mut self, policy: LedgerPolicy) -> Self {
        self.policy = policy;
        self
    }

    // ── master data ─────────────────────────────────────────────────────────

    /// The ledger these books belong to.
    #[must_use]
    pub fn ledger(&self) -> &LedgerId {
        &self.ledger
    }

    /// The accounts that may be posted to.
    #[must_use]
    pub fn accounts(&self) -> &AccountRegistry {
        &self.accounts
    }

    /// The account registry, for registering, closing, and reopening accounts.
    ///
    /// Master data is mutable while the journal is not: closing an account
    /// changes what may be booked next and never what was booked already.
    pub fn accounts_mut(&mut self) -> &mut AccountRegistry {
        &mut self.accounts
    }

    /// The period calendar.
    #[must_use]
    pub fn calendar(&self) -> &PeriodCalendar {
        &self.calendar
    }

    /// The period calendar, for defining periods and moving them through their
    /// lifecycle.
    ///
    /// Sealing is [`Journal::seal_period`]'s job: it has to commit to the
    /// balances before the period's state changes, which a bare transition
    /// cannot do.
    pub fn calendar_mut(&mut self) -> &mut PeriodCalendar {
        &mut self.calendar
    }

    /// The ledger-wide policy.
    #[must_use]
    pub fn policy(&self) -> &LedgerPolicy {
        &self.policy
    }

    /// Defines a period.
    ///
    /// # Errors
    ///
    /// Returns [`PeriodError`] for a duplicate identifier or an overlapping
    /// range.
    pub fn define_period(&mut self, period: Period) -> Result<(), JournalError> {
        self.calendar.define(period)?;
        Ok(())
    }

    /// Moves a period through its lifecycle.
    ///
    /// # Errors
    ///
    /// Returns [`PeriodError`] when the transition is not permitted.
    pub fn transition_period(
        &mut self,
        period: &PeriodId,
        to: PeriodState,
    ) -> Result<(), JournalError> {
        self.calendar.transition(period, to)?;
        Ok(())
    }

    /// Everything validation needs, borrowed from this journal.
    ///
    /// Use it to validate a draft without recording it — a dry run, or a batch
    /// checked before any of it is committed. [`Journal::record`] builds it for
    /// you.
    #[must_use]
    pub fn context(&self) -> SealContext<'_> {
        SealContext {
            accounts: &self.accounts,
            calendar: &self.calendar,
            policy: &self.policy,
        }
    }

    // ── recording ───────────────────────────────────────────────────────────

    /// Validates a draft against this journal and records it.
    ///
    /// The usual way in. [`Journal::record_validated`] exists for an entry that
    /// was validated elsewhere — read what it costs before reaching for it.
    ///
    /// # Idempotency comes first
    ///
    /// The key is resolved *before* validation runs, not after. Otherwise a
    /// retry of an entry that was accepted months ago would be refused today
    /// because its period has since been sealed or its account has closed —
    /// turning an at-least-once delivery path into a source of spurious errors
    /// precisely when the ledger is least able to act on them. A safe retry must
    /// not be able to trip a rule the original submission already passed.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Invalid`] with every violation found, and the
    /// idempotency and correction errors of [`Journal::record_validated`].
    pub fn record(&mut self, draft: Entry<Draft, P>) -> Result<Recorded, JournalError> {
        let content_hash = draft.digest();
        if let Some((existing_id, existing_index, stored)) =
            self.by_key.get(draft.idempotency_key())
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

        let entry = draft.seal(&self.context())?;
        self.record_validated(entry)
    }

    /// Records an entry that has already passed validation.
    ///
    /// The entry must have been sealed against *this* journal's accounts,
    /// calendar and policy. Sealing it against a different context and recording
    /// it here would book against accounts this ledger does not have, or into a
    /// period it has already sealed.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::IdempotencyConflict`] when the key has been used
    /// for different content, and the reversal errors when the entry violates
    /// the correction rules.
    pub fn record_validated(
        &mut self,
        entry: Entry<Balanced, P>,
    ) -> Result<Recorded, JournalError> {
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
            self.check_reversal(&entry, original)?;
        }

        // Everything that can fail has failed by now, except accumulating the
        // balances — which is checked against a copy so a rejected entry cannot
        // leave the journal half-applied.
        let mut balances = self.balances.clone();
        for posting in entry.postings() {
            balances.apply(posting)?;
        }
        self.check_limits(&entry, &balances)?;

        let index = LogIndex(self.log.append(content_hash));
        if let Some(original) = entry.reverses() {
            self.reversed_by.insert(original, entry.id());
        }
        for (position, posting) in entry.postings().iter().enumerate() {
            self.sites.entry(key_of(posting)).or_default().push(Site {
                index,
                posting: u16::try_from(position).unwrap_or(u16::MAX),
            });
        }
        self.balances = balances;
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

    /// Records several entries, or none of them.
    ///
    /// Atomicity across entries is not optional: an invoice and the entry that
    /// offsets it must not be separable by a failure partway through. Entries
    /// are applied in order and, if any is refused, everything this call added
    /// is undone — leaving the journal byte-identical to how it started.
    ///
    /// An idempotent replay inside a batch is not a failure: it returns the
    /// original outcome, like a single-entry replay, and the rest of the batch
    /// proceeds.
    ///
    /// # Errors
    ///
    /// Returns the first entry's failure, having applied nothing.
    pub fn record_batch(
        &mut self,
        entries: impl IntoIterator<Item = Entry<Balanced, P>>,
    ) -> Result<Vec<Recorded>, JournalError> {
        let mark = self.entries.len();
        let mut out = Vec::new();
        for entry in entries {
            match self.record_validated(entry) {
                Ok(recorded) => out.push(recorded),
                Err(e) => {
                    self.rollback_to(mark);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// Undoes every append since the journal held `mark` entries.
    ///
    /// The exact inverse of the appends it removes: identifiers, keys, reversal
    /// links, posting sites and balances all come back to what they were. The
    /// balance of a key an undone posting touched is recomputed from the
    /// postings that remain, rather than subtracted from — subtraction would
    /// have to trust that what is being removed is exactly what was added, and
    /// recomputation does not.
    fn rollback_to(&mut self, mark: usize) {
        let mut touched: BTreeSet<BalanceKey> = BTreeSet::new();
        while self.entries.len() > mark {
            let Some(entry) = self.entries.pop() else {
                break;
            };
            let index = LogIndex(self.entries.len() as u64);
            self.by_id.remove(&entry.id());
            self.by_key.remove(entry.idempotency_key());
            if let Some(original) = entry.reverses() {
                self.reversed_by.remove(&original);
            }
            for posting in entry.postings() {
                let key = key_of(posting);
                if let Some(sites) = self.sites.get_mut(&key) {
                    sites.retain(|site| site.index != index);
                }
                touched.insert(key);
            }
        }
        self.log.truncate(self.entries.len() as u64);

        for key in touched {
            let mut balance = Balance::ZERO;
            let mut overflowed = false;
            for posting in self.postings_on_prefix(&key, None) {
                if balance.add(posting.direction, posting.amount).is_err() {
                    overflowed = true;
                    break;
                }
            }
            // A prefix of a set of postings that already summed cannot overflow,
            // so this is unreachable; leaving the stale total in place would be
            // worse than reporting zero, and reporting zero is caught by
            // `verify_balances`.
            let balance = if overflowed { Balance::ZERO } else { balance };
            match self.sites.get(&key) {
                Some(sites) if !sites.is_empty() => self.balances.set(key, balance),
                _ => {
                    self.sites.remove(&key);
                    self.balances.remove(&key);
                }
            }
        }
    }

    /// Enforces every touched account's balance limit against the state the
    /// entry would leave behind.
    ///
    /// Checked on the *resulting* balance rather than posting by posting: an
    /// entry that dips an account below its limit and back within the same
    /// booking is one movement, and refusing it would make the answer depend on
    /// the order the caller happened to list its postings in.
    ///
    /// Deliberately here rather than in [`Entry::seal`]. Sealing asks whether an
    /// entry is well formed against master data, which is a question about the
    /// entry alone; a limit is a question about the *books*, and the same entry
    /// is legal or not depending on what has already been recorded.
    fn check_limits(
        &self,
        entry: &Entry<Balanced, P>,
        after: &TrialBalance<P>,
    ) -> Result<(), JournalError> {
        // One check per distinct key the entry touches, not per posting.
        let mut checked: BTreeSet<BalanceKey> = BTreeSet::new();
        for posting in entry.postings() {
            let key = key_of(posting);
            if !checked.insert(key) {
                continue;
            }
            let limit = self.accounts.limit_of(key.account);
            if limit.is_unlimited() {
                continue;
            }
            let balance = after.get_or_zero(&key);
            if !limit.permits(&balance) {
                return Err(JournalError::LimitBreached {
                    account: key.account,
                    currency: key.currency,
                    layer: key.layer,
                    limit,
                    net_minor: balance.signed_net().map_or(0, |n| n.to_minor()),
                    scale: P,
                });
            }
        }
        Ok(())
    }

    /// Enforces the correction rules against what is already recorded.
    fn check_reversal(
        &self,
        entry: &Entry<Balanced, P>,
        original: EntryId,
    ) -> Result<(), JournalError> {
        let Some(target) = self.get(original) else {
            return Err(JournalError::UnknownOriginal { id: original });
        };
        if let Some(existing) = self.reversed_by.get(&original) {
            return Err(JournalError::AlreadyReversed {
                id: original,
                by: *existing,
            });
        }
        if target.reverses().is_some() {
            return Err(JournalError::ReversalOfReversal { id: original });
        }
        if !is_inversion_of(target, entry) {
            return Err(JournalError::NotAnInversion { id: original });
        }
        Ok(())
    }

    // ── reading ─────────────────────────────────────────────────────────────

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

    /// The log position of an entry.
    #[must_use]
    pub fn index_of(&self, id: EntryId) -> Option<LogIndex> {
        self.by_id.get(&id).copied()
    }

    /// The reversal of an entry, if one has been recorded.
    #[must_use]
    pub fn reversal_of(&self, id: EntryId) -> Option<EntryId> {
        self.reversed_by.get(&id).copied()
    }

    // ── proofs ──────────────────────────────────────────────────────────────

    /// The current tree head.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        self.log.head()
    }

    /// The tree head as of an earlier size.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log.
    pub fn head_at(&self, size: u64) -> Result<TreeHead, ProofError> {
        self.log.head_at(size)
    }

    /// Proves that the entry at `index` is committed to by the current head.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::IndexOutOfRange`] for a position beyond the log.
    pub fn prove_inclusion(&self, index: LogIndex) -> Result<InclusionProof, ProofError> {
        self.log.inclusion_proof(index.get())
    }

    /// Proves that the entry at `index` was committed to by the head at `size`.
    ///
    /// What an auditor holding an archived head can actually check. Their head
    /// is not the current one, and the current root says nothing about it, so a
    /// proof against the present log is no use to them. Pair the result with
    /// [`head_at`](Self::head_at), or with the head they archived — the two must
    /// agree, and [`InclusionProof::verify`] is what says so.
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log, and
    /// [`ProofError::IndexOutOfRange`] for an entry the log had not yet reached
    /// at that size.
    pub fn prove_inclusion_at(
        &self,
        index: LogIndex,
        size: u64,
    ) -> Result<InclusionProof, ProofError> {
        self.log.inclusion_proof_at(index.get(), size)
    }

    /// Proves that the log at `old_size` is a prefix of the current log.
    ///
    /// A proof from `old_size == 0` is **vacuous** — every log extends the empty
    /// tree, so it constrains nothing about the newer one and verifies against
    /// any root at the right size. See
    /// [`ConsistencyProof::is_vacuous`](crate::ConsistencyProof::is_vacuous).
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] for a size beyond the log.
    pub fn prove_consistency(&self, old_size: u64) -> Result<ConsistencyProof, ProofError> {
        self.log.consistency_proof(old_size)
    }

    /// Proves that the log at `old_size` is a prefix of the log at `new_size`.
    ///
    /// The general form: two auditors holding different archived heads, neither
    /// of them current, can be shown that one is a prefix of the other without
    /// either learning the log's present size.
    ///
    /// A proof from `old_size == 0` is **vacuous** — every log extends the empty
    /// tree, so it constrains nothing about the newer one and verifies against
    /// any root at the right size. See
    /// [`ConsistencyProof::is_vacuous`](crate::ConsistencyProof::is_vacuous).
    ///
    /// # Errors
    ///
    /// Returns [`ProofError::SizeOutOfRange`] if `new_size` is beyond the log or
    /// `old_size` is beyond `new_size`.
    pub fn prove_consistency_between(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, ProofError> {
        self.log.consistency_proof_between(old_size, new_size)
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

    // ── balances ────────────────────────────────────────────────────────────

    /// The trial balance, optionally over a prefix of the log.
    ///
    /// `size` is a **count of entries**, not an index: `Some(0)` is the empty
    /// ledger, `Some(2)` is the journal as it stood after two entries, and
    /// `None` is everything recorded so far. It is the same number a
    /// [`TreeHead::size`] carries, so a balance and the root it belongs with are
    /// always named the same way.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if accumulating overflows, which cannot
    /// happen for a prefix of a journal that accepted the entries in the first
    /// place.
    pub fn trial_balance(&self, size: Option<u64>) -> Result<TrialBalance<P>, MoneyError> {
        let Some(size) = size else {
            return Ok(self.balances.clone());
        };
        if size >= self.entries.len() as u64 {
            return Ok(self.balances.clone());
        }
        let mut tb = TrialBalance::new();
        for entry in self.entries_prefix(size) {
            for posting in entry.postings() {
                tb.apply(posting)?;
            }
        }
        Ok(tb)
    }

    /// The balance of one account, currency, and layer, optionally over a prefix
    /// of the log.
    ///
    /// `size` counts entries, exactly as in [`Journal::trial_balance`].
    ///
    /// Reading the current balance is a lookup. Reading a historical one replays
    /// that key's postings, which is linear in how much has moved through the
    /// account rather than in the size of the journal.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if accumulating overflows.
    pub fn balance(&self, key: &BalanceKey, size: Option<u64>) -> Result<Balance<P>, MoneyError> {
        let Some(size) = size else {
            return Ok(self.balances.get_or_zero(key));
        };
        let mut balance = Balance::ZERO;
        for posting in self.postings_on_prefix(key, Some(size)) {
            balance.add(posting.direction, posting.amount)?;
        }
        Ok(balance)
    }

    /// The balance of one key over everything booked on or before `end`.
    ///
    /// Folds by **booking date**, so an entry recorded late still lands in the
    /// period it economically belongs to. This is the form to reconcile against
    /// an external statement, which is dated rather than positioned.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if accumulating overflows.
    pub fn balance_on_date(&self, key: &BalanceKey, end: Date) -> Result<Balance<P>, MoneyError> {
        let mut balance = Balance::ZERO;
        for site in self.sites.get(key).into_iter().flatten() {
            let Some((entry, posting)) = self.resolve(*site) else {
                continue;
            };
            if entry.booking_date() <= end {
                balance.add(posting.direction, posting.amount)?;
            }
        }
        Ok(balance)
    }

    /// Checks that debits equal credits across the whole journal.
    ///
    /// Every entry balances individually, so this must hold; running it is a
    /// direct test that the maintained balances and the invariant have not
    /// drifted apart.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if totalling overflows.
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

    /// Recomputes the maintained balances from the entries and compares them.
    ///
    /// The incremental trial balance is derived state, like the Merkle subtree
    /// stack. This proves it has not drifted from the entries it summarises.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if the recomputation overflows.
    pub fn verify_balances(&self) -> Result<bool, MoneyError> {
        let mut recomputed = TrialBalance::new();
        for entry in &self.entries {
            for posting in entry.postings() {
                recomputed.apply(posting)?;
            }
        }
        Ok(recomputed == self.balances)
    }

    /// Currencies present in the journal, in deterministic order.
    #[must_use]
    pub fn currencies(&self) -> Vec<Currency> {
        self.balances.currencies()
    }

    /// Folds every entry booked on or before `end` into a trial balance.
    ///
    /// This is what a period's *closing* balance means: cumulative through the
    /// period's last day. Folding the whole journal instead would pull in
    /// entries booked into later periods, which is wrong whenever a period is
    /// sealed after the next one has begun — the normal case.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if accumulating overflows.
    pub fn trial_balance_through_date(&self, end: Date) -> Result<TrialBalance<P>, MoneyError> {
        let mut tb = TrialBalance::new();
        for entry in self.entries.iter().filter(|e| e.booking_date() <= end) {
            for posting in entry.postings() {
                tb.apply(posting)?;
            }
        }
        Ok(tb)
    }

    // ── statements ──────────────────────────────────────────────────────────

    /// Every posting touching `key`, in log order, with the running balance
    /// after each.
    ///
    /// This is the account statement a reader actually wants: a trial balance
    /// says where an account ended up, and says nothing about how it got there.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if the running balance overflows.
    pub fn statement(&self, key: &BalanceKey) -> Result<Vec<StatementLine<P>>, MoneyError> {
        let mut running = Balance::ZERO;
        let mut out = Vec::new();
        for site in self.sites.get(key).into_iter().flatten() {
            let Some((entry, posting)) = self.resolve(*site) else {
                continue;
            };
            running.add(posting.direction, posting.amount)?;
            out.push(StatementLine {
                index: site.index,
                posting: PostingRef::new(entry.id(), site.posting),
                booking_date: entry.booking_date(),
                direction: posting.direction,
                amount: posting.amount,
                running,
                kind: entry.kind().cloned(),
            });
        }
        Ok(out)
    }

    /// References to every posting on `key`, in log order.
    #[must_use]
    pub fn postings_on(&self, key: &BalanceKey) -> Vec<PostingRef> {
        self.sites
            .get(key)
            .into_iter()
            .flatten()
            .filter_map(|site| {
                self.at(site.index)
                    .map(|entry| PostingRef::new(entry.id(), site.posting))
            })
            .collect()
    }

    fn resolve(&self, site: Site) -> Option<(&Entry<Balanced, P>, &Posting<P>)> {
        let entry = self.at(site.index)?;
        let posting = entry.postings().get(usize::from(site.posting))?;
        Some((entry, posting))
    }

    fn entries_prefix(&self, size: u64) -> impl Iterator<Item = &Entry<Balanced, P>> {
        self.entries
            .iter()
            .take(usize::try_from(size).unwrap_or(usize::MAX))
    }

    /// Postings on `key` within the first `size` entries, in log order.
    ///
    /// `sites` is maintained in log order, so the prefix is a `take_while`
    /// rather than a scan.
    fn postings_on_prefix(
        &self,
        key: &BalanceKey,
        size: Option<u64>,
    ) -> impl Iterator<Item = &Posting<P>> {
        self.sites
            .get(key)
            .into_iter()
            .flatten()
            .take_while(move |site| size.is_none_or(|n| site.index.get() < n))
            .filter_map(|site| self.resolve(*site).map(|(_, posting)| posting))
    }

    // ── clearing ────────────────────────────────────────────────────────────

    /// Records that a set of postings offset one another.
    ///
    /// # Errors
    ///
    /// Returns [`ClearingError`] when the clearing breaks one of its rules.
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
    ///
    /// # Errors
    ///
    /// Returns [`ClearingError`] when the clearing is unknown or already reset.
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
    ///
    /// # Errors
    ///
    /// Returns [`ClearingError`] if a recorded clearing references a posting
    /// this journal does not hold, which would be a bug in this crate.
    pub fn open_items(&self, key: &BalanceKey) -> Result<Vec<OpenItem<P>>, JournalError> {
        // Straight from `sites`, which is maintained in log order — so the
        // candidates arrive oldest first and carry the position that says so.
        let candidates: Vec<(PostingPosition, PostingRef)> = self
            .sites
            .get(key)
            .into_iter()
            .flatten()
            .filter_map(|site| {
                self.at(site.index).map(|entry| {
                    (
                        PostingPosition::new(site.index, site.posting),
                        PostingRef::new(entry.id(), site.posting),
                    )
                })
            })
            .collect();
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
    fn coverage(&self, start: Date, end: Date) -> PeriodCoverage {
        let mut coverage = PeriodCoverage::EMPTY;
        for (i, entry) in self.entries.iter().enumerate() {
            let date = entry.booking_date();
            if date >= start && date <= end {
                let index = i as u64;
                coverage.first_index.get_or_insert(index);
                coverage.last_index = Some(index);
                coverage.entry_count = coverage.entry_count.saturating_add(1);
            }
        }
        coverage
    }

    /// Seals a closing period, committing to its entries and closing balances.
    ///
    /// The preconditions are [`PeriodCalendar::check_sealable`]'s: the period is
    /// defined, it is in [`PeriodState::Closing`], and it is the next one due in
    /// date order. The last of those is what makes the closing balance a stable
    /// claim — it is cumulative through the period's last day, so an earlier
    /// period still open could restate it afterwards with a perfectly ordinary
    /// booking.
    ///
    /// The seal commits to three things: the log's tree head, the period's
    /// closing trial balance, and the account registry those balances are keyed
    /// on — see [`Seal::accounts`] for why the last one is not optional.
    ///
    /// On success the period advances to [`PeriodState::Sealed`] and the
    /// calendar's watermark moves with it, freezing every date up to and
    /// including the period's last. The seal is appended to the chain *first*,
    /// so a seal the chain refuses cannot leave a period marked sealed with
    /// nothing sealing it.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Period`] when the period cannot be sealed, and
    /// [`JournalError::Seal`] when the seal does not chain onto the last one.
    pub fn seal_period(&mut self, period: &PeriodId) -> Result<Seal, JournalError> {
        let definition = self.calendar.check_sealable(period)?;
        let (start, end) = (definition.start, definition.end);

        let coverage = self.coverage(start, end);
        let closing = self.trial_balance_through_date(end)?;
        let seal = Seal::build(
            self.ledger.clone(),
            period.clone(),
            coverage,
            self.head(),
            &closing,
            // The registry as it stands now: the same one the trial balance's
            // handles were resolved against.
            self.accounts.commitment(),
            self.seals.head(),
        );

        // Chain first: a refused seal must leave the period exactly as it was.
        self.seals.push(seal.clone())?;
        self.calendar.transition(period, PeriodState::Sealed)?;
        Ok(seal)
    }

    /// The chain of seals recorded so far.
    #[must_use]
    pub fn seals(&self) -> &SealChain {
        &self.seals
    }

    /// Verifies every seal and every link between them.
    ///
    /// # Errors
    ///
    /// Returns [`SealChainError`] naming the first seal that does not hold.
    pub fn verify_seals(&self) -> Result<(), SealChainError> {
        self.seals.verify()
    }

    /// Proves what one account closed a sealed period at, and names it.
    ///
    /// The complete claim an auditor wants, from one call: the seal, an
    /// `O(log n)` path to the balance row, and an `O(log n)` path binding that
    /// row's handle to an account path — disclosing nothing else.
    ///
    /// Assembled by hand this is a five-step recipe of which exactly one step
    /// matters, and it is the one nothing forces: comparing the rebuilt
    /// commitment against the one the seal recorded. Skip it and the proof is
    /// against a commitment you just computed yourself — internally consistent,
    /// evidence of nothing. [`SealedBalance::assemble`] is where that check
    /// lives, and every [`LedgerStore`](crate::LedgerStore) routes through the
    /// same function, so the durable backends cannot drift from this one.
    ///
    /// Note which fold rebuilds the closing balance:
    /// [`trial_balance_through_date`](Self::trial_balance_through_date), by
    /// booking date, not [`trial_balance`](Self::trial_balance) over a log
    /// prefix. Sealing March in April is the normal case, so at the moment the
    /// seal is taken the log already holds April entries and the two differ.
    ///
    /// "Nothing to prove" comes back as a [`SealedBalanceOutcome`] variant
    /// rather than an error, because it is an answer: see there for why the
    /// distinction matters.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::SealedBalance`] when the period is not sealed or
    /// when the books no longer reproduce the seal's closing balance.
    pub fn prove_sealed_balance(
        &self,
        period: &PeriodId,
        key: BalanceKey,
    ) -> Result<SealedBalanceOutcome<P>, JournalError> {
        let Some(seal) = self.seals.get(period).cloned() else {
            return Err(SealedBalanceError::NotSealed {
                period: period.clone(),
            }
            .into());
        };
        let Some(definition) = self.calendar.get(&seal.period) else {
            return Err(SealedBalanceError::UndefinedPeriod {
                period: period.clone(),
            }
            .into());
        };
        let closing = self.trial_balance_through_date(definition.end)?;
        Ok(SealedBalance::assemble(
            seal,
            &closing,
            &self.accounts,
            key,
        )?)
    }

    // ── checkpoints and assertions ──────────────────────────────────────────

    /// Takes a checkpoint of one balance over the whole log so far.
    ///
    /// Taking one on an empty journal is meaningful and stays valid: it records
    /// that the account had not moved after zero entries, which no later append
    /// can falsify.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if reading the balance overflows.
    pub fn checkpoint(&self, key: &BalanceKey) -> Result<Checkpoint<P>, MoneyError> {
        Ok(Checkpoint::new(*key, self.balance(key, None)?, self.head()))
    }

    /// Re-derives a checkpoint from the journal and compares it.
    ///
    /// Checks the tree head as well as the balance: a checkpoint that matches
    /// numerically but was taken against a different history is stale, and
    /// silently trusting it would carry a stale balance forward. The head is
    /// also what names the prefix, so the balance is re-folded over exactly the
    /// entries the checkpoint claims to cover — never over whatever the journal
    /// happens to hold now.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError`] naming which of the three checks failed.
    pub fn verify_checkpoint(&self, checkpoint: &Checkpoint<P>) -> Result<(), CheckpointError> {
        let size = checkpoint.size();
        if size > self.len() as u64 {
            return Err(CheckpointError::SizeOutOfRange { size });
        }
        let head = self
            .head_at(size)
            .map_err(|_| CheckpointError::SizeOutOfRange { size })?;
        if head != checkpoint.tree_head {
            return Err(CheckpointError::HeadMismatch);
        }

        let actual = self.balance(&checkpoint.key, Some(size))?;
        if actual == checkpoint.balance {
            Ok(())
        } else {
            Err(CheckpointError::BalanceMismatch)
        }
    }

    /// Evaluates a balance assertion against the journal.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if reading the balance overflows.
    pub fn check_assertion(
        &self,
        assertion: &BalanceAssertion<P>,
    ) -> Result<AssertionOutcome<P>, MoneyError> {
        let actual = match assertion.at {
            AssertAt::Now => self.balance(&assertion.key, None)?,
            AssertAt::Prefix { size } => self.balance(&assertion.key, Some(size))?,
            AssertAt::OnDate { date } => self.balance_on_date(&assertion.key, date)?,
        };
        assertion.check(&actual)
    }

    /// Every account with its handle, for a backend to persist.
    #[must_use]
    pub fn account_records(&self) -> Vec<AccountRecord> {
        self.accounts.records()
    }
}

fn key_of<const P: u8>(posting: &Posting<P>) -> BalanceKey {
    BalanceKey {
        account: posting.account,
        currency: posting.currency,
        layer: posting.layer,
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
    use super::*;
    use crate::account::{AccountId, BalanceLimit};
    use crate::entry::{Description, Draft};
    use crate::money::{Amount, Currency};
    use crate::posting::{Direction, Posting};
    use time::macros::date;

    type Eur = Amount<2>;

    /// A journal with two accounts, ready to post to.
    struct Books {
        journal: Journal<2>,
        cash: AccountId,
        revenue: AccountId,
    }

    impl Books {
        fn new() -> Self {
            let mut journal = Journal::<2>::new(LedgerId::new("test-ledger").expect("valid"));
            let cash = journal
                .accounts_mut()
                .register_path("Assets:Cash", date!(2026 - 01 - 01))
                .expect("registers");
            let revenue = journal
                .accounts_mut()
                .register_path("Income:Sales", date!(2026 - 01 - 01))
                .expect("registers");
            Self {
                journal,
                cash,
                revenue,
            }
        }

        fn draft(&self, key: &[u8], minor: i64) -> Entry<Draft, 2> {
            self.draft_on(key, minor, date!(2026 - 03 - 15))
        }

        fn draft_on(&self, key: &[u8], minor: i64, on: Date) -> Entry<Draft, 2> {
            Entry::new(
                EntryId::generate(),
                IdempotencyKey::new(key.to_vec()).expect("valid"),
                on,
            )
            .debit(self.cash, Eur::from_minor(minor), Currency::EUR)
            .credit(self.revenue, Eur::from_minor(minor), Currency::EUR)
        }

        fn sealed(&self, key: &[u8], minor: i64) -> Entry<Balanced, 2> {
            self.draft(key, minor)
                .seal(&self.journal.context())
                .expect("balances")
        }

        fn record(&mut self, key: &[u8], minor: i64) -> Recorded {
            let draft = self.draft(key, minor);
            self.journal.record(draft).expect("records")
        }

        fn cash_key(&self) -> BalanceKey {
            BalanceKey {
                account: self.cash,
                currency: Currency::EUR,
                layer: Layer::Settled,
            }
        }

        fn march(&mut self) -> PeriodId {
            let id = PeriodId::new("2026-03").expect("valid");
            self.journal
                .define_period(
                    Period::new(id.clone(), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
                        .expect("valid range"),
                )
                .expect("defines");
            id
        }
    }

    // ── recording ───────────────────────────────────────────────────────────

    #[test]
    fn records_entries_in_order() {
        let mut b = Books::new();
        let first = b.record(b"a", 100);
        let second = b.record(b"b", 200);
        assert_eq!(first.index.expect("sequenced inline").get(), 0);
        assert_eq!(second.index.expect("sequenced inline").get(), 1);
        assert_eq!(b.journal.len(), 2);
        assert!(first.is_new && second.is_new);
    }

    #[test]
    fn a_draft_is_validated_against_the_journals_own_accounts() {
        let mut b = Books::new();
        let ghost = AccountId::from_index(999);
        let bad = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"ghost".to_vec()).expect("valid"),
            date!(2026 - 03 - 15),
        )
        .debit(ghost, Eur::from_minor(10), Currency::EUR)
        .credit(b.revenue, Eur::from_minor(10), Currency::EUR);

        assert!(matches!(
            b.journal.record(bad),
            Err(JournalError::Invalid(_))
        ));
        assert!(b.journal.is_empty());
    }

    #[test]
    fn an_identical_resubmission_is_a_no_op() {
        let mut b = Books::new();
        let first = b.record(b"same-key", 100);

        // A different entry identifier, but the same logical transaction.
        let replay = b.record(b"same-key", 100);
        assert!(!replay.is_new);
        assert_eq!(replay.id, first.id);
        assert_eq!(replay.index, first.index);
        assert_eq!(b.journal.len(), 1, "a replay must not append");
    }

    #[test]
    fn the_same_key_with_different_content_is_a_conflict() {
        let mut b = Books::new();
        b.record(b"key", 100);
        let draft = b.draft(b"key", 999);
        let err = b.journal.record(draft).expect_err("must not overwrite");
        assert!(matches!(err, JournalError::IdempotencyConflict { .. }));
        assert_eq!(b.journal.len(), 1, "a conflict must not append");
    }

    #[test]
    fn duplicate_entry_ids_are_refused() {
        let mut b = Books::new();
        let entry = b.sealed(b"k1", 100);
        let id = entry.id();
        b.journal.record_validated(entry).expect("records");

        let clash = Entry::<Draft, 2>::new(
            id,
            IdempotencyKey::new(b"k2".to_vec()).expect("valid"),
            date!(2026 - 03 - 15),
        )
        .debit(b.cash, Eur::from_minor(1), Currency::EUR)
        .credit(b.revenue, Eur::from_minor(1), Currency::EUR);

        assert!(matches!(
            b.journal.record(clash),
            Err(JournalError::DuplicateId { .. })
        ));
    }

    #[test]
    fn a_policy_applies_to_what_is_recorded_next() {
        let mut b = Books::new();
        b.record(b"before", 100);

        b.journal = std::mem::replace(
            &mut b.journal,
            Journal::<2>::new(LedgerId::new("scratch").expect("valid")),
        )
        .with_policy(LedgerPolicy::permissive().in_currency(Currency::USD));

        // What was already recorded stays readable …
        assert_eq!(b.journal.len(), 1);
        // … and the new rule applies to the next booking.
        let draft = b.draft(b"after", 100);
        assert!(matches!(
            b.journal.record(draft),
            Err(JournalError::Invalid(_))
        ));
    }

    // ── proofs ──────────────────────────────────────────────────────────────

    #[test]
    fn every_recorded_entry_can_be_proven_included() {
        let mut b = Books::new();
        for i in 0..12i64 {
            b.record(format!("k{i}").as_bytes(), 100 + i);
        }
        let head = b.journal.head();
        for (i, entry) in b.journal.entries().iter().enumerate() {
            let proof = b
                .journal
                .prove_inclusion(LogIndex(i as u64))
                .expect("in range");
            assert!(
                proof.verify(&entry.content_hash(), &head),
                "entry {i} must be provably included"
            );
            assert_eq!(proof.leaf_index, i as u64);
        }
    }

    #[test]
    fn growth_is_provably_append_only() {
        let mut b = Books::new();
        for i in 0..5i64 {
            b.record(format!("a{i}").as_bytes(), 10 + i);
        }
        let early = b.journal.head();
        for i in 0..7i64 {
            b.record(format!("b{i}").as_bytes(), 20 + i);
        }
        let later = b.journal.head();

        let proof = b.journal.prove_consistency(early.size).expect("in range");
        assert!(proof.verify(&early, &later));
    }

    #[test]
    fn a_historical_head_matches_the_log_as_it_stood() {
        let mut b = Books::new();
        b.record(b"a", 100);
        let snapshot = b.journal.head();
        b.record(b"b", 200);

        let reconstructed = b.journal.head_at(snapshot.size).expect("in range");
        assert_eq!(reconstructed, snapshot);
        assert_ne!(b.journal.head(), snapshot);
    }

    // ── corrections ─────────────────────────────────────────────────────────

    #[test]
    fn a_reversal_requires_a_known_original() {
        let b = Books::new();
        let orphan = b.sealed(b"orphan", 100);
        let reversal = orphan
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&b.journal.context())
            .expect("balances");
        let mut journal = b.journal;
        assert!(matches!(
            journal.record_validated(reversal),
            Err(JournalError::UnknownOriginal { .. })
        ));
    }

    #[test]
    fn an_entry_can_only_be_reversed_once() {
        let mut b = Books::new();
        let original = b.sealed(b"orig", 100);
        b.journal
            .record_validated(original.clone())
            .expect("records");

        for (key, on) in [
            (b"rev1".as_slice(), date!(2026 - 04 - 01)),
            (b"rev2".as_slice(), date!(2026 - 04 - 02)),
        ] {
            let reversal = original.reverse(
                EntryId::generate(),
                IdempotencyKey::new(key.to_vec()).expect("valid"),
                on,
            );
            let outcome = b.journal.record(reversal);
            if key == b"rev1" {
                outcome.expect("the first reversal is accepted");
            } else {
                assert!(matches!(outcome, Err(JournalError::AlreadyReversed { .. })));
            }
        }
    }

    #[test]
    fn a_reversal_cannot_itself_be_reversed() {
        let mut b = Books::new();
        let original = b.sealed(b"orig", 100);
        b.journal
            .record_validated(original.clone())
            .expect("records");

        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&b.journal.context())
            .expect("balances");
        let reversal_id = reversal.id();
        b.journal
            .record_validated(reversal.clone())
            .expect("records");

        let double = reversal.reverse(
            EntryId::generate(),
            IdempotencyKey::new(b"rev-rev".to_vec()).expect("valid"),
            date!(2026 - 04 - 02),
        );
        assert!(matches!(
            b.journal.record(double),
            Err(JournalError::ReversalOfReversal { id }) if id == reversal_id
        ));
    }

    #[test]
    fn an_entry_claiming_a_reversal_it_does_not_perform_is_refused() {
        let mut b = Books::new();
        let original = b.sealed(b"orig", 1000);
        let original_id = original.id();
        b.journal.record_validated(original).expect("records");

        // Names the original, but the postings do not invert it.
        let forged = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"forged".to_vec()).expect("valid"),
            date!(2026 - 04 - 01),
        )
        .reversing(original_id, date!(2026 - 03 - 15))
        .debit(b.revenue, Eur::from_minor(1), Currency::EUR)
        .credit(b.cash, Eur::from_minor(1), Currency::EUR);

        assert!(matches!(
            b.journal.record(forged),
            Err(JournalError::NotAnInversion { .. })
        ));
        assert_eq!(b.journal.reversal_of(original_id), None, "stays unreversed");
        assert_eq!(b.journal.len(), 1);
    }

    #[test]
    fn a_reversal_nets_the_original_out() {
        let mut b = Books::new();
        let original = b.sealed(b"orig", 4200);
        b.journal
            .record_validated(original.clone())
            .expect("records");

        let key = b.cash_key();
        assert_eq!(
            b.journal.balance(&key, None).expect("no overflow").debits,
            Eur::from_minor(4200)
        );

        let reversal = original.reverse(
            EntryId::generate(),
            IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
            date!(2026 - 04 - 01),
        );
        b.journal.record(reversal).expect("records");

        // Net is zero, but both gross totals survive: the reversal is visible.
        let after = b.journal.balance(&key, None).expect("no overflow");
        assert_eq!(after.signed_net().expect("ok"), Eur::ZERO);
        assert_eq!(after.debits, Eur::from_minor(4200));
        assert_eq!(after.credits, Eur::from_minor(4200));
    }

    #[test]
    fn a_reversal_of_a_sealed_period_books_into_an_open_one() {
        let mut b = Books::new();
        let original = b.sealed(b"orig", 100);
        b.journal
            .record_validated(original.clone())
            .expect("records");

        let march = b.march();
        b.journal
            .transition_period(&march, PeriodState::Closing)
            .expect("ok");
        b.journal.seal_period(&march).expect("seals");

        // Back into March: refused, because March no longer accepts postings.
        let refused = original.reverse(
            EntryId::generate(),
            IdempotencyKey::new(b"r1".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        );
        assert!(matches!(
            b.journal.record(refused),
            Err(JournalError::Invalid(_))
        ));

        // Into April: accepted, and it still says which period it belongs to.
        let accepted = original.reverse(
            EntryId::generate(),
            IdempotencyKey::new(b"r2".to_vec()).expect("valid"),
            date!(2026 - 04 - 01),
        );
        let recorded = b.journal.record(accepted).expect("books into April");
        let stored = b.journal.get(recorded.id).expect("recorded");
        assert_eq!(stored.original_booking_date(), Some(date!(2026 - 03 - 15)));
    }

    // ── balances ────────────────────────────────────────────────────────────

    #[test]
    fn the_journal_always_balances() {
        let mut b = Books::new();
        for i in 0..6i64 {
            b.record(format!("k{i}").as_bytes(), 10 + i);
        }
        assert!(b.journal.verify_balanced().expect("no overflow"));
        assert!(b.journal.verify_balances().expect("no overflow"));
        assert!(b.journal.verify_log());
    }

    #[test]
    fn maintained_balances_match_a_full_recomputation_at_every_size() {
        // The incremental trial balance is derived state and has to be checked
        // the same way the Merkle subtree stack is.
        let mut b = Books::new();
        for i in 0..40i64 {
            b.record(format!("k{i}").as_bytes(), 1 + i);
            assert!(
                b.journal.verify_balances().expect("no overflow"),
                "diverged after {i} entries"
            );
        }
    }

    #[test]
    fn a_prefix_fold_reconstructs_an_earlier_state() {
        let mut b = Books::new();
        b.record(b"a", 100);
        b.record(b"b", 200);
        b.record(b"c", 300);

        let key = b.cash_key();
        // The prefix is a *count* of entries, so zero is the empty ledger.
        for (size, expected) in [(0u64, 0i64), (1, 100), (2, 300), (3, 600)] {
            assert_eq!(
                b.journal.balance(&key, Some(size)).expect("ok").debits,
                Eur::from_minor(expected),
                "balance over the first {size} entries"
            );
        }
        assert_eq!(
            b.journal.balance(&key, None).expect("ok").debits,
            Eur::from_minor(600)
        );

        // A size past the end is the whole log rather than an error.
        assert_eq!(
            b.journal.balance(&key, Some(99)).expect("ok").debits,
            Eur::from_minor(600)
        );

        // And the whole trial balance agrees with the per-key answer.
        let tb = b.journal.trial_balance(Some(2)).expect("ok");
        assert_eq!(tb.get_or_zero(&key).debits, Eur::from_minor(300));
        assert!(b.journal.trial_balance(Some(0)).expect("ok").is_empty());
    }

    #[test]
    fn a_rejected_entry_leaves_the_balances_untouched() {
        let mut b = Books::new();
        b.record(b"a", 100);
        let before = b.journal.trial_balance(None).expect("ok");

        let draft = b.draft(b"a", 999);
        assert!(b.journal.record(draft).is_err());
        assert_eq!(before, b.journal.trial_balance(None).expect("ok"));
        assert!(b.journal.verify_balances().expect("ok"));
    }

    // ── statements ──────────────────────────────────────────────────────────

    // ── balance limits ──────────────────────────────────────────────────────

    #[test]
    fn an_account_can_be_forbidden_from_going_negative() {
        let mut b = Books::new();
        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");

        // Funding it is fine.
        b.record(b"funding", 1_000);

        // Drawing more than it holds is not.
        let overdraw = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"overdraw".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(1_001), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1_001), Currency::EUR);

        let err = b.journal.record(overdraw).expect_err("would overdraw");
        assert!(matches!(
            err,
            JournalError::LimitBreached {
                limit: BalanceLimit::NoCreditBalance,
                net_minor: -1,
                ..
            }
        ));

        // Refused whole: nothing about the journal moved.
        assert_eq!(b.journal.len(), 1);
        assert_eq!(
            b.journal.balance(&b.cash_key(), None).expect("ok").debits,
            Eur::from_minor(1_000)
        );
        assert!(b.journal.verify_balances().expect("ok"));
        assert!(b.journal.verify_log());
    }

    #[test]
    fn a_limit_permits_the_entry_that_lands_exactly_on_zero() {
        let mut b = Books::new();
        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");
        b.record(b"funding", 1_000);

        let drain = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"drain".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(1_000), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1_000), Currency::EUR);
        b.journal.record(drain).expect("zero is on the right side");
    }

    #[test]
    fn a_limit_is_judged_on_the_whole_entry_not_posting_by_posting() {
        // An entry that dips the account below its limit and back within one
        // booking is one movement. Judging it posting by posting would make the
        // answer depend on the order the caller listed them in.
        let mut b = Books::new();
        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");

        let round_trip = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"round-trip".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(500), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(500), Currency::EUR)
        .debit(b.cash, Eur::from_minor(500), Currency::EUR)
        .credit(b.revenue, Eur::from_minor(500), Currency::EUR);
        b.journal.record(round_trip).expect("nets to zero");
    }

    #[test]
    fn a_limit_binds_each_currency_and_layer_on_its_own() {
        let mut b = Books::new();
        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");
        b.record(b"eur-funding", 1_000);

        // A different currency is a different balance and starts at zero.
        let usd = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"usd".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(1), Currency::USD)
        .debit(b.revenue, Eur::from_minor(1), Currency::USD);
        assert!(
            matches!(
                b.journal.record(usd),
                Err(JournalError::LimitBreached {
                    currency: Currency::USD,
                    ..
                })
            ),
            "the EUR funding must not cover a USD draw"
        );

        // As is a different layer.
        let reserved = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"reserved".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .post(Posting::credit(b.cash, Eur::from_minor(1), Currency::EUR).in_layer(Layer::Pending))
        .post(
            Posting::debit(b.revenue, Eur::from_minor(1), Currency::EUR).in_layer(Layer::Pending),
        );
        assert!(matches!(
            b.journal.record(reserved),
            Err(JournalError::LimitBreached {
                layer: Layer::Pending,
                ..
            })
        ));
    }

    #[test]
    fn a_liability_can_be_forbidden_from_going_into_debit() {
        let mut b = Books::new();
        b.journal
            .accounts_mut()
            .set_limit(b.revenue, BalanceLimit::NoDebitBalance)
            .expect("registered");
        // The fixture credits revenue, so this stays within the limit.
        b.record(b"sale", 1_000);

        let clawback = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"too-much".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .debit(b.revenue, Eur::from_minor(1_001), Currency::EUR)
        .credit(b.cash, Eur::from_minor(1_001), Currency::EUR);
        assert!(matches!(
            b.journal.record(clawback),
            Err(JournalError::LimitBreached {
                limit: BalanceLimit::NoDebitBalance,
                net_minor: 1,
                ..
            })
        ));
    }

    #[test]
    fn tightening_a_limit_does_not_invalidate_what_is_already_booked() {
        // Master data governs the next booking, never the last one. An account
        // already past a newly imposed limit keeps its balance and simply
        // refuses to move further the wrong way.
        let mut b = Books::new();
        let overdrawn = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"draw".to_vec()).expect("valid"),
            date!(2026 - 03 - 15),
        )
        .credit(b.cash, Eur::from_minor(500), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(500), Currency::EUR);
        b.journal.record(overdrawn).expect("records");

        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");

        // What is recorded stays recorded and stays readable.
        assert_eq!(b.journal.len(), 1);
        assert_eq!(
            b.journal
                .balance(&b.cash_key(), None)
                .expect("ok")
                .signed_net()
                .expect("ok"),
            Eur::from_minor(-500)
        );

        // Moving further the wrong way is refused …
        let worse = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"worse".to_vec()).expect("valid"),
            date!(2026 - 03 - 16),
        )
        .credit(b.cash, Eur::from_minor(1), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1), Currency::EUR);
        assert!(matches!(
            b.journal.record(worse),
            Err(JournalError::LimitBreached { .. })
        ));

        // … while repairing it is not.
        b.record(b"repair", 500);
    }

    #[test]
    fn a_limit_can_refuse_a_reversal() {
        // Worth pinning, because it is the one place a limit collides with the
        // correction rules: reversing a funding entry withdraws money the
        // account may since have committed. A limit constrains the *resulting
        // balance*, so it cannot make an exception for a correction — and
        // silently letting one through would be a limit that does not hold.
        //
        // The way out is the ordinary one: reverse whatever consumed the
        // funding first, or lift the limit deliberately.
        let mut b = Books::new();
        let funding = b.sealed(b"funding", 1_000);
        b.journal
            .record_validated(funding.clone())
            .expect("records");

        let spend = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"spend".to_vec()).expect("valid"),
            date!(2026 - 03 - 16),
        )
        .credit(b.cash, Eur::from_minor(1_000), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1_000), Currency::EUR);
        b.journal.record(spend).expect("records");

        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");

        let undo = funding.reverse(
            EntryId::generate(),
            IdempotencyKey::new(b"undo".to_vec()).expect("valid"),
            date!(2026 - 03 - 17),
        );
        assert!(matches!(
            b.journal.record(undo),
            Err(JournalError::LimitBreached { .. })
        ));
        assert_eq!(
            b.journal.reversal_of(funding.id()),
            None,
            "a refused reversal must not mark the original as corrected"
        );
    }

    #[test]
    fn a_batch_that_breaches_a_limit_lands_in_full_or_not_at_all() {
        let mut b = Books::new();
        b.journal
            .accounts_mut()
            .set_limit(b.cash, BalanceLimit::NoCreditBalance)
            .expect("registered");
        let before = b.journal.head();

        let fund = b.sealed(b"fund", 1_000);
        let draw = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"draw".to_vec()).expect("valid"),
            date!(2026 - 03 - 15),
        )
        .credit(b.cash, Eur::from_minor(1_500), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1_500), Currency::EUR)
        .seal(&b.journal.context())
        .expect("balances");

        assert!(matches!(
            b.journal.record_batch(vec![fund, draw]),
            Err(JournalError::LimitBreached { .. })
        ));
        assert!(b.journal.is_empty(), "the funding entry must unwind too");
        assert_eq!(b.journal.head(), before);
        assert!(b.journal.verify_balances().expect("ok"));
        assert!(b.journal.verify_log());
    }

    #[test]
    fn a_statement_shows_movements_and_the_running_balance() {
        let mut b = Books::new();
        b.record(b"a", 100);
        b.record(b"b", 250);

        let lines = b.journal.statement(&b.cash_key()).expect("no overflow");
        assert_eq!(lines.len(), 2);

        let first = lines.first().expect("present");
        assert_eq!(first.amount, Eur::from_minor(100));
        assert_eq!(first.direction, Direction::Debit);
        assert_eq!(first.running.debits, Eur::from_minor(100));

        let second = lines.get(1).expect("present");
        assert_eq!(second.running.debits, Eur::from_minor(350));
        assert_eq!(second.booking_date, date!(2026 - 03 - 15));
    }

    #[test]
    fn a_statement_ignores_other_accounts_and_layers() {
        let mut b = Books::new();
        b.record(b"a", 100);

        let pending = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"p".to_vec()).expect("valid"),
            date!(2026 - 03 - 16),
        )
        .post(Posting::debit(b.cash, Eur::from_minor(500), Currency::EUR).in_layer(Layer::Pending))
        .post(
            Posting::credit(b.revenue, Eur::from_minor(500), Currency::EUR)
                .in_layer(Layer::Pending),
        );
        b.journal.record(pending).expect("records");

        let settled = b.cash_key();
        assert_eq!(b.journal.statement(&settled).expect("ok").len(), 1);

        let reserved = BalanceKey {
            layer: Layer::Pending,
            ..settled
        };
        assert_eq!(b.journal.statement(&reserved).expect("ok").len(), 1);
        assert_eq!(
            b.journal.balance(&reserved, None).expect("ok").debits,
            Eur::from_minor(500)
        );
    }

    #[test]
    fn description_does_not_affect_ordering_or_balance() {
        let mut b = Books::new();
        let entry = b
            .draft(b"k", 500)
            .with_description(Description::new("annotated").expect("valid"));
        b.journal.record(entry).expect("records");
        assert!(b.journal.verify_balanced().expect("ok"));
    }

    // ── clearing ────────────────────────────────────────────────────────────

    #[test]
    fn open_items_track_invoices_against_payments() {
        let mut b = Books::new();

        // An invoice raises a receivable; a payment settles part of it.
        let invoice = b.record(b"invoice", 1000);

        let payment = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(400), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(400), Currency::EUR);
        let payment = b.journal.record(payment).expect("records");

        let key = b.cash_key();

        // Before clearing, both postings are open.
        assert_eq!(b.journal.open_items(&key).expect("ok").len(), 2);

        b.journal
            .clear(
                Clearing::new(ClearingId::generate(), key, date!(2026 - 03 - 20))
                    .apply(PostingRef::new(invoice.id, 0), Eur::from_minor(400))
                    .apply(PostingRef::new(payment.id, 0), Eur::from_minor(400)),
            )
            .expect("clears");

        // The payment is fully applied; the invoice keeps its remainder open.
        let open = b.journal.open_items(&key).expect("ok");
        assert_eq!(open.len(), 1);
        assert_eq!(
            open.first().expect("present").residual,
            Eur::from_minor(600)
        );
        assert_eq!(b.journal.clearings().len(), 1);
    }

    #[test]
    fn clearing_does_not_change_any_balance() {
        let mut b = Books::new();
        let invoice = b.record(b"invoice", 1000);
        let payment = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(1000), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1000), Currency::EUR);
        let payment = b.journal.record(payment).expect("records");

        let before = b.journal.trial_balance(None).expect("ok");
        b.journal
            .clear(
                Clearing::new(ClearingId::generate(), b.cash_key(), date!(2026 - 03 - 20))
                    .apply(PostingRef::new(invoice.id, 0), Eur::from_minor(1000))
                    .apply(PostingRef::new(payment.id, 0), Eur::from_minor(1000)),
            )
            .expect("clears");

        // Clearing is an assignment, not a movement.
        assert_eq!(before, b.journal.trial_balance(None).expect("ok"));
        assert!(b.journal.verify_log());
    }

    #[test]
    fn a_clearing_cannot_cross_layers() {
        let mut b = Books::new();
        let settled = b.record(b"settled", 500);
        let reserved = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"reserved".to_vec()).expect("valid"),
            date!(2026 - 03 - 16),
        )
        .post(Posting::credit(b.cash, Eur::from_minor(500), Currency::EUR).in_layer(Layer::Pending))
        .post(
            Posting::debit(b.revenue, Eur::from_minor(500), Currency::EUR).in_layer(Layer::Pending),
        );
        let reserved = b.journal.record(reserved).expect("records");

        let attempt = Clearing::new(ClearingId::generate(), b.cash_key(), date!(2026 - 03 - 20))
            .apply(PostingRef::new(settled.id, 0), Eur::from_minor(500))
            .apply(PostingRef::new(reserved.id, 0), Eur::from_minor(500));
        assert!(matches!(
            b.journal.clear(attempt),
            Err(JournalError::Clearing(ClearingError::WrongLayer { .. }))
        ));
    }

    // ── periods and seals ───────────────────────────────────────────────────

    #[test]
    fn sealing_requires_a_closing_period() {
        let mut b = Books::new();
        let id = b.march();
        b.record(b"a", 100);

        // Open is not enough: postings must be stopped before verification runs.
        assert!(matches!(
            b.journal.seal_period(&id),
            Err(JournalError::Period(PeriodError::NotClosing {
                state: PeriodState::Open,
                ..
            }))
        ));

        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        let seal = b.journal.seal_period(&id).expect("seals");
        assert!(seal.is_self_consistent());
        assert_eq!(seal.entry_count, 1);
        assert_eq!(
            b.journal.calendar().state_on(date!(2026 - 03 - 15)),
            PeriodState::Sealed
        );
    }

    #[test]
    fn a_sealed_closing_balance_cannot_be_restated_afterwards() {
        // A seal's closing balance is *cumulative* through the period's last
        // day. That claim is only worth anything if nothing can be booked at or
        // before that day afterwards — and two ordinary, individually legal
        // writes used to be able to.
        let mut b = Books::new();
        let feb = PeriodId::new("2026-02").expect("valid");
        b.journal
            .define_period(
                Period::new(feb.clone(), date!(2026 - 02 - 01), date!(2026 - 02 - 28))
                    .expect("valid range"),
            )
            .expect("defines");
        let march = b.march();
        b.record(b"m1", 100);

        // 1. March cannot be sealed while February is still open, because a
        //    later February booking would restate March's closing balance.
        b.journal
            .transition_period(&march, PeriodState::Closing)
            .expect("ok");
        assert!(matches!(
            b.journal.seal_period(&march),
            Err(JournalError::Period(PeriodError::UnsealedPredecessor {
                ref predecessor,
                ..
            })) if *predecessor == feb
        ));

        // Seal them in order instead. March is already closing.
        b.journal
            .transition_period(&feb, PeriodState::Closing)
            .expect("ok");
        b.journal.seal_period(&feb).expect("February seals first");
        b.journal.seal_period(&march).expect("then March");
        let sealed = b
            .journal
            .seals()
            .get(&march)
            .expect("March is sealed")
            .clone();

        // 2. Nothing may be booked at or before the watermark any more — not
        //    into February, and not into a gap the calendar never defined.
        assert_eq!(
            b.journal.calendar().sealed_through(),
            Some(date!(2026 - 03 - 31))
        );
        for (key, on) in [
            (b"f1".as_slice(), date!(2026 - 02 - 10)),
            (b"gap".as_slice(), date!(2025 - 11 - 30)),
            (b"last".as_slice(), date!(2026 - 03 - 31)),
        ] {
            let draft = b.draft_on(key, 500, on);
            let err = b
                .journal
                .record(draft)
                .expect_err("the books are sealed through 2026-03-31");
            assert!(matches!(err, JournalError::Invalid(ref e)
            if e.any(|v| matches!(v, crate::entry::ValidationError::ClosedPeriod {
                state: PeriodState::Sealed, ..
            }))));
        }

        // So the sealed closing balance still describes the books exactly.
        let recomputed = b
            .journal
            .trial_balance_through_date(date!(2026 - 03 - 31))
            .expect("no overflow");
        assert_eq!(
            crate::seal::trial_balance_head(&recomputed),
            sealed.trial_balance
        );

        // And the next day forward is still perfectly open.
        let draft = b.draft_on(b"apr", 700, date!(2026 - 04 - 01));
        b.journal.record(draft).expect("April is open");
    }

    #[test]
    fn sealing_an_unknown_period_is_an_error() {
        let mut b = Books::new();
        let ghost = PeriodId::new("nope").expect("valid");
        assert!(matches!(
            b.journal.seal_period(&ghost),
            Err(JournalError::Period(PeriodError::Unknown { .. }))
        ));
    }

    #[test]
    fn a_sealed_period_cannot_be_sealed_twice() {
        let mut b = Books::new();
        let id = b.march();
        b.record(b"a", 100);
        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        b.journal.seal_period(&id).expect("seals");
        assert!(matches!(
            b.journal.seal_period(&id),
            Err(JournalError::Period(PeriodError::NotClosing {
                state: PeriodState::Sealed,
                ..
            }))
        ));
        assert_eq!(b.journal.seals().len(), 1);
    }

    #[test]
    fn consecutive_seals_chain_and_verify() {
        let mut b = Books::new();
        let march_id = b.march();
        b.record(b"a", 100);
        b.journal
            .transition_period(&march_id, PeriodState::Closing)
            .expect("ok");
        let first = b.journal.seal_period(&march_id).expect("seals");

        let april_id = PeriodId::new("2026-04").expect("valid");
        b.journal
            .define_period(
                Period::new(
                    april_id.clone(),
                    date!(2026 - 04 - 01),
                    date!(2026 - 04 - 30),
                )
                .expect("valid range"),
            )
            .expect("defines");
        b.journal
            .transition_period(&april_id, PeriodState::Closing)
            .expect("ok");
        let second = b.journal.seal_period(&april_id).expect("seals");

        assert_eq!(first.prev_seal, None);
        assert_eq!(second.prev_seal, Some(first.seal_hash));
        assert_eq!(b.journal.seals().len(), 2);
        assert!(b.journal.verify_seals().is_ok());
    }

    #[test]
    fn a_seal_excludes_entries_booked_into_later_periods() {
        // Sealing March in April is the normal case; April must not leak into
        // March's closing balance.
        let mut b = Books::new();
        let id = b.march();
        b.record(b"march", 100);

        let april = b.draft_on(b"april", 900, date!(2026 - 04 - 10));
        b.journal.record(april).expect("records");

        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        let seal = b.journal.seal_period(&id).expect("seals");

        assert_eq!(seal.entry_count, 1, "only the March entry belongs to March");

        let march_only = b
            .journal
            .trial_balance_through_date(date!(2026 - 03 - 31))
            .expect("ok");
        assert_eq!(
            seal.trial_balance,
            crate::seal::trial_balance_head(&march_only)
        );
        assert_ne!(
            seal.trial_balance,
            crate::seal::trial_balance_head(&b.journal.trial_balance(None).expect("ok")),
            "the whole-journal balance must not be what was sealed"
        );
    }

    #[test]
    fn a_seal_commits_to_the_balances_at_that_moment() {
        let mut b = Books::new();
        let id = b.march();
        b.record(b"a", 100);
        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        let seal = b.journal.seal_period(&id).expect("seals");

        // A later entry cannot retroactively change what the seal committed to.
        let later = b.draft_on(b"later", 200, date!(2026 - 04 - 05));
        b.journal.record(later).expect("records");

        let recomputed =
            crate::seal::trial_balance_head(&b.journal.trial_balance(None).expect("ok"));
        assert_ne!(recomputed, seal.trial_balance);
        assert!(seal.is_self_consistent());
    }

    #[test]
    fn one_call_proves_and_names_a_sealed_balance() {
        // The whole audit answer, assembled where it cannot be assembled wrong.
        // The engine and every durable backend route through the same
        // `SealedBalance::assemble`, so they cannot drift.
        let mut b = Books::new();
        let id = b.march();
        b.record(b"m", 119_000);
        // Sealing March in April is the normal case, so the log holds a later
        // entry when the seal is taken — which is why the closing balance is a
        // fold by booking date rather than a prefix of the log.
        let later = b.draft_on(b"apr", 7_777, date!(2026 - 04 - 05));
        b.journal.record(later).expect("April is open");
        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        let seal = b.journal.seal_period(&id).expect("seals");

        // The books move on in every way that used to break the binding proof.
        b.journal
            .accounts_mut()
            .register_path("Assets:Bank", date!(2026 - 05 - 01))
            .expect("registers");
        b.journal
            .accounts_mut()
            .close(b.cash, date!(2026 - 05 - 31))
            .expect("registered");
        b.journal
            .accounts_mut()
            .set_limit(b.revenue, BalanceLimit::NoDebitBalance)
            .expect("registered");

        let proven = b
            .journal
            .prove_sealed_balance(&id, b.cash_key())
            .expect("the books still reproduce the seal")
            .into_proven()
            .expect("cash has a row");
        assert!(proven.verify());
        assert_eq!(proven.path().to_string(), "Assets:Cash");
        assert_eq!(proven.seal.seal_hash, seal.seal_hash);
        // April must not have leaked into March's closing balance.
        assert_eq!(proven.balance.balance.debits, Eur::from_minor(119_000));

        // A registered account with no row is `None`, not a fabricated zero.
        let no_row = BalanceKey {
            currency: Currency::USD,
            ..b.cash_key()
        };
        assert_eq!(
            b.journal
                .prove_sealed_balance(&id, no_row)
                .expect("no error"),
            SealedBalanceOutcome::NoRow,
        );

        // An account onboarded after the seal is a different answer again.
        let later_account = BalanceKey {
            account: AccountId::from_index(2),
            ..b.cash_key()
        };
        // An account onboarded after the seal is an *answer*, not an error: the
        // books are intact and the seal simply cannot name it.
        assert_eq!(
            b.journal
                .prove_sealed_balance(&id, later_account)
                .expect("nothing is wrong with the books"),
            SealedBalanceOutcome::NotYetRegistered,
        );
        assert!(
            b.journal
                .prove_sealed_balance(&id, later_account)
                .expect("no error")
                .is_absent()
        );

        // And an unsealed period has nothing to prove.
        let ghost = PeriodId::new("2026-09").expect("valid");
        assert!(matches!(
            b.journal.prove_sealed_balance(&ghost, b.cash_key()),
            Err(JournalError::SealedBalance(
                SealedBalanceError::NotSealed { .. }
            ))
        ));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_sealed_balance_survives_the_trip_to_whoever_needs_it() {
        // The artifact exists to leave the process that built it. A claim an
        // auditor cannot be handed is not a claim.
        let mut b = Books::new();
        let id = b.march();
        b.record(b"m", 119_000);
        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        b.journal.seal_period(&id).expect("seals");

        let proven = b
            .journal
            .prove_sealed_balance(&id, b.cash_key())
            .expect("provable")
            .into_proven()
            .expect("cash has a row");

        let json = serde_json::to_string(&proven).expect("serialises");
        let received: SealedBalance<2> = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(received, proven);
        // The receiver checks it themselves, holding nothing but this.
        assert!(received.verify());
        assert_eq!(received.path().to_string(), "Assets:Cash");

        // A seal edited on the wire does not even deserialise, so a recipient
        // who never thinks to call `verify` still cannot be fooled.
        let forged = json.replace("\"entry_count\":1", "\"entry_count\":99");
        assert_ne!(forged, json, "the test must actually alter the payload");
        assert!(serde_json::from_str::<SealedBalance<2>>(&forged).is_err());
    }

    #[test]
    fn a_sealed_balance_can_be_proven_from_the_seal_alone() {
        let mut b = Books::new();
        let id = b.march();
        b.record(b"a", 119_000);
        b.journal
            .transition_period(&id, PeriodState::Closing)
            .expect("ok");
        let seal = b.journal.seal_period(&id).expect("seals");

        let closing = b
            .journal
            .trial_balance_through_date(date!(2026 - 03 - 31))
            .expect("ok");
        let proof = crate::seal::TrialBalanceCommitment::of(&closing)
            .prove(&b.cash_key())
            .expect("cash was posted to");
        assert!(proof.verify_against(&seal));
        assert_eq!(proof.balance.debits, Eur::from_minor(119_000));
    }

    // ── checkpoints and assertions ──────────────────────────────────────────

    #[test]
    fn a_checkpoint_round_trips_against_the_journal() {
        let mut b = Books::new();
        b.record(b"a", 100);
        b.record(b"b", 250);

        let cp = b.journal.checkpoint(&b.cash_key()).expect("no overflow");
        assert_eq!(cp.balance.debits, Eur::from_minor(350));
        assert!(b.journal.verify_checkpoint(&cp).is_ok());
    }

    #[test]
    fn a_prefix_checkpoint_stays_valid_as_the_log_grows() {
        let mut b = Books::new();
        b.record(b"a", 100);
        let stale = b.journal.checkpoint(&b.cash_key()).expect("ok");

        // The log grows. The checkpoint still describes a real prefix, and its
        // pinned head lets it be re-derived exactly, so it remains valid.
        b.record(b"b", 250);
        assert!(b.journal.verify_checkpoint(&stale).is_ok());

        // A restated balance is caught.
        let mut restated = stale;
        restated.balance.debits = Eur::from_minor(9_999);
        assert!(matches!(
            b.journal.verify_checkpoint(&restated),
            Err(CheckpointError::BalanceMismatch)
        ));

        // So is a checkpoint claiming a history the log never had.
        let mut forged = stale;
        forged.tree_head.root = crate::Hash::from_bytes([0xabu8; 32]);
        assert!(matches!(
            b.journal.verify_checkpoint(&forged),
            Err(CheckpointError::HeadMismatch)
        ));
    }

    #[test]
    fn a_checkpoint_beyond_the_log_is_rejected() {
        let mut b = Books::new();
        b.record(b"a", 100);
        let mut cp = b.journal.checkpoint(&b.cash_key()).expect("ok");
        cp.tree_head.size = 99;
        assert!(matches!(
            b.journal.verify_checkpoint(&cp),
            Err(CheckpointError::SizeOutOfRange { size: 99 })
        ));
    }

    #[test]
    fn a_checkpoint_over_an_empty_log_stays_valid_forever() {
        // The prefix is the tree head's size, so an empty-prefix checkpoint says
        // "nothing had moved after zero entries" — which no later append can
        // falsify. When the position was a separate `Option`, `None` meant
        // "empty" here and "current" to the balance reader, and this checkpoint
        // silently started failing the moment anything was recorded.
        let mut b = Books::new();
        let key = b.cash_key();
        let cp = b.journal.checkpoint(&key).expect("ok");
        assert_eq!(cp.size(), 0);
        assert!(b.journal.verify_checkpoint(&cp).is_ok());

        b.record(b"a", 100);
        b.record(b"b", 250);
        assert!(
            b.journal.verify_checkpoint(&cp).is_ok(),
            "an empty-prefix checkpoint must survive the log growing"
        );
    }

    #[test]
    fn balance_assertions_catch_divergence() {
        let mut b = Books::new();
        b.record(b"a", 100);
        b.record(b"b", 250);
        let key = b.cash_key();

        let holds = BalanceAssertion::net(key, Eur::from_minor(350));
        assert!(b.journal.check_assertion(&holds).expect("ok").held());

        let wrong = BalanceAssertion::net(key, Eur::from_minor(300));
        let outcome = b.journal.check_assertion(&wrong).expect("ok");
        assert!(!outcome.held());
        assert!(matches!(
            outcome,
            AssertionOutcome::Failed { difference, .. } if difference == Eur::from_minor(50)
        ));
    }

    #[test]
    fn an_assertion_can_target_an_earlier_log_position() {
        let mut b = Books::new();
        b.record(b"a", 100);
        b.record(b"b", 250);
        let key = b.cash_key();
        let earlier = BalanceAssertion::net(key, Eur::from_minor(100)).over_prefix(1);
        assert!(b.journal.check_assertion(&earlier).expect("ok").held());
        // The empty prefix is a legal target and asserts an untouched ledger.
        let nothing = BalanceAssertion::net(key, Eur::ZERO).over_prefix(0);
        assert!(b.journal.check_assertion(&nothing).expect("ok").held());
    }

    #[test]
    fn an_assertion_can_target_a_date_the_way_a_statement_does() {
        // What reconciliation actually needs: an external statement is dated,
        // not positioned, and a backdated entry has to land in the period it
        // economically belongs to rather than where it happened to be recorded.
        let mut b = Books::new();
        let march = b.draft_on(b"march", 100, date!(2026 - 03 - 15));
        b.journal.record(march).expect("records");
        let april = b.draft_on(b"april", 250, date!(2026 - 04 - 02));
        b.journal.record(april).expect("records");
        // Recorded last, booked first.
        let backdated = b.draft_on(b"backdated", 40, date!(2026 - 03 - 01));
        b.journal.record(backdated).expect("records");

        let key = b.cash_key();
        let closing =
            BalanceAssertion::net(key, Eur::from_minor(140)).on_date(date!(2026 - 03 - 31));
        assert!(
            b.journal.check_assertion(&closing).expect("ok").held(),
            "March closes at 1.40, backdated entry included"
        );
        assert_eq!(
            b.journal
                .balance_on_date(&key, date!(2026 - 04 - 30))
                .expect("ok")
                .debits,
            Eur::from_minor(390)
        );
    }

    #[test]
    fn open_items_are_oldest_first_whatever_identifiers_the_caller_brings() {
        // Open items exist to be cleared, and FIFO — apply this payment to the
        // oldest open invoice — is the workflow. "Oldest" is log order, which is
        // the crate's answer to ordering everywhere else, and deliberately not
        // entry-*identifier* order: identity is caller-supplied, so sorting by
        // it is chronological only when the caller happens to use this crate's
        // own time-ordered generator. These identifiers descend, so the two
        // orders are exact opposites.
        let mut b = Books::new();
        let ids = [
            EntryId::from_uuid(uuid::Uuid::from_u128(0xcccc << 112)),
            EntryId::from_uuid(uuid::Uuid::from_u128(0xbbbb << 112)),
            EntryId::from_uuid(uuid::Uuid::from_u128(0xaaaa << 112)),
        ];
        for (n, id) in ids.iter().enumerate() {
            let draft = Entry::<Draft, 2>::new(
                *id,
                IdempotencyKey::new(format!("inv-{n}").into_bytes()).expect("valid"),
                date!(2026 - 03 - 15),
            )
            .debit(b.cash, Eur::from_minor(100), Currency::EUR)
            .credit(b.revenue, Eur::from_minor(100), Currency::EUR);
            b.journal.record(draft).expect("records");
        }

        let key = b.cash_key();
        let log_order = b.journal.postings_on(&key);
        let items: Vec<_> = b
            .journal
            .open_items(&key)
            .expect("no overflow")
            .iter()
            .map(|i| i.posting)
            .collect();

        assert_eq!(items, log_order, "open items must be oldest-first");
        // And that is genuinely the opposite of sorting by identifier, so the
        // assertion above is not satisfied by both orders at once.
        let mut by_identifier = log_order.clone();
        by_identifier.sort();
        assert_ne!(
            by_identifier, log_order,
            "the fixture must actually distinguish the two orders"
        );
    }

    #[test]
    fn a_clearing_may_not_be_recorded_twice_under_one_identifier() {
        let mut b = Books::new();
        let invoice = b.record(b"invoice", 1000);
        let payment = Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
            date!(2026 - 03 - 20),
        )
        .credit(b.cash, Eur::from_minor(1000), Currency::EUR)
        .debit(b.revenue, Eur::from_minor(1000), Currency::EUR);
        let payment = b.journal.record(payment).expect("records");

        let id = ClearingId::generate();
        let key = b.cash_key();
        let build = || {
            Clearing::new(id, key, date!(2026 - 03 - 20))
                .apply(PostingRef::new(invoice.id, 0), Eur::from_minor(400))
                .apply(PostingRef::new(payment.id, 0), Eur::from_minor(400))
        };
        b.journal.clear(build()).expect("clears");
        assert!(matches!(
            b.journal.clear(build()),
            Err(JournalError::Clearing(ClearingError::DuplicateId { .. }))
        ));
    }

    // ── batches ─────────────────────────────────────────────────────────────

    #[test]
    fn a_batch_lands_in_full_or_not_at_all() {
        let mut b = Books::new();
        b.record(b"existing", 100);
        let before = b.journal.clone();

        // The third entry reuses the second's key with different content.
        let batch = vec![
            b.sealed(b"batch-a", 100),
            b.sealed(b"batch-b", 200),
            b.sealed(b"batch-b", 300),
        ];
        assert!(matches!(
            b.journal.record_batch(batch),
            Err(JournalError::IdempotencyConflict { .. })
        ));

        // Byte-identical to how it started: index, keys, balances, tree head.
        assert_eq!(b.journal.len(), before.len());
        assert_eq!(b.journal.head(), before.head());
        assert_eq!(
            b.journal.trial_balance(None).expect("ok"),
            before.trial_balance(None).expect("ok")
        );
        assert!(b.journal.verify_log());
        assert!(b.journal.verify_balances().expect("ok"));
        assert!(b.journal.statement(&b.cash_key()).expect("ok").len() == 1);
    }

    #[test]
    fn rolling_back_a_batch_releases_its_keys_and_identifiers() {
        let mut b = Books::new();
        let good = b.sealed(b"reusable", 100);
        let poison = b.sealed(b"poison", 200);
        let clash = b.sealed(b"poison", 300);

        assert!(
            b.journal
                .record_batch(vec![good.clone(), poison, clash])
                .is_err()
        );

        // The rolled-back key and identifier are free again, so a corrected
        // batch can reuse them.
        b.journal
            .record_batch(vec![good])
            .expect("the key was released");
        assert_eq!(b.journal.len(), 1);
        assert!(b.journal.verify_log());
    }

    #[test]
    fn rolling_back_a_batch_releases_a_reversal_link() {
        let mut b = Books::new();
        let original = b.sealed(b"orig", 100);
        let original_id = original.id();
        b.journal.record_validated(original.clone()).expect("ok");

        let reversal = original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(b"rev".to_vec()).expect("valid"),
                date!(2026 - 04 - 01),
            )
            .seal(&b.journal.context())
            .expect("balances");
        let poison = b.sealed(b"orig", 999);

        assert!(b.journal.record_batch(vec![reversal, poison]).is_err());
        assert_eq!(
            b.journal.reversal_of(original_id),
            None,
            "an undone reversal must not leave the original marked corrected"
        );
        assert_eq!(b.journal.len(), 1);
    }

    #[test]
    fn a_replay_inside_a_batch_is_not_a_failure() {
        let mut b = Books::new();
        let first = b.record(b"a", 100);
        let outcomes = b
            .journal
            .record_batch(vec![b.sealed(b"a", 100), b.sealed(b"c", 300)])
            .expect("a replay is not a conflict");
        assert_eq!(outcomes.len(), 2);
        assert!(!outcomes[0].is_new);
        assert_eq!(outcomes[0].id, first.id);
        assert!(outcomes[1].is_new);
        assert_eq!(b.journal.len(), 2);
    }
}
