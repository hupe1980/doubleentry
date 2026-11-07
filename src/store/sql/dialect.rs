//! The sealed [`Dialect`] trait: everything SQLite and PostgreSQL do
//! differently — connecting, beginning transactions and taking the lock,
//! DDL type tokens and triggers, bulk inserts and list lookups, binding and
//! error classification. Everything else is shared.

use std::sync::Arc;

use super::SqlOptions;
use crate::store::StoreError;

/// An SQL dialect of [`SqlStore`](super::SqlStore): [`Sqlite`](super::Sqlite)
/// or [`Postgres`](super::Postgres). Sealed: the SQL store supports exactly
/// these.
pub trait Dialect: sealed::Driver {}

impl<D: sealed::Driver> Dialect for D {}

/// The type of a bound parameter or a returned column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A 64-bit integer.
    Int,
    /// A byte string.
    Bytes,
    /// UTF-8 text.
    Text,
}

#[cfg(feature = "postgres")]
impl Value {
    /// The integer, if this is one.
    pub(crate) fn into_int(self) -> Option<i64> {
        if let Self::Int(value) = self {
            Some(value)
        } else {
            None
        }
    }

    /// The bytes, if these are some.
    pub(crate) fn into_bytes(self) -> Option<Vec<u8>> {
        if let Self::Bytes(value) = self {
            Some(value)
        } else {
            None
        }
    }

    /// The text, if this is some.
    pub(crate) fn into_text(self) -> Option<String> {
        if let Self::Text(value) = self {
            Some(value)
        } else {
            None
        }
    }
}

/// A bound parameter or a returned column value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A 64-bit integer.
    Int(i64),
    /// A byte string.
    Bytes(Vec<u8>),
    /// UTF-8 text.
    Text(String),
    /// SQL `NULL` of a type.
    Null(Kind),
}

/// A statement rendered for one store: its text and the types of the
/// columns it returns.
#[derive(Debug, Clone)]
pub struct Statement {
    /// The SQL text (shared: statements are prepared once per connection).
    pub(crate) text: Arc<str>,
    /// The column types, in select-list order.
    pub(crate) columns: &'static [Kind],
}

/// A statement over a list of rows bound as parameters: a bulk insert
/// (`INSERT … {{rows}}`) or a batched lookup (`WITH k (i, …) AS ({{rows}})
/// SELECT k.i, …`, one round trip for every key of a plan round).
/// PostgreSQL binds one array per column whatever the row count, so its text
/// never changes; SQLite binds a `VALUES` list, chunked below its parameter
/// limit.
#[derive(Debug, Clone)]
pub struct Batch {
    /// The SQL text, with `{{rows}}` where the dialect's row source goes.
    pub(crate) template: Arc<str>,
    /// The types of each bound row's columns.
    pub(crate) kinds: Vec<Kind>,
    /// The types of the returned columns (none for an insert).
    pub(crate) columns: &'static [Kind],
}

impl Batch {
    /// The text of one execution over `rows` rows in dialect `D`.
    pub(crate) fn text<D: sealed::Driver>(&self, rows: usize) -> Arc<str> {
        Arc::from(
            self.template
                .replace("{{rows}}", &D::rows_source(&self.kinds, rows)),
        )
    }
}

/// One change of a write set: a single-row statement with its arguments,
/// or a bulk insert with its rows.
#[derive(Debug)]
pub enum Write<'s> {
    /// A statement and its arguments.
    Row(&'s Statement, Vec<Value>),
    /// A bulk insert and its rows (none: nothing to write).
    Rows(&'s Batch, Vec<Vec<Value>>),
}

#[cfg(feature = "postgres")]
impl<'s> Write<'s> {
    /// One statement applying `writes` in dialect `D` — each change a
    /// data-modifying `WITH` query, its parameters renumbered after the ones
    /// before — and the changes to bind, in order; `None` when nothing is
    /// to be written. Every sub-statement sees the snapshot from before the
    /// statement and none reads another's rows; constraints are checked when
    /// the statement ends, so a row may reference one inserted beside it.
    pub(crate) fn combine<D: sealed::Driver>(writes: Vec<Self>) -> Option<(String, Vec<Self>)> {
        let mut text = String::from("WITH ");
        let mut offset = 0usize;
        let mut kept = Vec::with_capacity(writes.len());
        for write in writes {
            let (sql, count) = match &write {
                Self::Rows(_, rows) if rows.is_empty() => continue,
                Self::Row(statement, args) => (Arc::clone(&statement.text), args.len()),
                Self::Rows(batch, _) => (batch.text::<D>(0), batch.kinds.len()),
            };
            if !kept.is_empty() {
                text.push_str(", ");
            }
            let name = kept.len();
            text.push_str(&format!("w{name} AS ({})", renumber(&sql, offset)));
            offset = offset.saturating_add(count);
            kept.push(write);
        }
        text.push_str(" SELECT 1");
        (!kept.is_empty()).then_some((text, kept))
    }
}

#[cfg(feature = "postgres")]
/// `sql` with each parameter `$n` renumbered `$(n + offset)`; the store's
/// statements use `$` for nothing else.
fn renumber(sql: &str, offset: usize) -> String {
    let mut out = String::with_capacity(sql.len().saturating_add(8));
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c != '$' {
            continue;
        }
        let mut number = 0usize;
        let mut digits = false;
        while let Some(digit) = chars.peek().and_then(|c| c.to_digit(10)) {
            chars.next();
            digits = true;
            number = number.saturating_mul(10).saturating_add(digit as usize);
        }
        if digits {
            out.push_str(&number.saturating_add(offset).to_string());
        }
    }
    out
}

#[cfg(feature = "sqlite")]
/// `rows` of `width` columns in chunks binding at most `parameters` values
/// each (at least one row per chunk).
pub(crate) fn chunks(
    rows: Vec<Vec<Value>>,
    parameters: usize,
    width: usize,
) -> Vec<Vec<Vec<Value>>> {
    let per_chunk = parameters.checked_div(width).unwrap_or(1).max(1);
    let mut rows = rows.into_iter().peekable();
    let mut chunks = Vec::new();
    while rows.peek().is_some() {
        chunks.push(rows.by_ref().take(per_chunk).collect());
    }
    chunks
}

/// How a transaction begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A write: SQLite `BEGIN IMMEDIATE`, PostgreSQL `READ COMMITTED`.
    Write,
    /// A read: one snapshot, no lock (SQLite deferred, PostgreSQL
    /// `REPEATABLE READ READ ONLY`).
    Read,
}

/// What a driver error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Transient and definitely not applied: busy, locked, a pool or lock
    /// timeout, a serialisation failure or deadlock (rolled back).
    Busy,
    /// Transient; raised while committing it may have applied: a lost
    /// connection, a cancelled statement, a server going away.
    Lost,
    /// A constraint, a trigger refusal, a corrupt file, an undecodable
    /// column: stored data the store cannot trust.
    Integrity,
    /// The table asked for does not exist (no ledger was created).
    MissingTable,
    /// Permanent and definitely not applied.
    Failed,
    /// Permanent; raised while committing it may have applied.
    Broken,
}

impl Class {
    /// What `error` means, its database errors classified by `database`
    /// (code, message).
    pub(crate) fn of(error: &sqlx::Error, database: fn(&str, &str) -> Self) -> Self {
        match error {
            sqlx::Error::Database(error) => {
                database(error.code().as_deref().unwrap_or_default(), error.message())
            }
            sqlx::Error::PoolTimedOut => Self::Busy,
            sqlx::Error::Io(_) => Self::Lost,
            sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => Self::Integrity,
            _ => Self::Broken,
        }
    }
}

/// The sqlx plumbing both dialects share, written once: binding values,
/// decoding rows, and the driver methods that only run statements. Invoked
/// in a dialect's module (`plumbing!(Db)`) and in its `Driver` impl
/// (`plumbing!(methods)`).
macro_rules! plumbing {
    (methods) => {
        async fn close(pool: &Self::Pool) {
            pool.close().await;
        }

        async fn commit(tx: Self::Tx) -> Result<(), sqlx::Error> {
            tx.commit().await
        }

        async fn rollback(tx: Self::Tx) -> Result<(), sqlx::Error> {
            tx.rollback().await
        }

        async fn fetch(
            tx: &mut Self::Tx,
            statement: &Statement,
            args: Vec<Value>,
        ) -> Result<Vec<Vec<Value>>, sqlx::Error> {
            let rows = query(&statement.text, args).fetch_all(&mut **tx).await?;
            rows.iter()
                .map(|row| decode(row, statement.columns))
                .collect()
        }

        async fn execute(
            tx: &mut Self::Tx,
            statement: &Statement,
            args: Vec<Value>,
        ) -> Result<u64, sqlx::Error> {
            Ok(query(&statement.text, args)
                .execute(&mut **tx)
                .await?
                .rows_affected())
        }

        async fn script(tx: &mut Self::Tx, script: String) -> Result<(), sqlx::Error> {
            sqlx::raw_sql(sqlx::AssertSqlSafe(script))
                .execute(&mut **tx)
                .await
                .map(drop)
        }
    };
    ($db:ty) => {
        /// A query of this dialect.
        type Query = sqlx::query::Query<'static, $db, <$db as sqlx::Database>::Arguments>;

        /// Binds one value.
        fn bind(query: Query, value: Value) -> Query {
            match value {
                Value::Int(value) => query.bind(value),
                Value::Bytes(value) => query.bind(value),
                Value::Text(value) => query.bind(value),
                Value::Null(Kind::Int) => query.bind(None::<i64>),
                Value::Null(Kind::Bytes) => query.bind(None::<Vec<u8>>),
                Value::Null(Kind::Text) => query.bind(None::<String>),
            }
        }

        /// A statement with its arguments bound.
        fn query(text: &std::sync::Arc<str>, args: Vec<Value>) -> Query {
            args.into_iter().fold(
                sqlx::query(sqlx::AssertSqlSafe(std::sync::Arc::clone(text))),
                bind,
            )
        }

        /// A row's columns as values of their declared kinds.
        fn decode(
            row: &<$db as sqlx::Database>::Row,
            columns: &[Kind],
        ) -> Result<Vec<Value>, sqlx::Error> {
            use sqlx::Row as _;
            columns
                .iter()
                .enumerate()
                .map(|(index, kind)| {
                    Ok(match kind {
                        Kind::Int => row.try_get::<Option<i64>, _>(index)?.map(Value::Int),
                        Kind::Bytes => row.try_get::<Option<Vec<u8>>, _>(index)?.map(Value::Bytes),
                        Kind::Text => row.try_get::<Option<String>, _>(index)?.map(Value::Text),
                    }
                    .unwrap_or(Value::Null(*kind)))
                })
                .collect()
        }
    };
}
pub(crate) use plumbing;

/// The DDL type tokens of the schema template.
#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    /// 64-bit integers.
    pub(crate) int: &'static str,
    /// Byte strings.
    pub(crate) bytes: &'static str,
    /// Text compared or ordered by bytes.
    pub(crate) text: &'static str,
    /// The byte-length function.
    pub(crate) len: &'static str,
    /// The table suffix.
    pub(crate) strict: &'static str,
}

/// The triggers of a dialect: templates per table (`{{t}}` the table,
/// `{{s}}` the qualifier) and the column-scoped ones.
#[derive(Debug, Clone, Copy)]
pub struct Triggers {
    /// For each append-only table: refuse `UPDATE` and `DELETE`.
    pub(crate) append_only: &'static str,
    /// For each maintained-value table: refuse `DELETE`.
    pub(crate) maintained: &'static str,
    /// For every table (PostgreSQL: refuse `TRUNCATE`).
    pub(crate) every_table: &'static str,
    /// For each append-only or column-scoped table (`{{cond}}`: a row of
    /// `o` with one of `NEW`'s keys): refuse an insert whose key exists —
    /// SQLite's `REPLACE` would delete the row it conflicts with (empty for
    /// PostgreSQL, which has no `REPLACE`).
    pub(crate) no_replace: &'static str,
    /// The functions they call and the triggers of memos, holds and entries.
    pub(crate) scoped: &'static str,
}

pub(crate) mod sealed {
    use core::future::Future;

    use super::{
        Batch, Class, Kind, Mode, SqlOptions, Statement, StoreError, Tokens, Triggers, Value, Write,
    };

    /// The dialect's driver; not nameable outside the crate.
    pub trait Driver: Send + Sync + Sized + 'static {
        /// The connection pool.
        type Pool: Clone + Send + Sync + 'static;
        /// An open transaction (rolled back when dropped).
        type Tx: Send;

        /// The DDL type tokens.
        const TOKENS: Tokens;

        /// The triggers that refuse changes to the log.
        const TRIGGERS: Triggers;

        /// The schema qualifier every statement uses (`"schema".` or empty).
        ///
        /// # Errors
        ///
        /// `InvalidSchemaName` for a schema name the dialect cannot address.
        fn qualifier(schema: &str) -> Result<String, StoreError>;

        /// Connects a pool; no DDL, no read.
        fn connect(
            url: &str,
            options: &SqlOptions,
        ) -> impl Future<Output = Result<Self::Pool, StoreError>> + Send;

        /// Closes every connection of the pool and waits until they are.
        fn close(pool: &Self::Pool) -> impl Future<Output = ()> + Send;

        /// Begins a transaction.
        fn begin(
            pool: &Self::Pool,
            mode: Mode,
        ) -> impl Future<Output = Result<Self::Tx, sqlx::Error>> + Send;

        /// Takes the transaction-scoped lock `key` (a no-op where beginning a
        /// write already locks the ledger).
        fn lock(
            tx: &mut Self::Tx,
            key: i64,
        ) -> impl Future<Output = Result<(), sqlx::Error>> + Send;

        /// Commits.
        fn commit(tx: Self::Tx) -> impl Future<Output = Result<(), sqlx::Error>> + Send;

        /// Rolls back (releasing a transaction-scoped lock at once).
        fn rollback(tx: Self::Tx) -> impl Future<Output = Result<(), sqlx::Error>> + Send;

        /// Runs a statement and returns its rows.
        fn fetch(
            tx: &mut Self::Tx,
            statement: &Statement,
            args: Vec<Value>,
        ) -> impl Future<Output = Result<Vec<Vec<Value>>, sqlx::Error>> + Send;

        /// Runs a statement and returns the number of rows it changed.
        fn execute(
            tx: &mut Self::Tx,
            statement: &Statement,
            args: Vec<Value>,
        ) -> impl Future<Output = Result<u64, sqlx::Error>> + Send;

        /// Runs a batch over `rows` — one execution where the dialect binds
        /// any number of rows — and returns the rows it selects.
        fn batch(
            tx: &mut Self::Tx,
            batch: &Batch,
            rows: Vec<Vec<Value>>,
        ) -> impl Future<Output = Result<Vec<Vec<Value>>, sqlx::Error>> + Send;

        /// Runs a DDL script.
        fn script(
            tx: &mut Self::Tx,
            script: String,
        ) -> impl Future<Output = Result<(), sqlx::Error>> + Send;

        /// The row source of a batch of `rows` rows of columns of `kinds`:
        /// a `VALUES` list (SQLite) or one array per column (PostgreSQL).
        fn rows_source(kinds: &[Kind], rows: usize) -> String;

        /// Applies the changes of one write set, in order: one statement
        /// after another unless the dialect can do better.
        fn apply(
            tx: &mut Self::Tx,
            writes: Vec<Write<'_>>,
        ) -> impl Future<Output = Result<(), sqlx::Error>> + Send {
            async move {
                for write in writes {
                    match write {
                        Write::Row(statement, args) => {
                            Self::execute(tx, statement, args).await?;
                        }
                        Write::Rows(_, rows) if rows.is_empty() => {}
                        Write::Rows(batch, rows) => {
                            Self::batch(tx, batch, rows).await?;
                        }
                    }
                }
                Ok(())
            }
        }

        /// Whether the `meta` table exists, given `$1` = its qualified name.
        const META_PROBE: &str;

        /// The base tables where the ledger lives, one text column; `$1`, if
        /// the dialect has schemas, is the schema name. Without `$1` the
        /// ledger is the whole database, which errors call `main`.
        const TABLES_PROBE: &str;

        /// The DDL run before the tables; `{{q}}` is the quoted schema.
        const PRELUDE: &str;

        /// What a database error (code, message) means.
        fn classify(code: &str, message: &str) -> Class;
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "postgres")]
    #[test]
    fn parameters_are_renumbered_after_the_ones_before() {
        assert_eq!(
            super::renumber("INSERT INTO t (a, b) VALUES ($1, $12) -- $", 3),
            "INSERT INTO t (a, b) VALUES ($4, $15) -- $"
        );
        assert_eq!(super::renumber("SELECT 1", 7), "SELECT 1");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn chunks_stay_below_the_parameter_limit() {
        use super::{Value, chunks};
        let rows = |n: usize| (0..n).map(|_| vec![Value::Int(1); 3]).collect::<Vec<_>>();
        let sizes = |chunks: Vec<Vec<Vec<Value>>>| chunks.iter().map(Vec::len).collect::<Vec<_>>();
        assert_eq!(sizes(chunks(rows(7), 9, 3)), [3, 3, 1]);
        assert_eq!(sizes(chunks(rows(2), 2, 3)), [1, 1]);
        assert!(chunks(rows(0), 9, 3).is_empty());
    }
}

/// A driver error with a given code and message, for classification tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::borrow::Cow;

    use sqlx::error::{DatabaseError, ErrorKind};

    #[derive(Debug)]
    struct Fake {
        code: &'static str,
        message: &'static str,
    }

    impl std::fmt::Display for Fake {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Fake {}

    impl DatabaseError for Fake {
        fn message(&self) -> &str {
            self.message
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    /// A database error with `code` and `message`.
    pub(crate) fn database(code: &'static str, message: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(Fake { code, message }))
    }
}
