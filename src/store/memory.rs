//! [`MemoryStore`]: the rule core's in-memory state behind the [`Store`]
//! contract.
//!
//! The read primitives are functions over [`MemState`], so `Journal` reads
//! its own state ([`MemView`]) through exactly the code `MemoryStore`
//! answers with.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::read::{
    AccountRow, BalanceRead, BalanceScope, EntryRef, Erasure, HoldCursor, HoldRead, HoldRow, Leaf,
    LeafRead, LedgerInfo, Memo, Page, PageRead, Position, PostingCursor, PostingLine, PostingOrder,
    PostingRead, SealRow, StoredEntry,
};
use super::{
    ProjectionPage, ProjectionRead, Store, StoreError, bound, checked_limit, page_of, paged,
    plan_did_not_converge,
};
use crate::core::{
    BalanceKey, Done, Fact, HoldView, Line, MemState, Op, Outcome, Snapshot, StateView, WriteSet,
    execute, plan,
};
use crate::error::{IntegrityKind, Rejection};
use crate::log::nodes::stored_count;
use crate::log::{Hash, leaf_hash};
use crate::model::{
    AccountHandle, AccountRef, Balance, Currency, EntryId, Genesis, Layer, PeriodId, Record,
    RecordKind,
};

/// A ledger kept in memory behind the [`Store`] contract — for tests,
/// examples and embedded use.
///
/// A cheap, cloneable, thread-safe handle: clones share one ledger. Writes
/// run the same read-plan loop as the SQL store, answering every read from
/// the rule core's in-memory state, under one lock (the ledger's
/// single-writer lock); nothing is persisted.
///
/// ```
/// # tokio_test_block(async {
/// use doubleentry::store::{MemoryStore, Store};
/// use doubleentry::{LedgerId, record::Genesis};
///
/// let store = MemoryStore::<2>::new();
/// let info = store.create(&Genesis { ledger: LedgerId::new("example.com/acme")? }).await?;
/// assert_eq!(info.size, 1);
/// assert_eq!(store.open().await?, info);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # });
/// # fn tokio_test_block<F: std::future::Future>(future: F) -> F::Output {
/// #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(future)
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct MemoryStore<const P: u8 = 2> {
    state: Arc<Mutex<Option<MemState<P>>>>,
}

/// A change to the answer of one read, for the deliberately broken stores of
/// the conformance suite; the identity everywhere else.
pub(crate) type Tamper<const P: u8> = fn(Fact<P>) -> Fact<P>;

impl<const P: u8> MemoryStore<P> {
    /// An empty store: [`create`](Store::create) writes its genesis.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(
        &self,
        operation: &'static str,
    ) -> Result<MutexGuard<'_, Option<MemState<P>>>, StoreError> {
        self.state.lock().map_err(|_| {
            StoreError::backend_message(operation, "the memory store's lock is poisoned")
        })
    }

    fn read<T>(
        &self,
        operation: &'static str,
        read: impl FnOnce(&MemState<P>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let state = self.lock(operation)?;
        read(
            state
                .as_ref()
                .ok_or_else(|| StoreError::not_created(operation))?,
        )
    }

    fn write_state<T>(
        &self,
        operation: &'static str,
        write: impl FnOnce(&mut MemState<P>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut state = self.lock(operation)?;
        write(
            state
                .as_mut()
                .ok_or_else(|| StoreError::not_created(operation))?,
        )
    }

    /// Decides `op` against the current state, answering each read through
    /// `tamper`; applies nothing.
    #[cfg(feature = "testing")]
    pub(crate) fn decide(
        &self,
        op: &Op<P>,
        tamper: Tamper<P>,
    ) -> Result<Result<Outcome<P>, Rejection>, StoreError> {
        self.read("write", |state| decide(state, op, tamper))
    }

    /// Applies an outcome decided by [`decide`](Self::decide); a write set
    /// decided at another size is refused as integrity.
    #[cfg(feature = "testing")]
    pub(crate) fn commit(&self, outcome: Outcome<P>) -> Result<Done<P>, StoreError> {
        self.write_state("write", |state| apply(state, outcome))
    }

    /// Decides and applies `op` in one critical section.
    pub(crate) fn run(
        &self,
        op: &Op<P>,
        tamper: Tamper<P>,
    ) -> Result<Result<Done<P>, Rejection>, StoreError> {
        self.write_state("write", |state| match decide(state, op, tamper)? {
            Err(rejection) => Ok(Err(rejection)),
            Ok(outcome) => apply(state, outcome).map(Ok),
        })
    }

    /// The balances of `read`, current ones as they were before the last
    /// entry.
    #[cfg(feature = "testing")]
    pub(crate) fn balances_before_last(
        &self,
        read: &BalanceRead,
    ) -> Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError> {
        self.read("balances", |state| {
            let last = state.last_posting();
            let scope = match (&read.scope, last) {
                (BalanceScope::Current, Some(last)) => BalanceScope::At(last),
                (other, _) => other.clone(),
            };
            balances(
                state,
                &BalanceRead {
                    scope,
                    ..read.clone()
                },
            )
        })
    }

    /// The holds of `read`, the due ones with the resolved ones among them.
    #[cfg(feature = "testing")]
    pub(crate) fn holds_keeping_resolved(
        &self,
        read: &HoldRead,
    ) -> Result<Page<HoldRow<P>, HoldCursor>, StoreError> {
        self.read("holds", |state| {
            let HoldRead::Due { on_or_before, page } = read else {
                return holds(state, read);
            };
            let limit = page.checked_limit("holds")?;
            let size = size(state);
            let through = bound(page.through, size);
            let mut rows: Vec<(HoldCursor, HoldRow<P>)> = state
                .hold_rows()
                .into_iter()
                .filter(|hold| {
                    hold.position < through && hold.expires.is_some_and(|on| on <= *on_or_before)
                })
                .map(|hold| ((hold.expires, hold.position), hold))
                .filter(|(cursor, _)| page.after.is_none_or(|after| *cursor > after))
                .collect();
            rows.sort_by_key(|(cursor, _)| *cursor);
            Ok(paged(rows, limit, size, through))
        })
    }
}

/// The plan loop: load the facts the core asks for, at most
/// [`Op::max_rounds`] times, then decide.
fn decide<const P: u8>(
    state: &MemState<P>,
    op: &Op<P>,
    tamper: Tamper<P>,
) -> Result<Result<Outcome<P>, Rejection>, StoreError> {
    let header = state.header();
    #[cfg(feature = "tracing")]
    let span = tracing::debug_span!(
        "write",
        ledger = %header.ledger,
        position = header.size,
        rounds = tracing::field::Empty,
        reads = tracing::field::Empty,
    );
    let mut snapshot = Snapshot::new(header);
    let mut loaded = 0usize;
    let rounds = op.max_rounds();
    for round in 0..=rounds {
        let reads = plan(&snapshot, op);
        if reads.is_empty() {
            #[cfg(feature = "tracing")]
            {
                span.record("rounds", round);
                span.record("reads", loaded);
            }
            return Ok(execute(&snapshot, op));
        }
        if round == rounds {
            break;
        }
        loaded = loaded.saturating_add(reads.len());
        for read in &reads {
            snapshot.insert(tamper(Fact::answer(state, read)));
        }
    }
    Err(plan_did_not_converge())
}

/// The read-plan loop's identity tamper.
pub(crate) const fn honest<const P: u8>(fact: Fact<P>) -> Fact<P> {
    fact
}

/// Applies a decided outcome; a write set that does not fit the state is
/// refused as integrity, before anything of it is applied.
pub(crate) fn apply<const P: u8>(
    state: &mut MemState<P>,
    outcome: Outcome<P>,
) -> Result<Done<P>, StoreError> {
    match outcome {
        Outcome::Unchanged(done) => Ok(done),
        Outcome::Append(write) => {
            let done = write.done.clone();
            apply_write(state, *write)?;
            Ok(done)
        }
        Outcome::Batch { writes, done } => {
            for write in writes {
                apply_write(state, write)?;
            }
            Ok(done)
        }
    }
}

fn apply_write<const P: u8>(state: &mut MemState<P>, write: WriteSet<P>) -> Result<(), StoreError> {
    let position = write.position;
    state.apply(write).map_err(|_| {
        StoreError::integrity_of(
            "write",
            IntegrityKind::ReplayMismatch,
            "the write set does not fit the stored state",
            Some(position),
        )
    })
}

fn info<const P: u8>(state: &MemState<P>) -> Result<LedgerInfo, StoreError> {
    let genesis = state
        .leaf(0)
        .ok_or_else(|| StoreError::integrity("open", "the genesis record is missing", Some(0)))?;
    Ok(LedgerInfo {
        ledger: state.ledger().clone(),
        precision: P,
        size: size(state),
        genesis: leaf_hash(genesis.bytes),
    })
}

fn size<const P: u8>(state: &MemState<P>) -> u64 {
    state.header().size
}

fn stored_leaf<const P: u8>(state: &MemState<P>, position: u64) -> Result<Leaf, StoreError> {
    let leaf = state.leaf(position).ok_or_else(|| {
        StoreError::integrity_of(
            "read_leaves",
            IntegrityKind::MissingRecord,
            "a leaf is missing",
            Some(position),
        )
    })?;
    let kind = leaf
        .bytes
        .first()
        .and_then(|kind| RecordKind::from_u8(*kind))
        .ok_or_else(|| {
            StoreError::integrity("read_leaves", "a leaf has no kind", Some(position))
        })?;
    Ok(Leaf {
        position,
        kind,
        bytes: leaf.bytes.to_vec(),
    })
}

fn read_leaves<const P: u8>(
    state: &MemState<P>,
    read: &LeafRead,
) -> Result<Page<Leaf, Position>, StoreError> {
    let limit = checked_limit(read.limit, "read_leaves")?;
    let size = size(state);
    let through = bound(read.through, size);
    let mut rows = Vec::new();
    for position in read.from..through {
        let leaf = stored_leaf(state, position)?;
        if read
            .kinds
            .as_ref()
            .is_none_or(|kinds| kinds.contains(&leaf.kind))
        {
            rows.push((position.saturating_add(1), leaf));
            if rows.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
                break;
            }
        }
    }
    Ok(paged(rows, limit, size, through))
}

fn read_nodes<const P: u8>(state: &MemState<P>, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
    let stored = state.tree().stored();
    let count = stored_count(size(state)).unwrap_or(0);
    ids.iter()
        .map(|id| {
            let found = usize::try_from(*id)
                .ok()
                .and_then(|index| stored.get(index));
            match found {
                Some(hash) => Ok(*hash),
                None if *id < count => Err(StoreError::integrity_of(
                    "read_nodes",
                    IntegrityKind::NodeMismatch,
                    "a stored node is missing",
                    None,
                )),
                None => Err(StoreError::backend_message(
                    "read_nodes",
                    "a node beyond the log was asked for",
                )),
            }
        })
        .collect()
}

fn seals<const P: u8>(
    state: &MemState<P>,
    through: Option<u64>,
) -> Result<Vec<SealRow>, StoreError> {
    let through = bound(through, size(state));
    state
        .seal_positions()
        .into_iter()
        .filter(|(position, _)| *position < through)
        .map(|(position, period)| {
            Ok(SealRow {
                period,
                position,
                leaf: stored_leaf(state, position)?,
            })
        })
        .collect()
}

fn accounts<const P: u8>(state: &MemState<P>, refs: &[AccountRef]) -> Vec<Option<AccountRow>> {
    refs.iter()
        .map(|account| {
            let handle = match account {
                AccountRef::Path(path) => state.handle_of(path)?,
                AccountRef::Handle(handle) => *handle,
            };
            state.account_row(handle)
        })
        .collect()
}

fn account_records<const P: u8>(
    state: &MemState<P>,
    handle: AccountHandle,
    page: &PageRead<Position>,
) -> Result<Page<Leaf, Position>, StoreError> {
    let limit = page.checked_limit("account_records")?;
    let size = size(state);
    let through = bound(page.through, size);
    let start = page.after.map_or(0, |after| after.saturating_add(1));
    let rows = state
        .account_records(handle)
        .iter()
        .filter(|position| (start..through).contains(*position))
        .take(limit_plus_one(limit))
        .map(|position| Ok((*position, stored_leaf(state, *position)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(paged(rows, limit, size, through))
}

fn entry<const P: u8>(
    state: &MemState<P>,
    by: &EntryRef<'_>,
    through: Option<u64>,
) -> Result<Option<StoredEntry>, StoreError> {
    let through = bound(through, size(state));
    let id = match by {
        EntryRef::Id(id) => *id,
        EntryRef::Key(key) => match state.entry_by_key(key) {
            Some(meta) => meta.id,
            None => return Ok(None),
        },
    };
    let Some(view) = StateView::entry(state, id).filter(|view| view.position < through) else {
        return Ok(None);
    };
    let key = view.record.parts().key.clone();
    let fingerprint = state.entry_by_key(&key).and_then(|meta| meta.fingerprint);
    Ok(Some(StoredEntry {
        position: view.position,
        id,
        key,
        fingerprint,
        leaf: stored_leaf(state, view.position)?,
    }))
}

fn holds<const P: u8>(
    state: &MemState<P>,
    read: &HoldRead,
) -> Result<Page<HoldRow<P>, HoldCursor>, StoreError> {
    let size = size(state);
    let row = |hold: &HoldView<P>, due: bool| {
        let cursor = (if due { hold.expires } else { None }, hold.position);
        (cursor, hold.row())
    };
    let (limit, through, rows) = match read {
        HoldRead::ById { id, through } => {
            let through = bound(*through, size);
            let rows = state
                .hold(*id)
                .filter(|hold| hold.position < through)
                .map(|hold| row(&hold, false))
                .into_iter()
                .collect();
            (1, through, rows)
        }
        HoldRead::OpenByAccount { handle, page } => {
            let (limit, through) = (page.checked_limit("holds")?, bound(page.through, size));
            let rows = state
                .open_holds_of(*handle, page.after.map(|(_, position)| position))
                .filter(|hold| hold.position < through)
                .take(limit_plus_one(limit))
                .map(|hold| row(hold, false))
                .collect();
            (limit, through, rows)
        }
        HoldRead::Due { on_or_before, page } => {
            let (limit, through) = (page.checked_limit("holds")?, bound(page.through, size));
            let after = page
                .after
                .and_then(|(expires, position)| Some((expires?, position)));
            let rows = state
                .holds_due(*on_or_before, after)
                .filter(|hold| hold.position < through)
                .take(limit_plus_one(limit))
                .map(|hold| row(hold, true))
                .collect();
            (limit, through, rows)
        }
    };
    Ok(paged(rows, limit, size, through))
}

fn clearings<const P: u8>(
    state: &MemState<P>,
    key: BalanceKey,
    page: &PageRead<Position>,
) -> Result<Page<Leaf, Position>, StoreError> {
    let limit = page.checked_limit("clearings")?;
    let size = size(state);
    let through = bound(page.through, size);
    let start = page.after.map_or(0, |after| after.saturating_add(1));
    let rows = state
        .clearing_refs(key, start..through)
        .take(limit_plus_one(limit))
        .map(|position| Ok((position, stored_leaf(state, position)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(paged(rows, limit, size, through))
}

fn projection<const P: u8>(
    state: &MemState<P>,
    read: &ProjectionRead,
) -> Result<ProjectionPage<P>, StoreError> {
    page_of(read.part, state.rows(read.part), &read.page, size(state))
}

fn seal_nodes<const P: u8>(
    state: &MemState<P>,
    period: &PeriodId,
    currency: Currency,
    layer: Layer,
    ids: &[(u32, u32)],
) -> Vec<Option<Hash>> {
    let nodes: BTreeMap<(u32, u32), Hash> = state
        .seal_tree(period, currency, layer)
        .map(|tree| {
            tree.stored_nodes()
                .map(|(level, index, hash)| ((level, index), hash))
                .collect()
        })
        .unwrap_or_default();
    ids.iter().map(|id| nodes.get(id).copied()).collect()
}

fn balances<const P: u8>(
    state: &MemState<P>,
    read: &BalanceRead,
) -> Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError> {
    let size = size(state);
    let through = bound(read.page.through, size);
    let at = |key: BalanceKey| match &read.scope {
        BalanceScope::Current => StateView::balance(state, key),
        BalanceScope::At(position) => state.balance_at(key, *position),
        BalanceScope::Sealed(period) => state.seal_leaf(period, key).unwrap_or(Balance::ZERO),
    };
    if let Some(keys) = &read.keys {
        let items = keys.iter().map(|key| (*key, at(*key))).collect();
        return Ok(Page {
            items,
            next: None,
            size,
            through,
        });
    }
    let limit = read.page.checked_limit("balances")?;
    let after = read.page.after;
    let rows: Vec<(BalanceKey, (BalanceKey, Balance<P>))> = match &read.scope {
        BalanceScope::Sealed(period) => state
            .seal_leaves(period)
            .map(|(currency, layer, handle, balance)| {
                let key = BalanceKey {
                    handle,
                    currency,
                    layer,
                };
                (key, (key, balance))
            })
            .filter(|(key, _)| after.is_none_or(|after| sealed_order(key) > sealed_order(&after)))
            .take(limit_plus_one(limit))
            .collect(),
        BalanceScope::Current | BalanceScope::At(_) => StateView::all_balances(state)
            .into_iter()
            .filter(|(key, _)| after.is_none_or(|after| *key > after))
            .map(|(key, _)| (key, (key, at(key))))
            .filter(|(_, (_, balance))| !balance.is_zero())
            .take(limit_plus_one(limit))
            .collect(),
    };
    Ok(paged(rows, limit, size, through))
}

fn postings<const P: u8>(
    state: &MemState<P>,
    read: &PostingRead,
) -> Result<Page<PostingLine<P>, PostingCursor>, StoreError> {
    let limit = read.page.checked_limit("postings")?;
    if read.dimensions.len() > super::PostingRead::MAX_DIMENSIONS {
        return Err(StoreError::backend_message(
            "postings",
            "more than eight dimension filters",
        ));
    }
    if matches!(&read.kinds, super::KindFilter::OneOf(kinds) if kinds.len() > super::KindFilter::MAX_KINDS)
    {
        return Err(StoreError::backend_message(
            "postings",
            "more than eight kind filters",
        ));
    }
    let size = size(state);
    let through = bound(read.page.through, size);
    let key = read.key;
    let matches = |line: &Line<P>| {
        read.booking
            .as_ref()
            .is_none_or(|range| range.contains(&line.booking))
            && read
                .value
                .as_ref()
                .is_none_or(|range| range.contains(&line.value))
            && read
                .dimensions
                .iter()
                .all(|(axis, value)| line.dimensions.get(axis) == Some(value))
            && read.kinds.keeps(line.kind.as_ref())
    };
    let rows: Vec<(PostingCursor, PostingLine<P>)> = match read.order {
        PostingOrder::Position => state
            .lines(key, 0..through)
            .filter(|((position, index), _)| {
                read.page
                    .after
                    .is_none_or(|after| (*position, *index) > (after.position, after.index))
            })
            .filter(|(_, line)| matches(line))
            .take(limit_plus_one(limit))
            .map(|(at, line)| {
                let cursor = PostingCursor {
                    booking: None,
                    position: at.0,
                    index: at.1,
                };
                (cursor, state.posting_line(at, line))
            })
            .collect(),
        PostingOrder::Booking => state
            .by_booking(key)
            .filter(|(booking, position, index)| {
                *position < through
                    && read.page.after.is_none_or(|after| {
                        (Some(*booking), *position, *index)
                            > (after.booking, after.position, after.index)
                    })
            })
            .filter_map(|(booking, position, index)| {
                let line = state.line(key, position, index)?;
                matches(line).then(|| {
                    let cursor = PostingCursor {
                        booking: Some(booking),
                        position,
                        index,
                    };
                    (cursor, state.posting_line((position, index), line))
                })
            })
            .take(limit_plus_one(limit))
            .collect(),
    };
    Ok(paged(rows, limit, size, through))
}

fn memo<const P: u8>(state: &MemState<P>, id: EntryId) -> Option<Memo> {
    let view = StateView::entry(state, id)?;
    if view.record.parts().memo.is_none() {
        return Some(Memo::None);
    }
    Some(match state.leaf(view.position).and_then(|leaf| leaf.memo) {
        Some(memo) => Memo::Kept {
            text: memo.text.clone(),
            salt: memo.salt,
        },
        None => Memo::Erased,
    })
}

/// The order of sealed balances: (currency, layer, handle), the order of
/// their trees.
const fn sealed_order(key: &BalanceKey) -> (Currency, Layer, AccountHandle) {
    (key.currency, key.layer, key.handle)
}

fn limit_plus_one(limit: u32) -> usize {
    usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .saturating_add(1)
}

/// The read primitives of a store over a [`MemState`], each the function of
/// the same name; `$with` runs one with the state (`self` and the
/// operation's name in scope).
macro_rules! reads {
    ($self:ident, $state:ident, $with:expr) => {
        async fn size(&$self) -> Result<u64, StoreError> {
            $with("size", &|$state: &MemState<P>| Ok(size($state)))
        }
        async fn read_leaves(&$self, read: &LeafRead) -> Result<Page<Leaf, Position>, StoreError> {
            $with("read_leaves", &|$state: &MemState<P>| read_leaves($state, read))
        }
        async fn read_nodes(&$self, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
            $with("read_nodes", &|$state: &MemState<P>| read_nodes($state, ids))
        }
        async fn seals(&$self, through: Option<u64>) -> Result<Vec<SealRow>, StoreError> {
            $with("seals", &|$state: &MemState<P>| seals($state, through))
        }
        async fn accounts(
            &$self,
            refs: &[AccountRef],
        ) -> Result<Vec<Option<AccountRow>>, StoreError> {
            $with("accounts", &|$state: &MemState<P>| Ok(accounts($state, refs)))
        }
        async fn account_records(
            &$self,
            handle: AccountHandle,
            page: PageRead<Position>,
        ) -> Result<Page<Leaf, Position>, StoreError> {
            $with("account_records", &|$state: &MemState<P>| {
                account_records($state, handle, &page)
            })
        }
        async fn entry(
            &$self,
            by: EntryRef<'_>,
            through: Option<u64>,
        ) -> Result<Option<StoredEntry>, StoreError> {
            $with("entry", &|$state: &MemState<P>| entry($state, &by, through))
        }
        async fn holds(&$self, read: &HoldRead) -> Result<Page<HoldRow<P>, HoldCursor>, StoreError> {
            $with("holds", &|$state: &MemState<P>| holds($state, read))
        }
        async fn clearings(
            &$self,
            key: BalanceKey,
            page: PageRead<Position>,
        ) -> Result<Page<Leaf, Position>, StoreError> {
            $with("clearings", &|$state: &MemState<P>| clearings($state, key, &page))
        }
        async fn projection(&$self, read: &ProjectionRead) -> Result<ProjectionPage<P>, StoreError> {
            $with("projection", &|$state: &MemState<P>| projection($state, read))
        }
        async fn seal_nodes(
            &$self,
            period: &PeriodId,
            currency: Currency,
            layer: Layer,
            ids: &[(u32, u32)],
        ) -> Result<Option<Vec<Option<Hash>>>, StoreError> {
            $with("seal_nodes", &|$state: &MemState<P>| {
                Ok(Some(seal_nodes($state, period, currency, layer, ids)))
            })
        }
        async fn balances(
            &$self,
            read: &BalanceRead,
        ) -> Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError> {
            $with("balances", &|$state: &MemState<P>| balances($state, read))
        }
        async fn postings(
            &$self,
            read: &PostingRead,
        ) -> Result<Page<PostingLine<P>, PostingCursor>, StoreError> {
            $with("postings", &|$state: &MemState<P>| postings($state, read))
        }
        async fn memo(&$self, id: EntryId) -> Result<Option<Memo>, StoreError> {
            $with("memo", &|$state: &MemState<P>| Ok(memo($state, id)))
        }
    };
}

impl<const P: u8> Store<P> for MemoryStore<P> {
    async fn create(&self, genesis: &Genesis) -> Result<LedgerInfo, StoreError> {
        let mut state = self.lock("create")?;
        let leaf = Record::<P>::Genesis(genesis.clone()).to_leaf_bytes();
        if let Some(existing) = state.as_ref() {
            return match existing.leaf(0) {
                Some(found) if found.bytes == leaf.as_slice() => info(existing),
                _ => Err(StoreError::ledger_mismatch("create")),
            };
        }
        let mut created = MemState::new(genesis.ledger.clone());
        match decide(&created, &Op::Genesis, honest)? {
            Ok(outcome @ Outcome::Append(_)) => {
                apply(&mut created, outcome)?;
            }
            _ => {
                return Err(StoreError::integrity(
                    "create",
                    "the genesis was not written",
                    Some(0),
                ));
            }
        }
        let info = info(&created)?;
        *state = Some(created);
        Ok(info)
    }

    async fn open(&self) -> Result<LedgerInfo, StoreError> {
        self.read("open", info)
    }

    async fn write(&self, op: &Op<P>) -> Result<Result<Done<P>, Rejection>, StoreError> {
        self.run(op, honest)
    }

    reads!(self, state, |operation, read| self.read(operation, read));

    async fn erase_memo(&self, id: EntryId) -> Result<Erasure, StoreError> {
        self.write_state("erase_memo", |state| Ok(state.erase_memo(id)))
    }
}

/// A read-only view of an in-memory state behind the [`Store`] contract:
/// how `Journal` reads its own state through the very primitives
/// `MemoryStore` answers with. Every future is ready at once; the writes
/// are refused (a journal writes its state directly).
#[derive(Debug, Clone)]
pub(crate) struct MemView<const P: u8>(pub(crate) Arc<MemState<P>>);

fn read_only(operation: &'static str) -> StoreError {
    StoreError::backend_message(operation, "a journal's view is read-only")
}

impl<const P: u8> Store<P> for MemView<P> {
    async fn create(&self, _genesis: &Genesis) -> Result<LedgerInfo, StoreError> {
        Err(read_only("create"))
    }

    async fn open(&self) -> Result<LedgerInfo, StoreError> {
        info(&self.0)
    }

    async fn write(&self, _op: &Op<P>) -> Result<Result<Done<P>, Rejection>, StoreError> {
        Err(read_only("write"))
    }

    reads!(self, state, |_, read: &dyn Fn(&MemState<P>) -> _| read(
        &self.0
    ));

    async fn erase_memo(&self, _id: EntryId) -> Result<Erasure, StoreError> {
        Err(read_only("erase_memo"))
    }
}
