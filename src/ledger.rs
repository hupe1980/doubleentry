//! [`Ledger`]: a ledger kept in a [`Store`], written asynchronously.
//!
//! ```
//! use doubleentry::store::MemoryStore;
//! use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
//!
//! # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
//! let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
//! let books = books.as_actor("setup");
//! books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
//! books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
//! books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
//! books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31")).await?;
//!
//! let rec = books
//!     .record(
//!         Entry::new("invoice-2026-0001", date!("2026-03-15"))
//!             .debit("Assets:Cash", amount!("1190.00"), EUR)
//!             .credit("Income:Sales", amount!("1190.00"), EUR),
//!     )
//!     .await?;
//! let proof = books.prove_entry(rec.id).await?; // verifies against books.head()
//! assert_eq!(proof.index, rec.position);
//! books.audit().await?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;

use time::Date;

mod export;

pub use export::ExportPages;

use crate::core::{
    self, AccountSpec, AuditReport, ChangeInput, Done, HoldState, MemoPlain, Op, Recorded,
    Replayer, SealReceipt,
};
use crate::error::{ConfigError, Error, IntegrityError, IntegrityKind, QueryError};
use crate::export::{ExportScope, Exporter};
use crate::facade::{Account, Reads, SaltSource, seal_first, within};
use crate::journal::Journal;
use crate::log::{ConsistencyProof, InclusionProof, SealedBalanceProof, TreeHead};
use crate::model::{
    AccountHandle, AccountKind, AccountRef, Amount, Balance, BalanceLimit, Currency, Entry,
    EntryId, EntryRecord, Fingerprint, Genesis, Layer, LedgerId, LedgerPolicy, MemoSalt,
    PostingRef, ValueError,
};
use crate::report::{
    AccountName, AssertionOutcome, BalanceAssertion, BalanceQuery, OpenItem, Rollup, Statement,
    TrialBalance,
};
use crate::store::{
    Cell, Erasure, ProjectionPart, ProjectionRead, ProjectionRow, Store, StoreErrorKind, each_row,
    head_at,
};
#[cfg(feature = "witness")]
use crate::transparency::{Checkpoint, KeyName, LogKey};
use crate::transparency::{ProofBundle, SignedCheckpoint, TileId};
use crate::{HoldCursor, Page, PageRead};

/// A ledger kept in a [`Store`] and written asynchronously: the rule core
/// run by the store inside its write transaction, under the ledger's lock.
///
/// The same methods as [`Journal`](crate::Journal), `async` and on `&self`:
/// a `Ledger` is a cheap clone sharing its store, so it can be handed to
/// every task of a service. Each clone has its own actor
/// ([`as_actor`](Self::as_actor)) and salt source.
///
/// [`Ledger::new`] creates the ledger when the store holds none and opens it
/// otherwise; a store holding another ledger, or this one at another scale,
/// is refused as [`Error::Config`]. A write that fails with
/// [`Error::may_have_applied`] is resolved by recording the same draft
/// again: a committed entry comes back with `replayed == true`.
///
/// ```
/// use doubleentry::prelude::*;
/// use doubleentry::store::MemoryStore;
///
/// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
/// let store = MemoryStore::new(); // or SqlStore::sqlite(url) / SqlStore::postgres(url)
/// let books = Ledger::new(store.clone(), LedgerId::new("example.com/acme")?).await?;
/// books.declare_currency(EUR, 2, date!("2026-01-01")).await?; // a repeat is a no-op
/// books.define_months(2026, 3..=3).await?;
/// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
/// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
///
/// // Clones share the store: hand one to every task, each with its actor.
/// let clerk = books.clone().as_actor("clerk");
/// let sale = Entry::new("sale-1", date!("2026-03-15"))
///     .debit("Assets:Cash", amount!("10.00"), EUR)
///     .credit("Income:Sales", amount!("10.00"), EUR);
/// let first = clerk.record(sale.clone()).await?;
/// let retry = books.record(sale).await?; // a redelivered message
/// assert!(retry.replayed && retry.id == first.id);
///
/// // The same store opened again is the same ledger.
/// let reopened = Ledger::new(store, LedgerId::new("example.com/acme")?).await?;
/// assert_eq!(reopened.head().await?, books.head().await?);
/// # Ok::<(), Error>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Ledger<S, const P: u8 = 2> {
    store: S,
    ledger: LedgerId,
    actor: String,
    salt: SaltSource,
}

impl<S, const P: u8> fmt::Debug for Ledger<S, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ledger")
            .field("ledger", &self.ledger)
            .field("actor", &self.actor)
            .finish_non_exhaustive()
    }
}

impl<S: Store<2>> Ledger<S, 2> {
    /// The ledger `id` with two decimal places (`P = 2`) in `store`: created
    /// when the store holds no ledger, opened otherwise.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the store holds another ledger or scale (or
    /// another schema); [`Error::Backend`] when it fails.
    pub async fn new(store: S, id: LedgerId) -> Result<Self, Error> {
        Self::with_precision(store, id).await
    }
}

impl<S: Store<P>, const P: u8> Ledger<S, P> {
    /// The compile-time scale check.
    const SCALE_OK: () = assert!(P <= Amount::<P>::MAX_PRECISION, "P must be <= 18");

    /// The ledger `id` of scale `P` (at most 18 decimal places) in `store`:
    /// created when the store holds no ledger, opened otherwise.
    ///
    /// # Errors
    ///
    /// As [`Ledger::new`].
    pub async fn with_precision(store: S, id: LedgerId) -> Result<Self, Error> {
        let () = Self::SCALE_OK;
        let info = match store.open().await {
            Ok(info) => info,
            Err(error) if *error.kind() == StoreErrorKind::NotCreated => {
                store.create(&Genesis { ledger: id.clone() }).await?
            }
            Err(error) => return Err(error.into()),
        };
        if info.ledger != id || info.precision != P {
            return Err(ConfigError::ledgers((&id, P), (&info.ledger, info.precision)).into());
        }
        Ok(Self {
            store,
            ledger: id,
            actor: String::from("system"),
            salt: SaltSource::Os,
        })
    }

    /// The same ledger, drawing memo salts from `source` instead of the
    /// operating system — for deterministic tests and simulations. A salt
    /// must be unpredictable for memo erasure to protect anything.
    #[must_use]
    pub fn with_salt_source(self, source: impl Fn() -> MemoSalt + Send + Sync + 'static) -> Self {
        Self {
            salt: SaltSource::custom(source),
            ..self
        }
    }

    /// The same ledger, recording `actor` on master data from now on. An
    /// invalid actor is refused by the next master-data write.
    #[must_use]
    pub fn as_actor(self, actor: impl AsRef<str>) -> Self {
        Self {
            actor: actor.as_ref().to_owned(),
            ..self
        }
    }

    /// The actor recorded on master data.
    #[must_use]
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// The ledger's id.
    #[must_use]
    pub const fn id(&self) -> &LedgerId {
        &self.ledger
    }

    /// Every read, over the store.
    fn reads(&self) -> Reads<'_, S, P> {
        Reads::new(self.store.clone(), &self.ledger)
    }

    /// Runs one operation through the store's write pipeline.
    pub(crate) async fn run(&self, op: Op<P>) -> Result<Done<P>, Error> {
        Ok(self.store.write(&op).await??)
    }

    // ── Master data and calendar ────────────────────────────────────────────

    /// Declares a currency with its number of decimal places (at most `P`),
    /// or raises it.
    ///
    /// # Errors
    ///
    /// As [`Journal::declare_currency`](crate::Journal::declare_currency),
    /// and [`Error::Backend`].
    pub async fn declare_currency(
        &self,
        code: Currency,
        exponent: u8,
        effective: Date,
    ) -> Result<(), Error> {
        self.run(Op::Currency {
            code,
            exponent,
            effective,
            actor: self.actor.clone(),
        })
        .await
        .map(drop)
    }

    /// Opens an account at `path` of `kind` from `on`; opening it again
    /// returns the same handle and writes nothing.
    ///
    /// # Errors
    ///
    /// As [`Journal::open`](crate::Journal::open), and [`Error::Backend`].
    pub async fn open(
        &self,
        path: impl AsRef<str>,
        kind: AccountKind,
        on: Date,
    ) -> Result<AccountHandle, Error> {
        self.register(AccountSpec::new(path, kind, on)).await
    }

    /// Registers an account with its full master data.
    ///
    /// # Errors
    ///
    /// As [`Journal::register`](crate::Journal::register), and
    /// [`Error::Backend`].
    pub async fn register(&self, spec: AccountSpec) -> Result<AccountHandle, Error> {
        self.run(Op::Register {
            spec,
            actor: self.actor.clone(),
        })
        .await?
        .into_account()
    }

    async fn change<A>(&self, account: A, change: ChangeInput, effective: Date) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        let account = core::account_ref(account)?;
        self.run(Op::Change {
            account,
            change,
            effective,
            actor: self.actor.clone(),
        })
        .await
        .map(drop)
    }

    /// Sets an account's balance limit from `effective` on.
    ///
    /// # Errors
    ///
    /// As [`Journal::set_limit`](crate::Journal::set_limit), and
    /// [`Error::Backend`].
    pub async fn set_limit<A>(
        &self,
        account: A,
        limit: BalanceLimit,
        effective: Date,
    ) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(account, ChangeInput::Limit(limit), effective)
            .await
    }

    /// Sets (or removes) an account's class; refused once it has postings.
    ///
    /// # Errors
    ///
    /// As [`Journal::set_class`](crate::Journal::set_class), and
    /// [`Error::Backend`].
    pub async fn set_class<A>(
        &self,
        account: A,
        class: Option<&str>,
        effective: Date,
    ) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        let class = ChangeInput::Class(class.map(str::to_owned));
        self.change(account, class, effective).await
    }

    /// Closes an account: `last_day` is the last day it can be posted to.
    ///
    /// # Errors
    ///
    /// As [`Journal::close_account`](crate::Journal::close_account), and
    /// [`Error::Backend`].
    pub async fn close_account<A>(&self, account: A, last_day: Date) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(account, ChangeInput::OpenTo(Some(last_day)), last_day)
            .await
    }

    /// Reopens a closed account from `effective` on.
    ///
    /// # Errors
    ///
    /// As [`Journal::reopen_account`](crate::Journal::reopen_account), and
    /// [`Error::Backend`].
    pub async fn reopen_account<A>(&self, account: A, effective: Date) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(account, ChangeInput::OpenTo(None), effective)
            .await
    }

    /// Sets the ledger policy from `effective` on.
    ///
    /// # Errors
    ///
    /// As [`Journal::set_policy`](crate::Journal::set_policy), and
    /// [`Error::Backend`].
    pub async fn set_policy(&self, policy: LedgerPolicy, effective: Date) -> Result<(), Error> {
        self.run(Op::Policy {
            policy,
            effective,
            actor: self.actor.clone(),
        })
        .await
        .map(drop)
    }

    /// Defines the next period of the calendar.
    ///
    /// # Errors
    ///
    /// As [`Journal::define_period`](crate::Journal::define_period), and
    /// [`Error::Backend`].
    pub async fn define_period(
        &self,
        id: impl AsRef<str>,
        start: Date,
        end: Date,
    ) -> Result<(), Error> {
        self.run(Op::Period {
            id: id.as_ref().to_owned(),
            start,
            end,
        })
        .await
        .map(drop)
    }

    /// Defines one period per month of `year`, ids `YYYY-MM`; months already
    /// defined identically are skipped, so a retry completes the calendar.
    ///
    /// # Errors
    ///
    /// As [`Journal::define_months`](crate::Journal::define_months), and
    /// [`Error::Backend`].
    pub async fn define_months(&self, year: i32, months: RangeInclusive<u8>) -> Result<(), Error> {
        for (id, start, end) in core::months(year, months)? {
            self.define_period(id, start, end).await?;
        }
        Ok(())
    }

    /// Seals a period; sealing a sealed period returns its receipt and
    /// writes nothing.
    ///
    /// # Errors
    ///
    /// As [`Journal::seal_period`](crate::Journal::seal_period), and
    /// [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "seal_period", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, period = %crate::trace::key(id.as_ref()), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub async fn seal_period(&self, id: impl AsRef<str>) -> Result<SealReceipt<P>, Error> {
        let result = self
            .run(Op::Seal {
                period: id.as_ref().to_owned(),
            })
            .await
            .and_then(Done::into_sealed);
        #[cfg(feature = "tracing")]
        crate::trace::sealed(result.as_ref());
        result
    }

    /// Seals every period up to and including `id` that has no seal yet,
    /// earliest first, and returns the receipts of the periods it sealed,
    /// `id`'s last.
    ///
    /// # Errors
    ///
    /// As [`Journal::seal_through`](crate::Journal::seal_through), and
    /// [`Error::Backend`].
    pub async fn seal_through(&self, id: impl AsRef<str>) -> Result<Vec<SealReceipt<P>>, Error> {
        let mut sealed = Vec::new();
        loop {
            match self.seal_period(id.as_ref()).await {
                Ok(receipt) => {
                    sealed.push(receipt);
                    return Ok(sealed);
                }
                Err(error) => match seal_first(&error) {
                    Some(earliest) => sealed.push(self.seal_period(earliest).await?),
                    None => return Err(error),
                },
            }
        }
    }

    /// The year-end closing drafts through `on` into `equity`, to be
    /// recorded in order with [`record`](Self::record).
    ///
    /// # Errors
    ///
    /// As [`Journal::close_year`](crate::Journal::close_year), and
    /// [`Error::Backend`].
    pub async fn close_year<A>(
        &self,
        key: &str,
        on: Date,
        equity: A,
    ) -> Result<Vec<Entry<P>>, Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        let op = Op::CloseYear {
            key: key.to_owned(),
            on,
            equity: core::account_ref(equity)?,
        };
        self.run(op).await?.into_closing()
    }

    // ── Bookings ────────────────────────────────────────────────────────────

    /// Records an entry; a retry of a recorded draft returns the original
    /// receipt with `replayed == true` and writes nothing.
    ///
    /// # Errors
    ///
    /// As [`Journal::record`](crate::Journal::record), and
    /// [`Error::Backend`] — when it [may have applied](Error::may_have_applied),
    /// record the same draft again.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, key = %crate::trace::draft_key(&entry), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub async fn record(&self, entry: Entry<P>) -> Result<Recorded, Error> {
        let result = self.record_entry(entry).await;
        #[cfg(feature = "tracing")]
        crate::trace::recorded(result.as_ref());
        result
    }

    async fn record_entry(&self, entry: Entry<P>) -> Result<Recorded, Error> {
        let entry = self.salt.salted(entry)?;

        self.run(Op::Record(Box::new(entry))).await?.into_recorded()
    }

    /// Records `drafts` atomically, in order, in one transaction under the
    /// ledger's lock: each draft is checked against the state the drafts
    /// before it leave, and either every draft is recorded or none is.
    /// Recording an identical batch again returns the original receipts with
    /// `replayed == true`. The same batch gives the same records on every
    /// store and on [`Journal::record_batch`](crate::Journal::record_batch).
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/shop")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// let sales: Vec<Entry> = (1..=100)
    ///     .map(|n| {
    ///         Entry::new(format!("sale-{n}"), date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("1.00"), EUR)
    ///             .credit("Income:Sales", amount!("1.00"), EUR)
    ///     })
    ///     .collect();
    /// let recorded = books.record_batch(sales).await?; // one transaction, one lock
    /// assert_eq!(recorded.len(), 100);
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::record_batch`](crate::Journal::record_batch), and
    /// [`Error::Backend`] — when it [may have applied](Error::may_have_applied),
    /// record the same batch again.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record_batch", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, outcome = tracing::field::Empty, code = tracing::field::Empty, index = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub async fn record_batch(
        &self,
        drafts: impl IntoIterator<Item = Entry<P>>,
    ) -> Result<Vec<Recorded>, Error> {
        let result = match self.salt.salted_all(drafts) {
            Ok(drafts) => self.run(Op::Batch(drafts)).await.and_then(Done::into_batch),
            Err(error) => Err(error),
        };
        #[cfg(feature = "tracing")]
        crate::trace::batch(result.as_ref());
        result
    }

    /// The draft that reverses entry `id`: its postings mirrored, booked on
    /// `on` under `key`. It checks no rule; record it with
    /// [`record`](Self::record).
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry has the id; [`Error::Backend`].
    pub async fn reversal(&self, id: EntryId, key: &str, on: Date) -> Result<Entry<P>, Error> {
        self.reads().reversal(id, key, on).await
    }

    /// Clears `amount` of a debit posting against a credit posting of the
    /// same balance; returns the clearing record's position.
    ///
    /// # Errors
    ///
    /// As [`Journal::clear`](crate::Journal::clear), and [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, key = %crate::trace::key(key), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub async fn clear(
        &self,
        key: &str,
        debit: PostingRef,
        credit: PostingRef,
        amount: Amount<P>,
        on: Date,
    ) -> Result<u64, Error> {
        let result = self
            .clearing(Op::Clear {
                key: key.to_owned(),
                debit,
                credit,
                amount,
                date: on,
            })
            .await;
        #[cfg(feature = "tracing")]
        crate::trace::cleared(result.as_ref());
        result
    }

    /// Runs a clearing operation; its record's position.
    async fn clearing(&self, op: Op<P>) -> Result<u64, Error> {
        self.run(op).await?.into_cleared()
    }

    /// Resets the clearing applied at position `clearing`; returns the reset
    /// record's position.
    ///
    /// # Errors
    ///
    /// As [`Journal::reset_clearing`](crate::Journal::reset_clearing), and
    /// [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, key = %crate::trace::key(key), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub async fn reset_clearing(&self, key: &str, clearing: u64, on: Date) -> Result<u64, Error> {
        let result = self
            .clearing(Op::ResetClearing {
                key: key.to_owned(),
                clearing,
                date: on,
            })
            .await;
        #[cfg(feature = "tracing")]
        crate::trace::cleared(result.as_ref());
        result
    }

    /// Erases entry `id`'s memo text and salt and its draft fingerprint; no
    /// leaf, hash or proof changes.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry has the id; [`Error::Backend`].
    pub async fn erase_memo(&self, id: EntryId) -> Result<(), Error> {
        match self.store.erase_memo(id).await? {
            Erasure::UnknownEntry => Err(Error::Query(QueryError::UnknownEntry { id })),
            Erasure::Erased | Erasure::AlreadyErased | Erasure::NoMemo => Ok(()),
        }
    }

    // ── Lookups (absence is `None`, never an error) ─────────────────────────

    /// The entry with id `id`, as recorded.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// assert!(books.entry(sale.id).await?.is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] when its stored record does not decode;
    /// [`Error::Backend`].
    pub async fn entry(&self, id: EntryId) -> Result<Option<EntryRecord<P>>, Error> {
        self.reads().entry(id).await
    }

    /// The entry recorded under idempotency key `key`.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// assert_eq!(books.entry_by_key("sale-1").await?, books.entry(sale.id).await?);
    /// assert_eq!(books.entry_by_key("sale-2").await?, None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`entry`](Self::entry).
    pub async fn entry_by_key(
        &self,
        key: impl AsRef<str>,
    ) -> Result<Option<EntryRecord<P>>, Error> {
        self.reads().entry_by_key(key.as_ref().as_bytes()).await
    }

    /// The master data in force of an account, by path or handle.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// let cash = books.account("Assets:Cash").await?.expect("opened");
    /// assert_eq!(books.account(cash.handle).await?, Some(cash));
    /// assert_eq!(books.account("Assets:Elsewhere").await?, None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] when one of its stored records does not decode;
    /// [`Error::Backend`].
    pub async fn account<A>(&self, account: A) -> Result<Option<Account>, Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        match account.try_into() {
            Ok(account) => self.reads().account(account).await,
            Err(_) => Ok(None),
        }
    }

    /// The memo text of entry `id`; `None` without a memo, once erased, or
    /// for an unknown entry.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// assert_eq!(books.memo(sale.id).await?.as_deref(), Some("for the customer"));
    /// books.erase_memo(sale.id).await?;
    /// assert_eq!(books.memo(sale.id).await?, None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Backend`].
    pub async fn memo(&self, id: EntryId) -> Result<Option<String>, Error> {
        self.memo_text(id).await
    }

    /// The declared exponent of `currency` — the decimal places its amounts
    /// have and reports print — or `None` while undeclared.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] when a stored declaration does not decode;
    /// [`Error::Backend`].
    pub async fn exponent(&self, currency: Currency) -> Result<Option<u8>, Error> {
        let size = self.store.size().await?;
        Ok(self.reads().exponents(size).await?.get(&currency).copied())
    }

    async fn memo_text(&self, id: EntryId) -> Result<Option<String>, Error> {
        self.reads().memo(id).await
    }

    // ── Integrity and proofs ────────────────────────────────────────────────

    /// Replays the whole log through the rule core and compares everything
    /// the store keeps — every [`ProjectionPart`], draft fingerprints and
    /// stored tree nodes included — with the replay's projection, byte for
    /// byte. A ledger growing meanwhile is followed until the tables match
    /// one size.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] at the first difference; [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "audit", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, records = tracing::field::Empty, outcome = tracing::field::Empty, code = tracing::field::Empty))
    )]
    pub async fn audit(&self) -> Result<AuditReport, Error> {
        let result = self.replay().await.map(|(report, _)| report);
        #[cfg(feature = "tracing")]
        crate::trace::audited(result.as_ref());
        result
    }

    /// An in-memory [`Journal`] replica of this ledger as the store has it
    /// now, rebuilt and checked like [`audit`](Self::audit). It writes as
    /// this ledger's actor with its salt source; nothing it records reaches
    /// the store.
    ///
    /// ```
    /// use doubleentry::store::MemoryStore;
    /// use doubleentry::{AccountKind, Ledger, LedgerId, date};
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// let replica = books.to_journal().await?;
    /// assert_eq!(replica.head(), books.head().await?);
    /// assert!(replica.account("Assets:Cash").is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`audit`](Self::audit).
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "audit", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, records = tracing::field::Empty, outcome = tracing::field::Empty, code = tracing::field::Empty))
    )]
    pub async fn to_journal(&self) -> Result<Journal<P>, Error> {
        let result = self.replay().await;
        #[cfg(feature = "tracing")]
        crate::trace::audited(result.as_ref().map(|(report, _)| report));
        let (_, replay) = result?;
        Ok(Journal::from_replay(replay, &self.actor, self.salt.clone()))
    }

    /// The audit's replay: every leaf fed with its `memos` row, then every
    /// other table compared at one log size; when the ledger grew meanwhile,
    /// the new leaves are fed and the tables read again.
    async fn replay(&self) -> Result<(AuditReport, Replayer<P>), Error> {
        let mut replay = Replayer::<P>::new(&self.ledger);
        let mut side: BTreeMap<u64, ProjectionRow> = BTreeMap::new();
        let mut fed = 0u64;
        loop {
            let size = self.store.size().await?;
            self.side_data(fed, size, &mut side).await?;
            self.feed(&mut replay, &side, &mut fed, size).await?;
            if fed != size {
                return Err(IntegrityError::new(IntegrityKind::MissingRecord, Some(fed)).into());
            }
            if self.compare(&replay, &side, size).await? {
                return Ok((replay.report(), replay));
            }
        }
    }

    /// The `memos` rows of the entries at positions `from..size`, into
    /// `side`.
    async fn side_data(
        &self,
        from: u64,
        size: u64,
        side: &mut BTreeMap<u64, ProjectionRow>,
    ) -> Result<(), Error> {
        let mut page = PageRead::first(PageRead::<()>::MAX_LIMIT);
        if let Some(last) = from.checked_sub(1) {
            page = page.after(key_after(last));
        }
        loop {
            let read = self
                .store
                .projection(&ProjectionRead {
                    part: ProjectionPart::Memos,
                    page: page.clone(),
                })
                .await?;
            for row in read.items {
                let position = row_position(&row)?;
                if position >= size {
                    return Ok(());
                }
                side.insert(position, row);
            }
            match read.next {
                Some(next) => page = page.after(next),
                None => return Ok(()),
            }
        }
    }

    /// Feeds the leaves at positions `fed..size` (the store's `leaves` rows)
    /// with their side data.
    async fn feed(
        &self,
        replay: &mut Replayer<P>,
        side: &BTreeMap<u64, ProjectionRow>,
        fed: &mut u64,
        size: u64,
    ) -> Result<(), Error> {
        while *fed < size {
            let mut page = PageRead::first(PageRead::<()>::MAX_LIMIT);
            if let Some(last) = fed.checked_sub(1) {
                page = page.after(key_after(last));
            }
            let read = self
                .store
                .projection(&ProjectionRead {
                    part: ProjectionPart::Leaves,
                    page,
                })
                .await?;
            if read.items.is_empty() {
                return Ok(());
            }
            for row in read.items {
                let position = row_position(&row)?;
                if position >= size {
                    return Ok(());
                }
                let (kind, bytes) = match row.cells() {
                    [_, Cell::Int(kind), Cell::Bytes(bytes)] => (*kind, bytes),
                    _ => return Err(undecodable(position)),
                };
                if bytes.first().map(|first| i64::from(*first)) != Some(kind) {
                    return Err(undecodable(position));
                }
                let (memo, fingerprint) =
                    side.get(&position).map_or(Ok((None, None)), side_cells)?;
                replay.feed(core::ReplayLeaf {
                    position,
                    bytes,
                    memo: memo.as_ref(),
                    fingerprint,
                })?;
                *fed = position.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Compares every table but the leaves with the replay at `size`:
    /// `Ok(false)` when the ledger grew meanwhile.
    async fn compare(
        &self,
        replay: &Replayer<P>,
        side: &BTreeMap<u64, ProjectionRow>,
        size: u64,
    ) -> Result<bool, Error> {
        if head_at(&self.store, size).await? != replay.head() {
            return Err(core::AuditFailure {
                position: None,
                kind: core::AuditFailureKind::HeadMismatch,
            }
            .into());
        }
        for part in ProjectionPart::ALL {
            let mut check = replay.check_part(part);
            match part {
                // Fed to the replay: every row, in order, is a leaf it took.
                ProjectionPart::Leaves => continue,
                // Read once, before the leaves they belong to were fed.
                ProjectionPart::Memos => {
                    for row in side.values() {
                        check.feed(row.clone())?;
                    }
                }
                _ => {
                    let complete = each_row(&self.store, part, size, |row| {
                        check.feed(row).map_err(Error::from)
                    })
                    .await?;
                    if !complete {
                        return Ok(false);
                    }
                }
            }
            check.finish()?;
        }
        Ok(true)
    }

    /// The store this ledger is kept in.
    #[cfg(feature = "testing")]
    pub(crate) const fn store(&self) -> &S {
        &self.store
    }

    /// The tree head: the log's size and root.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] when the stored nodes do not assemble;
    /// [`Error::Backend`].
    pub async fn head(&self) -> Result<TreeHead, Error> {
        self.reads().head().await
    }

    /// The tree head the log had at `size` records: what a proof at that
    /// size verifies against.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// let then = books.head().await?;
    /// books.define_months(2026, 4..=4).await?;
    /// assert_eq!(books.head_at(then.size).await?, then);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for a size beyond the head; [`Error::Integrity`]
    /// when the stored nodes do not assemble; [`Error::Backend`].
    pub async fn head_at(&self, size: u64) -> Result<TreeHead, Error> {
        self.reads().head_at(size).await
    }

    /// The proof that entry `id` is in the log, against the head of the
    /// size the store has when it is built.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry has the id; [`Error::Integrity`] when
    /// the stored nodes do not assemble; [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "entry", size = tracing::field::Empty))
    )]
    pub async fn prove_entry(&self, id: EntryId) -> Result<InclusionProof, Error> {
        self.reads().inclusion(id, None).await
    }

    /// The proof that entry `id` is in the log as it was at `size` records,
    /// against [`head_at(size)`](Self::head_at).
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// let then = books.head().await?;
    /// books.define_months(2026, 4..=4).await?;
    /// let proof = books.prove_entry_at(sale.id, then.size).await?;
    /// assert_eq!((proof.index, proof.size), (sale.position, then.size));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry had the id at that size, or for a size
    /// beyond the head; [`Error::Integrity`] when the stored nodes do not
    /// assemble; [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "entry", size = tracing::field::Empty))
    )]
    pub async fn prove_entry_at(&self, id: EntryId, size: u64) -> Result<InclusionProof, Error> {
        self.reads().inclusion(id, Some(size)).await
    }

    /// The proof that the log at `new_size` records extends the log at
    /// `old_size`: nothing recorded by `old_size` was changed or removed.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("10.00"), EUR)
    ///             .credit("Income:Sales", amount!("10.00"), EUR)
    ///             .memo("for the customer"),
    ///     )
    ///     .await?;
    /// let then = books.head().await?;
    /// books.define_months(2026, 4..=4).await?;
    /// let now = books.head().await?;
    /// books.prove_consistency(then.size, now.size).await?.verify(&then, &now)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for an old size of 0 or above the new one, or a new
    /// size beyond the head; [`Error::Integrity`] when the stored nodes do
    /// not assemble; [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "consistency", size = tracing::field::Empty))
    )]
    pub async fn prove_consistency(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, Error> {
        self.reads().consistency(old_size, new_size).await
    }

    // ── Holds ───────────────────────────────────────────────────────────────

    /// The hold opened by entry `id` and where it stands: open, or posted,
    /// voided or expired by the entry that resolved it. `None` when `id` is
    /// not a hold.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, amount, date, currency::EUR};
    /// use doubleentry::{HoldStatus, PostAmount};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/payments")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01")).await?;
    /// # books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 11..=11).await?;
    /// let hold = books
    ///     .record(
    ///         Entry::open_hold("auth-4711", date!("2026-11-20"))
    ///             .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
    ///             .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
    ///     )
    ///     .await?;
    /// let capture = books
    ///     .record(Entry::post_hold("capture-4711", date!("2026-11-22"), hold.id, PostAmount::Partial(amount!("60.00"))))
    ///     .await?;
    /// let state = books.hold(hold.id).await?.ok_or("no hold")?;
    /// assert_eq!(state.status, HoldStatus::Posted { by: capture.id, amount: amount!("60.00") });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] when the hold index disagrees with the opening or
    /// resolving entry's stored record; [`Error::Backend`].
    pub async fn hold(&self, id: EntryId) -> Result<Option<HoldState<P>>, Error> {
        self.reads().hold(id).await
    }

    /// The open holds that may be expired on `as_of` — their expiry date is
    /// on or before it — ordered by (expiry date, position), one page at a
    /// time: what a scheduler books an [`Entry::expire_hold`] for. Nothing
    /// expires by itself, and no clock is read. A limit outside `1..=1000`
    /// is clamped into it.
    ///
    /// ```
    /// # use doubleentry::store::MemoryStore;
    /// # use doubleentry::{AccountKind, Entry, Ledger, LedgerId, PageRead, amount, date, currency::EUR};
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// # let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/payments")?).await?;
    /// # books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// # books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01")).await?;
    /// # books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01")).await?;
    /// # books.define_months(2026, 11..=11).await?;
    /// let hold = books
    ///     .record(
    ///         Entry::open_hold("auth-4711", date!("2026-11-20"))
    ///             .expires(date!("2026-11-30"))
    ///             .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
    ///             .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
    ///     )
    ///     .await?;
    /// let mut page = PageRead::first(100);
    /// loop {
    ///     let due = books.holds_due(date!("2026-11-30"), page.clone()).await?;
    ///     for hold in &due.items {
    ///         let key = format!("expire-{}", hold.id);
    ///         books.record(Entry::expire_hold(key, date!("2026-11-30"), hold.id)).await?;
    ///     }
    ///     match due.next {
    ///         Some(next) => page = page.after(next),
    ///         None => break,
    ///     }
    /// }
    /// assert!(!books.hold(hold.id).await?.ok_or("no hold")?.is_open());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] when the hold index does not decode;
    /// [`Error::Backend`].
    pub async fn holds_due(
        &self,
        as_of: Date,
        page: PageRead<HoldCursor>,
    ) -> Result<Page<HoldState<P>, HoldCursor>, Error> {
        self.reads().holds_due(as_of, page).await
    }

    // ── Reports and export ──────────────────────────────────────────────────

    /// The balance of `account` in `currency`: the gross debit, credit and
    /// count of the postings `query` selects — read from the maintained
    /// balance, the balance after a position, or a fold of the account's
    /// posting rows, whichever answers.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Bank", amount!("100.00"), EUR)
    ///             .credit("Income:Sales", amount!("100.00"), EUR),
    ///     )
    ///     .await?;
    /// let balance = books.balance("Assets:Bank", EUR, &BalanceQuery::all()).await?;
    /// assert_eq!(balance.net(), amount!("100.00"));
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::balance`](crate::Journal::balance), and
    /// [`Error::Backend`].
    pub async fn balance<A>(
        &self,
        account: A,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Balance<P>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        self.reads()
            .balance(&AccountName::new(account), currency, query)
            .await
    }

    /// The trial balance: one row per (account, currency) with a posting
    /// `query` selects.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Bank", amount!("100.00"), EUR)
    ///             .credit("Income:Sales", amount!("100.00"), EUR),
    ///     )
    ///     .await?;
    /// let trial = books.trial_balance(&BalanceQuery::period("2026-03")).await?;
    /// assert!(trial.is_balanced());
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::trial_balance`](crate::Journal::trial_balance), and
    /// [`Error::Backend`].
    pub async fn trial_balance(&self, query: &BalanceQuery) -> Result<TrialBalance<P>, Error> {
        self.reads().trial_balance(query).await
    }

    /// The statement of `account` in `currency` over `query`'s window: the
    /// opening balance, every line in date order with a running balance,
    /// and the closing balance.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Bank", amount!("100.00"), EUR)
    ///             .credit("Income:Sales", amount!("100.00"), EUR),
    ///     )
    ///     .await?;
    /// let march = books.statement("Assets:Bank", EUR, &BalanceQuery::period("2026-03")).await?;
    /// assert_eq!(march.closing.net(), amount!("100.00"));
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::statement`](crate::Journal::statement), and
    /// [`Error::Backend`].
    pub async fn statement<A>(
        &self,
        account: A,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Statement<P>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        self.reads()
            .statement(&AccountName::new(account), currency, query)
            .await
    }

    /// The trial balance of `currency` folded up the account tree.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank:Main", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// books
    ///     .record(
    ///         Entry::new("sale-1", date!("2026-03-15"))
    ///             .debit("Assets:Bank:Main", amount!("100.00"), EUR)
    ///             .credit("Income:Sales", amount!("100.00"), EUR),
    ///     )
    ///     .await?;
    /// let sheet = books.rollup(EUR, &BalanceQuery::all()).await?;
    /// assert_eq!(sheet.nodes().len(), 5); // Assets, Assets:Bank, Assets:Bank:Main, Income, Income:Sales
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::rollup`](crate::Journal::rollup), and
    /// [`Error::Backend`].
    pub async fn rollup(
        &self,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Rollup<P>, Error> {
        self.reads().rollup(currency, query).await
    }

    /// The open items of `account` in `currency`: the postings `query`
    /// selects with an amount not offset by clearings, by position.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// books
    ///     .record(
    ///         Entry::new("invoice-1", date!("2026-03-01"))
    ///             .debit("Assets:Receivables", amount!("119.00"), EUR)
    ///             .credit("Income:Sales", amount!("119.00"), EUR),
    ///     )
    ///     .await?;
    /// let open = books.open_items("Assets:Receivables", EUR, &BalanceQuery::all()).await?;
    /// assert_eq!(open[0].residual, amount!("119.00"));
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::open_items`](crate::Journal::open_items), and
    /// [`Error::Backend`].
    pub async fn open_items<A>(
        &self,
        account: A,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Vec<OpenItem<P>>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        self.reads()
            .open_items(&AccountName::new(account), currency, query)
            .await
    }

    /// Checks one balance assertion.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    /// use doubleentry::{report::{AssertAt, BalanceAssertion, DateBasis}};
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    /// let at = AssertAt::EndOfDay { date: date!("2026-03-31"), basis: DateBasis::Value };
    /// let claim = BalanceAssertion::new("Assets:Bank", EUR, at, amount!("0.00"));
    /// assert!(books.check_assertion(&claim).await?.is_held());
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::check_assertion`](crate::Journal::check_assertion), and
    /// [`Error::Backend`].
    pub async fn check_assertion(
        &self,
        assertion: &BalanceAssertion<P>,
    ) -> Result<AssertionOutcome<P>, Error> {
        self.reads().check_assertion(assertion).await
    }

    /// Checks every assertion: one outcome each, in their order.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    /// use doubleentry::{report::{AssertAt, BalanceAssertion}};
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    /// let claims = [
    ///     BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(2), amount!("0.00")),
    ///     BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(2), amount!("1.00")),
    /// ];
    /// let outcomes = books.check_assertions(&claims).await?;
    /// assert!(outcomes[0].is_held() && !outcomes[1].is_held());
    /// # Ok::<(), Error>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// The first error of [`check_assertion`](Self::check_assertion).
    pub async fn check_assertions(
        &self,
        assertions: &[BalanceAssertion<P>],
    ) -> Result<Vec<AssertionOutcome<P>>, Error> {
        self.reads().check_assertions(assertions).await
    }

    /// The GoBD export view of `scope`, pinned to `head` (or to the head
    /// now), read page by page with
    /// [`next_page`](crate::export::ExportPages::next_page). See
    /// [`export`](crate::export) for the rows.
    ///
    /// ```
    /// use doubleentry::export::{ExportScope, verify};
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let books = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
    /// let head = books.head().await?;
    /// let mut export = books.export(ExportScope::All, Some(head)).await?;
    /// let mut rows = Vec::new();
    /// while let Some(page) = export.next_page(500).await? {
    ///     rows.extend(page);
    /// }
    /// verify(rows, &head)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::export`](crate::Journal::export), and
    /// [`Error::Backend`].
    pub async fn export(
        &self,
        scope: ExportScope,
        head: Option<TreeHead>,
    ) -> Result<ExportPages<'_, S, P>, Error> {
        let size = self.store.size().await?;
        let bound = match head {
            Some(head) => {
                within(head.size, size)?;
                self.reads().pinned(head).await?;
                head.size
            }
            None => size,
        };
        let exporter = Exporter::new(scope)?;
        if let Some(id) = exporter.period()
            && self.reads().period(id, bound).await?.is_none()
        {
            return Err(QueryError::UnknownPeriod {
                id: id.as_str().to_owned(),
            }
            .into());
        }
        Ok(ExportPages::new(self, exporter, bound))
    }
}

// ── Transparency ────────────────────────────────────────────────────────────

impl<S: Store<P>, const P: u8> Ledger<S, P> {
    /// The ledger's head as a C2SP checkpoint, signed by `key` (feature
    /// `witness`).
    ///
    /// ```
    /// use doubleentry::store::MemoryStore;
    /// use doubleentry::transparency::LogKey;
    /// use doubleentry::{Ledger, LedgerId};
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let ledger = LedgerId::new("example.com/ledgers/acme")?;
    /// let books = Ledger::new(MemoryStore::new(), ledger.clone()).await?;
    /// let key = LogKey::from_seed(&ledger, [7; 32]);
    /// let signed = books.checkpoint(&key).await?;
    /// assert_eq!(signed.checkpoint().head(), books.head().await?);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Transparency`] with
    /// [`KeyNameMismatch`](crate::transparency::Error::KeyNameMismatch) when
    /// the key is not this ledger's; [`Error::Integrity`];
    /// [`Error::Backend`].
    #[cfg(feature = "witness")]
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "checkpoint", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.ledger, size = tracing::field::Empty))
    )]
    pub async fn checkpoint(&self, key: &LogKey) -> Result<SignedCheckpoint, Error> {
        let head = self.head().await?;
        #[cfg(feature = "tracing")]
        crate::trace::proved(head.size);
        Ok(key.sign(&Checkpoint::new(KeyName::from(&self.ledger), head))?)
    }

    /// The proof of `account`'s balance in (`currency`, `layer`) as sealed
    /// for `period`, against the head: as
    /// [`Journal::prove_balance`](crate::Journal::prove_balance). Read from
    /// [`Store::seal_nodes`]; a store serving none rebuilds the tree from
    /// the period's sealed balances.
    ///
    /// # Errors
    ///
    /// As [`Journal::prove_balance`](crate::Journal::prove_balance), and
    /// [`Error::Integrity`] or [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "balance", size = tracing::field::Empty))
    )]
    pub async fn prove_balance<A>(
        &self,
        account: A,
        currency: Currency,
        layer: Layer,
        period: impl AsRef<str>,
    ) -> Result<SealedBalanceProof<P>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        self.reads()
            .prove_balance(&AccountName::new(account), currency, layer, period.as_ref())
            .await
    }

    /// A `tlog-proof` bundle of the record at `position`, at `checkpoint`'s
    /// size: as [`Journal::bundle_record`](crate::Journal::bundle_record).
    ///
    /// # Errors
    ///
    /// As [`Journal::bundle_record`](crate::Journal::bundle_record), and
    /// [`Error::Integrity`] or [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "record", size = tracing::field::Empty))
    )]
    pub async fn bundle_record(
        &self,
        position: u64,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        self.reads().bundle_record(position, checkpoint).await
    }

    /// A `tlog-proof` bundle of entry `id`, at `checkpoint`'s size: as
    /// [`Journal::bundle_entry`](crate::Journal::bundle_entry).
    ///
    /// ```
    /// # #[cfg(feature = "witness")]
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use doubleentry::prelude::*;
    /// use doubleentry::store::MemoryStore;
    /// use doubleentry::transparency::{LogKey, Policy, Proven};
    ///
    /// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
    /// let id = LedgerId::new("example.com/ledgers/acme")?;
    /// let books = Ledger::new(MemoryStore::new(), id.clone()).await?;
    /// books.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
    /// books.define_months(2026, 3..=3).await?;
    /// let sale = books
    ///     .record(
    ///         Entry::new("invoice-2026-0001", date!("2026-03-15"))
    ///             .debit("Assets:Cash", amount!("1190.00"), EUR)
    ///             .credit("Income:Sales", amount!("1190.00"), EUR),
    ///     )
    ///     .await?;
    /// let key = LogKey::from_seed(&id, [7; 32]);
    /// let bundle = books.bundle_entry(sale.id, &books.checkpoint(&key).await?).await?;
    /// let Proven::Record(proven) = bundle.verify(&Policy::log_only(key.vkey()))? else {
    ///     panic!("an entry bundle proves a record");
    /// };
    /// assert_eq!((proven.position, proven.entry_id), (sale.position, Some(sale.id)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok(())
    /// # }
    /// # #[cfg(not(feature = "witness"))]
    /// # fn main() {}
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Journal::bundle_entry`](crate::Journal::bundle_entry), and
    /// [`Error::Integrity`] or [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "entry", size = tracing::field::Empty))
    )]
    pub async fn bundle_entry(
        &self,
        id: EntryId,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        self.reads().bundle_entry(id, checkpoint).await
    }

    /// A `tlog-proof` bundle of `account`'s sealed balance, at
    /// `checkpoint`'s size: as
    /// [`Journal::bundle_balance`](crate::Journal::bundle_balance).
    ///
    /// # Errors
    ///
    /// As [`Journal::bundle_balance`](crate::Journal::bundle_balance), and
    /// [`Error::Integrity`] or [`Error::Backend`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.ledger, kind = "balance", size = tracing::field::Empty))
    )]
    pub async fn bundle_balance<A>(
        &self,
        account: A,
        currency: Currency,
        layer: Layer,
        period: impl AsRef<str>,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        let account = AccountName::new(account);
        self.reads()
            .bundle_balance(&account, currency, layer, period.as_ref(), checkpoint)
            .await
    }

    /// The bytes of hash tile `id`, from stored nodes: as
    /// [`Journal::tile`](crate::Journal::tile).
    ///
    /// # Errors
    ///
    /// As [`Journal::tile`](crate::Journal::tile), and
    /// [`Error::Integrity`] or [`Error::Backend`].
    pub async fn tile(&self, id: TileId) -> Result<Vec<u8>, Error> {
        self.reads().tile(id).await
    }

    /// The bytes of entry bundle `id`: as
    /// [`Journal::tile_entries`](crate::Journal::tile_entries).
    ///
    /// **This discloses every record in the range** in full, memo text
    /// excepted.
    ///
    /// # Errors
    ///
    /// As [`Journal::tile_entries`](crate::Journal::tile_entries), and
    /// [`Error::Integrity`] or [`Error::Backend`].
    pub async fn tile_entries(&self, id: TileId) -> Result<Vec<u8>, Error> {
        self.reads().entry_bundle(id).await
    }
}

/// The opaque projection cursor after the row keyed by `position`.
fn key_after(position: u64) -> Vec<u8> {
    ProjectionRow::new(vec![crate::store::cells::int(position)]).to_bytes()
}

/// The error for a leaf at `position` that does not decode.
fn undecodable(position: u64) -> Error {
    IntegrityError::new(IntegrityKind::Undecodable, Some(position)).into()
}

/// The position a `leaves` or `memos` row is keyed by.
fn row_position(row: &ProjectionRow) -> Result<u64, Error> {
    match row.cells().first() {
        Some(Cell::Int(position)) => u64::try_from(*position)
            .map_err(|_| IntegrityError::new(IntegrityKind::ReplayMismatch, None).into()),
        _ => Err(IntegrityError::new(IntegrityKind::ReplayMismatch, None).into()),
    }
}

/// The memo (text and salt) and fingerprint a `memos` row holds.
fn side_cells(row: &ProjectionRow) -> Result<(Option<MemoPlain>, Option<Fingerprint>), Error> {
    let bad = || -> Error { IntegrityError::new(IntegrityKind::ReplayMismatch, None).into() };
    let [_, fingerprint, _, salt, text] = row.cells() else {
        return Err(bad());
    };
    let fingerprint = match fingerprint {
        Cell::Null => None,
        Cell::Bytes(bytes) => Some(Fingerprint::from_bytes(
            bytes.as_slice().try_into().map_err(|_| bad())?,
        )),
        _ => return Err(bad()),
    };
    let memo = match (salt, text) {
        (Cell::Bytes(salt), Cell::Text(text)) => Some(MemoPlain {
            salt: MemoSalt(salt.as_slice().try_into().map_err(|_| bad())?),
            text: text.clone(),
        }),
        (Cell::Null, Cell::Null) => None,
        _ => return Err(bad()),
    };
    Ok((memo, fingerprint))
}
