//! `SqlStore<Postgres>` against a real PostgreSQL 17 (testcontainers): the
//! conformance suite, the store acceptance scenarios, isolation and locking,
//! the database's own refusals, least privilege and the storage
//! regressions.
#![cfg(all(feature = "postgres", feature = "testing"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod store_checks;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use doubleentry::store::{Postgres, SqlOptions, SqlStore, Store, StoreErrorKind};
use doubleentry::testing::conformance;
use doubleentry::{
    AccountKind, Entry, Error, Ledger, LedgerId, currency::EUR, date, record::Genesis,
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Row};
use store_checks::{entry_by_key_and_id, eur, ledger, populated, sale};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres as Image;

/// A running server: the container and its URL.
type Server = (ContainerAsync<Image>, String);

static ALPINE: tokio::sync::OnceCell<Server> = tokio::sync::OnceCell::const_new();
static DEBIAN: tokio::sync::OnceCell<Server> = tokio::sync::OnceCell::const_new();
static SCHEMAS: AtomicU64 = AtomicU64::new(0);

async fn start(tag: &str) -> Server {
    let container = Image::default().with_tag(tag).start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (
        container,
        format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
    )
}

/// The shared `postgres:17-alpine` server.
async fn url() -> &'static str {
    &ALPINE.get_or_init(|| start("17-alpine")).await.1
}

/// The shared Debian `postgres:17` server (`en_US.utf8`).
async fn debian() -> &'static str {
    &DEBIAN.get_or_init(|| start("17")).await.1
}

fn next_schema() -> String {
    format!("ledger_{}", SCHEMAS.fetch_add(1, Ordering::Relaxed))
}

async fn store_in(url: &str, schema: &str) -> SqlStore<Postgres> {
    let options = SqlOptions {
        schema: schema.to_owned(),
        max_connections: 4,
        ..SqlOptions::default()
    };
    SqlStore::postgres_with(url, options).await.unwrap()
}

async fn fresh() -> SqlStore<Postgres> {
    store_in(url().await, &next_schema()).await
}

/// A fresh store and its schema.
async fn fresh_named() -> (SqlStore<Postgres>, String) {
    let schema = next_schema();
    (store_in(url().await, &schema).await, schema)
}

async fn raw(url: &str) -> PgConnection {
    PgConnection::connect(url).await.unwrap()
}

async fn execute(connection: &mut PgConnection, sql: &str) -> Result<u64, sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(&mut *connection)
        .await
        .map(|done| done.rows_affected())
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_conforms() {
    let report = conformance(fresh)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(
        report.total() < Duration::from_secs(180),
        "SC-001: {:?}",
        report.total()
    );
    println!("{} checks in {:?}", report.checks.len(), report.total());
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_debian_conforms() {
    let report = conformance(|| async { store_in(debian().await, &next_schema()).await })
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
    println!("{} checks in {:?}", report.checks.len(), report.total());
}

// ── US1 ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn us1_create_then_open() {
    store_checks::us1_create_then_open(fresh().await).await;
}

#[tokio::test]
async fn us1_journal_parity() {
    store_checks::us1_journal_parity(fresh().await).await;
}

#[tokio::test]
async fn us1_rejection_writes_nothing() {
    store_checks::us1_rejection_writes_nothing(fresh().await).await;
}

#[tokio::test]
async fn us1_replay_and_key_conflict() {
    store_checks::us1_replay_and_key_conflict(fresh().await).await;
}

#[tokio::test]
async fn us1_open_before_create_is_not_created() {
    store_checks::us1_open_before_create_is_not_created(fresh().await).await;
}

#[tokio::test]
async fn seal_through_and_exponents() {
    store_checks::seal_through_and_exponents(fresh().await).await;
}

#[tokio::test]
async fn us1_other_genesis_refused_identical_accepted() {
    store_checks::us1_other_genesis_refused_identical_accepted(fresh().await).await;
}

#[tokio::test]
async fn us4_postings_pages_with_filters() {
    store_checks::us4_postings_pages_with_filters(fresh().await).await;
}

#[tokio::test]
async fn us4_proofs_at_historical_sizes() {
    store_checks::us4_proofs_at_historical_sizes(fresh().await).await;
}

/// `regress_f6_foreign_schema_is_refused` (PoC: F6), on PostgreSQL.
#[tokio::test]
// PoC: F6 poc_f6_migrate_over_a_0_6_database_is_silent_and_wrong
async fn regress_f6_foreign_schema_is_refused() {
    let (store, schema) = fresh_named().await;
    let mut db = raw(url().await).await;
    execute(
        &mut db,
        &format!(
            "CREATE SCHEMA {schema}; CREATE TABLE {schema}.meta (singleton BIGINT PRIMARY KEY, schema_id TEXT, \
             ledger_id TEXT, amount_scale BIGINT, genesis BYTEA); \
             INSERT INTO {schema}.meta VALUES (1, 'doubleentry/0', 'example.com/acme', 2, '\\x00');"
        ),
    )
    .await
    .unwrap();
    let expected = StoreErrorKind::SchemaMismatch {
        found: String::from("doubleentry/0"),
    };
    assert_eq!(store.open().await.unwrap_err().kind(), &expected);
    assert_eq!(
        store
            .create(&Genesis { ledger: ledger() })
            .await
            .unwrap_err()
            .kind(),
        &expected
    );
    let tables: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '{schema}'"
    )))
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(tables, 1, "nothing was created");
}

// ── US2 ─────────────────────────────────────────────────────────────────────

/// `regress_g2b_repeatable_read_default_is_overridden` (PoC: G2/G2b): with
/// the database's default isolation `repeatable read`, 16 concurrent appends
/// all succeed at dense positions — the pipeline pins `READ COMMITTED`.
#[tokio::test(flavor = "multi_thread")]
// PoC: G2b poc_g2b_inline_appends_fail_under_repeatable_read (G2 poc_g2_deferred_limit_breach_under_repeatable_read merged)
async fn regress_g2b_repeatable_read_default_is_overridden() {
    let mut db = raw(url().await).await;
    execute(&mut db, "CREATE DATABASE g2b").await.unwrap();
    execute(
        &mut db,
        "ALTER DATABASE g2b SET default_transaction_isolation = 'repeatable read'",
    )
    .await
    .unwrap();
    let g2b = format!("{}/g2b", url().await.rsplit_once('/').unwrap().0);
    let store = SqlStore::<Postgres>::postgres_with(
        &g2b,
        SqlOptions {
            max_connections: 16,
            ..SqlOptions::default()
        },
    )
    .await
    .unwrap();
    let (_, books) = store_checks::books(store).await;
    let start = books.head().await.unwrap().size;
    let tasks: Vec<_> = (0..16)
        .map(|n| {
            let books = books.clone();
            tokio::spawn(async move {
                books
                    .record(sale(&format!("rr-{n}"), date!("2026-01-15"), "1.00"))
                    .await
            })
        })
        .collect();
    let mut positions = Vec::new();
    for task in tasks {
        positions.push(task.await.unwrap().unwrap().position);
    }
    positions.sort_unstable();
    assert_eq!(positions, (start..start + 16).collect::<Vec<_>>());
}

/// The advisory lock key of ledger `id` in `schema` (FR-018; amended
/// 2026-10-16, A3-S11: keyed by the schema and the ledger id).
fn lock_key(schema: &str, id: &str) -> i64 {
    let digest = Sha256::new()
        .chain_update("doubleentry/ledger-lock")
        .chain_update(format!("{schema}\u{0}{id}"))
        .finalize();
    i64::from_be_bytes(digest[..8].try_into().unwrap())
}

/// `regress_g3_two_schemas_two_locks` (PoC: G3): while ledger A's lock is
/// held, an append to ledger B in another schema completes; A's waits.
#[tokio::test(flavor = "multi_thread")]
// PoC: G3 poc_g3_advisory_lock_is_shared_across_ledgers
async fn regress_g3_two_schemas_two_locks() {
    let (store_a, schema_a) = fresh_named().await;
    let a = Ledger::new(store_a, LedgerId::new("example.com/a").unwrap())
        .await
        .unwrap();
    let b = Ledger::new(fresh().await, LedgerId::new("example.com/b").unwrap())
        .await
        .unwrap();
    let mut holder = raw(url().await).await;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_key(&schema_a, "example.com/a"))
        .execute(&mut holder)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        b.declare_currency(EUR, 2, date!("2026-01-01")),
    )
    .await
    .expect("ledger B does not wait for ledger A")
    .unwrap();
    let waiting = tokio::time::timeout(
        Duration::from_millis(500),
        a.declare_currency(EUR, 2, date!("2026-01-01")),
    )
    .await;
    assert!(waiting.is_err(), "ledger A waits for its lock");
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_key(&schema_a, "example.com/a"))
        .execute(&mut holder)
        .await
        .unwrap();
    a.declare_currency(EUR, 2, date!("2026-01-01"))
        .await
        .unwrap();
}

/// An open `REPEATABLE READ` reader does not delay a write.
#[tokio::test(flavor = "multi_thread")]
async fn readers_never_block() {
    let (store, schema) = fresh_named().await;
    let (_, books) = store_checks::books(store).await;
    books
        .record(sale("first", date!("2026-01-15"), "1.00"))
        .await
        .unwrap();
    let mut reader = raw(url().await).await;
    execute(&mut reader, "BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    execute(
        &mut reader,
        &format!("SELECT * FROM {schema}.postings; SELECT * FROM {schema}.balances"),
    )
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        books.record(sale("second", date!("2026-01-15"), "1.00")),
    )
    .await
    .expect("the write does not wait for the reader")
    .unwrap();
    execute(&mut reader, "ROLLBACK").await.unwrap();
}

// ── US4 ─────────────────────────────────────────────────────────────────────

/// The plan of `sql` in `schema` with sequential and bitmap scans
/// discouraged (the test tables are tiny): whether an index serves the read
/// in order.
async fn plan(db: &mut PgConnection, schema: &str, sql: &str) -> String {
    execute(db, "SET enable_seqscan = off; SET enable_bitmapscan = off")
        .await
        .unwrap();
    execute(db, &format!("SET search_path = {schema}"))
        .await
        .unwrap();
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
        .fetch_all(&mut *db)
        .await
        .unwrap();
    rows.iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Postings pages are index range scans without a sort (Q-S8); sealing
/// reads `postings_by_day`; due holds read `holds_open_due`.
#[tokio::test]
async fn plan_reads_use_their_indexes_without_sorting() {
    let (store, schema) = fresh_named().await;
    populated(store).await;
    let mut db = raw(url().await).await;
    let by_position = plan(
        &mut db,
        &schema,
        "SELECT p.position, p.idx FROM postings p WHERE p.handle = 0 AND p.currency = 'EUR' AND p.layer = 0 \
         AND p.position < 100 AND (p.position, p.idx) > (-1, -1) ORDER BY p.position, p.idx LIMIT 10",
    )
    .await;
    assert!(
        by_position.contains("postings_by_key") && !by_position.contains("Sort"),
        "{by_position}"
    );
    let by_booking = plan(
        &mut db,
        &schema,
        "SELECT p.position, p.idx FROM postings p WHERE p.handle = 0 AND p.currency = 'EUR' AND p.layer = 0 \
         AND p.position < 100 AND (p.booking, p.position, p.idx) > (0, -1, -1) ORDER BY p.booking, p.position, p.idx LIMIT 10",
    )
    .await;
    assert!(
        by_booking.contains("postings_by_key_day") && !by_booking.contains("Sort"),
        "{by_booking}"
    );
    let after = plan(
        &mut db,
        &schema,
        "SELECT handle FROM postings WHERE booking > 0 ORDER BY booking, position, idx",
    )
    .await;
    assert!(
        after.contains("postings_by_day") && !after.contains("Sort"),
        "{after}"
    );
    let due = plan(
        &mut db,
        &schema,
        "SELECT position FROM holds WHERE resolved_by IS NULL AND expires IS NOT NULL AND expires <= 9 \
         AND (expires, position) > (0, -1) AND position < 100 ORDER BY expires, position LIMIT 10",
    )
    .await;
    assert!(
        due.contains("holds_open_due") && !due.contains("Sort"),
        "{due}"
    );
}

/// `regress_qs6_byte_order_on_every_collation` (PoC: Q-S6): on a database
/// collating `en_US.utf8`, paths and dimension values compare by bytes — the
/// leaf rule and year-end closing answer exactly as in memory.
#[tokio::test(flavor = "multi_thread")]
// PoC: Q-S6 q_s6_dimension_values_order_depends_on_collation
// PoC: Q-S6 q_s6_baseline_dimension_values_byte_order_on_sqlite_and_memory
async fn regress_qs6_byte_order_on_every_collation() {
    let mut db = raw(debian().await).await;
    let collation: String =
        sqlx::query_scalar("SELECT datcollate FROM pg_database WHERE datname = 'postgres'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(
        collation.starts_with("en_US"),
        "the database collates {collation}"
    );
    let store = store_in(debian().await, &next_schema()).await;
    let (mut journal, books) = store_checks::books(store).await;
    let names = ["apple", "émigré", "Network", "Zebra"];
    for name in names {
        let path = format!("Income:{name}");
        assert_eq!(
            journal
                .open(&path, AccountKind::Income, date!("2026-01-01"))
                .unwrap(),
            books
                .open(&path, AccountKind::Income, date!("2026-01-01"))
                .await
                .unwrap()
        );
    }
    // `Income:apple:x` makes `Income:apple` a parent under byte order only.
    journal
        .open("Income:apple:x", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:apple:x", AccountKind::Income, date!("2026-01-01"))
        .await
        .unwrap();
    for (n, name) in names.iter().enumerate() {
        for value in names {
            let entry = Entry::new(format!("s-{n}-{value}"), date!("2026-02-10"))
                .debit("Assets:Cash", eur("1.00"), EUR)
                .posting(
                    doubleentry::Posting::credit(format!("Income:{name}"), eur("1.00"), EUR)
                        .dimension("segment", value),
                );
            let expected = journal
                .record(entry.clone())
                .map_err(|error| error.rejection().map(|r| r.code()));
            let actual = books
                .record(entry)
                .await
                .map_err(|error| error.rejection().map(|r| r.code()));
            assert_eq!(expected, actual, "Income:{name} / {value}");
        }
    }
    let expected = journal
        .close_year("close", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    let actual = books
        .close_year("close", date!("2026-12-31"), "Equity:Retained")
        .await
        .unwrap();
    assert_eq!(expected, actual);
}

// ── US5 ─────────────────────────────────────────────────────────────────────

const APPEND_ONLY: [(&str, &str); 15] = [
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
    ("clearings", "idem_key"),
    ("clearing_resets", "position"),
    ("clearing_refs", "layer"),
];

const ALL_TABLES: [&str; 22] = [
    "meta",
    "leaves",
    "nodes",
    "accounts",
    "account_versions",
    "currency_versions",
    "policy_versions",
    "periods",
    "entries",
    "postings",
    "posting_dims",
    "memos",
    "holds",
    "seals",
    "seal_leaves",
    "clearings",
    "clearing_resets",
    "clearing_refs",
    "balances",
    "cleared",
    "outstanding",
    "slices",
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

/// FR-032, FR-033, FR-050 on PostgreSQL, `TRUNCATE` included.
#[tokio::test]
async fn immutability_is_enforced_by_the_database() {
    let (store, s) = fresh_named().await;
    let (_books, inv) = populated(store).await;
    let mut db = raw(url().await).await;
    for (table, column) in APPEND_ONLY {
        refused(
            execute(
                &mut db,
                &format!("UPDATE {s}.{table} SET {column} = {column}"),
            )
            .await,
            "UPDATE",
            table,
        );
        refused(
            execute(&mut db, &format!("DELETE FROM {s}.{table}")).await,
            "DELETE",
            table,
        );
    }
    for table in ALL_TABLES {
        refused(
            execute(&mut db, &format!("TRUNCATE {s}.{table} CASCADE")).await,
            "TRUNCATE",
            "",
        );
    }
    // The populated ledger has a row in every maintained table, the
    // amounts open holds reserve included.
    for table in ["balances", "cleared", "outstanding", "slices"] {
        refused(
            execute(&mut db, &format!("DELETE FROM {s}.{table}")).await,
            "DELETE",
            table,
        );
    }
    execute(
        &mut db,
        &format!("UPDATE {s}.balances SET postings = postings"),
    )
    .await
    .unwrap();
    refused(
        execute(&mut db, &format!("UPDATE {s}.memos SET text = 'other'")).await,
        "UPDATE",
        "memos",
    );
    refused(
        execute(&mut db, &format!("DELETE FROM {s}.memos")).await,
        "DELETE",
        "memos",
    );
    assert_eq!(
        execute(
            &mut db,
            &format!("UPDATE {s}.memos SET salt = NULL, text = NULL WHERE position = {inv}")
        )
        .await
        .unwrap(),
        1
    );
    refused(
        execute(
            &mut db,
            &format!("UPDATE {s}.entries SET idem_key = '\\x01'"),
        )
        .await,
        "UPDATE",
        "entries",
    );
    refused(
        execute(
            &mut db,
            &format!("UPDATE {s}.entries SET fingerprint = '\\x{0:064}'", 0),
        )
        .await,
        "UPDATE",
        "entries",
    );
    refused(
        execute(&mut db, &format!("DELETE FROM {s}.entries")).await,
        "DELETE",
        "entries",
    );
    assert_eq!(
        execute(
            &mut db,
            &format!("UPDATE {s}.entries SET fingerprint = NULL WHERE position = {inv}")
        )
        .await
        .unwrap(),
        1
    );
    execute(
        &mut db,
        &format!("INSERT INTO {s}.holds (position, entry_id, booking, debit_handle, credit_handle, currency, amount) \
                  SELECT position, entry_id, 0, 0, 1, 'EUR', '\\x{0:032}' FROM {s}.entries WHERE position = {inv}", 0),
    )
    .await
    .unwrap();
    let resolve = format!(
        "UPDATE {s}.holds SET resolved_by = (SELECT entry_id FROM {s}.entries WHERE position <> {inv} \
         AND entry_id NOT IN (SELECT resolved_by FROM {s}.holds WHERE resolved_by IS NOT NULL) LIMIT 1), \
         how = 1 WHERE position = {inv}"
    );
    refused(
        execute(&mut db, &format!("UPDATE {s}.holds SET booking = 1")).await,
        "UPDATE",
        "holds",
    );
    assert_eq!(execute(&mut db, &resolve).await.unwrap(), 1);
    refused(execute(&mut db, &resolve).await, "UPDATE", "holds");
    refused(
        execute(&mut db, &format!("DELETE FROM {s}.holds")).await,
        "DELETE",
        "holds",
    );
}

/// Runs `tamper` (with `{s}` the schema) with triggers disabled; the audit
/// must then fail as an integrity failure.
async fn audit_catches(tamper: &str) {
    let (store, schema) = fresh_named().await;
    let (books, _) = populated(store).await;
    let mut db = raw(url().await).await;
    execute(&mut db, "SET session_replication_role = replica")
        .await
        .unwrap();
    let tamper = tamper.replace("{s}", &schema);
    assert!(
        execute(&mut db, &tamper).await.unwrap() > 0,
        "{tamper} changed nothing"
    );
    let error = books.audit().await.expect_err(&tamper);
    assert!(matches!(error, Error::Integrity(_)), "{tamper}: {error}");
}

/// SC-007 on PostgreSQL.
#[tokio::test]
async fn tampering_fails_the_audit() {
    audit_catches(
        "UPDATE {s}.leaves SET bytes = overlay(bytes placing '\\xff' from octet_length(bytes)) \
         WHERE position = (SELECT MIN(position) FROM {s}.entries)",
    )
    .await;
    audit_catches(
        "CREATE TEMP TABLE ids AS SELECT ordinal, entry_id FROM {s}.entries WHERE ordinal IN (0, 1); \
         UPDATE {s}.entries SET entry_id = '\\x00000000000000000000000000000000' WHERE ordinal = 0; \
         UPDATE {s}.entries SET entry_id = (SELECT entry_id FROM ids WHERE ordinal = 0) WHERE ordinal = 1; \
         UPDATE {s}.entries SET entry_id = (SELECT entry_id FROM ids WHERE ordinal = 1) WHERE ordinal = 0",
    )
    .await;
    audit_catches(
        "UPDATE {s}.postings SET debit_after = credit_after WHERE handle = 0 AND idx = 0",
    )
    .await;
    audit_catches("UPDATE {s}.balances SET postings = postings + 1 WHERE handle = 0").await;
}

/// 023 SC-006 on PostgreSQL: a resolved hold marked open, an open hold
/// marked resolved, an edited reserved amount.
#[tokio::test]
async fn us5_audit_catches_hold_tampering() {
    audit_catches(
        "UPDATE {s}.holds SET resolved_by = NULL, how = NULL WHERE resolved_by IS NOT NULL",
    )
    .await;
    audit_catches(
        "UPDATE {s}.holds SET resolved_by = (SELECT entry_id FROM {s}.entries WHERE ordinal = 0), \
         how = 1 WHERE resolved_by IS NULL",
    )
    .await;
    audit_catches("UPDATE {s}.outstanding SET debit = credit WHERE debit <> credit").await;
}

/// `holds_resolution_link_is_committed` (023 R9; PoC: A1 / Q-E1) on
/// PostgreSQL: the immutability trigger is disabled for the tampering
/// statement only; pointing a resolution at another hold rewrites its leaf,
/// which the audit refuses.
#[tokio::test]
async fn holds_resolution_link_is_committed() {
    let (store, schema) = fresh_named().await;
    let (books, _) = populated(store.clone()).await;
    let (position, bytes) = store_checks::retargeted_resolution(&store).await;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut db = raw(url().await).await;
    let leaves = format!("{schema}.leaves");
    let triggers: Vec<String> = sqlx::query(
        "SELECT tgname::text FROM pg_trigger WHERE tgrelid = $1::regclass AND NOT tgisinternal",
    )
    .bind(&leaves)
    .fetch_all(&mut db)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect();
    assert!(!triggers.is_empty(), "the leaves table is guarded");
    let tamper = format!(
        "BEGIN; ALTER TABLE {leaves} DISABLE TRIGGER USER; \
         UPDATE {leaves} SET bytes = '\\x{hex}' WHERE position = {position}; \
         ALTER TABLE {leaves} ENABLE TRIGGER USER; COMMIT"
    );
    execute(&mut db, &tamper).await.unwrap();
    let error = books.audit().await.unwrap_err();
    assert!(matches!(error, Error::Integrity(_)), "{error}");
}

/// `regress_qs2_sparse_handle_fails_audit` (PoC: Q-S2), on PostgreSQL.
#[tokio::test]
// PoC: Q-S2 q_s2_sparse_account_handle_bricks_sealing
async fn regress_qs2_sparse_handle_fails_audit() {
    audit_catches("INSERT INTO {s}.accounts (handle, path, kind, position) SELECT 9, 'Assets:Sparse', 0, MAX(position) FROM {s}.entries").await;
}

/// `regress_g5_forged_entry_fails_audit` (PoC: G5), on PostgreSQL.
#[tokio::test]
// PoC: G5 poc_g5_entry_without_postings_passes_db_triggers
async fn regress_g5_forged_entry_fails_audit() {
    audit_catches(
        "INSERT INTO {s}.leaves (position, kind, bytes) SELECT (SELECT MAX(position) + 1 FROM {s}.leaves), kind, bytes \
         FROM {s}.leaves WHERE position = (SELECT MIN(position) FROM {s}.entries); \
         INSERT INTO {s}.entries (position, ordinal, entry_id, idem_key) \
         SELECT (SELECT MAX(position) FROM {s}.leaves), (SELECT MAX(ordinal) + 1 FROM {s}.entries), \
         '\\x00000000000000000000000000000000', '\\x666f72676564'",
    )
    .await;
}

/// `regress_g8_schema_name_is_an_identifier` (PoC: G8): schema names with
/// quotes, spaces and an option string address their schema as a quoted
/// identifier and change no server setting.
#[tokio::test]
// PoC: G8 poc_g8_schema_name_handling
// PoC: A3-S4 a3_s4_dollar_in_schema_name_redirects_writes
async fn regress_g8_schema_name_is_an_identifier() {
    // Amended 2026-10-16 (A3-S4): a schema name is `[A-Za-z_][A-Za-z0-9_]{0,62}`,
    // refused with a typed error otherwise — before anything connects.
    for refused in [
        "Led \"x\"",
        "led -c statement_timeout=1",
        "acme$1",
        "a{{s}}",
    ] {
        let options = SqlOptions {
            schema: refused.to_owned(),
            ..SqlOptions::default()
        };
        let error = SqlStore::<Postgres>::postgres_with(url().await, options)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            &StoreErrorKind::InvalidSchemaName {
                name: refused.to_owned()
            }
        );
    }
    // A valid name is quoted: its case is kept.
    let schema = format!("Ledger_G8_{}", SCHEMAS.fetch_add(1, Ordering::Relaxed));
    let store = store_in(url().await, &schema).await;
    let books = Ledger::new(store.clone(), ledger()).await.unwrap();
    books
        .declare_currency(EUR, 2, date!("2026-01-01"))
        .await
        .unwrap();
    assert_eq!(store.open().await.unwrap().size, 2);
    let mut db = raw(url().await).await;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
            .bind(&schema)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(exists, "{schema}");
}

/// `regress_qs4_concurrent_create` (PoC: Q-S4): 16 concurrent `create` calls
/// on an empty schema all succeed; one genesis is written.
#[tokio::test(flavor = "multi_thread")]
// PoC: Q-S4 q_s4_concurrent_migrate_fails
async fn regress_qs4_concurrent_create() {
    let schema = next_schema();
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let schema = schema.clone();
            tokio::spawn(async move {
                let store = store_in(url().await, &schema).await;
                store.create(&Genesis { ledger: ledger() }).await
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap().size, 1);
    }
    assert_eq!(
        store_in(url().await, &schema).await.size().await.unwrap(),
        1
    );
}

/// `regress_qs5_open_does_not_wait_for_readers` (PoC: Q-S5): `open` runs no
/// DDL, so an open reader does not delay it.
#[tokio::test(flavor = "multi_thread")]
// PoC: Q-S5 q_s5_startup_migrate_blocks_behind_an_open_reader
async fn regress_qs5_open_does_not_wait_for_readers() {
    let (store, schema) = fresh_named().await;
    let (_, books) = store_checks::books(store.clone()).await;
    books
        .record(sale("one", date!("2026-01-15"), "1.00"))
        .await
        .unwrap();
    let mut reader = raw(url().await).await;
    execute(
        &mut reader,
        &format!("BEGIN; SELECT * FROM {schema}.postings"),
    )
    .await
    .unwrap();
    let again = store_in(url().await, &schema).await;
    tokio::time::timeout(Duration::from_secs(1), again.open())
        .await
        .expect("open does not wait")
        .unwrap();
    execute(&mut reader, "ROLLBACK").await.unwrap();
}

/// FR-034: the suite passes under exactly the documented grants.
#[tokio::test(flavor = "multi_thread")]
async fn least_privilege_role_conforms() {
    let mut db = raw(url().await).await;
    execute(&mut db, "CREATE ROLE app LOGIN PASSWORD 'app'")
        .await
        .unwrap();
    let app = url().await.replace("postgres:postgres@", "app:app@");
    let report = conformance(|| {
        let app = app.clone();
        async move {
            let schema = next_schema();
            let owner = store_in(url().await, &schema).await;
            owner.create(&Genesis { ledger: LedgerId::new("example.com/conformance").unwrap() }).await.unwrap();
            let mut db = raw(url().await).await;
            execute(
                &mut db,
                &format!(
                    "GRANT USAGE ON SCHEMA {schema} TO app; \
                     GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA {schema} TO app; \
                     GRANT UPDATE ON {schema}.balances, {schema}.cleared, {schema}.outstanding, {schema}.slices TO app; \
                     GRANT UPDATE (resolved_by, how) ON {schema}.holds TO app; \
                     GRANT UPDATE (salt, text) ON {schema}.memos TO app; \
                     GRANT UPDATE (fingerprint) ON {schema}.entries TO app;"
                ),
            )
            .await
            .unwrap();
            store_in(&app, &schema).await
        }
    })
    .await
    .unwrap_or_else(|failure| panic!("{failure}"));
    println!("{} checks as `app`", report.checks.len());
}

/// FR-035 and FR-050 on PostgreSQL: no text, salt or fingerprint after
/// erasure.
#[tokio::test]
async fn erase_leaves_no_text_salt_or_fingerprint() {
    let (store, schema) = fresh_named().await;
    let (books, inv) = populated(store.clone()).await;
    let entry = entry_by_key_and_id(&store, "inv").await;
    books.erase_memo(entry.id).await.unwrap();
    let mut db = raw(url().await).await;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT m.salt IS NULL, m.text IS NULL, e.fingerprint IS NULL FROM {schema}.memos m \
         JOIN {schema}.entries e ON e.position = m.position WHERE m.position = {inv}"
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
    books.audit().await.unwrap();
}

/// R9: the column names, order and nullability of all 23 tables are the
/// same in both dialects.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn schema_columns_match() {
    use sqlx::SqliteConnection;
    let (store, schema) = fresh_named().await;
    store.create(&Genesis { ledger: ledger() }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = format!("sqlite://{}", dir.path().join("schema.db").display());
    doubleentry::store::SqlStore::<doubleentry::store::Sqlite>::sqlite(&file)
        .await
        .unwrap()
        .create(&Genesis { ledger: ledger() })
        .await
        .unwrap();
    let mut pg = raw(url().await).await;
    let mut lite = SqliteConnection::connect(&file).await.unwrap();
    for table in ALL_TABLES {
        let postgres: Vec<(String, bool)> = sqlx::query(
            "SELECT column_name, is_nullable = 'YES' FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
        )
        .bind(&schema)
        .bind(table)
        .fetch_all(&mut pg)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
        let sqlite: Vec<(String, bool)> =
            sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
                .fetch_all(&mut lite)
                .await
                .unwrap()
                .iter()
                .map(|row| {
                    (
                        row.get::<String, _>(1),
                        row.get::<i64, _>(3) == 0 && row.get::<i64, _>(5) == 0,
                    )
                })
                .collect();
        assert_eq!(
            postgres.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            sqlite.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "{table}"
        );
        assert!(!postgres.is_empty(), "{table}");
    }
}

// ── Round-3 audit regressions (2026-10-16) ──────────────────────────────────

/// A3-S5: a memo holding NUL is refused structurally (108) on PostgreSQL as
/// everywhere — never a backend failure of a valid draft.
// PoC: A3-S5 a3_s5_nul_in_memo_splits_the_dialects
#[tokio::test(flavor = "multi_thread")]
async fn a3_s5_a_nul_memo_is_refused_like_everywhere() {
    let (_, books) = store_checks::books(fresh().await).await;
    let error = books
        .record(sale("nul", date!("2026-03-15"), "1.00").memo("a\0b"))
        .await
        .unwrap_err();
    assert_eq!(error.rejection().map(|r| r.code()), Some(108), "{error}");
    books
        .record(sale("ok", date!("2026-03-15"), "1.00").memo("line\nbreak"))
        .await
        .unwrap();
    books.audit().await.unwrap();
}

/// A3-S1 under exactly the documented least-privilege grants: a version the
/// application role inserts (no trigger fires on INSERT) fails the audit.
// PoC: A3-S1 a3_s1_least_privilege_role_lifts_a_limit
#[tokio::test(flavor = "multi_thread")]
async fn a3_s1_an_app_role_insert_fails_the_audit() {
    let (owner, schema) = fresh_named().await;
    populated(owner).await;
    let mut db = raw(url().await).await;
    let _ = execute(&mut db, "CREATE ROLE a3app LOGIN PASSWORD 'app'").await;
    execute(
        &mut db,
        &format!(
            "GRANT USAGE ON SCHEMA {schema} TO a3app; \
             GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA {schema} TO a3app; \
             GRANT UPDATE ON {schema}.balances, {schema}.cleared, {schema}.outstanding, {schema}.slices TO a3app; \
             GRANT UPDATE (resolved_by, how) ON {schema}.holds TO a3app; \
             GRANT UPDATE (salt, text) ON {schema}.memos TO a3app; \
             GRANT UPDATE (fingerprint) ON {schema}.entries TO a3app;"
        ),
    )
    .await
    .unwrap();
    let app_url = url().await.replace("postgres:postgres@", "a3app:app@");
    let mut app = PgConnection::connect(&app_url).await.unwrap();
    execute(
        &mut app,
        &format!(
            "INSERT INTO {schema}.account_versions (handle, field, effective, position, limit_code) \
             SELECT a.handle, 1, 0, (SELECT MAX(position) FROM {schema}.leaves), 0 \
             FROM {schema}.accounts a WHERE a.path = 'Liabilities:Wallet'"
        ),
    )
    .await
    .unwrap();
    // The role may not null a fingerprint whose memo it did not erase.
    let refused = execute(
        &mut app,
        &format!("UPDATE {schema}.entries SET fingerprint = NULL WHERE idem_key = 'feb'::bytea"),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    let books = Ledger::new(store_in(&app_url, &schema).await, ledger())
        .await
        .unwrap();
    let error = books.audit().await.unwrap_err();
    assert!(matches!(error, Error::Integrity(_)), "{error}");
}

/// A3-S9: a `BEGIN` failing on a pooled connection the server closed ran
/// nothing, and is tried again on the next one: after every backend of the
/// pool is terminated, every write still succeeds.
// PoC: A3-S9 a3_s9_dead_pooled_connections_fail_writes
#[tokio::test(flavor = "multi_thread")]
async fn a3_s9_dead_pooled_connections_do_not_fail_writes() {
    // A database of its own, so terminating its backends touches no other
    // test.
    let mut admin = raw(url().await).await;
    let _ = execute(&mut admin, "CREATE DATABASE a3_s9").await;
    let database = format!("{}/a3_s9", url().await.rsplit_once('/').unwrap().0);
    let store = SqlStore::<Postgres>::postgres_with(
        &database,
        SqlOptions {
            schema: next_schema(),
            max_connections: 6,
            ..SqlOptions::default()
        },
    )
    .await
    .unwrap();
    let (_, books) = store_checks::books(store).await;
    let warm: Vec<_> = (0..6)
        .map(|n| {
            let books = books.clone();
            tokio::spawn(async move {
                books
                    .record(sale(&format!("w{n}"), date!("2026-03-15"), "1.00"))
                    .await
            })
        })
        .collect();
    for task in warm {
        task.await.unwrap().unwrap();
    }
    execute(
        &mut admin,
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = 'a3_s9' AND pid <> pg_backend_pid()",
    )
    .await
    .unwrap();
    for n in 0..10 {
        books
            .record(sale(&format!("x{n}"), date!("2026-03-15"), "1.00"))
            .await
            .unwrap_or_else(|error| panic!("write {n}: {error}"));
    }
    books.audit().await.unwrap();
}

/// A3-S8: two stores opened independently on one schema (two pools) race
/// appends, limited draws and a hold's resolutions: the advisory lock holds
/// across them.
#[tokio::test(flavor = "multi_thread")]
async fn a3_s8_independent_pools_race_on_one_schema() {
    let schema = next_schema();
    let first = store_in(url().await, &schema).await;
    let second = store_in(url().await, &schema).await;
    first
        .create(&Genesis {
            ledger: LedgerId::new("example.com/conformance").unwrap(),
        })
        .await
        .unwrap();
    doubleentry::testing::independent_handles(first, second)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

/// A3-S10: every write re-reads the meta row with the header: a schema
/// re-created under a running store with another ledger is refused, never
/// written with the old ledger id.
#[tokio::test(flavor = "multi_thread")]
async fn a3_s10_a_write_rechecks_the_ledger() {
    let (store, schema) = fresh_named().await;
    let (_, books) = store_checks::books(store).await;
    let mut db = raw(url().await).await;
    execute(&mut db, &format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    Ledger::new(
        store_in(url().await, &schema).await,
        LedgerId::new("example.com/other").unwrap(),
    )
    .await
    .unwrap();
    let error = books
        .record(sale("after", date!("2026-03-01"), "1.00"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error}");
}

/// Round 3 (F9): one pool serves many ledgers, one schema each. Five
/// tenants share a pool of two connections, write concurrently, stay
/// apart, and audit clean; a tenant's schema opened under another ledger
/// id is a configuration error; a hostile schema name connects nothing.
#[tokio::test]
async fn one_pool_serves_many_ledgers() {
    let pool = SqlStore::<Postgres>::postgres_with(
        url().await,
        SqlOptions {
            schema: next_schema(),
            max_connections: 2,
            ..SqlOptions::default()
        },
    )
    .await
    .unwrap();
    let tenants: Vec<(String, LedgerId)> = (0..5)
        .map(|n| {
            (
                next_schema(),
                LedgerId::new(format!("example.com/tenant-{n}")).unwrap(),
            )
        })
        .collect();
    let mut tasks = Vec::new();
    for (index, (schema, id)) in tenants.iter().cloned().enumerate() {
        let store = pool.with_schema(&schema).unwrap();
        tasks.push(tokio::spawn(async move {
            let books = Ledger::new(store, id).await.unwrap();
            books
                .declare_currency(EUR, 2, date!("2026-01-01"))
                .await
                .unwrap();
            books
                .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
                .await
                .unwrap();
            books
                .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
                .await
                .unwrap();
            books.define_months(2026, 3..=3).await.unwrap();
            for n in 0..=index {
                books
                    .record(sale(&format!("sale-{n}"), date!("2026-03-10"), "1.00"))
                    .await
                    .unwrap();
            }
            books.audit().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    for (index, (schema, id)) in tenants.iter().enumerate() {
        let books = Ledger::new(pool.with_schema(schema).unwrap(), id.clone())
            .await
            .unwrap();
        let cash = books
            .balance("Assets:Cash", EUR, &doubleentry::BalanceQuery::all())
            .await
            .unwrap();
        assert_eq!(cash.postings(), index as u64 + 1, "{schema} stays apart");
    }
    let (schema, _) = &tenants[0];
    let wrong = Ledger::new(pool.with_schema(schema).unwrap(), tenants[1].1.clone()).await;
    assert!(matches!(wrong, Err(Error::Config(_))), "{wrong:?}");
    assert_eq!(
        pool.with_schema("x\"; DROP SCHEMA public; --")
            .unwrap_err()
            .kind(),
        &StoreErrorKind::InvalidSchemaName {
            name: String::from("x\"; DROP SCHEMA public; --")
        }
    );
}

// ── PoC audit regressions (026) ─────────────────────────────────────────────

/// `regress_qs1_batch_order_is_log_order` (PoC: Q-S1): a batch of an entry
/// and its reversal is sequenced in batch order, contiguously, as on the
/// `Journal` — same receipts, same head — and the log replays in its own
/// order.
#[tokio::test(flavor = "multi_thread")]
// PoC: Q-S1 q_s1_deferred_sequencer_reorders_a_batch_by_uuid
async fn regress_qs1_batch_order_is_log_order() {
    let (mut journal, books) = store_checks::books(fresh().await).await;
    let batch = || {
        vec![
            sale("orig", date!("2026-03-15"), "5.00"),
            Entry::new("rev", date!("2026-03-16"))
                .credit("Assets:Cash", eur("5.00"), EUR)
                .debit("Income:Sales", eur("5.00"), EUR)
                .reverses(store_checks::id("orig")),
        ]
    };
    let start = books.head().await.unwrap().size;
    let expected = journal.record_batch(batch()).unwrap();
    let receipts = books.record_batch(batch()).await.unwrap();
    assert_eq!(receipts, expected);
    assert_eq!(
        receipts.iter().map(|r| r.position).collect::<Vec<_>>(),
        [start, start + 1]
    );
    assert_eq!(books.head().await.unwrap(), journal.head());
    books.audit().await.unwrap();
}

/// Kept from audit round 3 (PoC: cancellation): 300 batches dropped after
/// 0–40 ms, many of them mid-transaction, leak no lock and no transaction —
/// the next write completes and the audit passes.
#[tokio::test(flavor = "multi_thread")]
// PoC: a3_cancelled_writes_leak_nothing
async fn cancelled_writes_leak_nothing() {
    let (_, books) = store_checks::books(fresh().await).await;
    let mut dropped = 0;
    for n in 0..300u64 {
        let books = books.clone();
        let budget = Duration::from_micros((n * 997) % 40_000);
        let write = async move {
            books
                .record_batch(
                    (0..5).map(|k| sale(&format!("c{n}-{k}"), date!("2026-03-15"), "1.00")),
                )
                .await
        };
        if tokio::time::timeout(budget, write).await.is_err() {
            dropped += 1;
        }
    }
    assert!(dropped > 0, "no write was cancelled");
    let next = tokio::time::timeout(
        Duration::from_secs(10),
        books.record(sale("after", date!("2026-03-15"), "1.00")),
    )
    .await;
    assert!(matches!(next, Ok(Ok(_))), "{next:?}");
    books.audit().await.unwrap();
}
