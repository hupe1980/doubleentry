//! Stores the suite must catch ([`BrokenStore`]) and stores that fail like a
//! network does ([`Flaky`]).

use core::fmt;
use std::sync::{Arc, Mutex};

use super::model::SplitMix64;
use crate::core::{BalanceKey, Done, Fact, Op};
use crate::error::Rejection;
use crate::log::Hash;
use crate::model::{
    AccountHandle, AccountRef, Balance, Currency, EntryId, Genesis, Layer, PeriodId,
};
use crate::store::{
    AccountRow, BalanceRead, EntryRef, Erasure, HoldRead, HoldRow, Leaf, LeafRead, LedgerInfo,
    Memo, MemoryStore, Position, PostingCursor, PostingLine, PostingRead, ProjectionPage,
    ProjectionRead, SealRow, Store, StoreError, StoredEntry, honest,
};
use crate::{HoldCursor, Page, PageRead};

/// The methods of a wrapping store that answer straight from its `inner`
/// store, one line each.
macro_rules! forward {
    ($($method:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {
        $(
            async fn $method(&self, $($arg: $ty),*) -> Result<$out, StoreError> {
                self.inner.$method($($arg),*).await
            }
        )*
    };
}

/// One deliberate defect of a [`BrokenStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Fault {
    /// Key lookups answer "absent": a retry is recorded twice.
    ForgetsKeys,
    /// Deciding and persisting happen in separate critical sections: two
    /// writers decide against the same state.
    NoLock,
    /// `balances` returns the values from before the last entry.
    StaleBalances,
    /// A seal ignores the postings booked after its period's end.
    LateSeal,
    /// `memo` answers "unknown entry" for every entry.
    LosesMemo,
    /// Resolved holds stay in the open-hold index.
    KeepsResolvedHolds,
    /// `entry`, `holds` by id and `seals` ignore `through`: they answer as
    /// of now, not as of the log size asked.
    IgnoresThrough,
    /// `holds` of an account's open holds always answers an empty page.
    NoOpenHoldsByAccount,
}

/// A canary: a [`MemoryStore`] with one [`Fault`] the conformance suite must
/// catch. A crate test asserts that it catches each one.
///
/// ```
/// use doubleentry::testing::{BrokenStore, Fault};
///
/// assert_eq!(BrokenStore::<2>::ALL.len(), 8);
/// let canary = BrokenStore::<2>::new(Fault::NoLock);
/// # let _ = canary;
/// ```
#[derive(Debug, Clone)]
pub struct BrokenStore<const P: u8 = 2> {
    inner: MemoryStore<P>,
    fault: Fault,
}

impl<const P: u8> BrokenStore<P> {
    /// Every fault.
    pub const ALL: [Fault; 8] = [
        Fault::ForgetsKeys,
        Fault::NoLock,
        Fault::StaleBalances,
        Fault::LateSeal,
        Fault::LosesMemo,
        Fault::KeepsResolvedHolds,
        Fault::IgnoresThrough,
        Fault::NoOpenHoldsByAccount,
    ];

    /// An empty store with `fault`.
    #[must_use]
    pub fn new(fault: Fault) -> Self {
        Self {
            inner: MemoryStore::new(),
            fault,
        }
    }
}

/// Forgets every key.
fn forget_keys<const P: u8>(fact: Fact<P>) -> Fact<P> {
    match fact {
        Fact::EntryByKey(key, _) => Fact::EntryByKey(key, None),
        other => other,
    }
}

/// Sees no posting booked after a date.
fn late_seal<const P: u8>(fact: Fact<P>) -> Fact<P> {
    match fact {
        Fact::PostingsAfter(date, _) => Fact::PostingsAfter(date, Vec::new()),
        other => other,
    }
}

impl<const P: u8> Store<P> for BrokenStore<P> {
    forward! {
        create(genesis: &Genesis) -> LedgerInfo;
        open() -> LedgerInfo;
        size() -> u64;
        read_leaves(read: &LeafRead) -> Page<Leaf, Position>;
        read_nodes(ids: &[u64]) -> Vec<Hash>;
        accounts(refs: &[AccountRef]) -> Vec<Option<AccountRow>>;
        account_records(handle: AccountHandle, page: PageRead<Position>) -> Page<Leaf, Position>;
        clearings(key: BalanceKey, page: PageRead<Position>) -> Page<Leaf, Position>;
        projection(read: &ProjectionRead) -> ProjectionPage<P>;
        seal_nodes(period: &PeriodId, currency: Currency, layer: Layer, ids: &[(u32, u32)]) -> Option<Vec<Option<Hash>>>;
        postings(read: &PostingRead) -> Page<PostingLine<P>, PostingCursor>;
        erase_memo(id: EntryId) -> Erasure;
    }

    async fn seals(&self, through: Option<u64>) -> Result<Vec<SealRow>, StoreError> {
        let through = if self.fault == Fault::IgnoresThrough {
            None
        } else {
            through
        };
        self.inner.seals(through).await
    }

    async fn entry(
        &self,
        by: EntryRef<'_>,
        through: Option<u64>,
    ) -> Result<Option<StoredEntry>, StoreError> {
        let through = if self.fault == Fault::IgnoresThrough {
            None
        } else {
            through
        };
        self.inner.entry(by, through).await
    }

    async fn write(&self, op: &Op<P>) -> Result<Result<Done<P>, Rejection>, StoreError> {
        match self.fault {
            Fault::ForgetsKeys => self.inner.run(op, forget_keys),
            Fault::LateSeal => self.inner.run(op, late_seal),
            Fault::NoLock => {
                let decided = self.inner.decide(op, honest)?;
                // Another writer may decide against the same state meanwhile.
                tokio::task::yield_now().await;
                match decided {
                    Ok(outcome) => self.inner.commit(outcome).map(Ok),
                    Err(rejection) => Ok(Err(rejection)),
                }
            }
            _ => self.inner.write(op).await,
        }
    }

    async fn holds(&self, read: &HoldRead) -> Result<Page<HoldRow<P>, HoldCursor>, StoreError> {
        match (self.fault, read) {
            (Fault::KeepsResolvedHolds, _) => self.inner.holds_keeping_resolved(read),
            (Fault::IgnoresThrough, HoldRead::ById { id, .. }) => {
                self.inner
                    .holds(&HoldRead::ById {
                        id: *id,
                        through: None,
                    })
                    .await
            }
            (Fault::NoOpenHoldsByAccount, HoldRead::OpenByAccount { .. }) => {
                let mut page = self.inner.holds(read).await?;
                page.items.clear();
                page.next = None;
                Ok(page)
            }
            _ => self.inner.holds(read).await,
        }
    }

    async fn balances(
        &self,
        read: &BalanceRead,
    ) -> Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError> {
        if self.fault == Fault::StaleBalances {
            self.inner.balances_before_last(read)
        } else {
            self.inner.balances(read).await
        }
    }

    async fn memo(&self, id: EntryId) -> Result<Option<Memo>, StoreError> {
        if self.fault == Fault::LosesMemo {
            Ok(None)
        } else {
            self.inner.memo(id).await
        }
    }
}

/// The failure [`Flaky`] injects.
#[derive(Debug)]
struct Injected;

impl fmt::Display for Injected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("injected failure")
    }
}

impl std::error::Error for Injected {}

/// A store failing a seeded share of its writes like a network does: some
/// before the inner write (definitely not applied), some after it (it may
/// have applied — and did, when the inner write succeeded). Reads are
/// untouched.
///
/// ```
/// use doubleentry::store::MemoryStore;
/// use doubleentry::testing::Flaky;
///
/// let flaky = Flaky::new(MemoryStore::<2>::new(), 42).rates(10, 5);
/// # let _ = flaky;
/// ```
#[derive(Debug, Clone)]
pub struct Flaky<S> {
    inner: S,
    random: Arc<Mutex<SplitMix64>>,
    definite: u8,
    indefinite: u8,
}

impl<S> Flaky<S> {
    /// `inner`, failing 10 % of writes before and 5 % after the inner write,
    /// seeded with `seed`.
    #[must_use]
    pub fn new(inner: S, seed: u64) -> Self {
        Self {
            inner,
            random: Arc::new(Mutex::new(SplitMix64::new(seed))),
            definite: 10,
            indefinite: 5,
        }
    }

    /// The same store failing `definite` % of writes before and `indefinite`
    /// % after the inner write.
    #[must_use]
    pub fn rates(self, definite: u8, indefinite: u8) -> Self {
        Self {
            definite,
            indefinite,
            ..self
        }
    }

    fn roll(&self) -> Result<u64, StoreError> {
        let mut random = self
            .random
            .lock()
            .map_err(|_| StoreError::transient("write", Injected))?;
        Ok(random.below(100))
    }
}

impl<S: Store<P>, const P: u8> Store<P> for Flaky<S> {
    forward! {
        create(genesis: &Genesis) -> LedgerInfo;
        open() -> LedgerInfo;
        size() -> u64;
        read_leaves(read: &LeafRead) -> Page<Leaf, Position>;
        read_nodes(ids: &[u64]) -> Vec<Hash>;
        seals(through: Option<u64>) -> Vec<SealRow>;
        accounts(refs: &[AccountRef]) -> Vec<Option<AccountRow>>;
        account_records(handle: AccountHandle, page: PageRead<Position>) -> Page<Leaf, Position>;
        entry(by: EntryRef<'_>, through: Option<u64>) -> Option<StoredEntry>;
        holds(read: &HoldRead) -> Page<HoldRow<P>, HoldCursor>;
        clearings(key: BalanceKey, page: PageRead<Position>) -> Page<Leaf, Position>;
        projection(read: &ProjectionRead) -> ProjectionPage<P>;
        seal_nodes(period: &PeriodId, currency: Currency, layer: Layer, ids: &[(u32, u32)]) -> Option<Vec<Option<Hash>>>;
        balances(read: &BalanceRead) -> Page<(BalanceKey, Balance<P>), BalanceKey>;
        postings(read: &PostingRead) -> Page<PostingLine<P>, PostingCursor>;
        memo(id: EntryId) -> Option<Memo>;
        erase_memo(id: EntryId) -> Erasure;
    }

    async fn write(&self, op: &Op<P>) -> Result<Result<Done<P>, Rejection>, StoreError> {
        let roll = self.roll()?;
        let definite = u64::from(self.definite);
        if roll < definite {
            return Err(StoreError::transient("write", Injected));
        }
        let result = self.inner.write(op).await;
        if roll < definite.saturating_add(u64::from(self.indefinite)) {
            return Err(StoreError::transient("write", Injected).outcome_unknown());
        }
        result
    }
}
