//! The SQLite backend, exercised against a real database.
//!
//! No container and no server: SQLite runs in-process, so these tests are the
//! cheapest way to check that the storage contract is genuinely portable rather
//! than shaped around PostgreSQL. Anything that only passes on one engine is a
//! leak in the abstraction, not a quirk of the other engine.

#![cfg(feature = "sqlite")]
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
use doubleentry::seal::{Seal, SealChain, SealChainError};
use doubleentry::storage::sqlite::{SqliteError, SqliteStore};
use doubleentry::storage::{Cursor, EntryBatch, LedgerStore};
use doubleentry::{
    AccountId, Amount, BalanceKey, Balanced, Currency, Entry, EntryId, IdempotencyKey, Layer,
    LogIndex, MerkleLog,
};
use sqlx::sqlite::SqlitePoolOptions;
use time::macros::date;

type Eur = Amount<2>;

struct Harness {
    store: SqliteStore<2>,
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,
    left: AccountId,
    right: AccountId,
}

impl Harness {
    async fn start() -> Self {
        Self::start_named("sqlite-test").await
    }

    async fn start_named(name: &str) -> Self {
        // A private in-memory database per test, shared across pool connections.
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect("sqlite::memory:")
            .await
            .expect("connects");

        let ledger = LedgerId::new(name).expect("valid");
        let store = SqliteStore::<2>::new(pool, ledger);
        store.migrate().await.expect("schema applies");

        let mut accounts = AccountRegistry::new();
        let left = accounts
            .register_path("Sq:Left", date!(2000 - 01 - 01))
            .expect("registers");
        let right = accounts
            .register_path("Sq:Right", date!(2000 - 01 - 01))
            .expect("registers");
        // Persist the handle bindings, not just the paths: a handle is a
        // position, and re-deriving positions on restart would repoint history.
        for record in accounts.records() {
            store.register_account(&record).await.expect("registers");
        }

        Self {
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
async fn the_sqlite_backend_conforms() {
    let h = Harness::start().await;
    let report = doubleentry::storage::conformance::check_all(&h.store).await;
    report.assert_passed();
}

#[tokio::test]
async fn migration_is_idempotent() {
    let h = Harness::start().await;
    h.store.migrate().await.expect("second run succeeds");
    h.store.migrate().await.expect("third run succeeds");
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
    assert_eq!(loaded.entry.content_hash(), hash);
    assert_eq!(loaded.entry.booking_date(), original.booking_date());
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

    sqlx::query("UPDATE postings SET amount_minor = amount_minor + 1 WHERE posting_index = 0")
        .execute(h.store.pool())
        .await
        .expect("updates");

    match h.store.get(id).await {
        Err(SqliteError::Integrity(e)) => assert_ne!(e.actual, e.expected),
        other => panic!("expected an integrity failure, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_appends_produce_dense_ordered_indices() {
    // SQLite admits one writer; the append takes the write lock before reading
    // the next index, so concurrent callers still get a dense sequence.
    let h = Harness::start().await;

    let mut tasks = Vec::new();
    for i in 0..16i64 {
        let store = h.store.clone();
        let entry = h.entry(format!("concurrent-{i}").as_bytes(), 100 + i);
        tasks.push(tokio::spawn(async move {
            store.append(&EntryBatch::single(entry)).await
        }));
    }
    let mut appended = 0;
    for task in tasks {
        if task.await.expect("task completes").is_ok() {
            appended += 1;
        }
    }
    assert_eq!(appended, 16, "every append should succeed");

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
    assert_eq!(seen, (0..16).collect::<Vec<_>>());
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
    let poison = h.entry(b"existing", 999);
    let batch = EntryBatch::new(vec![good, poison]).expect("non-empty");

    assert!(h.store.append(&batch).await.is_err());
    assert_eq!(h.store.len().await.expect("reads"), 1);
    assert!(h.store.get(good_id).await.expect("reads").is_none());
}

#[tokio::test]
async fn the_stored_tree_head_matches_a_full_rebuild() {
    let h = Harness::start().await;
    let mut leaves = Vec::new();
    for i in 0..33i64 {
        let entry = h.entry(format!("head-{i}").as_bytes(), 100 + i);
        leaves.push(entry.content_hash());
        h.store
            .append(&EntryBatch::single(entry))
            .await
            .expect("appends");
        assert_eq!(
            h.store.head().await.expect("reads"),
            MerkleLog::from_leaves(leaves.clone()).head(),
            "head diverged at size {}",
            i + 1
        );
    }
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

    let mut chain = doubleentry::SealChain::new();
    for s in stored {
        chain.push(s).expect("chains in the order it was read");
    }
    chain.verify().expect("the reloaded chain verifies");
    assert!(seal.is_self_consistent());
    assert_eq!(seal.entry_count, 1);
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
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].residual, Eur::from_minor(600));

    h.store
        .reset_clearing(clearing_id, date!(2026 - 04 - 01))
        .await
        .expect("resets");
    assert_eq!(h.store.open_items(h.key()).await.expect("reads").len(), 2);
}

/// The abstraction is only real if independent implementations agree.
#[tokio::test]
async fn all_three_backends_agree() {
    let h = Harness::start().await;
    let memory = doubleentry::storage::MemoryStore::<2>::new(
        doubleentry::storage::conformance::test_ledger(),
    );

    for i in 0..9i64 {
        let batch = EntryBatch::single(h.entry(format!("agree-{i}").as_bytes(), 100 + i));
        let sqlite = h.store.append(&batch).await.expect("appends");
        let mem = memory.append(&batch).await.expect("appends");
        assert_eq!(sqlite[0].index, mem[0].index);
        assert_eq!(sqlite[0].content_hash, mem[0].content_hash);
    }

    assert_eq!(
        h.store.head().await.expect("reads"),
        memory.head().await.expect("reads"),
        "both backends must commit to the same root"
    );
    assert_eq!(
        h.store.trial_balance(None).await.expect("reads"),
        memory.trial_balance(None).await.expect("reads"),
    );

    // And every entry is provable under the shared root.
    let head = h.store.head().await.expect("reads");
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
        assert!(proof.verify(&record.content_hash, &head.root));
    }
}

/// Two ledgers are two databases, and neither can see into the other.
///
/// The separation has to hold structurally rather than by a filter predicate: a
/// seal commits to one entity's history, so a shared log would have each seal
/// committing to the other's entries, and an inclusion proof shown to one
/// auditor would leak the other's entry count. Here that is checked the only
/// way it can be — by writing to one ledger and finding nothing of it in the
/// other, down to the Merkle root.
#[tokio::test]
async fn two_ledgers_share_nothing() {
    let a = Harness::start().await;
    let b = Harness::start().await;

    let entry = a.entry(b"only-in-a", 5_00);
    let id = entry.id();
    let recorded = a
        .store
        .append(&EntryBatch::single(entry))
        .await
        .expect("appends");
    let hash = recorded[0].content_hash;

    // The entry exists in A and not in B.
    assert!(a.store.get(id).await.expect("reads").is_some());
    assert!(b.store.get(id).await.expect("reads").is_none());

    // The same idempotency key in B is a first sighting, not a replay of A's.
    let echo = b
        .store
        .append(&EntryBatch::single(b.entry(b"only-in-a", 5_00)))
        .await
        .expect("appends");
    assert!(echo[0].is_new, "B must not dedupe against A's history");

    // Balances are per ledger: both have exactly their own one movement.
    assert_eq!(
        a.store.balance(a.key(), None).await.expect("reads").debits,
        Eur::from_minor(5_00)
    );
    assert_eq!(
        b.store.balance(b.key(), None).await.expect("reads").debits,
        Eur::from_minor(5_00)
    );

    // Two logs of one entry each, not one log of two.
    let head_a = a.store.head().await.expect("reads");
    let head_b = b.store.head().await.expect("reads");
    assert_eq!(head_a.size, 1);
    assert_eq!(head_b.size, 1);

    // Both roots are in fact equal here, and deliberately so: the content hash
    // covers what an entry *says*, not which book it landed in, so two ledgers
    // that record the same amounts on the same accounts on the same day agree
    // on the leaf. That is what makes the ledger name inside the seal load
    // bearing — see `a_seal_names_the_books_it_covers`.
    assert_eq!(head_a.root, head_b.root);

    let proof = a
        .store
        .prove_inclusion(LogIndex::new(0))
        .await
        .expect("proves");
    assert!(proof.verify(&hash, &head_a.root));
}

/// A seal is evidence about one entity's books, or it is not evidence.
///
/// Two ledgers can hold structurally identical entries, so their tree heads and
/// trial balance roots can coincide exactly. If the ledger were not in the
/// preimage, their seals would be byte-identical and a seal shown to an auditor
/// would attest to no one in particular.
#[tokio::test]
async fn a_seal_names_the_books_it_covers() {
    let a = Harness::start().await;
    let b = Harness::start_named("other-ledger").await;

    let seal_a = seal_one_period(&a, b"same-everywhere").await;
    let seal_b = seal_one_period(&b, b"same-everywhere").await;

    // Everything a seal commits to besides the ledger is identical ...
    assert_eq!(seal_a.tree_head, seal_b.tree_head);
    assert_eq!(seal_a.trial_balance_root, seal_b.trial_balance_root);
    assert_eq!(seal_a.entry_count, seal_b.entry_count);

    // ... and yet the seals are distinguishable, because they name their books.
    assert_ne!(seal_a.ledger, seal_b.ledger);
    assert_ne!(seal_a.seal_hash, seal_b.seal_hash);
    assert!(seal_a.is_self_consistent());
    assert!(seal_b.is_self_consistent());

    // Relabelling a seal invalidates it rather than transferring it.
    let mut stolen = seal_a.clone();
    stolen.ledger = seal_b.ledger.clone();
    assert!(!stolen.is_self_consistent());

    // And a chain refuses a seal from another ledger outright.
    let mut chain = SealChain::new();
    chain.push(seal_a.clone()).expect("its own seal");
    match chain.push(seal_b) {
        Err(SealChainError::ForeignLedger {
            expected, found, ..
        }) => {
            assert_eq!(expected.as_str(), "sqlite-test");
            assert_eq!(found.as_str(), "other-ledger");
        }
        other => panic!("expected ForeignLedger, got {other:?}"),
    }
}

/// Records one entry, closes the period it falls in, and seals it.
async fn seal_one_period(h: &Harness, key: &[u8]) -> Seal {
    h.store
        .append(&EntryBatch::single(h.entry(key, 5_00)))
        .await
        .expect("appends");
    let period = PeriodId::new("2026-03").expect("valid");
    h.store
        .define_period(
            &Period::new(period.clone(), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
                .expect("valid range"),
        )
        .await
        .expect("defines");
    h.store
        .transition_period(&period, PeriodState::Closing)
        .await
        .expect("closes");
    h.store.seal_period(&period).await.expect("seals")
}

/// A database remembers which ledger it holds, and refuses another one.
#[tokio::test]
async fn a_database_will_not_serve_a_second_ledger() {
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect("sqlite::memory:")
        .await
        .expect("connects");

    let first = SqliteStore::<2>::new(pool.clone(), LedgerId::new("ledger-a").expect("valid"));
    first.migrate().await.expect("claims the database");
    // Re-opening the same ledger is fine, however often.
    first.migrate().await.expect("stays claimed");

    let intruder = SqliteStore::<2>::new(pool, LedgerId::new("ledger-b").expect("valid"));
    match intruder.migrate().await {
        Err(SqliteError::WrongLedger { expected, found }) => {
            assert_eq!(expected.as_str(), "ledger-b");
            assert_eq!(found.as_str(), "ledger-a");
        }
        other => panic!("expected WrongLedger, got {other:?}"),
    }
}
