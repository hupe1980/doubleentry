//! The PostgreSQL dialect.

use std::str::FromStr;

use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use super::dialect::{
    Batch, Class, Kind, Mode, Statement, Tokens, Triggers, Value, Write, plumbing, sealed::Driver,
};
use super::{SqlOptions, SqlStore};
use crate::store::StoreError;

/// PostgreSQL (feature `postgres`): one ledger per schema.
///
/// `create` needs a role that may create the schema and its tables; the
/// application can then run with a role that only reads and appends. These
/// grants (schema `ledger`, application role `app`) are exactly what a store
/// needs after `create`:
///
/// ```sql
/// GRANT USAGE ON SCHEMA ledger TO app;
/// GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA ledger TO app;
/// GRANT UPDATE ON ledger.balances, ledger.cleared, ledger.outstanding, ledger.slices TO app;
/// GRANT UPDATE (resolved_by, how) ON ledger.holds TO app;
/// GRANT UPDATE (salt, text) ON ledger.memos TO app;
/// GRANT UPDATE (fingerprint) ON ledger.entries TO app;
/// ```
///
/// No `DELETE`, `TRUNCATE` or DDL: triggers refuse every change to the log
/// beyond these (their messages start `doubleentry:`; a fingerprint is
/// nulled only together with its entry's memo erasure), and the role cannot
/// drop them. A row the role inserts beyond what the write path writes is
/// not refused by a trigger, but `audit` finds it: every table is compared
/// with the replay of the log.
#[derive(Debug, Clone, Copy)]
pub struct Postgres;

impl<const P: u8> SqlStore<Postgres, P> {
    /// Connects to the PostgreSQL database at `url` with the default
    /// [`SqlOptions`] (schema `doubleentry`). The URL carries every connection
    /// setting: see [`postgres_with`](Self::postgres_with), "Connection settings".
    ///
    /// # Errors
    ///
    /// `Backend` for an invalid URL, or for a URL that requires TLS on a
    /// build without `postgres-tls` (the message names the feature);
    /// `Unavailable` when the server cannot be reached.
    pub async fn postgres(url: &str) -> Result<Self, StoreError> {
        Self::postgres_with(url, SqlOptions::default()).await
    }

    /// Connects to the PostgreSQL database at `url` with `options`; the
    /// schema name is quoted as an identifier everywhere and never put in
    /// the connection's settings.
    ///
    /// # Connection settings
    ///
    /// The URL — and the standard `PG…` environment variables such as
    /// `PGAPPNAME` and `PGSSLMODE` — carries everything the host owns:
    /// identity (`application_name`), TLS (`sslmode`, `sslrootcert`,
    /// `sslcert`, `sslkey`) and server `options`.
    ///
    /// ```no_run
    /// use doubleentry::store::{Postgres, SqlOptions, SqlStore};
    ///
    /// # async fn run() -> Result<(), doubleentry::store::StoreError> {
    /// let url = "postgres://app@db/books?sslmode=verify-full\
    ///            &sslrootcert=/etc/ssl/db-ca.pem&application_name=accountingd";
    /// let options = SqlOptions { max_connections: 4, ..SqlOptions::default() };
    /// let store = SqlStore::<Postgres>::postgres_with(url, options).await?;
    /// # drop(store); Ok(()) }
    /// ```
    ///
    /// - Only `sslmode=require`, `verify-ca` and `verify-full` guarantee
    ///   encryption, and they need the `postgres-tls` feature; prefer
    ///   `verify-full`. `prefer`, the default, falls back to plain text.
    /// - [`SqlOptions::max_connections`] is the store's whole connection
    ///   budget, shared by every store made with
    ///   [`with_schema`](SqlStore::with_schema).
    /// - The store owns its pool: it takes no external pool or connection and
    ///   joins no external transaction. To post together with a write to
    ///   your own database, use an
    ///   [outbox](https://hupe1980.github.io/doubleentry/docs/concepts/idempotency/#together-with-your-own-database-the-outbox).
    ///
    /// # Errors
    ///
    /// As [`postgres`](Self::postgres);
    /// [`InvalidSchemaName`](crate::store::StoreErrorKind::InvalidSchemaName)
    /// for a schema name other than `[A-Za-z_][A-Za-z0-9_]{0,62}`.
    pub async fn postgres_with(url: &str, options: SqlOptions) -> Result<Self, StoreError> {
        Self::connect(url, options).await
    }
}

plumbing!(sqlx::Postgres);

/// One column of values, bound as one array.
fn bind_column(query: Query, kind: Kind, values: Vec<Value>) -> Query {
    let values = values.into_iter();
    match kind {
        Kind::Int => query.bind(values.map(Value::into_int).collect::<Vec<_>>()),
        Kind::Bytes => query.bind(values.map(Value::into_bytes).collect::<Vec<_>>()),
        Kind::Text => query.bind(values.map(Value::into_text).collect::<Vec<_>>()),
    }
}

/// Rows of columns of `kinds`, bound as one array per column.
fn bind_rows(query: Query, kinds: &[Kind], rows: Vec<Vec<Value>>) -> Query {
    let mut columns: Vec<Vec<Value>> = kinds
        .iter()
        .map(|_| Vec::with_capacity(rows.len()))
        .collect();
    for row in rows {
        for (column, value) in columns.iter_mut().zip(row) {
            column.push(value);
        }
    }
    kinds
        .iter()
        .zip(columns)
        .fold(query, |query, (kind, values)| {
            bind_column(query, *kind, values)
        })
}

/// The array type of a column kind.
const fn array_type(kind: Kind) -> &'static str {
    match kind {
        Kind::Int => "bigint[]",
        Kind::Bytes => "bytea[]",
        Kind::Text => "text[]",
    }
}

impl Driver for Postgres {
    type Pool = sqlx::PgPool;
    type Tx = sqlx::Transaction<'static, sqlx::Postgres>;

    plumbing!(methods);

    const TRIGGERS: Triggers = Triggers {
        append_only: "CREATE TRIGGER {{t}}_append_only BEFORE UPDATE OR DELETE ON {{s}}{{t}} \
            FOR EACH ROW EXECUTE FUNCTION {{s}}refuse_change();\n",
        maintained: "CREATE TRIGGER {{t}}_no_delete BEFORE DELETE ON {{s}}{{t}} \
            FOR EACH ROW EXECUTE FUNCTION {{s}}refuse_change();\n",
        every_table: "CREATE TRIGGER {{t}}_no_truncate BEFORE TRUNCATE ON {{s}}{{t}} \
            FOR EACH STATEMENT EXECUTE FUNCTION {{s}}refuse_change();\n",
        no_replace: "",
        scoped: include_str!("triggers_postgres.sql"),
    };

    const META_PROBE: &str =
        "SELECT CAST(CASE WHEN to_regclass($1) IS NULL THEN 0 ELSE 1 END AS BIGINT)";
    const PRELUDE: &str = "CREATE SCHEMA IF NOT EXISTS {{q}};\n";

    fn rows_source(kinds: &[Kind], _rows: usize) -> String {
        let arrays: Vec<String> = (1..)
            .zip(kinds)
            .map(|(n, kind)| format!("${n}::{}", array_type(*kind)))
            .collect();
        format!("SELECT * FROM unnest({})", arrays.join(", "))
    }

    const TOKENS: Tokens = Tokens {
        int: "BIGINT",
        bytes: "BYTEA",
        text: "TEXT COLLATE \"C\"",
        len: "octet_length",
        strict: "",
    };

    fn qualifier(schema: &str) -> Result<String, StoreError> {
        // `[A-Za-z_][A-Za-z0-9_]{0,62}`: no character a statement template
        // gives a meaning (`$n`, `{{…}}`) and nothing that needs escaping —
        // and still quoted, so its case is kept.
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let leading_digit = schema.starts_with(|c: char| c.is_ascii_digit());
        if schema.is_empty() || leading_digit || !schema.chars().all(word) || schema.len() > 63 {
            return Err(StoreError::invalid_schema_name(schema));
        }
        Ok(format!("\"{schema}\"."))
    }

    async fn connect(url: &str, options: &SqlOptions) -> Result<Self::Pool, StoreError> {
        let connect = PgConnectOptions::from_str(url)
            .map_err(|error| StoreError::backend("connect", error))?
            // Every statement of this store is a parameterised index lookup
            // or append whose plan does not depend on its values; planning
            // each execution again (PostgreSQL's custom plans) costs more
            // than a round trip on the batched fact lookups. Set on this
            // pool's connections only.
            .options([("plan_cache_mode", "force_generic_plan")]);
        // No ping before each use: a connection that died fails its `BEGIN`
        // as a transient error, before anything could apply.
        PgPoolOptions::new()
            .max_connections(options.max_connections)
            .test_before_acquire(false)
            .connect_with(connect)
            .await
            .map_err(|error| super::failure::<Self>("connect", &error, false).with_source(error))
    }

    async fn begin(pool: &Self::Pool, mode: Mode) -> Result<Self::Tx, sqlx::Error> {
        match mode {
            Mode::Write => {
                pool.begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
                    .await
            }
            Mode::Read => {
                pool.begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
                    .await
            }
        }
    }

    async fn lock(tx: &mut Self::Tx, key: i64) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn batch(
        tx: &mut Self::Tx,
        batch: &Batch,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>, sqlx::Error> {
        // One statement whatever the row count, its text never changing (one
        // prepared statement per connection): one array per column.
        let query = bind_rows(
            sqlx::query(AssertSqlSafe(batch.text::<Self>(0))),
            &batch.kinds,
            rows,
        );
        let found = query.fetch_all(&mut **tx).await?;
        found.iter().map(|row| decode(row, batch.columns)).collect()
    }

    async fn apply(tx: &mut Self::Tx, writes: Vec<Write<'_>>) -> Result<(), sqlx::Error> {
        // One statement, one round trip, for the whole write set.
        let Some((text, writes)) = Write::combine::<Self>(writes) else {
            return Ok(());
        };
        let query =
            writes.into_iter().fold(
                sqlx::query(AssertSqlSafe(text)),
                |query, write| match write {
                    Write::Row(_, args) => args.into_iter().fold(query, bind),
                    Write::Rows(batch, rows) => bind_rows(query, &batch.kinds, rows),
                },
            );
        query.execute(&mut **tx).await.map(drop)
    }

    fn classify(code: &str, message: &str) -> Class {
        match code {
            "40001" | "40P01" => Class::Busy, // serialization failure, deadlock: rolled back
            "55P03" | "57014" => Class::Lost, // lock not available, query cancelled
            _ if ["57P", "53", "08"]
                .iter()
                .any(|class| code.starts_with(class)) =>
            {
                Class::Lost
            }
            _ if code.starts_with("23") => Class::Integrity,
            "P0001" if message.starts_with("doubleentry:") => Class::Integrity,
            "42P01" | "3F000" => Class::MissingTable,
            _ => Class::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::dialect::fake::database;
    use super::*;

    fn class(error: &sqlx::Error) -> Class {
        Class::of(error, Postgres::classify)
    }

    #[test]
    fn errors_are_classified_by_sqlstate() {
        for code in ["40001", "40P01"] {
            assert_eq!(class(&database(code, "retry")), Class::Busy, "{code}");
        }
        for code in [
            "55P03", "57014", "57P01", "57P02", "57P03", "53300", "08006", "08000",
        ] {
            assert_eq!(class(&database(code, "unavailable")), Class::Lost, "{code}");
        }
        for code in ["23505", "23503", "23514", "23502"] {
            assert_eq!(
                class(&database(code, "constraint")),
                Class::Integrity,
                "{code}"
            );
        }
        let trigger = database(
            "P0001",
            "doubleentry: UPDATE on s.leaves refused (append-only)",
        );
        assert_eq!(class(&trigger), Class::Integrity);
        assert_eq!(class(&database("P0001", "raised elsewhere")), Class::Failed);
        for code in ["42P01", "3F000"] {
            assert_eq!(
                class(&database(code, "missing")),
                Class::MissingTable,
                "{code}"
            );
        }
        assert_eq!(class(&sqlx::Error::PoolTimedOut), Class::Busy);
        assert_eq!(class(&sqlx::Error::PoolClosed), Class::Broken);
    }

    // PoC: a3_s4_dollar_in_schema_name_redirects_writes — a schema name is
    // an identifier, refused with a typed error otherwise, and always quoted.
    #[test]
    fn schema_names_are_validated_quoted_identifiers() {
        assert_eq!(Postgres::qualifier("Ledger_1").unwrap(), "\"Ledger_1\".");
        assert_eq!(Postgres::qualifier("_x").unwrap(), "\"_x\".");
        assert!(Postgres::qualifier(&"x".repeat(63)).is_ok());
        for refused in [
            "",
            "1ledger",
            "acme$1",
            "victim$4",
            "a{{s}}b",
            "{{rows}}",
            "Led \"x\"",
            "led -c statement_timeout=1",
            "a\0b",
            "café",
            &"x".repeat(64),
        ] {
            let error = Postgres::qualifier(refused).unwrap_err();
            assert_eq!(
                error.kind(),
                &crate::store::StoreErrorKind::InvalidSchemaName {
                    name: refused.to_owned()
                },
                "{refused:?}"
            );
        }
    }

    /// Connects to a listener that accepts and hangs up, with `sslmode=require`:
    /// the driver decides about TLS before it writes a byte.
    fn connect_requiring_tls() -> Result<SqlStore<Postgres>, StoreError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| StoreError::backend("test", e))?;
        let port = listener
            .local_addr()
            .map_err(|e| StoreError::backend("test", e))?
            .port();
        let server = std::thread::spawn(move || drop(listener.accept()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| StoreError::backend("test", e))?;
        let url = format!("postgres://u@127.0.0.1:{port}/d?sslmode=require");
        let outcome = runtime.block_on(SqlStore::<Postgres>::postgres(&url));
        drop(server.join());
        outcome
    }

    #[cfg(not(feature = "postgres-tls"))]
    #[test]
    fn tls_without_the_feature_names_it() {
        use std::error::Error as _;
        let Err(error) = connect_requiring_tls() else {
            panic!("connected without TLS support")
        };
        assert_eq!(error.kind(), &crate::store::StoreErrorKind::Backend);
        assert!(!error.is_transient() && !error.may_have_applied());
        assert!(error.to_string().contains("`postgres-tls`"), "{error}");
        assert!(error.source().is_some());
    }

    #[cfg(feature = "postgres-tls")]
    #[test]
    fn tls_with_the_feature_does_not_hint() {
        let Err(error) = connect_requiring_tls() else {
            panic!("connected to a server that hung up")
        };
        assert!(!error.to_string().contains("postgres-tls"), "{error}");
    }

    #[test]
    fn batches_bind_one_array_per_column() {
        let source = Postgres::rows_source(&[Kind::Int, Kind::Bytes, Kind::Text], 7);
        assert_eq!(
            source,
            "SELECT * FROM unnest($1::bigint[], $2::bytea[], $3::text[])"
        );
    }
}
