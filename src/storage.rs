//! Persistence.
//!
//! The engine keeps no storage of its own. This module defines what a backend
//! must do, ships an in-memory one, and — more usefully — ships the
//! [`conformance`] suite that decides whether any other backend is correct.
//!
//! # Why a conformance suite is part of the library
//!
//! A ledger's guarantees are only as good as its weakest backend. Publishing the
//! trait alone would leave every implementor to guess what "idempotent" or
//! "atomic" means here, and the failures that follow are silent. The suite makes
//! the contract executable: a backend either passes it or is not a backend.
//!
//! # Pagination, not streams
//!
//! Reads are cursor-paged rather than streamed. A cursor maps onto
//! `WHERE index > ? ORDER BY index LIMIT ?` in any SQL backend, survives a
//! dropped connection, and needs no async-iteration machinery — so this crate
//! stays free of a futures dependency and a backend stays free of an executor
//! choice.
//!
//! # Static and dynamic dispatch
//!
//! [`LedgerStore`] uses `async fn` in trait, which compiles to static dispatch
//! with no per-call allocation but is not `dyn`-compatible. Where a backend must
//! be chosen at run time, [`DynLedgerStore`] boxes the futures and restores
//! object safety. Static by default, dynamic when you ask.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use time::Date;

use crate::account::{AccountId, AccountRecord};
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::checkpoint::Checkpoint;
use crate::clearing::{Clearing, ClearingId, OpenItem};
use crate::entry::{Balanced, Entry, EntryId};
use crate::hash::Hash;
use crate::journal::{Journal, JournalError, LogIndex, NotSequenced, Recorded};
use crate::merkle::{ConsistencyProof, InclusionProof, TreeHead};
use crate::money::Currency;
use crate::period::{LedgerId, Period, PeriodId, PeriodState};
use crate::posting::Layer;
use crate::seal::Seal;

pub mod conformance;

#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
pub mod postgres;

#[cfg(feature = "sqlite")]
#[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
pub mod sqlite;

#[cfg(feature = "iceberg")]
#[cfg_attr(docsrs, doc(cfg(feature = "iceberg")))]
pub mod iceberg;

/// Default number of records a page returns.
pub const DEFAULT_PAGE_SIZE: usize = 256;

/// Largest page a store will return, however large a limit is requested.
pub const MAX_PAGE_SIZE: usize = 4096;

/// A set of entries that must land together or not at all.
///
/// Atomicity across entries is not optional: an invoice and the entry that
/// offsets it must not be separable by a crash, and a single-entry append cannot
/// express that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryBatch<const P: u8> {
    entries: Vec<Entry<Balanced, P>>,
}

impl<const P: u8> EntryBatch<P> {
    /// Creates a batch from at least one entry.
    pub fn new(entries: Vec<Entry<Balanced, P>>) -> Result<Self, BatchError> {
        if entries.is_empty() {
            return Err(BatchError::Empty);
        }
        Ok(Self { entries })
    }

    /// Creates a batch holding one entry.
    #[must_use]
    pub fn single(entry: Entry<Balanced, P>) -> Self {
        Self {
            entries: vec![entry],
        }
    }

    /// The entries, in the order they will be appended.
    #[must_use]
    pub fn entries(&self) -> &[Entry<Balanced, P>] {
        &self.entries
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always false: a batch holds at least one entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Failure building a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BatchError {
    /// The batch held no entries.
    #[error("an entry batch must hold at least one entry")]
    Empty,
}

/// Where to resume reading from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Return records strictly after this index; `None` starts at the beginning.
    pub after: Option<LogIndex>,
    /// Maximum records to return, clamped to [`MAX_PAGE_SIZE`].
    pub limit: usize,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            after: None,
            limit: DEFAULT_PAGE_SIZE,
        }
    }
}

impl Cursor {
    /// A cursor starting at the beginning of the log.
    #[must_use]
    pub fn start() -> Self {
        Self::default()
    }

    /// A cursor resuming after `index`.
    #[must_use]
    pub fn after(index: LogIndex) -> Self {
        Self {
            after: Some(index),
            limit: DEFAULT_PAGE_SIZE,
        }
    }

    /// Sets the page size.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// The effective limit, clamped and never zero.
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        self.limit.clamp(1, MAX_PAGE_SIZE)
    }
}

/// One entry as it sits in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry<const P: u8> {
    /// Position in the log, or `None` if it has not been sequenced yet.
    pub index: Option<LogIndex>,
    /// The entry.
    pub entry: Entry<Balanced, P>,
    /// Its content hash, as committed to by the log.
    pub content_hash: Hash,
}

impl<const P: u8> StoredEntry<P> {
    /// The log position, or an error naming the entry that has none.
    ///
    /// Records returned by [`LedgerStore::page`] are always sequenced — an
    /// unsequenced entry is not in the log — so this cannot fail there. It can
    /// for [`LedgerStore::get`], which finds an entry the moment it is durable.
    ///
    /// # Errors
    ///
    /// Returns [`NotSequenced`] when the entry has not been assigned a position.
    pub fn require_index(&self) -> Result<LogIndex, NotSequenced> {
        self.index.ok_or(NotSequenced {
            id: self.entry.id(),
        })
    }
}

/// A page of log records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<const P: u8> {
    /// The records, in log order.
    pub records: Vec<StoredEntry<P>>,
    /// Cursor for the next page, or `None` at the end of the log.
    pub next: Option<Cursor>,
}

impl<const P: u8> Page<P> {
    /// True when the page holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// A durable home for a ledger.
///
/// # Contract
///
/// Implementations must guarantee all of the following. The [`conformance`]
/// suite checks each one.
///
/// 1. **Append-only.** Recorded entries are never modified or removed.
/// 2. **Atomic batches.** Every entry in a batch lands, or none does.
/// 3. **Idempotent.** Re-appending an entry whose idempotency key is already
///    present with identical content is a no-op returning the original outcome.
///    The same key with different content is an error, never an overwrite.
///    The uniqueness check must be part of the write itself, not a prior read —
///    a read-then-write races under concurrency and duplicates the entry.
/// 4. **Dense, ordered indices.** Log indices start at zero and increase by one
///    per entry, with no gaps, in commit order. A backend may assign them during
///    the append or afterwards; if afterwards, [`LedgerStore::append`] returns
///    `index: None` until [`LedgerStore::sequence`] has run.
/// 5. **Stable reads.** A record read twice returns the same bytes and the same
///    content hash — every field of it, including the entry's `kind` and each
///    posting's dimensions, because those are covered by that hash. A backend
///    that drops one does not under-report; it makes the entry unreadable.
/// 6. **Master data survives a restart.** An account comes back at the handle it
///    was issued, and a period comes back in the state it was left in. Both are
///    ledger state: a repointed handle rewrites which account history refers to,
///    and a sealed period that reopens accepts postings into books already
///    committed to.
/// 7. **Seals chain.** [`LedgerStore::seals`] returns them in chain order, and
///    what comes back reproduces a chain that verifies.
///
/// # Sequencing
///
/// Assigning positions inline means serialising appends: the next index cannot
/// be read until the previous writer has committed. Assigning them out of band
/// lets writers insert concurrently and leaves ordering to a single sequencer —
/// at the cost of a window in which an entry is durable but not yet provable.
///
/// Both are legitimate. The contract covers both, and [`LedgerStore::sequence`]
/// is a no-op for backends that need none.
pub trait LedgerStore<const P: u8>: Send + Sync {
    /// The backend's failure type.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The ledger this handle serves.
    ///
    /// Bound at construction rather than passed per call, so two ledgers cannot
    /// be mixed by a caller that forgets an argument.
    fn ledger(&self) -> &LedgerId;

    /// Appends a batch atomically.
    fn append(
        &self,
        batch: &EntryBatch<P>,
    ) -> impl Future<Output = Result<Vec<Recorded>, Self::Error>> + Send;

    /// Records an account and the handle it was issued.
    ///
    /// Handles are positions in registration order and are written into every
    /// posting row and into the trial balance leaves a seal commits to, so the
    /// binding is ledger state that has to outlive the process that made it.
    /// Re-registering an existing handle with the same account is a no-op.
    fn register_account(
        &self,
        record: &AccountRecord,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Every stored account with its handle, in handle order.
    ///
    /// Feed this to [`AccountRegistry::from_records`](crate::account::AccountRegistry::from_records) on start-up rather than
    /// re-registering paths, which would reissue handles in whatever order the
    /// caller happened to use and silently repoint history.
    fn accounts(&self) -> impl Future<Output = Result<Vec<AccountRecord>, Self::Error>> + Send;

    /// Fetches one entry by identifier.
    fn get(
        &self,
        id: EntryId,
    ) -> impl Future<Output = Result<Option<StoredEntry<P>>, Self::Error>> + Send;

    /// Reads a page of the log.
    fn page(&self, cursor: Cursor) -> impl Future<Output = Result<Page<P>, Self::Error>> + Send;

    /// The current tree head.
    fn head(&self) -> impl Future<Output = Result<TreeHead, Self::Error>> + Send;

    /// Assigns log positions to everything recorded but not yet sequenced.
    ///
    /// Returns the number of entries sequenced. Backends that assign positions
    /// during the append have nothing to do and return zero.
    ///
    /// Safe to call concurrently and repeatedly; a backend must ensure only one
    /// sequencing pass makes progress at a time, since the positions it assigns
    /// must be dense.
    fn sequence(&self) -> impl Future<Output = Result<u64, Self::Error>> + Send {
        async { Ok(0) }
    }

    /// Number of entries in the log.
    ///
    /// Counts sequenced entries only: an entry without a position is not yet
    /// part of the log.
    fn len(&self) -> impl Future<Output = Result<u64, Self::Error>> + Send;

    /// True when the log holds nothing.
    fn is_empty(&self) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        async { self.len().await.map(|n| n == 0) }
    }

    /// One account balance, optionally as of a log position.
    fn balance(
        &self,
        key: BalanceKey,
        through: Option<LogIndex>,
    ) -> impl Future<Output = Result<Balance<P>, Self::Error>> + Send;

    /// The trial balance, optionally as of a log position.
    fn trial_balance(
        &self,
        through: Option<LogIndex>,
    ) -> impl Future<Output = Result<TrialBalance<P>, Self::Error>> + Send;

    /// Proves an entry is committed to by the current head.
    fn prove_inclusion(
        &self,
        index: LogIndex,
    ) -> impl Future<Output = Result<InclusionProof, Self::Error>> + Send;

    /// Proves the log at `old_size` is a prefix of the current log.
    fn prove_consistency(
        &self,
        old_size: u64,
    ) -> impl Future<Output = Result<ConsistencyProof, Self::Error>> + Send;

    /// Defines an accounting period, or confirms one already defined.
    ///
    /// Periods live in the store because a sealed one has to stay sealed across
    /// a restart. A calendar held only in the caller's memory would come back
    /// open and start accepting postings into books that have been committed to.
    ///
    /// Re-defining an identical period is a no-op, so a caller may declare its
    /// calendar on every start-up. Re-defining the same identifier over a
    /// *different* range is an error: that moves the boundary of a period
    /// entries have already been booked into.
    fn define_period(
        &self,
        period: &Period,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Moves a period through its lifecycle.
    ///
    /// Permitted transitions are `Open → Closing`, `Closing → Sealed`, and
    /// `Closing → Open` to abandon a close that failed verification. Sealing is
    /// [`LedgerStore::seal_period`]'s job, not this one's.
    fn transition_period(
        &self,
        period: &PeriodId,
        to: PeriodState,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Every defined period with its persisted state, in start-date order.
    ///
    /// Feed this to [`PeriodCalendar::from_periods`](crate::PeriodCalendar::from_periods)
    /// to rebuild a calendar for
    /// local validation.
    fn periods(&self) -> impl Future<Output = Result<Vec<Period>, Self::Error>> + Send;

    /// Seals a period, committing to its entries and closing balances.
    ///
    /// The period must be in [`PeriodState::Closing`]. On success it advances to
    /// [`PeriodState::Sealed`] and the seal is appended to the chain.
    fn seal_period(
        &self,
        period: &PeriodId,
    ) -> impl Future<Output = Result<Seal, Self::Error>> + Send;

    /// Every seal recorded, oldest first.
    fn seals(&self) -> impl Future<Output = Result<Vec<Seal>, Self::Error>> + Send;

    /// Records that a set of postings offset one another.
    fn clear(&self, clearing: Clearing<P>) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Releases a clearing.
    fn reset_clearing(
        &self,
        id: ClearingId,
        on: Date,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Postings on an account with something still outstanding.
    fn open_items(
        &self,
        key: BalanceKey,
    ) -> impl Future<Output = Result<Vec<OpenItem<P>>, Self::Error>> + Send;

    /// Balances for several accounts at once.
    ///
    /// One query rather than one per account. A report over a customer's
    /// accounts, or over a whole subtree, is otherwise as many round trips as
    /// there are accounts — which at subledger scale is the difference between
    /// a report and an outage.
    ///
    /// Accounts with no postings are absent from the result rather than present
    /// with a zero: the caller knows which it asked for.
    fn balances(
        &self,
        accounts: &[AccountId],
        currency: Currency,
        layer: Layer,
        through: Option<LogIndex>,
    ) -> impl Future<Output = Result<BTreeMap<AccountId, Balance<P>>, Self::Error>> + Send;

    /// One account's movements, with the running balance after each.
    ///
    /// A balance says where an account ended up and nothing about how it got
    /// there. Paged, because an account statement over ten years is not a
    /// response body.
    fn statement(
        &self,
        key: BalanceKey,
        cursor: Cursor,
    ) -> impl Future<Output = Result<StatementPage<P>, Self::Error>> + Send;

    /// Records a checkpoint so later balance reads can start from it.
    ///
    /// A checkpoint is a cache for a definition — the fold over the journal — so
    /// it is only safe if it can be re-derived. It carries the log position and
    /// the tree head it was taken against for exactly that reason.
    fn save_checkpoint(
        &self,
        checkpoint: &Checkpoint<P>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// The most recent checkpoint for a key, if one was ever taken.
    fn load_checkpoint(
        &self,
        key: BalanceKey,
    ) -> impl Future<Output = Result<Option<Checkpoint<P>>, Self::Error>> + Send;
}

/// One line of an account statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementLine<const P: u8> {
    /// Where the entry sits in the log.
    pub index: LogIndex,
    /// Which posting produced this line.
    pub posting: crate::clearing::PostingRef,
    /// The entry's booking date.
    pub booking_date: Date,
    /// Which side the movement fell on.
    pub direction: crate::posting::Direction,
    /// The movement.
    pub amount: crate::money::Amount<P>,
    /// The account's balance after this line.
    pub running: Balance<P>,
    /// The owning entry's caller-defined kind, if any (e.g. an invoice or payment
    /// type). Lets a statement group or filter by document type without a second
    /// lookup per line. Opaque to the engine.
    pub kind: Option<crate::dimensions::Label>,
}

/// A page of statement lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementPage<const P: u8> {
    /// The lines, in log order.
    pub lines: Vec<StatementLine<P>>,
    /// Cursor for the next page, or `None` at the end.
    pub next: Option<Cursor>,
}

/// A boxed future returned by [`DynLedgerStore`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe counterpart to [`LedgerStore`].
///
/// `async fn` in traits is not `dyn`-compatible, because `impl Future` is a
/// distinct type per implementation and dispatch needs one erased type. Boxing
/// each future restores object safety at the cost of one allocation per call —
/// worth it when the backend is chosen from configuration, and avoidable
/// entirely when it is not.
///
/// A blanket implementation covers every [`LedgerStore`], so any backend can be
/// used either way without extra code.
pub trait DynLedgerStore<const P: u8>: Send + Sync {
    /// The backend's failure type, erased.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Appends a batch atomically.
    fn append_boxed<'a>(
        &'a self,
        batch: &'a EntryBatch<P>,
    ) -> BoxFuture<'a, Result<Vec<Recorded>, Self::Error>>;

    /// Fetches one entry by identifier.
    fn get_boxed(&self, id: EntryId) -> BoxFuture<'_, Result<Option<StoredEntry<P>>, Self::Error>>;

    /// Reads a page of the log.
    fn page_boxed(&self, cursor: Cursor) -> BoxFuture<'_, Result<Page<P>, Self::Error>>;

    /// The current tree head.
    fn head_boxed(&self) -> BoxFuture<'_, Result<TreeHead, Self::Error>>;

    /// One account balance, optionally as of a log position.
    fn balance_boxed(
        &self,
        key: BalanceKey,
        through: Option<LogIndex>,
    ) -> BoxFuture<'_, Result<Balance<P>, Self::Error>>;
}

impl<const P: u8, S> DynLedgerStore<P> for S
where
    S: LedgerStore<P>,
{
    type Error = S::Error;

    fn append_boxed<'a>(
        &'a self,
        batch: &'a EntryBatch<P>,
    ) -> BoxFuture<'a, Result<Vec<Recorded>, Self::Error>> {
        Box::pin(self.append(batch))
    }

    fn get_boxed(&self, id: EntryId) -> BoxFuture<'_, Result<Option<StoredEntry<P>>, Self::Error>> {
        Box::pin(self.get(id))
    }

    fn page_boxed(&self, cursor: Cursor) -> BoxFuture<'_, Result<Page<P>, Self::Error>> {
        Box::pin(self.page(cursor))
    }

    fn head_boxed(&self) -> BoxFuture<'_, Result<TreeHead, Self::Error>> {
        Box::pin(self.head())
    }

    fn balance_boxed(
        &self,
        key: BalanceKey,
        through: Option<LogIndex>,
    ) -> BoxFuture<'_, Result<Balance<P>, Self::Error>> {
        Box::pin(self.balance(key, through))
    }
}

/// Failure from the in-memory backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryStoreError {
    /// The journal refused the operation.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// An account binding could not be restored.
    #[error(transparent)]
    Account(#[from] crate::account::AccountError),
    /// The calendar refused a period operation.
    #[error(transparent)]
    Period(#[from] crate::period::PeriodError),
    /// A proof could not be built.
    #[error(transparent)]
    Proof(#[from] crate::merkle::ProofError),
    /// Arithmetic overflowed.
    #[error(transparent)]
    Money(#[from] crate::money::MoneyError),
}

/// An in-memory [`LedgerStore`].
///
/// Backed by a [`Journal`], so it inherits the engine's semantics exactly. It is
/// the reference a durable backend is expected to agree with, and the substrate
/// for tests that need a ledger but not a database.
#[derive(Debug)]
pub struct MemoryStore<const P: u8> {
    ledger: LedgerId,
    inner: Mutex<Journal<P>>,
    checkpoints: Mutex<BTreeMap<BalanceKey, Checkpoint<P>>>,
}

impl<const P: u8> MemoryStore<P> {
    /// Creates an empty store for one ledger.
    #[must_use]
    pub fn new(ledger: LedgerId) -> Self {
        Self {
            inner: Mutex::new(Journal::new(ledger.clone())),
            ledger,
            checkpoints: Mutex::new(BTreeMap::new()),
        }
    }

    /// Wraps an existing journal, accounts, periods and all.
    #[must_use]
    pub fn from_journal(journal: Journal<P>) -> Self {
        Self {
            ledger: journal.ledger().clone(),
            inner: Mutex::new(journal),
            checkpoints: Mutex::new(BTreeMap::new()),
        }
    }

    /// Runs `f` against the journal.
    ///
    /// A poisoned lock is recovered rather than propagated: the journal is
    /// append-only, so a panic elsewhere cannot have left it half-written.
    fn with<R>(&self, f: impl FnOnce(&Journal<P>) -> R) -> R {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    /// Runs `f` against the journal mutably.
    fn with_mut<R>(&self, f: impl FnOnce(&mut Journal<P>) -> R) -> R {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// A snapshot of the underlying journal.
    #[must_use]
    pub fn snapshot(&self) -> Journal<P> {
        self.with(Clone::clone)
    }
}

impl<const P: u8> LedgerStore<P> for MemoryStore<P> {
    type Error = MemoryStoreError;

    fn ledger(&self) -> &LedgerId {
        &self.ledger
    }

    fn register_account(
        &self,
        record: &AccountRecord,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let record = record.clone();
        let result = self.with_mut(|journal| {
            journal.accounts_mut().restore(record)?;
            Ok(())
        });
        async move { result }
    }

    fn accounts(&self) -> impl Future<Output = Result<Vec<AccountRecord>, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.account_records()));
        async move { result }
    }

    fn append(
        &self,
        batch: &EntryBatch<P>,
    ) -> impl Future<Output = Result<Vec<Recorded>, Self::Error>> + Send {
        // The journal applies the batch and undoes it exactly if any entry is
        // refused. A durable backend gets the same guarantee from its
        // transaction instead.
        let result =
            self.with_mut(|journal| Ok(journal.record_batch(batch.entries().iter().cloned())?));
        async move { result }
    }

    fn get(
        &self,
        id: EntryId,
    ) -> impl Future<Output = Result<Option<StoredEntry<P>>, Self::Error>> + Send {
        let result = self.with(|journal| {
            let found = journal.index_of(id).and_then(|index| {
                journal.at(index).map(|entry| StoredEntry {
                    index: Some(index),
                    entry: entry.clone(),
                    content_hash: entry.content_hash(),
                })
            });
            Ok(found)
        });
        async move { result }
    }

    fn page(&self, cursor: Cursor) -> impl Future<Output = Result<Page<P>, Self::Error>> + Send {
        let result = self.with(|journal| {
            let start = cursor.after.map_or(0usize, |i| {
                usize::try_from(i.get().saturating_add(1)).unwrap_or(usize::MAX)
            });
            let limit = cursor.effective_limit();
            let mut records = Vec::new();
            for (offset, entry) in journal.entries().iter().skip(start).take(limit).enumerate() {
                let index = start.saturating_add(offset);
                records.push(StoredEntry {
                    index: Some(LogIndex::new(index as u64)),
                    entry: entry.clone(),
                    content_hash: entry.content_hash(),
                });
            }
            let next = records
                .last()
                .filter(|_| start.saturating_add(records.len()) < journal.len())
                .and_then(|r| r.index)
                .map(|index| Cursor {
                    after: Some(index),
                    limit: cursor.limit,
                });
            Ok(Page { records, next })
        });
        async move { result }
    }

    fn head(&self) -> impl Future<Output = Result<TreeHead, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.head()));
        async move { result }
    }

    fn len(&self) -> impl Future<Output = Result<u64, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.len() as u64));
        async move { result }
    }

    fn balance(
        &self,
        key: BalanceKey,
        through: Option<LogIndex>,
    ) -> impl Future<Output = Result<Balance<P>, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.balance(&key, through)?));
        async move { result }
    }

    fn trial_balance(
        &self,
        through: Option<LogIndex>,
    ) -> impl Future<Output = Result<TrialBalance<P>, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.trial_balance(through)?));
        async move { result }
    }

    fn prove_inclusion(
        &self,
        index: LogIndex,
    ) -> impl Future<Output = Result<InclusionProof, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.prove_inclusion(index)?));
        async move { result }
    }

    fn prove_consistency(
        &self,
        old_size: u64,
    ) -> impl Future<Output = Result<ConsistencyProof, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.prove_consistency(old_size)?));
        async move { result }
    }

    fn define_period(
        &self,
        period: &Period,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let period = period.clone();
        let result = self.with_mut(|journal| {
            journal.calendar_mut().ensure(period)?;
            Ok(())
        });
        async move { result }
    }

    fn transition_period(
        &self,
        period: &PeriodId,
        to: PeriodState,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result = self.with_mut(|journal| Ok(journal.transition_period(period, to)?));
        async move { result }
    }

    fn periods(&self) -> impl Future<Output = Result<Vec<Period>, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.calendar().iter().cloned().collect()));
        async move { result }
    }

    fn seal_period(
        &self,
        period: &PeriodId,
    ) -> impl Future<Output = Result<Seal, Self::Error>> + Send {
        let result = self.with_mut(|journal| Ok(journal.seal_period(period)?));
        async move { result }
    }

    fn seals(&self) -> impl Future<Output = Result<Vec<Seal>, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.seals().seals().to_vec()));
        async move { result }
    }

    fn clear(&self, clearing: Clearing<P>) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result = self.with_mut(|journal| Ok(journal.clear(clearing)?));
        async move { result }
    }

    fn reset_clearing(
        &self,
        id: ClearingId,
        on: Date,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result = self.with_mut(|journal| Ok(journal.reset_clearing(id, on)?));
        async move { result }
    }

    fn open_items(
        &self,
        key: BalanceKey,
    ) -> impl Future<Output = Result<Vec<OpenItem<P>>, Self::Error>> + Send {
        let result = self.with(|journal| Ok(journal.open_items(&key)?));
        async move { result }
    }

    fn balances(
        &self,
        accounts: &[AccountId],
        currency: Currency,
        layer: Layer,
        through: Option<LogIndex>,
    ) -> impl Future<Output = Result<BTreeMap<AccountId, Balance<P>>, Self::Error>> + Send {
        let wanted: Vec<AccountId> = accounts.to_vec();
        let result = self.with(|journal| {
            let tb = journal.trial_balance(through)?;
            let mut out = BTreeMap::new();
            for account in wanted {
                let key = BalanceKey {
                    account,
                    currency,
                    layer,
                };
                if let Some(balance) = tb.get(&key) {
                    out.insert(account, *balance);
                }
            }
            Ok(out)
        });
        async move { result }
    }

    fn statement(
        &self,
        key: BalanceKey,
        cursor: Cursor,
    ) -> impl Future<Output = Result<StatementPage<P>, Self::Error>> + Send {
        let result = self.with(|journal| {
            let all = journal.statement(&key)?;
            let start = cursor.after.map_or(0usize, |after| {
                all.iter()
                    .position(|line| line.index.get() > after.get())
                    .unwrap_or(all.len())
            });
            let limit = cursor.effective_limit();
            let lines: Vec<StatementLine<P>> = all
                .iter()
                .skip(start)
                .take(limit)
                .map(|l| StatementLine {
                    index: l.index,
                    posting: l.posting,
                    booking_date: l.booking_date,
                    direction: l.direction,
                    amount: l.amount,
                    running: l.running,
                    kind: l.kind.clone(),
                })
                .collect();
            let next = lines
                .last()
                .filter(|_| start.saturating_add(lines.len()) < all.len())
                .map(|l| Cursor {
                    after: Some(l.index),
                    limit: cursor.limit,
                });
            Ok(StatementPage { lines, next })
        });
        async move { result }
    }

    fn save_checkpoint(
        &self,
        checkpoint: &Checkpoint<P>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let checkpoint = *checkpoint;
        let mut guard = self.checkpoints.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(checkpoint.key, checkpoint);
        async move { Ok(()) }
    }

    fn load_checkpoint(
        &self,
        key: BalanceKey,
    ) -> impl Future<Output = Result<Option<Checkpoint<P>>, Self::Error>> + Send {
        let guard = self.checkpoints.lock().unwrap_or_else(|e| e.into_inner());
        let found = guard.get(&key).copied();
        async move { Ok(found) }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::entry::Draft;
    use crate::storage::conformance::test_ledger;

    #[test]
    fn a_batch_needs_at_least_one_entry() {
        let empty: Vec<Entry<Balanced, 2>> = Vec::new();
        assert_eq!(EntryBatch::new(empty), Err(BatchError::Empty));
    }

    #[test]
    fn cursor_limits_are_clamped() {
        assert_eq!(Cursor::start().effective_limit(), DEFAULT_PAGE_SIZE);
        assert_eq!(Cursor::start().with_limit(0).effective_limit(), 1);
        assert_eq!(
            Cursor::start().with_limit(usize::MAX).effective_limit(),
            MAX_PAGE_SIZE
        );
    }

    #[test]
    fn the_memory_store_is_usable_behind_dyn() {
        // The point of the boxed adapter: a backend picked at run time.
        let store: Box<dyn DynLedgerStore<2, Error = MemoryStoreError>> =
            Box::new(MemoryStore::<2>::new(test_ledger()));
        let head = conformance::block_on(store.head_boxed()).expect("reads");
        assert_eq!(head.size, 0);
    }

    #[test]
    fn drafts_are_not_storable() {
        // Compile-time note: `EntryBatch` takes `Entry<Balanced, P>`, so a draft
        // cannot reach a store without passing validation first.
        fn _accepts_only_balanced<const P: u8>(_: Vec<Entry<Balanced, P>>) {}
        let _ = std::marker::PhantomData::<Entry<Draft, 2>>;
    }
}
