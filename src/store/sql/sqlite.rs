//! The SQLite dialect.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use super::dialect::{
    Batch, Class, Kind, Mode, Statement, Tokens, Triggers, Value, chunks, plumbing, sealed::Driver,
};
use super::{SqlOptions, SqlStore};
use crate::store::StoreError;

/// SQLite (feature `sqlite`): one ledger per database file.
#[derive(Debug, Clone, Copy)]
pub struct Sqlite;

impl<const P: u8> SqlStore<Sqlite, P> {
    /// Connects to the SQLite database at `url` (`sqlite://path/to/books.db`,
    /// created if missing) with the default [`SqlOptions`].
    ///
    /// ```
    /// use doubleentry::store::{SqlStore, Sqlite};
    /// use doubleentry::{Ledger, LedgerId};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// # let url = format!("sqlite://{}", dir.path().join("books.db").display());
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
    /// let store = SqlStore::<Sqlite>::sqlite(&url).await?;
    /// let books = Ledger::new(store, LedgerId::new("example.com/acme")?).await?; // create or open
    /// assert_eq!(books.head().await?.size, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// `Backend` for an invalid URL; `Unavailable` when the file cannot be
    /// opened.
    pub async fn sqlite(url: &str) -> Result<Self, StoreError> {
        Self::sqlite_with(url, SqlOptions::default()).await
    }

    /// Connects to the SQLite database at `url` with `options`.
    ///
    /// # Errors
    ///
    /// As [`sqlite`](Self::sqlite).
    pub async fn sqlite_with(url: &str, options: SqlOptions) -> Result<Self, StoreError> {
        Self::connect(url, options).await
    }
}

plumbing!(sqlx::Sqlite);

impl Driver for Sqlite {
    type Pool = sqlx::SqlitePool;
    type Tx = sqlx::Transaction<'static, sqlx::Sqlite>;

    plumbing!(methods);

    const TRIGGERS: Triggers = Triggers {
        append_only: "CREATE TRIGGER {{t}}_no_update BEFORE UPDATE ON {{t}} BEGIN SELECT \
            RAISE(ABORT, 'doubleentry: UPDATE on {{t}} refused (append-only)'); END;\n\
            CREATE TRIGGER {{t}}_no_delete BEFORE DELETE ON {{t}} BEGIN SELECT \
            RAISE(ABORT, 'doubleentry: DELETE on {{t}} refused (append-only)'); END;\n",
        maintained: "CREATE TRIGGER {{t}}_no_delete BEFORE DELETE ON {{t}} BEGIN SELECT \
            RAISE(ABORT, 'doubleentry: DELETE on {{t}} refused (maintained)'); END;\n",
        every_table: "",
        no_replace: include_str!("no_replace_sqlite.sql"),
        scoped: include_str!("triggers_sqlite.sql"),
    };

    const META_PROBE: &str =
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = $1";
    const PRELUDE: &str = "";

    fn rows_source(kinds: &[Kind], rows: usize) -> String {
        let mut next = 0usize;
        let rows: Vec<String> = (0..rows)
            .map(|_| {
                let row: Vec<String> = kinds
                    .iter()
                    .map(|_| {
                        next = next.saturating_add(1);
                        format!("${next}")
                    })
                    .collect();
                format!("({})", row.join(", "))
            })
            .collect();
        format!("VALUES {}", rows.join(", "))
    }

    const TOKENS: Tokens = Tokens {
        int: "INTEGER",
        bytes: "BLOB",
        text: "TEXT",
        len: "length",
        strict: " STRICT",
    };

    fn qualifier(_schema: &str) -> Result<String, StoreError> {
        Ok(String::new())
    }

    async fn connect(url: &str, options: &SqlOptions) -> Result<Self::Pool, StoreError> {
        let connect = SqliteConnectOptions::from_str(url)
            .map_err(|error| StoreError::backend("connect", error))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            // A `REPLACE` (or `INSERT OR REPLACE`) deletes the conflicting
            // row: with recursive triggers on, that delete fires the
            // tables' delete triggers, so it is refused like any other.
            .pragma("recursive_triggers", "ON")
            .busy_timeout(options.busy_timeout);
        // Every connection to an in-memory database sees its own database.
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        let connections = if in_memory {
            1
        } else {
            options.max_connections
        };
        SqlitePoolOptions::new()
            .max_connections(connections)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    let on: i64 = sqlx::query_scalar(
                        "SELECT foreign_keys * recursive_triggers \
                         FROM pragma_foreign_keys, pragma_recursive_triggers",
                    )
                    .fetch_one(&mut *connection)
                    .await?;
                    if on == 1 {
                        Ok(())
                    } else {
                        Err(sqlx::Error::Configuration("a pragma is off".into()))
                    }
                })
            })
            .connect_with(connect)
            .await
            .map_err(|error| super::failure::<Self>("connect", &error, false).with_source(error))
    }

    async fn begin(pool: &Self::Pool, mode: Mode) -> Result<Self::Tx, sqlx::Error> {
        match mode {
            Mode::Write => pool.begin_with("BEGIN IMMEDIATE").await,
            Mode::Read => pool.begin().await,
        }
    }

    async fn lock(_tx: &mut Self::Tx, _key: i64) -> Result<(), sqlx::Error> {
        // `BEGIN IMMEDIATE` holds the file's write lock: one writer per
        // ledger.
        Ok(())
    }

    async fn batch(
        tx: &mut Self::Tx,
        batch: &Batch,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>, sqlx::Error> {
        // In-process: multi-row statements, their text cached per row count,
        // as few as SQLITE_MAX_VARIABLE_NUMBER (32 766) allows.
        let mut found = Vec::new();
        for chunk in chunks(rows, 32_766, batch.kinds.len()) {
            let text = batch.text::<Self>(chunk.len());
            let selected = query(&text, chunk.into_iter().flatten().collect())
                .fetch_all(&mut **tx)
                .await?;
            for row in &selected {
                found.push(decode(row, batch.columns)?);
            }
        }
        Ok(found)
    }

    fn classify(code: &str, message: &str) -> Class {
        // Extended result codes: the primary code is the low byte.
        match code.parse::<i32>().unwrap_or_default() & 0xff {
            5 | 6 => Class::Busy,             // BUSY, LOCKED: another writer holds the file
            19 | 11 | 26 => Class::Integrity, // CONSTRAINT (triggers too), CORRUPT, NOTADB
            10 | 13 => Class::Broken,         // IOERR, FULL
            1 if message.starts_with("no such table") => Class::MissingTable,
            _ => Class::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::dialect::fake::database;
    use super::*;

    fn class(error: &sqlx::Error) -> Class {
        Class::of(error, Sqlite::classify)
    }

    #[test]
    fn errors_are_classified_by_result_code() {
        for code in ["5", "517", "6", "262"] {
            assert_eq!(class(&database(code, "busy")), Class::Busy, "{code}");
        }
        for code in ["19", "1811", "2067", "787", "275", "1555", "11", "26"] {
            assert_eq!(
                class(&database(code, "refused")),
                Class::Integrity,
                "{code}"
            );
        }
        for code in ["10", "13"] {
            assert_eq!(class(&database(code, "io")), Class::Broken, "{code}");
        }
        assert_eq!(
            class(&database("1", "no such table: meta")),
            Class::MissingTable
        );
        assert_eq!(class(&database("1", "syntax error")), Class::Failed);
        assert_eq!(class(&sqlx::Error::PoolTimedOut), Class::Busy);
        assert_eq!(
            class(&sqlx::Error::Io(std::io::Error::other("gone"))),
            Class::Lost
        );
    }

    #[test]
    fn batches_bind_a_values_list() {
        assert_eq!(
            Sqlite::rows_source(&[Kind::Int, Kind::Bytes], 2),
            "VALUES ($1, $2), ($3, $4)"
        );
    }
}
