//! [`SqlStore`]: one SQL implementation of the [`Store`] contract for SQLite
//! and PostgreSQL.
//!
//! Every statement is written once with `$n` placeholders
//! ([`statements`]); a [`Dialect`] contributes only connecting, beginning
//! transactions and taking the lock, the DDL type tokens and triggers, bulk
//! inserts and list lookups, binding and error classification. Amounts are
//! stored as 16-byte big-endian integers and SQL never computes with them;
//! maintained figures are copied from the core's `balance_after` or summed in
//! Rust with checked arithmetic.

mod codec;
mod dialect;
mod facts;
#[cfg(feature = "postgres")]
mod postgres;
mod reads;
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;
mod statements;
mod write;

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub use dialect::Dialect;
#[cfg(feature = "postgres")]
pub use postgres::Postgres;
#[cfg(feature = "sqlite")]
pub use sqlite::Sqlite;

use self::codec::Row;
use self::dialect::{Batch, Class, Mode, Statement, Value, Write};
use self::statements::Catalogue;
use super::read::{
    AccountRow, BalanceRead, EntryRef, Erasure, HoldCursor, HoldRead, HoldRow, Leaf, LeafRead,
    LedgerInfo, Memo, Page, PageRead, Position, PostingCursor, PostingLine, PostingRead, SealRow,
    StoredEntry,
};
use super::{ProjectionPage, ProjectionRead, Store, StoreError};
use crate::core::{BalanceKey, Done, Op};
use crate::error::Rejection;
use crate::log::Hash;
use crate::model::{
    AccountHandle, AccountRef, Balance, Currency, EntryId, Genesis, Layer, LedgerId, PeriodId,
};

/// How a [`SqlStore`] connects.
///
/// ```
/// use doubleentry::store::SqlOptions;
///
/// let options = SqlOptions { schema: "ledger_acme".into(), ..SqlOptions::default() };
/// assert_eq!(options.max_connections, 10);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlOptions {
    /// The PostgreSQL schema the ledger lives in (one ledger per schema): an
    /// identifier `[A-Za-z_][A-Za-z0-9_]{0,62}`, refused otherwise with
    /// [`StoreErrorKind::InvalidSchemaName`](crate::store::StoreErrorKind::InvalidSchemaName)
    /// before anything connects; always quoted, so its case is kept; never
    /// put in the `search_path`. Ignored by SQLite: one ledger per file.
    pub schema: String,
    /// The size of the connection pool: the store's whole connection budget.
    /// Every store made with `with_schema` shares this pool.
    pub max_connections: u32,
    /// How long a SQLite writer waits for the file lock before the write
    /// fails as unavailable (transient). Ignored by PostgreSQL.
    pub busy_timeout: Duration,
}

impl Default for SqlOptions {
    fn default() -> Self {
        Self {
            schema: String::from("doubleentry"),
            max_connections: 10,
            busy_timeout: Duration::from_secs(5),
        }
    }
}

/// A ledger kept in a SQL database: SQLite (feature `sqlite`) or PostgreSQL
/// (feature `postgres`).
///
/// A cheap, cloneable handle around one connection pool; clones share it.
/// Built from a URL with [`SqlStore::sqlite`] / [`SqlStore::postgres`] (or
/// their `_with` forms taking [`SqlOptions`]), which only connect:
/// [`create`](Store::create) writes the schema and the genesis record,
/// [`open`](Store::open) checks them. No driver type appears in its API.
///
/// - **SQLite**: one ledger per file; every connection runs with
///   `journal_mode=WAL`, `synchronous=FULL` and `foreign_keys=ON`; writes
///   begin `IMMEDIATE`, so writers queue on the file lock for up to the busy
///   timeout. `sqlite::memory:` uses one connection (each connection would
///   see its own database).
/// - **PostgreSQL**: one ledger per schema; one pool serves many schemas —
///   [`with_schema`](SqlStore::with_schema) gives each tenant its store
///   without a pool of its own; writes run `READ COMMITTED`
///   (pinned per transaction, whatever the server default) under a
///   transaction advisory lock keyed by the schema and the ledger id; reads
///   run `REPEATABLE READ READ ONLY` and never wait for a writer. A `BEGIN`
///   that fails on a pooled connection the server closed is tried again on
///   the next one; every write re-checks the meta row (schema, ledger,
///   scale) in its header read.
///
/// Both dialects refuse `UPDATE` and `DELETE` on the log and its indexes
/// with triggers whose messages start `doubleentry:`; PostgreSQL also
/// refuses `TRUNCATE`, SQLite a `REPLACE` (an insert repeating a key, and —
/// every store connection running with `recursive_triggers` on — the delete
/// it implies). What a trigger cannot refuse (an insert of a new row, a
/// change by the database's owner) `audit` finds: it compares every table
/// with the replay of the log.
pub struct SqlStore<D: Dialect, const P: u8 = 2> {
    inner: Arc<Inner<D>>,
}

struct Inner<D: Dialect> {
    pool: D::Pool,
    sql: Catalogue,
    schema: String,
    ledger: OnceLock<LedgerLock>,
    /// How often a `BEGIN` that failed on a dead pooled connection is
    /// tried again: once per pooled connection.
    retries: u32,
}

// Shared pools: PostgreSQL only — a SQLite ledger is its file.
#[cfg(feature = "postgres")]
impl<const P: u8> SqlStore<Postgres, P> {
    /// The store of the ledger in `schema`, sharing this store's connection
    /// pool: one pool serves every ledger of a database, one schema each —
    /// a multi-tenant service keeps one `SqlStore` per tenant schema without
    /// a pool per tenant. Nothing connects, reads or is created; each
    /// returned store keeps its own ledger lock key, so writers to different
    /// schemas never wait for each other. [`close`](SqlStore::close) on any
    /// of them closes the shared pool.
    ///
    /// ```no_run
    /// use doubleentry::store::{Postgres, SqlStore};
    /// use doubleentry::{Ledger, LedgerId};
    ///
    /// # async fn tenants(url: &str) -> Result<(), doubleentry::Error> {
    /// let pool = SqlStore::<Postgres>::postgres(url).await?; // one pool
    /// for tenant in ["acme", "globex"] {
    ///     let store = pool.with_schema(&format!("ledger_{tenant}"))?;
    ///     let books = Ledger::new(store, LedgerId::new(format!("example.com/{tenant}"))?).await?;
    ///     books.audit().await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// [`InvalidSchemaName`](crate::store::StoreErrorKind::InvalidSchemaName)
    /// for a schema name other than `[A-Za-z_][A-Za-z0-9_]{0,62}`.
    pub fn with_schema(&self, schema: &str) -> Result<Self, StoreError> {
        self.sharing_pool(schema)
    }
}

/// The ledger a store holds and the key of its single-writer lock.
#[derive(Debug, Clone)]
struct LedgerLock {
    id: LedgerId,
    key: i64,
}

impl<D: Dialect, const P: u8> Clone for SqlStore<D, P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<D: Dialect, const P: u8> fmt::Debug for SqlStore<D, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlStore")
            .field("schema", &self.inner.schema)
            .finish_non_exhaustive()
    }
}

/// Why a TLS connection failed on a build without TLS support.
const NO_TLS: &str = "TLS failed and doubleentry was built without its `postgres-tls` feature";

/// The first 8 bytes of `SHA-256(tag ‖ name)`, as a lock key.
fn lock_key(tag: &str, name: &str) -> i64 {
    let digest = Sha256::new()
        .chain_update(tag.as_bytes())
        .chain_update(name.as_bytes())
        .finalize();
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    i64::from_be_bytes(key)
}

impl<D: Dialect, const P: u8> SqlStore<D, P> {
    /// Connects a pool for `options`; nothing is read or created.
    async fn connect(url: &str, options: SqlOptions) -> Result<Self, StoreError> {
        let qualifier = D::qualifier(&options.schema)?;
        let pool = D::connect(url, &options).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                pool,
                sql: Catalogue::new(qualifier),
                schema: options.schema,
                ledger: OnceLock::new(),
                retries: options.max_connections,
            }),
        })
    }

    /// A store over `schema` sharing this store's pool.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    fn sharing_pool(&self, schema: &str) -> Result<Self, StoreError> {
        let qualifier = D::qualifier(schema)?;
        Ok(Self {
            inner: Arc::new(Inner {
                pool: self.inner.pool.clone(),
                sql: Catalogue::new(qualifier),
                schema: schema.to_owned(),
                ledger: OnceLock::new(),
                retries: self.inner.retries,
            }),
        })
    }

    /// Closes the store: every connection of its pool (shared by its clones)
    /// is closed, waiting for the ones in use. On SQLite the last one writes
    /// the write-ahead log into the database file, so the file alone is then
    /// a complete copy — close before backing up a file. Any later operation
    /// fails as [`Error::Backend`](crate::Error::Backend).
    pub async fn close(&self) {
        D::close(&self.inner.pool).await;
    }

    /// Opens a transaction for `operation`. A `BEGIN` that fails on a
    /// lost connection — a pooled connection the server closed (restart,
    /// failover, idle timeout) — provably ran nothing, so it is tried again
    /// on the next connection, at most once per pooled connection; the pool
    /// discards the dead one when it comes back.
    async fn session(
        &self,
        mode: Mode,
        operation: &'static str,
    ) -> Result<Session<'_, D>, StoreError> {
        let mut retries = self.inner.retries;
        let tx = loop {
            match D::begin(&self.inner.pool, mode).await {
                Ok(tx) => break tx,
                Err(error) if retries > 0 && Class::of(&error, D::classify) == Class::Lost => {
                    retries = retries.saturating_sub(1);
                }
                Err(error) => {
                    return Err(failure::<D>(operation, &error, false).with_source(error));
                }
            }
        };
        Ok(Session {
            tx,
            operation,
            sql: &self.inner.sql,
            rows: 0,
        })
    }

    /// The ledger this store holds and its lock key, read from `meta` once.
    async fn ledger(&self, session: &mut Session<'_, D>) -> Result<LedgerLock, StoreError> {
        if let Some(ledger) = self.inner.ledger.get() {
            return Ok(ledger.clone());
        }
        let sql = session.sql;
        let row = session
            .first(&sql.shared.meta, Vec::new())
            .await?
            .ok_or_else(|| StoreError::not_created(session.operation))?;
        let id = LedgerId::new(row.text(1)?).map_err(|_| {
            StoreError::integrity(session.operation, "the ledger id is invalid", None)
        })?;
        Ok(self.remember(id))
    }

    /// Caches the ledger id and its lock key: keyed by the schema and the
    /// ledger id, so two schemas never wait for each other, even holding the
    /// same ledger id (a restored copy beside the original).
    fn remember(&self, id: LedgerId) -> LedgerLock {
        self.inner
            .ledger
            .get_or_init(|| LedgerLock {
                key: lock_key(
                    "doubleentry/ledger-lock",
                    &format!("{}\u{0}{}", self.inner.schema, id.as_str()),
                ),
                id,
            })
            .clone()
    }

    /// The key of the lock that serialises concurrent creators.
    fn create_key(&self) -> i64 {
        lock_key("doubleentry/create", &self.inner.schema)
    }
}

/// An open transaction of one operation, counting the rows it reads.
pub(crate) struct Session<'s, D: Dialect> {
    tx: D::Tx,
    operation: &'static str,
    sql: &'s Catalogue,
    rows: u64,
}

/// A driver error as a store error of `operation`; `at_commit` when raised
/// while committing.
fn failure<D: Dialect>(
    operation: &'static str,
    error: &sqlx::Error,
    at_commit: bool,
) -> StoreError {
    // A hint, not a pre-check: a build whose sqlx gets TLS from another
    // dependency still connects. SQLite never fails with `Tls`.
    if matches!(error, sqlx::Error::Tls(_)) && !cfg!(feature = "postgres-tls") {
        return StoreError::backend_message(operation, NO_TLS);
    }
    let (failure, unknown) = match Class::of(error, D::classify) {
        Class::Busy => (StoreError::unavailable(operation), false),
        Class::Lost => (StoreError::unavailable(operation), true),
        Class::Integrity => (
            StoreError::integrity(operation, "the database refused the change", None),
            false,
        ),
        Class::MissingTable => (StoreError::not_created(operation), false),
        Class::Failed => (StoreError::failed(operation), false),
        Class::Broken => (StoreError::failed(operation), true),
    };
    if at_commit && unknown {
        failure.outcome_unknown()
    } else {
        failure
    }
}

impl<D: Dialect> Session<'_, D> {
    fn fail(&self, error: sqlx::Error) -> StoreError {
        failure::<D>(self.operation, &error, false).with_source(error)
    }

    /// Runs a statement and returns its rows.
    async fn fetch(
        &mut self,
        statement: &Statement,
        args: Vec<Value>,
    ) -> Result<Vec<Row>, StoreError> {
        let rows = D::fetch(&mut self.tx, statement, args)
            .await
            .map_err(|error| self.fail(error))?;
        self.count(rows.len());
        Ok(rows
            .into_iter()
            .map(|values| Row::new(self.operation, values))
            .collect())
    }

    /// Runs a statement and returns its rows as raw values.
    async fn fetch_values(
        &mut self,
        statement: &Statement,
        args: Vec<Value>,
    ) -> Result<Vec<Vec<Value>>, StoreError> {
        let rows = D::fetch(&mut self.tx, statement, args)
            .await
            .map_err(|error| self.fail(error))?;
        self.count(rows.len());
        Ok(rows)
    }

    /// Runs a statement and returns its first row.
    async fn first(
        &mut self,
        statement: &Statement,
        args: Vec<Value>,
    ) -> Result<Option<Row>, StoreError> {
        Ok(self.fetch(statement, args).await?.into_iter().next())
    }

    /// Runs a statement and returns the number of rows it changed.
    async fn execute(
        &mut self,
        statement: &Statement,
        args: Vec<Value>,
    ) -> Result<u64, StoreError> {
        D::execute(&mut self.tx, statement, args)
            .await
            .map_err(|error| self.fail(error))
    }

    /// Runs a batch over `rows` and returns the rows it selects (nothing
    /// for no rows).
    async fn batch(
        &mut self,
        batch: &Batch,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>, StoreError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let found = D::batch(&mut self.tx, batch, rows)
            .await
            .map_err(|error| self.fail(error))?;
        self.count(found.len());
        Ok(found)
    }

    /// Applies the changes of one write set.
    async fn apply(&mut self, writes: Vec<Write<'_>>) -> Result<(), StoreError> {
        D::apply(&mut self.tx, writes)
            .await
            .map_err(|error| self.fail(error))
    }

    /// Looks up `keys` with a batched lookup whose first column is the key's
    /// index: the rows found for each key, in key order — one round trip for
    /// every key.
    async fn keyed(
        &mut self,
        lookup: &Batch,
        keys: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Row>>, StoreError> {
        let mut grouped: Vec<Vec<Row>> = keys.iter().map(|_| Vec::new()).collect();
        let rows = keys
            .into_iter()
            .zip(0i64..)
            .map(|(mut key, index)| {
                key.insert(0, Value::Int(index));
                key
            })
            .collect();
        for mut values in self.batch(lookup, rows).await? {
            let index = match values.first() {
                Some(Value::Int(index)) => usize::try_from(*index).ok(),
                _ => None,
            };
            let slot = index
                .and_then(|index| grouped.get_mut(index))
                .ok_or_else(|| {
                    StoreError::integrity(self.operation, "a batched row names no key", None)
                })?;
            values.remove(0);
            slot.push(Row::new(self.operation, values));
        }
        Ok(grouped)
    }

    /// Takes the transaction-scoped lock `key`. `Err(None)` when it failed
    /// on a lost connection before anything of the transaction ran: the
    /// caller may begin again.
    async fn lock(&mut self, key: i64) -> Result<(), Option<StoreError>> {
        match D::lock(&mut self.tx, key).await {
            Ok(()) => Ok(()),
            Err(error) if Class::of(&error, D::classify) == Class::Lost => Err(None),
            Err(error) => Err(Some(self.fail(error))),
        }
    }

    /// Runs a DDL script.
    async fn script(&mut self, script: String) -> Result<(), StoreError> {
        D::script(&mut self.tx, script)
            .await
            .map_err(|error| self.fail(error))
    }

    /// The committed size, as this transaction sees it.
    async fn size(&mut self) -> Result<u64, StoreError> {
        let sql = self.sql;
        match self.first(&sql.shared.size, Vec::new()).await? {
            Some(row) => row.u64(0),
            None => Ok(0),
        }
    }

    /// Commits; a failure raised while committing may have applied.
    async fn commit(self) -> Result<(), StoreError> {
        let operation = self.operation;
        D::commit(self.tx)
            .await
            .map_err(|error| failure::<D>(operation, &error, true).with_source(error))
    }

    /// Rolls back: nothing of this transaction is kept.
    async fn rollback(self) -> Result<(), StoreError> {
        let operation = self.operation;
        D::rollback(self.tx)
            .await
            .map_err(|error| failure::<D>(operation, &error, false).with_source(error))
    }

    fn count(&mut self, rows: usize) {
        self.rows = self
            .rows
            .saturating_add(u64::try_from(rows).unwrap_or(u64::MAX));
    }
}

impl<D: Dialect, const P: u8> Store<P> for SqlStore<D, P> {
    async fn create(&self, genesis: &Genesis) -> Result<LedgerInfo, StoreError> {
        write::create(self, genesis).await
    }

    async fn open(&self) -> Result<LedgerInfo, StoreError> {
        write::open(self).await
    }

    async fn write(&self, op: &Op<P>) -> Result<Result<Done<P>, Rejection>, StoreError> {
        write::write(self, op).await
    }

    async fn size(&self) -> Result<u64, StoreError> {
        let mut session = self.session(Mode::Read, "size").await?;
        let size = session.size().await?;
        session.commit().await?;
        Ok(size)
    }

    async fn read_leaves(&self, read: &LeafRead) -> Result<Page<Leaf, Position>, StoreError> {
        reads::read_leaves(self, read).await
    }

    async fn read_nodes(&self, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
        reads::read_nodes(self, ids).await
    }

    async fn seals(&self, through: Option<u64>) -> Result<Vec<SealRow>, StoreError> {
        reads::seals(self, through).await
    }

    async fn accounts(&self, refs: &[AccountRef]) -> Result<Vec<Option<AccountRow>>, StoreError> {
        reads::accounts(self, refs).await
    }

    async fn account_records(
        &self,
        handle: AccountHandle,
        page: PageRead<Position>,
    ) -> Result<Page<Leaf, Position>, StoreError> {
        reads::account_records(self, handle, &page).await
    }

    async fn entry(
        &self,
        by: EntryRef<'_>,
        through: Option<u64>,
    ) -> Result<Option<StoredEntry>, StoreError> {
        reads::entry(self, by, through).await
    }

    async fn holds(&self, read: &HoldRead) -> Result<Page<HoldRow<P>, HoldCursor>, StoreError> {
        reads::holds(self, read).await
    }

    async fn clearings(
        &self,
        key: BalanceKey,
        page: PageRead<Position>,
    ) -> Result<Page<Leaf, Position>, StoreError> {
        reads::clearings(self, key, &page).await
    }

    async fn projection(&self, read: &ProjectionRead) -> Result<ProjectionPage<P>, StoreError> {
        reads::projection(self, read).await
    }

    async fn seal_nodes(
        &self,
        period: &PeriodId,
        currency: Currency,
        layer: Layer,
        ids: &[(u32, u32)],
    ) -> Result<Option<Vec<Option<Hash>>>, StoreError> {
        reads::seal_nodes(self, period, currency, layer, ids)
            .await
            .map(Some)
    }

    async fn balances(
        &self,
        read: &BalanceRead,
    ) -> Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError> {
        reads::balances(self, read).await
    }

    async fn postings(
        &self,
        read: &PostingRead,
    ) -> Result<Page<PostingLine<P>, PostingCursor>, StoreError> {
        reads::postings(self, read).await
    }

    async fn memo(&self, id: EntryId) -> Result<Option<Memo>, StoreError> {
        reads::memo(self, id).await
    }

    async fn erase_memo(&self, id: EntryId) -> Result<Erasure, StoreError> {
        write::erase_memo(self, id).await
    }
}

#[cfg(test)]
mod tests {
    use super::lock_key;

    #[test]
    fn lock_keys_differ_per_ledger_and_purpose() {
        let a = lock_key("doubleentry/ledger-lock", "example.com/a");
        assert_ne!(a, lock_key("doubleentry/ledger-lock", "example.com/b"));
        assert_ne!(a, lock_key("doubleentry/create", "example.com/a"));
        assert_eq!(a, lock_key("doubleentry/ledger-lock", "example.com/a"));
    }
}
