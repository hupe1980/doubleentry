//! A PostgreSQL-backed [`LedgerStore`].
//!
//! The schema is [`schema/postgres.sql`](https://github.com/hupe1980/doubleentry/blob/main/schema/postgres.sql),
//! applied by [`PostgresStore::migrate`]. Every constraint in it is load-bearing;
//! the two that are easy to get wrong are worth restating here.
//!
//! # Index assignment
//!
//! Log indices must be dense, gap-free, and in commit order. A `SEQUENCE` cannot
//! provide that: `nextval` is consumed before commit, so a transaction holding
//! index 5 may commit *after* one holding 6, and a reader tracking a high-water
//! mark steps over 5 permanently. The index is therefore assigned inside the
//! append, under a per-ledger advisory lock held for the transaction's duration.
//!
//! That serialises appends. It is the right trade for a first backend — correct
//! and simple — and the shape to move to under load is described in the schema's
//! operational notes.
//!
//! # How the tree is stored
//!
//! In `log_nodes`, apart from the entries: every Merkle node whose subtree is
//! complete, keyed by its position in the order nodes become complete. A
//! completed subtree can never change in an append-only log, so the table is
//! INSERT-only with a monotonic key, exactly like `entries`.
//!
//! Two things follow. A proof reads at most `1 + ceil(log2 n)` rows rather than
//! replaying the whole log, and archiving a prefix of `entries` to a
//! [cold tier](crate::storage::iceberg) leaves the tree — and therefore every
//! proof — intact.
//!
//! There is no stored root beside it. A head at any size is the same `O(log n)`
//! lookup, so a root column would be a second copy of a derived value, and two
//! fields that must agree are two fields that can disagree.
//!
//! # Integrity on read
//!
//! Rows are rehydrated through [`Entry::adopt_verified`], which recomputes the
//! content hash and compares it with the one stored alongside. A row altered
//! underneath the engine surfaces as an error on the next read rather than as a
//! wrong number in a report.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Row, Transaction};
use time::Date;

use crate::account::{
    Account, AccountId, AccountKind, AccountPath, AccountRecord, AccountRegistry, BalanceLimit,
};
use crate::balance::{Balance, BalanceKey, BalanceQuery, DateBasis, TrialBalance};
use crate::checkpoint::Checkpoint;
use crate::clearing::{Clearing, ClearingId, OpenItem, PostingPosition, PostingRef};
use crate::dimensions::{Dimensions, Label};
use crate::entry::{
    Balanced, Description, DocumentRef, Draft, Entry, EntryId, IdempotencyKey, IntegrityError,
    Provenance,
};
use crate::hash::Hash;
use crate::journal::{LogIndex, Recorded};
use crate::merkle::nodes;
use crate::merkle::{
    ConsistencyProof, InclusionProof, MalformedAccumulator, MerkleAccumulator, ProofError, TreeHead,
};
use crate::money::{Amount, Currency, MoneyError};
use crate::period::{LedgerId, Period, PeriodCalendar, PeriodId, PeriodState};
use crate::posting::{Direction, Layer, Posting};
use crate::seal::{PeriodCoverage, Seal, SealChain};
use crate::storage::{
    Cursor, EntryBatch, LedgerStore, OpenItemPage, Page, PostingCursor, StatementPage, StoredEntry,
};

/// The reference DDL, applied by [`PostgresStore::migrate`].
pub const SCHEMA: &str = include_str!("../../schema/postgres.sql");

/// The schema [`PostgresStore`] places its tables in unless told otherwise.
///
/// The ledger's tables live in a schema of their own rather than in `public` so
/// they can share a database with an application's own tables without competing
/// for names — `accounts` in particular is a name many applications have already
/// spent on something else.
///
/// This is a default, not a policy: pass any schema to
/// [`PostgresStore::connect_with`], including `"public"` when the database is
/// the ledger's alone. Whatever the choice, [`PostgresStore::migrate`] verifies
/// that unqualified names actually resolve there and refuses to run otherwise,
/// rather than quietly creating a second set of tables somewhere else.
pub const DEFAULT_SCHEMA: &str = "doubleentry";

/// Advisory-lock key serialising position assignment for one ledger.
///
/// Taken by an inline append, and by every sequencing pass. Reads take no lock.
const APPEND_LOCK: i64 = 0x0064_6f75_626c_6531;

/// Whether one append assigns a position or leaves it to the sequencer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Inline,
    Deferred,
}

/// Failure from the PostgreSQL backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostgresError {
    /// The database refused or failed the operation.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// A stored row did not match its recorded content hash.
    #[error(transparent)]
    Integrity(#[from] IntegrityError),
    /// A stored row could not be interpreted.
    #[error("stored data is malformed: {0}")]
    Malformed(String),
    /// The idempotency key is already held by an entry with different content.
    #[error("idempotency key already used by entry {existing} with different content")]
    IdempotencyConflict {
        /// The entry already holding the key.
        existing: EntryId,
    },
    /// A proof could not be built.
    #[error(transparent)]
    Proof(#[from] ProofError),
    /// A Merkle node the tree requires is not in the store.
    ///
    /// Nodes are written once, never updated and never deleted, so a missing one
    /// is not a state the engine can reach: it means the rows read back are not
    /// the rows written. Named rather than skipped, because folding around a
    /// hole would produce a proof that verifies against a root nobody else
    /// computes.
    #[error(
        "merkle node {index} is missing; nodes are append-only, so this is a \
         partial restore or a table something else has written to"
    )]
    MissingNode {
        /// The storage position that could not be read.
        index: u64,
    },
    /// The nodes read back are not the ones a proof plan asked for.
    #[error(transparent)]
    Nodes(#[from] crate::merkle::nodes::MalformedNodes),
    /// The node store holds a count no whole number of records produces.
    ///
    /// A record writes all of its nodes or none of them, inside the transaction
    /// that writes the entry, so this is not a state an append can leave behind.
    /// It means a partial restore, or something other than the engine writing to
    /// the table.
    #[error(
        "the node store holds {nodes} nodes, which no number of records \
         produces; this is a partial restore rather than a log"
    )]
    PartialLog {
        /// How many nodes are held.
        nodes: u64,
    },
    /// A seal did not chain onto the one before it.
    #[error(transparent)]
    Seal(#[from] crate::seal::SealChainError),
    /// A sealed balance could not be proven.
    #[error(transparent)]
    SealedBalance(#[from] crate::seal::SealedBalanceError),
    /// An account binding could not be rebuilt.
    #[error(transparent)]
    Account(#[from] crate::account::AccountError),
    /// Arithmetic overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),
    /// A clearing was refused.
    #[error(transparent)]
    Clearing(#[from] crate::clearing::ClearingError),
    /// A reversal referenced an entry that is not stored.
    #[error("cannot reverse unknown entry {id}")]
    UnknownOriginal {
        /// The referenced identifier.
        id: EntryId,
    },
    /// The referenced entry has already been reversed.
    #[error("entry {id} has already been reversed")]
    AlreadyReversed {
        /// The entry being reversed.
        id: EntryId,
    },
    /// A reversal was aimed at another reversal.
    #[error("entry {id} is itself a reversal and cannot be reversed")]
    ReversalOfReversal {
        /// The offending identifier.
        id: EntryId,
    },
    /// An entry claiming to reverse another does not actually invert it.
    #[error("entry claiming to reverse {id} does not invert its postings")]
    NotAnInversion {
        /// The entry it claims to reverse.
        id: EntryId,
    },
    /// A clearing was reset that is unknown or already released.
    #[error("clearing {id} is unknown or already reset")]
    ClearingNotResettable {
        /// The offending identifier.
        id: ClearingId,
    },
    /// Unqualified names would not resolve to this store's schema.
    #[error(
        "search_path resolves to {found:?}, not {expected:?}; \
         build the pool with PostgresStore::connect, or set \
         `options=-c search_path={expected}` on the connection"
    )]
    WrongSearchPath {
        /// The schema this store was configured for.
        expected: String,
        /// The schema unqualified names currently resolve to.
        found: String,
    },
    /// The subtree cover read back does not describe a log of the claimed size.
    #[error(transparent)]
    Accumulator(#[from] MalformedAccumulator),
    /// The entry identifier is already used by a different entry.
    #[error("entry {id} is already recorded")]
    DuplicateId {
        /// The offending identifier.
        id: EntryId,
    },
    /// The calendar refused a period operation.
    #[error(transparent)]
    Period(#[from] crate::period::PeriodError),
    /// Registering this path would put postings on an aggregation node.
    ///
    /// Only leaves are postable, and the named ancestor has already been posted
    /// to. See [`JournalError::AncestorHasPostings`](crate::JournalError::AncestorHasPostings).
    #[error(
        "cannot register {path}: its ancestor {ancestor} has already been posted \
         to, and only leaves are postable"
    )]
    AncestorHasPostings {
        /// The path being registered.
        path: String,
        /// The registered ancestor that already carries postings.
        ancestor: String,
    },
    /// A handle was re-registered against a different account path.
    ///
    /// The path at a handle is what every posting row and every sealed balance
    /// means by it. Rebinding one would silently repoint history, so it is
    /// refused rather than applied.
    #[error("account {id} is already bound to a different path")]
    AccountRebound {
        /// The offending handle.
        id: AccountId,
    },
    /// An entry would leave an account past its balance limit.
    #[error(
        "account {account} in {currency} would be {} minor units past its \
         {limit} limit",
        .headroom_minor.saturating_neg()
    )]
    LimitBreached {
        /// The account whose limit would be breached.
        account: AccountId,
        /// The currency the limit was breached in.
        currency: Currency,
        /// The limit in force.
        limit: BalanceLimit,
        /// Room left under the limit, in minor units — negative here.
        headroom_minor: i128,
    },
    /// The database already holds a different ledger.
    #[error("this database holds ledger {found}, not {expected}")]
    WrongLedger {
        /// The ledger this store was opened for.
        expected: LedgerId,
        /// The ledger the database actually holds.
        found: LedgerId,
    },
}

impl PostgresError {
    fn malformed(what: impl Into<String>) -> Self {
        Self::Malformed(what.into())
    }
}

/// When log positions are assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sequencing {
    /// Assigned inside the append, under an advisory lock.
    ///
    /// An entry is provable the moment it is durable. Appends to one ledger
    /// serialise, because the next position cannot be read until the previous
    /// writer has committed.
    #[default]
    Inline,
    /// Assigned afterwards by [`PostgresStore::sequence`].
    ///
    /// Writers insert concurrently and never block each other. The cost is a
    /// window in which an entry is durable and idempotency-checked but has no
    /// position yet, so [`LedgerStore::append`] returns `index: None` and the
    /// entry is not in the log until a sequencing pass has run.
    ///
    /// Worth it when many small appends arrive at once; unnecessary when writes
    /// already arrive in batches, since a batch takes the lock once.
    ///
    /// # Sequencing latency depends on the whole cluster
    ///
    /// The watermark the sequencer advances on —
    /// `pg_snapshot_xmin(pg_current_snapshot())` — is **cluster-wide**, not
    /// per-database and not per-table. A transaction left open anywhere in the
    /// instance holds it back, and entries recorded after that transaction began
    /// wait until it ends.
    ///
    /// This is safe, never lossy: the sequencer declines to place rows it cannot
    /// yet prove are settled, and places them on a later pass. But it means
    /// sequencing latency is bounded by the *longest open transaction in the
    /// cluster*, so a reporting query left running for ten minutes delays
    /// provability by ten minutes. Deployments that care should monitor
    /// `pg_stat_activity` for long transactions, or keep analytics on a replica.
    Deferred,
}

/// A ledger stored in PostgreSQL.
#[derive(Debug, Clone)]
pub struct PostgresStore<const P: u8> {
    pool: PgPool,
    ledger: LedgerId,
    sequencing: Sequencing,
    schema: String,
}

impl<const P: u8> PostgresStore<P> {
    /// Connects to `url` and serves one ledger from [`DEFAULT_SCHEMA`].
    ///
    /// # Errors
    ///
    /// Returns any error the database raises while connecting.
    pub async fn connect(url: &str, ledger: LedgerId) -> Result<Self, PostgresError> {
        Self::connect_with(url, ledger, DEFAULT_SCHEMA).await
    }

    /// Connects to `url` and serves one ledger from `schema`.
    ///
    /// Sets `search_path` so unqualified names resolve to `schema`. Pass
    /// `"public"` when the database belongs to the ledger alone, or your own
    /// name when a naming policy says so. Prefer this over building the pool
    /// yourself unless you need pool options of your own.
    ///
    /// # Errors
    ///
    /// Returns any error the database raises while connecting.
    pub async fn connect_with(
        url: &str,
        ledger: LedgerId,
        schema: &str,
    ) -> Result<Self, PostgresError> {
        let options: PgConnectOptions = url
            .parse::<PgConnectOptions>()
            .map_err(PostgresError::from)?
            .options([("search_path", schema)]);
        let pool = PgPoolOptions::new().connect_with(options).await?;
        Ok(Self::new(pool, ledger).in_schema(schema))
    }

    /// Wraps a connection pool, serving one ledger, assigning positions inline.
    ///
    /// The pool must resolve unqualified names to the store's schema —
    /// [`DEFAULT_SCHEMA`] unless changed with [`Self::in_schema`] — and
    /// [`Self::migrate`] refuses to run otherwise. [`Self::connect`] and
    /// [`Self::connect_with`] set this up for you.
    #[must_use]
    pub fn new(pool: PgPool, ledger: LedgerId) -> Self {
        Self {
            pool,
            ledger,
            sequencing: Sequencing::Inline,
            schema: DEFAULT_SCHEMA.to_owned(),
        }
    }

    /// Wraps a connection pool with the given sequencing mode.
    #[must_use]
    pub fn with_sequencing(pool: PgPool, ledger: LedgerId, sequencing: Sequencing) -> Self {
        Self {
            pool,
            ledger,
            sequencing,
            schema: DEFAULT_SCHEMA.to_owned(),
        }
    }

    /// Expects this store's tables in `schema` rather than [`DEFAULT_SCHEMA`].
    ///
    /// Only says where the tables belong; the pool must already resolve
    /// unqualified names there, which [`Self::migrate`] checks.
    #[must_use]
    pub fn in_schema(mut self, schema: &str) -> Self {
        self.schema = schema.to_owned();
        self
    }

    /// The schema this store expects its tables in.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// How this store assigns log positions.
    #[must_use]
    pub fn sequencing(&self) -> Sequencing {
        self.sequencing
    }

    /// The underlying pool.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies [`SCHEMA`].
    ///
    /// # Errors
    ///
    /// Returns any error the database raises.
    pub async fn migrate(&self) -> Result<(), PostgresError> {
        // Create the schema before checking for it, so a correctly configured
        // pool works on a database that has never seen this crate.
        // Quoted so a schema name needing quoting is not silently folded to
        // lower case or split on a dot.
        sqlx::query(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            self.schema.replace('"', "\"\"")
        ))
        .execute(&self.pool)
        .await?;

        // `current_schema()` is where an unqualified CREATE TABLE lands. If it
        // is not ours, every table below would be created somewhere else — most
        // likely `public`, on top of whatever the application keeps there.
        let current: Option<String> = sqlx::query("SELECT current_schema() AS s")
            .fetch_one(&self.pool)
            .await?
            .try_get("s")?;
        if current.as_deref() != Some(self.schema.as_str()) {
            return Err(PostgresError::WrongSearchPath {
                expected: self.schema.clone(),
                found: current.unwrap_or_default(),
            });
        }

        self.pool.execute_schema().await?;
        // One database, one ledger. Claim it on first use and refuse it
        // afterwards if it belongs to someone else — pointing two ledgers at one
        // database would merge two logs, two index spaces, and two seal chains
        // into one, silently.
        sqlx::query(
            "INSERT INTO ledger_meta (only_row, ledger_id) VALUES (1, $1) \
             ON CONFLICT (only_row) DO NOTHING",
        )
        .bind(self.ledger.as_str())
        .execute(&self.pool)
        .await?;

        let found: String = sqlx::query("SELECT ledger_id FROM ledger_meta WHERE only_row = 1")
            .fetch_one(&self.pool)
            .await?
            .try_get("ledger_id")?;
        if found != self.ledger.as_str() {
            return Err(PostgresError::WrongLedger {
                expected: self.ledger.clone(),
                found: LedgerId::new(found).map_err(|e| PostgresError::malformed(e.to_string()))?,
            });
        }
        Ok(())
    }

    /// The calendar as the database holds it.
    ///
    /// Use it to validate drafts locally without another round trip per entry.
    ///
    /// # Errors
    ///
    /// Returns any error the database raises, or a malformed row.
    pub async fn calendar(&self) -> Result<PeriodCalendar, PostgresError> {
        Ok(PeriodCalendar::from_periods(
            <Self as LedgerStore<P>>::periods(self).await?,
        )?)
    }

    /// How many records the node store describes.
    ///
    /// Derived from `log_nodes` rather than counted from `entries`, because the
    /// nodes **are** the log: an entry archived to a cold tier and pruned from
    /// hot storage leaves the tree exactly as it was, and a size counted from
    /// surviving rows would shrink underneath every proof and every seal.
    ///
    /// One indexed row read: the highest position held, inverted through
    /// [`nodes::count`]. A record writes all of its nodes or none of them, so a
    /// count that lands between two sizes is a partially applied append — which
    /// a transaction rules out, and which is named rather than rounded off.
    async fn log_size<'e, E>(executor: E) -> Result<u64, PostgresError>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        let row = sqlx::query("SELECT MAX(node_index) AS top FROM log_nodes")
            .fetch_one(executor)
            .await?;
        let top: Option<i64> = row.try_get("top")?;
        let Some(top) = top else { return Ok(0) };
        let held = u64::try_from(top).unwrap_or(0).saturating_add(1);
        nodes::size_from_count(held).ok_or(PostgresError::PartialLog { nodes: held })
    }

    /// Reads the nodes a plan named, **in plan order**.
    ///
    /// One query however many nodes are wanted, which is at most
    /// `1 + ceil(log2 n)` — 64 rows for a log of every entry that will ever
    /// exist. The result is reordered to the plan rather than returned in
    /// primary-key order: a plan's output is consumed positionally, so a
    /// silently reordered read would assemble a well-formed proof of the wrong
    /// shape.
    ///
    /// A position the store cannot produce is named rather than skipped. Nodes
    /// are never updated and never deleted, so a missing one means the rows read
    /// back are not the rows written.
    async fn read_nodes<'e, E>(executor: E, want: &[u64]) -> Result<Vec<Hash>, PostgresError>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        if want.is_empty() {
            return Ok(Vec::new());
        }
        let positions: Vec<i64> = want
            .iter()
            .map(|index| i64::try_from(*index).unwrap_or(i64::MAX))
            .collect();
        let rows = sqlx::query("SELECT node_index, node FROM log_nodes WHERE node_index = ANY($1)")
            .bind(&positions)
            .fetch_all(executor)
            .await?;

        let mut found: BTreeMap<u64, Hash> = BTreeMap::new();
        for row in &rows {
            let at: i64 = row.try_get("node_index")?;
            let node: Vec<u8> = row.try_get("node")?;
            found.insert(u64::try_from(at).unwrap_or(0), hash_from_bytes(&node)?);
        }
        want.iter()
            .map(|index| {
                found
                    .get(index)
                    .copied()
                    .ok_or(PostgresError::MissingNode { index: *index })
            })
            .collect()
    }

    /// The tree head at `size`, from `O(log n)` node reads.
    ///
    /// Deliberately **not** in a transaction, and safe without one. The cover of
    /// a log of `size` records is made entirely of completed subtrees, and a
    /// completed subtree never changes in an append-only log — so a concurrent
    /// append cannot disturb this read. The worst it can do is make the answer
    /// one entry stale, which is what any read of a growing log is.
    async fn head_from_nodes<'e, E>(executor: E, size: u64) -> Result<TreeHead, PostgresError>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        let plan = nodes::RootPlan::new(size);
        let read = Self::read_nodes(executor, plan.nodes()).await?;
        Ok(TreeHead {
            size,
            root: plan.assemble(&read)?,
        })
    }

    /// Restores the subtree cover an append needs, from the stored nodes.
    ///
    /// The cover is one node per set bit in the size, and every one of them is a
    /// completed subtree — so it is a lookup rather than a rebuild, and there is
    /// no separate accumulator table that could stop agreeing with the tree.
    async fn accumulator(
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<MerkleAccumulator, PostgresError> {
        let size = Self::log_size(&mut **tx).await?;
        let plan = nodes::RootPlan::new(size);
        let read = Self::read_nodes(&mut **tx, plan.nodes()).await?;
        let subtrees = plan
            .nodes()
            .iter()
            .zip(read)
            .map(|(at, hash)| (nodes::split_index(*at).0, hash))
            .collect();
        // Checked, not trusted: the heights have to be the decomposition the
        // size requires, or the rows read back are not the rows written.
        Ok(MerkleAccumulator::try_from_parts(subtrees, size)?)
    }

    /// Appends the nodes a record's arrival completed.
    ///
    /// Positions are consecutive from [`nodes::count`] of the previous size, so
    /// this table is INSERT-only with a monotonic key — never an update, never a
    /// delete.
    async fn store_nodes(
        tx: &mut Transaction<'_, Postgres>,
        first_index: u64,
        written: &[Hash],
    ) -> Result<(), PostgresError> {
        for (offset, node) in written.iter().enumerate() {
            let at = first_index.saturating_add(offset as u64);
            sqlx::query("INSERT INTO log_nodes (node_index, node) VALUES ($1, $2)")
                .bind(i64::try_from(at).unwrap_or(i64::MAX))
                .bind(node.as_bytes().as_slice())
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    /// Gross totals over the postings `query` selects on `key`, optionally
    /// stopping at a posting position.
    ///
    /// The two halves of a statement page's opening balance, and nothing else
    /// uses it. `None` for the query selects nothing at all, which is what an
    /// unscoped statement carries in.
    async fn fold_postings(
        &self,
        key: BalanceKey,
        query: Option<BalanceQuery<'_>>,
        through: Option<PostingPosition>,
    ) -> Result<Balance<P>, PostgresError> {
        let Some(query) = query else {
            return Ok(Balance::ZERO);
        };
        let mut sql = Sql::default();
        let joins = dimension_joins(&mut sql, &query);
        let account_bind = sql.int(i64::from(key.account.index()));
        let currency = sql.text(key.currency.code());
        let layer = sql.text(layer_str(key.layer));
        let bound = match through {
            Some(position) => {
                let index = sql.int(i64::try_from(position.index.get()).unwrap_or(i64::MAX));
                let posting = sql.int(i64::from(position.posting));
                format!(
                    " AND (e.log_index < {index} \
                       OR (e.log_index = {index} AND p.posting_index <= {posting}))"
                )
            }
            None => String::new(),
        };
        let predicate = entry_predicate(&mut sql, &query);
        let text = format!(
            "SELECT \
                COALESCE(SUM(CASE WHEN p.direction = 'D' \
                                  THEN p.amount_minor ELSE 0 END), 0)::BIGINT AS debits, \
                COALESCE(SUM(CASE WHEN p.direction = 'C' \
                                  THEN p.amount_minor ELSE 0 END), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id{joins} \
             WHERE p.account_index = {account_bind} AND p.currency = {currency} \
               AND p.layer = {layer}{bound}{predicate}"
        );
        let row = sql.apply(sqlx::query(&text)).fetch_one(&self.pool).await?;
        Ok(Balance::<P> {
            debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
            credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
        })
    }

    /// Loads postings for several entries at once, grouped by log index.
    async fn load_postings_for(
        &self,
        ids: &[uuid::Uuid],
    ) -> Result<std::collections::BTreeMap<uuid::Uuid, Vec<Posting<P>>>, PostgresError> {
        let mut grouped: std::collections::BTreeMap<uuid::Uuid, Vec<Posting<P>>> =
            std::collections::BTreeMap::new();
        if ids.is_empty() {
            return Ok(grouped);
        }
        let rows = sqlx::query(
            "SELECT entry_id, posting_index, account_index, direction, amount_minor, currency, \
             layer \
             FROM postings WHERE entry_id = ANY($1) ORDER BY entry_id, posting_index",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;

        // One query for the axes too, rather than one per posting.
        let dim_rows = sqlx::query(
            "SELECT entry_id, posting_index, axis, value FROM posting_dimensions \
             WHERE entry_id = ANY($1) ORDER BY entry_id, posting_index, axis",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        let dimensions = dimensions_from(&dim_rows)?;

        for row in &rows {
            let entry_id: uuid::Uuid = row.try_get("entry_id")?;
            let posting_index: i16 = row.try_get("posting_index")?;
            let mut posting = build_posting::<P>(row)?;
            if let Some(dims) = dimensions.get(&(entry_id, posting_index)) {
                posting.dimensions = dims.clone();
            }
            grouped.entry(entry_id).or_default().push(posting);
        }
        Ok(grouped)
    }

    /// Appends one entry inside an open transaction.
    ///
    /// Returns the outcome, distinguishing a fresh append from a safe replay.
    async fn append_one(
        tx: &mut Transaction<'_, Postgres>,
        entry: &Entry<Balanced, P>,
        next_index: &mut i64,
        accumulator: &mut MerkleAccumulator,
        placement: Placement,
    ) -> Result<Recorded, PostgresError> {
        let content_hash = entry.content_hash();

        // The primary key would refuse this anyway; catching it here turns a
        // constraint violation into an error that names what went wrong. Scoped
        // to a *different* idempotency key, so a genuine retry — same
        // identifier, same key — still falls through to the replay path below
        // rather than being reported as a clash with itself.
        let clash: Option<i32> =
            sqlx::query("SELECT 1 AS x FROM entries WHERE entry_id = $1 AND idempotency_key <> $2")
                .bind(entry.id().as_uuid())
                .bind(entry.idempotency_key().as_bytes())
                .fetch_optional(&mut **tx)
                .await?
                .map(|row| row.try_get("x"))
                .transpose()?;
        if clash.is_some() {
            return Err(PostgresError::DuplicateId { id: entry.id() });
        }

        if let Some(original) = entry.reverses() {
            Self::check_reversal(tx, entry, original).await?;
        }

        // The unique index is the idempotency gate, claimed by this INSERT
        // rather than by a preceding SELECT — a read-then-write races.
        // Speculative: the nodes this entry would complete. Only committed to
        // the accumulator, and only written, if the row actually lands.
        //
        // In inline mode the position is assigned now and the tree grows with
        // it; in deferred mode the position stays NULL and no node is written
        // until the sequencer runs, because an entry with no position is not in
        // the log the tree commits to.
        let (assigned_index, staged) = match placement {
            Placement::Inline => {
                let mut projected = accumulator.clone();
                let first_node = nodes::count(projected.size());
                let (_, written) = projected.push_recording(content_hash);
                (Some(*next_index), Some((first_node, written, projected)))
            }
            Placement::Deferred => (None, None),
        };

        let inserted = sqlx::query(
            "INSERT INTO entries ( \
                log_index, entry_id, idempotency_key, content_hash, booking_date, value_date, \
                description, provenance_actor, provenance_source, provenance_correlation, \
                document_id, document_content_hash, reverses, original_booking_date, \
                kind \
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) \
             ON CONFLICT (idempotency_key) DO NOTHING \
             RETURNING entry_id",
        )
        .bind(assigned_index)
        .bind(entry.id().as_uuid())
        .bind(entry.idempotency_key().as_bytes())
        .bind(content_hash.as_bytes().as_slice())
        .bind(entry.booking_date())
        .bind(entry.value_date())
        .bind(entry.description().as_str())
        .bind(entry.provenance().actor.as_ref().map(|l| l.as_str()))
        .bind(entry.provenance().source.as_ref().map(|l| l.as_str()))
        .bind(entry.provenance().correlation.as_ref().map(|l| l.as_str()))
        .bind(entry.document().map(|d| d.id.as_str()))
        .bind(
            entry
                .document()
                .and_then(|d| d.content_hash.as_ref())
                .map(|h| h.as_bytes().as_slice()),
        )
        .bind(entry.reverses().map(|r| *r.as_uuid()))
        .bind(entry.original_booking_date())
        .bind(entry.kind().map(|k| k.as_str()))
        .fetch_optional(&mut **tx)
        .await?;

        let Some(_) = inserted else {
            // The key was taken. Identical content is a safe replay; anything
            // else is a conflict and must not overwrite.
            let existing = sqlx::query(
                "SELECT log_index, entry_id, content_hash FROM entries WHERE idempotency_key = $1",
            )
            .bind(entry.idempotency_key().as_bytes())
            .fetch_one(&mut **tx)
            .await?;

            let stored_hash: Vec<u8> = existing.try_get("content_hash")?;
            let stored_id: uuid::Uuid = existing.try_get("entry_id")?;
            let stored_index: Option<i64> = existing.try_get("log_index")?;

            if hash_from_bytes(&stored_hash)? != content_hash {
                return Err(PostgresError::IdempotencyConflict {
                    existing: EntryId::from_uuid(stored_id),
                });
            }
            return Ok(Recorded {
                id: EntryId::from_uuid(stored_id),
                index: stored_index.map(|i| LogIndex::new(u64::try_from(i).unwrap_or(0))),
                content_hash,
                is_new: false,
            });
        };

        for (position, posting) in entry.postings().iter().enumerate() {
            let position = i16::try_from(position).unwrap_or(i16::MAX);
            sqlx::query(
                "INSERT INTO postings ( \
                    entry_id, posting_index, account_index, direction, amount_minor, currency, \
                    layer \
                 ) VALUES ($1,$2,$3,$4,$5,$6,$7)",
            )
            .bind(entry.id().as_uuid())
            .bind(position)
            .bind(i32::try_from(posting.account.index()).unwrap_or(i32::MAX))
            .bind(direction_str(posting.direction))
            .bind(posting.amount.to_minor())
            .bind(posting.currency.code())
            .bind(layer_str(posting.layer))
            .execute(&mut **tx)
            .await?;

            for (axis, value) in posting.dimensions.iter() {
                sqlx::query(
                    "INSERT INTO posting_dimensions (entry_id, posting_index, axis, value) \
                     VALUES ($1,$2,$3,$4)",
                )
                .bind(entry.id().as_uuid())
                .bind(position)
                .bind(axis.as_str())
                .bind(value.as_str())
                .execute(&mut **tx)
                .await?;
            }
        }

        Self::check_limits(tx, entry).await?;

        if let Some((first_node, written, projected)) = staged {
            // The tree, in the same transaction as the entry that extended it.
            // A crash between the two would leave a log whose size and whose
            // nodes disagree, which is the one inconsistency nothing downstream
            // could repair.
            Self::store_nodes(tx, first_node, &written).await?;
            *next_index = next_index.saturating_add(1);
            *accumulator = projected;
        }
        Ok(Recorded {
            id: entry.id(),
            index: assigned_index.map(|i| LogIndex::new(u64::try_from(i).unwrap_or(0))),
            content_hash,
            is_new: true,
        })
    }

    /// Refuses the entry if it leaves a constrained account on a forbidden side.
    ///
    /// Run *after* the postings are inserted and inside the same transaction, so
    /// the aggregate sees exactly the balance the entry would leave behind and a
    /// breach rolls the whole batch back with it. Checking beforehand would race
    /// with any concurrent append and would have to reimplement the fold.
    ///
    /// The `FOR UPDATE` on the account row is what makes it hold under
    /// concurrency: two appends that would each stay within the limit but
    /// together breach it must not both read the pre-image and both commit.
    /// Serialising them per constrained account is the narrowest lock that
    /// makes the invariant true, and it costs nothing for the unconstrained
    /// accounts that are the overwhelming majority.
    ///
    /// Deliberately not filtered on `log_index IS NOT NULL`: an unsequenced
    /// entry is durable, and money it has already committed counts against the
    /// limit whether or not the sequencer has placed it yet.
    async fn check_limits(
        tx: &mut Transaction<'_, Postgres>,
        entry: &Entry<Balanced, P>,
    ) -> Result<(), PostgresError> {
        let mut checked: BTreeSet<(u32, Currency)> = BTreeSet::new();
        for posting in entry.postings() {
            if !checked.insert((posting.account.index(), posting.currency)) {
                continue;
            }
            let index = i32::try_from(posting.account.index()).unwrap_or(i32::MAX);
            // `FOR UPDATE` on the account row, so two concurrent draws serialise
            // on it: each would otherwise read a pre-image the other's write has
            // not landed in, fit under the limit separately, and together breach
            // it.
            let locked: Option<String> = sqlx::query_scalar(
                "SELECT balance_limit FROM accounts \
                 WHERE account_index = $1 AND balance_limit <> 'unlimited' FOR UPDATE",
            )
            .bind(index)
            .fetch_optional(&mut **tx)
            .await?;
            let Some(code) = locked else { continue };
            let limit = limit_from_code(&code)
                .ok_or_else(|| PostgresError::malformed(format!("balance limit {code:?}")))?;

            // Both layers, as four gross totals: the fold is asymmetric, so a
            // signed net per layer cannot express it. See
            // `BalanceLimit::headroom_minor`.
            let row = sqlx::query(
                "SELECT \
                    COALESCE(SUM(CASE WHEN layer = 'settled' AND direction = 'D' \
                                      THEN amount_minor ELSE 0 END), 0)::BIGINT AS sd, \
                    COALESCE(SUM(CASE WHEN layer = 'settled' AND direction = 'C' \
                                      THEN amount_minor ELSE 0 END), 0)::BIGINT AS sc, \
                    COALESCE(SUM(CASE WHEN layer = 'pending' AND direction = 'D' \
                                      THEN amount_minor ELSE 0 END), 0)::BIGINT AS pd, \
                    COALESCE(SUM(CASE WHEN layer = 'pending' AND direction = 'C' \
                                      THEN amount_minor ELSE 0 END), 0)::BIGINT AS pc \
                 FROM postings WHERE account_index = $1 AND currency = $2",
            )
            .bind(index)
            .bind(posting.currency.code())
            .fetch_one(&mut **tx)
            .await?;

            let settled = Balance::<P> {
                debits: Amount::from_minor(row.try_get::<i64, _>("sd")?),
                credits: Amount::from_minor(row.try_get::<i64, _>("sc")?),
            };
            let pending = Balance::<P> {
                debits: Amount::from_minor(row.try_get::<i64, _>("pd")?),
                credits: Amount::from_minor(row.try_get::<i64, _>("pc")?),
            };
            // The same function the in-memory journal calls, so the two cannot
            // drift on a rule with this much subtlety in it.
            if let Some(headroom) = limit.headroom_minor(&settled, &pending)
                && headroom < 0
            {
                return Err(PostgresError::LimitBreached {
                    account: posting.account,
                    currency: posting.currency,
                    limit,
                    headroom_minor: headroom,
                });
            }
        }
        Ok(())
    }

    /// The registered ancestor of `path` that already carries postings, if any.
    ///
    /// One query over the ancestor paths, which is at most
    /// [`MAX_DEPTH`](crate::account::MAX_DEPTH) values and only ever run when
    /// an account is registered.
    async fn posted_to_ancestor(
        pool: &PgPool,
        path: &AccountPath,
    ) -> Result<Option<String>, PostgresError> {
        let ancestors: Vec<String> = path.ancestors().iter().map(ToString::to_string).collect();
        if ancestors.is_empty() {
            return Ok(None);
        }
        sqlx::query(
            "SELECT a.path AS path FROM accounts a \
             WHERE a.path = ANY($1) \
               AND EXISTS (SELECT 1 FROM postings p WHERE p.account_index = a.account_index) \
             ORDER BY a.account_index LIMIT 1",
        )
        .bind(&ancestors)
        .fetch_optional(pool)
        .await?
        .map(|row| row.try_get::<String, _>("path"))
        .transpose()
        .map_err(PostgresError::from)
    }

    /// Enforces the correction rules the schema cannot express.
    ///
    /// The unique index on `reverses` covers at-most-once. Whether the target is
    /// itself a reversal, and whether the postings actually invert it, are
    /// relational facts a constraint cannot see — and skipping them would let an
    /// entry mark an original as corrected while the amounts never netted.
    async fn check_reversal(
        tx: &mut Transaction<'_, Postgres>,
        entry: &Entry<Balanced, P>,
        original: EntryId,
    ) -> Result<(), PostgresError> {
        let Some(row) = sqlx::query(
            "SELECT reverses, \
             (SELECT entry_id FROM entries r WHERE r.reverses = e.entry_id) AS reversed_by \
             FROM entries e WHERE e.entry_id = $1",
        )
        .bind(original.as_uuid())
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Err(PostgresError::UnknownOriginal { id: original });
        };

        if row.try_get::<Option<uuid::Uuid>, _>("reverses")?.is_some() {
            return Err(PostgresError::ReversalOfReversal { id: original });
        }
        if row
            .try_get::<Option<uuid::Uuid>, _>("reversed_by")?
            .is_some_and(|by| by != *entry.id().as_uuid())
        {
            return Err(PostgresError::AlreadyReversed { id: original });
        }

        let target = load_postings_tx::<P>(tx, *original.as_uuid()).await?;
        let candidate = entry.postings();
        let inverts = target.len() == candidate.len()
            && target.iter().zip(candidate.iter()).all(|(o, r)| {
                r.account == o.account
                    && r.amount == o.amount
                    && r.currency == o.currency
                    && r.layer == o.layer
                    && r.dimensions == o.dimensions
                    && r.direction == o.direction.inverse()
            });
        if !inverts {
            return Err(PostgresError::NotAnInversion { id: original });
        }
        Ok(())
    }

    async fn fold_balance(
        &self,
        key: &BalanceKey,
        query: BalanceQuery<'_>,
    ) -> Result<Balance<P>, PostgresError> {
        // Placeholders are handed out as the text is written, so the binds can
        // only be in the order the database reads them.
        let mut sql = Sql::default();
        let joins = dimension_joins(&mut sql, &query);
        let account = sql.int(i64::from(key.account.index()));
        let currency = sql.text(key.currency.code());
        let layer = sql.text(layer_str(key.layer));
        let predicate = entry_predicate(&mut sql, &query);
        let text = format!(
            "SELECT \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id{joins} \
             WHERE p.account_index = {account} AND p.currency = {currency} \
               AND p.layer = {layer}{predicate}"
        );
        let row = sql.apply(sqlx::query(&text)).fetch_one(&self.pool).await?;

        Ok(Balance {
            debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
            credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
        })
    }
}

/// One bind value for a generated query, in placeholder order.
enum Bind {
    Int(i64),
    Text(String),
    Date(Date),
    /// A set of account handles, bound as an array for `= ANY(…)`.
    Accounts(Vec<i32>),
}

/// A query built up alongside the bind values its placeholders refer to.
///
/// Placeholders are handed out as the SQL is written, so the values can only be
/// in the order the database will read them. Building the text and the binds
/// separately is how a filter clause ends up matched against a date.
#[derive(Default)]
struct Sql {
    binds: Vec<Bind>,
}

impl Sql {
    /// Records a bind and returns the numbered placeholder that refers to it.
    fn bind(&mut self, value: Bind) -> String {
        self.binds.push(value);
        format!("${}", self.binds.len())
    }

    fn int(&mut self, v: i64) -> String {
        self.bind(Bind::Int(v))
    }

    fn text(&mut self, v: &str) -> String {
        self.bind(Bind::Text(v.to_owned()))
    }

    fn date(&mut self, v: Date) -> String {
        self.bind(Bind::Date(v))
    }

    /// Applies every recorded bind, in order.
    fn apply<'q>(
        self,
        query: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
        let mut query = query;
        for bind in self.binds {
            query = match bind {
                Bind::Int(v) => query.bind(v),
                Bind::Text(v) => query.bind(v),
                Bind::Date(v) => query.bind(v),
                Bind::Accounts(v) => query.bind(v),
            };
        }
        query
    }
}

/// `JOIN` clauses for the dimension equalities a query requires.
///
/// An equality is an inner join rather than a correlated `EXISTS` so the planner
/// can drive from `posting_dimensions (axis, value)` — the index that exists for
/// exactly this, and that nothing used before dimensional reporting did. The
/// join cannot duplicate a posting: `(entry_id, posting_index, axis)` is the
/// primary key, so it matches at most one row.
///
/// Absences stay in the predicate, because there is no row to join to.
fn dimension_joins(sql: &mut Sql, query: &BalanceQuery<'_>) -> String {
    let mut out = String::new();
    let Some(filter) = query.dimensions() else {
        return out;
    };
    for (i, (axis, value)) in filter.clauses().enumerate() {
        let Some(value) = value else { continue };
        let axis = sql.text(axis.as_str());
        let value = sql.text(value.as_str());
        out.push_str(&format!(
            " JOIN posting_dimensions d{i} ON d{i}.entry_id = p.entry_id \
              AND d{i}.posting_index = p.posting_index \
              AND d{i}.axis = {axis} AND d{i}.value = {value}"
        ));
    }
    out
}

/// The `AND …` predicate a query adds to a `WHERE` clause.
///
/// Always starts by excluding unsequenced entries: an entry with no log position
/// is durable but not in the log, so it is not part of any balance the log can
/// be proven against.
fn entry_predicate(sql: &mut Sql, query: &BalanceQuery<'_>) -> String {
    let mut out = String::from(" AND e.log_index IS NOT NULL");
    if let Some(size) = query.prefix() {
        let bound = sql.int(prefix_bound(Some(size)));
        out.push_str(&format!(" AND e.log_index < {bound}"));
    }
    // Whichever date the query says it means. The column is chosen from a
    // two-variant enum, never interpolated from caller text.
    let column = match query.basis() {
        DateBasis::Booking => "e.booking_date",
        DateBasis::Value => "e.value_date",
    };
    if let Some(start) = query.start() {
        let bound = sql.date(start);
        out.push_str(&format!(" AND {column} >= {bound}"));
    }
    if let Some(end) = query.end() {
        let bound = sql.date(end);
        out.push_str(&format!(" AND {column} <= {bound}"));
    }
    if let Some(filter) = query.dimensions() {
        for (axis, value) in filter.clauses() {
            if value.is_some() {
                continue;
            }
            let axis = sql.text(axis.as_str());
            out.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM posting_dimensions dx \
                  WHERE dx.entry_id = p.entry_id AND dx.posting_index = p.posting_index \
                    AND dx.axis = {axis})"
            ));
        }
    }
    out
}

/// Turns grouped `(account, currency, layer, debits, credits)` rows into a trial
/// balance.
fn build_trial_balance<const P: u8>(
    rows: &[sqlx::postgres::PgRow],
) -> Result<TrialBalance<P>, PostgresError> {
    let mut tb = TrialBalance::new();
    for row in rows {
        let account: i32 = row.try_get("account_index")?;
        let currency: String = row.try_get("currency")?;
        let layer: String = row.try_get("layer")?;
        tb.set(
            BalanceKey {
                account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
                currency: Currency::new(currency.trim())
                    .map_err(|_| PostgresError::malformed(format!("currency {currency:?}")))?,
                layer: match layer.as_str() {
                    "settled" => Layer::Settled,
                    "pending" => Layer::Pending,
                    other => return Err(PostgresError::malformed(format!("layer {other:?}"))),
                },
            },
            Balance {
                debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
                credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
            },
        );
    }
    Ok(tb)
}

/// Columns selected whenever a whole entry is loaded.
const ENTRY_COLUMNS: &str = "entry_id, log_index, idempotency_key, content_hash, booking_date, \
     value_date, description, provenance_actor, provenance_source, provenance_correlation, \
     document_id, document_content_hash, reverses, original_booking_date, kind";

/// The exclusive upper bound on `log_index` for a prefix of `size` entries.
///
/// `size` counts entries, so the entries in it are indices `0..size` — hence a
/// strict `<`. `None` means the whole log. Expressed as an exclusive bound
/// rather than `size - 1` so that an empty prefix needs no special case: it
/// binds zero, and nothing is strictly below zero.
fn prefix_bound(size: Option<u64>) -> i64 {
    size.map_or(i64::MAX, |n| i64::try_from(n).unwrap_or(i64::MAX))
}

fn direction_str(direction: Direction) -> &'static str {
    match direction {
        Direction::Debit => "D",
        Direction::Credit => "C",
    }
}

fn layer_str(layer: Layer) -> &'static str {
    match layer {
        Layer::Settled => "settled",
        Layer::Pending => "pending",
    }
}

fn period_state_str(state: PeriodState) -> &'static str {
    match state {
        PeriodState::Open => "open",
        PeriodState::Closing => "closing",
        PeriodState::Sealed => "sealed",
    }
}

fn period_state_from(s: &str) -> Option<PeriodState> {
    match s {
        "open" => Some(PeriodState::Open),
        "closing" => Some(PeriodState::Closing),
        "sealed" => Some(PeriodState::Sealed),
        _ => None,
    }
}

fn hash_from_bytes(bytes: &[u8]) -> Result<Hash, PostgresError> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| PostgresError::malformed("hash is not 32 bytes"))?;
    Ok(Hash::from_bytes(array))
}

fn build_posting<const P: u8>(row: &sqlx::postgres::PgRow) -> Result<Posting<P>, PostgresError> {
    let account: i32 = row.try_get("account_index")?;
    let direction: String = row.try_get("direction")?;
    let amount: i64 = row.try_get("amount_minor")?;
    let currency: String = row.try_get("currency")?;
    let layer: String = row.try_get("layer")?;

    let direction = match direction.as_str() {
        "D" => Direction::Debit,
        "C" => Direction::Credit,
        other => return Err(PostgresError::malformed(format!("direction {other:?}"))),
    };
    let layer = match layer.as_str() {
        "settled" => Layer::Settled,
        "pending" => Layer::Pending,
        other => return Err(PostgresError::malformed(format!("layer {other:?}"))),
    };
    let currency = Currency::new(currency.trim())
        .map_err(|_| PostgresError::malformed(format!("currency {currency:?}")))?;

    Ok(Posting {
        account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
        direction,
        amount: Amount::from_minor(amount),
        currency,
        layer,
        dimensions: Dimensions::none(),
    })
}

/// `(entry_id, posting_index)` to the axes attached to that posting.
type DimensionIndex = std::collections::BTreeMap<(uuid::Uuid, i16), Dimensions>;

fn dimensions_from(rows: &[sqlx::postgres::PgRow]) -> Result<DimensionIndex, PostgresError> {
    let mut out: DimensionIndex = std::collections::BTreeMap::new();
    for row in rows {
        let entry_id: uuid::Uuid = row.try_get("entry_id")?;
        let posting_index: i16 = row.try_get("posting_index")?;
        let axis: String = row.try_get("axis")?;
        let value: String = row.try_get("value")?;
        out.entry((entry_id, posting_index))
            .or_default()
            .set(
                Label::new(axis).map_err(|e| PostgresError::malformed(e.to_string()))?,
                Label::new(value).map_err(|e| PostgresError::malformed(e.to_string()))?,
            )
            .map_err(|e| PostgresError::malformed(e.to_string()))?;
    }
    Ok(out)
}

fn build_stored_entry<const P: u8>(
    row: &sqlx::postgres::PgRow,
    postings: Vec<Posting<P>>,
) -> Result<StoredEntry<P>, PostgresError> {
    let log_index: Option<i64> = row.try_get("log_index")?;
    let entry_id: uuid::Uuid = row.try_get("entry_id")?;
    let key: Vec<u8> = row.try_get("idempotency_key")?;
    let stored_hash = hash_from_bytes(&row.try_get::<Vec<u8>, _>("content_hash")?)?;
    let booking_date: Date = row.try_get("booking_date")?;
    let value_date: Date = row.try_get("value_date")?;
    let description: String = row.try_get("description")?;

    let mut provenance = Provenance::none();
    if let Some(v) = row.try_get::<Option<String>, _>("provenance_actor")? {
        provenance = provenance
            .with_actor(&v)
            .map_err(|e| PostgresError::malformed(e.to_string()))?;
    }
    if let Some(v) = row.try_get::<Option<String>, _>("provenance_source")? {
        provenance = provenance
            .with_source(&v)
            .map_err(|e| PostgresError::malformed(e.to_string()))?;
    }
    if let Some(v) = row.try_get::<Option<String>, _>("provenance_correlation")? {
        provenance = provenance
            .with_correlation(&v)
            .map_err(|e| PostgresError::malformed(e.to_string()))?;
    }

    let mut draft = Entry::<Draft, P>::new(
        EntryId::from_uuid(entry_id),
        IdempotencyKey::new(key).map_err(|e| PostgresError::malformed(e.to_string()))?,
        booking_date,
    )
    .with_value_date(value_date)
    .with_description(
        Description::new(description).map_err(|e| PostgresError::malformed(e.to_string()))?,
    )
    .with_provenance(provenance);

    if let Some(kind) = row.try_get::<Option<String>, _>("kind")? {
        draft =
            draft.with_kind(Label::new(kind).map_err(|e| PostgresError::malformed(e.to_string()))?);
    }

    // The hash is independently optional: an entry may name a document without
    // committing to its contents. Requiring both would silently drop the
    // reference on read and change the entry's content hash.
    if let Some(id) = row.try_get::<Option<String>, _>("document_id")? {
        let stored = row.try_get::<Option<Vec<u8>>, _>("document_content_hash")?;
        let document = match stored {
            Some(hash) => DocumentRef::new(&id, hash_from_bytes(&hash)?),
            None => DocumentRef::unverified(&id),
        };
        draft = draft.with_document(document.map_err(|e| PostgresError::malformed(e.to_string()))?);
    }
    if let (Some(reverses), Some(original)) = (
        row.try_get::<Option<uuid::Uuid>, _>("reverses")?,
        row.try_get::<Option<Date>, _>("original_booking_date")?,
    ) {
        draft = draft.reversing(EntryId::from_uuid(reverses), original);
    }
    for posting in postings {
        draft = draft.post(posting);
    }

    // Verified, not re-validated: the hash proves these are the exact bytes that
    // passed validation when they were written.
    let entry = draft.adopt_verified(stored_hash)?;
    Ok(StoredEntry {
        index: log_index.map(|i| LogIndex::new(u64::try_from(i).unwrap_or(0))),
        entry,
        content_hash: stored_hash,
    })
}

/// Applies the schema. Kept as a trait so the query text stays next to it.
trait ExecuteSchema {
    fn execute_schema(&self) -> impl Future<Output = Result<(), PostgresError>> + Send;
}

impl ExecuteSchema for PgPool {
    async fn execute_schema(&self) -> Result<(), PostgresError> {
        // `btree_gist` backs the non-overlapping period constraint.
        sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS btree_gist")
            .execute(self)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(self).await?;
        Ok(())
    }
}

impl<const P: u8> LedgerStore<P> for PostgresStore<P> {
    type Error = PostgresError;

    fn ledger(&self) -> &LedgerId {
        &self.ledger
    }

    async fn register_account(&self, record: &AccountRecord) -> Result<(), Self::Error> {
        {
            // The leaf rule, checked where the postings are. A registry holds
            // none, so it cannot see that this path would turn an account that
            // has already been posted to into an aggregation node — and the
            // journal enforces the same rule for the same reason.
            //
            // Only a path the store has not seen can break it: a record it
            // already holds cannot change the shape of the tree.
            if let Some(ancestor) =
                Self::posted_to_ancestor(&self.pool, &record.account.path).await?
            {
                return Err(PostgresError::AncestorHasPostings {
                    path: record.account.path.to_string(),
                    ancestor,
                });
            }

            // Upsert, not insert-or-ignore. The path at a handle is immutable
            // — changing it would repoint every posting row that names it, and
            // the WHERE clause refuses that outright — but the classification,
            // the open window and the balance limit are master data. A store
            // that only ever inserted could not close an account or tighten a
            // limit, which would leave `AccountRegistry`'s own mutators with
            // nowhere to go once a ledger became durable.
            let updated = sqlx::query(
                "INSERT INTO accounts \
                    (account_index, path, kind, opened_on, closed_on, balance_limit) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (account_index) DO UPDATE SET \
                    kind          = EXCLUDED.kind, \
                    opened_on     = EXCLUDED.opened_on, \
                    closed_on     = EXCLUDED.closed_on, \
                    balance_limit = EXCLUDED.balance_limit \
                 WHERE accounts.path = EXCLUDED.path",
            )
            .bind(i32::try_from(record.id.index()).unwrap_or(i32::MAX))
            .bind(record.account.path.to_string())
            .bind(record.account.kind.map(kind_code))
            .bind(record.account.opened_on)
            .bind(record.account.closed_on)
            .bind(limit_code(record.account.limit))
            .execute(&self.pool)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(PostgresError::AccountRebound { id: record.id });
            }
            Ok(())
        }
    }

    async fn accounts(&self) -> Result<Vec<AccountRecord>, Self::Error> {
        {
            let rows = sqlx::query(
                "SELECT account_index, path, kind, opened_on, closed_on, balance_limit \
                 FROM accounts ORDER BY account_index",
            )
            .fetch_all(&self.pool)
            .await?;
            rows.iter().map(account_record).collect()
        }
    }

    async fn append(&self, batch: &EntryBatch<P>) -> Result<Vec<Recorded>, Self::Error> {
        let mut tx = self.pool.begin().await?;

        let placement = match self.sequencing {
            Sequencing::Inline => {
                // Serialise appends so positions stay dense and follow commit
                // order. Held for the transaction, released on commit or abort.
                sqlx::query("SELECT pg_advisory_xact_lock($1)")
                    .bind(APPEND_LOCK)
                    .execute(&mut *tx)
                    .await?;
                Placement::Inline
            }
            // No lock: writers do not contend, and ordering is the sequencer's
            // problem rather than theirs.
            Sequencing::Deferred => Placement::Deferred,
        };

        let mut next_index = 0i64;
        let mut accumulator = MerkleAccumulator::new();
        if placement == Placement::Inline {
            // From the nodes, not from `MAX(log_index)`: the nodes are the log,
            // and an archived prefix pruned from `entries` must not renumber
            // what comes next.
            accumulator = Self::accumulator(&mut tx).await?;
            next_index = i64::try_from(accumulator.size()).unwrap_or(i64::MAX);
        }

        let mut out = Vec::with_capacity(batch.len());
        for entry in batch.entries() {
            out.push(
                Self::append_one(&mut tx, entry, &mut next_index, &mut accumulator, placement)
                    .await?,
            );
        }

        tx.commit().await?;
        Ok(out)
    }

    async fn sequence(&self) -> Result<u64, Self::Error> {
        if self.sequencing == Sequencing::Inline {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        // One sequencing pass at a time: the positions it assigns must be dense,
        // so two passes must not both believe they start at the same index.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(APPEND_LOCK)
            .execute(&mut *tx)
            .await?;

        // Only rows whose inserting transaction has *finished*. A row still in
        // flight is left for the next pass rather than skipped — skipping it
        // would place it behind the reader once it committed, and it would never
        // be picked up again.
        let rows = sqlx::query(
            "SELECT entry_id, content_hash FROM entries \
             WHERE log_index IS NULL \
               AND insert_xid < pg_snapshot_xmin(pg_current_snapshot()) \
             ORDER BY insert_xid, entry_id",
        )
        .fetch_all(&mut *tx)
        .await?;

        if rows.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        // Both the next position and the subtree cover come from the node
        // store, so a sequencing pass cannot place an entry at a position the
        // tree disagrees about.
        let mut accumulator = Self::accumulator(&mut tx).await?;
        let mut index = i64::try_from(accumulator.size()).unwrap_or(i64::MAX);

        let mut sequenced = 0u64;
        for row in &rows {
            let entry_id: uuid::Uuid = row.try_get("entry_id")?;
            let content_hash = hash_from_bytes(&row.try_get::<Vec<u8>, _>("content_hash")?)?;
            let first_node = nodes::count(accumulator.size());
            let (_, written) = accumulator.push_recording(content_hash);
            Self::store_nodes(&mut tx, first_node, &written).await?;

            sqlx::query("UPDATE entries SET log_index = $2 WHERE entry_id = $1")
                .bind(entry_id)
                .bind(index)
                .execute(&mut *tx)
                .await?;

            index = index.saturating_add(1);
            sequenced = sequenced.saturating_add(1);
        }

        tx.commit().await?;
        Ok(sequenced)
    }

    async fn get(&self, id: EntryId) -> Result<Option<StoredEntry<P>>, Self::Error> {
        let sql = format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE entry_id = $1");
        let Some(row) = sqlx::query(&sql)
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let mut grouped = self.load_postings_for(&[*id.as_uuid()]).await?;
        Ok(Some(build_stored_entry::<P>(
            &row,
            grouped.remove(id.as_uuid()).unwrap_or_default(),
        )?))
    }

    async fn get_by_key(
        &self,
        key: &IdempotencyKey,
    ) -> Result<Option<StoredEntry<P>>, Self::Error> {
        // The same unique index that makes the append idempotent.
        let sql = format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE idempotency_key = $1");
        let Some(row) = sqlx::query(&sql)
            .bind(key.as_bytes())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let id: uuid::Uuid = row.try_get("entry_id")?;
        let mut grouped = self.load_postings_for(&[id]).await?;
        Ok(Some(build_stored_entry::<P>(
            &row,
            grouped.remove(&id).unwrap_or_default(),
        )?))
    }

    async fn page(&self, cursor: Cursor) -> Result<Page<P>, Self::Error> {
        let after = cursor
            .after
            .map_or(-1i64, |i| i64::try_from(i.get()).unwrap_or(i64::MAX));
        let limit = i64::try_from(cursor.effective_limit()).unwrap_or(i64::MAX);

        let rows = sqlx::query(&format!(
            "SELECT {ENTRY_COLUMNS} FROM entries \
             WHERE log_index IS NOT NULL AND log_index > $1 ORDER BY log_index LIMIT $2"
        ))
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        // Fetch every posting for the page in one query rather than one per
        // record; a page of 256 entries would otherwise be 257 round trips.
        let ids: Vec<uuid::Uuid> = rows
            .iter()
            .map(|r| r.try_get::<uuid::Uuid, _>("entry_id"))
            .collect::<Result<_, _>>()?;
        let mut grouped = self.load_postings_for(&ids).await?;

        let mut records = Vec::with_capacity(rows.len());
        for (row, id) in rows.iter().zip(ids.iter()) {
            let postings = grouped.remove(id).unwrap_or_default();
            records.push(build_stored_entry::<P>(row, postings)?);
        }

        let total = self.len().await?;
        let next = records
            .last()
            .and_then(|r| r.index)
            .filter(|index| index.get().saturating_add(1) < total)
            .map(|index| Cursor {
                after: Some(index),
                limit: cursor.limit,
            });
        Ok(Page { records, next })
    }

    async fn head(&self) -> Result<TreeHead, Self::Error> {
        let size = Self::log_size(&self.pool).await?;
        Self::head_from_nodes(&self.pool, size).await
    }

    /// `O(log n)` node reads — the perfect-subtree cover at that size.
    ///
    /// There is no stored root to look up, and deliberately so: a root column
    /// beside the nodes would be a second copy of a derived value, and two
    /// fields that must agree are two fields that can disagree. Folding the
    /// cover is one query and cannot drift.
    async fn head_at(&self, size: u64) -> Result<TreeHead, Self::Error> {
        let held = Self::log_size(&self.pool).await?;
        if size > held {
            // The log's real length, not a placeholder: this error is read by
            // whoever was told their archived head could not be reproduced, and
            // a wrong size sends them looking at their own records rather than
            // at the prefix this store is missing.
            return Err(ProofError::SizeOutOfRange {
                from: size,
                size: held,
            }
            .into());
        }
        Self::head_from_nodes(&self.pool, size).await
    }

    /// Counts the log, not the surviving entry rows.
    ///
    /// The nodes are the log. An entry archived to a cold tier and pruned from
    /// hot storage leaves the tree exactly as it was, so a length counted from
    /// `entries` would shrink underneath every head, proof and seal that had
    /// already been published.
    async fn len(&self) -> Result<u64, Self::Error> {
        Self::log_size(&self.pool).await
    }

    async fn balance(
        &self,
        key: BalanceKey,
        query: BalanceQuery<'_>,
    ) -> Result<Balance<P>, Self::Error> {
        self.fold_balance(&key, query).await
    }

    async fn trial_balance(&self, query: BalanceQuery<'_>) -> Result<TrialBalance<P>, Self::Error> {
        let mut sql = Sql::default();
        let joins = dimension_joins(&mut sql, &query);
        let predicate = entry_predicate(&mut sql, &query);
        let text = format!(
            "SELECT p.account_index, p.currency, p.layer, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id{joins} \
             WHERE TRUE{predicate} \
             GROUP BY p.account_index, p.currency, p.layer \
             ORDER BY p.account_index, p.currency, p.layer"
        );
        let rows = sql.apply(sqlx::query(&text)).fetch_all(&self.pool).await?;
        build_trial_balance::<P>(&rows)
    }

    async fn dimension_values(&self, axis: &str) -> Result<Vec<Label>, Self::Error> {
        // Index-driven on `posting_dimensions (axis, value)`.
        let rows = sqlx::query(
            "SELECT DISTINCT value FROM posting_dimensions WHERE axis = $1 ORDER BY value",
        )
        .bind(axis)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let value: String = row.try_get("value")?;
                Label::new(value.clone())
                    .map_err(|_| PostgresError::malformed(format!("dimension value {value:?}")))
            })
            .collect()
    }

    async fn prove_inclusion(&self, index: LogIndex) -> Result<InclusionProof, Self::Error> {
        let size = Self::log_size(&self.pool).await?;
        self.prove_inclusion_at(index, size).await
    }

    /// `O(log n)` node reads, not a replay of the log.
    ///
    /// The plan names at most `1 + ceil(log2 n)` positions, one `= ANY($1)`
    /// fetches them, and the fold is pure. A hundred-million-entry ledger
    /// answers this from 28 rows.
    async fn prove_inclusion_at(
        &self,
        index: LogIndex,
        size: u64,
    ) -> Result<InclusionProof, Self::Error> {
        let held = Self::log_size(&self.pool).await?;
        if size > held {
            return Err(ProofError::SizeOutOfRange {
                from: size,
                size: held,
            }
            .into());
        }
        let plan = nodes::InclusionPlan::new(index.get(), size)?;
        let read = Self::read_nodes(&self.pool, plan.nodes()).await?;
        Ok(plan.assemble(&read)?)
    }

    async fn prove_consistency(&self, old_size: u64) -> Result<ConsistencyProof, Self::Error> {
        let size = Self::log_size(&self.pool).await?;
        self.prove_consistency_between(old_size, size).await
    }

    async fn prove_consistency_between(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, Self::Error> {
        let held = Self::log_size(&self.pool).await?;
        if new_size > held {
            return Err(ProofError::SizeOutOfRange {
                from: new_size,
                size: held,
            }
            .into());
        }
        let plan = nodes::ConsistencyPlan::new(old_size, new_size)?;
        let read = Self::read_nodes(&self.pool, plan.nodes()).await?;
        Ok(plan.assemble(&read)?)
    }

    async fn define_period(&self, period: &Period) -> Result<(), Self::Error> {
        // The EXCLUDE constraint enforces non-overlap; this insert only has to
        // be idempotent for a caller that declares its calendar on every start.
        sqlx::query(
            "INSERT INTO periods (period_id, starts_on, ends_on, state) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (period_id) DO UPDATE SET \
                starts_on = EXCLUDED.starts_on, \
                ends_on   = EXCLUDED.ends_on, \
                state     = EXCLUDED.state",
        )
        .bind(period.id.as_str())
        .bind(period.start)
        .bind(period.end)
        .bind(period_state_str(period.state))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn transition_period(
        &self,
        period: &PeriodId,
        to: PeriodState,
    ) -> Result<(), Self::Error> {
        // Checked against the calendar's rules rather than written blindly: the
        // database can say a state is one of three, not that this one follows
        // from the last.
        let mut calendar = self.calendar().await?;
        calendar.transition(period, to)?;
        sqlx::query("UPDATE periods SET state = $2 WHERE period_id = $1")
            .bind(period.as_str())
            .bind(period_state_str(to))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn periods(&self) -> Result<Vec<Period>, Self::Error> {
        let rows = sqlx::query(
            "SELECT period_id, starts_on, ends_on, state FROM periods ORDER BY starts_on",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: String = row.try_get("period_id")?;
            let starts_on: Date = row.try_get("starts_on")?;
            let ends_on: Date = row.try_get("ends_on")?;
            let state: String = row.try_get("state")?;
            let id = PeriodId::new(id).map_err(|e| PostgresError::malformed(e.to_string()))?;
            let mut period = Period::new(id, starts_on, ends_on)
                .map_err(|e| PostgresError::malformed(e.to_string()))?;
            period.state = period_state_from(&state).ok_or_else(|| {
                PostgresError::malformed(format!("unknown period state {state:?}"))
            })?;
            out.push(period);
        }
        Ok(out)
    }

    /// Folds every *sequenced* entry booked on or before `end`.
    ///
    /// The `log_index IS NOT NULL` predicate is what keeps a seal honest: the
    /// tree head it carries covers only sequenced entries, so a closing balance
    /// that folded in unsequenced ones would commit to money the tree head does
    /// not account for. In deferred mode that window is real.
    async fn seal_period(&self, period: &PeriodId) -> Result<Seal, Self::Error> {
        // The same rule the in-memory journal applies, from the same place: the
        // period is defined, closing, and next in date order. Sealing out of
        // order would let a later booking into an earlier open period restate a
        // closing balance this seal is about to commit to.
        let definition = self.calendar().await?.check_sealable(period)?.clone();

        // Which entries belong to the period, and the closing balance through
        // its last day — not the whole journal, which would pull in entries
        // booked into later periods.
        //
        // Sequenced entries only. In deferred mode an entry can be durable
        // without a position, and one that is not in the log the tree head
        // commits to must not be counted as covered by it.
        let span = sqlx::query(
            "SELECT MIN(log_index) AS first, MAX(log_index) AS last, COUNT(*)::BIGINT AS n \
             FROM entries \
             WHERE log_index IS NOT NULL AND booking_date BETWEEN $1 AND $2",
        )
        .bind(definition.start)
        .bind(definition.end)
        .fetch_one(&self.pool)
        .await?;
        let first: Option<i64> = span.try_get("first")?;
        let last: Option<i64> = span.try_get("last")?;
        let count: i64 = span.try_get("n")?;

        let closing = self
            .trial_balance(BalanceQuery::through(definition.end))
            .await?;
        // Rebuilt through the chain rather than counted: seals read back from a
        // table are rows, not evidence, and the new one has to chain onto a
        // predecessor that itself still holds.
        let chain = SealChain::from_seals(self.ledger.clone(), self.seals().await?)
            .map_err(|e| PostgresError::malformed(e.to_string()))?;
        let position = i64::try_from(chain.len()).unwrap_or(0);

        // The seal's tree head, and — when it chains onto one — the consistency
        // proof from its predecessor's tree, both read from the stored nodes in
        // `O(log n)`. Derived from one source, so the proof and the head it
        // relates cannot have come from different trees; `Seal::from_parts`
        // re-checks that rather than trusting it.
        let size = Self::log_size(&self.pool).await?;
        let tree_head = Self::head_from_nodes(&self.pool, size).await?;
        let prev_consistency = match chain.last() {
            Some(previous) if previous.tree_head.size > 0 => {
                let plan = nodes::ConsistencyPlan::new(previous.tree_head.size, size)?;
                let read = Self::read_nodes(&self.pool, plan.nodes()).await?;
                Some(plan.assemble(&read)?)
            }
            _ => None,
        };

        let seal = Seal::from_parts(
            self.ledger.clone(),
            period.clone(),
            PeriodCoverage {
                first_index: first.map(|v| u64::try_from(v).unwrap_or(0)),
                last_index: last.map(|v| u64::try_from(v).unwrap_or(0)),
                entry_count: u64::try_from(count).unwrap_or(0),
            },
            tree_head,
            prev_consistency,
            &closing,
            // Built from the stored bindings, not from a caller-supplied
            // registry: the seal must commit to the handles this database
            // actually resolved the balances against.
            AccountRegistry::from_records(LedgerStore::<P>::accounts(self).await?)
                .map_err(|e: crate::account::AccountError| PostgresError::malformed(e.to_string()))?
                .commitment(),
            chain.last(),
        )?;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO seals ( \
                period_id, first_index, last_index, entry_count, tree_size, tree_root, \
                trial_balance_size, trial_balance_root, accounts_size, accounts_root, \
                prev_seal, prev_consistency, seal_hash, chain_position \
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
        )
        .bind(seal.period.as_str())
        .bind(seal.first_index.map(|v| i64::try_from(v).unwrap_or(0)))
        .bind(seal.last_index.map(|v| i64::try_from(v).unwrap_or(0)))
        .bind(i64::try_from(seal.entry_count).unwrap_or(0))
        .bind(i64::try_from(seal.tree_head.size).unwrap_or(0))
        .bind(seal.tree_head.root.as_bytes().as_slice())
        .bind(i64::try_from(seal.trial_balance.size).unwrap_or(0))
        .bind(seal.trial_balance.root.as_bytes().as_slice())
        .bind(i64::try_from(seal.accounts.size).unwrap_or(0))
        .bind(seal.accounts.root.as_bytes().as_slice())
        .bind(seal.prev_seal.map(|h| h.as_bytes().to_vec()))
        // The crate's own canonical encoding, not a general-purpose format:
        // the seal hash covers these bytes, so their framing has to be fixed
        // for as long as the seal is evidence.
        .bind(
            seal.prev_consistency
                .as_ref()
                .map(crate::Canonical::to_canonical_bytes),
        )
        .bind(seal.seal_hash.as_bytes().as_slice())
        .bind(position)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE periods SET state = 'sealed' WHERE period_id = $1")
            .bind(seal.period.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(seal)
    }

    async fn seals(&self) -> Result<Vec<Seal>, Self::Error> {
        let rows = sqlx::query(
            "SELECT period_id, first_index, last_index, entry_count, tree_size, tree_root, \
             trial_balance_size, trial_balance_root, accounts_size, accounts_root, \
             prev_seal, prev_consistency, seal_hash \
             FROM seals ORDER BY chain_position",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let period: String = row.try_get("period_id")?;
            let first: Option<i64> = row.try_get("first_index")?;
            let last: Option<i64> = row.try_get("last_index")?;
            let count: i64 = row.try_get("entry_count")?;
            let size: i64 = row.try_get("tree_size")?;
            let tb_size: i64 = row.try_get("trial_balance_size")?;
            let accounts_size: i64 = row.try_get("accounts_size")?;
            let prev: Option<Vec<u8>> = row.try_get("prev_seal")?;
            let consistency: Option<Vec<u8>> = row.try_get("prev_consistency")?;
            let prev_consistency = consistency
                .as_deref()
                .map(crate::ConsistencyProof::from_canonical_bytes)
                .transpose()
                .map_err(|e: crate::MalformedProof| PostgresError::malformed(e.to_string()))?;
            out.push(Seal {
                ledger: self.ledger.clone(),
                period: PeriodId::new(period)
                    .map_err(|e| PostgresError::malformed(e.to_string()))?,
                first_index: first.map(|v| u64::try_from(v).unwrap_or(0)),
                last_index: last.map(|v| u64::try_from(v).unwrap_or(0)),
                entry_count: u64::try_from(count).unwrap_or(0),
                tree_head: TreeHead {
                    size: u64::try_from(size).unwrap_or(0),
                    root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("tree_root")?)?,
                },
                trial_balance: TreeHead {
                    size: u64::try_from(tb_size).unwrap_or(0),
                    root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("trial_balance_root")?)?,
                },
                accounts: TreeHead {
                    size: u64::try_from(accounts_size).unwrap_or(0),
                    root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("accounts_root")?)?,
                },
                prev_seal: prev.as_deref().map(hash_from_bytes).transpose()?,
                prev_consistency,
                seal_hash: hash_from_bytes(&row.try_get::<Vec<u8>, _>("seal_hash")?)?,
            });
        }
        Ok(out)
    }

    async fn clear(&self, clearing: Clearing<P>) -> Result<(), Self::Error> {
        // Validate against the residuals the database reports, then record.
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(APPEND_LOCK)
            .execute(&mut *tx)
            .await?;

        if clearing.items.len() < 2 {
            return Err(crate::clearing::ClearingError::TooFewItems {
                count: clearing.items.len(),
            }
            .into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &clearing.items {
            if !seen.insert(item.posting) {
                return Err(crate::clearing::ClearingError::DuplicateItem {
                    posting: item.posting,
                }
                .into());
            }
            if !item.applied.is_positive() {
                return Err(crate::clearing::ClearingError::NonPositiveApplication {
                    posting: item.posting,
                }
                .into());
            }
        }

        // A repeated identifier is a caller mistake, not a database failure, so
        // it is named rather than surfacing as a primary-key violation.
        let taken: Option<i32> = sqlx::query("SELECT 1 AS x FROM clearings WHERE clearing_id = $1")
            .bind(clearing.id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| row.try_get("x"))
            .transpose()?;
        if taken.is_some() {
            return Err(crate::clearing::ClearingError::DuplicateId { id: clearing.id }.into());
        }

        let mut sides = Balance::<P>::ZERO;
        for item in &clearing.items {
            let facts = posting_facts::<P>(&mut tx, item.posting).await?;
            if facts.account != clearing.account {
                return Err(crate::clearing::ClearingError::WrongAccount {
                    posting: item.posting,
                    expected: clearing.account,
                }
                .into());
            }
            if facts.currency != clearing.currency {
                return Err(crate::clearing::ClearingError::WrongCurrency {
                    posting: item.posting,
                    expected: clearing.currency,
                }
                .into());
            }
            if facts.layer != clearing.layer {
                return Err(crate::clearing::ClearingError::WrongLayer {
                    posting: item.posting,
                    expected: clearing.layer,
                }
                .into());
            }
            if item.applied > facts.residual {
                return Err(crate::clearing::ClearingError::OverApplied {
                    posting: item.posting,
                    requested_minor: item.applied.to_minor(),
                    residual_minor: facts.residual.to_minor(),
                    scale: P,
                }
                .into());
            }
            sides.add(facts.direction, item.applied)?;
        }
        if !sides.is_balanced() {
            return Err(crate::clearing::ClearingError::Unbalanced {
                debits_minor: sides.debits.to_minor(),
                credits_minor: sides.credits.to_minor(),
                scale: P,
            }
            .into());
        }

        sqlx::query(
            "INSERT INTO clearings (clearing_id, account_index, currency, layer, cleared_on) \
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(clearing.id.as_uuid())
        .bind(i32::try_from(clearing.account.index()).unwrap_or(i32::MAX))
        .bind(clearing.currency.code())
        .bind(layer_str(clearing.layer))
        .bind(clearing.cleared_on)
        .execute(&mut *tx)
        .await?;

        for item in &clearing.items {
            sqlx::query(
                "INSERT INTO clearing_items (clearing_id, entry_id, posting_index, applied_minor) \
                 VALUES ($1,$2,$3,$4)",
            )
            .bind(clearing.id.as_uuid())
            .bind(item.posting.entry.as_uuid())
            .bind(i16::try_from(item.posting.index).unwrap_or(i16::MAX))
            .bind(item.applied.to_minor())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn reset_clearing(&self, id: ClearingId, on: Date) -> Result<(), Self::Error> {
        let result = sqlx::query(
            "UPDATE clearings SET reset_on = $2 WHERE clearing_id = $1 AND reset_on IS NULL",
        )
        .bind(id.as_uuid())
        .bind(on)
        .execute(&self.pool)
        .await?;

        // An UPDATE that matched nothing is not success: the caller asked to
        // release an assignment that either never existed or was already
        // released, and silently agreeing would hide a double reset.
        if result.rows_affected() == 0 {
            return Err(PostgresError::ClearingNotResettable { id });
        }
        Ok(())
    }

    async fn open_items(
        &self,
        key: BalanceKey,
        cursor: PostingCursor,
    ) -> Result<OpenItemPage<P>, Self::Error> {
        // Same order and same cursor as a statement: open items are the
        // filtered view of the same postings, so they page the same way.
        let (after_index, after_posting) = cursor.after.map_or((-1i64, -1i64), |p| {
            (
                i64::try_from(p.index.get()).unwrap_or(i64::MAX),
                i64::from(p.posting),
            )
        });
        let limit = cursor.effective_limit();
        let probe = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);

        let mut rows = sqlx::query(
            "SELECT e.log_index, o.entry_id, o.posting_index, o.direction, o.original_minor, \
                    o.applied_minor, o.residual_minor \
             FROM open_items o JOIN entries e ON e.entry_id = o.entry_id \
             WHERE o.account_index = $1 AND o.currency = $2 AND o.layer = $3 \
               AND e.log_index IS NOT NULL \
               AND (e.log_index > $4 \
                    OR (e.log_index = $4 AND o.posting_index > $5)) \
             ORDER BY e.log_index, o.posting_index LIMIT $6",
        )
        .bind(i32::try_from(key.account.index()).unwrap_or(i32::MAX))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .bind(after_index)
        .bind(i16::try_from(after_posting).unwrap_or(i16::MAX))
        .bind(probe)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);

        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let log_index: i64 = row.try_get("log_index")?;
            let entry_id: uuid::Uuid = row.try_get("entry_id")?;
            let entry_id = EntryId::from_uuid(entry_id);
            let posting_index = u16::try_from(row.try_get::<i16, _>("posting_index")?).unwrap_or(0);
            let direction: String = row.try_get("direction")?;
            items.push(OpenItem {
                position: PostingPosition::new(
                    LogIndex::new(u64::try_from(log_index).unwrap_or(0)),
                    posting_index,
                ),
                posting: PostingRef::new(entry_id, posting_index),
                direction: match direction.as_str() {
                    "D" => Direction::Debit,
                    _ => Direction::Credit,
                },
                original: Amount::from_minor(row.try_get::<i64, _>("original_minor")?),
                applied: Amount::from_minor(row.try_get::<i64, _>("applied_minor")?),
                residual: Amount::from_minor(row.try_get::<i64, _>("residual_minor")?),
            });
        }
        let next = items.last().filter(|_| has_more).map(|i| PostingCursor {
            after: Some(i.position),
            limit: cursor.limit,
        });
        Ok(OpenItemPage { items, next })
    }

    async fn balances(
        &self,
        accounts: &[AccountId],
        currency: Currency,
        layer: Layer,
        query: BalanceQuery<'_>,
    ) -> Result<std::collections::BTreeMap<AccountId, Balance<P>>, Self::Error> {
        let mut sql = Sql::default();
        let joins = dimension_joins(&mut sql, &query);
        let currency_bind = sql.text(currency.code());
        let layer_bind = sql.text(layer_str(layer));
        // `ANY` over a bound array rather than a generated `IN` list: one
        // prepared statement whatever the caller asks for.
        let accounts_bind = {
            let placeholder = format!("${}", sql.binds.len().saturating_add(1));
            sql.binds.push(Bind::Accounts(
                accounts
                    .iter()
                    .map(|a| i32::try_from(a.index()).unwrap_or(i32::MAX))
                    .collect(),
            ));
            placeholder
        };
        let predicate = entry_predicate(&mut sql, &query);
        let text = format!(
            "SELECT p.account_index, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id{joins} \
             WHERE p.account_index = ANY({accounts_bind}) AND p.currency = {currency_bind} \
               AND p.layer = {layer_bind}{predicate} \
             GROUP BY p.account_index"
        );
        let rows = sql.apply(sqlx::query(&text)).fetch_all(&self.pool).await?;

        let mut out = std::collections::BTreeMap::new();
        for row in &rows {
            let account: i32 = row.try_get("account_index")?;
            out.insert(
                AccountId::from_index(u32::try_from(account).unwrap_or(0)),
                Balance {
                    debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
                    credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
                },
            );
        }
        Ok(out)
    }

    async fn statement(
        &self,
        key: BalanceKey,
        query: BalanceQuery<'_>,
        cursor: PostingCursor,
    ) -> Result<StatementPage<P>, Self::Error> {
        // A statement is a list of *postings*, and one entry may put several on
        // this account, so the cursor addresses `(log_index, posting_index)`.
        // Resuming from an entry position alone would skip whatever remained of
        // the entry a page ended inside.
        let (after_index, after_posting) = cursor.after.map_or((-1i64, -1i16), |p| {
            (
                i64::try_from(p.index.get()).unwrap_or(i64::MAX),
                i16::try_from(p.posting).unwrap_or(i16::MAX),
            )
        });
        let limit = cursor.effective_limit();
        let account = i64::from(key.account.index());

        // The page first: one row past it, so "is there more" is answered by
        // the query rather than guessed from a full page — which would hand back
        // a cursor that yields nothing.
        let probe = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut sql = Sql::default();
        let joins = dimension_joins(&mut sql, &query);
        let account_bind = sql.int(account);
        let currency = sql.text(key.currency.code());
        let layer = sql.text(layer_str(key.layer));
        let after_index_bind = sql.int(after_index);
        let after_posting_bind = sql.int(i64::from(after_posting));
        let predicate = entry_predicate(&mut sql, &query);
        let probe_bind = sql.int(probe);
        let text = format!(
            "SELECT e.log_index, e.entry_id, e.booking_date, e.value_date, e.kind, \
                    p.posting_index, \
                    p.direction, p.amount_minor \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id{joins} \
             WHERE p.account_index = {account_bind} AND p.currency = {currency} \
               AND p.layer = {layer} \
               AND (e.log_index > {after_index_bind} \
                    OR (e.log_index = {after_index_bind} \
                        AND p.posting_index > {after_posting_bind})){predicate} \
             ORDER BY e.log_index, p.posting_index LIMIT {probe_bind}"
        );
        let mut rows = sql.apply(sqlx::query(&text)).fetch_all(&self.pool).await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);

        // Then what the page opens at, which is two disjoint folds:
        //
        //   1. everything the query narrows to that was booked *before* its
        //      window — `BalanceQuery::opening`, defined by date so a backdated
        //      entry lands in the opening rather than mid-statement; and
        //   2. the statement's own lines up to and including the cursor, which
        //      earlier pages have already shown.
        //
        // Bounded by the cursor rather than by the first row: an exhausted
        // cursor must still open at the figure the window closed at, which is
        // what "no activity this period" should read as.
        let carried = self.fold_postings(key, query.opening(), None).await?;
        let shown = match cursor.after {
            Some(after) => self.fold_postings(key, Some(query), Some(after)).await?,
            None => Balance::ZERO,
        };
        let opening = carried.checked_add(&shown)?;

        let mut running = opening;
        let mut lines = Vec::with_capacity(rows.len());
        for row in &rows {
            let log_index: i64 = row.try_get("log_index")?;
            let entry_id: uuid::Uuid = row.try_get("entry_id")?;
            let posting_index: i16 = row.try_get("posting_index")?;
            let direction: String = row.try_get("direction")?;
            let amount = Amount::<P>::from_minor(row.try_get::<i64, _>("amount_minor")?);
            let direction = if direction == "D" {
                Direction::Debit
            } else {
                Direction::Credit
            };
            let kind = row
                .try_get::<Option<String>, _>("kind")?
                .and_then(|s| Label::new(s).ok());
            running.add(direction, amount)?;
            lines.push(crate::storage::StatementLine {
                index: LogIndex::new(u64::try_from(log_index).unwrap_or(0)),
                posting: PostingRef::new(
                    EntryId::from_uuid(entry_id),
                    u16::try_from(posting_index).unwrap_or(0),
                ),
                booking_date: row.try_get("booking_date")?,
                value_date: row.try_get("value_date")?,
                direction,
                amount,
                running,
                kind,
            });
        }

        let next = lines.last().filter(|_| has_more).map(|l| PostingCursor {
            after: Some(l.position()),
            limit: cursor.limit,
        });
        Ok(StatementPage {
            opening,
            lines,
            next,
        })
    }

    /// Conditional on the tree size not going backwards — see the trait.
    async fn save_checkpoint(&self, checkpoint: &Checkpoint<P>) -> Result<(), Self::Error> {
        sqlx::query(
            "INSERT INTO checkpoints ( \
                account_index, currency, layer, debits_minor, credits_minor, \
                tree_size, tree_root \
             ) VALUES ($1,$2,$3,$4,$5,$6,$7) \
             ON CONFLICT (account_index, currency, layer) DO UPDATE SET \
                debits_minor  = EXCLUDED.debits_minor, \
                credits_minor = EXCLUDED.credits_minor, \
                tree_size     = EXCLUDED.tree_size, \
                tree_root     = EXCLUDED.tree_root, \
                taken_at      = now() \
             WHERE EXCLUDED.tree_size >= checkpoints.tree_size",
        )
        .bind(i32::try_from(checkpoint.key.account.index()).unwrap_or(i32::MAX))
        .bind(checkpoint.key.currency.code())
        .bind(layer_str(checkpoint.key.layer))
        .bind(checkpoint.balance.debits.to_minor())
        .bind(checkpoint.balance.credits.to_minor())
        .bind(i64::try_from(checkpoint.tree_head.size).unwrap_or(i64::MAX))
        .bind(checkpoint.tree_head.root.as_bytes().as_slice())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_checkpoint(&self, key: BalanceKey) -> Result<Option<Checkpoint<P>>, Self::Error> {
        let Some(row) = sqlx::query(
            "SELECT debits_minor, credits_minor, tree_size, tree_root \
             FROM checkpoints WHERE account_index = $1 AND currency = $2 AND layer = $3",
        )
        .bind(i32::try_from(key.account.index()).unwrap_or(i32::MAX))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let size: i64 = row.try_get("tree_size")?;
        Ok(Some(Checkpoint::new(
            key,
            Balance {
                debits: Amount::from_minor(row.try_get::<i64, _>("debits_minor")?),
                credits: Amount::from_minor(row.try_get::<i64, _>("credits_minor")?),
            },
            TreeHead {
                size: u64::try_from(size).unwrap_or(0),
                root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("tree_root")?)?,
            },
        )))
    }
}

impl<const P: u8> PostgresStore<P> {}

/// What a clearing needs to know about one posting: which side, which account,
/// which currency, which layer, and how much of it is still open.
struct PostingFacts<const P: u8> {
    direction: Direction,
    account: AccountId,
    currency: Currency,
    layer: Layer,
    residual: Amount<P>,
}

/// One query rather than two: the residual and the posting's own facts come from
/// the same row, so they cannot describe different moments.
async fn posting_facts<const P: u8>(
    tx: &mut Transaction<'_, Postgres>,
    reference: PostingRef,
) -> Result<PostingFacts<P>, PostgresError> {
    let row = sqlx::query(
        "SELECT p.direction, p.account_index, p.currency, p.layer, \
           (p.amount_minor - COALESCE(( \
                SELECT SUM(ci.applied_minor) FROM clearing_items ci \
                JOIN clearings c ON c.clearing_id = ci.clearing_id \
                WHERE ci.entry_id = p.entry_id AND ci.posting_index = p.posting_index \
                  AND c.reset_on IS NULL \
            ), 0))::BIGINT AS residual \
         FROM postings p WHERE p.entry_id = $1 AND p.posting_index = $2",
    )
    .bind(reference.entry.as_uuid())
    .bind(i16::try_from(reference.index).unwrap_or(i16::MAX))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(crate::clearing::ClearingError::UnknownPosting { posting: reference })?;

    let direction: String = row.try_get("direction")?;
    let account: i32 = row.try_get("account_index")?;
    let currency: String = row.try_get("currency")?;
    let layer: String = row.try_get("layer")?;
    Ok(PostingFacts {
        direction: match direction.as_str() {
            "D" => Direction::Debit,
            _ => Direction::Credit,
        },
        account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
        currency: Currency::new(currency.trim())
            .map_err(|_| PostgresError::malformed(format!("currency {currency:?}")))?,
        layer: match layer.as_str() {
            "pending" => Layer::Pending,
            _ => Layer::Settled,
        },
        residual: Amount::from_minor(row.try_get::<i64, _>("residual")?),
    })
}

/// Loads an entry's postings inside an open transaction.
async fn load_postings_tx<const P: u8>(
    tx: &mut Transaction<'_, Postgres>,
    entry_id: uuid::Uuid,
) -> Result<Vec<Posting<P>>, PostgresError> {
    let rows = sqlx::query(
        "SELECT posting_index, account_index, direction, amount_minor, currency, layer \
         FROM postings WHERE entry_id = $1 ORDER BY posting_index",
    )
    .bind(entry_id)
    .fetch_all(&mut **tx)
    .await?;
    let dim_rows = sqlx::query(
        "SELECT entry_id, posting_index, axis, value FROM posting_dimensions \
         WHERE entry_id = $1 ORDER BY posting_index, axis",
    )
    .bind(entry_id)
    .fetch_all(&mut **tx)
    .await?;
    let dimensions = dimensions_from(&dim_rows)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let posting_index: i16 = row.try_get("posting_index")?;
        let mut posting = build_posting::<P>(row)?;
        if let Some(dims) = dimensions.get(&(entry_id, posting_index)) {
            posting.dimensions = dims.clone();
        }
        out.push(posting);
    }
    Ok(out)
}

/// The stored code for a balance limit.
fn limit_code(limit: BalanceLimit) -> &'static str {
    match limit {
        BalanceLimit::Unlimited => "unlimited",
        BalanceLimit::NoCreditBalance => "no_credit",
        BalanceLimit::NoDebitBalance => "no_debit",
    }
}

/// The balance limit a stored code names.
fn limit_from_code(code: &str) -> Option<BalanceLimit> {
    match code {
        "unlimited" => Some(BalanceLimit::Unlimited),
        "no_credit" => Some(BalanceLimit::NoCreditBalance),
        "no_debit" => Some(BalanceLimit::NoDebitBalance),
        _ => None,
    }
}

/// The stored code for a reporting classification.
fn kind_code(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::Asset => "asset",
        AccountKind::Liability => "liability",
        AccountKind::Equity => "equity",
        AccountKind::Income => "income",
        AccountKind::Expense => "expense",
    }
}

/// Parses a stored classification code.
fn kind_from_code(code: &str) -> Option<AccountKind> {
    match code {
        "asset" => Some(AccountKind::Asset),
        "liability" => Some(AccountKind::Liability),
        "equity" => Some(AccountKind::Equity),
        "income" => Some(AccountKind::Income),
        "expense" => Some(AccountKind::Expense),
        _ => None,
    }
}

/// Rebuilds one handle-to-account binding from its row.
fn account_record(row: &sqlx::postgres::PgRow) -> Result<AccountRecord, PostgresError> {
    let index: i32 = row.try_get("account_index")?;
    let path: String = row.try_get("path")?;
    let kind: Option<String> = row.try_get("kind")?;
    let mut account = Account::new(
        AccountPath::parse(&path).map_err(|e| PostgresError::malformed(e.to_string()))?,
        row.try_get("opened_on")?,
    );
    account.kind = kind.as_deref().and_then(kind_from_code);
    account.closed_on = row.try_get("closed_on")?;
    let limit: String = row.try_get("balance_limit")?;
    account.limit = limit_from_code(&limit)
        .ok_or_else(|| PostgresError::malformed(format!("balance limit {limit:?}")))?;
    Ok(AccountRecord {
        id: AccountId::from_index(u32::try_from(index).unwrap_or(0)),
        account,
    })
}
