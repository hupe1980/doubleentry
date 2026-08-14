//! A SQLite-backed [`LedgerStore`].
//!
//! The schema is [`schema/sqlite.sql`](https://github.com/hupe1980/doubleentry/blob/main/schema/sqlite.sql),
//! applied by [`SqliteStore::migrate`]. It suits embedded and single-process
//! deployments; where the ledger must be defended against processes other than
//! this one, PostgreSQL is the stronger choice, because it enforces the balance
//! invariant in the database and can revoke `UPDATE` and `DELETE`.
//!
//! # Write serialisation
//!
//! SQLite admits one writer at a time. Appends open with `BEGIN IMMEDIATE`, so
//! the write lock is taken *before* the next log index is read. A deferred
//! transaction would read the index first and only then try to upgrade — which
//! either fails under contention or, worse, commits against a stale read and
//! duplicates an index.
//!
//! # What SQLite cannot enforce
//!
//! No deferrable constraint triggers, so the balance invariant is not checked a
//! second time by the database; no `EXCLUDE` constraint, so period non-overlap
//! rests on [`PeriodCalendar`]; no per-table privileges, so append-only rests on
//! the application. The engine's guarantees are unchanged — what is reduced is
//! defence in depth against writers that are not the engine.
//!
//! # Foreign keys belong to the pool
//!
//! `PRAGMA foreign_keys` is per connection, and SQLite defaults it to `OFF`.
//! Setting it from here would configure one pooled connection and leave the rest
//! ignoring every `REFERENCES` clause in the schema, so [`SqliteStore::migrate`]
//! *verifies* it instead and refuses a pool that does not enforce it. `sqlx`
//! enables it by default, so an ordinary pool already passes.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use time::Date;

use crate::account::{
    Account, AccountId, AccountKind, AccountPath, AccountRecord, AccountRegistry, BalanceLimit,
};
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::checkpoint::Checkpoint;
use crate::clearing::{Clearing, ClearingError, ClearingId, OpenItem, PostingRef};
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

/// The reference DDL, applied by [`SqliteStore::migrate`].
pub const SCHEMA: &str = include_str!("../../schema/sqlite.sql");

/// Failure from the SQLite backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SqliteError {
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
    Clearing(#[from] ClearingError),
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
    /// The pool opens connections without foreign-key enforcement.
    #[error(transparent)]
    ForeignKeysDisabled(#[from] ForeignKeysDisabled),
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

impl SqliteError {
    fn malformed(what: impl Into<String>) -> Self {
        Self::Malformed(what.into())
    }
}

/// The pool does not enforce foreign keys.
///
/// `PRAGMA foreign_keys` is per connection, so this is a property of how the
/// pool opens them and cannot be repaired by the store. Build the pool with
/// `SqliteConnectOptions::foreign_keys(true)` — which is `sqlx`'s default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "this pool opens connections with `PRAGMA foreign_keys = OFF`, which disables every \
     REFERENCES clause in the schema; build it with SqliteConnectOptions::foreign_keys(true)"
)]
pub struct ForeignKeysDisabled;

/// A ledger stored in SQLite.
#[derive(Debug, Clone)]
pub struct SqliteStore<const P: u8> {
    pool: SqlitePool,
    ledger: LedgerId,
}

impl<const P: u8> SqliteStore<P> {
    /// Wraps a connection pool, serving one ledger.
    #[must_use]
    pub fn new(pool: SqlitePool, ledger: LedgerId) -> Self {
        Self { pool, ledger }
    }

    /// The underlying pool.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Applies [`SCHEMA`] and checks the settings it relies on.
    ///
    /// # Foreign keys are a property of the pool, not of this call
    ///
    /// `PRAGMA foreign_keys` is **per connection**. Issuing it here would set it
    /// on whichever pooled connection happened to serve this call and leave
    /// every other one with SQLite's default of `OFF` — silently disabling every
    /// `REFERENCES` clause in the schema for most work the store does. It has to
    /// be part of how connections are opened, which is why this verifies the
    /// setting instead of trying to apply it.
    ///
    /// `sqlx` enables it by default, so a pool built the ordinary way already
    /// satisfies this. A pool built with `SqliteConnectOptions::foreign_keys(false)`
    /// does not, and is refused rather than run with a constraint set that is
    /// present in the DDL and absent at run time.
    ///
    /// `journal_mode` is a property of the database file rather than the
    /// connection, so setting it once is correct.
    ///
    /// # Errors
    ///
    /// Returns [`SqliteError::ForeignKeysDisabled`] when the pool does not
    /// enforce foreign keys, and any error the database raises.
    pub async fn migrate(&self) -> Result<(), SqliteError> {
        // WAL is durable in the file header; one connection setting it is enough
        // and it keeps readers from blocking the single writer.
        sqlx::raw_sql("PRAGMA journal_mode = WAL")
            .execute(&self.pool)
            .await?;
        self.check_foreign_keys().await?;
        sqlx::raw_sql(SCHEMA).execute(&self.pool).await?;
        // One database, one ledger. Claim it on first use and refuse it
        // afterwards if it belongs to someone else — pointing two ledgers at one
        // database would merge two logs, two index spaces, and two seal chains
        // into one, silently.
        sqlx::query(
            "INSERT INTO ledger_meta (only_row, ledger_id) VALUES (1, ?1) \
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
            return Err(SqliteError::WrongLedger {
                expected: self.ledger.clone(),
                found: LedgerId::new(found).map_err(|e| SqliteError::malformed(e.to_string()))?,
            });
        }
        Ok(())
    }

    /// Confirms the pool opens connections with foreign keys enforced.
    ///
    /// Checked on a connection the pool hands out, not on one this call
    /// configures — the question is what an arbitrary future connection will do,
    /// and the only way to learn that is to ask one.
    async fn check_foreign_keys(&self) -> Result<(), SqliteError> {
        let enabled: i64 = sqlx::query("PRAGMA foreign_keys")
            .fetch_one(&self.pool)
            .await?
            .try_get(0)?;
        if enabled == 1 {
            Ok(())
        } else {
            Err(ForeignKeysDisabled.into())
        }
    }

    /// The calendar as the database holds it.
    ///
    /// # Errors
    ///
    /// Returns any error the database raises, or a malformed row.
    pub async fn calendar(&self) -> Result<PeriodCalendar, SqliteError> {
        Ok(PeriodCalendar::from_periods(self.periods().await?)?)
    }

    async fn log(&self) -> Result<MerkleLog, SqliteError> {
        let rows = sqlx::query(
            "SELECT content_hash FROM entries WHERE log_index IS NOT NULL ORDER BY log_index",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut leaves = Vec::with_capacity(rows.len());
        for row in &rows {
            leaves.push(hash_from_bytes(
                &row.try_get::<Vec<u8>, _>("content_hash")?,
            )?);
        }
        Ok(MerkleLog::from_leaves(leaves))
    }

    async fn accumulator(
        tx: &mut Transaction<'_, Sqlite>,
    ) -> Result<MerkleAccumulator, SqliteError> {
        let rows = sqlx::query("SELECT height, node FROM log_subtrees ORDER BY position")
            .fetch_all(&mut **tx)
            .await?;
        let mut subtrees = Vec::with_capacity(rows.len());
        for row in &rows {
            let height: i64 = row.try_get("height")?;
            let node: Vec<u8> = row.try_get("node")?;
            subtrees.push((u8::try_from(height).unwrap_or(0), hash_from_bytes(&node)?));
        }
        let size: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM entries WHERE log_index IS NOT NULL")
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

    async fn store_accumulator(
        tx: &mut Transaction<'_, Sqlite>,
        accumulator: &MerkleAccumulator,
    ) -> Result<(), SqliteError> {
        sqlx::query("DELETE FROM log_subtrees")
            .execute(&mut **tx)
            .await?;
        for (position, (height, node)) in accumulator.subtrees().iter().enumerate() {
            sqlx::query("INSERT INTO log_subtrees (height, node, position) VALUES (?1, ?2, ?3)")
                .bind(i64::from(*height))
                .bind(node.as_bytes().as_slice())
                .bind(i64::try_from(position).unwrap_or(i64::MAX))
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    async fn check_reversal(
        tx: &mut Transaction<'_, Sqlite>,
        entry: &Entry<Balanced, P>,
        original: EntryId,
    ) -> Result<(), SqliteError> {
        let Some(row) = sqlx::query(
            "SELECT reverses, \
             (SELECT entry_id FROM entries r WHERE r.reverses = e.entry_id) AS reversed_by \
             FROM entries e WHERE e.entry_id = ?1",
        )
        .bind(uuid_bytes(original))
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Err(SqliteError::UnknownOriginal { id: original });
        };

        if row.try_get::<Option<Vec<u8>>, _>("reverses")?.is_some() {
            return Err(SqliteError::ReversalOfReversal { id: original });
        }
        if row
            .try_get::<Option<Vec<u8>>, _>("reversed_by")?
            .is_some_and(|by| by != uuid_bytes(entry.id()))
        {
            return Err(SqliteError::AlreadyReversed { id: original });
        }

        let target = load_postings_tx::<P>(tx, uuid_bytes(original)).await?;
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
        if inverts {
            Ok(())
        } else {
            Err(SqliteError::NotAnInversion { id: original })
        }
    }

    async fn append_one(
        tx: &mut Transaction<'_, Sqlite>,
        entry: &Entry<Balanced, P>,
        next_index: &mut i64,
        accumulator: &mut MerkleAccumulator,
    ) -> Result<Recorded, SqliteError> {
        let content_hash = entry.content_hash();

        // The primary key would refuse this anyway; catching it here turns a
        // constraint violation into an error that names what went wrong. Scoped
        // to a *different* idempotency key, so a genuine retry — same
        // identifier, same key — still falls through to the replay path below
        // rather than being reported as a clash with itself.
        let clash: Option<i64> =
            sqlx::query("SELECT 1 AS x FROM entries WHERE entry_id = ?1 AND idempotency_key <> ?2")
                .bind(uuid_bytes(entry.id()))
                .bind(entry.idempotency_key().as_bytes())
                .fetch_optional(&mut **tx)
                .await?
                .map(|row| row.try_get("x"))
                .transpose()?;
        if clash.is_some() {
            return Err(SqliteError::DuplicateId { id: entry.id() });
        }

        if let Some(original) = entry.reverses() {
            Self::check_reversal(tx, entry, original).await?;
        }

        let mut projected = accumulator.clone();
        projected.push(content_hash);
        let projected_root = projected.root();

        let inserted = sqlx::query(
            "INSERT INTO entries ( \
                log_index, entry_id, idempotency_key, content_hash, booking_date, value_date, \
                description, provenance_actor, provenance_source, provenance_correlation, \
                document_id, document_content_hash, reverses, original_booking_date, tree_root, \
                kind \
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) \
             ON CONFLICT (idempotency_key) DO NOTHING \
             RETURNING log_index",
        )
        .bind(*next_index)
        .bind(uuid_bytes(entry.id()))
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
                .map(|h| h.as_bytes().to_vec()),
        )
        .bind(entry.reverses().map(uuid_bytes))
        .bind(entry.original_booking_date())
        .bind(projected_root.as_bytes().as_slice())
        .bind(entry.kind().map(|k| k.as_str()))
        .fetch_optional(&mut **tx)
        .await?;

        let Some(row) = inserted else {
            let existing = sqlx::query(
                "SELECT log_index, entry_id, content_hash FROM entries WHERE idempotency_key = ?1",
            )
            .bind(entry.idempotency_key().as_bytes())
            .fetch_one(&mut **tx)
            .await?;

            let stored_hash = hash_from_bytes(&existing.try_get::<Vec<u8>, _>("content_hash")?)?;
            let stored_id = uuid_from_bytes(&existing.try_get::<Vec<u8>, _>("entry_id")?)?;
            let stored_index: i64 = existing.try_get("log_index")?;

            if stored_hash != content_hash {
                return Err(SqliteError::IdempotencyConflict {
                    existing: stored_id,
                });
            }
            return Ok(Recorded {
                id: stored_id,
                index: Some(LogIndex::new(u64::try_from(stored_index).unwrap_or(0))),
                content_hash,
                is_new: false,
            });
        };

        let assigned: i64 = row.try_get("log_index")?;
        for (position, posting) in entry.postings().iter().enumerate() {
            let position = i64::try_from(position).unwrap_or(i64::MAX);
            sqlx::query(
                "INSERT INTO postings ( \
                    entry_id, posting_index, account_index, direction, amount_minor, currency, \
                    layer \
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )
            .bind(uuid_bytes(entry.id()))
            .bind(position)
            .bind(i64::from(posting.account.index()))
            .bind(direction_str(posting.direction))
            .bind(posting.amount.to_minor())
            .bind(posting.currency.code())
            .bind(layer_str(posting.layer))
            .execute(&mut **tx)
            .await?;

            for (axis, value) in posting.dimensions.iter() {
                sqlx::query(
                    "INSERT INTO posting_dimensions (entry_id, posting_index, axis, value) \
                     VALUES (?1,?2,?3,?4)",
                )
                .bind(uuid_bytes(entry.id()))
                .bind(position)
                .bind(axis.as_str())
                .bind(value.as_str())
                .execute(&mut **tx)
                .await?;
            }
        }

        Self::check_limits(tx, entry).await?;

        *next_index = next_index.saturating_add(1);
        *accumulator = projected;
        Ok(Recorded {
            id: entry.id(),
            index: Some(LogIndex::new(u64::try_from(assigned).unwrap_or(0))),
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
    /// Deliberately not filtered on `log_index IS NOT NULL`: an unsequenced
    /// entry is durable, and money it has already committed counts against the
    /// limit whether or not the sequencer has placed it yet.
    async fn check_limits(
        tx: &mut Transaction<'_, Sqlite>,
        entry: &Entry<Balanced, P>,
    ) -> Result<(), SqliteError> {
        let mut checked: BTreeSet<(u32, Currency, Layer)> = BTreeSet::new();
        for posting in entry.postings() {
            if !checked.insert((posting.account.index(), posting.currency, posting.layer)) {
                continue;
            }
            let row = sqlx::query(
                "SELECT a.balance_limit AS lim, \
                    COALESCE(SUM(CASE WHEN p.direction = 'D' \
                                      THEN p.amount_minor ELSE -p.amount_minor END), 0) AS net \
                 FROM accounts a \
                 LEFT JOIN postings p \
                   ON p.account_index = a.account_index \
                  AND p.currency = ?2 AND p.layer = ?3 \
                 WHERE a.account_index = ?1 AND a.balance_limit <> 'unlimited' \
                 GROUP BY a.balance_limit",
            )
            .bind(i64::from(posting.account.index()))
            .bind(posting.currency.code())
            .bind(layer_str(posting.layer))
            .fetch_optional(&mut **tx)
            .await?;

            let Some(row) = row else { continue };
            let code: String = row.try_get("lim")?;
            let limit = limit_from_code(&code)
                .ok_or_else(|| SqliteError::malformed(format!("balance limit {code:?}")))?;
            let net: i64 = row.try_get("net")?;
            let permitted = match limit {
                BalanceLimit::Unlimited => true,
                BalanceLimit::NoCreditBalance => net >= 0,
                BalanceLimit::NoDebitBalance => net <= 0,
            };
            if !permitted {
                return Err(SqliteError::LimitBreached {
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

    /// Loads postings for several entries at once, grouped by log index.
    ///
    /// SQLite has no array binding, so the placeholder list is generated. The
    /// values are integers the caller just read from this database, so there is
    /// nothing to interpolate but digits.
    async fn load_postings_for(
        &self,
        ids: &[Vec<u8>],
    ) -> Result<BTreeMap<Vec<u8>, Vec<Posting<P>>>, SqliteError> {
        let mut grouped: BTreeMap<Vec<u8>, Vec<Posting<P>>> = BTreeMap::new();
        if ids.is_empty() {
            return Ok(grouped);
        }
        let placeholders = (1..=ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT entry_id, posting_index, account_index, direction, amount_minor, currency, \
             layer \
             FROM postings WHERE entry_id IN ({placeholders}) ORDER BY entry_id, posting_index"
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id.clone());
        }

        // One query for the axes too, rather than one per posting.
        let dim_sql = format!(
            "SELECT entry_id, posting_index, axis, value FROM posting_dimensions \
             WHERE entry_id IN ({placeholders}) ORDER BY entry_id, posting_index, axis"
        );
        let mut dim_query = sqlx::query(&dim_sql);
        for id in ids {
            dim_query = dim_query.bind(id.clone());
        }
        let dimensions = dimensions_from(&dim_query.fetch_all(&self.pool).await?)?;

        for row in &query.fetch_all(&self.pool).await? {
            let entry_id: Vec<u8> = row.try_get("entry_id")?;
            let posting_index: i64 = row.try_get("posting_index")?;
            let mut posting = build_posting::<P>(row)?;
            if let Some(dims) = dimensions.get(&(entry_id.clone(), posting_index)) {
                posting.dimensions = dims.clone();
            }
            grouped.entry(entry_id).or_default().push(posting);
        }
        Ok(grouped)
    }

    /// Folds every *sequenced* entry booked on or before `end`.
    ///
    /// The `log_index IS NOT NULL` predicate is what keeps a seal honest: the
    /// tree head it carries covers only sequenced entries, so a closing balance
    /// that folded in unsequenced ones would commit to money the tree head does
    /// not account for.
    async fn trial_balance_through_date(&self, end: Date) -> Result<TrialBalance<P>, SqliteError> {
        let rows = sqlx::query(
            "SELECT p.account_index, p.currency, p.layer, \
               COALESCE(SUM(CASE WHEN p.direction = 'D' THEN p.amount_minor END), 0) AS debits, \
               COALESCE(SUM(CASE WHEN p.direction = 'C' THEN p.amount_minor END), 0) AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE e.log_index IS NOT NULL AND e.booking_date <= ?1 \
             GROUP BY p.account_index, p.currency, p.layer \
             ORDER BY p.account_index, p.currency, p.layer",
        )
        .bind(end)
        .fetch_all(&self.pool)
        .await?;
        build_trial_balance::<P>(&rows)
    }
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

/// SQLite has no UUID type; identifiers are stored as their 16 raw bytes.
fn uuid_bytes(id: EntryId) -> Vec<u8> {
    id.as_uuid().as_bytes().to_vec()
}

fn clearing_bytes(id: ClearingId) -> Vec<u8> {
    id.as_uuid().as_bytes().to_vec()
}

fn uuid_from_bytes(bytes: &[u8]) -> Result<EntryId, SqliteError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| SqliteError::malformed("identifier is not 16 bytes"))?;
    Ok(EntryId::from_uuid(uuid::Uuid::from_bytes(array)))
}

fn hash_from_bytes(bytes: &[u8]) -> Result<Hash, SqliteError> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SqliteError::malformed("hash is not 32 bytes"))?;
    Ok(Hash::from_bytes(array))
}

/// The exclusive upper bound on `log_index` for a prefix of `size` entries.
///
/// `size` counts entries, so the entries in it are indices `0..size` — hence a
/// strict `<`. `None` means the whole log. Expressed as an exclusive bound
/// rather than `size - 1` so that an empty prefix needs no special case: it
/// binds zero, and nothing is strictly below zero.
fn prefix_bound(size: Option<u64>) -> i64 {
    size.map_or(i64::MAX, |n| i64::try_from(n).unwrap_or(i64::MAX))
}

fn build_posting<const P: u8>(row: &sqlx::sqlite::SqliteRow) -> Result<Posting<P>, SqliteError> {
    let account: i64 = row.try_get("account_index")?;
    let direction: String = row.try_get("direction")?;
    let amount: i64 = row.try_get("amount_minor")?;
    let currency: String = row.try_get("currency")?;
    let layer: String = row.try_get("layer")?;

    let direction = match direction.as_str() {
        "D" => Direction::Debit,
        "C" => Direction::Credit,
        other => return Err(SqliteError::malformed(format!("direction {other:?}"))),
    };
    let layer = match layer.as_str() {
        "settled" => Layer::Settled,
        "pending" => Layer::Pending,
        other => return Err(SqliteError::malformed(format!("layer {other:?}"))),
    };

    Ok(Posting {
        account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
        direction,
        amount: Amount::from_minor(amount),
        currency: Currency::new(currency.trim())
            .map_err(|_| SqliteError::malformed(format!("currency {currency:?}")))?,
        layer,
        dimensions: Dimensions::none(),
    })
}

/// `(entry_id, posting_index)` to the axes attached to that posting.
type DimensionIndex = BTreeMap<(Vec<u8>, i64), Dimensions>;

fn dimensions_from(rows: &[sqlx::sqlite::SqliteRow]) -> Result<DimensionIndex, SqliteError> {
    let mut out: DimensionIndex = BTreeMap::new();
    for row in rows {
        let entry_id: Vec<u8> = row.try_get("entry_id")?;
        let posting_index: i64 = row.try_get("posting_index")?;
        let axis: String = row.try_get("axis")?;
        let value: String = row.try_get("value")?;
        let slot = out.entry((entry_id, posting_index)).or_default();
        slot.set(
            Label::new(axis).map_err(|e| SqliteError::malformed(e.to_string()))?,
            Label::new(value).map_err(|e| SqliteError::malformed(e.to_string()))?,
        )
        .map_err(|e| SqliteError::malformed(e.to_string()))?;
    }
    Ok(out)
}

fn build_trial_balance<const P: u8>(
    rows: &[sqlx::sqlite::SqliteRow],
) -> Result<TrialBalance<P>, SqliteError> {
    let mut tb = TrialBalance::new();
    for row in rows {
        let account: i64 = row.try_get("account_index")?;
        let currency: String = row.try_get("currency")?;
        let layer: String = row.try_get("layer")?;
        tb.set(
            BalanceKey {
                account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
                currency: Currency::new(currency.trim())
                    .map_err(|_| SqliteError::malformed(format!("currency {currency:?}")))?,
                layer: match layer.as_str() {
                    "settled" => Layer::Settled,
                    "pending" => Layer::Pending,
                    other => return Err(SqliteError::malformed(format!("layer {other:?}"))),
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

fn build_stored_entry<const P: u8>(
    row: &sqlx::sqlite::SqliteRow,
    postings: Vec<Posting<P>>,
) -> Result<StoredEntry<P>, SqliteError> {
    let log_index: Option<i64> = row.try_get("log_index")?;
    let entry_id = uuid_from_bytes(&row.try_get::<Vec<u8>, _>("entry_id")?)?;
    let key: Vec<u8> = row.try_get("idempotency_key")?;
    let stored_hash = hash_from_bytes(&row.try_get::<Vec<u8>, _>("content_hash")?)?;
    let booking_date: Date = row.try_get("booking_date")?;
    let value_date: Date = row.try_get("value_date")?;
    let description: String = row.try_get("description")?;

    let mut provenance = Provenance::none();
    if let Some(v) = row.try_get::<Option<String>, _>("provenance_actor")? {
        provenance = provenance
            .with_actor(&v)
            .map_err(|e| SqliteError::malformed(e.to_string()))?;
    }
    if let Some(v) = row.try_get::<Option<String>, _>("provenance_source")? {
        provenance = provenance
            .with_source(&v)
            .map_err(|e| SqliteError::malformed(e.to_string()))?;
    }
    if let Some(v) = row.try_get::<Option<String>, _>("provenance_correlation")? {
        provenance = provenance
            .with_correlation(&v)
            .map_err(|e| SqliteError::malformed(e.to_string()))?;
    }

    let mut draft = Entry::<Draft, P>::new(
        entry_id,
        IdempotencyKey::new(key).map_err(|e| SqliteError::malformed(e.to_string()))?,
        booking_date,
    )
    .with_value_date(value_date)
    .with_description(
        Description::new(description).map_err(|e| SqliteError::malformed(e.to_string()))?,
    )
    .with_provenance(provenance);

    if let Some(kind) = row.try_get::<Option<String>, _>("kind")? {
        draft =
            draft.with_kind(Label::new(kind).map_err(|e| SqliteError::malformed(e.to_string()))?);
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
        draft = draft.with_document(document.map_err(|e| SqliteError::malformed(e.to_string()))?);
    }
    if let (Some(reverses), Some(original)) = (
        row.try_get::<Option<Vec<u8>>, _>("reverses")?,
        row.try_get::<Option<Date>, _>("original_booking_date")?,
    ) {
        draft = draft.reversing(uuid_from_bytes(&reverses)?, original);
    }
    for posting in postings {
        draft = draft.post(posting);
    }

    let entry = draft.adopt_verified(stored_hash)?;
    Ok(StoredEntry {
        index: log_index.map(|i| LogIndex::new(u64::try_from(i).unwrap_or(0))),
        entry,
        content_hash: stored_hash,
    })
}

const ENTRY_COLUMNS: &str = "entry_id, log_index, idempotency_key, content_hash, booking_date, \
     value_date, description, provenance_actor, provenance_source, provenance_correlation, \
     document_id, document_content_hash, reverses, original_booking_date, kind";

async fn load_postings_tx<const P: u8>(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: Vec<u8>,
) -> Result<Vec<Posting<P>>, SqliteError> {
    let rows = sqlx::query(
        "SELECT posting_index, account_index, direction, amount_minor, currency, layer \
         FROM postings WHERE entry_id = ?1 ORDER BY posting_index",
    )
    .bind(entry_id.clone())
    .fetch_all(&mut **tx)
    .await?;
    let dim_rows = sqlx::query(
        "SELECT entry_id, posting_index, axis, value FROM posting_dimensions \
         WHERE entry_id = ?1 ORDER BY posting_index, axis",
    )
    .bind(entry_id.clone())
    .fetch_all(&mut **tx)
    .await?;
    let dimensions = dimensions_from(&dim_rows)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let posting_index: i64 = row.try_get("posting_index")?;
        let mut posting = build_posting::<P>(row)?;
        if let Some(dims) = dimensions.get(&(entry_id.clone(), posting_index)) {
            posting.dimensions = dims.clone();
        }
        out.push(posting);
    }
    Ok(out)
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

async fn posting_facts<const P: u8>(
    tx: &mut Transaction<'_, Sqlite>,
    reference: PostingRef,
) -> Result<PostingFacts<P>, SqliteError> {
    let row = sqlx::query(
        "SELECT p.direction, p.account_index, p.currency, p.layer, \
           p.amount_minor - COALESCE(( \
               SELECT SUM(ci.applied_minor) FROM clearing_items ci \
               JOIN clearings c ON c.clearing_id = ci.clearing_id \
               WHERE ci.entry_id = p.entry_id AND ci.posting_index = p.posting_index \
                 AND c.reset_on IS NULL \
           ), 0) AS residual \
         FROM postings p WHERE p.entry_id = ?1 AND p.posting_index = ?2",
    )
    .bind(uuid_bytes(reference.entry))
    .bind(i64::from(reference.index))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ClearingError::UnknownPosting { posting: reference })?;

    let direction: String = row.try_get("direction")?;
    let account: i64 = row.try_get("account_index")?;
    let currency: String = row.try_get("currency")?;
    let layer: String = row.try_get("layer")?;
    let residual: i64 = row.try_get("residual")?;

    Ok(PostingFacts {
        direction: match direction.as_str() {
            "D" => Direction::Debit,
            _ => Direction::Credit,
        },
        account: AccountId::from_index(u32::try_from(account).unwrap_or(0)),
        currency: Currency::new(currency.trim())
            .map_err(|_| SqliteError::malformed(format!("currency {currency:?}")))?,
        layer: match layer.as_str() {
            "pending" => Layer::Pending,
            _ => Layer::Settled,
        },
        residual: Amount::from_minor(residual),
    })
}

impl<const P: u8> LedgerStore<P> for SqliteStore<P> {
    type Error = SqliteError;

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
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (account_index) DO UPDATE SET \
                    kind          = excluded.kind, \
                    opened_on     = excluded.opened_on, \
                    closed_on     = excluded.closed_on, \
                    balance_limit = excluded.balance_limit \
                 WHERE accounts.path = excluded.path",
            )
            .bind(i64::from(record.id.index()))
            .bind(record.account.path.to_string())
            .bind(record.account.kind.map(kind_code))
            .bind(record.account.opened_on)
            .bind(record.account.closed_on)
            .bind(limit_code(record.account.limit))
            .execute(&self.pool)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(SqliteError::AccountRebound { id: record.id });
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
        // BEGIN IMMEDIATE takes the write lock before the next index is read.
        // A deferred transaction would read first and upgrade later, which under
        // contention either fails or commits against a stale read.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let next: i64 = sqlx::query("SELECT COALESCE(MAX(log_index) + 1, 0) AS next FROM entries")
            .fetch_one(&mut *tx)
            .await?
            .try_get("next")?;
        let mut next_index = next;

        let mut accumulator = Self::accumulator(&mut tx).await?;
        let mut out = Vec::with_capacity(batch.len());
        for entry in batch.entries() {
            out.push(Self::append_one(&mut tx, entry, &mut next_index, &mut accumulator).await?);
        }
        Self::store_accumulator(&mut tx, &accumulator).await?;

        tx.commit().await?;
        Ok(out)
    }

    async fn get(&self, id: EntryId) -> Result<Option<StoredEntry<P>>, Self::Error> {
        let sql = format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE entry_id = ?1");
        let Some(row) = sqlx::query(&sql)
            .bind(uuid_bytes(id))
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let key = uuid_bytes(id);
        let mut grouped = self.load_postings_for(std::slice::from_ref(&key)).await?;
        Ok(Some(build_stored_entry::<P>(
            &row,
            grouped.remove(&key).unwrap_or_default(),
        )?))
    }

    async fn page(&self, cursor: Cursor) -> Result<Page<P>, Self::Error> {
        let after = cursor
            .after
            .map_or(-1i64, |i| i64::try_from(i.get()).unwrap_or(i64::MAX));
        let limit = i64::try_from(cursor.effective_limit()).unwrap_or(i64::MAX);

        let sql = format!(
            "SELECT {ENTRY_COLUMNS} FROM entries WHERE log_index > ?1 ORDER BY log_index LIMIT ?2"
        );
        let rows = sqlx::query(&sql)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        let ids: Vec<Vec<u8>> = rows
            .iter()
            .map(|r| r.try_get::<Vec<u8>, _>("entry_id"))
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
        let Some(row) = sqlx::query(
            "SELECT log_index, tree_root FROM entries \
             WHERE log_index IS NOT NULL ORDER BY log_index DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(TreeHead {
                size: 0,
                root: empty_root(),
            });
        };
        let log_index: i64 = row.try_get("log_index")?;
        Ok(TreeHead {
            size: u64::try_from(log_index).unwrap_or(0).saturating_add(1),
            root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("tree_root")?)?,
        })
    }

    async fn len(&self) -> Result<u64, Self::Error> {
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM entries WHERE log_index IS NOT NULL")
            .fetch_one(&self.pool)
            .await?
            .try_get("n")?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn balance(&self, key: BalanceKey, size: Option<u64>) -> Result<Balance<P>, Self::Error> {
        let row = sqlx::query(
            "SELECT \
               COALESCE(SUM(CASE WHEN direction = 'D' THEN amount_minor END), 0) AS debits, \
               COALESCE(SUM(CASE WHEN direction = 'C' THEN amount_minor END), 0) AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE p.account_index = ?1 AND p.currency = ?2 AND p.layer = ?3 \
               AND e.log_index IS NOT NULL AND e.log_index < ?4",
        )
        .bind(i64::from(key.account.index()))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .bind(prefix_bound(size))
        .fetch_one(&self.pool)
        .await?;

        Ok(Balance {
            debits: Amount::from_minor(row.try_get::<i64, _>("debits")?),
            credits: Amount::from_minor(row.try_get::<i64, _>("credits")?),
        })
    }

    async fn trial_balance(&self, size: Option<u64>) -> Result<TrialBalance<P>, Self::Error> {
        let rows = sqlx::query(
            "SELECT p.account_index, p.currency, p.layer, \
               COALESCE(SUM(CASE WHEN p.direction = 'D' THEN p.amount_minor END), 0) AS debits, \
               COALESCE(SUM(CASE WHEN p.direction = 'C' THEN p.amount_minor END), 0) AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE e.log_index IS NOT NULL AND e.log_index < ?1 \
             GROUP BY p.account_index, p.currency, p.layer \
             ORDER BY p.account_index, p.currency, p.layer",
        )
        .bind(prefix_bound(size))
        .fetch_all(&self.pool)
        .await?;
        build_trial_balance::<P>(&rows)
    }

    async fn prove_inclusion(&self, index: LogIndex) -> Result<InclusionProof, Self::Error> {
        Ok(self.log().await?.inclusion_proof(index.get())?)
    }

    async fn prove_consistency(&self, old_size: u64) -> Result<ConsistencyProof, Self::Error> {
        Ok(self.log().await?.consistency_proof(old_size)?)
    }

    async fn define_period(&self, period: &Period) -> Result<(), Self::Error> {
        // Non-overlap is not expressible in SQLite, so it is checked here
        // against everything already stored — the same rule `PeriodCalendar`
        // applies, run at the point of writing rather than trusted afterwards.
        let mut calendar = self.calendar().await?;
        calendar.ensure(period.clone())?;

        sqlx::query(
            "INSERT INTO periods (period_id, starts_on, ends_on, state) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (period_id) DO UPDATE SET \
                starts_on = excluded.starts_on, \
                ends_on   = excluded.ends_on, \
                state     = excluded.state",
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
        let mut calendar = self.calendar().await?;
        calendar.transition(period, to)?;
        sqlx::query("UPDATE periods SET state = ?2 WHERE period_id = ?1")
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
        rows.iter().map(period_row).collect()
    }

    async fn seal_period(&self, period: &PeriodId) -> Result<Seal, Self::Error> {
        let Some(definition) = self.periods().await?.into_iter().find(|p| p.id == *period) else {
            return Err(SqliteError::UnknownPeriod {
                period: period.clone(),
            });
        };
        if definition.state != PeriodState::Closing {
            return Err(SqliteError::PeriodNotClosing {
                period: period.clone(),
                state: definition.state,
            });
        }

        // Sequenced entries only. An entry with no position is not in the log
        // the tree head commits to, so counting it here would have the seal
        // claim coverage the head does not support.
        let span = sqlx::query(
            "SELECT MIN(log_index) AS first, MAX(log_index) AS last, COUNT(*) AS n \
             FROM entries \
             WHERE log_index IS NOT NULL AND booking_date BETWEEN ?1 AND ?2",
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
            .map_err(|e| SqliteError::malformed(e.to_string()))?;
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
                .map_err(|e: crate::account::AccountError| SqliteError::malformed(e.to_string()))?
                .commitment(),
            chain.head(),
        );

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO seals ( \
                period_id, first_index, last_index, entry_count, tree_size, tree_root, \
                trial_balance_root, accounts_root, prev_seal, seal_hash, chain_position \
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        )
        .bind(seal.period.as_str())
        .bind(seal.first_index.map(|v| i64::try_from(v).unwrap_or(0)))
        .bind(seal.last_index.map(|v| i64::try_from(v).unwrap_or(0)))
        .bind(i64::try_from(seal.entry_count).unwrap_or(0))
        .bind(i64::try_from(seal.tree_head.size).unwrap_or(0))
        .bind(seal.tree_head.root.as_bytes().as_slice())
        .bind(seal.trial_balance_root.as_bytes().as_slice())
        .bind(seal.accounts_root.as_bytes().as_slice())
        .bind(seal.prev_seal.map(|h| h.as_bytes().to_vec()))
        .bind(seal.seal_hash.as_bytes().as_slice())
        .bind(position)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE periods SET state = 'sealed' WHERE period_id = ?1")
            .bind(seal.period.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(seal)
    }

    async fn seals(&self) -> Result<Vec<Seal>, Self::Error> {
        let rows = sqlx::query(
            "SELECT period_id, first_index, last_index, entry_count, tree_size, tree_root, \
             trial_balance_root, accounts_root, prev_seal, seal_hash \
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
            let prev: Option<Vec<u8>> = row.try_get("prev_seal")?;
            out.push(Seal {
                ledger: self.ledger.clone(),
                period: PeriodId::new(period).map_err(|e| SqliteError::malformed(e.to_string()))?,
                first_index: first.map(|v| u64::try_from(v).unwrap_or(0)),
                last_index: last.map(|v| u64::try_from(v).unwrap_or(0)),
                entry_count: u64::try_from(count).unwrap_or(0),
                tree_head: TreeHead {
                    size: u64::try_from(size).unwrap_or(0),
                    root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("tree_root")?)?,
                },
                trial_balance_root: hash_from_bytes(
                    &row.try_get::<Vec<u8>, _>("trial_balance_root")?,
                )?,
                accounts_root: hash_from_bytes(&row.try_get::<Vec<u8>, _>("accounts_root")?)?,
                prev_seal: prev.as_deref().map(hash_from_bytes).transpose()?,
                seal_hash: hash_from_bytes(&row.try_get::<Vec<u8>, _>("seal_hash")?)?,
            });
        }
        Ok(out)
    }

    async fn clear(&self, clearing: Clearing<P>) -> Result<(), Self::Error> {
        if clearing.items.len() < 2 {
            return Err(ClearingError::TooFewItems {
                count: clearing.items.len(),
            }
            .into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &clearing.items {
            if !seen.insert(item.posting) {
                return Err(ClearingError::DuplicateItem {
                    posting: item.posting,
                }
                .into());
            }
            if !item.applied.is_positive() {
                return Err(ClearingError::NonPositiveApplication {
                    posting: item.posting,
                }
                .into());
            }
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // A repeated identifier is a caller mistake, not a database failure, so
        // it is named rather than surfacing as a primary-key violation.
        let taken: Option<i64> = sqlx::query("SELECT 1 AS x FROM clearings WHERE clearing_id = ?1")
            .bind(clearing_bytes(clearing.id))
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| row.try_get("x"))
            .transpose()?;
        if taken.is_some() {
            return Err(ClearingError::DuplicateId { id: clearing.id }.into());
        }

        let mut sides = Balance::<P>::ZERO;
        for item in &clearing.items {
            let facts = posting_facts::<P>(&mut tx, item.posting).await?;
            if facts.account != clearing.account {
                return Err(ClearingError::WrongAccount {
                    posting: item.posting,
                    expected: clearing.account,
                }
                .into());
            }
            if facts.currency != clearing.currency {
                return Err(ClearingError::WrongCurrency {
                    posting: item.posting,
                    expected: clearing.currency,
                }
                .into());
            }
            if facts.layer != clearing.layer {
                return Err(ClearingError::WrongLayer {
                    posting: item.posting,
                    expected: clearing.layer,
                }
                .into());
            }
            if item.applied > facts.residual {
                return Err(ClearingError::OverApplied {
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
            return Err(ClearingError::Unbalanced {
                debits_minor: sides.debits.to_minor(),
                credits_minor: sides.credits.to_minor(),
                scale: P,
            }
            .into());
        }

        sqlx::query(
            "INSERT INTO clearings (clearing_id, account_index, currency, layer, cleared_on) \
             VALUES (?1,?2,?3,?4,?5)",
        )
        .bind(clearing_bytes(clearing.id))
        .bind(i64::from(clearing.account.index()))
        .bind(clearing.currency.code())
        .bind(layer_str(clearing.layer))
        .bind(clearing.cleared_on)
        .execute(&mut *tx)
        .await?;

        for item in &clearing.items {
            sqlx::query(
                "INSERT INTO clearing_items (clearing_id, entry_id, posting_index, applied_minor) \
                 VALUES (?1,?2,?3,?4)",
            )
            .bind(clearing_bytes(clearing.id))
            .bind(uuid_bytes(item.posting.entry))
            .bind(i64::from(item.posting.index))
            .bind(item.applied.to_minor())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn reset_clearing(&self, id: ClearingId, on: Date) -> Result<(), Self::Error> {
        let result = sqlx::query(
            "UPDATE clearings SET reset_on = ?2 WHERE clearing_id = ?1 AND reset_on IS NULL",
        )
        .bind(clearing_bytes(id))
        .bind(on)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(SqliteError::ClearingNotResettable { id });
        }
        Ok(())
    }

    async fn open_items(&self, key: BalanceKey) -> Result<Vec<OpenItem<P>>, Self::Error> {
        let rows = sqlx::query(
            "SELECT o.entry_id, o.posting_index, o.direction, o.original_minor, \
                    o.applied_minor, o.residual_minor \
             FROM open_items o \
             WHERE o.account_index = ?1 AND o.currency = ?2 AND o.layer = ?3 \
             ORDER BY o.entry_id, o.posting_index",
        )
        .bind(i64::from(key.account.index()))
        .bind(key.currency.code())
        .bind(layer_str(key.layer))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let entry_id = uuid_from_bytes(&row.try_get::<Vec<u8>, _>("entry_id")?)?;
            let posting_index: i64 = row.try_get("posting_index")?;
            let direction: String = row.try_get("direction")?;
            out.push(OpenItem {
                posting: PostingRef::new(entry_id, u16::try_from(posting_index).unwrap_or(0)),
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
    ) -> Result<BTreeMap<AccountId, Balance<P>>, Self::Error> {
        let mut out = BTreeMap::new();
        if accounts.is_empty() {
            return Ok(out);
        }
        // SQLite has no array binding, so the placeholder list is generated.
        // The values are account indices the caller already holds.
        let placeholders = (0..accounts.len())
            .map(|i| format!("?{}", i.saturating_add(4)))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT p.account_index, \
               COALESCE(SUM(CASE WHEN p.direction = 'D' THEN p.amount_minor END), 0) AS debits, \
               COALESCE(SUM(CASE WHEN p.direction = 'C' THEN p.amount_minor END), 0) AS credits \
             FROM postings p JOIN entries e ON e.entry_id = p.entry_id \
             WHERE p.currency = ?1 AND p.layer = ?2 \
               AND e.log_index IS NOT NULL AND e.log_index < ?3 \
               AND p.account_index IN ({placeholders}) \
             GROUP BY p.account_index"
        );
        let mut query = sqlx::query(&sql)
            .bind(currency.code())
            .bind(layer_str(layer))
            .bind(prefix_bound(size));
        for account in accounts {
            query = query.bind(i64::from(account.index()));
        }
        for row in &query.fetch_all(&self.pool).await? {
            let account: i64 = row.try_get("account_index")?;
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

        // The running balance needs everything strictly before the page, summed
        // in the database rather than by reading the account's whole history.
        // Nothing precedes the first page — reading the account's full balance
        // here would start every statement at its own closing figure.
        // `after` is an index, and the prefix that precedes the page is
        // everything up to and including it — one more entry than its index.
        let opening = match cursor.after {
            Some(after) => {
                self.balance(key, Some(after.get().saturating_add(1)))
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
             WHERE p.account_index = ?1 AND p.currency = ?2 AND p.layer = ?3 \
               AND e.log_index IS NOT NULL AND e.log_index > ?4 \
             ORDER BY e.log_index, p.posting_index LIMIT ?5",
        )
        .bind(i64::from(key.account.index()))
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
            let entry_id = uuid_from_bytes(&row.try_get::<Vec<u8>, _>("entry_id")?)?;
            let posting_index: i64 = row.try_get("posting_index")?;
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
                posting: PostingRef::new(entry_id, u16::try_from(posting_index).unwrap_or(0)),
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
             ) VALUES (?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT (account_index, currency, layer) DO UPDATE SET \
                debits_minor  = excluded.debits_minor, \
                credits_minor = excluded.credits_minor, \
                tree_size     = excluded.tree_size, \
                tree_root     = excluded.tree_root",
        )
        .bind(i64::from(checkpoint.key.account.index()))
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
             FROM checkpoints WHERE account_index = ?1 AND currency = ?2 AND layer = ?3",
        )
        .bind(i64::from(key.account.index()))
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

/// Rebuilds one period definition from its row.
fn period_row(row: &sqlx::sqlite::SqliteRow) -> Result<Period, SqliteError> {
    let id: String = row.try_get("period_id")?;
    let state: String = row.try_get("state")?;
    let mut period = Period::new(
        PeriodId::new(id).map_err(|e| SqliteError::malformed(e.to_string()))?,
        row.try_get("starts_on")?,
        row.try_get("ends_on")?,
    )
    .map_err(|e| SqliteError::malformed(e.to_string()))?;
    period.state = match state.as_str() {
        "open" => PeriodState::Open,
        "closing" => PeriodState::Closing,
        "sealed" => PeriodState::Sealed,
        other => return Err(SqliteError::malformed(format!("period state {other:?}"))),
    };
    Ok(period)
}

/// Rebuilds one handle-to-account binding from its row.
fn account_record(row: &sqlx::sqlite::SqliteRow) -> Result<AccountRecord, SqliteError> {
    let index: i64 = row.try_get("account_index")?;
    let path: String = row.try_get("path")?;
    let kind: Option<String> = row.try_get("kind")?;
    let mut account = Account::new(
        AccountPath::parse(&path).map_err(|e| SqliteError::malformed(e.to_string()))?,
        row.try_get("opened_on")?,
    );
    account.kind = kind.as_deref().and_then(kind_from_code);
    account.closed_on = row.try_get("closed_on")?;
    let limit: String = row.try_get("balance_limit")?;
    account.limit = limit_from_code(&limit)
        .ok_or_else(|| SqliteError::malformed(format!("balance limit {limit:?}")))?;
    Ok(AccountRecord {
        id: AccountId::from_index(u32::try_from(index).unwrap_or(0)),
        account,
    })
}
