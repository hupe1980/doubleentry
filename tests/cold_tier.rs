//! The cold tier, exercised against a real Iceberg table on disk.
//!
//! Nothing is mocked: a real catalog, a real table, real Parquet files, and the
//! snapshot metadata Iceberg actually wrote. The two things worth proving are
//! that a faithful copy commits *and carries its seal*, and that an unfaithful
//! one is refused before anything is written.

#![cfg(feature = "iceberg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::HashMap;

use doubleentry::account::AccountRegistry;
use doubleentry::entry::{Draft, LedgerPolicy, SealContext};
use doubleentry::period::{Period, PeriodCalendar, PeriodId, PeriodState};
use doubleentry::storage::conformance::test_ledger;
use doubleentry::storage::iceberg::{
    ColdTier, ColdTierError, PROP_ARCHIVED_THROUGH, PROP_ENTRY_COUNT, PROP_PERIOD, PROP_SEAL_HASH,
    PROP_TREE_ROOT, PROP_TRIAL_BALANCE_ROOT, iceberg_schema,
};
use doubleentry::storage::{EntryBatch, LedgerStore, MemoryStore};
use doubleentry::{
    AccountId, Amount, Currency, Dimensions, Entry, EntryId, Hash, IdempotencyKey, Label, Posting,
    Provenance, Seal, TreeHead,
};
use iceberg::{Catalog, CatalogBuilder, NamespaceIdent, TableCreation, TableIdent};
use time::macros::date;

type Eur = Amount<2>;

struct Fixture {
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,
    left: AccountId,
    right: AccountId,
    store: MemoryStore<2>,
}

impl Fixture {
    fn new() -> Self {
        let mut accounts = AccountRegistry::new();
        let left = accounts
            .register_path("Cold:Left", date!(2000 - 01 - 01))
            .expect("registers");
        let right = accounts
            .register_path("Cold:Right", date!(2000 - 01 - 01))
            .expect("registers");
        Self {
            accounts,
            calendar: PeriodCalendar::new(),
            policy: LedgerPolicy::default(),
            left,
            right,
            store: MemoryStore::new(test_ledger()),
        }
    }

    fn ctx(&self) -> SealContext<'_> {
        SealContext {
            accounts: &self.accounts,
            calendar: &self.calendar,
            policy: &self.policy,
        }
    }

    /// A balanced entry carrying dimensions and provenance, so the archive has
    /// something in every column worth checking.
    fn entry_on(&self, key: &[u8], minor: i64, on: time::Date) -> Entry<doubleentry::Balanced, 2> {
        let dims = Dimensions::none()
            .with(
                Label::new("activity").expect("valid"),
                Label::new("Network").expect("valid"),
            )
            .expect("fits")
            .with(
                Label::new("segment").expect("valid"),
                Label::new("Electricity").expect("valid"),
            )
            .expect("fits");
        Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).expect("valid"),
            on,
        )
        .with_provenance(
            Provenance::none()
                .with_actor("billing")
                .expect("valid")
                .with_source("cold-tier-test")
                .expect("valid"),
        )
        .post(
            Posting::debit(self.left, Eur::from_minor(minor), Currency::EUR)
                .with_dimensions(dims.clone()),
        )
        .post(Posting::credit(
            self.right,
            Eur::from_minor(minor),
            Currency::EUR,
        ))
        .seal(&self.ctx())
        .expect("balances")
    }

    async fn fill(&self, count: i64) {
        self.fill_on(count, date!(2026 - 03 - 15), "march").await;
    }

    async fn fill_on(&self, count: i64, on: time::Date, tag: &str) {
        for i in 0..count {
            self.store
                .append(&EntryBatch::single(self.entry_on(
                    format!("cold-{tag}-{i}").as_bytes(),
                    100 + i,
                    on,
                )))
                .await
                .expect("appends");
        }
    }

    /// Seals March, which covers everything recorded so far.
    async fn seal_march(&mut self) -> Seal {
        let id = PeriodId::new("2026-03").expect("valid");
        self.store
            .define_period(
                &Period::new(id.clone(), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
                    .expect("valid range"),
            )
            .await
            .expect("defines");
        self.store
            .transition_period(&id, PeriodState::Closing)
            .await
            .expect("closes");
        self.store.seal_period(&id).await.expect("seals")
    }
}

/// A catalog over a throwaway warehouse directory, and a table to write into.
async fn catalog_with_table(dir: &std::path::Path) -> (iceberg::MemoryCatalog, TableIdent) {
    use iceberg::memory::{MEMORY_CATALOG_WAREHOUSE, MemoryCatalogBuilder};

    let warehouse = format!("file://{}", dir.display());
    let catalog = MemoryCatalogBuilder::default()
        .load(
            "cold",
            HashMap::from([(MEMORY_CATALOG_WAREHOUSE.to_owned(), warehouse)]),
        )
        .await
        .expect("catalog loads");

    let namespace = NamespaceIdent::new("ledger".to_owned());
    catalog
        .create_namespace(&namespace, HashMap::new())
        .await
        .expect("namespace created");

    let creation = TableCreation::builder()
        .name("journal".to_owned())
        .schema(iceberg_schema().expect("schema builds"))
        .build();
    let table = catalog
        .create_table(&namespace, creation)
        .await
        .expect("table created");

    (catalog, table.identifier().clone())
}

#[tokio::test]
async fn a_sealed_period_is_archived_and_carries_its_seal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;

    let mut f = Fixture::new();
    f.fill(7).await;
    let seal = f.seal_march().await;

    let cold = ColdTier::new(ident.clone());
    let result = cold
        .compact(&f.store, &seal, &catalog)
        .await
        .expect("compacts");

    assert_eq!(result.entries, 7);
    assert_eq!(result.postings, 14, "two legs per entry");
    assert_eq!(result.archived_through, 7);
    assert_eq!(
        result.verified_root, seal.tree_head.root,
        "the archive must reproduce exactly what the seal committed to"
    );

    // The commitment is in Iceberg's own metadata, readable without this crate.
    let table = catalog.load_table(&ident).await.expect("loads");
    let snapshot = table
        .metadata()
        .current_snapshot()
        .expect("a snapshot was committed");
    let summary = &snapshot.summary().additional_properties;

    assert_eq!(
        summary.get(PROP_PERIOD).map(String::as_str),
        Some("2026-03")
    );
    assert_eq!(
        summary.get(PROP_SEAL_HASH).map(String::as_str),
        Some(seal.seal_hash.to_hex().as_str())
    );
    assert_eq!(
        summary.get(PROP_TREE_ROOT).map(String::as_str),
        Some(seal.tree_head.root.to_hex().as_str())
    );
    assert_eq!(
        summary.get(PROP_TRIAL_BALANCE_ROOT).map(String::as_str),
        Some(seal.trial_balance_root.to_hex().as_str())
    );
    assert_eq!(
        summary.get(PROP_ENTRY_COUNT).map(String::as_str),
        Some(seal.entry_count.to_string().as_str())
    );
    assert_eq!(
        summary.get(PROP_ARCHIVED_THROUGH).map(String::as_str),
        Some("7")
    );

    // And the write was an append, not anything else.
    assert_eq!(
        snapshot.summary().operation,
        iceberg::spec::Operation::Append
    );
    assert_eq!(Some(snapshot.snapshot_id()), result.snapshot_id);
}

#[tokio::test]
async fn a_seal_that_does_not_match_the_ledger_is_refused() {
    // The point of the protocol: an archive that would not reproduce the seal
    // must not be written, because the operational rows would then be deleted
    // in favour of a copy nobody can verify.
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;

    let mut f = Fixture::new();
    f.fill(4).await;
    let genuine = f.seal_march().await;

    // A seal claiming a tree the ledger never had.
    let forged = Seal {
        tree_head: TreeHead {
            size: genuine.tree_head.size,
            root: Hash::from_bytes([0xabu8; 32]),
        },
        ..genuine.clone()
    };

    let cold = ColdTier::new(ident.clone());
    let err = cold
        .compact(&f.store, &forged, &catalog)
        .await
        .expect_err("must be refused");
    assert!(matches!(err, ColdTierError::RootMismatch { .. }));

    // Nothing was committed.
    let table = catalog.load_table(&ident).await.expect("loads");
    assert!(
        table.metadata().current_snapshot().is_none(),
        "a refused compaction must leave the archive untouched"
    );
}

#[tokio::test]
async fn successive_periods_append_rather_than_replace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;
    let cold = ColdTier::new(ident.clone());

    let mut f = Fixture::new();
    f.fill(3).await;
    let march = f.seal_march().await;
    let first = cold
        .compact(&f.store, &march, &catalog)
        .await
        .expect("compacts");

    // More entries — booked into April, since March is sealed and the engine
    // refuses to back-date into it.
    f.fill_on(5, date!(2026 - 04 - 10), "april").await;
    let april_id = PeriodId::new("2026-04").expect("valid");
    f.store
        .define_period(
            &Period::new(
                april_id.clone(),
                date!(2026 - 04 - 01),
                date!(2026 - 04 - 30),
            )
            .expect("valid range"),
        )
        .await
        .expect("defines");
    f.store
        .transition_period(&april_id, PeriodState::Closing)
        .await
        .expect("closes");
    let april = f.store.seal_period(&april_id).await.expect("seals");

    let second = cold
        .compact(&f.store, &april, &catalog)
        .await
        .expect("compacts");

    // The second compaction writes only what the first did not. Re-archiving
    // the prefix would double every row an analytics query counts.
    assert_eq!(first.entries, 3);
    assert_eq!(first.archived_through, 3);
    assert_eq!(second.entries, 5, "only the delta, not the whole log");
    assert_eq!(second.archived_through, 8);
    assert_ne!(first.snapshot_id, second.snapshot_id);

    // Together they cover the log exactly once.
    assert_eq!(first.entries + second.entries, 8);
    assert_eq!(
        second.verified_root, april.tree_head.root,
        "the delta plus the archive must still reproduce the seal"
    );

    // Two append snapshots, and the log records both.
    let table = catalog.load_table(&ident).await.expect("loads");
    assert_eq!(table.metadata().snapshots().count(), 2);
    assert_eq!(
        table
            .metadata()
            .current_snapshot()
            .expect("present")
            .summary()
            .additional_properties
            .get(PROP_PERIOD)
            .map(String::as_str),
        Some("2026-04")
    );
}

#[tokio::test]
async fn an_empty_period_archives_nothing_and_still_verifies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;

    let mut f = Fixture::new();
    let seal = f.seal_march().await;

    let cold = ColdTier::new(ident);
    let result = cold
        .compact(&f.store, &seal, &catalog)
        .await
        .expect("compacts an empty period");

    assert_eq!(result.entries, 0);
    assert_eq!(result.postings, 0);
    assert_eq!(result.archived_through, 0);
    assert_eq!(result.verified_root, seal.tree_head.root);
    assert_eq!(
        result.snapshot_id, None,
        "an empty compaction must not commit a snapshot that says nothing"
    );
}

#[tokio::test]
async fn re_archiving_the_same_seal_is_a_no_op() {
    // Sealing does not always mean new entries. Running compaction twice on the
    // same seal must not write the rows a second time.
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;
    let cold = ColdTier::new(ident.clone());

    let mut f = Fixture::new();
    f.fill(5).await;
    let seal = f.seal_march().await;

    let first = cold
        .compact(&f.store, &seal, &catalog)
        .await
        .expect("compacts");
    assert_eq!(first.entries, 5);

    let again = cold
        .compact(&f.store, &seal, &catalog)
        .await
        .expect("compacts");
    assert_eq!(again.entries, 0, "nothing new to archive");
    assert_eq!(again.snapshot_id, None);
    assert_eq!(again.archived_through, 5);

    // Still exactly one snapshot.
    let table = catalog.load_table(&ident).await.expect("loads");
    assert_eq!(table.metadata().snapshots().count(), 1);
}

#[tokio::test]
async fn the_archive_refuses_to_move_backwards() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;
    let cold = ColdTier::new(ident.clone());

    let mut f = Fixture::new();
    f.fill(4).await;
    let early = f.seal_march().await;
    cold.compact(&f.store, &early, &catalog)
        .await
        .expect("compacts");

    // A seal committing to a shorter log than the archive already holds.
    let stale = Seal {
        tree_head: TreeHead {
            size: 2,
            root: early.tree_head.root,
        },
        ..early
    };
    let err = cold
        .compact(&f.store, &stale, &catalog)
        .await
        .expect_err("must be refused");
    assert!(matches!(err, ColdTierError::SealBehindArchive { .. }));
}

#[tokio::test]
async fn a_foreign_table_is_not_appended_to() {
    // A table holding rows this crate did not write has no archive state, so
    // treating it as empty would duplicate everything already in it.
    let dir = tempfile::tempdir().expect("temp dir");
    let (catalog, ident) = catalog_with_table(dir.path()).await;

    // Commit a snapshot without the crate's properties.
    let table = catalog.load_table(&ident).await.expect("loads");
    let tx = iceberg::transaction::Transaction::new(&table);
    let action = tx
        .fast_append()
        .add_data_files(Vec::new())
        .set_snapshot_properties(HashMap::from([(
            "written-by".to_owned(),
            "something-else".to_owned(),
        )]));
    use iceberg::transaction::ApplyTransactionAction;
    let _ = action
        .apply(tx)
        .expect("applies")
        .commit(&catalog)
        .await
        .expect("commits an empty snapshot");

    let mut f = Fixture::new();
    f.fill(2).await;
    let seal = f.seal_march().await;

    let cold = ColdTier::new(ident);
    match cold.compact(&f.store, &seal, &catalog).await {
        Err(ColdTierError::MalformedState(_)) => {}
        other => panic!("expected a malformed-state failure, got {other:?}"),
    }
}
