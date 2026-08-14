//! The PostgreSQL backend, exercised against a real database.
//!
//! Every test starts a throwaway PostgreSQL container, applies the reference
//! schema, and runs against it. Nothing is mocked: the constraints, the advisory
//! lock, the deferred triggers, and the concurrency behaviour are the real ones,
//! because those are exactly the parts that cannot be checked any other way.
//!
//! The headline test is [`the_postgres_backend_conforms`], which runs the
//! library's own conformance suite. A backend either passes it or is not a
//! backend.
//!
//! One container is shared by the whole binary, and each test creates its own
//! database inside it. Starting a container per test is both slow and unstable —
//! twenty containers racing for the same daemon is a source of failures that
//! have nothing to do with the code under test.

#![cfg(feature = "postgres")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::account::AccountRegistry;
use doubleentry::clearing::{Clearing, ClearingId, PostingRef};
use doubleentry::entry::{Draft, LedgerPolicy, SealContext};
use doubleentry::period::{LedgerId, Period, PeriodCalendar, PeriodId, PeriodState};
use doubleentry::storage::postgres::{DEFAULT_SCHEMA, PostgresError, PostgresStore, Sequencing};
use doubleentry::storage::{Cursor, EntryBatch, LedgerStore};
use doubleentry::{
    AccountId, Amount, BalanceKey, Balanced, Currency, Entry, EntryId, IdempotencyKey, Layer,
};
use sqlx::postgres::PgPoolOptions;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres as PostgresImage;
use time::macros::date;

type Eur = Amount<2>;

/// The one container this binary uses.
static POSTGRES: tokio::sync::OnceCell<Shared> = tokio::sync::OnceCell::const_new();

struct Shared {
    /// Held so it outlives every pool; dropping it stops the database.
    _container: ContainerAsync<PostgresImage>,
    port: u16,
}

async fn shared() -> &'static Shared {
    POSTGRES
        .get_or_init(|| async {
            let container = PostgresImage::default()
                .with_tag("17-alpine")
                .start()
                .await
                .expect("postgres container starts");
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("port is mapped");
            Shared {
                _container: container,
                port,
            }
        })
        .await
}

/// Counter giving each test its own database name.
static NEXT_DB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A private database and a store pointed at it.
struct Harness {
    /// Set when this harness owns its cluster rather than sharing one.
    _owned_container: Option<ContainerAsync<PostgresImage>>,
    /// The connection string, for tests that open a second store.
    url: String,
    store: PostgresStore<2>,
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,
    left: AccountId,
    right: AccountId,
}

impl Harness {
    /// A harness on a **dedicated** cluster.
    ///
    /// Deferred sequencing advances on a cluster-wide watermark, so a
    /// transaction held open by an unrelated test — in any database of the same
    /// instance — stalls it. Tests of that mode therefore need their own
    /// instance, which is a consequence of the technique rather than of the
    /// tests.
    async fn start_isolated() -> Self {
        let container = PostgresImage::default()
            .with_tag("17-alpine")
            .start()
            .await
            .expect("postgres container starts");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("port is mapped");
        let mut harness = Self::connect(port, "postgres".to_owned()).await;
        harness._owned_container = Some(container);
        harness
    }

    async fn start() -> Self {
        let shared = shared().await;
        let port = shared.port;

        // A private database per test: isolation without a container per test.
        let name = format!(
            "de_{}",
            NEXT_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!(
                "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
            ))
            .await
            .expect("connects to the maintenance database");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("creates a database");
        admin.close().await;

        Self::connect(port, name).await
    }

    async fn connect(port: u16, database: String) -> Self {
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/{database}");
        let ledger = LedgerId::new(format!("pg-{database}")).expect("valid");
        // `connect` configures `search_path`, so the ledger's tables land in
        // their own schema instead of competing with `public` for names.
        let store = PostgresStore::<2>::connect(&url, ledger)
            .await
            .expect("connects");
        store.migrate().await.expect("schema applies");
        let url_for_reopen = url.clone();

        let mut accounts = AccountRegistry::new();
        let left = accounts
            .register_path("Pg:Left", date!(2000 - 01 - 01))
            .expect("registers");
        let right = accounts
            .register_path("Pg:Right", date!(2000 - 01 - 01))
            .expect("registers");
        // Persist the handle bindings, not just the paths: a handle is a
        // position, and re-deriving positions on restart would repoint history.
        for record in accounts.records() {
            store.register_account(&record).await.expect("registers");
        }

        Self {
            _owned_container: None,
            url: url_for_reopen,
            store,
            accounts,
            calendar: PeriodCalendar::new(),
            policy: LedgerPolicy::default(),
            left,
            right,
        }
    }

    fn ctx(&self) -> SealContext<'_> {
        SealContext {
            accounts: &self.accounts,
            calendar: &self.calendar,
            policy: &self.policy,
        }
    }

    fn entry(&self, key: &[u8], minor: i64) -> Entry<Balanced, 2> {
        self.entry_on(key, minor, date!(2026 - 03 - 15))
    }

    fn entry_on(&self, key: &[u8], minor: i64, on: time::Date) -> Entry<Balanced, 2> {
        Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).expect("valid"),
            on,
        )
        .debit(self.left, Eur::from_minor(minor), Currency::EUR)
        .credit(self.right, Eur::from_minor(minor), Currency::EUR)
        .seal(&self.ctx())
        .expect("balances")
    }

    fn key(&self) -> BalanceKey {
        BalanceKey {
            account: self.left,
            currency: Currency::EUR,
            layer: Layer::Settled,
        }
    }
}

/// The suite that decides whether an implementation is a backend.
#[tokio::test]
async fn the_postgres_backend_conforms() {
    let h = Harness::start().await;
    let report = doubleentry::storage::conformance::check_all(&h.store).await;
    report.assert_passed();
}

/// The same suite, with positions assigned out of band instead of inline.
#[tokio::test]
async fn the_deferred_backend_conforms() {
    let h = Harness::start_isolated().await;
    let deferred = PostgresStore::<2>::with_sequencing(
        h.store.pool().clone(),
        h.store.ledger().clone(),
        Sequencing::Deferred,
    );
    let report = doubleentry::storage::conformance::check_all(&deferred).await;
    report.assert_passed();
}

#[tokio::test]
async fn deferred_appends_are_durable_before_they_are_sequenced() {
    // The window the mode exists to create: recorded and safe from loss, but not
    // yet placed in the log and so not yet provable.
    let h = Harness::start_isolated().await;
    let store = PostgresStore::<2>::with_sequencing(
        h.store.pool().clone(),
        h.store.ledger().clone(),
        Sequencing::Deferred,
    );

    let entry = h.entry(b"deferred", 1000);
    let id = entry.id();
    let recorded = store
        .append(&EntryBatch::single(entry))
        .await
        .expect("appends");

    assert_eq!(recorded[0].index, None, "no position before sequencing");
    assert!(recorded[0].require_index().is_err());

    // Durable and findable …
    let found = store.get(id).await.expect("reads").expect("present");
    assert_eq!(found.index, None);
    // … but not in the log.
    assert_eq!(store.len().await.expect("reads"), 0);
    assert_eq!(store.head().await.expect("reads").size, 0);

    // The watermark is cluster-wide, so a pass may decline until unrelated
    // transactions elsewhere in the instance have finished.
    let mut placed = 0;
    for _ in 0..64 {
        placed += store.sequence().await.expect("sequences");
        if placed > 0 {
            break;
        }
    }
    assert_eq!(placed, 1);

    let placed = store.get(id).await.expect("reads").expect("present");
    assert_eq!(placed.require_index().expect("sequenced").get(), 0);
    assert_eq!(store.len().await.expect("reads"), 1);

    // And now provable.
    let head = store.head().await.expect("reads");
    let proof = store
        .prove_inclusion(placed.index.expect("sequenced"))
        .await
        .expect("proves");
    assert!(proof.verify(&placed.content_hash, &head.root));

    // A second pass has nothing left to do.
    assert_eq!(store.sequence().await.expect("sequences"), 0);
}

#[tokio::test]
async fn deferred_sequencing_loses_nothing_under_concurrency() {
    // The reason the sequencer advances on a commit-order watermark rather than
    // a high-water mark: with concurrent writers, rows commit out of order, and
    // a reader tracking "the highest thing I have seen" steps over the ones that
    // commit late and never picks them up again.
    let h = Harness::start_isolated().await;
    let store = PostgresStore::<2>::with_sequencing(
        h.store.pool().clone(),
        h.store.ledger().clone(),
        Sequencing::Deferred,
    );

    // Writers and sequencer run at the same time, so passes see partial state.
    let mut writers = Vec::new();
    for i in 0..48i64 {
        let store = store.clone();
        let entry = h.entry(format!("race-{i}").as_bytes(), 100 + i);
        writers.push(tokio::spawn(async move {
            store.append(&EntryBatch::single(entry)).await
        }));
    }
    let sequencer = {
        let store = store.clone();
        tokio::spawn(async move {
            let mut total = 0u64;
            for _ in 0..24 {
                total += store.sequence().await.expect("sequences");
                tokio::task::yield_now().await;
            }
            total
        })
    };

    for writer in writers {
        writer.await.expect("task completes").expect("appends");
    }
    sequencer.await.expect("task completes");

    // Drain whatever the interleaving left unplaced. Bounded, because the
    // cluster-wide watermark means a pass can legitimately place nothing.
    for _ in 0..256 {
        store.sequence().await.expect("sequences");
        if store.len().await.expect("reads") == 48 {
            break;
        }
    }

    assert_eq!(
        store.len().await.expect("reads"),
        48,
        "every recorded entry must end up in the log"
    );

    // Positions are dense and gap-free, with nothing skipped.
    let mut seen = Vec::new();
    let mut cursor = Some(Cursor::start().with_limit(10));
    while let Some(c) = cursor {
        let page = store.page(c).await.expect("pages");
        seen.extend(
            page.records
                .iter()
                .map(|r| r.require_index().expect("sequenced").get()),
        );
        cursor = page.next;
    }
    assert_eq!(seen, (0..48).collect::<Vec<_>>());

    // And the log the sequencer built is the log the definition describes.
    let head = store.head().await.expect("reads");
    let mut leaves = Vec::new();
    let mut cursor = Some(Cursor::start().with_limit(100));
    while let Some(c) = cursor {
        let page = store.page(c).await.expect("pages");
        leaves.extend(page.records.iter().map(|r| r.content_hash));
        cursor = page.next;
    }
    assert_eq!(head, doubleentry::MerkleLog::from_leaves(leaves).head());
}

#[tokio::test]
async fn concurrent_sequencing_passes_do_not_duplicate_positions() {
    let h = Harness::start_isolated().await;
    let store = PostgresStore::<2>::with_sequencing(
        h.store.pool().clone(),
        h.store.ledger().clone(),
        Sequencing::Deferred,
    );

    for i in 0..20i64 {
        store
            .append(&EntryBatch::single(
                h.entry(format!("dup-{i}").as_bytes(), 100 + i),
            ))
            .await
            .expect("appends");
    }

    // Several sequencers at once; the advisory lock must make all but one wait.
    let mut passes = Vec::new();
    for _ in 0..6 {
        let store = store.clone();
        passes.push(tokio::spawn(async move { store.sequence().await }));
    }
    let mut total = 0u64;
    for pass in passes {
        total += pass.await.expect("task completes").expect("sequences");
    }
    for _ in 0..64 {
        if store.len().await.expect("reads") == 20 {
            break;
        }
        total += store.sequence().await.expect("sequences");
    }

    assert_eq!(total, 20, "each entry must be sequenced exactly once");
    assert_eq!(store.len().await.expect("reads"), 20);
}

#[tokio::test]
async fn migration_is_idempotent() {
    // A backend is restarted far more often than it is created, so applying the
    // schema again must be a no-op rather than a startup failure.
    let h = Harness::start().await;
    h.store.migrate().await.expect("second run succeeds");
    h.store.migrate().await.expect("third run succeeds");

    // And the schema still works afterwards.
    h.store
        .append(&EntryBatch::single(h.entry(b"after-migrate", 100)))
        .await
        .expect("appends");
    assert_eq!(h.store.len().await.expect("reads"), 1);
}

#[tokio::test]
async fn entries_round_trip_through_the_database() {
    let h = Harness::start().await;
    let original = h.entry(b"round-trip", 119_000);
    let id = original.id();
    let hash = original.content_hash();

    h.store
        .append(&EntryBatch::single(original.clone()))
        .await
        .expect("appends");

    let loaded = h.store.get(id).await.expect("reads").expect("present");
    assert_eq!(loaded.content_hash, hash);
    assert_eq!(loaded.entry.postings().len(), 2);
    assert_eq!(loaded.entry.booking_date(), original.booking_date());
    // Byte-for-byte: the hash is recomputed on load and compared.
    assert_eq!(loaded.entry.content_hash(), hash);
}

#[tokio::test]
async fn a_tampered_row_is_caught_on_read() {
    let h = Harness::start().await;
    let entry = h.entry(b"tamper", 1000);
    let id = entry.id();
    h.store
        .append(&EntryBatch::single(entry))
        .await
        .expect("appends");

    // Reach past the engine and alter a posting, as a compromised operator or a
    // corrupt page would.
    sqlx::query("UPDATE postings SET amount_minor = amount_minor + 1 WHERE posting_index = 0")
        .execute(h.store.pool())
        .await
        .expect("updates");

    match h.store.get(id).await {
        Err(PostgresError::Integrity(e)) => {
            assert_ne!(e.actual, e.expected, "the mismatch must be reported");
        }
        other => panic!("expected an integrity failure, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_appends_produce_dense_ordered_indices() {
    // The reason `log_index` is not a SEQUENCE: values consumed before commit
    // can commit out of order, leaving a gap a reader steps over permanently.
    let h = Harness::start().await;

    let mut tasks = Vec::new();
    for i in 0..24i64 {
        let store = h.store.clone();
        let entry = h.entry(format!("concurrent-{i}").as_bytes(), 100 + i);
        tasks.push(tokio::spawn(async move {
            store
                .append(&EntryBatch::single(entry))
                .await
                .expect("appends")
        }));
    }
    for task in tasks {
        task.await.expect("task completes");
    }

    assert_eq!(h.store.len().await.expect("reads"), 24);

    let mut seen = Vec::new();
    let mut cursor = Some(Cursor::start().with_limit(5));
    while let Some(c) = cursor {
        let page = h.store.page(c).await.expect("pages");
        seen.extend(
            page.records
                .iter()
                .map(|r| r.require_index().expect("sequenced").get()),
        );
        cursor = page.next;
    }
    assert_eq!(
        seen,
        (0..24).collect::<Vec<_>>(),
        "indices must be dense and gap-free under concurrency"
    );
}

#[tokio::test]
async fn concurrent_replays_of_one_key_append_once() {
    // At-least-once delivery means the same transaction arrives twice at once.
    let h = Harness::start().await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let store = h.store.clone();
        let entry = h.entry(b"same-key", 4242);
        tasks.push(tokio::spawn(async move {
            store.append(&EntryBatch::single(entry)).await
        }));
    }

    let mut new_count = 0;
    for task in tasks {
        let outcome = task.await.expect("task completes").expect("appends");
        if outcome[0].is_new {
            new_count += 1;
        }
    }

    assert_eq!(new_count, 1, "exactly one racer may win");
    assert_eq!(h.store.len().await.expect("reads"), 1);
}

#[tokio::test]
async fn concurrent_draws_cannot_together_breach_a_balance_limit() {
    // The failure a balance limit exists to prevent, and the one a naive
    // implementation still allows: two withdrawals that each fit the balance
    // read separately, and together do not. Checking before the write reads a
    // pre-image both racers see; the row lock inside the write is what makes
    // the invariant hold rather than usually hold.
    let h = Harness::start().await;

    let mut limited = h.accounts.clone();
    limited
        .set_limit(h.left, doubleentry::account::BalanceLimit::NoCreditBalance)
        .expect("registered");
    for record in limited.records() {
        h.store
            .register_account(&record)
            .await
            .expect("master data updates");
    }

    // Fund it with exactly 10.00 …
    h.store
        .append(&EntryBatch::single(h.entry(b"limit-funding", 1_000)))
        .await
        .expect("appends");

    // … then race eight withdrawals of 4.00. At most two can be accepted.
    let draw = |key: &[u8]| {
        Entry::<Draft, 2>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).expect("valid"),
            date!(2026 - 03 - 16),
        )
        .credit(h.left, Eur::from_minor(400), Currency::EUR)
        .debit(h.right, Eur::from_minor(400), Currency::EUR)
        .seal(&SealContext {
            accounts: &limited,
            calendar: &h.calendar,
            policy: &h.policy,
        })
        .expect("balances")
    };

    let mut tasks = Vec::new();
    for i in 0..8u8 {
        let store = h.store.clone();
        let entry = draw(format!("draw-{i}").as_bytes());
        tasks.push(tokio::spawn(async move {
            store.append(&EntryBatch::single(entry)).await
        }));
    }

    let mut accepted = 0;
    for task in tasks {
        match task.await.expect("task completes") {
            Ok(_) => accepted += 1,
            Err(PostgresError::LimitBreached { .. }) => {}
            Err(e) => panic!("unexpected failure: {e}"),
        }
    }

    assert_eq!(accepted, 2, "10.00 funds exactly two withdrawals of 4.00");
    let net = h
        .store
        .balance(h.key(), None)
        .await
        .expect("reads")
        .signed_net()
        .expect("no overflow");
    assert_eq!(net, Eur::from_minor(200));
    assert!(!net.is_negative(), "the limit must hold under concurrency");
}

#[tokio::test]
async fn a_conflicting_key_is_refused_without_writing() {
    let h = Harness::start().await;
    h.store
        .append(&EntryBatch::single(h.entry(b"key", 100)))
        .await
        .expect("appends");

    let err = h
        .store
        .append(&EntryBatch::single(h.entry(b"key", 999)))
        .await
        .expect_err("must be refused");
    assert!(matches!(err, PostgresError::IdempotencyConflict { .. }));
    assert_eq!(h.store.len().await.expect("reads"), 1);
}

#[tokio::test]
async fn a_batch_rolls_back_as_a_whole() {
    let h = Harness::start().await;
    h.store
        .append(&EntryBatch::single(h.entry(b"existing", 100)))
        .await
        .expect("appends");

    let good = h.entry(b"batch-good", 500);
    let good_id = good.id();
    let poison = h.entry(b"existing", 999); // same key, different content
    let batch = EntryBatch::new(vec![good, poison]).expect("non-empty");

    assert!(h.store.append(&batch).await.is_err());
    assert_eq!(h.store.len().await.expect("reads"), 1);
    assert!(
        h.store.get(good_id).await.expect("reads").is_none(),
        "the valid half of a refused batch must not survive"
    );
}

#[tokio::test]
async fn the_database_refuses_an_unbalanced_entry_directly() {
    // Defence in depth: the engine cannot produce one, but the deferred trigger
    // is what protects the table from everything that is not the engine.
    let h = Harness::start().await;
    let mut tx = h.store.pool().begin().await.expect("begins");

    let entry_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entries (log_index, entry_id, idempotency_key, content_hash, \
         booking_date, value_date, tree_root) \
         VALUES (0, $1, $2, $3, DATE '2026-03-15', DATE '2026-03-15', $4)",
    )
    .bind(entry_id)
    .bind(vec![1u8])
    .bind(vec![0u8; 32])
    .bind(vec![0u8; 32])
    .execute(&mut *tx)
    .await
    .expect("inserts the entry");

    for (index, direction, amount) in [(0i16, "D", 100i64), (1, "C", 90)] {
        sqlx::query(
            "INSERT INTO postings (entry_id, posting_index, account_index, direction, \
             amount_minor, currency, layer) VALUES ($1, $2, $3, $4, $5, 'EUR', 'settled')",
        )
        .bind(entry_id)
        .bind(index)
        .bind(0i32)
        .bind(direction)
        .bind(amount)
        .execute(&mut *tx)
        .await
        .expect("inserts the posting");
    }

    assert!(
        tx.commit().await.is_err(),
        "the deferred trigger must reject an unbalanced entry at COMMIT"
    );
}

#[tokio::test]
async fn balances_and_trial_balances_agree_with_the_log() {
    let h = Harness::start().await;
    for i in 0..5i64 {
        h.store
            .append(&EntryBatch::single(
                h.entry(format!("bal-{i}").as_bytes(), 100),
            ))
            .await
            .expect("appends");
    }

    let balance = h.store.balance(h.key(), None).await.expect("reads");
    assert_eq!(balance.debits, Eur::from_minor(500));
    assert_eq!(balance.credits, Eur::ZERO);

    // Gross totals survive netting.
    let tb = h.store.trial_balance(None).await.expect("reads");
    let totals = tb
        .totals(Currency::EUR, Layer::Settled)
        .expect("no overflow");
    assert!(totals.is_balanced());
    assert_eq!(totals.debits, Eur::from_minor(500));

    // As-of reads reconstruct an earlier state.
    let earlier = h.store.balance(h.key(), Some(2)).await.expect("reads");
    assert_eq!(earlier.debits, Eur::from_minor(200));
}

#[tokio::test]
async fn proofs_verify_against_the_stored_log() {
    let h = Harness::start().await;
    for i in 0..9i64 {
        h.store
            .append(&EntryBatch::single(
                h.entry(format!("proof-{i}").as_bytes(), 10 + i),
            ))
            .await
            .expect("appends");
    }

    let head = h.store.head().await.expect("reads");
    assert_eq!(head.size, 9);

    let page = h
        .store
        .page(Cursor::start().with_limit(100))
        .await
        .expect("pages");
    for record in &page.records {
        let proof = h
            .store
            .prove_inclusion(record.require_index().expect("sequenced"))
            .await
            .expect("proves");
        assert!(
            proof.verify(&record.content_hash, &head.root),
            "index {} must be provable",
            record.require_index().expect("sequenced")
        );
    }

    // Growth is provably append-only.
    h.store
        .append(&EntryBatch::single(h.entry(b"grown", 77)))
        .await
        .expect("appends");
    let grown = h.store.head().await.expect("reads");
    let proof = h.store.prove_consistency(head.size).await.expect("proves");
    assert!(proof.verify(&head.root, &grown.root));
}

#[tokio::test]
async fn sealing_a_period_excludes_later_entries() {
    let h = Harness::start().await;

    h.store
        .append(&EntryBatch::single(h.entry_on(
            b"march",
            100,
            date!(2026 - 03 - 15),
        )))
        .await
        .expect("appends");
    h.store
        .append(&EntryBatch::single(h.entry_on(
            b"april",
            900,
            date!(2026 - 04 - 10),
        )))
        .await
        .expect("appends");

    let march = PeriodId::new("2026-03").expect("valid");
    h.store
        .define_period(
            &Period::new(march.clone(), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
                .expect("valid range"),
        )
        .await
        .expect("defines");
    h.store
        .transition_period(&march, PeriodState::Closing)
        .await
        .expect("closes");

    let seal = h.store.seal_period(&march).await.expect("seals");

    // Seal a second period so the chain has an order to get wrong.
    let april = PeriodId::new("2026-04").expect("valid");
    h.store
        .define_period(
            &Period::new(april.clone(), date!(2026 - 04 - 01), date!(2026 - 04 - 30))
                .expect("valid range"),
        )
        .await
        .expect("defines");
    h.store
        .transition_period(&april, PeriodState::Closing)
        .await
        .expect("closes");
    let second = h.store.seal_period(&april).await.expect("seals");
    assert_eq!(second.prev_seal, Some(seal.seal_hash));

    // Reading them back must reproduce the chain, and it must verify.
    let stored = h.store.seals().await.expect("reads");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].seal_hash, seal.seal_hash);
    assert_eq!(stored[1].seal_hash, second.seal_hash);

    let chain = doubleentry::SealChain::from_seals(h.store.ledger().clone(), stored)
        .expect("chains in the order it was read");
    chain.verify().expect("the reloaded chain verifies");

    assert!(seal.is_self_consistent());
    assert_eq!(seal.entry_count, 1, "only March belongs to March");
    assert_eq!(seal.prev_seal, None);
}

#[tokio::test]
async fn open_items_track_partial_settlement() {
    let h = Harness::start().await;

    let invoice = h.entry(b"invoice", 1000);
    let invoice_id = invoice.id();
    h.store
        .append(&EntryBatch::single(invoice))
        .await
        .expect("appends");

    // A payment: credit the receivable.
    let payment = Entry::<Draft, 2>::new(
        EntryId::generate(),
        IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
        date!(2026 - 03 - 20),
    )
    .credit(h.left, Eur::from_minor(400), Currency::EUR)
    .debit(h.right, Eur::from_minor(400), Currency::EUR)
    .seal(&h.ctx())
    .expect("balances");
    let payment_id = payment.id();
    h.store
        .append(&EntryBatch::single(payment))
        .await
        .expect("appends");

    assert_eq!(h.store.open_items(h.key()).await.expect("reads").len(), 2);

    let clearing_id = ClearingId::generate();
    h.store
        .clear(
            Clearing::new(clearing_id, h.key(), date!(2026 - 03 - 20))
                .apply(PostingRef::new(invoice_id, 0), Eur::from_minor(400))
                .apply(PostingRef::new(payment_id, 0), Eur::from_minor(400)),
        )
        .await
        .expect("clears");

    let open = h.store.open_items(h.key()).await.expect("reads");
    assert_eq!(open.len(), 1, "the payment is fully applied");
    assert_eq!(open[0].posting, PostingRef::new(invoice_id, 0));
    assert_eq!(open[0].residual, Eur::from_minor(600));

    // Clearing is an assignment, never a movement.
    let balance = h.store.balance(h.key(), None).await.expect("reads");
    assert_eq!(balance.debits, Eur::from_minor(1000));
    assert_eq!(balance.credits, Eur::from_minor(400));

    // Releasing it reopens both items.
    h.store
        .reset_clearing(clearing_id, date!(2026 - 04 - 01))
        .await
        .expect("resets");
    assert_eq!(h.store.open_items(h.key()).await.expect("reads").len(), 2);
}

#[tokio::test]
async fn over_applying_a_clearing_is_refused() {
    let h = Harness::start().await;
    let invoice = h.entry(b"invoice", 500);
    let invoice_id = invoice.id();
    h.store
        .append(&EntryBatch::single(invoice))
        .await
        .expect("appends");

    let payment = Entry::<Draft, 2>::new(
        EntryId::generate(),
        IdempotencyKey::new(b"payment".to_vec()).expect("valid"),
        date!(2026 - 03 - 20),
    )
    .credit(h.left, Eur::from_minor(900), Currency::EUR)
    .debit(h.right, Eur::from_minor(900), Currency::EUR)
    .seal(&h.ctx())
    .expect("balances");
    let payment_id = payment.id();
    h.store
        .append(&EntryBatch::single(payment))
        .await
        .expect("appends");

    let err = h
        .store
        .clear(
            Clearing::new(ClearingId::generate(), h.key(), date!(2026 - 03 - 20))
                .apply(PostingRef::new(invoice_id, 0), Eur::from_minor(900))
                .apply(PostingRef::new(payment_id, 0), Eur::from_minor(900)),
        )
        .await
        .expect_err("the invoice only has 500 open");
    assert!(matches!(err, PostgresError::Clearing(_)));

    // Nothing was written.
    assert_eq!(h.store.open_items(h.key()).await.expect("reads").len(), 2);
}

#[tokio::test]
async fn the_stored_tree_head_matches_a_full_rebuild() {
    // `head()` reads one row instead of rebuilding the tree. That row is derived
    // state, so it has to be checked against the definition — at every size,
    // including the ragged ones between powers of two.
    let h = Harness::start().await;
    let mut leaves = Vec::new();

    for i in 0..33i64 {
        let entry = h.entry(format!("head-{i}").as_bytes(), 100 + i);
        leaves.push(entry.content_hash());
        h.store
            .append(&EntryBatch::single(entry))
            .await
            .expect("appends");

        let stored = h.store.head().await.expect("reads");
        let rebuilt = doubleentry::MerkleLog::from_leaves(leaves.clone()).head();
        assert_eq!(stored, rebuilt, "head diverged at size {}", i + 1);
    }
}

#[tokio::test]
async fn a_page_returns_complete_entries() {
    // Postings are fetched for the whole page in one query; a grouping mistake
    // would silently hand back entries with the wrong legs.
    let h = Harness::start().await;
    let mut expected = Vec::new();
    for i in 0..12i64 {
        let entry = h.entry(format!("page-{i}").as_bytes(), 100 + i);
        expected.push((entry.id(), entry.content_hash()));
        h.store
            .append(&EntryBatch::single(entry))
            .await
            .expect("appends");
    }

    let mut seen = Vec::new();
    let mut cursor = Some(Cursor::start().with_limit(5));
    while let Some(c) = cursor {
        let page = h.store.page(c).await.expect("pages");
        for record in &page.records {
            assert_eq!(
                record.entry.postings().len(),
                2,
                "both legs must be present"
            );
            assert_eq!(
                record.entry.content_hash(),
                record.content_hash,
                "each entry must hash to what was stored beside it"
            );
            seen.push((record.entry.id(), record.content_hash));
        }
        cursor = page.next;
    }
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn the_memory_and_postgres_backends_agree() {
    // Differential check: the same operations against both backends must give
    // the same log, the same head, and the same balances.
    let h = Harness::start().await;
    let memory = doubleentry::storage::MemoryStore::<2>::new(
        doubleentry::storage::conformance::test_ledger(),
    );

    for i in 0..7i64 {
        let entry = h.entry(format!("agree-{i}").as_bytes(), 100 + i);
        let batch = EntryBatch::single(entry);
        let pg = h.store.append(&batch).await.expect("appends");
        let mem = memory.append(&batch).await.expect("appends");
        assert_eq!(pg[0].index, mem[0].index);
        assert_eq!(pg[0].content_hash, mem[0].content_hash);
    }

    assert_eq!(
        h.store.head().await.expect("reads"),
        memory.head().await.expect("reads"),
        "both backends must commit to the same root"
    );
    assert_eq!(
        h.store.balance(h.key(), None).await.expect("reads"),
        memory.balance(h.key(), None).await.expect("reads"),
    );
    assert_eq!(
        h.store.trial_balance(None).await.expect("reads"),
        memory.trial_balance(None).await.expect("reads"),
    );
}

/// The ledger can share a database with an application that already has its own
/// `accounts` table.
///
/// This is the whole point of keeping the ledger's tables in their own schema:
/// `accounts` is a name most accounting applications have already spent, and a
/// ledger that squats on it in `public` cannot be adopted without a rename.
#[tokio::test]
async fn the_ledger_coexists_with_an_application_schema() {
    let h = Harness::start().await;

    // An application table with a colliding name and an incompatible shape.
    sqlx::query(
        "CREATE TABLE public.accounts (              customer_id TEXT PRIMARY KEY,              iban        TEXT NOT NULL,              balance_ct  BIGINT NOT NULL          )",
    )
    .execute(h.store.pool())
    .await
    .expect("application table is created");
    sqlx::query("INSERT INTO public.accounts VALUES ('C-1', 'DE00', 4200)")
        .execute(h.store.pool())
        .await
        .expect("application row is written");

    // The ledger keeps working, and its own accounts are untouched.
    h.store
        .append(&EntryBatch::single(h.entry(b"coexist", 1_00)))
        .await
        .expect("appends");
    let ledger_accounts = h.store.accounts().await.expect("reads");
    assert_eq!(ledger_accounts.len(), 2);
    assert!(
        ledger_accounts
            .iter()
            .all(|r| r.account.path.to_string().starts_with("Pg:"))
    );

    // Two tables, two schemas, one database.
    let names: Vec<(String, String)> =
        sqlx::query_as("SELECT table_schema, table_name FROM information_schema.tables                         WHERE table_name = 'accounts' ORDER BY table_schema")
            .fetch_all(h.store.pool())
            .await
            .expect("reads catalogue");
    assert_eq!(
        names,
        vec![
            (DEFAULT_SCHEMA.to_owned(), "accounts".to_owned()),
            ("public".to_owned(), "accounts".to_owned()),
        ]
    );

    // And the application's row is still its own.
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_ct FROM public.accounts WHERE customer_id = 'C-1'")
            .fetch_one(h.store.pool())
            .await
            .expect("reads application row");
    assert_eq!(balance, 4200);
}

/// A pool that does not resolve to the ledger's schema is refused, not silently
/// pointed at `public`.
#[tokio::test]
async fn a_misconfigured_search_path_is_refused() {
    let h = Harness::start().await;
    let url = h.url.clone();

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connects");
    let store = PostgresStore::<2>::new(pool, LedgerId::new("stray").expect("valid"));

    match store.migrate().await {
        Err(PostgresError::WrongSearchPath { expected, found }) => {
            assert_eq!(expected, DEFAULT_SCHEMA);
            assert_eq!(found, "public");
        }
        other => panic!("expected WrongSearchPath, got {other:?}"),
    }
}

/// Handles survive a restart: a second store over the same database rebuilds the
/// registry from stored bindings rather than reissuing them.
#[tokio::test]
async fn a_reopened_ledger_keeps_its_account_handles() {
    let h = Harness::start().await;
    let before = h.store.accounts().await.expect("reads");
    let original = AccountRegistry::from_records(before).expect("rebuilds");

    // Reopen the same database with a fresh store, as a restart would.
    let reopened = PostgresStore::<2>::connect(&h.url, h.store.ledger().clone())
        .await
        .expect("reconnects");
    let rebuilt =
        AccountRegistry::from_records(reopened.accounts().await.expect("reads")).expect("rebuilds");

    assert_eq!(rebuilt.commitment(), original.commitment());
    for record in original.records() {
        assert_eq!(
            rebuilt.get(record.id).map(|a| &a.path),
            Some(&record.account.path)
        );
    }
}

/// The schema is a default, not a policy: a database that belongs to the ledger
/// alone can keep the tables in `public`.
#[tokio::test]
async fn the_ledger_can_live_in_public() {
    let h = Harness::start().await;

    // A second, independent database, opened with a schema of the caller's choosing.
    let url = h
        .url
        .replace(h.url.rsplit('/').next().expect("database name"), "postgres");
    let store = PostgresStore::<2>::connect_with(
        &url,
        LedgerId::new("in-public").expect("valid"),
        "public",
    )
    .await
    .expect("connects");
    assert_eq!(store.schema(), "public");
    store.migrate().await.expect("schema applies");

    let names: Vec<String> = sqlx::query_scalar(
        "SELECT table_schema::text FROM information_schema.tables WHERE table_name = 'entries'",
    )
    .fetch_all(store.pool())
    .await
    .expect("reads catalogue");
    assert_eq!(names, vec!["public".to_owned()]);
}
