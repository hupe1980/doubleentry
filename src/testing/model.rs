//! The suite's model: its random numbers ([`SplitMix64`]), an independent
//! oracle folded from the reference journal's leaves, and the store
//! invariants I1–I9, checked through `Store` reads only.

use std::collections::{BTreeMap, BTreeSet};

use time::Date;

use crate::PageRead;
use crate::core::BalanceKey;
use crate::core::ResolutionKind;
use crate::log::nodes::{ConsistencyPlan, InclusionPlan, RootPlan};
use crate::log::{Tree, TreeHead, leaf_hash};
use crate::model::{
    AccountAction, AccountHandle, Amount, Balance, ClearingAction, Currency, Dimensions, EntryId,
    EntryParts, HoldAction, IdempotencyKey, Layer, PeriodId, PostingRef, Record, RecordKind, Side,
};
use crate::store::{
    BalanceRead, BalanceScope, EntryRef, HoldRead, HoldRow, LeafRead, Memo, PostingCursor,
    PostingOrder, PostingRead, Store, StoreError,
};
use crate::{Journal, Ledger};

/// SplitMix64: the suite's only source of randomness, seeded per run.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator seeded with `seed`.
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ z.wrapping_shr(31)
    }

    /// A number in `0..bound` (0 for an empty range).
    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }

    /// A number in `low..=high`.
    pub(crate) fn between(&mut self, low: u64, high: u64) -> u64 {
        low.saturating_add(self.below(high.saturating_sub(low).saturating_add(1)))
    }

    /// `true` with `percent` % probability.
    pub(crate) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// A random index into a collection of `len` items.
    pub(crate) fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(u64::try_from(len).unwrap_or(u64::MAX))).unwrap_or(0)
    }

    /// A random item.
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        let index = self.index(items.len());
        items.get(index)
    }
}

/// A failed check: what was expected and what was found.
pub(crate) type Check = Result<(), String>;

/// A store failure while checking.
pub(crate) fn store_failed(error: &StoreError) -> String {
    format!("the store failed: {error}")
}

/// One posting as the oracle folds it from the reference's leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Posted<const P: u8> {
    pub(crate) position: u64,
    pub(crate) index: u16,
    pub(crate) entry: EntryId,
    pub(crate) key: BalanceKey,
    pub(crate) side: Side,
    pub(crate) amount: Amount<P>,
    pub(crate) booking: Date,
    pub(crate) value: Date,
    pub(crate) dimensions: Dimensions,
    pub(crate) balance_after: Balance<P>,
}

/// An entry as the oracle knows it.
#[derive(Debug, Clone)]
pub(crate) struct Known {
    pub(crate) position: u64,
    pub(crate) key: IdempotencyKey,
    pub(crate) memo: Memo,
}

/// Everything a store must answer, folded independently from the reference
/// journal's leaves and side data (not from its indexes).
#[derive(Debug, Clone, Default)]
pub(crate) struct Oracle<const P: u8> {
    pub(crate) size: u64,
    pub(crate) leaves: Vec<Vec<u8>>,
    pub(crate) postings: Vec<Posted<P>>,
    pub(crate) entries: BTreeMap<EntryId, Known>,
    pub(crate) cleared: BTreeMap<(u64, u16), Amount<P>>,
    pub(crate) clearing_refs: BTreeMap<BalanceKey, BTreeSet<u64>>,
    pub(crate) account_records: BTreeMap<AccountHandle, Vec<u64>>,
    pub(crate) periods: BTreeMap<PeriodId, (Date, Date)>,
    pub(crate) seals: Vec<(PeriodId, u64)>,
    /// Every hold as its index must hold it, and how many entries resolved
    /// it.
    pub(crate) holds: BTreeMap<EntryId, (HoldRow<P>, u32)>,
}

impl<const P: u8> Oracle<P> {
    /// Folds the reference's leaves.
    pub(crate) fn of(journal: &Journal<P>) -> Result<Self, String> {
        let ledger = journal.id().clone();
        let mut oracle = Self::default();
        let mut applies: BTreeMap<u64, (PostingRef, PostingRef, Amount<P>)> = BTreeMap::new();
        let mut balances: BTreeMap<BalanceKey, Balance<P>> = BTreeMap::new();
        for leaf in journal.leaves() {
            let position = leaf.position;
            let record = Record::<P>::decode(leaf.bytes).map_err(|error| {
                format!("the reference's leaf {position} does not decode: {error}")
            })?;
            oracle.leaves.push(leaf.bytes.to_vec());
            match record {
                Record::Account(event) => {
                    let handle = match event.action {
                        AccountAction::Register(registration) => registration.handle,
                        AccountAction::Change { handle, .. } => handle,
                    };
                    oracle
                        .account_records
                        .entry(handle)
                        .or_default()
                        .push(position);
                }
                Record::Period(period) => {
                    oracle
                        .periods
                        .insert(period.id().clone(), (period.start(), period.end()));
                }
                Record::Seal(seal) => oracle.seals.push((seal.parts().period.clone(), position)),
                Record::Entry(entry) => {
                    let parts = entry.parts();
                    let id = entry.id(&ledger);
                    oracle.hold(id, position, parts)?;
                    let memo = match (parts.memo, leaf.memo) {
                        (None, _) => Memo::None,
                        (Some(_), Some(memo)) => Memo::Kept {
                            text: memo.text.clone(),
                            salt: memo.salt,
                        },
                        (Some(_), None) => Memo::Erased,
                    };
                    oracle.entries.insert(
                        id,
                        Known {
                            position,
                            key: parts.key.clone(),
                            memo,
                        },
                    );
                    for (index, posting) in (0u16..).zip(&parts.postings) {
                        let key = BalanceKey {
                            handle: posting.handle,
                            currency: posting.currency,
                            layer: posting.layer,
                        };
                        let balance = balances.entry(key).or_insert(Balance::ZERO);
                        *balance = balance
                            .post(posting.side, posting.amount)
                            .map_err(|_| format!("a reference balance overflows at {position}"))?;
                        oracle.postings.push(Posted {
                            position,
                            index,
                            entry: id,
                            key,
                            side: posting.side,
                            amount: posting.amount,
                            booking: parts.booking_date,
                            value: parts.value_date,
                            dimensions: posting.dimensions.clone(),
                            balance_after: *balance,
                        });
                    }
                }
                Record::Clearing(event) => match event.action {
                    ClearingAction::Apply {
                        debit,
                        credit,
                        amount,
                        ..
                    } => {
                        oracle.clear(debit, amount, position)?;
                        oracle.clear(credit, amount, position)?;
                        applies.insert(position, (debit, credit, amount));
                    }
                    ClearingAction::Reset { clearing, .. } => {
                        let (debit, credit, amount) = applies
                            .get(&clearing)
                            .copied()
                            .ok_or_else(|| format!("a reset at {position} names no apply"))?;
                        oracle.clear(debit, amount.negate(), position)?;
                        oracle.clear(credit, amount.negate(), position)?;
                    }
                },
                Record::Genesis(_) | Record::Currency(_) | Record::Policy(_) => {}
            }
            oracle.size = position.saturating_add(1);
        }
        Ok(oracle)
    }

    /// Folds what entry `id` at `position` does to a hold.
    fn hold(&mut self, id: EntryId, position: u64, parts: &EntryParts<P>) -> Check {
        let (target, how) = match parts.hold {
            None => return Ok(()),
            Some(HoldAction::Open { expires }) => {
                let leg = |side: Side| {
                    parts
                        .postings
                        .iter()
                        .find(|posting| posting.side == side)
                        .ok_or_else(|| format!("the hold at {position} lacks a {side} leg"))
                };
                let (debit, credit) = (leg(Side::Debit)?, leg(Side::Credit)?);
                let row = HoldRow {
                    id,
                    position,
                    booking: parts.booking_date,
                    expires,
                    debit: debit.handle,
                    credit: credit.handle,
                    currency: debit.currency,
                    amount: debit.amount,
                    resolution: None,
                };
                self.holds.insert(id, (row, 0));
                return Ok(());
            }
            Some(HoldAction::Post { hold, .. }) => (hold, ResolutionKind::Posted),
            Some(HoldAction::Void { hold }) => (hold, ResolutionKind::Voided),
            Some(HoldAction::Expire { hold }) => (hold, ResolutionKind::Expired),
        };
        let (row, resolutions) = self
            .holds
            .get_mut(&target)
            .ok_or_else(|| format!("the resolution at {position} names no hold"))?;
        row.resolution = Some((id, how));
        *resolutions = resolutions.saturating_add(1);
        Ok(())
    }

    /// The hold invariants of the log alone: every hold resolved at most
    /// once, and every account's Pending net exactly what its open holds
    /// reserve.
    pub(crate) fn hold_invariants(&self) -> Check {
        if let Some((id, _)) = self.holds.iter().find(|(_, (_, count))| *count > 1) {
            return Err(format!("the hold {id} was resolved more than once"));
        }
        let mut reserved: BTreeMap<(AccountHandle, Currency), Amount<P>> = BTreeMap::new();
        for (row, _) in self
            .holds
            .values()
            .filter(|(row, _)| row.resolution.is_none())
        {
            for (handle, amount) in [(row.debit, row.amount), (row.credit, row.amount.negate())] {
                let net = reserved
                    .entry((handle, row.currency))
                    .or_insert(Amount::ZERO);
                *net = net
                    .checked_add(amount)
                    .map_err(|_| String::from("a reserved amount overflows"))?;
            }
        }
        let mut pending: BTreeMap<(AccountHandle, Currency), Amount<P>> = BTreeMap::new();
        for line in self
            .postings
            .iter()
            .filter(|line| line.key.layer == Layer::Pending)
        {
            let net = pending
                .entry((line.key.handle, line.key.currency))
                .or_insert(Amount::ZERO);
            let signed = match line.side {
                Side::Debit => line.amount,
                Side::Credit => line.amount.negate(),
            };
            *net = net
                .checked_add(signed)
                .map_err(|_| String::from("a Pending net overflows"))?;
        }
        reserved.retain(|_, net| !net.is_zero());
        pending.retain(|_, net| !net.is_zero());
        if reserved != pending {
            return Err(format!(
                "the Pending nets {pending:?} are not what the open holds reserve {reserved:?}"
            ));
        }
        Ok(())
    }

    /// The open holds expiring on or before `as_of`, by (expiry, position).
    pub(crate) fn due(&self, as_of: Date) -> Vec<HoldRow<P>> {
        let mut due: Vec<HoldRow<P>> = self
            .holds
            .values()
            .map(|(row, _)| row.clone())
            .filter(|row| row.resolution.is_none() && row.expires.is_some_and(|on| on <= as_of))
            .collect();
        due.sort_by_key(|row| (row.expires, row.position));
        due
    }

    /// Adds `amount` to what is cleared against `posting` by the clearing
    /// record at `position`.
    fn clear(&mut self, posting: PostingRef, amount: Amount<P>, position: u64) -> Check {
        let known = self
            .entries
            .get(&posting.entry)
            .ok_or_else(|| format!("a clearing at {position} names an unknown entry"))?;
        let key = (known.position, posting.index);
        let line = self
            .postings
            .iter()
            .find(|line| (line.position, line.index) == key)
            .ok_or_else(|| format!("a clearing at {position} names an unknown posting"))?;
        self.clearing_refs
            .entry(line.key)
            .or_default()
            .insert(position);
        let total = self.cleared.entry(key).or_insert(Amount::ZERO);
        *total = total
            .checked_add(amount)
            .map_err(|_| format!("a cleared amount overflows at {position}"))?;
        Ok(())
    }

    /// The balance of `key` after every posting below `position`.
    pub(crate) fn balance_at(&self, key: BalanceKey, position: u64) -> Balance<P> {
        self.postings
            .iter()
            .rev()
            .find(|line| line.key == key && line.position < position)
            .map_or(Balance::ZERO, |line| line.balance_after)
    }

    /// The balance of `key` through `end` by booking date (a seal's leaf).
    pub(crate) fn balance_through(&self, key: BalanceKey, end: Date) -> Result<Balance<P>, String> {
        self.postings
            .iter()
            .filter(|line| line.key == key && line.booking <= end)
            .try_fold(Balance::ZERO, |balance, line| {
                balance.post(line.side, line.amount)
            })
            .map_err(|_| String::from("a reference balance overflows"))
    }

    /// Every key with postings.
    pub(crate) fn keys(&self) -> BTreeSet<BalanceKey> {
        self.postings.iter().map(|line| line.key).collect()
    }

    /// The tree head at `size`, from the reference's leaves.
    pub(crate) fn head_at(&self, size: u64) -> Result<TreeHead, String> {
        let mut tree = Tree::new();
        for leaf in self
            .leaves
            .iter()
            .take(usize::try_from(size).unwrap_or(usize::MAX))
        {
            tree.push(leaf_hash(leaf))
                .map_err(|error| format!("the reference tree: {error}"))?;
        }
        Ok(tree.head())
    }
}

/// I1: every leaf below the oracle's size is the reference's, leaf 0 is
/// the genesis, and the stored nodes give the leaves' root.
async fn leaves_match<S: Store<P>, const P: u8>(store: &S, oracle: &Oracle<P>) -> Check {
    let mut from = 0;
    let mut tree = Tree::new();
    while from < oracle.size {
        let page = store
            .read_leaves(&LeafRead {
                from,
                through: Some(oracle.size),
                kinds: None,
                limit: PageRead::<()>::MAX_LIMIT,
            })
            .await
            .map_err(|error| store_failed(&error))?;
        if page.items.is_empty() {
            return Err(format!(
                "I1: no leaf at {from} below the size {}",
                oracle.size
            ));
        }
        for leaf in page.items {
            let expected = usize::try_from(leaf.position)
                .ok()
                .and_then(|index| oracle.leaves.get(index));
            if leaf.position != from || expected != Some(&leaf.bytes) {
                return Err(format!("I1: the leaf at {from} is not the reference's"));
            }
            if from == 0 && leaf.kind != RecordKind::Genesis {
                return Err(String::from("I1: leaf 0 is not a genesis record"));
            }
            tree.push(leaf_hash(&leaf.bytes))
                .map_err(|error| format!("I1: {error}"))?;
            from = from.saturating_add(1);
        }
    }
    let plan = RootPlan::new(oracle.size).map_err(|error| format!("I1: {error}"))?;
    let nodes = store
        .read_nodes(plan.ids())
        .await
        .map_err(|error| store_failed(&error))?;
    let root = plan
        .assemble(&nodes)
        .map_err(|error| format!("I1: {error}"))?;
    if root != tree.head().root {
        return Err(String::from(
            "I1: the root from the stored nodes is not the root of the leaves",
        ));
    }
    Ok(())
}

/// I8: an inclusion and a consistency proof at `old` built from the store's
/// nodes verify against the reference's heads.
pub(crate) async fn proofs_verify<S: Store<P>, const P: u8>(
    store: &S,
    oracle: &Oracle<P>,
    index: u64,
    old: u64,
) -> Check {
    let new = oracle.size;
    let (old_head, new_head) = (oracle.head_at(old)?, oracle.head_at(new)?);
    let inclusion = InclusionPlan::new(index, old).map_err(|error| format!("I8: {error}"))?;
    let hashes = store
        .read_nodes(inclusion.ids())
        .await
        .map_err(|error| store_failed(&error))?;
    let leaf = usize::try_from(index)
        .ok()
        .and_then(|index| oracle.leaves.get(index))
        .ok_or_else(|| format!("I8: no leaf {index}"))?;
    inclusion
        .assemble(&hashes)
        .and_then(|proof| proof.verify(&leaf_hash(leaf), &old_head))
        .map_err(|error| format!("I8: the inclusion proof of {index} at size {old}: {error}"))?;
    if old > 0 && old < new {
        let consistency = ConsistencyPlan::new(old, new).map_err(|error| format!("I8: {error}"))?;
        let hashes = store
            .read_nodes(consistency.ids())
            .await
            .map_err(|error| store_failed(&error))?;
        consistency
            .assemble(&hashes)
            .and_then(|proof| proof.verify(&old_head, &new_head))
            .map_err(|error| format!("I8: the consistency proof {old} → {new}: {error}"))?;
    }
    Ok(())
}

/// Every posting of `key` in position order, paged.
pub(crate) async fn all_postings<S: Store<P>, const P: u8>(
    store: &S,
    key: BalanceKey,
    through: u64,
) -> Result<Vec<crate::store::PostingLine<P>>, String> {
    let mut lines = Vec::new();
    let mut page: PageRead<PostingCursor> =
        PageRead::first(PageRead::<()>::MAX_LIMIT).through(through);
    loop {
        let read = store
            .postings(&PostingRead {
                key,
                order: PostingOrder::Position,
                booking: None,
                value: None,
                dimensions: Vec::new(),
                kinds: crate::store::KindFilter::Any,
                page: page.clone(),
            })
            .await
            .map_err(|error| store_failed(&error))?;
        lines.extend(read.items);
        match read.next {
            Some(next) => page = page.after(next),
            None => return Ok(lines),
        }
    }
}

/// Every hold the store's index lists as due on `as_of`, read in pages of
/// `limit`.
pub(crate) async fn due_holds<S: Store<P>, const P: u8>(
    store: &S,
    as_of: Date,
    limit: u32,
) -> Result<Vec<HoldRow<P>>, String> {
    let mut page = PageRead::first(limit);
    let mut found = Vec::new();
    loop {
        let read = store
            .holds(&HoldRead::Due {
                on_or_before: as_of,
                page: page.clone(),
            })
            .await
            .map_err(|error| store_failed(&error))?;
        if read.items.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err(String::from("a hold page exceeds its limit"));
        }
        found.extend(read.items);
        match read.next {
            Some(next) => page = page.after(next),
            None => return Ok(found),
        }
    }
}

/// The invariants I1–I9 of a store holding exactly the reference's log
/// (`ledger` is the store behind a façade, so I9 can run its audit).
pub(crate) async fn check_invariants<S: Store<P>, const P: u8>(
    ledger: &Ledger<S, P>,
    oracle: &Oracle<P>,
    random: &mut SplitMix64,
) -> Check {
    let store = ledger.store();
    let size = store.size().await.map_err(|error| store_failed(&error))?;
    if size != oracle.size {
        return Err(format!(
            "I1: the store has {size} records, the reference {}",
            oracle.size
        ));
    }
    // I1: the leaves and the root.
    leaves_match(store, oracle).await?;
    // I2: every entry has its index row.
    for (id, known) in &oracle.entries {
        let by_id = store
            .entry(EntryRef::Id(*id), None)
            .await
            .map_err(|error| store_failed(&error))?;
        let by_key = store
            .entry(EntryRef::Key(&known.key), None)
            .await
            .map_err(|error| store_failed(&error))?;
        match (by_id, by_key) {
            (Some(a), Some(b)) if a.position == known.position && a == b && a.key == known.key => {}
            _ => {
                return Err(format!(
                    "I2: the entry at {} is not indexed by id and key",
                    known.position
                ));
            }
        }
    }
    // I3: each balance's postings chain to its maintained balance; I6: the
    // cleared amounts.
    let mut totals: BTreeMap<
        (crate::model::Currency, crate::model::Layer),
        (Amount<P>, Amount<P>),
    > = BTreeMap::new();
    for key in oracle.keys() {
        let lines = all_postings(store, key, size).await?;
        let mut running = Balance::<P>::ZERO;
        for line in &lines {
            running = running
                .post(line.side, line.amount)
                .map_err(|_| format!("I3: the {key} chain overflows"))?;
            if line.balance_after != running {
                return Err(format!(
                    "I3: the balance after posting {}/{} of {key} breaks the chain",
                    line.position, line.index
                ));
            }
            let expected = oracle
                .cleared
                .get(&(line.position, line.index))
                .copied()
                .unwrap_or(Amount::ZERO);
            if line.cleared != expected
                || line.cleared.is_negative()
                || line
                    .amount
                    .checked_sub(line.cleared)
                    .is_ok_and(|rest| rest.is_negative())
            {
                return Err(format!(
                    "I6: the amount cleared against {}/{} is wrong",
                    line.position, line.index
                ));
            }
        }
        let current = store
            .balances(&BalanceRead {
                scope: BalanceScope::Current,
                keys: Some(vec![key]),
                page: PageRead::first(1),
            })
            .await
            .map_err(|error| store_failed(&error))?;
        if current.items.first().map(|(_, balance)| *balance) != Some(running) {
            return Err(format!(
                "I3: the maintained balance of {key} is not its chain's end"
            ));
        }
        let total = totals
            .entry((key.currency, key.layer))
            .or_insert((Amount::ZERO, Amount::ZERO));
        total.0 = total
            .0
            .checked_add(running.debit())
            .map_err(|_| String::from("I4: a total overflows"))?;
        total.1 = total
            .1
            .checked_add(running.credit())
            .map_err(|_| String::from("I4: a total overflows"))?;
    }
    // I4: every (currency, layer) balances.
    if let Some(((currency, layer), _)) = totals.iter().find(|(_, (debit, credit))| debit != credit)
    {
        return Err(format!("I4: {currency} {layer:?} does not balance"));
    }
    // I5: each account's records are the reference's.
    for (handle, positions) in &oracle.account_records {
        let records = store
            .account_records(*handle, PageRead::first(PageRead::<()>::MAX_LIMIT))
            .await
            .map_err(|error| store_failed(&error))?;
        let found: Vec<u64> = records.items.iter().map(|leaf| leaf.position).collect();
        if found != *positions {
            return Err(format!(
                "I5: the records of account {handle} are not the reference's"
            ));
        }
    }
    // I7: the hold index is the log's — every hold by id, and the due
    // holds in order, open ones only.
    for (row, _) in oracle.holds.values() {
        let found = store
            .holds(&HoldRead::ById {
                id: row.id,
                through: None,
            })
            .await
            .map_err(|error| store_failed(&error))?;
        if found.items.first() != Some(row) {
            return Err(format!("I7: the hold {}: got {:?}", row.id, found.items));
        }
    }
    let due = due_holds(store, Date::MAX, PageRead::<()>::MAX_LIMIT).await?;
    if due != oracle.due(Date::MAX) {
        return Err(format!(
            "I7: the open-hold index holds {due:?}, not the open holds"
        ));
    }
    // I8: proofs at sampled sizes.
    for _ in 0..3 {
        let old = random.between(1, size.max(1));
        let index = random.below(old);
        proofs_verify(store, oracle, index, old).await?;
    }
    // I9: the audit passes and covers exactly the log.
    let report = ledger
        .audit()
        .await
        .map_err(|error| format!("I9: audit failed: {error}"))?;
    if report.head != oracle.head_at(size)? {
        return Err(String::from("I9: the audited head is not the reference's"));
    }
    Ok(())
}
