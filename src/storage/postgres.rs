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
//! # Integrity on read
//!
//! Rows are rehydrated through [`Entry::adopt_verified`], which recomputes the
//! content hash and compares it with the one stored alongside. A row altered
//! underneath the engine surfaces as an error on the next read rather than as a
//! wrong number in a report.

use std::collections::BTreeSet;
use std::future::Future;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Row, Transaction};
use time::Date;

use crate::account::{
    Account, AccountId, AccountKind, AccountPath, AccountRecord, AccountRegistry, BalanceLimit,
};
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::checkpoint::Checkpoint;
use crate::clearing::{Clearing, ClearingId, OpenItem, PostingRef};
use crate::dimensions::{Dimensions, Label};
use crate::entry::{
    Balanced, Description, DocumentRef, Draft, Entry, EntryId, IdempotencyKey, IntegrityError,
    Provenance,
};
use crate::hash::Hash;
use crate::journal::{LogIndex, Recorded};
use crate::merkle::{
    ConsistencyProof, InclusionProof, MalformedAccumulator, MerkleAccumulator, MerkleLog,
    ProofError, TreeHead, empty_root,
};
use crate::money::{Amount, Currency, MoneyError};
use crate::period::{LedgerId, Period, PeriodCalendar, PeriodId, PeriodState};
use crate::posting::{Direction, Layer, Posting};
use crate::seal::{PeriodCoverage, Seal, SealChain};
use crate::storage::{Cursor, EntryBatch, LedgerStore, Page, StatementPage, StoredEntry};

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
    /// Arithmetic overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),
    /// The period is not defined.
    #[error("period {period} is not defined")]
    UnknownPeriod {
        /// The missing period.
        period: PeriodId,
    },
    /// The period is not ready to be sealed.
    #[error("period {period} is {state}; only a closing period can be sealed")]
    PeriodNotClosing {
        /// The period.
        period: PeriodId,
        /// Its current state.
        state: PeriodState,
    },
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
    /// The persisted Merkle accumulator does not describe the stored log.
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
    /// An entry would leave an account on a side its balance limit forbids.
    #[error(
        "account {account} in {currency} ({layer}) would net to {net_minor} \
         minor units, which its {limit} limit forbids"
    )]
    LimitBreached {
        /// The account whose limit would be breached.
        account: AccountId,
        /// The currency the limit was breached in.
        currency: Currency,
        /// The layer the limit was breached in.
        layer: Layer,
        /// The limit in force.
        limit: BalanceLimit,
        /// The signed net the entry would leave, debit positive.
        net_minor: i64,
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

    /// Every content hash in log order, which is the Merkle log's leaf sequence.
    async fn leaves(&self) -> Result<Vec<Hash>, PostgresError> {
        let rows = sqlx::query(
            "SELECT content_hash FROM entries WHERE log_index IS NOT NULL ORDER BY log_index",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let bytes: Vec<u8> = r.try_get("content_hash")?;
                hash_from_bytes(&bytes)
            })
            .collect()
    }

    /// Rebuilds the full log, leaves included.
    ///
    /// Needed only to construct proofs, which read the whole history by nature.
    /// Head queries do not go through here — see [`LedgerStore::head`].
    async fn log(&self) -> Result<MerkleLog, PostgresError> {
        Ok(MerkleLog::from_leaves(self.leaves().await?))
    }

    /// Restores the persisted accumulator.
    async fn accumulator(
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<MerkleAccumulator, PostgresError> {
        let rows = sqlx::query("SELECT height, node FROM log_subtrees ORDER BY position")
            .fetch_all(&mut **tx)
            .await?;
        let mut subtrees = Vec::with_capacity(rows.len());
        for row in &rows {
            let height: i16 = row.try_get("height")?;
            let node: Vec<u8> = row.try_get("node")?;
            subtrees.push((u8::try_from(height).unwrap_or(0), hash_from_bytes(&node)?));
        }

        let size: i64 =
            sqlx::query("SELECT COUNT(*)::BIGINT AS n FROM entries WHERE log_index IS NOT NULL")
                .fetch_one(&mut **tx)
                .await?
                .try_get("n")?;
        // Checked, not trusted: a row lost, duplicated or returned out of order
        // would otherwise produce a plausible-looking wrong root.
        Ok(MerkleAccumulator::try_from_parts(
            subtrees,
            u64::try_from(size).unwrap_or(0),
        )?)
    }

    /// Persists the accumulator, replacing what was there.
    async fn store_accumulator(
        tx: &mut Transaction<'_, Postgres>,
        accumulator: &MerkleAccumulator,
    ) -> Result<(), PostgresError> {
        sqlx::query("DELETE FROM log_subtrees")
            .execute(&mut **tx)
            .await?;
        for (position, (height, node)) in accumulator.subtrees().iter().enumerate() {
            sqlx::query("INSERT INTO log_subtrees (height, node, position) VALUES ($1, $2, $3)")
                .bind(i16::from(*height))
                .bind(node.as_bytes().as_slice())
                .bind(i16::try_from(position).unwrap_or(i16::MAX))
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
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
        // Speculative: the root this entry would produce. Only committed to the
        // accumulator if the row actually lands.
        // In inline mode the position and its root are assigned now; in deferred
        // mode both stay NULL until the sequencer runs.
        let (assigned_index, projected_root, projected) = match placement {
            Placement::Inline => {
                let mut projected = accumulator.clone();
                projected.push(content_hash);
                let root = projected.root();
                (Some(*next_index), Some(root), Some(projected))
            }
            Placement::Deferred => (None, None, None),
        };

        let inserted = sqlx::query(
            "INSERT INTO entries ( \
                log_index, entry_id, idempotency_key, content_hash, booking_date, value_date, \
                description, provenance_actor, provenance_source, provenance_correlation, \
                document_id, document_content_hash, reverses, original_booking_date, tree_root, \
                kind \
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) \
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
        .bind(projected_root.map(|r| r.as_bytes().to_vec()))
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

        if let Some(projected) = projected {
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
        let mut checked: BTreeSet<(u32, Currency, Layer)> = BTreeSet::new();
        for posting in entry.postings() {
            if !checked.insert((posting.account.index(), posting.currency, posting.layer)) {
                continue;
            }
            let index = i32::try_from(posting.account.index()).unwrap_or(i32::MAX);
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

            let net: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(CASE WHEN direction = 'D' \
                                          THEN amount_minor ELSE -amount_minor END), 0)::BIGINT \
                 FROM postings \
                 WHERE account_index = $1 AND currency = $2 AND layer = $3",
            )
            .bind(index)
            .bind(posting.currency.code())
            .bind(layer_str(posting.layer))
            .fetch_one(&mut **tx)
            .await?;

            let permitted = match limit {
                BalanceLimit::Unlimited => true,
                BalanceLimit::NoCreditBalance => net >= 0,
                BalanceLimit::NoDebitBalance => net <= 0,
            };
            if !permitted {
                return Err(PostgresError::LimitBreached {
                    account: posting.account,
                    currency: posting.currency,
                    layer: posting.layer,
                    limit,
                    net_minor: net,
                });
            }
        }
        Ok(())
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
        size: Option<u64>,
    ) -> Result<Balance<P>, PostgresError> {
        let bound = prefix_bound(size);
        let row = sqlx::query(
            "SELECT \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE p.account_index = $1 AND p.currency = $2 AND p.layer = $3 \
               AND e.log_index IS NOT NULL AND e.log_index < $4",
        )
        .bind(i32::try_from(key.account.index()).unwrap_or(i32::MAX))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .bind(bound)
        .fetch_one(&self.pool)
        .await?;

        Ok(Balance {
            debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
            credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
        })
    }
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
            let next: Option<i64> =
                sqlx::query("SELECT COALESCE(MAX(log_index) + 1, 0) AS next FROM entries")
                    .fetch_one(&mut *tx)
                    .await?
                    .try_get("next")?;
            next_index = next.unwrap_or(0);
            accumulator = Self::accumulator(&mut tx).await?;
        }

        let mut out = Vec::with_capacity(batch.len());
        for entry in batch.entries() {
            out.push(
                Self::append_one(&mut tx, entry, &mut next_index, &mut accumulator, placement)
                    .await?,
            );
        }
        if placement == Placement::Inline {
            Self::store_accumulator(&mut tx, &accumulator).await?;
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

        let next: Option<i64> =
            sqlx::query("SELECT COALESCE(MAX(log_index) + 1, 0) AS next FROM entries")
                .fetch_one(&mut *tx)
                .await?
                .try_get("next")?;
        let mut index = next.unwrap_or(0);
        let mut accumulator = Self::accumulator(&mut tx).await?;

        let mut sequenced = 0u64;
        for row in &rows {
            let entry_id: uuid::Uuid = row.try_get("entry_id")?;
            let content_hash = hash_from_bytes(&row.try_get::<Vec<u8>, _>("content_hash")?)?;
            accumulator.push(content_hash);

            sqlx::query("UPDATE entries SET log_index = $2, tree_root = $3 WHERE entry_id = $1")
                .bind(entry_id)
                .bind(index)
                .bind(accumulator.root().as_bytes().as_slice())
                .execute(&mut *tx)
                .await?;

            index = index.saturating_add(1);
            sequenced = sequenced.saturating_add(1);
        }

        Self::store_accumulator(&mut tx, &accumulator).await?;
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
        // One row, not a rebuild: each entry records the root it produced.
        let Some(row) = sqlx::query(
            "SELECT log_index, tree_root FROM entries \
             WHERE log_index IS NOT NULL ORDER BY log_index DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(TreeHead {
                size: 0,
                root: crate::merkle::empty_root(),
            });
        };
        let log_index: i64 = row.try_get("log_index")?;
        Ok(TreeHead {
            size: u64::try_from(log_index).unwrap_or(0).saturating_add(1),
            root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("tree_root")?)?,
        })
    }

    async fn head_at(&self, size: u64) -> Result<TreeHead, Self::Error> {
        if size == 0 {
            return Ok(TreeHead {
                size: 0,
                root: empty_root(),
            });
        }
        // Each entry stores the root as of its own sequencing, so a historical
        // head is a row lookup rather than a replay of the log.
        let index = i64::try_from(size.saturating_sub(1)).unwrap_or(i64::MAX);
        let row = sqlx::query("SELECT tree_root FROM entries WHERE log_index = $1")
            .bind(index)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                PostgresError::from(ProofError::SizeOutOfRange {
                    from: size,
                    size: 0,
                })
            })?;
        Ok(TreeHead {
            size,
            root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("tree_root")?)?,
        })
    }

    async fn len(&self) -> Result<u64, Self::Error> {
        let row =
            sqlx::query("SELECT COUNT(*)::BIGINT AS n FROM entries WHERE log_index IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        let n: i64 = row.try_get("n")?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn balance(&self, key: BalanceKey, size: Option<u64>) -> Result<Balance<P>, Self::Error> {
        self.fold_balance(&key, size).await
    }

    async fn trial_balance(&self, size: Option<u64>) -> Result<TrialBalance<P>, Self::Error> {
        let rows = sqlx::query(
            "SELECT p.account_index, p.currency, p.layer, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE e.log_index IS NOT NULL AND e.log_index < $1 \
             GROUP BY p.account_index, p.currency, p.layer \
             ORDER BY p.account_index, p.currency, p.layer",
        )
        .bind(prefix_bound(size))
        .fetch_all(&self.pool)
        .await?;

        let mut tb = TrialBalance::new();
        for row in &rows {
            let account: i32 = row.try_get("account_index")?;
            let currency: String = row.try_get("currency")?;
            let layer: String = row.try_get("layer")?;
            let key = BalanceKey {
                account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
                currency: Currency::new(currency.trim())
                    .map_err(|_| PostgresError::malformed(format!("currency {currency:?}")))?,
                layer: match layer.as_str() {
                    "settled" => Layer::Settled,
                    "pending" => Layer::Pending,
                    other => return Err(PostgresError::malformed(format!("layer {other:?}"))),
                },
            };
            tb.set(
                key,
                Balance {
                    debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
                    credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
                },
            );
        }
        Ok(tb)
    }

    async fn prove_inclusion(&self, index: LogIndex) -> Result<InclusionProof, Self::Error> {
        Ok(self.log().await?.inclusion_proof(index.get())?)
    }

    async fn prove_inclusion_at(
        &self,
        index: LogIndex,
        size: u64,
    ) -> Result<InclusionProof, Self::Error> {
        Ok(self.log().await?.inclusion_proof_at(index.get(), size)?)
    }

    async fn prove_consistency(&self, old_size: u64) -> Result<ConsistencyProof, Self::Error> {
        Ok(self.log().await?.consistency_proof(old_size)?)
    }

    async fn prove_consistency_between(
        &self,
        old_size: u64,
        new_size: u64,
    ) -> Result<ConsistencyProof, Self::Error> {
        Ok(self
            .log()
            .await?
            .consistency_proof_between(old_size, new_size)?)
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

    async fn seal_period(&self, period: &PeriodId) -> Result<Seal, Self::Error> {
        let Some(definition) = <Self as LedgerStore<P>>::periods(self)
            .await?
            .into_iter()
            .find(|p| p.id == *period)
        else {
            return Err(PostgresError::UnknownPeriod {
                period: period.clone(),
            });
        };
        if definition.state != PeriodState::Closing {
            return Err(PostgresError::PeriodNotClosing {
                period: period.clone(),
                state: definition.state,
            });
        }

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

        let closing = self.trial_balance_through_date(definition.end).await?;
        // Rebuilt through the chain rather than counted: seals read back from a
        // table are rows, not evidence, and the new one has to chain onto a
        // predecessor that itself still holds.
        let chain = SealChain::from_seals(self.ledger.clone(), self.seals().await?)
            .map_err(|e| PostgresError::malformed(e.to_string()))?;
        let position = i64::try_from(chain.len()).unwrap_or(0);

        let seal = Seal::build(
            self.ledger.clone(),
            period.clone(),
            PeriodCoverage {
                first_index: first.map(|v| u64::try_from(v).unwrap_or(0)),
                last_index: last.map(|v| u64::try_from(v).unwrap_or(0)),
                entry_count: u64::try_from(count).unwrap_or(0),
            },
            self.head().await?,
            &closing,
            // Built from the stored bindings, not from a caller-supplied
            // registry: the seal must commit to the handles this database
            // actually resolved the balances against.
            AccountRegistry::from_records(LedgerStore::<P>::accounts(self).await?)
                .map_err(|e: crate::account::AccountError| PostgresError::malformed(e.to_string()))?
                .commitment(),
            chain.head(),
        );

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO seals ( \
                period_id, first_index, last_index, entry_count, tree_size, tree_root, \
                trial_balance_size, trial_balance_root, accounts_size, accounts_root, \
                prev_seal, seal_hash, chain_position \
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
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
             prev_seal, seal_hash \
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

    async fn open_items(&self, key: BalanceKey) -> Result<Vec<OpenItem<P>>, Self::Error> {
        let rows = sqlx::query(
            "SELECT o.entry_id, o.posting_index, o.direction, o.original_minor, \
                    o.applied_minor, o.residual_minor \
             FROM open_items o \
             WHERE o.account_index = $1 AND o.currency = $2 AND o.layer = $3 \
             ORDER BY o.entry_id, o.posting_index",
        )
        .bind(i32::try_from(key.account.index()).unwrap_or(i32::MAX))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let entry_id: uuid::Uuid = row.try_get("entry_id")?;
            let posting_index: i16 = row.try_get("posting_index")?;
            let direction: String = row.try_get("direction")?;
            out.push(OpenItem {
                posting: PostingRef::new(
                    EntryId::from_uuid(entry_id),
                    u16::try_from(posting_index).unwrap_or(0),
                ),
                direction: match direction.as_str() {
                    "D" => Direction::Debit,
                    _ => Direction::Credit,
                },
                original: Amount::from_minor(row.try_get::<i64, _>("original_minor")?),
                applied: Amount::from_minor(row.try_get::<i64, _>("applied_minor")?),
                residual: Amount::from_minor(row.try_get::<i64, _>("residual_minor")?),
            });
        }
        out.sort_by_key(|i| i.posting);
        Ok(out)
    }

    async fn balances(
        &self,
        accounts: &[AccountId],
        currency: Currency,
        layer: Layer,
        size: Option<u64>,
    ) -> Result<std::collections::BTreeMap<AccountId, Balance<P>>, Self::Error> {
        let wanted: Vec<i32> = accounts
            .iter()
            .map(|a| i32::try_from(a.index()).unwrap_or(i32::MAX))
            .collect();
        let rows = sqlx::query(
            "SELECT p.account_index, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE p.account_index = ANY($1) AND p.currency = $2 AND p.layer = $3 \
               AND e.log_index IS NOT NULL AND e.log_index < $4 \
             GROUP BY p.account_index",
        )
        .bind(&wanted)
        .bind(currency.code())
        .bind(layer_str(layer))
        .bind(prefix_bound(size))
        .fetch_all(&self.pool)
        .await?;

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
        cursor: Cursor,
    ) -> Result<StatementPage<P>, Self::Error> {
        let after = cursor
            .after
            .map_or(-1i64, |i| i64::try_from(i.get()).unwrap_or(i64::MAX));
        let limit = cursor.effective_limit();

        // The running balance needs everything strictly before the page, so it
        // is summed in the database rather than by reading the account's whole
        // history. Nothing precedes the first page — reading the account's full
        // balance here would start every statement at its own closing figure.
        // `after` is an index, and the prefix that precedes the page is
        // everything up to and including it — one more entry than its index.
        let opening = match cursor.after {
            Some(after) => {
                self.fold_balance(&key, Some(after.get().saturating_add(1)))
                    .await?
            }
            None => Balance::ZERO,
        };

        // One row past the page, so "is there more" is answered by the query
        // rather than guessed from a full page — which would hand back a cursor
        // that yields nothing.
        let probe = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut rows = sqlx::query(
            "SELECT e.log_index, e.entry_id, e.booking_date, e.kind, p.posting_index, \
                    p.direction, p.amount_minor \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE p.account_index = $1 AND p.currency = $2 AND p.layer = $3 \
               AND e.log_index IS NOT NULL AND e.log_index > $4 \
             ORDER BY e.log_index, p.posting_index LIMIT $5",
        )
        .bind(i32::try_from(key.account.index()).unwrap_or(i32::MAX))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .bind(after)
        .bind(probe)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);

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
                direction,
                amount,
                running,
                kind,
            });
        }

        let next = lines.last().filter(|_| has_more).map(|l| Cursor {
            after: Some(l.index),
            limit: cursor.limit,
        });
        Ok(StatementPage { lines, next })
    }

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
                taken_at      = now()",
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

impl<const P: u8> PostgresStore<P> {
    /// Folds every *sequenced* entry booked on or before `end`.
    ///
    /// The `log_index IS NOT NULL` predicate is what keeps a seal honest: the
    /// tree head it carries covers only sequenced entries, so a closing balance
    /// that folded in unsequenced ones would commit to money the tree head does
    /// not account for. In deferred mode that window is real.
    async fn trial_balance_through_date(
        &self,
        end: Date,
    ) -> Result<TrialBalance<P>, PostgresError> {
        let rows = sqlx::query(
            "SELECT p.account_index, p.currency, p.layer, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'D'), 0)::BIGINT AS debits, \
               COALESCE(SUM(p.amount_minor) FILTER (WHERE p.direction = 'C'), 0)::BIGINT AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE e.log_index IS NOT NULL AND e.booking_date <= $1 \
             GROUP BY p.account_index, p.currency, p.layer \
             ORDER BY p.account_index, p.currency, p.layer",
        )
        .bind(end)
        .fetch_all(&self.pool)
        .await?;

        let mut tb = TrialBalance::new();
        for row in &rows {
            let account: i32 = row.try_get("account_index")?;
            let currency: String = row.try_get("currency")?;
            let layer: String = row.try_get("layer")?;
            tb.set(
                BalanceKey {
                    account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
                    currency: Currency::new(currency.trim())
                        .map_err(|_| PostgresError::malformed("currency"))?,
                    layer: match layer.as_str() {
                        "settled" => Layer::Settled,
                        _ => Layer::Pending,
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
}

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
