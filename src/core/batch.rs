//! Atomic batches: entries, clearings and clearing resets decided in order,
//! each against the state its predecessors leave, all or nothing.
//!
//! [`batch`] runs the single-write rule ([`check`], [`clearing::apply`],
//! [`clearing::reset`]) for every item over a [`Pending`] view — the ledger
//! as the store has it plus the write sets of the items already decided —
//! so a store persists nothing until the whole batch is decided, and a
//! refusal of any item leaves the ledger untouched. An item whose key is
//! already recorded (by an earlier write or an earlier item of the same
//! batch) replays exactly as a single write would, so recording an
//! identical batch twice returns the original receipts.
//!
//! A clearing may name a posting of an entry decided earlier in the batch,
//! and its open amount accounts for the clearings and resets before it.
//!
//! On a [`Snapshot`](super::Snapshot) still missing facts, a refused item
//! does not end the run: the run is a plan, and the later items' reads are
//! discovered in the same round.

use std::collections::BTreeMap;

use time::Date;

use super::check::{BatchItem, check};
use super::clearing;
use super::holds::{HoldEffect, HoldView};
use super::state::{
    AccountView, BalanceKey, ClearingView, EntryMeta, EntryView, Header, Movement, PeriodView,
    Slice, StateView, Terms, Versioned,
};
use super::writeset::{BatchReceipt, Done, IndexRow, Outcome, Outstanding, WriteSet};
use crate::error::Rejection;
use crate::model::{
    AccountHandle, AccountPath, Amount, Balance, Currency, EntryId, IdempotencyKey, LedgerPolicy,
    PeriodId, PostingRef, Record,
};

/// Decides `items` in order against `view`: `Batch` with one write set per
/// new record and every receipt (replays included), or
/// `Unchanged(Done::BatchRejected)` naming the first refused item.
pub(crate) fn batch<const P: u8>(view: &impl StateView<P>, items: &[BatchItem<P>]) -> Outcome<P> {
    let mut pending = Pending::new(view);
    let mut writes = Vec::new();
    let mut receipts = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match decide(&pending, item) {
            Ok(Outcome::Append(write)) => {
                if let Some(receipt) = receipt(&write.done) {
                    receipts.push(receipt);
                }
                pending.absorb(&write);
                writes.push(*write);
            }
            Ok(outcome) => {
                if let Some(receipt) = receipt(outcome.done()) {
                    receipts.push(receipt);
                }
            }
            // A plan: the refusal may stem from a fact not loaded yet.
            Err(_) if !view.complete() => {}
            Err(rejection) => {
                return Outcome::Unchanged(Done::BatchRejected { index, rejection });
            }
        }
    }
    let done = Done::Batch(receipts);
    if writes.is_empty() {
        Outcome::Unchanged(done)
    } else {
        Outcome::Batch { writes, done }
    }
}

/// One item, decided by the rule a single write of it runs.
fn decide<const P: u8>(
    view: &impl StateView<P>,
    item: &BatchItem<P>,
) -> Result<Outcome<P>, Rejection> {
    match item {
        BatchItem::Record(draft) => check(view, draft),
        BatchItem::Clear(clearing) => clearing::apply(
            view,
            &clearing.key,
            clearing.debit,
            clearing.credit,
            clearing.amount,
            clearing.on,
        ),
        BatchItem::Reset { key, clearing, on } => clearing::reset(view, key, *clearing, *on),
    }
}

/// An item's receipt.
const fn receipt<const P: u8>(done: &Done<P>) -> Option<BatchReceipt> {
    match done {
        Done::Entry(recorded) => Some(BatchReceipt::Recorded(*recorded)),
        Done::Cleared(cleared) => Some(BatchReceipt::Cleared(*cleared)),
        _ => None,
    }
}

/// A view and the effects of the entries a batch has decided so far.
struct Pending<'a, V, const P: u8> {
    base: &'a V,
    header: Header<P>,
    keys: BTreeMap<IdempotencyKey, EntryMeta>,
    entries: BTreeMap<EntryId, EntryView<P>>,
    reversals: BTreeMap<EntryId, EntryId>,
    balances: BTreeMap<BalanceKey, Balance<P>>,
    booked: BTreeMap<AccountHandle, Date>,
    holds: BTreeMap<EntryId, HoldView<P>>,
    outstanding: BTreeMap<(AccountHandle, Currency), Outstanding<P>>,
    clearing_keys: BTreeMap<IdempotencyKey, u64>,
    clearings: BTreeMap<u64, ClearingView<P>>,
    applied: BTreeMap<PostingRef, Amount<P>>,
}

impl<'a, V: StateView<P>, const P: u8> Pending<'a, V, P> {
    fn new(base: &'a V) -> Self {
        Self {
            base,
            header: base.header(),
            keys: BTreeMap::new(),
            entries: BTreeMap::new(),
            reversals: BTreeMap::new(),
            balances: BTreeMap::new(),
            booked: BTreeMap::new(),
            holds: BTreeMap::new(),
            outstanding: BTreeMap::new(),
            clearing_keys: BTreeMap::new(),
            clearings: BTreeMap::new(),
            applied: BTreeMap::new(),
        }
    }

    /// Takes in the effects of an entry's write set.
    fn absorb(&mut self, write: &WriteSet<P>) {
        self.header.size = write.position.saturating_add(1);
        for row in &write.index {
            match row {
                IndexRow::EntryKey {
                    key,
                    id,
                    position,
                    fingerprint,
                } => {
                    self.header.entries = self.header.entries.saturating_add(1);
                    self.keys.insert(
                        key.clone(),
                        EntryMeta {
                            id: *id,
                            position: *position,
                            fingerprint: *fingerprint,
                        },
                    );
                    if let Record::Entry(record) = &write.record {
                        self.entries.insert(
                            *id,
                            EntryView {
                                position: *position,
                                record: record.clone(),
                            },
                        );
                    }
                }
                IndexRow::Reversal { target, by } => {
                    self.reversals.insert(*target, *by);
                }
                IndexRow::ClearingKey { key, position } => {
                    self.clearing_keys.insert(key.clone(), *position);
                    if let Record::Clearing(event) = &write.record {
                        self.clearings.insert(
                            *position,
                            ClearingView {
                                position: *position,
                                event: event.clone(),
                                reset_by: None,
                            },
                        );
                    }
                }
                IndexRow::Applied { posting, delta } => {
                    // The core checked the open amount when it decided.
                    let applied = self.applied(*posting);
                    if let Ok(sum) = applied.checked_add(*delta) {
                        self.applied.insert(*posting, sum);
                    }
                }
                IndexRow::ClearingReset { clearing, position } => {
                    if let Some(mut view) = self.clearing_at(*clearing) {
                        view.reset_by = Some(*position);
                        self.clearings.insert(*clearing, view);
                    }
                }
                _ => {}
            }
        }
        for delta in &write.balances {
            // The core checked the sum when it decided the write set.
            if let Ok(balance) = self.balance(delta.key).checked_add(&delta.delta) {
                self.balances.insert(delta.key, balance);
            }
        }
        for row in &write.postings {
            self.header.last_booking = self.header.last_booking.max(Some(row.booking));
            let booked = self.booked.entry(row.key.handle).or_insert(row.booking);
            *booked = (*booked).max(row.booking);
        }
        match &write.hold {
            Some(HoldEffect::Opened(view)) => {
                self.holds.insert(view.id, view.clone());
            }
            Some(HoldEffect::Resolved { hold, by, how }) => {
                if let Some(mut view) = self.hold(*hold) {
                    view.resolution = Some((*by, *how));
                    self.holds.insert(*hold, view);
                }
            }
            None => {}
        }
        for row in &write.outstanding {
            self.outstanding.insert((row.handle, row.currency), *row);
        }
    }

    /// An account as the batch leaves it: its last booking date moved by
    /// the batch's postings.
    fn booked(&self, view: Option<AccountView>) -> Option<AccountView> {
        view.map(|mut view| {
            if let Some(date) = self.booked.get(&view.registration.handle) {
                view.last_booked = view.last_booked.max(Some(*date));
            }
            view
        })
    }

    fn outstanding(&self, handle: AccountHandle, currency: Currency) -> Option<&Outstanding<P>> {
        self.outstanding.get(&(handle, currency))
    }
}

impl<V: StateView<P>, const P: u8> StateView<P> for Pending<'_, V, P> {
    fn header(&self) -> Header<P> {
        self.header.clone()
    }

    fn entry_by_key(&self, key: &IdempotencyKey) -> Option<EntryMeta> {
        self.keys
            .get(key)
            .copied()
            .or_else(|| self.base.entry_by_key(key))
    }

    fn account_by_path(&self, path: &AccountPath) -> Option<AccountView> {
        self.booked(self.base.account_by_path(path))
    }

    fn account(&self, handle: AccountHandle) -> Option<AccountView> {
        self.booked(self.base.account(handle))
    }

    fn account_terms(&self, handle: AccountHandle, on: Date) -> Option<Terms> {
        self.base.account_terms(handle, on)
    }

    fn currency(&self, code: Currency, on: Date) -> Option<u8> {
        self.base.currency(code, on)
    }

    fn currency_latest(&self, code: Currency) -> Option<Versioned<u8>> {
        self.base.currency_latest(code)
    }

    fn policy(&self, on: Date) -> LedgerPolicy {
        self.base.policy(on)
    }

    fn policy_latest(&self) -> Option<Versioned<LedgerPolicy>> {
        self.base.policy_latest()
    }

    fn period_at(&self, date: Date) -> Option<PeriodView<P>> {
        self.base.period_at(date)
    }

    fn period(&self, id: &PeriodId) -> Option<PeriodView<P>> {
        self.base.period(id)
    }

    fn first_unsealed(&self) -> Option<PeriodView<P>> {
        self.base.first_unsealed()
    }

    fn entry(&self, id: EntryId) -> Option<EntryView<P>> {
        match self.entries.get(&id) {
            Some(view) => Some(view.clone()),
            None => self.base.entry(id),
        }
    }

    fn reversed_by(&self, id: EntryId) -> Option<EntryId> {
        self.reversals
            .get(&id)
            .copied()
            .or_else(|| self.base.reversed_by(id))
    }

    fn hold(&self, id: EntryId) -> Option<HoldView<P>> {
        match self.holds.get(&id) {
            Some(view) => Some(view.clone()),
            None => self.base.hold(id),
        }
    }

    fn outstanding_hold_debits(&self, handle: AccountHandle, currency: Currency) -> Amount<P> {
        self.outstanding(handle, currency).map_or_else(
            || self.base.outstanding_hold_debits(handle, currency),
            |row| row.debits,
        )
    }

    fn outstanding_hold_credits(&self, handle: AccountHandle, currency: Currency) -> Amount<P> {
        self.outstanding(handle, currency).map_or_else(
            || self.base.outstanding_hold_credits(handle, currency),
            |row| row.credits,
        )
    }

    fn balance(&self, key: BalanceKey) -> Balance<P> {
        match self.balances.get(&key) {
            Some(balance) => *balance,
            None => self.base.balance(key),
        }
    }

    fn clearing_by_key(&self, key: &IdempotencyKey) -> Option<ClearingView<P>> {
        match self.clearing_keys.get(key) {
            Some(position) => self.clearing_at(*position),
            None => self.base.clearing_by_key(key),
        }
    }

    fn clearing_at(&self, position: u64) -> Option<ClearingView<P>> {
        // A clearing of this batch, or a committed one an item of it reset.
        match self.clearings.get(&position) {
            Some(view) => Some(view.clone()),
            None => self.base.clearing_at(position),
        }
    }

    fn applied(&self, posting: PostingRef) -> Amount<P> {
        match self.applied.get(&posting) {
            Some(applied) => *applied,
            None => self.base.applied(posting),
        }
    }

    // A batch holds entries and clearings: none of them reads what sealing
    // and year-end closing read.

    fn all_balances(&self) -> Vec<(BalanceKey, Balance<P>)> {
        let mut all: BTreeMap<_, _> = self.base.all_balances().into_iter().collect();
        all.extend(self.balances.iter().map(|(key, balance)| (*key, *balance)));
        all.into_iter()
            .filter(|(_, balance)| !balance.is_zero())
            .collect()
    }

    fn postings_after(&self, date: Date) -> Vec<Movement<P>> {
        self.base.postings_after(date)
    }

    fn pnl_slices(&self, through: Date) -> Vec<Slice<P>> {
        self.base.pnl_slices(through)
    }

    fn complete(&self) -> bool {
        self.base.complete()
    }
}
