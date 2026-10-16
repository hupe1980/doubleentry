//! [`Journal`]: a ledger in memory, written synchronously.
//!
//! ```
//! use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
//!
//! let books = Journal::new(LedgerId::new("example.com/acme")?); // P = 2
//! let mut books = books.as_actor("setup"); // actor for master data
//! books.declare_currency(EUR, 2, date!("2026-01-01"))?;
//! books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
//! books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
//! books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))?;
//!
//! let rec = books.record(
//!     Entry::new("invoice-2026-0001", date!("2026-03-15"))
//!         .debit("Assets:Cash", amount!("1190.00"), EUR)
//!         .credit("Income:Sales", amount!("1190.00"), EUR),
//! )?; // Ok(Recorded) | Err(Error::Rejected(r))
//! let proof = books.prove_entry(rec.id)?; // verifies against books.head()
//! # let leaf = books.leaves().nth(usize::try_from(rec.position)?).map(|leaf| leaf.bytes.to_vec());
//! # proof.verify_record::<2>(&books.head(), &leaf.unwrap_or_default())?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::ops::RangeInclusive;
use std::sync::Arc;

use time::Date;

use crate::core::{
    self, AccountSpec, AuditFailure, AuditFailureKind, AuditReport, ChangeInput, Done, HoldState,
    MemState, Op, Outcome, Recorded, ReplayLeaf, Replayer, SealReceipt, StateView,
};
use crate::error::{Error, QueryError};
use crate::export::{Export, ExportScope, Exporter};
use crate::facade::{Account, Reads, SaltSource, now, seal_first, unexpected};
use crate::log::{ConsistencyProof, InclusionProof, SealedBalanceProof, TreeHead};
use crate::model::{
    AccountHandle, AccountKind, AccountRef, Amount, Balance, BalanceLimit, Currency, Entry,
    EntryId, EntryRecord, Layer, LedgerId, LedgerPolicy, MemoSalt, PostingRef, ValueError,
};
use crate::report::{
    AccountName, AssertionOutcome, BalanceAssertion, BalanceQuery, OpenItem, Rollup, Statement,
    TrialBalance,
};
use crate::store::{Erasure, MemView, apply};
#[cfg(feature = "witness")]
use crate::transparency::{Checkpoint, KeyName, LogKey};
use crate::transparency::{ProofBundle, SignedCheckpoint, TileId};
use crate::{HoldCursor, Page, PageRead};

/// A ledger kept in memory and written synchronously: the rule core over an
/// in-memory state.
///
/// Every write builds an operation, lets the rule core decide it against the
/// journal's current state, and applies the outcome completely or not at
/// all. A refused or already-applied operation leaves the journal
/// byte-identical, whatever its history. `Ledger<S>` has the same methods,
/// `async`, over a store.
///
/// Master data is written by the journal's actor ([`as_actor`](Self::as_actor),
/// `system` by default). Names (account paths, period ids) are taken as text
/// and validated by the core, so a malformed one is a
/// [`Rejection`](crate::Rejection), never a panic.
///
/// The scale `P` is fixed per journal: [`Journal::new`] gives `P = 2`;
/// other scales come from [`with_precision`](Self::with_precision).
#[derive(Clone)]
pub struct Journal<const P: u8 = 2> {
    state: Arc<MemState<P>>,
    actor: String,
    salt: SaltSource,
}

impl Journal<2> {
    /// A new journal with two decimal places (`P = 2`): the genesis record
    /// of `ledger` and nothing else.
    #[must_use]
    pub fn new(ledger: LedgerId) -> Self {
        Self::with_precision(ledger)
    }
}

impl<const P: u8> fmt::Debug for Journal<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal")
            .field("ledger", self.state.ledger())
            .field("head", &self.state.head())
            .field("actor", &self.actor)
            .finish_non_exhaustive()
    }
}

impl<const P: u8> Journal<P> {
    /// The compile-time scale check.
    const SCALE_OK: () = assert!(P <= Amount::<P>::MAX_PRECISION, "P must be <= 18");

    /// A new journal of scale `P` (at most 18 decimal places).
    ///
    /// ```
    /// use doubleentry::{Journal, LedgerId};
    ///
    /// let books = Journal::<4>::with_precision(LedgerId::new("example.com/fx")?);
    /// assert_eq!(books.head().size, 1); // the genesis record
    /// # Ok::<(), doubleentry::error::ValueError>(())
    /// ```
    #[must_use]
    pub fn with_precision(ledger: LedgerId) -> Self {
        let () = Self::SCALE_OK;
        let mut state = MemState::new(ledger);
        if let Ok(Outcome::Append(genesis)) = core::execute(&state, &Op::Genesis) {
            // The genesis of an empty state always applies.
            let _applied = state.apply(*genesis);
        }
        Self::from_state(state)
    }

    fn from_state(state: MemState<P>) -> Self {
        Self {
            state: Arc::new(state),
            actor: String::from("system"),
            salt: SaltSource::Os,
        }
    }

    /// The journal a finished replay rebuilt, writing as `actor` with memo
    /// salts from `salt`.
    pub(crate) fn from_replay(replay: Replayer<P>, actor: &str, salt: SaltSource) -> Self {
        Self {
            state: Arc::new(replay.into_state()),
            actor: actor.to_owned(),
            salt,
        }
    }

    /// A journal rebuilt from its leaves and their side data, replaying each
    /// through the rule core (so the result has passed the same checks as an
    /// [`audit`](Self::audit)).
    ///
    /// ```
    /// use doubleentry::{AccountKind, Journal, LedgerId};
    /// use doubleentry::date;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// let restored: Journal = Journal::from_log(books.id(), books.leaves())?;
    /// assert_eq!(restored.head(), books.head());
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] at the first leaf the core would not have
    /// written, or when the log has no genesis.
    pub fn from_log<'a>(
        ledger: &LedgerId,
        leaves: impl IntoIterator<Item = ReplayLeaf<'a>>,
    ) -> Result<Self, Error> {
        let mut replay = Replayer::new(ledger);
        for leaf in leaves {
            replay.feed(leaf)?;
        }
        if replay.head().size == 0 {
            return Err(AuditFailure {
                position: Some(0),
                kind: AuditFailureKind::GenesisMissing,
            }
            .into());
        }
        Ok(Self::from_state(replay.into_state()))
    }

    /// The same journal, drawing memo salts from `source` instead of the
    /// operating system — for deterministic tests and simulations. A salt
    /// must be unpredictable for memo erasure to protect anything.
    #[must_use]
    pub fn with_salt_source(self, source: impl Fn() -> MemoSalt + Send + Sync + 'static) -> Self {
        Self {
            salt: SaltSource::custom(source),
            ..self
        }
    }

    /// The same journal, recording `actor` on master data from now on. An
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
    pub fn id(&self) -> &LedgerId {
        self.state.ledger()
    }

    /// Every read, over the journal's state.
    fn reads(&self) -> Reads<'_, MemView<P>, P> {
        Reads::new(MemView(Arc::clone(&self.state)), self.state.ledger())
    }

    /// Runs one operation through the core and applies its outcome.
    pub(crate) fn run(&mut self, op: &Op<P>) -> Result<Done<P>, Error> {
        let outcome = core::execute(&*self.state, op)?;
        self.commit(outcome)
    }

    fn commit(&mut self, outcome: Outcome<P>) -> Result<Done<P>, Error> {
        Ok(apply(Arc::make_mut(&mut self.state), outcome)?)
    }

    // ── Master data and calendar ────────────────────────────────────────────

    /// Declares a currency with its number of decimal places (at most `P`),
    /// or raises it. Declared currencies are the only ones entries may use.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 107, 123, 309, 310, 424.
    pub fn declare_currency(
        &mut self,
        code: Currency,
        exponent: u8,
        effective: Date,
    ) -> Result<(), Error> {
        let actor = self.actor.clone();
        self.run(&Op::Currency {
            code,
            exponent,
            effective,
            actor,
        })
        .map(drop)
    }

    /// Opens an account at `path` of `kind` from `on`: no class, no end, no
    /// limit. Returns its handle; opening the same account again returns the
    /// same handle and writes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 103, 107, 309, 420, 421, 428.
    pub fn open(
        &mut self,
        path: impl AsRef<str>,
        kind: AccountKind,
        on: Date,
    ) -> Result<AccountHandle, Error> {
        self.register(AccountSpec::new(path, kind, on))
    }

    /// Registers an account with its full master data.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 103, 107, 122, 309, 420, 421, 428.
    pub fn register(&mut self, spec: AccountSpec) -> Result<AccountHandle, Error> {
        let actor = self.actor.clone();
        self.run(&Op::Register { spec, actor })?.into_account()
    }

    fn change<A>(&mut self, account: A, change: ChangeInput, effective: Date) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        let account = core::account_ref(account)?;
        let actor = self.actor.clone();
        self.run(&Op::Change {
            account,
            change,
            effective,
            actor,
        })
        .map(drop)
    }

    /// Sets an account's balance limit from `effective` on. The latest limit
    /// applies to every entry, whatever its booking date.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 103, 107, 302, 309, 310.
    pub fn set_limit<A>(
        &mut self,
        account: A,
        limit: BalanceLimit,
        effective: Date,
    ) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(account, ChangeInput::Limit(limit), effective)
    }

    /// Sets (or removes) an account's class; refused once it has postings.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 103, 107, 302, 309, 310, 422.
    pub fn set_class<A>(
        &mut self,
        account: A,
        class: Option<&str>,
        effective: Date,
    ) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(
            account,
            ChangeInput::Class(class.map(str::to_owned)),
            effective,
        )
    }

    /// Closes an account: `last_day` is the last day it can be posted to.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 103, 107, 122, 302, 309, 310, 423.
    pub fn close_account<A>(&mut self, account: A, last_day: Date) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(account, ChangeInput::OpenTo(Some(last_day)), last_day)
    }

    /// Reopens a closed account from `effective` on.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 103, 107, 302, 309, 310.
    pub fn reopen_account<A>(&mut self, account: A, effective: Date) -> Result<(), Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        self.change(account, ChangeInput::OpenTo(None), effective)
    }

    /// Sets the ledger policy from `effective` on.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 107, 309, 310, 431.
    pub fn set_policy(&mut self, policy: LedgerPolicy, effective: Date) -> Result<(), Error> {
        let actor = self.actor.clone();
        self.run(&Op::Policy {
            policy,
            effective,
            actor,
        })
        .map(drop)
    }

    /// Defines the next period of the calendar: the first may start on any
    /// day, every later one the day after the last one ends.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 107, 121, 425, 426.
    pub fn define_period(
        &mut self,
        id: impl AsRef<str>,
        start: Date,
        end: Date,
    ) -> Result<(), Error> {
        self.run(&Op::Period {
            id: id.as_ref().to_owned(),
            start,
            end,
        })
        .map(drop)
    }

    /// Defines one period per month of `year`, ids `YYYY-MM`, back to back —
    /// sugar over [`define_period`](Self::define_period). Each month is one
    /// period record; months already defined identically are skipped, so a
    /// retry after a failure completes the calendar.
    ///
    /// ```
    /// use doubleentry::{Journal, LedgerId};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.define_months(2026, 1..=12)?;
    /// books.define_months(2026, 1..=12)?; // already defined: writes nothing
    /// assert_eq!(books.head().size, 13);
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 107 for a month outside `1..=12` (or a year whose
    /// days cannot be represented), and those of
    /// [`define_period`](Self::define_period).
    pub fn define_months(&mut self, year: i32, months: RangeInclusive<u8>) -> Result<(), Error> {
        for (id, start, end) in core::months(year, months)? {
            self.define_period(id, start, end)?;
        }
        Ok(())
    }

    /// Seals a period: its seal record commits each account's balance
    /// through the period's end. Sealing a sealed period returns its receipt
    /// and writes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 107, 111, 311, 427, 500.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "seal_period", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), period = %crate::trace::key(id.as_ref()), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub fn seal_period(&mut self, id: impl AsRef<str>) -> Result<SealReceipt<P>, Error> {
        let result = self
            .run(&Op::Seal {
                period: id.as_ref().to_owned(),
            })
            .and_then(Done::into_sealed);
        #[cfg(feature = "tracing")]
        crate::trace::sealed(result.as_ref());
        result
    }

    /// Seals every period up to and including `id` that has no seal yet,
    /// earliest first — one [`seal_period`](Self::seal_period) each — and
    /// returns the receipts of the periods it sealed, `id`'s last (only
    /// `id`'s when nothing earlier was open). Each seal is its own write: a
    /// failure keeps the seals before it, and calling again continues.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.define_months(2026, 1..=12)?;
    /// // March alone is refused: January and February are open.
    /// let refused = books.seal_period("2026-03").unwrap_err();
    /// assert_eq!(refused.code(), Some(427));
    /// assert!(refused.to_string().contains("seal from 2026-01 on"));
    /// let sealed = books.seal_through("2026-03")?;
    /// let periods: Vec<String> = sealed.iter().map(|r| r.period.to_string()).collect();
    /// assert_eq!(periods, ["2026-01", "2026-02", "2026-03"]);
    /// assert_eq!(books.seal_through("2026-03")?.len(), 1); // sealed: its receipt
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Those of [`seal_period`](Self::seal_period) but 427.
    pub fn seal_through(&mut self, id: impl AsRef<str>) -> Result<Vec<SealReceipt<P>>, Error> {
        let mut sealed = Vec::new();
        loop {
            match self.seal_period(id.as_ref()) {
                Ok(receipt) => {
                    sealed.push(receipt);
                    return Ok(sealed);
                }
                Err(error) => match seal_first(&error) {
                    Some(earliest) => sealed.push(self.seal_period(earliest)?),
                    None => return Err(error),
                },
            }
        }
    }

    /// The year-end closing drafts: every income and expense slice through
    /// `on` zeroed into `equity`, chunked to the posting and leaf limits
    /// under the keys `{key}/1`, `{key}/2`, … Record them in order with
    /// [`record`](Self::record). Nothing to close gives no drafts.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 100, 103, 111, 113, 302, 303, 429, 430.
    pub fn close_year<A>(&self, key: &str, on: Date, equity: A) -> Result<Vec<Entry<P>>, Error>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        let op = Op::CloseYear {
            key: key.to_owned(),
            on,
            equity: core::account_ref(equity)?,
        };
        match core::execute(&*self.state, &op)? {
            Outcome::Unchanged(done) => done.into_closing(),
            _ => Err(unexpected()),
        }
    }

    // ── Bookings ────────────────────────────────────────────────────────────

    /// Records an entry.
    ///
    /// A retry of a recorded draft returns the original receipt with
    /// `replayed == true` and writes nothing — even after its period was
    /// sealed or its account closed. "The same draft" means the same postings
    /// (an account named by path or by handle is the same account), memo
    /// text and capture time. A memo without a salt
    /// gets one from the journal's salt source; retries are unaffected.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`] with the rejection of the highest precedence;
    /// [`Error::Backend`] when the operating system has no randomness for a
    /// memo salt.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), key = %crate::trace::draft_key(&entry), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub fn record(&mut self, entry: Entry<P>) -> Result<Recorded, Error> {
        let result = self.record_entry(entry);
        #[cfg(feature = "tracing")]
        crate::trace::recorded(result.as_ref());
        result
    }

    fn record_entry(&mut self, entry: Entry<P>) -> Result<Recorded, Error> {
        let entry = self.salt.salted(entry)?;

        let outcome = core::check(&*self.state, &entry)?;

        self.commit(outcome)?.into_recorded()
    }

    /// Records `drafts` atomically, in order: each draft is checked against
    /// the state the drafts before it leave, and either every draft is
    /// recorded or none is. Returns one receipt per draft.
    ///
    /// A draft already recorded — by an earlier write, or earlier in the
    /// same batch — replays as [`record`](Self::record) would, so recording
    /// an identical batch again returns the original receipts, each with
    /// `replayed == true`, and writes nothing.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/payroll")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Expenses:Wages", AccountKind::Expense, date!("2026-01-01"))?;
    /// books.set_limit("Assets:Bank", BalanceLimit::NoCreditBalance, date!("2026-01-01"))?;
    /// let pay = |key: &str, eur| {
    ///     Entry::new(key, date!("2026-03-31"))
    ///         .debit("Expenses:Wages", eur, EUR)
    ///         .credit("Assets:Bank", eur, EUR)
    /// };
    /// let fund = Entry::new("fund", date!("2026-03-01"))
    ///     .debit("Assets:Bank", amount!("3000.00"), EUR)
    ///     .credit("Expenses:Wages", amount!("3000.00"), EUR);
    ///
    /// // The second wage would overdraw the bank: nothing of the batch is recorded.
    /// let run = [fund.clone(), pay("alice", amount!("2000.00")), pay("bob", amount!("1500.00"))];
    /// match books.record_batch(run) {
    ///     Err(Error::BatchRejected { index: 2, rejection: Rejection::LimitExceeded { .. } }) => {}
    ///     other => panic!("expected draft 2 refused, got {other:?}"),
    /// }
    /// assert!(books.entry_by_key("fund").is_none());
    ///
    /// let run = [fund, pay("alice", amount!("2000.00")), pay("bob", amount!("1000.00"))];
    /// let first = books.record_batch(run.clone())?;
    /// let again = books.record_batch(run)?; // a retry of the whole batch
    /// assert!(again.iter().zip(&first).all(|(a, f)| a.replayed && a.id == f.id));
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::BatchRejected`] naming the first refused draft and its
    /// rejection; [`Error::Backend`] when the operating system has no
    /// randomness for a memo salt.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record_batch", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), outcome = tracing::field::Empty, code = tracing::field::Empty, index = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub fn record_batch(
        &mut self,
        drafts: impl IntoIterator<Item = Entry<P>>,
    ) -> Result<Vec<Recorded>, Error> {
        let result = self
            .salt
            .salted_all(drafts)
            .and_then(|drafts| self.run(&Op::Batch(drafts)))
            .and_then(Done::into_batch);
        #[cfg(feature = "tracing")]
        crate::trace::batch(result.as_ref());
        result
    }

    /// The draft that reverses entry `id`: its postings mirrored (sides
    /// swapped), booked on `on` under `key`. It checks no rule; record it
    /// with [`record`](Self::record).
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry has the id.
    pub fn reversal(&self, id: EntryId, key: &str, on: Date) -> Result<Entry<P>, Error> {
        now(self.reads().reversal(id, key, on))
    }

    /// Clears `amount` of a debit posting against a credit posting of the
    /// same balance; returns the clearing record's position (replays
    /// included).
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 100, 120, 200, 300, 301, 410–415, 419.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), key = %crate::trace::key(key), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub fn clear(
        &mut self,
        key: &str,
        debit: PostingRef,
        credit: PostingRef,
        amount: Amount<P>,
        on: Date,
    ) -> Result<u64, Error> {
        let result = self.clearing(&Op::Clear {
            key: key.to_owned(),
            debit,
            credit,
            amount,
            date: on,
        });
        #[cfg(feature = "tracing")]
        crate::trace::cleared(result.as_ref());
        result
    }

    /// Runs a clearing operation; its record's position.
    fn clearing(&mut self, op: &Op<P>) -> Result<u64, Error> {
        self.run(op)?.into_cleared()
    }

    /// Resets the clearing applied at position `clearing`; returns the reset
    /// record's position.
    ///
    /// # Errors
    ///
    /// [`Error::Rejected`]: 100, 200, 300, 301, 416–418.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "record", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), key = %crate::trace::key(key), outcome = tracing::field::Empty, code = tracing::field::Empty, position = tracing::field::Empty))
    )]
    pub fn reset_clearing(&mut self, key: &str, clearing: u64, on: Date) -> Result<u64, Error> {
        let result = self.clearing(&Op::ResetClearing {
            key: key.to_owned(),
            clearing,
            date: on,
        });
        #[cfg(feature = "tracing")]
        crate::trace::cleared(result.as_ref());
        result
    }

    /// Erases entry `id`'s memo text and salt (GDPR Art. 17): no leaf, hash
    /// or proof changes. The stored fingerprint of its draft goes too, so a
    /// later submission of its key is a key conflict and a guessed memo
    /// cannot be confirmed. Erasing again, or an entry without a memo,
    /// changes nothing. When to erase is the caller's decision (the end of
    /// the retention period).
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry has the id.
    pub fn erase_memo(&mut self, id: EntryId) -> Result<(), Error> {
        match Arc::make_mut(&mut self.state).erase_memo(id) {
            Erasure::UnknownEntry => Err(Error::Query(QueryError::UnknownEntry { id })),
            Erasure::Erased | Erasure::AlreadyErased | Erasure::NoMemo => Ok(()),
        }
    }

    // ── Lookups (absence is `None`, never an error) ─────────────────────────

    /// The entry with id `id`, as recorded.
    ///
    /// ```
    /// # use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
    /// # let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// # books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// # books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR)
    ///         .memo("for the customer"),
    /// )?;
    /// assert_eq!(books.entry(sale.id).map(|entry| entry.parts().postings.len()), Some(2));
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    #[must_use]
    pub fn entry(&self, id: EntryId) -> Option<EntryRecord<P>> {
        now(self.reads().entry(id)).ok().flatten()
    }

    /// The entry recorded under idempotency key `key`.
    ///
    /// ```
    /// use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// let recorded = books.record(
    ///     Entry::new("invoice-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR),
    /// )?;
    /// assert_eq!(books.entry_by_key("invoice-1"), books.entry(recorded.id));
    /// assert_eq!(books.entry_by_key("invoice-2"), None);
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    #[must_use]
    pub fn entry_by_key(&self, key: impl AsRef<str>) -> Option<EntryRecord<P>> {
        now(self.reads().entry_by_key(key.as_ref().as_bytes()))
            .ok()
            .flatten()
    }

    /// The master data in force of an account, by path or handle.
    ///
    /// ```
    /// use doubleentry::{AccountKind, BalanceLimit, Journal, LedgerId, date};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// let wallet = books.open("Liabilities:Wallet", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.set_limit(wallet, BalanceLimit::NoDebitBalance, date!("2026-01-01"))?;
    /// let account = books.account("Liabilities:Wallet").expect("opened");
    /// assert_eq!((account.handle, account.limit), (wallet, BalanceLimit::NoDebitBalance));
    /// assert!(books.account("Liabilities:Other").is_none());
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    #[must_use]
    pub fn account<A>(&self, account: A) -> Option<Account>
    where
        A: TryInto<AccountRef>,
        A::Error: Into<ValueError>,
    {
        let account = account.try_into().ok()?;
        now(self.reads().account(account)).ok().flatten()
    }

    /// The memo text of entry `id`; `None` without a memo, once erased, or
    /// for an unknown entry.
    ///
    /// ```
    /// # use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
    /// # let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// # books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// # books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR)
    ///         .memo("for the customer"),
    /// )?;
    /// assert_eq!(books.memo(sale.id).as_deref(), Some("for the customer"));
    /// books.erase_memo(sale.id)?;
    /// assert_eq!(books.memo(sale.id), None);
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    #[must_use]
    pub fn memo(&self, id: EntryId) -> Option<String> {
        self.memo_text(id)
    }

    /// The declared exponent of `currency` — the decimal places its amounts
    /// have and reports print (`0` for JPY) — or `None` while undeclared.
    /// Write an amount with it through
    /// [`Amount::display_at`](crate::Amount::display_at).
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(JPY, 0, date!("2026-01-01"))?;
    /// assert_eq!(books.exponent(JPY), Some(0));
    /// assert_eq!(books.exponent(EUR), None);
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    #[must_use]
    pub fn exponent(&self, currency: Currency) -> Option<u8> {
        self.state
            .currency_latest(currency)
            .map(|latest| latest.value)
    }

    /// The memo text of entry `id`, unless erased.
    fn memo_text(&self, id: EntryId) -> Option<String> {
        now(self.reads().memo(id)).ok().flatten()
    }

    // ── Integrity and proofs ────────────────────────────────────────────────

    /// Replays the whole log through the rule core and compares the result
    /// with the journal's state.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] at the first difference.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "audit", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), records = tracing::field::Empty, outcome = tracing::field::Empty, code = tracing::field::Empty))
    )]
    pub fn audit(&self) -> Result<AuditReport, Error> {
        let result = self.replay();
        #[cfg(feature = "tracing")]
        crate::trace::audited(result.as_ref());
        result
    }

    /// The audit's replay of every leaf, compared with the journal's state.
    fn replay(&self) -> Result<AuditReport, Error> {
        let mut replay = Replayer::new(self.id());
        for leaf in self.state.leaves() {
            replay.feed(leaf)?;
        }
        Ok(replay.finish_state(&self.state)?)
    }

    /// The tree head: the log's size and root.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        self.state.head()
    }

    /// The tree head the log had at `size` records: what a proof at that
    /// size verifies against.
    ///
    /// ```
    /// # use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
    /// # let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// # books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// # books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR)
    ///         .memo("for the customer"),
    /// )?;
    /// let then = books.head();
    /// books.define_months(2026, 4..=4)?;
    /// assert_eq!(books.head_at(then.size)?, then);
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for a size beyond the head.
    pub fn head_at(&self, size: u64) -> Result<TreeHead, Error> {
        now(self.reads().head_at(size))
    }

    /// The proof that entry `id` is in the log, against [`head`](Self::head).
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry has the id.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "entry", size = tracing::field::Empty))
    )]
    pub fn prove_entry(&self, id: EntryId) -> Result<InclusionProof, Error> {
        now(self.reads().inclusion(id, None))
    }

    /// The proof that entry `id` is in the log as it was at `size` records,
    /// against [`head_at(size)`](Self::head_at).
    ///
    /// ```
    /// use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR),
    /// )?;
    /// let then = books.head();
    /// books.define_months(2026, 4..=4)?;
    /// let proof = books.prove_entry_at(sale.id, then.size)?;
    /// assert_eq!(books.head_at(then.size)?, then);
    /// assert_eq!((proof.index, proof.size), (sale.position, then.size));
    /// // The old head is a prefix of the new one.
    /// books.prove_consistency(then.size, books.head().size)?.verify(&then, &books.head())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when no entry had the id at that size, or for a size
    /// beyond the head.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "entry", size = tracing::field::Empty))
    )]
    pub fn prove_entry_at(&self, id: EntryId, size: u64) -> Result<InclusionProof, Error> {
        now(self.reads().inclusion(id, Some(size)))
    }

    /// The proof that the log at `new_size` records extends the log at
    /// `old_size`: nothing recorded by `old_size` was changed or removed.
    ///
    /// ```
    /// # use doubleentry::{AccountKind, Entry, Journal, LedgerId, amount, date, currency::EUR};
    /// # let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// # books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// # books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// # books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// # books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR)
    ///         .memo("for the customer"),
    /// )?;
    /// let then = books.head();
    /// books.define_months(2026, 4..=4)?;
    /// let proof = books.prove_consistency(then.size, books.head().size)?;
    /// assert!(proof.verify(&then, &books.head()).is_ok());
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for an old size of 0 or above the new one, or a new
    /// size beyond the head.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "consistency", size = tracing::field::Empty))
    )]
    pub fn prove_consistency(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, Error> {
        now(self.reads().consistency(old_size, new_size))
    }

    /// Every leaf with its side data (memo text and salt, draft fingerprint),
    /// in position order — what [`from_log`](Self::from_log) takes.
    pub fn leaves(&self) -> impl Iterator<Item = ReplayLeaf<'_>> {
        self.state.leaves()
    }

    // ── Holds ───────────────────────────────────────────────────────────────

    /// The hold opened by entry `id` and where it stands: open, or posted,
    /// voided or expired by the entry that resolved it. `None` when `id` is
    /// not a hold.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::{HoldStatus, PostAmount};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/payments")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.define_months(2026, 11..=11)?;
    /// let hold = books.record(
    ///     Entry::open_hold("auth-4711", date!("2026-11-20"))
    ///         .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
    ///         .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
    /// )?;
    /// let void = books.record(Entry::void_hold("void-4711", date!("2026-11-21"), hold.id))?;
    /// let state = books.hold(hold.id).ok_or("no hold")?;
    /// assert_eq!(state.status, HoldStatus::Voided { by: void.id });
    /// assert!(books.hold(void.id).is_none()); // a resolution is no hold
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn hold(&self, id: EntryId) -> Option<HoldState<P>> {
        now(self.reads().hold(id)).ok().flatten()
    }

    /// The open holds that may be expired on `as_of` — their expiry date is
    /// on or before it — ordered by (expiry date, position), one page at a
    /// time: what a scheduler books an [`Entry::expire_hold`] for. Nothing
    /// expires by itself, and no clock is read. A limit outside
    /// `1..=1000` is clamped into it.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::PageRead;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/payments")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
    /// books.define_months(2026, 11..=12)?;
    /// let hold = books.record(
    ///     Entry::open_hold("auth-4711", date!("2026-11-20"))
    ///         .expires(date!("2026-11-30"))
    ///         .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
    ///         .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
    /// )?;
    /// assert!(books.holds_due(date!("2026-11-29"), PageRead::first(50)).items.is_empty());
    /// let due = books.holds_due(date!("2026-11-30"), PageRead::first(50));
    /// assert_eq!(due.items.iter().map(|h| h.id).collect::<Vec<_>>(), [hold.id]);
    ///
    /// books.record(Entry::expire_hold("expire-4711", date!("2026-11-30"), hold.id))?;
    /// assert!(books.holds_due(date!("2026-12-31"), PageRead::first(50)).items.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn holds_due(
        &self,
        as_of: Date,
        page: PageRead<HoldCursor>,
    ) -> Page<HoldState<P>, HoldCursor> {
        let size = self.head().size;
        // The state answers every page; the fallback is never taken.
        now(self.reads().holds_due(as_of, page)).unwrap_or(Page {
            items: Vec::new(),
            next: None,
            size,
            through: size,
        })
    }

    // ── Reports and export ──────────────────────────────────────────────────

    /// The balance of `account` in `currency`: the gross debit, credit and
    /// count of the postings `query` selects.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank", amount!("100.00"), EUR)
    ///         .credit("Income:Sales", amount!("100.00"), EUR),
    /// )?;
    /// assert_eq!(books.balance("Assets:Bank", EUR, &BalanceQuery::all())?.net(), amount!("100.00"));
    /// let before = BalanceQuery::through_position(sale.position - 1);
    /// assert!(books.balance("Assets:Bank", EUR, &before)?.is_zero());
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for an invalid query or an account not registered
    /// in the prefix read; [`Error::Integrity`] for a pinned head this log
    /// does not have.
    pub fn balance<A>(
        &self,
        account: A,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Balance<P>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        now(self
            .reads()
            .balance(&AccountName::new(account), currency, query))
    }

    /// The trial balance: one row per (account, currency) with a posting
    /// `query` selects.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank", amount!("100.00"), EUR)
    ///         .credit("Income:Sales", amount!("100.00"), EUR),
    /// )?;
    /// let trial = books.trial_balance(&BalanceQuery::through(date!("2026-03-31")))?;
    /// assert!(trial.is_balanced() && trial.rows().len() == 2);
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`balance`](Self::balance), and [`Error::Integrity`] for a stored
    /// account record that does not decode.
    pub fn trial_balance(&self, query: &BalanceQuery) -> Result<TrialBalance<P>, Error> {
        now(self.reads().trial_balance(query))
    }

    /// The statement of `account` in `currency` over `query`'s window: the
    /// opening balance, every line in date order with a running balance,
    /// and the closing balance.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank", amount!("100.00"), EUR)
    ///         .credit("Income:Sales", amount!("100.00"), EUR),
    /// )?;
    /// let march = books.statement("Assets:Bank", EUR, &BalanceQuery::period("2026-03"))?;
    /// assert_eq!((march.opening.net(), march.closing.net()), (amount!("0.00"), amount!("100.00")));
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`balance`](Self::balance).
    pub fn statement<A>(
        &self,
        account: A,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Statement<P>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        now(self
            .reads()
            .statement(&AccountName::new(account), currency, query))
    }

    /// The trial balance of `currency` folded up the account tree.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Bank:Main", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank:Main", amount!("100.00"), EUR)
    ///         .credit("Income:Sales", amount!("100.00"), EUR),
    /// )?;
    /// let sheet = books.rollup(EUR, &BalanceQuery::all())?;
    /// assert_eq!(sheet.roots().count(), 2); // Assets, Income
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`trial_balance`](Self::trial_balance), and [`Error::Query`] when
    /// a subtree total does not fit an amount.
    pub fn rollup(&self, currency: Currency, query: &BalanceQuery) -> Result<Rollup<P>, Error> {
        now(self.reads().rollup(currency, query))
    }

    /// The open items of `account` in `currency`: the postings `query`
    /// selects with an amount not offset by clearings (those dated on or
    /// before the window's end, and not reset by then), by position.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// let invoice = books.record(
    ///     Entry::new("invoice-1", date!("2026-03-01"))
    ///         .debit("Assets:Receivables", amount!("119.00"), EUR)
    ///         .credit("Income:Sales", amount!("119.00"), EUR),
    /// )?;
    /// let payment = books.record(
    ///     Entry::new("payment-1", date!("2026-03-10"))
    ///         .debit("Assets:Bank", amount!("100.00"), EUR)
    ///         .credit("Assets:Receivables", amount!("100.00"), EUR),
    /// )?;
    /// books.clear(
    ///     "clear-1",
    ///     PostingRef::at(invoice.id, 0),
    ///     PostingRef::at(payment.id, 1),
    ///     amount!("100.00"),
    ///     date!("2026-03-10"),
    /// )?;
    /// let open = books.open_items("Assets:Receivables", EUR, &BalanceQuery::all())?;
    /// assert_eq!(open.len(), 1);
    /// assert_eq!(open[0].residual, amount!("19.00"));
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`balance`](Self::balance).
    pub fn open_items<A>(
        &self,
        account: A,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Vec<OpenItem<P>>, Error>
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        now(self
            .reads()
            .open_items(&AccountName::new(account), currency, query))
    }

    /// Checks one balance assertion.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::{report::{AssertAt, BalanceAssertion}};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// let sale = books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank", amount!("100.00"), EUR)
    ///         .credit("Income:Sales", amount!("100.00"), EUR),
    /// )?;
    /// let claim = BalanceAssertion::new("Assets:Bank", EUR, AssertAt::Position(sale.position), amount!("100.00"));
    /// assert!(books.check_assertion(&claim)?.is_held());
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`balance`](Self::balance) — a position beyond the head included —
    /// never for a difference.
    pub fn check_assertion(
        &self,
        assertion: &BalanceAssertion<P>,
    ) -> Result<AssertionOutcome<P>, Error> {
        now(self.reads().check_assertion(assertion))
    }

    /// Checks every assertion: one outcome each, in their order.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    /// use doubleentry::{report::{AssertAt, BalanceAssertion, DateBasis}};
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank", amount!("100.00"), EUR)
    ///         .credit("Income:Sales", amount!("100.00"), EUR),
    /// )?;
    /// let day = |date| AssertAt::EndOfDay { date, basis: DateBasis::Booking };
    /// let outcomes = books.check_assertions(&[
    ///     BalanceAssertion::new("Assets:Bank", EUR, day(date!("2026-03-14")), amount!("0.00")),
    ///     BalanceAssertion::new("Assets:Bank", EUR, day(date!("2026-03-15")), amount!("90.00")),
    /// ])?;
    /// assert_eq!(outcomes.iter().map(|outcome| outcome.is_held()).collect::<Vec<_>>(), [true, false]);
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// The first error of [`check_assertion`](Self::check_assertion).
    pub fn check_assertions(
        &self,
        assertions: &[BalanceAssertion<P>],
    ) -> Result<Vec<AssertionOutcome<P>>, Error> {
        now(self.reads().check_assertions(assertions))
    }

    /// The GoBD export view of `scope`, pinned to `head` (or to the head
    /// now): every row, in log order. See [`export`](crate::export) for the
    /// rows and how a recipient verifies them.
    ///
    /// ```
    /// use doubleentry::export::{ExportRow, ExportScope};
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(JPY, 0, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Bank", amount!("500.00"), JPY)
    ///         .credit("Income:Sales", amount!("500.00"), JPY),
    /// )?;
    /// let postings: Vec<(i128, u8)> = books
    ///     .export(ExportScope::All, None)?
    ///     .filter_map(|row| match row {
    ///         Ok(ExportRow::Posting(posting)) => Some((posting.minor_units, posting.exponent)),
    ///         _ => None,
    ///     })
    ///     .collect();
    /// assert_eq!(postings, [(500, 0), (500, 0)]); // yen have no minor unit
    /// # Ok::<(), Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for a period id that is no label or names no period
    /// in the prefix, or a head beyond the log; [`Error::Integrity`] for a
    /// head this log does not have. A row is an error only for a record the
    /// rule core would not have written.
    pub fn export(
        &self,
        scope: ExportScope,
        head: Option<TreeHead>,
    ) -> Result<Export<'_, P>, Error> {
        let bound = match head {
            Some(head) => {
                now(self.reads().pinned(head))?;
                head.size
            }
            None => self.head().size,
        };
        let exporter = Exporter::new(scope)?;
        if let Some(id) = exporter.period()
            && !self
                .state
                .period(id)
                .is_some_and(|view| view.position < bound)
        {
            return Err(QueryError::UnknownPeriod {
                id: id.as_str().to_owned(),
            }
            .into());
        }
        Ok(Export::new(&self.state, exporter, bound))
    }
}

// ── Transparency ────────────────────────────────────────────────────────────

impl<const P: u8> Journal<P> {
    /// The ledger's head as a C2SP checkpoint, signed by `key` (feature
    /// `witness`).
    ///
    /// ```
    /// use doubleentry::transparency::LogKey;
    /// use doubleentry::{Journal, LedgerId};
    ///
    /// let ledger = LedgerId::new("example.com/ledgers/acme")?;
    /// let books = Journal::new(ledger.clone());
    /// let key = LogKey::from_seed(&ledger, [7; 32]);
    /// let signed = books.checkpoint(&key)?;
    /// assert_eq!(signed.checkpoint().head(), books.head());
    /// signed.verify(&[key.vkey()])?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Transparency`] with
    /// [`KeyNameMismatch`](crate::transparency::Error::KeyNameMismatch) when
    /// the key is not this ledger's.
    #[cfg(feature = "witness")]
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "checkpoint", target = "doubleentry", level = "info", skip_all, fields(ledger = %self.id(), size = tracing::field::Empty))
    )]
    pub fn checkpoint(&self, key: &LogKey) -> Result<SignedCheckpoint, Error> {
        let head = self.head();
        #[cfg(feature = "tracing")]
        crate::trace::proved(head.size);
        Ok(key.sign(&Checkpoint::new(KeyName::from(self.id()), head))?)
    }

    /// The proof of `account`'s balance in (`currency`, `layer`) as sealed
    /// for `period`, against [`head`](Self::head): the seal's inclusion,
    /// the registration that names the handle, and the balance leaf at the
    /// handle in the seal's tree (or no path when the seal has no tree for
    /// (currency, layer): the balance is zero). It reads the account's leaf
    /// and the `O(log² accounts)` stored nodes of its path, never every
    /// leaf of the period, and is verified before it is handed out.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 3..=3)?;
    /// books.record(
    ///     Entry::new("sale-1", date!("2026-03-15"))
    ///         .debit("Assets:Cash", amount!("10.00"), EUR)
    ///         .credit("Income:Sales", amount!("10.00"), EUR),
    /// )?;
    /// books.seal_period("2026-03")?;
    /// let proof = books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-03")?;
    /// let verified = proof.verify(&books.head())?;
    /// assert_eq!(verified.account.to_string(), "Assets:Cash");
    /// assert_eq!(verified.balance.net(), amount!("10.00"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`]: an invalid or unknown period
    /// ([`QueryError::NotSealed`] before its seal), or an account unknown or
    /// registered after the seal ([`QueryError::UnknownAccount`]).
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "balance", size = tracing::field::Empty))
    )]
    pub fn prove_balance<A>(
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
        let account = AccountName::new(account);
        now(self
            .reads()
            .prove_balance(&account, currency, layer, period.as_ref()))
    }

    /// A `tlog-proof` bundle of the record at `position`, at `checkpoint`'s
    /// size (which may be below the head).
    ///
    /// ```
    /// # #[cfg(feature = "witness")]
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use doubleentry::transparency::{LogKey, Proven};
    /// use doubleentry::{Journal, LedgerId, record::Record};
    ///
    /// let ledger = LedgerId::new("example.com/ledgers/acme")?;
    /// let books = Journal::new(ledger.clone());
    /// let signed = books.checkpoint(&LogKey::from_seed(&ledger, [7; 32]))?;
    /// let bundle = books.bundle_record(0, &signed)?;
    /// let Proven::Record(genesis) = bundle.verify_inclusion()? else { panic!() };
    /// assert!(matches!(genesis.record, Record::Genesis(_)));
    /// # Ok(())
    /// # }
    /// # #[cfg(not(feature = "witness"))]
    /// # fn main() {}
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Transparency`] with `ForeignCheckpoint` for another log's
    /// checkpoint, `CheckpointMismatch` for a size this ledger has not
    /// reached or another root at that size; [`Error::Query`] for a
    /// position not below the checkpoint's size.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "record", size = tracing::field::Empty))
    )]
    pub fn bundle_record(
        &self,
        position: u64,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        now(self.reads().bundle_record(position, checkpoint))
    }

    /// A `tlog-proof` bundle of entry `id`, at `checkpoint`'s size. Its
    /// verification names the entry and recomputes its id.
    ///
    /// # Errors
    ///
    /// As [`bundle_record`](Self::bundle_record), and
    /// [`QueryError::UnknownEntry`] when no entry had the id at that size.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "entry", size = tracing::field::Empty))
    )]
    pub fn bundle_entry(
        &self,
        id: EntryId,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        now(self.reads().bundle_entry(id, checkpoint))
    }

    /// A `tlog-proof` bundle of `account`'s sealed balance in (`currency`,
    /// `layer`) for `period`, at `checkpoint`'s size: one file that names
    /// the account and proves its balance and nothing about any other
    /// account (layout: [`transparency`](crate::transparency::ProofBundle)).
    ///
    /// # Errors
    ///
    /// As [`bundle_record`](Self::bundle_record) and
    /// [`prove_balance`](Self::prove_balance), at the checkpoint's size.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "prove", target = "doubleentry", level = "debug", skip_all, fields(ledger = %self.id(), kind = "balance", size = tracing::field::Empty))
    )]
    pub fn bundle_balance<A>(
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
        now(self
            .reads()
            .bundle_balance(&account, currency, layer, period.as_ref(), checkpoint))
    }

    /// The bytes of hash tile `id` (`tlog-tiles`): its 32-byte hashes, read
    /// from stored nodes. Hashes only — nothing about any record but its
    /// leaf hash.
    ///
    /// ```
    /// use doubleentry::transparency::TileId;
    /// use doubleentry::{Journal, LedgerId};
    ///
    /// let books = Journal::new(LedgerId::new("example.com/acme")?);
    /// for tile in TileId::changed(0, books.head().size) {
    ///     assert_eq!(books.tile(tile)?.len(), 32 * usize::from(tile.width()));
    /// }
    /// # Ok::<(), doubleentry::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for a tile beyond the head; [`Error::Transparency`]
    /// with `WrongTileKind` for an entry bundle.
    pub fn tile(&self, id: TileId) -> Result<Vec<u8>, Error> {
        now(self.reads().tile(id))
    }

    /// The bytes of entry bundle `id` (`tlog-tiles`): the leaf bytes of its
    /// records, each prefixed by its big-endian 16-bit length.
    ///
    /// **This discloses every record in the range** — postings, accounts,
    /// amounts, documents — in full; only memo text stays out (a leaf
    /// commits to it by a salted hash). Publish entry bundles only where
    /// the books may be read.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for a bundle beyond the head;
    /// [`Error::Transparency`] with `WrongTileKind` for a hash tile.
    pub fn tile_entries(&self, id: TileId) -> Result<Vec<u8>, Error> {
        now(self.reads().entry_bundle(id))
    }
}
