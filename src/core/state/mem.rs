//! [`MemState`]: the whole ledger in ordered maps — the reference
//! implementation of applying a [`WriteSet`].

mod rows;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use time::Date;

use super::{
    AccountView, BalanceKey, ClearingView, EntryMeta, EntryView, Header, LatestTerms, Movement,
    PeriodView, Slice, StateView, Terms, Versioned, Window,
};
use crate::core::audit::ReplayLeaf;
use crate::core::holds::{HoldEffect, HoldView};
use crate::core::writeset::{AccountTerm, IndexRow, MemoPlain, Outstanding, SealReceipt, WriteSet};
use crate::log::{BalanceTree, Tree, TreeHead, leaf_hash};
use crate::model::{
    AccountHandle, AccountKind, AccountPath, AccountRegistration, Amount, Balance, BalanceLimit,
    Currency, Dimensions, EntryId, Fingerprint, IdempotencyKey, Label, Layer, LedgerId,
    LedgerPolicy, PeriodId, PostingRef, Record, Side,
};
use crate::store::{AccountRow, Erasure, PostingLine};

/// Why a [`WriteSet`] cannot be applied. Nothing was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyError {
    /// The write set was decided at another position.
    Stale,
    /// The write set does not fit this state (a balance or cleared amount
    /// would leave its range, or the log is full): it was decided against
    /// another state.
    Inconsistent,
}

/// The whole ledger in memory: the log, its side data and every index, in
/// ordered maps only.
#[derive(Debug, Clone)]
pub(crate) struct MemState<const P: u8> {
    ledger: LedgerId,
    tree: Tree,
    leaves: Vec<StoredLeaf>,
    entries: u64,
    keys: BTreeMap<IdempotencyKey, EntryMeta>,
    records: BTreeMap<EntryId, EntryView<P>>,
    reversals: BTreeMap<EntryId, EntryId>,
    accounts: Vec<AccountState>,
    paths: BTreeMap<AccountPath, AccountHandle>,
    currencies: BTreeMap<Currency, Vec<Versioned<u8>>>,
    policies: Vec<Versioned<LedgerPolicy>>,
    periods: BTreeMap<Date, PeriodView<P>>,
    period_starts: BTreeMap<PeriodId, Date>,
    watermark: Option<Date>,
    balances: BTreeMap<BalanceKey, Balance<P>>,
    postings: BTreeMap<(Date, u64, u16), Posted<P>>,
    clearings: BTreeMap<u64, ClearingView<P>>,
    clearing_keys: BTreeMap<IdempotencyKey, u64>,
    applied: BTreeMap<PostingRef, Amount<P>>,
    lines: BTreeMap<(BalanceKey, u64, u16), Line<P>>,
    clearing_refs: BTreeSet<(BalanceKey, u64)>,
    holds: BTreeMap<EntryId, HoldView<P>>,
    due: BTreeMap<(Date, u64), EntryId>,
    open_holds: BTreeMap<(AccountHandle, u64), EntryId>,
    outstanding: BTreeMap<(AccountHandle, Currency), Outstanding<P>>,
    by_booking: BTreeSet<(BalanceKey, Date, u64, u16)>,
    seal_trees: BTreeMap<(PeriodId, Currency, Layer), BalanceTree<P>>,
}

/// One posting as a statement reads it, keyed elsewhere by (balance,
/// position, index): what the reads of `Journal` and `MemoryStore` page.
#[derive(Debug, Clone)]
pub(crate) struct Line<const P: u8> {
    /// The entry's id.
    pub(crate) entry: EntryId,
    /// The side.
    pub(crate) side: Side,
    /// The amount.
    pub(crate) amount: Amount<P>,
    /// The booking date.
    pub(crate) booking: Date,
    /// The value date.
    pub(crate) value: Date,
    /// The dimensions.
    pub(crate) dimensions: Dimensions,
    /// The balance's gross totals after this posting.
    pub(crate) balance_after: Balance<P>,
}

/// A leaf and the side data kept beside it.
#[derive(Debug, Clone)]
struct StoredLeaf {
    bytes: Vec<u8>,
    memo: Option<MemoPlain>,
    fingerprint: Option<Fingerprint>,
}

/// One account's registration and field versions.
#[derive(Debug, Clone)]
struct AccountState {
    registration: AccountRegistration,
    position: u64,
    /// The positions of its records: the registration, then each change.
    records: Vec<u64>,
    windows: Vec<Versioned<Window>>,
    limits: Vec<Versioned<BalanceLimit>>,
    classes: Vec<Versioned<Option<Label>>>,
    is_leaf: bool,
    last_booked: Option<Date>,
}

/// One posting, keyed elsewhere by (booking date, position, index).
#[derive(Debug, Clone)]
struct Posted<const P: u8> {
    key: BalanceKey,
    side: Side,
    amount: Amount<P>,
    dimensions: Dimensions,
}

/// The version in force on `on`: the last one whose effective date is on or
/// before it (effective dates never decrease within one field).
fn in_force<T: Clone>(versions: &[Versioned<T>], on: Date) -> Option<T> {
    let count = versions.partition_point(|version| version.effective <= on);
    versions
        .get(count.checked_sub(1)?)
        .map(|version| version.value.clone())
}

impl AccountState {
    fn view(&self) -> Option<AccountView> {
        Some(AccountView {
            registration: self.registration.clone(),
            latest: LatestTerms {
                window: self.windows.last()?.clone(),
                limit: self.limits.last()?.clone(),
                class: self.classes.last()?.clone(),
            },
            is_leaf: self.is_leaf,
            last_booked: self.last_booked,
        })
    }
}

impl<const P: u8> MemState<P> {
    /// The empty state of a ledger: no record yet, not even its genesis.
    pub(crate) const fn new(ledger: LedgerId) -> Self {
        Self {
            ledger,
            tree: Tree::new(),
            leaves: Vec::new(),
            entries: 0,
            keys: BTreeMap::new(),
            records: BTreeMap::new(),
            reversals: BTreeMap::new(),
            accounts: Vec::new(),
            paths: BTreeMap::new(),
            currencies: BTreeMap::new(),
            policies: Vec::new(),
            periods: BTreeMap::new(),
            period_starts: BTreeMap::new(),
            watermark: None,
            balances: BTreeMap::new(),
            postings: BTreeMap::new(),
            clearings: BTreeMap::new(),
            clearing_keys: BTreeMap::new(),
            applied: BTreeMap::new(),
            lines: BTreeMap::new(),
            clearing_refs: BTreeSet::new(),
            holds: BTreeMap::new(),
            due: BTreeMap::new(),
            open_holds: BTreeMap::new(),
            outstanding: BTreeMap::new(),
            by_booking: BTreeSet::new(),
            seal_trees: BTreeMap::new(),
        }
    }

    /// The ledger's id.
    pub(crate) const fn ledger(&self) -> &LedgerId {
        &self.ledger
    }

    /// The tree head.
    pub(crate) fn head(&self) -> TreeHead {
        self.tree.head()
    }

    /// Every leaf with its side data, in position order.
    pub(crate) fn leaves(&self) -> impl Iterator<Item = ReplayLeaf<'_>> {
        (0u64..)
            .zip(&self.leaves)
            .map(|(position, leaf)| ReplayLeaf {
                position,
                bytes: &leaf.bytes,
                memo: leaf.memo.as_ref(),
                fingerprint: leaf.fingerprint,
            })
    }

    /// The leaf at `position` with its side data.
    pub(crate) fn leaf(&self, position: u64) -> Option<ReplayLeaf<'_>> {
        let leaf = self.leaves.get(usize::try_from(position).ok()?)?;
        Some(ReplayLeaf {
            position,
            bytes: &leaf.bytes,
            memo: leaf.memo.as_ref(),
            fingerprint: leaf.fingerprint,
        })
    }

    /// The log's tree.
    pub(crate) const fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Erases the memo of entry `id`: its text and salt, and the fingerprint
    /// of the draft it was recorded from (a later submission of its key is a
    /// key conflict, so a guessed memo cannot be confirmed). No leaf, hash
    /// or balance changes.
    pub(crate) fn erase_memo(&mut self, id: EntryId) -> Erasure {
        let Some(view) = self.records.get(&id) else {
            return Erasure::UnknownEntry;
        };
        if view.record.parts().memo.is_none() {
            return Erasure::NoMemo;
        }
        let key = view.record.parts().key.clone();
        let Some(leaf) = usize::try_from(view.position)
            .ok()
            .and_then(|index| self.leaves.get_mut(index))
        else {
            return Erasure::UnknownEntry;
        };
        if leaf.memo.take().is_none() {
            return Erasure::AlreadyErased;
        }
        leaf.fingerprint = None;
        if let Some(meta) = self.keys.get_mut(&key) {
            meta.fingerprint = None;
        }
        Erasure::Erased
    }

    /// The balance a recorded posting moved.
    fn posting_key(&self, posting: PostingRef) -> Option<BalanceKey> {
        let record = &self.records.get(&posting.entry)?.record;
        let row = record.parts().postings.get(usize::from(posting.index))?;
        Some(BalanceKey {
            handle: row.handle,
            currency: row.currency,
            layer: row.layer,
        })
    }

    /// The postings of `key` at positions in `positions`, in log order.
    pub(crate) fn lines(
        &self,
        key: BalanceKey,
        positions: core::ops::Range<u64>,
    ) -> impl DoubleEndedIterator<Item = ((u64, u16), &Line<P>)> {
        let (start, end) = (positions.start, positions.end.max(positions.start));
        self.lines
            .range((key, start, 0)..(key, end, 0))
            .map(|((_, position, index), line)| ((*position, *index), line))
    }

    /// One posting of `key`.
    pub(crate) fn line(&self, key: BalanceKey, position: u64, index: u16) -> Option<&Line<P>> {
        self.lines.get(&(key, position, index))
    }

    /// A posting as the store reads return it: with the amount cleared
    /// against it now, net of resets.
    pub(crate) fn posting_line(
        &self,
        (position, index): (u64, u16),
        line: &Line<P>,
    ) -> PostingLine<P> {
        PostingLine {
            position,
            index,
            entry: line.entry,
            side: line.side,
            amount: line.amount,
            booking: line.booking,
            value: line.value,
            dimensions: line.dimensions.clone(),
            balance_after: line.balance_after,
            cleared: StateView::applied(self, PostingRef::at(line.entry, index)),
        }
    }

    /// The balance of `key` after every record below `position`.
    pub(crate) fn balance_at(&self, key: BalanceKey, position: u64) -> Balance<P> {
        self.lines(key, 0..position)
            .next_back()
            .map_or(Balance::ZERO, |(_, line)| line.balance_after)
    }

    /// The positions of the clearing records (applies and resets) touching
    /// a posting of `key`, among `positions`, ascending.
    pub(crate) fn clearing_refs(
        &self,
        key: BalanceKey,
        positions: core::ops::Range<u64>,
    ) -> impl Iterator<Item = u64> {
        let (start, end) = (positions.start, positions.end.max(positions.start));
        self.clearing_refs
            .range((key, start)..(key, end))
            .map(|(_, position)| *position)
    }

    /// The open holds expiring on or before `on_or_before`, by (expiry,
    /// position), after the cursor `after`.
    pub(crate) fn holds_due(
        &self,
        on_or_before: Date,
        after: Option<(Date, u64)>,
    ) -> impl Iterator<Item = &HoldView<P>> {
        let start = after.map_or(Bound::Unbounded, Bound::Excluded);
        self.due
            .range((start, Bound::Included((on_or_before, u64::MAX))))
            .filter_map(|(_, id)| self.holds.get(id))
    }

    /// Every hold of the hold index, open and resolved, by position.
    #[cfg(feature = "testing")]
    pub(crate) fn hold_rows(&self) -> Vec<crate::store::HoldRow<P>> {
        let mut rows: Vec<crate::store::HoldRow<P>> =
            self.holds.values().map(HoldView::row).collect();
        rows.sort_by_key(|row| row.position);
        rows
    }

    /// The open holds debiting or crediting `handle`, by position, after
    /// position `after`.
    pub(crate) fn open_holds_of(
        &self,
        handle: AccountHandle,
        after: Option<u64>,
    ) -> impl Iterator<Item = &HoldView<P>> {
        let start = after.map_or(Bound::Included((handle, 0)), |position| {
            Bound::Excluded((handle, position))
        });
        self.open_holds
            .range((start, Bound::Included((handle, u64::MAX))))
            .filter_map(|(_, id)| self.holds.get(id))
    }

    /// A registered account as the account index holds it.
    pub(crate) fn account_row(&self, handle: AccountHandle) -> Option<AccountRow> {
        let account = self.account_state(handle)?;
        Some(AccountRow {
            handle: account.registration.handle,
            path: account.registration.path.clone(),
            kind: account.registration.kind,
            position: account.position,
        })
    }

    /// The handle registered under `path`.
    pub(crate) fn handle_of(&self, path: &AccountPath) -> Option<AccountHandle> {
        self.paths.get(path).copied()
    }

    /// The positions of an account's records, ascending.
    pub(crate) fn account_records(&self, handle: AccountHandle) -> &[u64] {
        self.account_state(handle)
            .map_or(&[], |account| account.records.as_slice())
    }

    /// Every seal's (position, period), ascending by position.
    pub(crate) fn seal_positions(&self) -> Vec<(u64, PeriodId)> {
        let mut seals: Vec<(u64, PeriodId)> = self
            .periods
            .values()
            .filter_map(|view| Some((view.sealed.as_ref()?.position, view.id.clone())))
            .collect();
        seals.sort();
        seals
    }

    /// The leaves of the balance trees sealed for `period`, ascending by
    /// (currency, layer, handle).
    pub(crate) fn seal_leaves(
        &self,
        period: &PeriodId,
    ) -> impl Iterator<Item = (Currency, Layer, AccountHandle, Balance<P>)> {
        self.seal_trees
            .iter()
            .filter(move |((sealed, ..), _)| sealed == period)
            .flat_map(|((_, currency, layer), tree)| {
                tree.leaves().filter_map(move |(index, balance)| {
                    Some((*currency, *layer, AccountHandle::new(index)?, balance))
                })
            })
    }

    /// A sealed balance-tree leaf of `period`; `None` when it is zero.
    pub(crate) fn seal_leaf(&self, period: &PeriodId, key: BalanceKey) -> Option<Balance<P>> {
        self.seal_trees
            .get(&(period.clone(), key.currency, key.layer))
            .map(|tree| tree.leaf(key.handle))
            .filter(|balance| !balance.is_zero())
    }

    /// The sealed balance tree of `period` for (currency, layer).
    pub(crate) fn seal_tree(
        &self,
        period: &PeriodId,
        currency: Currency,
        layer: Layer,
    ) -> Option<&BalanceTree<P>> {
        self.seal_trees.get(&(period.clone(), currency, layer))
    }

    /// The position of the last entry with postings.
    #[cfg(feature = "testing")]
    pub(crate) fn last_posting(&self) -> Option<u64> {
        self.by_booking
            .iter()
            .map(|(.., position, _)| *position)
            .max()
    }

    /// The (booking date, position, index) of a balance's postings, in
    /// booking order.
    pub(crate) fn by_booking(
        &self,
        key: BalanceKey,
    ) -> impl Iterator<Item = (Date, u64, u16)> + '_ {
        self.by_booking
            .range((key, Date::MIN, 0, 0)..=(key, Date::MAX, u64::MAX, u16::MAX))
            .map(|(_, booking, position, index)| (*booking, *position, *index))
    }

    /// Applies a write set decided against this state: completely, or —
    /// with an error — not at all.
    pub(crate) fn apply(&mut self, write: WriteSet<P>) -> Result<(), ApplyError> {
        let size = self.tree.size();
        if write.position != size {
            return Err(ApplyError::Stale);
        }
        // Every check before the first mutation.
        let balances = write
            .balances
            .iter()
            .map(|delta| {
                self.balance(delta.key)
                    .checked_add(&delta.delta)
                    .map(|balance| (delta.key, balance))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ApplyError::Inconsistent)?;
        if let Some(HoldEffect::Resolved { hold, .. }) = &write.hold
            && self
                .holds
                .get(hold)
                .is_none_or(|view| view.resolution.is_some())
        {
            return Err(ApplyError::Inconsistent);
        }
        let mut applied = Vec::new();
        for row in &write.index {
            if let IndexRow::Applied { posting, delta } = row {
                let total = StateView::applied(self, *posting)
                    .checked_add(*delta)
                    .map_err(|_| ApplyError::Inconsistent)?;
                applied.push((*posting, total));
            }
        }
        self.tree
            .push(leaf_hash(&write.leaf))
            .map_err(|_| ApplyError::Inconsistent)?;

        let fingerprint = write.index.iter().find_map(|row| match row {
            IndexRow::EntryKey { fingerprint, .. } => *fingerprint,
            _ => None,
        });
        let entry = write.index.iter().find_map(|row| match row {
            IndexRow::EntryKey { id, .. } => Some(*id),
            _ => None,
        });
        // The balances a clearing record touches: those of the postings it
        // changes the cleared amount of (an apply's two, a reset's two).
        let cleared_keys: Vec<BalanceKey> = write
            .index
            .iter()
            .filter_map(|row| match row {
                IndexRow::Applied { posting, .. } => self.posting_key(*posting),
                _ => None,
            })
            .collect();
        for (key, balance) in balances {
            self.balances.insert(key, balance);
        }
        // A posting once cleared keeps its row, zero after a reset — as a
        // store's `cleared` table does.
        for (posting, total) in applied {
            self.applied.insert(posting, total);
        }
        for key in cleared_keys {
            self.clearing_refs.insert((key, write.position));
        }
        for row in write.postings {
            if let Some(account) = self.account_mut(row.key.handle) {
                account.last_booked = account.last_booked.max(Some(row.booking));
            }
            if let Some(entry) = entry {
                self.by_booking
                    .insert((row.key, row.booking, write.position, row.index));
                self.lines.insert(
                    (row.key, write.position, row.index),
                    Line {
                        entry,
                        side: row.side,
                        amount: row.amount,
                        booking: row.booking,
                        value: row.value,
                        dimensions: row.dimensions.clone(),
                        balance_after: row.balance_after,
                    },
                );
            }
            self.postings.insert(
                (row.booking, write.position, row.index),
                Posted {
                    key: row.key,
                    side: row.side,
                    amount: row.amount,
                    dimensions: row.dimensions,
                },
            );
        }
        for row in write.index {
            self.index(row, &write.record);
        }
        if let Some(effect) = write.hold {
            self.hold_index(effect);
        }
        // A reserved amount keeps its row once written, zero when the
        // holds are resolved — as a store's `outstanding` table does.
        for row in write.outstanding {
            self.outstanding.insert((row.handle, row.currency), row);
        }
        self.leaves.push(StoredLeaf {
            bytes: write.leaf,
            memo: write.memo,
            fingerprint,
        });
        Ok(())
    }

    /// Maintains the hold index (the resolved hold exists and is open:
    /// checked before the first mutation).
    fn hold_index(&mut self, effect: HoldEffect<P>) {
        match effect {
            HoldEffect::Opened(view) => {
                if let Some(expires) = view.expires {
                    self.due.insert((expires, view.position), view.id);
                }
                for leg in &view.legs {
                    self.open_holds.insert((leg.handle, view.position), view.id);
                }
                self.holds.insert(view.id, view);
            }
            HoldEffect::Resolved { hold, by, how } => {
                if let Some(view) = self.holds.get_mut(&hold) {
                    view.resolution = Some((by, how));
                    if let Some(expires) = view.expires {
                        self.due.remove(&(expires, view.position));
                    }
                    for leg in &view.legs {
                        self.open_holds.remove(&(leg.handle, view.position));
                    }
                }
            }
        }
    }

    fn account_mut(&mut self, handle: AccountHandle) -> Option<&mut AccountState> {
        self.accounts.get_mut(usize::try_from(handle.get()).ok()?)
    }

    fn account_state(&self, handle: AccountHandle) -> Option<&AccountState> {
        self.accounts.get(usize::try_from(handle.get()).ok()?)
    }

    /// Maintains one index row; `record` is the record the row belongs to.
    fn index(&mut self, row: IndexRow<P>, record: &Record<P>) {
        match row {
            IndexRow::EntryKey {
                key,
                id,
                position,
                fingerprint,
            } => {
                self.keys.insert(
                    key,
                    EntryMeta {
                        id,
                        position,
                        fingerprint,
                    },
                );
                if let Record::Entry(entry) = record {
                    self.records.insert(
                        id,
                        EntryView {
                            position,
                            record: entry.clone(),
                        },
                    );
                }
                self.entries = self.entries.saturating_add(1);
            }
            IndexRow::Reversal { target, by } => {
                self.reversals.insert(target, by);
            }
            IndexRow::Account {
                registration,
                position,
            } => self.register(registration, position),
            IndexRow::AccountVersion {
                handle,
                term,
                effective,
                position,
            } => {
                if let Some(account) = self.account_mut(handle) {
                    if account.records.last() != Some(&position) {
                        account.records.push(position);
                    }
                    match term {
                        AccountTerm::Window(value) => account.windows.push(Versioned {
                            value,
                            effective,
                            position,
                        }),
                        AccountTerm::Limit(value) => account.limits.push(Versioned {
                            value,
                            effective,
                            position,
                        }),
                        AccountTerm::Class(value) => account.classes.push(Versioned {
                            value,
                            effective,
                            position,
                        }),
                    }
                }
            }
            IndexRow::CurrencyVersion {
                code,
                exponent,
                effective,
                position,
            } => self.currencies.entry(code).or_default().push(Versioned {
                value: exponent,
                effective,
                position,
            }),
            IndexRow::PolicyVersion {
                policy,
                effective,
                position,
            } => self.policies.push(Versioned {
                value: policy,
                effective,
                position,
            }),
            IndexRow::Period {
                id,
                start,
                end,
                position,
            } => {
                self.period_starts.insert(id.clone(), start);
                self.periods.insert(
                    start,
                    PeriodView {
                        id,
                        start,
                        end,
                        position,
                        sealed: None,
                    },
                );
            }
            IndexRow::Sealed { period, position } => {
                let start = self.period_starts.get(&period).copied();
                if let (Some(view), Record::Seal(seal)) =
                    (start.and_then(|start| self.periods.get_mut(&start)), record)
                {
                    self.watermark = Some(view.end);
                    view.sealed = Some(SealReceipt {
                        period,
                        position,
                        seal: seal.clone(),
                    });
                }
            }
            IndexRow::SealTree {
                period,
                currency,
                layer,
                leaves,
            } => {
                if let Record::Seal(seal) = record {
                    let mut tree = BalanceTree::new(currency, layer, seal.parts().accounts);
                    for (handle, balance) in leaves {
                        // The core builds the leaves below the seal's account
                        // count; a handle beyond it cannot be kept.
                        let _ = tree.set(handle, balance);
                    }
                    self.seal_trees.insert((period, currency, layer), tree);
                }
            }
            IndexRow::ClearingKey { key, position } => {
                self.clearing_keys.insert(key, position);
                if let Record::Clearing(event) = record {
                    self.clearings.insert(
                        position,
                        ClearingView {
                            position,
                            event: event.clone(),
                            reset_by: None,
                        },
                    );
                }
            }
            // Applied amounts were computed and stored before the first
            // mutation.
            IndexRow::Applied { .. } => {}
            IndexRow::ClearingReset { clearing, position } => {
                if let Some(view) = self.clearings.get_mut(&clearing) {
                    view.reset_by = Some(position);
                }
            }
        }
    }

    /// Adds a registration: it is a leaf unless an account below it exists,
    /// and every registered account above it stops being one.
    fn register(&mut self, registration: AccountRegistration, position: u64) {
        let path = registration.path.clone();
        let has_descendant = self
            .paths
            .range((Bound::Excluded(&path), Bound::Unbounded))
            .next()
            .is_some_and(|(next, _)| path.is_ancestor_of(next));
        let mut ancestor = path.parent();
        while let Some(parent) = ancestor {
            if let Some(handle) = self.paths.get(&parent).copied()
                && let Some(account) = self.account_mut(handle)
            {
                account.is_leaf = false;
            }
            ancestor = parent.parent();
        }
        self.paths.insert(path, registration.handle);
        self.accounts.push(AccountState {
            registration,
            position,
            records: vec![position],
            windows: Vec::new(),
            limits: Vec::new(),
            classes: Vec::new(),
            is_leaf: !has_descendant,
            last_booked: None,
        });
    }
}

impl<const P: u8> StateView<P> for MemState<P> {
    fn header(&self) -> Header<P> {
        Header {
            ledger: self.ledger.clone(),
            size: self.tree.size(),
            accounts: u32::try_from(self.accounts.len()).unwrap_or(u32::MAX),
            entries: self.entries,
            watermark: self.watermark,
            last_booking: self
                .postings
                .last_key_value()
                .map(|((booking, ..), _)| *booking),
            last_period: self.periods.last_key_value().map(|(_, view)| view.clone()),
        }
    }

    fn entry_by_key(&self, key: &IdempotencyKey) -> Option<EntryMeta> {
        self.keys.get(key).copied()
    }

    fn account_by_path(&self, path: &AccountPath) -> Option<AccountView> {
        self.account(*self.paths.get(path)?)
    }

    fn account(&self, handle: AccountHandle) -> Option<AccountView> {
        self.account_state(handle)?.view()
    }

    fn account_terms(&self, handle: AccountHandle, on: Date) -> Option<Terms> {
        let account = self.account_state(handle)?;
        Some(Terms {
            class: in_force(&account.classes, on)?,
            window: in_force(&account.windows, on)?,
            limit: in_force(&account.limits, on)?,
        })
    }

    fn currency(&self, code: Currency, on: Date) -> Option<u8> {
        in_force(self.currencies.get(&code)?, on)
    }

    fn currency_latest(&self, code: Currency) -> Option<Versioned<u8>> {
        self.currencies.get(&code)?.last().cloned()
    }

    fn policy(&self, on: Date) -> LedgerPolicy {
        in_force(&self.policies, on).unwrap_or_default()
    }

    fn policy_latest(&self) -> Option<Versioned<LedgerPolicy>> {
        self.policies.last().cloned()
    }

    fn period_at(&self, date: Date) -> Option<PeriodView<P>> {
        let (_, view) = self.periods.range(..=date).next_back()?;
        (date <= view.end).then(|| view.clone())
    }

    fn period(&self, id: &PeriodId) -> Option<PeriodView<P>> {
        self.periods.get(self.period_starts.get(id)?).cloned()
    }

    fn first_unsealed(&self) -> Option<PeriodView<P>> {
        self.periods
            .values()
            .find(|view| view.sealed.is_none())
            .cloned()
    }

    fn entry(&self, id: EntryId) -> Option<EntryView<P>> {
        self.records.get(&id).cloned()
    }

    fn reversed_by(&self, id: EntryId) -> Option<EntryId> {
        self.reversals.get(&id).copied()
    }

    fn hold(&self, id: EntryId) -> Option<HoldView<P>> {
        self.holds.get(&id).cloned()
    }

    fn outstanding_hold_debits(&self, handle: AccountHandle, currency: Currency) -> Amount<P> {
        self.outstanding
            .get(&(handle, currency))
            .map_or(Amount::ZERO, |row| row.debits)
    }

    fn outstanding_hold_credits(&self, handle: AccountHandle, currency: Currency) -> Amount<P> {
        self.outstanding
            .get(&(handle, currency))
            .map_or(Amount::ZERO, |row| row.credits)
    }

    fn balance(&self, key: BalanceKey) -> Balance<P> {
        self.balances.get(&key).copied().unwrap_or(Balance::ZERO)
    }

    fn clearing_by_key(&self, key: &IdempotencyKey) -> Option<ClearingView<P>> {
        self.clearing_at(*self.clearing_keys.get(key)?)
    }

    fn clearing_at(&self, position: u64) -> Option<ClearingView<P>> {
        self.clearings.get(&position).cloned()
    }

    fn applied(&self, posting: PostingRef) -> Amount<P> {
        self.applied.get(&posting).copied().unwrap_or(Amount::ZERO)
    }

    fn all_balances(&self) -> Vec<(BalanceKey, Balance<P>)> {
        self.balances.iter().map(|(key, b)| (*key, *b)).collect()
    }

    fn postings_after(&self, date: Date) -> Vec<Movement<P>> {
        self.postings
            .range((
                Bound::Excluded((date, u64::MAX, u16::MAX)),
                Bound::Unbounded,
            ))
            .map(|(_, posted)| Movement {
                key: posted.key,
                side: posted.side,
                amount: posted.amount,
            })
            .collect()
    }

    fn pnl_slices(&self, through: Date) -> Vec<Slice<P>> {
        let mut slices: BTreeMap<(AccountHandle, Currency, &Dimensions), Balance<P>> =
            BTreeMap::new();
        for posted in self
            .postings
            .range(..=(through, u64::MAX, u16::MAX))
            .map(|(_, p)| p)
        {
            let in_scope = self
                .account_state(posted.key.handle)
                .is_some_and(|account| {
                    matches!(
                        account.registration.kind,
                        AccountKind::Income | AccountKind::Expense
                    )
                });
            if posted.key.layer != Layer::Settled || !in_scope {
                continue;
            }
            let slice = slices
                .entry((posted.key.handle, posted.key.currency, &posted.dimensions))
                .or_insert(Balance::ZERO);
            // A slice sums a subset of the postings of one maintained
            // balance, which fits, so the sum fits too.
            *slice = slice.post(posted.side, posted.amount).unwrap_or(*slice);
        }
        slices
            .into_iter()
            .map(|((handle, currency, dimensions), balance)| Slice {
                handle,
                currency,
                dimensions: dimensions.clone(),
                balance,
            })
            .collect()
    }
}
