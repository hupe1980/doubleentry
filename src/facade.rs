//! What `Journal` and `Ledger` share: every read, written once against the
//! [`Store`] read primitives so both answer it with the same code; the
//! memo-salt source; and small helpers of their writes.
//!
//! `Journal` drives the reads with [`now`]: its source is a read-only view
//! of its own state, whose futures are ready at once.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use time::Date;

use crate::core::{self, BalanceKey, Done, HoldState, HoldView, ResolutionKind};
use crate::error::{BackendError, Error, IntegrityError, IntegrityKind, QueryError, Rejection};
use crate::log::balance_tree::proof_nodes;
use crate::log::nodes::{ConsistencyPlan, InclusionPlan};
use crate::log::{
    BalanceTree, ConsistencyProof, InclusionProof, SealedBalanceProof, TreeHead, leaf_hash,
};
use crate::model::{
    AccountAction, AccountChange, AccountHandle, AccountKind, AccountPath, AccountRef, Balance,
    BalanceLimit, Currency, Entry, EntryId, EntryRecord, HoldAction, IdempotencyKey, Label, Layer,
    LedgerId, MemoSalt, PeriodId, Record, RecordKind,
};
use crate::report::{
    AccountBook, AccountName, AssertionOutcome, BalanceAssertion, BalanceFold, BalanceQuery,
    OpenItem, OpenItemsFold, Plan, Rollup, Selection, Statement, StatementFold, TrialBalance,
};
use crate::store::{
    AccountRow, BalanceRead, BalanceScope, EntryRef, HoldRead, LeafRead, Memo, Store, StoredEntry,
    head_at, stored_index,
};
use crate::transparency::prove::{
    SealedParts, claimed_head, period_label, same_root, seal_record, sealed_balance_proof,
    tile_beyond, tile_bytes,
};
use crate::transparency::{ProofBundle, SignedCheckpoint, TileId, balance_bundle, entry_bundle};
use crate::{HoldCursor, Page, PageRead};

/// Runs a future that never waits — a read of `Journal`'s own state — to
/// completion.
pub(crate) fn now<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

/// Where a façade gets the salt of a memo the caller did not salt.
#[derive(Clone)]
pub(crate) enum SaltSource {
    /// The operating system's randomness.
    Os,
    /// A caller-supplied source (deterministic tests, simulations).
    Custom(Arc<dyn Fn() -> MemoSalt + Send + Sync>),
}

impl SaltSource {
    /// A caller-supplied source.
    pub(crate) fn custom(source: impl Fn() -> MemoSalt + Send + Sync + 'static) -> Self {
        Self::Custom(Arc::new(source))
    }

    /// The draft with a salt for its memo, when it has a memo without one.
    pub(crate) fn salted<const P: u8>(&self, entry: Entry<P>) -> Result<Entry<P>, Error> {
        let Some(text) = entry.memo_text().filter(|_| entry.memo_salt().is_none()) else {
            return Ok(entry);
        };
        let text = text.to_owned();
        let salt = match self {
            Self::Os => {
                let mut salt = [0u8; 16];
                getrandom::fill(&mut salt).map_err(BackendError::transient)?; // purity-exempt: the façade draws the salt; the core only receives it
                MemoSalt(salt)
            }
            Self::Custom(source) => source(),
        };
        Ok(entry.memo_salted(text, salt))
    }

    /// Every item of a batch, its entry drafts salted, in order. More than
    /// [`Op::MAX_BATCH`](core::Op::MAX_BATCH) items are passed on unsalted:
    /// the core refuses the batch before reading anything (124), and no
    /// randomness is drawn for it.
    pub(crate) fn salted_items<const P: u8>(
        &self,
        items: impl IntoIterator<Item = core::BatchItem<P>>,
    ) -> Result<Vec<core::BatchItem<P>>, Error> {
        let items: Vec<_> = items.into_iter().collect();
        if items.len() > core::Op::<P>::MAX_BATCH {
            return Ok(items);
        }
        items
            .into_iter()
            .map(|item| match item {
                core::BatchItem::Record(entry) => self.salted(entry).map(core::BatchItem::Record),
                other => Ok(other),
            })
            .collect()
    }
}

/// The period to seal first when sealing failed with
/// [`Rejection::PredecessorNotSealed`]: what
/// [`Journal::seal_through`](crate::Journal::seal_through) seals next.
pub(crate) fn seal_first(error: &Error) -> Option<String> {
    match error {
        Error::Rejected(Rejection::PredecessorNotSealed { earliest, .. }) => {
            Some(earliest.to_string())
        }
        _ => None,
    }
}

/// The entry receipts of a batch of entries.
pub(crate) fn recorded_all(
    receipts: Vec<core::BatchReceipt>,
) -> Result<Vec<core::Recorded>, Error> {
    receipts
        .into_iter()
        .map(|receipt| receipt.as_recorded().copied().ok_or_else(unexpected))
        .collect()
}

/// The clearing receipts of a batch of clearings.
pub(crate) fn cleared_all(receipts: Vec<core::BatchReceipt>) -> Result<Vec<core::Cleared>, Error> {
    receipts
        .into_iter()
        .map(|receipt| receipt.as_cleared().copied().ok_or_else(unexpected))
        .collect()
}

/// The error for a receipt of the wrong kind: the core answered another
/// operation.
pub(crate) fn unexpected() -> Error {
    IntegrityError::new(IntegrityKind::ReplayMismatch, None).into()
}

impl<const P: u8> Done<P> {
    /// The handle of a registered account.
    pub(crate) fn into_account(self) -> Result<AccountHandle, Error> {
        match self {
            Self::Account(handle) => Ok(handle),
            _ => Err(unexpected()),
        }
    }

    /// The receipt of a recorded entry.
    pub(crate) fn into_recorded(self) -> Result<core::Recorded, Error> {
        match self {
            Self::Entry(recorded) => Ok(recorded),
            _ => Err(unexpected()),
        }
    }

    /// The receipts of a recorded batch, or its refusal.
    pub(crate) fn into_batch(self) -> Result<Vec<core::BatchReceipt>, Error> {
        match self {
            Self::Batch(receipts) => Ok(receipts),
            Self::BatchRejected { index, rejection } => {
                Err(Error::BatchRejected { index, rejection })
            }
            _ => Err(unexpected()),
        }
    }

    /// The receipt of a seal.
    pub(crate) fn into_sealed(self) -> Result<core::SealReceipt<P>, Error> {
        match self {
            Self::Sealed(receipt) => Ok(receipt),
            _ => Err(unexpected()),
        }
    }

    /// The receipt of a clearing record.
    pub(crate) fn into_cleared(self) -> Result<core::Cleared, Error> {
        match self {
            Self::Cleared(cleared) => Ok(cleared),
            _ => Err(unexpected()),
        }
    }

    /// The year-end closing drafts.
    pub(crate) fn into_closing(self) -> Result<Vec<Entry<P>>, Error> {
        match self {
            Self::Closing(drafts) => Ok(drafts),
            _ => Err(unexpected()),
        }
    }
}

/// Refuses a `size` beyond a log of `head` records.
pub(crate) fn within(size: u64, head: u64) -> Result<(), Error> {
    if size > head {
        Err(QueryError::PositionBeyondHead {
            position: size,
            size: head,
        }
        .into())
    } else {
        Ok(())
    }
}

/// An idempotency key from the bytes a lookup was given; `None` when they
/// name no key.
fn lookup_key(key: &[u8]) -> Option<IdempotencyKey> {
    IdempotencyKey::new(std::str::from_utf8(key).ok()?).ok()
}

fn missing(position: Option<u64>) -> Error {
    IntegrityError::new(IntegrityKind::MissingRecord, position).into()
}

fn undecodable(position: u64) -> Error {
    IntegrityError::new(IntegrityKind::Undecodable, Some(position)).into()
}

/// An account's master data in force: what
/// [`Journal::account`](crate::Journal::account) and
/// [`Ledger::account`](crate::Ledger::account) return. The latest version of
/// each field — the window, limit and class every later entry is checked
/// against.
///
/// ```
/// use doubleentry::{AccountKind, AccountSpec, BalanceLimit, Journal, LedgerId, date};
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.register(
///     AccountSpec::new("Liabilities:Wallet", AccountKind::Liability, date!("2026-01-01"))
///         .limit(BalanceLimit::NoDebitBalance),
/// )?;
/// books.close_account("Liabilities:Wallet", date!("2026-12-31"))?;
/// let wallet = books.account("Liabilities:Wallet").expect("registered");
/// assert_eq!(wallet.limit, BalanceLimit::NoDebitBalance);
/// assert_eq!((wallet.open_from, wallet.open_to), (date!("2026-01-01"), Some(date!("2026-12-31"))));
/// # Ok::<(), doubleentry::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Account {
    /// Its handle.
    pub handle: AccountHandle,
    /// Its path.
    pub path: AccountPath,
    /// Its kind.
    pub kind: AccountKind,
    /// Its class, if it has one.
    pub class: Option<Label>,
    /// The first day it can be posted to.
    pub open_from: Date,
    /// The last day it can be posted to (`None`: open-ended).
    pub open_to: Option<Date>,
    /// Its balance limit.
    pub limit: BalanceLimit,
}

impl Account {
    /// The account after one more of its records, in position order: its
    /// registration first, then each change (`None` for a change without a
    /// registration).
    fn apply(account: Option<Self>, action: AccountAction) -> Option<Self> {
        match action {
            AccountAction::Register(registration) => Some(Self {
                handle: registration.handle,
                path: registration.path,
                kind: registration.kind,
                class: registration.class,
                open_from: registration.open_from,
                open_to: registration.open_to,
                limit: registration.limit,
            }),
            AccountAction::Change { change, .. } => {
                let mut account = account?;
                match change {
                    AccountChange::OpenTo(open_to) => account.open_to = open_to,
                    AccountChange::Limit(limit) => account.limit = limit,
                    AccountChange::Class(class) => account.class = class,
                }
                Some(account)
            }
        }
    }
}

/// A period of the calendar as the façades list it: its id, its first and
/// last day, the position of its definition and, once sealed, its seal.
///
/// ```
/// use doubleentry::{Journal, LedgerId, date};
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.define_period("FY2026-Q1", date!("2026-01-01"), date!("2026-03-31"))?;
/// books.define_period("FY2026-Q2", date!("2026-04-01"), date!("2026-06-30"))?;
/// books.seal_period("FY2026-Q1")?;
///
/// let periods = books.periods()?;
/// assert_eq!(periods.len(), 2);
/// assert!(periods[0].is_sealed() && !periods[1].is_sealed());
/// assert_eq!(books.watermark()?, Some(date!("2026-03-31")));
/// assert_eq!(books.seals()?[0].end(), date!("2026-03-31"));
/// # Ok::<(), doubleentry::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Period<const P: u8 = 2> {
    /// The period's id, as defined (`2026-03`, `FY2026-Q1`, …).
    pub id: PeriodId,
    /// Its first day.
    pub start: Date,
    /// Its last day.
    pub end: Date,
    /// The position of its definition record.
    pub position: u64,
    /// Its seal, once sealed.
    pub seal: Option<core::SealReceipt<P>>,
}

impl<const P: u8> Period<P> {
    /// Whether the period is sealed: no posting may be booked in it.
    #[must_use]
    pub const fn is_sealed(&self) -> bool {
        self.seal.is_some()
    }
}

/// One line, `2026-03 2026-03-01..=2026-03-31` (and `, sealed at position
/// 40`), honouring width, fill and alignment.
impl<const P: u8> std::fmt::Display for Period<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sealed = self
            .seal
            .as_ref()
            .map(|seal| format!(", sealed at position {}", seal.position))
            .unwrap_or_default();
        let text = format!("{} {}..={}{sealed}", self.id, self.start, self.end);
        crate::model::pad(f, &text)
    }
}

/// Every read of the façades, over a store.
pub(crate) struct Reads<'a, S, const P: u8> {
    source: S,
    ledger: &'a LedgerId,
}

impl<'a, S: Store<P>, const P: u8> Reads<'a, S, P> {
    /// The reads of ledger `ledger` kept in `source`.
    pub(crate) const fn new(source: S, ledger: &'a LedgerId) -> Self {
        Self { source, ledger }
    }

    // ── Lookups ─────────────────────────────────────────────────────────────

    /// The draft that reverses entry `id`.
    pub(crate) async fn reversal(
        &self,
        id: EntryId,
        key: &str,
        on: Date,
    ) -> Result<Entry<P>, Error> {
        let original = self
            .entry(id)
            .await?
            .ok_or(Error::Query(QueryError::UnknownEntry { id }))?;
        Ok(core::reversal(id, &original, key, on))
    }

    /// The entry with id `id`.
    pub(crate) async fn entry(&self, id: EntryId) -> Result<Option<EntryRecord<P>>, Error> {
        self.stored_entry(EntryRef::Id(id), None).await
    }

    /// The entry recorded under the key `key` spells.
    pub(crate) async fn entry_by_key(&self, key: &[u8]) -> Result<Option<EntryRecord<P>>, Error> {
        let Some(key) = lookup_key(key) else {
            return Ok(None);
        };
        self.stored_entry(EntryRef::Key(&key), None).await
    }

    /// The entry `by` names, recorded below `through`, decoded — verified
    /// before it is trusted.
    async fn stored_entry(
        &self,
        by: EntryRef<'_>,
        through: Option<u64>,
    ) -> Result<Option<EntryRecord<P>>, Error> {
        match self.source.entry(by, through).await? {
            Some(stored) => Ok(Some(self.verified(stored, by).await?.1)),
            None => Ok(None),
        }
    }

    /// A stored entry checked before it is handed on: it is the
    /// entry `by` names, its id derives from its key and from the key its
    /// leaf commits to, and its leaf bytes hash to the leaf hash stored in
    /// the tree at its position. Returns the position and the record.
    async fn verified(
        &self,
        stored: StoredEntry,
        by: EntryRef<'_>,
    ) -> Result<(u64, EntryRecord<P>), Error> {
        let position = stored.position;
        let mismatch = || -> Error {
            IntegrityError::new(IntegrityKind::ReplayMismatch, Some(position)).into()
        };
        let named = match by {
            EntryRef::Id(id) => stored.id == id,
            EntryRef::Key(key) => stored.key == *key,
        };
        if !named
            || stored.leaf.position != position
            || EntryId::derive(self.ledger, &stored.key) != stored.id
        {
            return Err(mismatch());
        }
        let Record::Entry(record) = stored.leaf.record::<P>()? else {
            return Err(undecodable(position));
        };
        if record.parts().key != stored.key {
            return Err(mismatch());
        }
        self.leaf_is_stored(position, &stored.leaf.bytes).await?;
        Ok((position, record))
    }

    /// Checks that `bytes` hash to the leaf hash the tree stores at
    /// `position`: one node read.
    async fn leaf_is_stored(&self, position: u64, bytes: &[u8]) -> Result<(), Error> {
        let node = stored_index(0, position).map_err(|_| missing(Some(position)))?;
        let stored = self.source.read_nodes(&[node]).await?;
        if stored.first() == Some(&leaf_hash(bytes)) {
            Ok(())
        } else {
            Err(IntegrityError::new(IntegrityKind::NodeMismatch, Some(position)).into())
        }
    }

    /// The master data in force of `account`, folded from its records.
    pub(crate) async fn account(&self, account: AccountRef) -> Result<Option<Account>, Error> {
        let Some(Some(row)) = self
            .source
            .accounts(std::slice::from_ref(&account))
            .await?
            .pop()
        else {
            return Ok(None);
        };
        let named = match &account {
            AccountRef::Path(path) => row.path == *path,
            AccountRef::Handle(handle) => row.handle == *handle,
        };
        if !named {
            return Err(
                IntegrityError::new(IntegrityKind::ReplayMismatch, Some(row.position)).into(),
            );
        }
        let mut found = None;
        let mut page = PageRead::first(PageRead::<()>::MAX_LIMIT);
        loop {
            let read = self
                .source
                .account_records(row.handle, page.clone())
                .await?;
            for leaf in read.items {
                let position = leaf.position;
                found = match leaf.record::<P>()? {
                    Record::Account(event) => Account::apply(found, event.action),
                    _ => None,
                };
                if found.is_none() {
                    return Err(
                        IntegrityError::new(IntegrityKind::ReplayMismatch, Some(position)).into(),
                    );
                }
            }
            match read.next {
                Some(next) => page = page.after(next),
                None => break,
            }
        }
        // The account the records fold to is the one the index names.
        match &found {
            Some(folded) if folded.handle == row.handle && folded.path == row.path => Ok(found),
            _ => Err(IntegrityError::new(IntegrityKind::ReplayMismatch, Some(row.position)).into()),
        }
    }

    /// The memo text of entry `id`.
    pub(crate) async fn memo(&self, id: EntryId) -> Result<Option<String>, Error> {
        Ok(match self.source.memo(id).await? {
            Some(Memo::Kept { text, .. }) => Some(text),
            _ => None,
        })
    }

    // ── Heads and proofs ────────────────────────────────────────────────────

    /// The tree head now.
    pub(crate) async fn head(&self) -> Result<TreeHead, Error> {
        let size = self.source.size().await?;
        Ok(head_at(&self.source, size).await?)
    }

    /// The tree head at `size` records.
    pub(crate) async fn head_at(&self, size: u64) -> Result<TreeHead, Error> {
        within(size, self.source.size().await?)?;
        Ok(head_at(&self.source, size).await?)
    }

    /// The inclusion proof of entry `id` at `size` records (`None`: the
    /// size now).
    pub(crate) async fn inclusion(
        &self,
        id: EntryId,
        size: Option<u64>,
    ) -> Result<InclusionProof, Error> {
        let head = self.source.size().await?;
        let size = size.unwrap_or(head);
        #[cfg(feature = "tracing")]
        crate::trace::proved(size);
        within(size, head)?;
        let stored = self
            .source
            .entry(EntryRef::Id(id), Some(size))
            .await?
            .ok_or(Error::Query(QueryError::UnknownEntry { id }))?;
        let leaf = leaf_hash(&stored.leaf.bytes);
        let (position, _) = self.verified(stored, EntryRef::Id(id)).await?;
        if position >= size {
            return Err(Error::Query(QueryError::UnknownEntry { id }));
        }
        let proof = self.inclusion_at(position, size).await?;
        // Verified against the head the store's nodes give before it is
        // handed out: a tampered stored node is an integrity error here.
        proof
            .verify(&leaf, &head_at(&self.source, size).await?)
            .map_err(|_| IntegrityError::new(IntegrityKind::NodeMismatch, Some(position)))?;
        Ok(proof)
    }

    /// The inclusion proof of `position` at `size`, from stored nodes.
    async fn inclusion_at(&self, position: u64, size: u64) -> Result<InclusionProof, Error> {
        let plan = InclusionPlan::new(position, size).map_err(|_| missing(Some(position)))?;
        let hashes = self.source.read_nodes(plan.ids()).await?;
        plan.assemble(&hashes).map_err(|_| missing(Some(position)))
    }

    /// The consistency proof from `old_size` to `new_size`.
    pub(crate) async fn consistency(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, Error> {
        #[cfg(feature = "tracing")]
        crate::trace::proved(new_size);
        within(new_size, self.source.size().await?)?;
        if old_size == 0 || old_size > new_size {
            return Err(QueryError::InvalidSizes {
                old: old_size,
                new: new_size,
            }
            .into());
        }
        let plan = ConsistencyPlan::new(old_size, new_size).map_err(|_| missing(None))?;
        let hashes = self.source.read_nodes(plan.ids()).await?;
        let proof = plan.assemble(&hashes).map_err(|_| missing(None))?;
        // Verified against the heads the store's nodes give before it is
        // handed out.
        let (old, new) = (
            head_at(&self.source, old_size).await?,
            head_at(&self.source, new_size).await?,
        );
        proof
            .verify(&old, &new)
            .map_err(|_| IntegrityError::new(IntegrityKind::NodeMismatch, None))?;
        Ok(proof)
    }

    // ── Holds ───────────────────────────────────────────────────────────────

    /// The hold opened by entry `id` and where it stands.
    pub(crate) async fn hold(&self, id: EntryId) -> Result<Option<HoldState<P>>, Error> {
        let page = self
            .source
            .holds(&HoldRead::ById { id, through: None })
            .await?;
        let Some(row) = page.items.into_iter().next() else {
            return Ok(None);
        };
        // The row must be the hold its entry opened.
        let stored = self
            .source
            .entry(EntryRef::Id(id), None)
            .await?
            .ok_or_else(|| IntegrityError::new(IntegrityKind::HoldMismatch, Some(row.position)))?;
        let (position, record) = self.verified(stored, EntryRef::Id(id)).await?;
        let opened = HoldView::open(id, position, &record).map(|view| view.row());
        let matches = opened.is_some_and(|opened| {
            (
                row.id,
                row.position,
                row.booking,
                row.expires,
                row.debit,
                row.credit,
            ) == (
                opened.id,
                opened.position,
                opened.booking,
                opened.expires,
                opened.debit,
                opened.credit,
            ) && (row.currency, row.amount) == (opened.currency, opened.amount)
        });
        if !matches {
            return Err(
                IntegrityError::new(IntegrityKind::HoldMismatch, Some(row.position)).into(),
            );
        }
        let posted = match row.resolution {
            Some((by, ResolutionKind::Posted)) => {
                let action = self
                    .stored_entry(EntryRef::Id(by), None)
                    .await?
                    .and_then(|record| record.parts().hold);
                match action {
                    Some(HoldAction::Post { hold, amount }) if hold == id => {
                        core::posted(row.amount, amount)
                    }
                    _ => return Err(IntegrityError::new(IntegrityKind::HoldMismatch, None).into()),
                }
            }
            _ => row.amount,
        };
        Ok(Some(HoldState::from_row(&row, posted)))
    }

    /// The open holds that may be expired on `as_of`, one page at a time.
    pub(crate) async fn holds_due(
        &self,
        as_of: Date,
        page: PageRead<HoldCursor>,
    ) -> Result<Page<HoldState<P>, HoldCursor>, Error> {
        let page = PageRead {
            limit: page.limit.clamp(1, PageRead::<()>::MAX_LIMIT),
            ..page
        };
        let due = self
            .source
            .holds(&HoldRead::Due {
                on_or_before: as_of,
                page,
            })
            .await?;
        Ok(Page {
            items: due
                .items
                .iter()
                .map(|row| HoldState::from_row(row, row.amount))
                .collect(),
            next: due.next,
            size: due.size,
            through: due.through,
        })
    }

    // ── Reports ─────────────────────────────────────────────────────────────

    /// The balance of `account` in `currency` the query selects.
    pub(crate) async fn balance(
        &self,
        account: &AccountName,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Balance<P>, Error> {
        let selection = self.selection(query).await?;
        let (key, _) = self.key(account, currency, &selection).await?;
        self.key_balance(key, &selection).await
    }

    /// The trial balance the query selects.
    pub(crate) async fn trial_balance(
        &self,
        query: &BalanceQuery,
    ) -> Result<TrialBalance<P>, Error> {
        let selection = self.selection(query).await?;
        let accounts = self.account_book(selection.bound()).await?;
        let layer = |(key, _): &(BalanceKey, Balance<P>)| key.layer == selection.layer();
        let balances = match selection.plan() {
            Plan::Maintained | Plan::AtPosition(_) => {
                let mut balances = self.balances(None, &selection).await?;
                balances.retain(layer);
                balances
            }
            Plan::Fold => {
                let (mut keys, _) = self.balance_pages(BalanceScope::Current, None).await?;
                keys.retain(layer);
                let mut balances = Vec::with_capacity(keys.len());
                for (key, _) in keys {
                    balances.push((key, self.fold_balance(key, &selection).await?));
                }
                balances
            }
        };
        let exponents = self.exponents(selection.bound()).await?;
        Ok(TrialBalance::of(
            &selection, balances, &accounts, exponents,
        )?)
    }

    /// The statement of `account` in `currency` over the query's window.
    pub(crate) async fn statement(
        &self,
        account: &AccountName,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Statement<P>, Error> {
        let selection = self.selection(query).await?;
        let (key, row) = self.key(account, currency, &selection).await?;
        let mut fold = StatementFold::new(&selection);
        let mut after = None;
        loop {
            let page = self
                .source
                .postings(&selection.posting_read(key, after))
                .await?;
            for line in page.items {
                fold.feed(line)?;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => {
                    let exponents = self.exponents(selection.bound()).await?;
                    let exponent = exponents.get(&currency).copied().unwrap_or(P);
                    return Ok(fold.finish(row.handle, row.path, currency, exponent)?);
                }
            }
        }
    }

    /// The trial balance of `currency` folded up the account tree.
    pub(crate) async fn rollup(
        &self,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Rollup<P>, Error> {
        Ok(Rollup::of(&self.trial_balance(query).await?, currency)?)
    }

    /// The open items of `account` in `currency` the query selects.
    pub(crate) async fn open_items(
        &self,
        account: &AccountName,
        currency: Currency,
        query: &BalanceQuery,
    ) -> Result<Vec<OpenItem<P>>, Error> {
        let selection = self.selection(query).await?;
        let (key, _) = self.key(account, currency, &selection).await?;
        let mut fold = OpenItemsFold::new(&selection);
        let mut after = None;
        loop {
            let page = self
                .source
                .postings(&selection.posting_read(key, after))
                .await?;
            for line in page.items {
                fold.feed(line);
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let mut page = PageRead::first(PageRead::<()>::MAX_LIMIT).through(selection.bound());
        loop {
            let read = self.source.clearings(key, page.clone()).await?;
            for leaf in read.items {
                match leaf.record::<P>()? {
                    Record::Clearing(event) => fold.clearing(leaf.position, &event),
                    _ => {
                        return Err(IntegrityError::new(
                            IntegrityKind::ReplayMismatch,
                            Some(leaf.position),
                        )
                        .into());
                    }
                }
            }
            match read.next {
                Some(next) => page = page.after(next),
                None => return Ok(fold.finish()?),
            }
        }
    }

    /// Checks one balance assertion.
    pub(crate) async fn check_assertion(
        &self,
        assertion: &BalanceAssertion<P>,
    ) -> Result<AssertionOutcome<P>, Error> {
        let selection = self.selection(&assertion.query()).await?;
        let (key, _) = self
            .key(assertion.name(), assertion.currency(), &selection)
            .await?;
        Ok(assertion.check(&self.key_balance(key, &selection).await?))
    }

    /// Checks every assertion, in their order.
    pub(crate) async fn check_assertions(
        &self,
        assertions: &[BalanceAssertion<P>],
    ) -> Result<Vec<AssertionOutcome<P>>, Error> {
        let mut outcomes = Vec::with_capacity(assertions.len());
        for assertion in assertions {
            outcomes.push(self.check_assertion(assertion).await?);
        }
        Ok(outcomes)
    }

    /// Checks that the log has `head` at its size.
    pub(crate) async fn pinned(&self, head: TreeHead) -> Result<(), Error> {
        if self.head_at(head.size).await? == head {
            Ok(())
        } else {
            Err(IntegrityError::new(IntegrityKind::HeadMismatch, None).into())
        }
    }

    /// Each currency's declared exponent below `bound`: the latest
    /// declaration (an exponent is only ever raised).
    pub(crate) async fn exponents(&self, bound: u64) -> Result<BTreeMap<Currency, u8>, Error> {
        let mut exponents = BTreeMap::new();
        let mut from = 0;
        loop {
            let page = self.records(from, bound, RecordKind::Currency).await?;
            for leaf in page.items {
                if let Record::Currency(declaration) = leaf.record::<P>()? {
                    exponents.insert(declaration.code, declaration.exponent);
                }
            }
            match page.next {
                Some(next) => from = next,
                None => return Ok(exponents),
            }
        }
    }

    /// The log size a calendar read sees: the pinned head's (checked
    /// against the store's root), or the store's size now.
    async fn calendar_bound(&self, head: Option<TreeHead>) -> Result<u64, Error> {
        match head {
            Some(head) => {
                self.pinned(head).await?;
                Ok(head.size)
            }
            None => Ok(self.source.size().await?),
        }
    }

    /// Every period defined below the bound, in calendar order, each with
    /// its seal when that was recorded below the bound.
    pub(crate) async fn periods(&self, head: Option<TreeHead>) -> Result<Vec<Period<P>>, Error> {
        let bound = self.calendar_bound(head).await?;
        let mut seals = self.seal_map(bound).await?;
        let mut periods = Vec::new();
        let mut from = 0;
        loop {
            let page = self.records(from, bound, RecordKind::Period).await?;
            for leaf in page.items {
                if let Record::Period(period) = leaf.record::<P>()? {
                    periods.push(Period {
                        id: period.id().clone(),
                        start: period.start(),
                        end: period.end(),
                        position: leaf.position,
                        seal: seals.remove(period.id()),
                    });
                }
            }
            match page.next {
                Some(next) => from = next,
                None => return Ok(periods),
            }
        }
    }

    /// Every seal recorded below the bound, in order.
    pub(crate) async fn seals(
        &self,
        head: Option<TreeHead>,
    ) -> Result<Vec<core::SealReceipt<P>>, Error> {
        let bound = self.calendar_bound(head).await?;
        let mut seals: Vec<_> = self.seal_map(bound).await?.into_values().collect();
        seals.sort_by_key(|seal| seal.position);
        Ok(seals)
    }

    /// The last day of the last seal below the bound: nothing may be booked
    /// on or before it (309, 301).
    pub(crate) async fn watermark(&self, head: Option<TreeHead>) -> Result<Option<Date>, Error> {
        Ok(self.seals(head).await?.last().map(core::SealReceipt::end))
    }

    /// The seals recorded below `bound`, by period.
    async fn seal_map(
        &self,
        bound: u64,
    ) -> Result<BTreeMap<PeriodId, core::SealReceipt<P>>, Error> {
        self.source
            .seals(Some(bound))
            .await?
            .into_iter()
            .filter(|row| row.position < bound)
            .map(|row| {
                let seal = seal_record::<P>(&row.leaf.bytes, row.position)?;
                Ok((
                    row.period.clone(),
                    core::SealReceipt {
                        period: row.period,
                        position: row.position,
                        seal,
                    },
                ))
            })
            .collect()
    }

    /// The (first, last) day of period `id`, when it was defined below
    /// `bound`.
    pub(crate) async fn period(
        &self,
        id: &PeriodId,
        bound: u64,
    ) -> Result<Option<(Date, Date)>, Error> {
        let mut from = 0;
        loop {
            let page = self.records(from, bound, RecordKind::Period).await?;
            for leaf in page.items {
                if let Record::Period(period) = leaf.record::<P>()?
                    && period.id() == id
                {
                    return Ok(Some((period.start(), period.end())));
                }
            }
            match page.next {
                Some(next) => from = next,
                None => return Ok(None),
            }
        }
    }

    /// One page of the records of `kind` from `from` below `bound`.
    async fn records(
        &self,
        from: u64,
        bound: u64,
        kind: RecordKind,
    ) -> Result<Page<crate::store::Leaf, u64>, Error> {
        Ok(self
            .source
            .read_leaves(&LeafRead {
                from,
                through: Some(bound),
                kinds: Some(vec![kind]),
                limit: PageRead::<()>::MAX_LIMIT,
            })
            .await?)
    }

    /// `query` resolved against the log prefix it reads.
    async fn selection(&self, query: &BalanceQuery) -> Result<Selection, Error> {
        let prepared = query.prepare(self.source.size().await?)?;
        if let Some(pin) = prepared.pin() {
            self.pinned(pin).await?;
        }
        let period = match prepared.period() {
            Some(id) => self.period(id, prepared.bound()).await?,
            None => None,
        };
        Ok(prepared.select(period)?)
    }

    /// The row of the account `name` names.
    async fn row(&self, name: &AccountName) -> Result<Option<AccountRow>, Error> {
        Ok(match name.reference() {
            Some(reference) => self
                .source
                .accounts(std::slice::from_ref(reference))
                .await?
                .pop()
                .flatten(),
            None => None,
        })
    }

    /// The balance `name` names in `currency`, in the selection's layer, and
    /// its account.
    async fn key(
        &self,
        name: &AccountName,
        currency: Currency,
        selection: &Selection,
    ) -> Result<(BalanceKey, AccountRow), Error> {
        let found = self.row(name).await?;
        Ok(name.key(found, selection.bound(), currency, selection.layer())?)
    }

    /// The balance of `key` the selection reads, by its plan.
    async fn key_balance(
        &self,
        key: BalanceKey,
        selection: &Selection,
    ) -> Result<Balance<P>, Error> {
        match selection.plan() {
            Plan::Maintained | Plan::AtPosition(_) => Ok(self
                .balances(Some(vec![key]), selection)
                .await?
                .pop()
                .map_or(Balance::ZERO, |(_, balance)| balance)),
            Plan::Fold => self.fold_balance(key, selection).await,
        }
    }

    /// The balances of `keys` (every non-zero one: `None`) at the
    /// selection's prefix: the maintained ones when they were read at it,
    /// else those after its last record.
    async fn balances(
        &self,
        keys: Option<Vec<BalanceKey>>,
        selection: &Selection,
    ) -> Result<Vec<(BalanceKey, Balance<P>)>, Error> {
        let bound = selection.bound();
        if selection.plan() == Plan::Maintained {
            let (balances, size) = self
                .balance_pages(BalanceScope::Current, keys.clone())
                .await?;
            if size == Some(bound) {
                return Ok(balances);
            }
        }
        let (balances, _) = self.balance_pages(BalanceScope::At(bound), keys).await?;
        Ok(balances)
    }

    /// Every page of one balance read, and the log size every page saw
    /// (`None` when they differ).
    async fn balance_pages(
        &self,
        scope: BalanceScope,
        keys: Option<Vec<BalanceKey>>,
    ) -> Result<(Vec<(BalanceKey, Balance<P>)>, Option<u64>), Error> {
        let mut read = BalanceRead {
            scope,
            keys,
            page: PageRead::first(PageRead::<()>::MAX_LIMIT),
        };
        let mut balances = Vec::new();
        let mut size = None;
        let mut first = true;
        loop {
            let page = self.source.balances(&read).await?;
            size = if first || size == Some(page.size) {
                Some(page.size)
            } else {
                None
            };
            first = false;
            balances.extend(page.items);
            match page.next {
                Some(next) => read.page = read.page.after(next),
                None => return Ok((balances, size)),
            }
        }
    }

    /// The balance of the postings of `key` the selection selects, folded
    /// from its posting rows.
    async fn fold_balance(
        &self,
        key: BalanceKey,
        selection: &Selection,
    ) -> Result<Balance<P>, Error> {
        let mut fold = BalanceFold::new(selection);
        let mut after = None;
        loop {
            let page = self
                .source
                .postings(&selection.posting_read(key, after))
                .await?;
            for line in &page.items {
                fold.feed(line)?;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(fold.finish()),
            }
        }
    }

    /// The accounts registered below `bound`, from their records.
    async fn account_book(&self, bound: u64) -> Result<AccountBook, Error> {
        let mut book = AccountBook::default();
        let mut from = 0;
        loop {
            let page = self.records(from, bound, RecordKind::Account).await?;
            for leaf in page.items {
                match leaf.record::<P>()? {
                    Record::Account(event) => book.feed(&event),
                    _ => return Err(undecodable(leaf.position)),
                }
            }
            match page.next {
                Some(next) => from = next,
                None => return Ok(book),
            }
        }
    }

    // ── Transparency ────────────────────────────────────────────────────────

    /// The proof of `account`'s sealed balance for `period`, against the
    /// head now.
    pub(crate) async fn prove_balance(
        &self,
        account: &AccountName,
        currency: Currency,
        layer: Layer,
        period: &str,
    ) -> Result<SealedBalanceProof<P>, Error> {
        let head = self.head().await?;
        #[cfg(feature = "tracing")]
        crate::trace::proved(head.size);
        self.sealed_balance(account, currency, layer, period, head)
            .await
    }

    /// A `tlog-proof` bundle of the record at `position`.
    pub(crate) async fn bundle_record(
        &self,
        position: u64,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        let head = self.checkpoint_head(checkpoint).await?;
        self.record_bundle(position, head, checkpoint).await
    }

    /// A `tlog-proof` bundle of entry `id`.
    pub(crate) async fn bundle_entry(
        &self,
        id: EntryId,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        let head = self.checkpoint_head(checkpoint).await?;
        let stored = self
            .source
            .entry(EntryRef::Id(id), Some(head.size))
            .await?
            .ok_or(Error::Query(QueryError::UnknownEntry { id }))?;
        let (position, _) = self.verified(stored, EntryRef::Id(id)).await?;
        self.record_bundle(position, head, checkpoint).await
    }

    /// A `tlog-proof` bundle of `account`'s sealed balance.
    pub(crate) async fn bundle_balance(
        &self,
        account: &AccountName,
        currency: Currency,
        layer: Layer,
        period: &str,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        let head = self.checkpoint_head(checkpoint).await?;
        let proof = self
            .sealed_balance(account, currency, layer, period, head)
            .await?;
        Ok(balance_bundle(proof, checkpoint.clone())?)
    }

    /// The bytes of hash tile `id`, from stored nodes.
    pub(crate) async fn tile(&self, id: TileId) -> Result<Vec<u8>, Error> {
        let size = self.source.size().await?;
        if id.needs_size() > size {
            return Err(tile_beyond(&id, size));
        }
        let ids = id.node_ids(size)?;
        let hashes = self.source.read_nodes(&ids).await?;
        if hashes.len() != ids.len() {
            return Err(missing(None));
        }
        Ok(tile_bytes(&hashes))
    }

    /// The bytes of entry bundle `id`.
    pub(crate) async fn entry_bundle(&self, id: TileId) -> Result<Vec<u8>, Error> {
        let size = self.source.size().await?;
        if id.needs_size() > size {
            return Err(tile_beyond(&id, size));
        }
        let range = id.leaf_range(size)?;
        let page = self
            .source
            .read_leaves(&LeafRead {
                from: range.start,
                through: Some(range.end),
                kinds: None,
                limit: u32::from(id.width()),
            })
            .await?;
        if !page.items.iter().map(|leaf| leaf.position).eq(range) {
            return Err(missing(None));
        }
        Ok(entry_bundle(
            page.items
                .iter()
                .map(|leaf| (leaf.position, leaf.bytes.as_slice())),
        )?)
    }

    /// The head `checkpoint` claims, once it is this ledger's at a size it
    /// has reached, with the same root.
    async fn checkpoint_head(&self, checkpoint: &SignedCheckpoint) -> Result<TreeHead, Error> {
        let claimed = claimed_head(self.ledger, checkpoint, self.source.size().await?)?;
        same_root(claimed, head_at(&self.source, claimed.size).await?)?;
        #[cfg(feature = "tracing")]
        crate::trace::proved(claimed.size);
        Ok(claimed)
    }

    /// The leaf bytes at `position`.
    async fn leaf_bytes(&self, position: u64) -> Result<Vec<u8>, Error> {
        let page = self
            .source
            .read_leaves(&LeafRead {
                from: position,
                through: position.checked_add(1),
                kinds: None,
                limit: 1,
            })
            .await?;
        page.items
            .into_iter()
            .find(|leaf| leaf.position == position)
            .map(|leaf| leaf.bytes)
            .ok_or_else(|| missing(Some(position)))
    }

    /// The bundle of the record at `position` at `head`.
    async fn record_bundle(
        &self,
        position: u64,
        head: TreeHead,
        checkpoint: &SignedCheckpoint,
    ) -> Result<ProofBundle<P>, Error> {
        if position >= head.size {
            return Err(QueryError::PositionBeyondHead {
                position,
                size: head.size,
            }
            .into());
        }
        let leaf = self.leaf_bytes(position).await?;
        let proof = self.inclusion_at(position, head.size).await?;
        proof
            .verify(&leaf_hash(&leaf), &head)
            .map_err(|_| IntegrityError::new(IntegrityKind::NodeMismatch, Some(position)))?;
        Ok(ProofBundle::new(leaf, proof, checkpoint.clone())?)
    }

    /// The sealed-balance proof of `account` at `head`.
    async fn sealed_balance(
        &self,
        account: &AccountName,
        currency: Currency,
        layer: Layer,
        period: &str,
        head: TreeHead,
    ) -> Result<SealedBalanceProof<P>, Error> {
        let id = period_label(period)?;
        if self.period(&id, head.size).await?.is_none() {
            return Err(QueryError::UnknownPeriod {
                id: period.to_owned(),
            }
            .into());
        }
        let row = self
            .source
            .seals(Some(head.size))
            .await?
            .into_iter()
            .find(|row| row.period == id && row.position < head.size)
            .ok_or_else(|| QueryError::NotSealed {
                period: period.to_owned(),
            })?;
        let seal = seal_record::<P>(&row.leaf.bytes, row.position)?;
        let parts = seal.parts();
        let account_row = self
            .row(account)
            .await?
            .filter(|found| found.position <= parts.through && found.handle.get() < parts.accounts)
            .ok_or_else(|| QueryError::UnknownAccount {
                account: account.text().to_owned(),
            })?;
        let sealed = SealedParts {
            seal_leaf: row.leaf.bytes.clone(),
            seal_proof: self.inclusion_at(row.position, head.size).await?,
            account_leaf: self.leaf_bytes(account_row.position).await?,
            account_proof: self.inclusion_at(account_row.position, head.size).await?,
            handle: account_row.handle,
            currency,
            layer,
        };
        if seal.tree(currency, layer).is_none() {
            return sealed_balance_proof(sealed, &seal, Vec::new(), &head);
        }
        // The account's leaf and the O(log² n) stored nodes its proof reads,
        // never every leaf; the proof is verified against the seal and the
        // head before it is handed out.
        let mismatch = || -> Error {
            IntegrityError::new(IntegrityKind::SealMismatch, Some(row.position)).into()
        };
        let key = BalanceKey {
            handle: sealed.handle,
            currency,
            layer,
        };
        let balance = self
            .source
            .balances(&BalanceRead {
                scope: BalanceScope::Sealed(id.clone()),
                keys: Some(vec![key]),
                page: PageRead::first(1),
            })
            .await?
            .items
            .pop()
            .filter(|(found, _)| *found == key)
            .map(|(_, balance)| balance)
            .ok_or_else(mismatch)?;
        let ids = proof_nodes(parts.accounts, sealed.handle);
        let Some(hashes) = self.source.seal_nodes(&id, currency, layer, &ids).await? else {
            // A store serving no seal nodes: the tree is rebuilt from its
            // leaves.
            let leaves: Vec<_> = self
                .balance_pages(BalanceScope::Sealed(id), None)
                .await?
                .0
                .into_iter()
                .filter(|(key, _)| key.currency == currency && key.layer == layer)
                .map(|(key, balance)| (key.handle, balance))
                .collect();
            return sealed_balance_proof(sealed, &seal, leaves, &head);
        };
        if hashes.len() != ids.len() {
            return Err(mismatch());
        }
        let nodes = ids
            .into_iter()
            .zip(hashes)
            .filter_map(|(id, hash)| Some((id, hash?)));
        let tree = BalanceTree::from_nodes(
            currency,
            layer,
            parts.accounts,
            (sealed.handle, balance),
            nodes,
        );
        let proof = SealedBalanceProof {
            seal_leaf: sealed.seal_leaf,
            seal_proof: sealed.seal_proof,
            account_leaf: sealed.account_leaf,
            account_proof: sealed.account_proof,
            currency,
            layer,
            handle: sealed.handle,
            balance: Some(tree.prove(sealed.handle).map_err(|_| mismatch())?),
        };
        proof.verify(&head).map_err(|_| mismatch())?;
        Ok(proof)
    }
}
