//! `MemoryStore`: the conformance suite, its canaries, a store written
//! against the public protocol only, and the store acceptance scenarios.
#![cfg(feature = "testing")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod store_checks;

use doubleentry::store::MemoryStore;
use doubleentry::testing::{BrokenStore, Config, Fault, Flaky, conformance, conformance_with};

#[tokio::test(flavor = "multi_thread")]
async fn memory_conforms() {
    let report = conformance(|| async { MemoryStore::new() })
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(
        report.checks.len() > 80,
        "every check ran: {}",
        report.checks.len()
    );
    println!("{} checks in {:?}", report.checks.len(), report.total());
}

/// The suite fails for the canary with `fault`.
async fn caught(fault: Fault) {
    let failure = conformance_with(
        || async move { BrokenStore::<2>::new(fault) },
        Config::default(),
    )
    .await
    .expect_err("the canary must be caught");
    println!("{fault:?} caught by {}", failure.check);
}

#[tokio::test(flavor = "multi_thread")]
async fn canary_forgets_keys() {
    caught(Fault::ForgetsKeys).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn canary_no_lock() {
    caught(Fault::NoLock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn canary_stale_balances() {
    caught(Fault::StaleBalances).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn canary_late_seal() {
    caught(Fault::LateSeal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn canary_loses_memo() {
    caught(Fault::LosesMemo).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn canary_keeps_resolved_holds() {
    caught(Fault::KeepsResolvedHolds).await;
}

// PoC: A3-S8 a3_s8_wrong_stores_pass_the_suite — a store answering `entry`,
// `holds` by id and `seals` as of
// now, whatever `through` asks, is caught.
#[tokio::test(flavor = "multi_thread")]
async fn canary_ignores_through() {
    caught(Fault::IgnoresThrough).await;
}

// PoC: A3-S8 a3_s8_wrong_stores_pass_the_suite — a store never returning an
// account's open holds is caught.
#[tokio::test(flavor = "multi_thread")]
async fn canary_no_open_holds_by_account() {
    caught(Fault::NoOpenHoldsByAccount).await;
}

#[test]
fn every_fault_has_a_canary_test() {
    assert_eq!(BrokenStore::<2>::ALL.len(), 8);
}

/// FR-044: the simulation, retrying definite and indefinite failures, passes
/// against a flaky store.
#[tokio::test(flavor = "multi_thread")]
async fn flaky_memory_conforms() {
    let config = Config {
        check: Some(String::from("simulation")),
        ..Config::default()
    };
    let report = conformance_with(|| async { Flaky::new(MemoryStore::new(), 7) }, config)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_eq!(report.checks.len(), 9);
}

#[tokio::test]
async fn us1_create_then_open() {
    store_checks::us1_create_then_open(MemoryStore::new()).await;
}

#[tokio::test]
async fn us1_journal_parity() {
    store_checks::us1_journal_parity(MemoryStore::new()).await;
}

#[tokio::test]
async fn us1_rejection_writes_nothing() {
    store_checks::us1_rejection_writes_nothing(MemoryStore::new()).await;
}

#[tokio::test]
async fn us1_replay_and_key_conflict() {
    store_checks::us1_replay_and_key_conflict(MemoryStore::new()).await;
}

#[tokio::test]
async fn us1_open_before_create_is_not_created() {
    store_checks::us1_open_before_create_is_not_created(MemoryStore::<2>::new()).await;
}

#[tokio::test]
async fn seal_through_and_exponents() {
    store_checks::seal_through_and_exponents(MemoryStore::new()).await;
}

#[tokio::test]
async fn us1_other_genesis_refused_identical_accepted() {
    store_checks::us1_other_genesis_refused_identical_accepted(MemoryStore::new()).await;
}

#[tokio::test]
async fn us4_balance_at_a_position() {
    store_checks::us4_balance_at_a_position(MemoryStore::new()).await;
}

#[tokio::test]
async fn us4_postings_pages_with_filters() {
    store_checks::us4_postings_pages_with_filters(MemoryStore::new()).await;
}

#[tokio::test]
async fn us4_sealed_leaves_in_handle_order() {
    store_checks::us4_sealed_leaves_in_handle_order(MemoryStore::new()).await;
}

#[tokio::test]
async fn us4_proofs_at_historical_sizes() {
    store_checks::us4_proofs_at_historical_sizes(MemoryStore::new()).await;
}

#[tokio::test]
async fn us4_pages_pinned_to_through() {
    store_checks::us4_pages_pinned_to_through(MemoryStore::new()).await;
}

/// T055: a store written by a third party, naming only
/// `doubleentry::store::*` and the crate's paging pair, passes the suite.
mod third_party {
    use doubleentry::HoldCursor;
    use doubleentry::store::{
        AccountRow, BalanceRead, Done, EntryRef, Erasure, HoldRead, HoldRow, Leaf, LeafRead,
        LedgerInfo, Memo, MemoryStore, Op, Position, PostingCursor, PostingLine, PostingRead,
        ProjectionPage, ProjectionRead, SealRow, Store, StoreError, StoredEntry,
    };
    use doubleentry::{
        AccountHandle, AccountRef, Balance, Currency, EntryId, Layer, Page, PageRead, PeriodId,
        Rejection, log::Hash, record::Genesis, store::BalanceKey,
    };

    /// A store keeping its ledger in a `MemoryStore` it wraps.
    #[derive(Clone, Default)]
    pub struct Wrapped(MemoryStore);

    impl Store<2> for Wrapped {
        async fn create(&self, genesis: &Genesis) -> Result<LedgerInfo, StoreError> {
            self.0.create(genesis).await
        }
        async fn open(&self) -> Result<LedgerInfo, StoreError> {
            self.0.open().await
        }
        async fn write(&self, op: &Op<2>) -> Result<Result<Done<2>, Rejection>, StoreError> {
            self.0.write(op).await
        }
        async fn size(&self) -> Result<u64, StoreError> {
            self.0.size().await
        }
        async fn read_leaves(&self, read: &LeafRead) -> Result<Page<Leaf, Position>, StoreError> {
            self.0.read_leaves(read).await
        }
        async fn read_nodes(&self, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
            self.0.read_nodes(ids).await
        }
        async fn seals(&self, through: Option<u64>) -> Result<Vec<SealRow>, StoreError> {
            self.0.seals(through).await
        }
        async fn accounts(
            &self,
            refs: &[AccountRef],
        ) -> Result<Vec<Option<AccountRow>>, StoreError> {
            self.0.accounts(refs).await
        }
        async fn account_records(
            &self,
            handle: AccountHandle,
            page: PageRead<Position>,
        ) -> Result<Page<Leaf, Position>, StoreError> {
            self.0.account_records(handle, page).await
        }
        async fn entry(
            &self,
            by: EntryRef<'_>,
            through: Option<u64>,
        ) -> Result<Option<StoredEntry>, StoreError> {
            self.0.entry(by, through).await
        }
        async fn holds(&self, read: &HoldRead) -> Result<Page<HoldRow<2>, HoldCursor>, StoreError> {
            self.0.holds(read).await
        }
        async fn clearings(
            &self,
            key: BalanceKey,
            page: PageRead<Position>,
        ) -> Result<Page<Leaf, Position>, StoreError> {
            self.0.clearings(key, page).await
        }
        async fn projection(&self, read: &ProjectionRead) -> Result<ProjectionPage<2>, StoreError> {
            self.0.projection(read).await
        }
        async fn seal_nodes(
            &self,
            period: &PeriodId,
            currency: Currency,
            layer: Layer,
            ids: &[(u32, u32)],
        ) -> Result<Option<Vec<Option<Hash>>>, StoreError> {
            self.0.seal_nodes(period, currency, layer, ids).await
        }
        async fn balances(
            &self,
            read: &BalanceRead,
        ) -> Result<Page<(BalanceKey, Balance), BalanceKey>, StoreError> {
            self.0.balances(read).await
        }
        async fn postings(
            &self,
            read: &PostingRead,
        ) -> Result<Page<PostingLine<2>, PostingCursor>, StoreError> {
            self.0.postings(read).await
        }
        async fn memo(&self, id: EntryId) -> Result<Option<Memo>, StoreError> {
            self.0.memo(id).await
        }
        async fn erase_memo(&self, id: EntryId) -> Result<Erasure, StoreError> {
            self.0.erase_memo(id).await
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_third_party_store_conforms() {
        doubleentry::testing::conformance(|| async { Wrapped::default() })
            .await
            .unwrap_or_else(|failure| panic!("{failure}"));
    }
}

/// SC-008: the SQL store is one implementation; what the two dialects add
/// stays within 300 lines of Rust (comments, blank lines and tests aside).
#[test]
fn the_dialects_stay_small() {
    let rust_lines = |source: &str| {
        source
            .lines()
            .take_while(|line| !line.starts_with("#[cfg(test)]"))
            .filter(|line| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with("//")
            })
            .count()
    };
    let lines = rust_lines(include_str!("../src/store/sql/sqlite.rs"))
        + rust_lines(include_str!("../src/store/sql/postgres.rs"));
    assert!(lines <= 300, "{lines} dialect lines");
}

/// 028 US2: periods, seals and the watermark, also pinned.
#[tokio::test]
async fn calendar_reads() {
    store_checks::calendar_reads(MemoryStore::new()).await;
}

/// 028 US5: totals per entry kind.
#[tokio::test]
async fn kind_queries() {
    store_checks::kind_queries(MemoryStore::new()).await;
}

/// 028 US3: a payment and its assignment in one commit.
#[tokio::test]
async fn payment_run() {
    store_checks::payment_run(MemoryStore::new()).await;
}
