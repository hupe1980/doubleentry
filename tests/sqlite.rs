//! `SqlStore<Sqlite>`: the conformance suite, the store acceptance
//! scenarios, the database's own refusals and the storage regressions.
#![cfg(all(feature = "sqlite", feature = "testing"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod store_checks;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use doubleentry::PageRead;
use doubleentry::store::{
    BalanceRead, BalanceScope, PostingOrder, PostingRead, SqlOptions, SqlStore, Sqlite, Store,
    StoreErrorKind,
};
use doubleentry::testing::conformance;
use doubleentry::{
    AccountHandle, Error, Layer, Ledger, currency::EUR, date, error::ConfigError, record::Genesis,
    store::BalanceKey,
};
use sqlx::{Connection, Row, SqliteConnection};
use store_checks::{entry_by_key_and_id, ledger, populated, sale};

static STORES: AtomicU64 = AtomicU64::new(0);

/// A fresh database file in `dir`.
fn file(dir: &Path) -> PathBuf {
    let n = STORES.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("ledger-{n}.db"))
}

fn url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

async fn fresh(dir: &Path) -> SqlStore<Sqlite> {
    SqlStore::sqlite(&url(&file(dir))).await.unwrap()
}

/// A store on a fresh file, and the file.
async fn fresh_file() -> (tempfile::TempDir, PathBuf, SqlStore<Sqlite>) {
    let dir = tempfile::tempdir().unwrap();
    let path = file(dir.path());
    let store = SqlStore::sqlite(&url(&path)).await.unwrap();
    (dir, path, store)
}

/// A raw connection to the file, outside the store.
async fn raw(path: &Path) -> SqliteConnection {
    SqliteConnection::connect(&format!("{}?mode=rw", url(path)))
        .await
        .unwrap()
}

async fn execute(connection: &mut SqliteConnection, sql: &str) -> Result<u64, sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(&mut *connection)
        .await
        .map(|done| done.rows_affected())
}

/// Drops every trigger: what an operator with write access to the file can
/// do, and what `audit` must catch.
async fn drop_triggers(connection: &mut SqliteConnection) {
    let names: Vec<String> = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'trigger'")
        .fetch_all(&mut *connection)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    for name in names {
        execute(connection, &format!("DROP TRIGGER {name}"))
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_conforms() {
    let dir = tempfile::tempdir().unwrap();
    let report = conformance(|| fresh(dir.path()))
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(
        report.total() < Duration::from_secs(180),
        "SC-001: {:?}",
        report.total()
    );
    println!("{} checks in {:?}", report.checks.len(), report.total());
}

// ── US1 ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn us1_create_then_open() {
    store_checks::us1_create_then_open(fresh_file().await.2).await;
}

#[tokio::test]
async fn us1_journal_parity() {
    store_checks::us1_journal_parity(fresh_file().await.2).await;
}

#[tokio::test]
async fn us1_rejection_writes_nothing() {
    store_checks::us1_rejection_writes_nothing(fresh_file().await.2).await;
}

#[tokio::test]
async fn us1_replay_and_key_conflict() {
    store_checks::us1_replay_and_key_conflict(fresh_file().await.2).await;
}

#[tokio::test]
async fn us1_open_before_create_is_not_created() {
    store_checks::us1_open_before_create_is_not_created(fresh_file().await.2).await;
}

#[tokio::test]
async fn seal_through_and_exponents() {
    store_checks::seal_through_and_exponents(fresh_file().await.2).await;
}

#[tokio::test]
async fn us1_other_genesis_refused_identical_accepted() {
    store_checks::us1_other_genesis_refused_identical_accepted(fresh_file().await.2).await;
}

/// A file created at one scale is another ledger at any other scale.
#[tokio::test]
async fn another_scale_is_another_ledger() {
    let (_dir, path, store) = fresh_file().await;
    Ledger::new(store, ledger()).await.unwrap();
    let wide = SqlStore::<Sqlite, 4>::sqlite(&url(&path)).await.unwrap();
    assert_eq!(
        wide.open().await.unwrap_err().kind(),
        &StoreErrorKind::LedgerMismatch
    );
    let error = Ledger::<_, 4>::with_precision(wide, ledger())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Config(ConfigError::LedgerMismatch { .. })),
        "{error}"
    );
}

/// `regress_f6_foreign_schema_is_refused` (PoC: F6): a database of another
/// schema identifier is refused by `open` and `create`, naming it; a
/// 0.7-style database is not created; nothing is changed either way.
#[tokio::test]
// PoC: F6 poc_f6_migrate_over_a_0_6_database_is_silent_and_wrong
async fn regress_f6_foreign_schema_is_refused() {
    let (_dir, path, store) = fresh_file().await;
    let mut connection = raw(&path).await;
    execute(
        &mut connection,
        "CREATE TABLE meta (singleton INTEGER PRIMARY KEY, schema_id TEXT, ledger_id TEXT, \
         amount_scale INTEGER, genesis BLOB); \
         INSERT INTO meta VALUES (1, 'doubleentry/0', 'example.com/acme', 2, zeroblob(32));",
    )
    .await
    .unwrap();
    let found = |error: doubleentry::store::StoreError| error.kind().clone();
    let expected = StoreErrorKind::SchemaMismatch {
        found: String::from("doubleentry/0"),
    };
    assert_eq!(found(store.open().await.unwrap_err()), expected);
    assert_eq!(
        found(
            store
                .create(&Genesis { ledger: ledger() })
                .await
                .unwrap_err()
        ),
        expected
    );
    let tables: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(tables, 1, "nothing was created");

    let (_dir, path, store) = fresh_file().await;
    let mut connection = raw(&path).await;
    execute(
        &mut connection,
        "CREATE TABLE ledger_meta (only_row INTEGER PRIMARY KEY, ledger_id TEXT)",
    )
    .await
    .unwrap();
    assert_eq!(
        store.open().await.unwrap_err().kind(),
        &StoreErrorKind::NotCreated
    );
    let error = Error::from(store.open().await.unwrap_err());
    assert!(matches!(error, Error::Backend(_)));
}

/// `regress_dollar_placeholders_rebind_out_of_order` (PoC:
/// `sqlite_accepts_dollar_n_out_of_order`): SQLite binds `$n` by number, so
/// the shared `$n` statements are correct in both dialects.
#[tokio::test]
// PoC: sqlite_accepts_dollar_n_out_of_order
async fn regress_dollar_placeholders_rebind_out_of_order() {
    let (_dir, path, _store) = fresh_file().await;
    let mut connection = raw(&path).await;
    let row = sqlx::query("SELECT $2, $1, $2")
        .bind(10i64)
        .bind(20i64)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let values: (i64, i64, i64) = (row.get(0), row.get(1), row.get(2));
    assert_eq!(values, (20, 10, 20));
}

// ── US2 ─────────────────────────────────────────────────────────────────────

/// Writers queue on the file lock for the busy timeout; with none, a
/// contended write is unavailable and definitely not applied.
#[tokio::test(flavor = "multi_thread")]
async fn busy_timeout_queues_writers() {
    let (_dir, path, store) = fresh_file().await;
    let books = Ledger::new(store, ledger()).await.unwrap();
    let impatient = SqlStore::<Sqlite>::sqlite_with(
        &url(&path),
        SqlOptions {
            busy_timeout: Duration::ZERO,
            ..SqlOptions::default()
        },
    )
    .await
    .unwrap();
    let impatient = Ledger::new(impatient, ledger()).await.unwrap();
    let mut holder = raw(&path).await;
    execute(&mut holder, "BEGIN IMMEDIATE").await.unwrap();
    let error = impatient
        .declare_currency(EUR, 2, date!("2026-01-01"))
        .await
        .unwrap_err();
    assert!(error.is_transient() && !error.may_have_applied(), "{error}");
    let patient =
        tokio::spawn(async move { books.declare_currency(EUR, 2, date!("2026-01-01")).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    execute(&mut holder, "ROLLBACK").await.unwrap();
    patient.await.unwrap().unwrap();
}

// ── US4 ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn us4_balance_at_a_position() {
    store_checks::us4_balance_at_a_position(fresh_file().await.2).await;
}

#[tokio::test]
async fn us4_postings_pages_with_filters() {
    store_checks::us4_postings_pages_with_filters(fresh_file().await.2).await;
}

#[tokio::test]
async fn us4_sealed_leaves_in_handle_order() {
    store_checks::us4_sealed_leaves_in_handle_order(fresh_file().await.2).await;
}

#[tokio::test]
async fn us4_proofs_at_historical_sizes() {
    store_checks::us4_proofs_at_historical_sizes(fresh_file().await.2).await;
}

#[tokio::test]
async fn us4_pages_pinned_to_through() {
    store_checks::us4_pages_pinned_to_through(fresh_file().await.2).await;
}

/// `regress_qs3_balances_for_40000_accounts` (PoC: Q-S3): balances of 40 000
/// keys in one read — no bound-parameter limit (keys without postings read
/// as zero).
#[tokio::test]
// PoC: Q-S3 q_s3_sqlite_balances_fails_above_32766_accounts
async fn regress_qs3_balances_for_40000_accounts() {
    let (_dir, _path, store) = fresh_file().await;
    let (_, books) = store_checks::books(store.clone()).await;
    books
        .record(sale("one", date!("2026-01-15"), "1.00"))
        .await
        .unwrap();
    let keys: Vec<BalanceKey> = (0..40_000)
        .map(|handle| BalanceKey {
            handle: AccountHandle::new(handle).unwrap(),
            currency: EUR,
            layer: Layer::Settled,
        })
        .collect();
    let read = BalanceRead {
        scope: BalanceScope::Current,
        keys: Some(keys),
        page: PageRead::first(1),
    };
    let balances = store.balances(&read).await.unwrap();
    assert_eq!(balances.items.len(), 40_000);
    assert_eq!(
        balances
            .items
            .iter()
            .filter(|(_, balance)| !balance.is_zero())
            .count(),
        2
    );
}

/// The query plan of `sql`, joined.
async fn plan(connection: &mut SqliteConnection, sql: &str) -> String {
    sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
        .fetch_all(&mut *connection)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>(3))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Postings pages are range scans of `postings_by_key` /
/// `postings_by_key_day` with no sort (Q-S8); sealing reads
/// `postings_by_day`; due holds read `holds_open_due`.
#[tokio::test]
async fn plan_reads_use_their_indexes_without_sorting() {
    let (_dir, path, store) = fresh_file().await;
    populated(store).await;
    let mut connection = raw(&path).await;
    execute(&mut connection, "ANALYZE").await.unwrap();
    let by_position = plan(
        &mut connection,
        "SELECT p.position, p.idx FROM postings p JOIN entries e ON e.position = p.position \
         WHERE p.handle = 0 AND p.currency = 'EUR' AND p.layer = 0 AND p.position < 100 \
         AND (p.position, p.idx) > (-1, -1) ORDER BY p.position, p.idx LIMIT 10",
    )
    .await;
    assert!(
        by_position.contains("postings_by_key") && !by_position.contains("TEMP B-TREE"),
        "{by_position}"
    );
    let by_booking = plan(
        &mut connection,
        "SELECT p.position, p.idx FROM postings p JOIN entries e ON e.position = p.position \
         WHERE p.handle = 0 AND p.currency = 'EUR' AND p.layer = 0 AND p.position < 100 \
         AND (p.booking, p.position, p.idx) > (0, -1, -1) ORDER BY p.booking, p.position, p.idx LIMIT 10",
    )
    .await;
    assert!(
        by_booking.contains("postings_by_key_day") && !by_booking.contains("TEMP B-TREE"),
        "{by_booking}"
    );
    let after = plan(
        &mut connection,
        "SELECT handle, currency, layer, side, amount FROM postings WHERE booking > 0 ORDER BY booking, position, idx",
    )
    .await;
    assert!(
        after.contains("postings_by_day") && !after.contains("TEMP B-TREE"),
        "{after}"
    );
    let due = plan(
        &mut connection,
        "SELECT position FROM holds WHERE resolved_by IS NULL AND expires IS NOT NULL AND expires <= 9 \
         AND (expires, position) > (0, -1) AND position < 100 ORDER BY expires, position LIMIT 10",
    )
    .await;
    assert!(
        due.contains("holds_open_due") && !due.contains("TEMP B-TREE"),
        "{due}"
    );
}

/// Q-S12: an out-of-domain stored value is an integrity failure, never a
/// default.
#[tokio::test]
async fn strict_decode_refuses_out_of_domain_values() {
    let (_dir, path, store) = fresh_file().await;
    let (_books, _) = populated(store.clone()).await;
    let mut connection = raw(&path).await;
    drop_triggers(&mut connection).await;
    execute(&mut connection, "PRAGMA ignore_check_constraints = ON")
        .await
        .unwrap();
    execute(
        &mut connection,
        "UPDATE postings SET side = 7, debit_after = zeroblob(15) WHERE handle = 0",
    )
    .await
    .unwrap();
    let cash = BalanceKey {
        handle: AccountHandle::new(0).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
    };
    let postings = store
        .postings(&PostingRead {
            key: cash,
            order: PostingOrder::Position,
            booking: None,
            value: None,
            dimensions: Vec::new(),
            page: PageRead::first(10),
        })
        .await
        .unwrap_err();
    assert_eq!(postings.kind(), &StoreErrorKind::Integrity);
    let at = BalanceRead {
        scope: BalanceScope::At(1_000),
        keys: Some(vec![cash]),
        page: PageRead::first(1),
    };
    assert_eq!(
        store.balances(&at).await.unwrap_err().kind(),
        &StoreErrorKind::Integrity
    );
}

// ── US5 ─────────────────────────────────────────────────────────────────────

/// The column each append-only table's refused `UPDATE` sets.
const APPEND_ONLY: [(&str, &str); 16] = [
    ("meta", "schema_id"),
    ("leaves", "kind"),
    ("nodes", "hash"),
    ("accounts", "kind"),
    ("account_versions", "effective"),
    ("currency_versions", "exponent"),
    ("policy_versions", "effective"),
    ("periods", "last_day"),
    ("postings", "amount"),
    ("posting_dims", "value"),
    ("seals", "position"),
    ("seal_leaves", "debit"),
    ("seal_nodes", "hash"),
    ("clearings", "idem_key"),
    ("clearing_resets", "position"),
    ("clearing_refs", "layer"),
];

fn refused(result: Result<u64, sqlx::Error>, op: &str, table: &str) {
    let error = result.expect_err(&format!("{op} on {table} must be refused"));
    let message = error
        .as_database_error()
        .map(|error| error.message().to_owned())
        .unwrap_or_default();
    assert!(
        message.starts_with("doubleentry:") && message.contains(op) && message.contains(table),
        "{message}"
    );
}

/// FR-032, FR-033, FR-050: every append-only table refuses `UPDATE` and
/// `DELETE`; maintained values refuse `DELETE`; memos allow erasure only;
/// holds resolve once; entries allow only the fingerprint's erasure.
#[tokio::test]
async fn immutability_is_enforced_by_the_database() {
    let (_dir, path, store) = fresh_file().await;
    let (_books, inv) = populated(store).await;
    let mut db = raw(&path).await;
    for (table, column) in APPEND_ONLY {
        refused(
            execute(&mut db, &format!("UPDATE {table} SET {column} = {column}")).await,
            "UPDATE",
            table,
        );
        refused(
            execute(&mut db, &format!("DELETE FROM {table}")).await,
            "DELETE",
            table,
        );
    }
    // The populated ledger has a row in every maintained table, the
    // amounts open holds reserve included.
    for table in ["balances", "cleared", "outstanding", "slices"] {
        refused(
            execute(&mut db, &format!("DELETE FROM {table}")).await,
            "DELETE",
            table,
        );
    }
    execute(&mut db, "UPDATE balances SET postings = postings")
        .await
        .unwrap();
    // Memos: erasure only.
    refused(
        execute(&mut db, "UPDATE memos SET text = 'other'").await,
        "UPDATE",
        "memos",
    );
    refused(
        execute(&mut db, "DELETE FROM memos").await,
        "DELETE",
        "memos",
    );
    assert_eq!(
        execute(
            &mut db,
            &format!("UPDATE memos SET salt = NULL, text = NULL WHERE position = {inv}")
        )
        .await
        .unwrap(),
        1
    );
    // Entries: the fingerprint's erasure only.
    refused(
        execute(&mut db, "UPDATE entries SET idem_key = x'01'").await,
        "UPDATE",
        "entries",
    );
    refused(
        execute(&mut db, "UPDATE entries SET fingerprint = zeroblob(32)").await,
        "UPDATE",
        "entries",
    );
    refused(
        execute(&mut db, "DELETE FROM entries").await,
        "DELETE",
        "entries",
    );
    assert_eq!(
        execute(
            &mut db,
            &format!("UPDATE entries SET fingerprint = NULL WHERE position = {inv}")
        )
        .await
        .unwrap(),
        1
    );
    // Holds: resolve once.
    execute(
        &mut db,
        &format!("INSERT INTO holds (position, entry_id, booking, debit_handle, credit_handle, currency, amount) \
                  SELECT position, entry_id, 0, 0, 1, 'EUR', zeroblob(16) FROM entries WHERE position = {inv}"),
    )
    .await
    .unwrap();
    let resolve = format!(
        "UPDATE holds SET resolved_by = (SELECT entry_id FROM entries WHERE position <> {inv} \
         AND entry_id NOT IN (SELECT resolved_by FROM holds WHERE resolved_by IS NOT NULL) LIMIT 1), \
         how = 1 WHERE position = {inv}"
    );
    refused(
        execute(&mut db, "UPDATE holds SET booking = 1").await,
        "UPDATE",
        "holds",
    );
    assert_eq!(execute(&mut db, &resolve).await.unwrap(), 1);
    refused(execute(&mut db, &resolve).await, "UPDATE", "holds");
    refused(
        execute(&mut db, "DELETE FROM holds").await,
        "DELETE",
        "holds",
    );
}

/// Runs `tamper` on a populated ledger with its triggers dropped; the audit
/// must then fail as an integrity failure.
async fn audit_catches(tamper: &str) {
    let (_dir, path, store) = fresh_file().await;
    let (books, _) = populated(store).await;
    let mut db = raw(&path).await;
    drop_triggers(&mut db).await;
    execute(&mut db, "PRAGMA foreign_keys = OFF").await.unwrap();
    assert!(
        execute(&mut db, tamper).await.unwrap() > 0,
        "{tamper} changed nothing"
    );
    let error = books.audit().await.expect_err(tamper);
    assert!(matches!(error, Error::Integrity(_)), "{tamper}: {error}");
}

/// SC-007: each tamper fixture makes `audit()` fail.
#[tokio::test]
async fn tampering_fails_the_audit() {
    // An edited leaf byte.
    audit_catches(
        "UPDATE leaves SET bytes = CAST(substr(bytes, 1, length(bytes) - 1) || x'ff' AS BLOB) \
         WHERE position = (SELECT MIN(position) FROM entries)",
    )
    .await;
    // Two entries' ids swapped.
    audit_catches(
        "CREATE TEMP TABLE ids AS SELECT ordinal, entry_id FROM entries WHERE ordinal IN (0, 1); \
         UPDATE entries SET entry_id = zeroblob(16) WHERE ordinal = 0; \
         UPDATE entries SET entry_id = (SELECT entry_id FROM ids WHERE ordinal = 0) WHERE ordinal = 1; \
         UPDATE entries SET entry_id = (SELECT entry_id FROM ids WHERE ordinal = 1) WHERE ordinal = 0;",
    )
    .await;
    // An edited balance after.
    audit_catches("UPDATE postings SET debit_after = credit_after WHERE handle = 0 AND idx = 0")
        .await;
    // An edited maintained balance.
    audit_catches("UPDATE balances SET postings = postings + 1 WHERE handle = 0").await;
}

/// 023 SC-006: the hold index and the amounts open holds reserve are
/// recomputed by the audit — a resolved hold marked open, an open hold
/// marked resolved, an edited reserved amount.
#[tokio::test]
async fn us5_audit_catches_hold_tampering() {
    audit_catches("UPDATE holds SET resolved_by = NULL, how = NULL WHERE resolved_by IS NOT NULL")
        .await;
    audit_catches(
        "UPDATE holds SET resolved_by = (SELECT entry_id FROM entries WHERE ordinal = 0), how = 1 \
         WHERE resolved_by IS NULL",
    )
    .await;
    audit_catches("UPDATE outstanding SET debit = credit WHERE debit <> credit").await;
}

/// `holds_resolution_link_is_committed` (023 R9; PoC: A1 / Q-E1): pointing a
/// stored resolution at another hold rewrites its leaf, which the audit
/// refuses.
#[tokio::test]
async fn holds_resolution_link_is_committed() {
    let (_dir, path, store) = fresh_file().await;
    let (books, _) = populated(store.clone()).await;
    let (position, bytes) = store_checks::retargeted_resolution(&store).await;
    let mut db = raw(&path).await;
    drop_triggers(&mut db).await;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let tamper = format!("UPDATE leaves SET bytes = x'{hex}' WHERE position = {position}");
    assert_eq!(execute(&mut db, &tamper).await.unwrap(), 1);
    let error = books.audit().await.unwrap_err();
    assert!(matches!(error, Error::Integrity(_)), "{error}");
}

/// `regress_qs2_sparse_handle_fails_audit` (PoC: Q-S2): an account row with a
/// handle that skips the count fails the audit.
#[tokio::test]
// PoC: Q-S2 q_s2_sparse_account_handle_bricks_sealing
async fn regress_qs2_sparse_handle_fails_audit() {
    audit_catches(
        "INSERT INTO accounts (handle, path, kind, position) \
         SELECT 9, 'Assets:Sparse', 0, MAX(position) FROM entries",
    )
    .await;
}

/// `regress_g5_forged_entry_fails_audit` (PoC: G5): a forged entry leaf with
/// its index row, but no postings, fails the audit.
#[tokio::test]
// PoC: G5 poc_g5_entry_without_postings_passes_db_triggers
async fn regress_g5_forged_entry_fails_audit() {
    audit_catches(
        "INSERT INTO leaves (position, kind, bytes) SELECT (SELECT MAX(position) + 1 FROM leaves), kind, bytes \
         FROM leaves WHERE position = (SELECT MIN(position) FROM entries); \
         INSERT INTO entries (position, ordinal, entry_id, idem_key, fingerprint) \
         SELECT (SELECT MAX(position) FROM leaves), (SELECT MAX(ordinal) + 1 FROM entries), zeroblob(16), x'666f72676564', NULL",
    )
    .await;
}

// ── US6 ─────────────────────────────────────────────────────────────────────

/// FR-035, FR-050: after erasure the side table holds no text or salt, the
/// fingerprint is gone, and the entry still proves against the same head.
#[tokio::test]
async fn erase_leaves_no_text_salt_or_fingerprint() {
    let (_dir, path, store) = fresh_file().await;
    let (books, inv) = populated(store.clone()).await;
    let head = books.head().await.unwrap();
    let entry = entry_by_key_and_id(&store, "inv").await;
    assert!(entry.fingerprint.is_some());
    books.erase_memo(entry.id).await.unwrap();
    let mut db = raw(&path).await;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT m.salt IS NULL, m.text IS NULL, e.fingerprint IS NULL FROM memos m JOIN entries e ON e.position = m.position WHERE m.position = {inv}"
    )))
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        (
            row.get::<bool, _>(0),
            row.get::<bool, _>(1),
            row.get::<bool, _>(2)
        ),
        (true, true, true)
    );
    assert_eq!(books.head().await.unwrap(), head);
    assert_eq!(entry_by_key_and_id(&store, "inv").await.fingerprint, None);
    books.audit().await.unwrap();
}

/// US4-6 (T037): of 5 000 holds, 4 990 resolved, the due read returns the
/// 10 open ones in (expiry, position) order, from the open-hold index.
#[tokio::test]
async fn us4_due_holds_read_only_open_ones() {
    use doubleentry::store::HoldRead;
    let (_dir, _path, store) = fresh_file().await;
    let (_, books) = store_checks::books(store.clone()).await;
    let mut open = Vec::new();
    for n in 0..5_000 {
        let hold = doubleentry::Entry::open_hold(format!("hold-{n}"), date!("2026-01-15"))
            .expires(date!("2026-01-20"))
            .debit("Assets:Cash", store_checks::eur("1.00"), EUR)
            .credit("Income:Sales", store_checks::eur("1.00"), EUR);
        let hold = books.record(hold).await.unwrap();
        if n % 500 == 0 {
            open.push(hold.position);
        } else {
            let void =
                doubleentry::Entry::void_hold(format!("void-{n}"), date!("2026-01-16"), hold.id);
            books.record(void).await.unwrap();
        }
    }
    let due = HoldRead::Due {
        on_or_before: date!("2026-01-31"),
        page: PageRead::first(100),
    };
    let due = store.holds(&due).await.unwrap();
    assert_eq!(
        due.items
            .iter()
            .map(|hold| hold.position)
            .collect::<Vec<_>>(),
        open
    );
}

// ── Round-3 audit regressions (2026-10-16) ──────────────────────────────────

/// A3-S1, A3-C1, A3-C2, A3-S3: every table is compared with the replay, so
/// a change to any persisted column fails the audit — the 16 tampers that
/// passed before, and the seal nodes and posting dimensions beside them.
// PoC: A3-S1 a3_s1_audit_misses_index_tampering a3_s1_booking_tamper_changes_statements_silently
#[tokio::test]
async fn a3_s1_the_audit_compares_every_table() {
    for tamper in [
        "UPDATE postings SET booking = booking + 400",
        "UPDATE postings SET value_date = value_date + 400",
        "UPDATE postings SET dimensions = ''; DELETE FROM posting_dims",
        "DELETE FROM posting_dims",
        "UPDATE account_versions SET limit_code = 0 WHERE field = 1",
        "UPDATE account_versions SET open_to = open_from WHERE field = 0",
        "UPDATE account_versions SET class = 'forged' WHERE field = 2",
        "UPDATE accounts SET kind = 0 WHERE kind = 3",
        "UPDATE currency_versions SET exponent = 0",
        "UPDATE policy_versions SET effective = effective + 1000",
        "UPDATE periods SET last_day = last_day + 1000 WHERE period_id = '2026-12'",
        "UPDATE seal_leaves SET debit = credit",
        "UPDATE seal_nodes SET hash = zeroblob(32)",
        "UPDATE slices SET debit = credit",
        "DELETE FROM clearing_refs",
        "UPDATE entries SET fingerprint = zeroblob(32) WHERE fingerprint IS NOT NULL",
        "UPDATE entries SET fingerprint = NULL WHERE idem_key = CAST('feb' AS BLOB)",
        "UPDATE entries SET ordinal = ordinal + 1000",
        "UPDATE nodes SET hash = zeroblob(32) WHERE id = 0",
        "UPDATE nodes SET hash = zeroblob(32) WHERE id = 2",
        "UPDATE cleared SET applied = zeroblob(16)",
        "UPDATE outstanding SET debit = credit",
        "UPDATE holds SET expires = 0",
        "UPDATE meta SET genesis = zeroblob(32)",
    ] {
        audit_catches(tamper).await;
    }
}

/// A3-S1 under the least-privilege grants: an inserted master-data version
/// (no trigger fires on INSERT) fails the audit.
// PoC: A3-S1 a3_s1_unaudited_limit_index_poisons_the_log
#[tokio::test]
async fn a3_s1_an_inserted_limit_version_fails_the_audit() {
    let (_dir, path, store) = fresh_file().await;
    let (books, _) = populated(store).await;
    let mut db = raw(&path).await;
    execute(
        &mut db,
        "INSERT INTO account_versions (handle, field, effective, position, limit_code) \
         SELECT a.handle, 1, 0, (SELECT MAX(position) FROM leaves), 0 FROM accounts a \
         WHERE a.path = 'Liabilities:Wallet'",
    )
    .await
    .unwrap();
    let error = books.audit().await.unwrap_err();
    assert!(matches!(error, Error::Integrity(_)), "{error}");
}

/// A3-C1 (limit row rewritten) and A3-C2 (a posting moved to December):
/// both fail the audit.
// PoC: A3-C1 a3_c1_audit_passes_tampered_account_limit_and_overdraft_is_accepted
// PoC: A3-C2 a3_c2_audit_passes_moved_posting_booking_date
#[tokio::test]
async fn a3_c1_c2_master_data_and_posting_dates_are_audited() {
    audit_catches("UPDATE account_versions SET limit_code = 0 WHERE field = 1 AND limit_code = 2")
        .await;
    let december = date!("2026-12-01").to_julian_day();
    audit_catches(&format!(
        "UPDATE postings SET booking = {december} WHERE position = (SELECT MAX(position) FROM entries)"
    ))
    .await;
}

/// A3-S1 impact: a tampered stored node is never served as a proof — the
/// inclusion proof is verified before it is handed out — and tampered seal
/// leaves fail the audit and every proof of the period.
// PoC: A3-S1 a3_s1_audit_passing_store_serves_bad_proofs
#[tokio::test]
async fn a3_s1_tampered_nodes_are_never_served_as_proofs() {
    let (_dir, path, store) = fresh_file().await;
    let (books, inv) = populated(store).await;
    let sibling = doubleentry::store::stored_index(0, inv ^ 1).unwrap();
    let mut db = raw(&path).await;
    drop_triggers(&mut db).await;
    execute(
        &mut db,
        &format!("UPDATE nodes SET hash = zeroblob(32) WHERE id = {sibling}"),
    )
    .await
    .unwrap();
    let error = books
        .prove_entry(store_checks::id("inv"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Integrity(_)), "{error}");
    assert!(books.audit().await.is_err());

    let (_dir, path, store) = fresh_file().await;
    let (books, _) = populated(store).await;
    let mut db = raw(&path).await;
    drop_triggers(&mut db).await;
    execute(
        &mut db,
        "UPDATE seal_leaves SET debit = credit, credit = debit",
    )
    .await
    .unwrap();
    assert!(books.audit().await.is_err());
    let proved = books
        .prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-01")
        .await;
    assert!(matches!(proved, Err(Error::Integrity(_))), "{proved:?}");
}

/// The primary key columns of `table`, in key order.
async fn primary_key(db: &mut SqliteConnection, table: &str) -> Vec<String> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT name FROM pragma_table_info('{table}') WHERE pk > 0 ORDER BY pk"
    )))
    .fetch_all(&mut *db)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

/// A3-S2: with every trigger in place, `REPLACE` is refused on every log
/// and index table from any connection (an insert repeating a key is
/// refused), and on the maintained tables too from a connection with
/// `recursive_triggers` on — as every store connection runs, so the implicit
/// delete fires the delete trigger. (From a connection without it, a
/// `REPLACE` of a maintained value is an update, which they allow, and the
/// audit compares.) `UPSERT` is an update: refused everywhere but on the
/// maintained values, which the write path upserts.
// PoC: A3-S2 a3_s2_sqlite_replace_bypasses_every_trigger
#[tokio::test]
async fn a3_s2_replace_and_upsert_are_refused_on_every_record_table() {
    const MAINTAINED: [&str; 4] = ["balances", "cleared", "outstanding", "slices"];
    let (_dir, path, store) = fresh_file().await;
    let (books, _) = populated(store).await;
    for recursive in [false, true] {
        let mut db = raw(&path).await;
        if recursive {
            execute(&mut db, "PRAGMA recursive_triggers = ON")
                .await
                .unwrap();
        }
        let tables: Vec<String> = sqlx::query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&mut db)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
        assert_eq!(tables.len(), 23);
        for table in &tables {
            let maintained = MAINTAINED.contains(&table.as_str());
            let replace = execute(
                &mut db,
                &format!("INSERT OR REPLACE INTO {table} SELECT * FROM {table} LIMIT 1"),
            )
            .await;
            // Without recursive triggers, `clearing_refs` (whose key is all
            // its columns) gets the row back as it was.
            if !recursive && (table == "clearing_refs" || maintained) {
                replace.unwrap();
            } else {
                let error = replace.expect_err(&format!("REPLACE on {table} must be refused"));
                let message = error.as_database_error().unwrap().message().to_owned();
                assert!(message.starts_with("doubleentry:"), "{table}: {message}");
            }
            let key = primary_key(&mut db, table).await.join(", ");
            let column = primary_key(&mut db, table).await.remove(0);
            let upsert = execute(
                &mut db,
                &format!(
                    "INSERT INTO {table} SELECT * FROM {table} WHERE true LIMIT 1 \
                     ON CONFLICT ({key}) DO UPDATE SET {column} = excluded.{column}"
                ),
            )
            .await;
            if maintained {
                upsert.unwrap();
            } else {
                let error = upsert.expect_err(&format!("UPSERT on {table} must be refused"));
                let message = error.as_database_error().unwrap().message().to_owned();
                assert!(message.starts_with("doubleentry:"), "{table}: {message}");
            }
        }
    }
    books.audit().await.unwrap();
}

/// A3-S3: a fingerprint is nulled only together with the erasure of its
/// entry's memo; nulling one of an entry without a memo, or with its memo
/// kept, is refused.
// PoC: A3-S3 a3_s3_fingerprint_forgery_is_invisible_to_audit
#[tokio::test]
async fn a3_s3_a_fingerprint_is_nulled_only_with_its_memo() {
    let (_dir, path, store) = fresh_file().await;
    let (books, _) = populated(store).await;
    let mut db = raw(&path).await;
    for key in ["feb", "refund"] {
        refused(
            execute(
                &mut db,
                &format!(
                    "UPDATE entries SET fingerprint = NULL WHERE idem_key = CAST('{key}' AS BLOB)"
                ),
            )
            .await,
            "UPDATE",
            "entries",
        );
    }
    drop(db);
    books.erase_memo(store_checks::id("refund")).await.unwrap();
    books.audit().await.unwrap();
    // The retry of an erased draft is a key conflict; of a kept one, a replay.
    let retry = books
        .record(sale("feb", date!("2026-02-10"), "7.00"))
        .await
        .unwrap();
    assert!(retry.replayed);
}

/// A3-C9: an entry read checks that the id it was asked for derives from
/// the key its leaf commits to; with two ids swapped in the index the read,
/// the reversal built on it and a reversal recorded against it are refused
/// as integrity errors, and the audit fails.
// PoC: A3-C9 a3_c9_entry_read_does_not_check_derived_id
#[tokio::test]
async fn a3_c9_swapped_entry_ids_are_refused() {
    let (_dir, path, store) = fresh_file().await;
    let (_, books) = store_checks::books(store).await;
    books
        .record(sale("fund", date!("2026-03-01"), "100.00"))
        .await
        .unwrap();
    books
        .record(sale("rent", date!("2026-03-03"), "7.00"))
        .await
        .unwrap();
    let fund = store_checks::id("fund");
    let reversal = books
        .reversal(fund, "storno-fund", date!("2026-03-04"))
        .await
        .unwrap();
    let mut db = raw(&path).await;
    drop_triggers(&mut db).await;
    execute(&mut db, "PRAGMA foreign_keys = OFF").await.unwrap();
    execute(
        &mut db,
        "CREATE TEMP TABLE ids AS SELECT ordinal, entry_id FROM entries; \
         UPDATE entries SET entry_id = zeroblob(16) WHERE ordinal = 0; \
         UPDATE entries SET entry_id = (SELECT entry_id FROM ids WHERE ordinal = 0) WHERE ordinal = 1; \
         UPDATE entries SET entry_id = (SELECT entry_id FROM ids WHERE ordinal = 1) WHERE ordinal = 0;",
    )
    .await
    .unwrap();
    drop(db);
    assert!(matches!(books.entry(fund).await, Err(Error::Integrity(_))));
    assert!(matches!(
        books.reversal(fund, "storno-2", date!("2026-03-04")).await,
        Err(Error::Integrity(_))
    ));
    assert!(matches!(
        books.prove_entry(fund).await,
        Err(Error::Integrity(_))
    ));
    let recorded = books.record(reversal).await;
    assert!(matches!(recorded, Err(Error::Integrity(_))), "{recorded:?}");
    assert!(matches!(books.audit().await, Err(Error::Integrity(_))));
}

/// A3-S10: every write re-reads the meta row with the header; a database
/// whose ledger changed under a running store is refused, never written
/// with the old ledger id.
#[tokio::test]
async fn a3_s10_a_write_rechecks_the_ledger() {
    let (_dir, path, store) = fresh_file().await;
    let (_, books) = store_checks::books(store).await;
    let mut db = raw(&path).await;
    drop_triggers(&mut db).await;
    execute(&mut db, "UPDATE meta SET ledger_id = 'example.com/other'")
        .await
        .unwrap();
    drop(db);
    let error = books
        .record(sale("after", date!("2026-03-01"), "1.00"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error}");
}

/// A3-S8: two stores opened independently on one file (two pools) race
/// appends, limited draws and a hold's resolutions: the file lock holds
/// across them.
#[tokio::test(flavor = "multi_thread")]
async fn a3_s8_independent_handles_race_on_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = file(dir.path());
    let first = SqlStore::<Sqlite>::sqlite(&url(&path)).await.unwrap();
    let second = SqlStore::<Sqlite>::sqlite(&url(&path)).await.unwrap();
    doubleentry::testing::independent_handles(first, second)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

/// A3-S6: balances after a position early in the log, paged one at a time,
/// are exactly the keys with postings below it — one statement per page.
#[tokio::test]
// PoC: A3-S6 a3_s10_balances_at_walks_every_key
async fn a3_s6_balances_at_an_early_position_page_correctly() {
    let (_dir, _path, store) = fresh_file().await;
    let (_, books) = store_checks::books(store.clone()).await;
    books
        .record(sale("first", date!("2026-01-02"), "1.00"))
        .await
        .unwrap();
    let early = books.head().await.unwrap().size;
    for n in 0..40 {
        let path = format!("Assets:Bank:{n}");
        books
            .open(
                path.as_str(),
                doubleentry::AccountKind::Asset,
                date!("2026-01-01"),
            )
            .await
            .unwrap();
        books
            .record(
                doubleentry::Entry::new(format!("s{n}"), date!("2026-01-03"))
                    .debit(path.as_str(), store_checks::eur("1.00"), EUR)
                    .credit("Income:Sales", store_checks::eur("1.00"), EUR),
            )
            .await
            .unwrap();
    }
    let mut page = PageRead::first(1);
    let mut keys = Vec::new();
    loop {
        let read = store
            .balances(&BalanceRead {
                scope: BalanceScope::At(early),
                keys: None,
                page: page.clone(),
            })
            .await
            .unwrap();
        keys.extend(read.items.iter().map(|(key, _)| key.handle.get()));
        match read.next {
            Some(next) => page = page.after(next),
            None => break,
        }
    }
    assert_eq!(keys, [0, 1]);
}
