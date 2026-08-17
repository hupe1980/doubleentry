//! Apache Iceberg cold tier.
//!
//! A sealed accounting period is finished: its entries will not change, and
//! keeping them in the operational database forever costs money and query time
//! for data nobody books against. This crate moves them into Iceberg — columnar,
//! on object storage, queryable from DataFusion, DuckDB, Trino or Spark — without
//! weakening what the seal promises.
//!
//! # Copy, verify, then tombstone
//!
//! Moving data out of hot storage *is* a deletion from hot storage, which sits
//! in tension with an append-only ledger. The tension is resolved by making it
//! explicit rather than by pretending otherwise:
//!
//! 1. The period is sealed, so its contents can no longer change.
//! 2. Its entries are written to Iceberg, and the Merkle root of *what was
//!    actually written* is computed from those entries.
//! 3. That root is compared against the seal. **A mismatch aborts and deletes
//!    nothing.**
//! 4. Only then *may* the operational store drop the rows — and this crate never
//!    does it for you, because retention is a decision with legal weight.
//!
//! The precise claim is that *content* is immutable and *location* is not.
//!
//! # What pruning costs
//!
//! Step 4 is optional, and it is not free. A store's inclusion and consistency
//! proofs are built from the leaves it holds, so removing a prefix renumbers
//! every leaf after it and the store can no longer build a proof for **any**
//! entry — not just the archived ones. The tree head is unaffected, since it is
//! read from the last row's stored root, so the two would disagree silently.
//!
//! The SQL backends therefore check that the log they read back is dense from
//! zero and refuse with a `LogNotDense` error naming the hole. Proofs over an
//! archived period come from the archive and its seal after that; the seal is in
//! the snapshot summary precisely so they can.
//!
//! Keeping the rows is the default for a reason. Compaction is worth doing on
//! its own — a queryable columnar mirror that any engine can read — and pruning
//! is a separate decision you make afterwards, if at all.
//!
//! # The seal travels with the data
//!
//! Each compaction commits one Iceberg snapshot whose summary carries the seal
//! hash, the tree root, and the period identifier. An auditor handed a table
//! name and a seal hash can verify the archive with off-the-shelf tooling and no
//! access to this crate — which is the point of putting the commitment in the
//! table metadata rather than in a sidecar only we can read.
//!
//! Iceberg's own guarantees compose with the ledger's: snapshots are immutable,
//! the snapshot log is append-only, and every write here is an `append`
//! operation, so a snapshot recording anything else is itself evidence.

use std::collections::HashMap;
use std::sync::Arc;

use crate::dimensions::Dimensions;
use crate::merkle::MerkleAccumulator;
use crate::storage::{Cursor, LedgerStore, StoredEntry};
use crate::{Direction, Hash, Layer, LogIndex, Seal};
use arrow_array::{ArrayRef, Date32Array, Int32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use iceberg::spec::{DataFileFormat, Schema as IcebergSchema};
use iceberg::table::Table;
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::writer::base_writer::data_file_writer::DataFileWriterBuilder;
use iceberg::writer::file_writer::ParquetWriterBuilder;
use iceberg::writer::file_writer::location_generator::{
    DefaultFileNameGenerator, DefaultLocationGenerator,
};
use iceberg::writer::file_writer::rolling_writer::RollingFileWriterBuilder;
use iceberg::writer::{IcebergWriter, IcebergWriterBuilder};
use iceberg::{Catalog, TableIdent};
use parquet::file::properties::WriterProperties;

/// Snapshot summary key carrying the period identifier.
pub const PROP_PERIOD: &str = "doubleentry.period";
/// Snapshot summary key carrying the seal hash, as lowercase hex.
pub const PROP_SEAL_HASH: &str = "doubleentry.seal_hash";
/// Snapshot summary key carrying the seal's Merkle tree root, as lowercase hex.
pub const PROP_TREE_ROOT: &str = "doubleentry.tree_root";
/// Snapshot summary key carrying the seal's trial-balance root, as lowercase hex.
pub const PROP_TRIAL_BALANCE_ROOT: &str = "doubleentry.trial_balance_root";
/// Snapshot summary key carrying the trial-balance row count.
///
/// The size half of the sealed head. A balance proof is checked against both
/// halves, so a reader working from the table alone needs this to check one.
pub const PROP_TRIAL_BALANCE_SIZE: &str = "doubleentry.trial_balance_size";
/// Snapshot summary key carrying the seal's account-bindings root, as lowercase hex.
///
/// The archived posting rows name their account by handle, exactly as the
/// trial-balance root does. This is what says which account each of those
/// integers was, so a reader working from the table alone can still tell.
pub const PROP_ACCOUNTS_ROOT: &str = "doubleentry.accounts_root";
/// Snapshot summary key carrying how many handles the registry had issued.
///
/// The size half of the sealed account-bindings head, needed for the same reason
/// as [`PROP_TRIAL_BALANCE_SIZE`].
pub const PROP_ACCOUNTS_SIZE: &str = "doubleentry.accounts_size";
/// Snapshot summary key carrying how many entries the period holds.
pub const PROP_ENTRY_COUNT: &str = "doubleentry.entry_count";
/// Snapshot summary key carrying the log size archived through, exclusive.
///
/// The next compaction starts here, which is what keeps successive snapshots
/// from overlapping.
pub const PROP_ARCHIVED_THROUGH: &str = "doubleentry.archived_through";
/// Snapshot summary key carrying the Merkle accumulator after this compaction.
///
/// `height:hex` pairs, comma separated, largest subtree first. Carrying it means
/// the next compaction can verify its own delta against the seal without
/// re-reading everything already archived.
pub const PROP_ACCUMULATOR: &str = "doubleentry.accumulator";

/// Failure compacting a period into Iceberg.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ColdTierError {
    /// Iceberg refused or failed the operation.
    #[error(transparent)]
    Iceberg(#[from] iceberg::Error),
    /// Arrow could not build a batch from the ledger's rows.
    #[error(transparent)]
    Arrow(#[from] arrow_schema::ArrowError),
    /// The store could not be read.
    #[error("reading the ledger failed: {0}")]
    Store(String),
    /// An entry in the period had no log position.
    #[error("entry {entry} is in the period but has not been sequenced")]
    NotSequenced {
        /// The offending entry.
        entry: String,
    },
    /// The archive is ahead of the seal being compacted.
    ///
    /// Compaction only ever moves forward; a seal older than what is already
    /// archived would mean re-writing history.
    #[error(
        "period {period} seals the log at size {seal_size}, \
         but the archive already holds {archived}"
    )]
    SealBehindArchive {
        /// The period being compacted.
        period: String,
        /// Log size the seal commits to.
        seal_size: u64,
        /// Log size already archived.
        archived: u64,
    },
    /// The archive's recorded state could not be read back.
    #[error("the archive's recorded state is malformed: {0}")]
    MalformedState(String),
    /// What was read does not match what the seal committed to.
    ///
    /// Nothing has been written and nothing should be deleted: the archive would
    /// not be a faithful copy.
    #[error(
        "archive does not match the seal for period {period}: \
         the seal committed to {expected}, the archive plus this delta hashes to {actual}"
    )]
    RootMismatch {
        /// The period being compacted.
        period: String,
        /// Root the seal committed to.
        expected: String,
        /// Root of what was collected.
        actual: String,
    },
}

/// The Arrow schema a compacted period is written in.
///
/// One row per **posting**, with the owning entry's fields repeated. Analytics
/// engines aggregate over postings, not entries, and a flat table is what they
/// are fastest at; the normalised shape belongs in the operational store.
#[must_use]
pub fn arrow_schema() -> ArrowSchema {
    ArrowSchema::new(vec![
        // Iceberg has no unsigned types. A log position is bounded by the
        // number of entries ever written, so i64 is not a constraint in practice.
        // The period whose compaction wrote this row. A natural partition key,
        // and the column that makes "what did we archive when" answerable.
        Field::new("period", DataType::Utf8, false),
        Field::new("log_index", DataType::Int64, false),
        Field::new("entry_id", DataType::Utf8, false),
        Field::new("content_hash", DataType::Utf8, false),
        // Lowercase hex: an idempotency key is arbitrary bytes, and it is part
        // of the content hash, so the archive cannot recompute that hash
        // without it.
        Field::new("idempotency_key", DataType::Utf8, false),
        Field::new("booking_date", DataType::Date32, false),
        Field::new("value_date", DataType::Date32, false),
        Field::new("description", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, true),
        // Iceberg's narrowest integer is 32-bit.
        Field::new("posting_index", DataType::Int32, false),
        Field::new("account_index", DataType::Int64, false),
        Field::new("direction", DataType::Utf8, false),
        Field::new("amount_minor", DataType::Int64, false),
        Field::new("currency", DataType::Utf8, false),
        Field::new("layer", DataType::Utf8, false),
        // Axis names are the caller's, so the axes cannot be columns. A JSON
        // object keeps the archive queryable — every engine that reads Iceberg
        // has JSON functions — without this crate guessing at a schema.
        Field::new("dimensions", DataType::Utf8, true),
        Field::new("provenance_actor", DataType::Utf8, true),
        Field::new("provenance_source", DataType::Utf8, true),
        Field::new("provenance_correlation", DataType::Utf8, true),
        Field::new("document_id", DataType::Utf8, true),
        Field::new("document_content_hash", DataType::Utf8, true),
        Field::new("reverses", DataType::Utf8, true),
        Field::new("original_booking_date", DataType::Date32, true),
    ])
}

/// Renders a posting's axes as a canonical JSON object.
///
/// Keys are already ordered and both keys and values are [`Label`]s — no control
/// characters, so the only escaping a strict JSON reader needs is for `"` and
/// `\`. Returns `None` for a posting with no axes, so the column reads as NULL
/// rather than as an empty object.
fn dimensions_json(dimensions: &Dimensions) -> Option<String> {
    if dimensions.is_empty() {
        return None;
    }
    let mut out = String::from("{");
    for (i, (axis, value)) in dimensions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_string(&mut out, axis.as_str());
        out.push(':');
        push_json_string(&mut out, value.as_str());
    }
    out.push('}');
    Some(out)
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// The Iceberg schema a compacted period is written in.
///
/// Derived from [`fn@arrow_schema`] with field ids assigned, which is what a table
/// needs and what Arrow alone does not carry. Pass this to `create_table`.
///
/// # Errors
///
/// Returns an error only if the Arrow schema above becomes unrepresentable in
/// Iceberg, which would be a bug in this crate rather than in the caller.
pub fn iceberg_schema() -> Result<IcebergSchema, ColdTierError> {
    Ok(iceberg::arrow::arrow_schema_to_schema_auto_assign_ids(
        &arrow_schema(),
    )?)
}

/// The Arrow schema carrying the field ids Iceberg assigned.
///
/// Batches must be written with this rather than with [`fn@arrow_schema`]: the
/// Parquet writer matches columns by field id, and a schema without them does
/// not line up with the table.
fn batch_schema() -> Result<ArrowSchema, ColdTierError> {
    Ok(iceberg::arrow::schema_to_arrow_schema(&iceberg_schema()?)?)
}

/// The Arrow `Date32` origin. Identical to the Unix epoch, so this is a named zero.
const DATE32_EPOCH: time::Date = time::macros::date!(1970 - 01 - 01);

fn date32(date: time::Date) -> i32 {
    date.to_julian_day()
        .saturating_sub(DATE32_EPOCH.to_julian_day())
}

/// What a compaction wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// The period compacted.
    pub period: String,
    /// Log position the archive now covers, exclusive.
    pub archived_through: u64,
    /// Entries written by *this* compaction, not the total archived.
    pub entries: u64,
    /// Postings written by this compaction.
    pub postings: u64,
    /// Root the archive reproduces, equal to the seal's tree root.
    pub verified_root: Hash,
    /// The snapshot this compaction produced, or `None` when there was nothing
    /// new to archive and no snapshot was committed.
    pub snapshot_id: Option<i64>,
}

/// Where a previous compaction left the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveState {
    /// Log size archived through, exclusive.
    through: u64,
    /// Merkle accumulator covering exactly that prefix.
    accumulator: MerkleAccumulator,
}

impl ArchiveState {
    fn empty() -> Self {
        Self {
            through: 0,
            accumulator: MerkleAccumulator::new(),
        }
    }
}

/// Renders an accumulator as `height:hex` pairs for the snapshot summary.
fn encode_accumulator(accumulator: &MerkleAccumulator) -> String {
    accumulator
        .subtrees()
        .iter()
        .map(|(height, node)| format!("{height}:{}", node.to_hex()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parses what [`encode_accumulator`] wrote.
fn decode_accumulator(raw: &str, size: u64) -> Result<MerkleAccumulator, ColdTierError> {
    if raw.is_empty() {
        return MerkleAccumulator::try_from_parts(Vec::new(), size)
            .map_err(|e| ColdTierError::MalformedState(e.to_string()));
    }
    let mut subtrees = Vec::new();
    for part in raw.split(',') {
        let (height, hex) = part.split_once(':').ok_or_else(|| {
            ColdTierError::MalformedState(format!("accumulator entry {part:?} has no height"))
        })?;
        let height: u8 = height.parse().map_err(|_| {
            ColdTierError::MalformedState(format!("accumulator height {height:?} is not a number"))
        })?;
        let node = Hash::parse_hex(hex)
            .map_err(|e| ColdTierError::MalformedState(format!("accumulator node: {e}")))?;
        subtrees.push((height, node));
    }
    MerkleAccumulator::try_from_parts(subtrees, size)
        .map_err(|e| ColdTierError::MalformedState(e.to_string()))
}

/// Writes sealed periods into an Iceberg table.
#[derive(Debug, Clone)]
pub struct ColdTier {
    table: TableIdent,
}

impl ColdTier {
    /// Targets an existing Iceberg table.
    #[must_use]
    pub fn new(table: TableIdent) -> Self {
        Self { table }
    }

    /// The table being written to.
    #[must_use]
    pub fn table(&self) -> &TableIdent {
        &self.table
    }

    /// Archives everything sealed but not yet stored, verified against the seal.
    ///
    /// Successive compactions do not overlap: each writes only the entries the
    /// archive does not already hold, picking up where the last snapshot left
    /// off. Re-archiving a prefix would double every row an analytics query
    /// counts, and re-reading it would make each compaction cost more than the
    /// last.
    ///
    /// Verification is likewise incremental. The previous snapshot carries the
    /// Merkle accumulator for the prefix already stored; the new entries are
    /// pushed onto it, and the resulting root must equal the seal's. That checks
    /// the whole archive — not just the delta — while reading only the delta,
    /// because an accumulator is a commitment to everything behind it.
    ///
    /// Nothing is written unless that check passes, so a ledger that does not
    /// reproduce its seal leaves the archive exactly as it was.
    ///
    /// # Errors
    ///
    /// Returns [`ColdTierError::RootMismatch`] when the archive plus the delta
    /// does not reproduce the seal, [`ColdTierError::SealBehindArchive`] when
    /// asked to move backwards, and passes through Iceberg, Arrow, and store
    /// errors.
    pub async fn compact<const P: u8, S, C>(
        &self,
        store: &S,
        seal: &Seal,
        catalog: &C,
    ) -> Result<Compaction, ColdTierError>
    where
        S: LedgerStore<P>,
        C: Catalog,
    {
        let table = catalog.load_table(&self.table).await?;
        let state = archive_state(&table)?;

        if seal.tree_head.size < state.through {
            return Err(ColdTierError::SealBehindArchive {
                period: seal.period.to_string(),
                seal_size: seal.tree_head.size,
                archived: state.through,
            });
        }

        let entries = self
            .collect(store, state.through, seal.tree_head.size)
            .await?;

        // Continue the accumulator the last compaction left behind. The root it
        // reaches commits to everything archived so far *and* this delta, so a
        // match verifies the whole archive while reading only what is new.
        let mut accumulator = state.accumulator.clone();
        for record in &entries {
            accumulator.push(record.content_hash);
        }
        let reached = accumulator.root();

        if reached != seal.tree_head.root {
            return Err(ColdTierError::RootMismatch {
                period: seal.period.to_string(),
                expected: seal.tree_head.root.to_hex(),
                actual: reached.to_hex(),
            });
        }

        // Nothing new: the seal is already covered, and an empty snapshot would
        // add a file and a commit that say nothing.
        if entries.is_empty() {
            return Ok(Compaction {
                period: seal.period.to_string(),
                archived_through: state.through,
                entries: 0,
                postings: 0,
                verified_root: reached,
                snapshot_id: None,
            });
        }

        let (batch, postings) = build_batch(&entries, seal)?;
        let snapshot_id = write_snapshot(&table, catalog, seal, batch, &accumulator).await?;

        Ok(Compaction {
            period: seal.period.to_string(),
            archived_through: seal.tree_head.size,
            entries: entries.len() as u64,
            postings,
            verified_root: reached,
            snapshot_id: Some(snapshot_id),
        })
    }

    /// Reads log positions `[from, to)`, in order.
    async fn collect<const P: u8, S: LedgerStore<P>>(
        &self,
        store: &S,
        from: u64,
        to: u64,
    ) -> Result<Vec<StoredEntry<P>>, ColdTierError> {
        if from >= to {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        // `Cursor::after` is exclusive, so resuming at `from` means starting
        // after `from - 1`.
        let mut cursor = Some(match from.checked_sub(1) {
            Some(previous) => Cursor::after(LogIndex::new(previous)),
            None => Cursor::start(),
        });

        while let Some(c) = cursor {
            let page = store
                .page(c)
                .await
                .map_err(|e| ColdTierError::Store(e.to_string()))?;
            if page.records.is_empty() {
                break;
            }
            let mut done = false;
            for record in page.records {
                let index = record.index.ok_or_else(|| ColdTierError::NotSequenced {
                    entry: record.entry.id().to_string(),
                })?;
                if index.get() >= to {
                    done = true;
                    break;
                }
                out.push(record);
            }
            if done {
                break;
            }
            cursor = page.next;
        }
        Ok(out)
    }
}

/// Reads where the last compaction left the archive.
///
/// A table with no snapshot has archived nothing; a snapshot without the
/// properties this crate writes is not one of ours, and is reported rather than
/// silently treated as empty — starting over would duplicate every row.
fn archive_state(table: &Table) -> Result<ArchiveState, ColdTierError> {
    let Some(snapshot) = table.metadata().current_snapshot() else {
        return Ok(ArchiveState::empty());
    };
    let summary = &snapshot.summary().additional_properties;

    let (Some(through), Some(raw)) = (
        summary.get(PROP_ARCHIVED_THROUGH),
        summary.get(PROP_ACCUMULATOR),
    ) else {
        return Err(ColdTierError::MalformedState(format!(
            "snapshot {} carries no archive state; \
             the table holds data this crate did not write",
            snapshot.snapshot_id()
        )));
    };

    let through: u64 = through.parse().map_err(|_| {
        ColdTierError::MalformedState(format!("archived_through {through:?} is not a number"))
    })?;
    Ok(ArchiveState {
        through,
        accumulator: decode_accumulator(raw, through)?,
    })
}

/// Writes one Parquet file and commits it as an append snapshot.
async fn write_snapshot<C: Catalog>(
    table: &Table,
    catalog: &C,
    seal: &Seal,
    batch: RecordBatch,
    accumulator: &MerkleAccumulator,
) -> Result<i64, ColdTierError> {
    let location_generator = DefaultLocationGenerator::new(table.metadata())?;
    let file_name_generator =
        DefaultFileNameGenerator::new(seal.period.to_string(), None, DataFileFormat::Parquet);
    let parquet_writer_builder = ParquetWriterBuilder::new(
        WriterProperties::default(),
        table.metadata().current_schema().clone(),
    );
    let rolling = RollingFileWriterBuilder::new_with_default_file_size(
        parquet_writer_builder,
        table.file_io().clone(),
        location_generator,
        file_name_generator,
    );
    let mut writer = DataFileWriterBuilder::new(rolling).build(None).await?;
    writer.write(batch).await?;
    let data_files = writer.close().await?;

    // The commitment travels with the data: an auditor holding the table and a
    // seal hash can check the archive without this crate.
    let properties = HashMap::from([
        (PROP_PERIOD.to_owned(), seal.period.to_string()),
        (PROP_SEAL_HASH.to_owned(), seal.seal_hash.to_hex()),
        (PROP_TREE_ROOT.to_owned(), seal.tree_head.root.to_hex()),
        (
            PROP_TRIAL_BALANCE_ROOT.to_owned(),
            seal.trial_balance.root.to_hex(),
        ),
        (
            PROP_TRIAL_BALANCE_SIZE.to_owned(),
            seal.trial_balance.size.to_string(),
        ),
        // The archived rows name accounts by handle; this is what those handles
        // meant when the period was sealed.
        (PROP_ACCOUNTS_ROOT.to_owned(), seal.accounts.root.to_hex()),
        (
            PROP_ACCOUNTS_SIZE.to_owned(),
            seal.accounts.size.to_string(),
        ),
        (PROP_ENTRY_COUNT.to_owned(), seal.entry_count.to_string()),
        // Where the next compaction resumes, and the commitment it resumes
        // from. Without these the archive could only be extended by re-reading
        // everything it already holds.
        (
            PROP_ARCHIVED_THROUGH.to_owned(),
            seal.tree_head.size.to_string(),
        ),
        (PROP_ACCUMULATOR.to_owned(), encode_accumulator(accumulator)),
    ]);

    let tx = Transaction::new(table);
    let action = tx
        .fast_append()
        .add_data_files(data_files)
        .set_snapshot_properties(properties);
    let committed = action.apply(tx)?.commit(catalog).await?;

    Ok(committed
        .metadata()
        .current_snapshot()
        .map_or(0, |s| s.snapshot_id()))
}

/// Flattens entries into one Arrow batch, one row per posting.
fn build_batch<const P: u8>(
    entries: &[StoredEntry<P>],
    seal: &Seal,
) -> Result<(RecordBatch, u64), ColdTierError> {
    let mut period = Vec::new();
    let mut log_index = Vec::new();
    let mut entry_id = Vec::new();
    let mut content_hash = Vec::new();
    let mut idempotency_key = Vec::new();
    let mut booking_date = Vec::new();
    let mut value_date = Vec::new();
    let mut description = Vec::new();
    let mut kind: Vec<Option<String>> = Vec::new();
    let mut posting_index = Vec::new();
    let mut account_index = Vec::new();
    let mut direction = Vec::new();
    let mut amount_minor = Vec::new();
    let mut currency = Vec::new();
    let mut layer = Vec::new();
    let mut dimensions: Vec<Option<String>> = Vec::new();
    let mut actor: Vec<Option<String>> = Vec::new();
    let mut source: Vec<Option<String>> = Vec::new();
    let mut correlation: Vec<Option<String>> = Vec::new();
    let mut document_id: Vec<Option<String>> = Vec::new();
    let mut document_hash: Vec<Option<String>> = Vec::new();
    let mut reverses: Vec<Option<String>> = Vec::new();
    let mut original_booking_date: Vec<Option<i32>> = Vec::new();

    for record in entries {
        let index: LogIndex = record.index.ok_or_else(|| ColdTierError::NotSequenced {
            entry: record.entry.id().to_string(),
        })?;
        let entry = &record.entry;
        for (position, posting) in entry.postings().iter().enumerate() {
            period.push(seal.period.to_string());
            log_index.push(i64::try_from(index.get()).unwrap_or(i64::MAX));
            entry_id.push(entry.id().to_string());
            content_hash.push(record.content_hash.to_hex());
            idempotency_key.push(entry.idempotency_key().to_hex());
            booking_date.push(date32(entry.booking_date()));
            value_date.push(date32(entry.value_date()));
            description.push(entry.description().as_str().to_owned());
            kind.push(entry.kind().map(ToString::to_string));
            posting_index.push(i32::try_from(position).unwrap_or(i32::MAX));
            account_index.push(i64::from(posting.account.index()));
            direction.push(
                match posting.direction {
                    Direction::Debit => "D",
                    Direction::Credit => "C",
                }
                .to_owned(),
            );
            amount_minor.push(posting.amount.to_minor());
            currency.push(posting.currency.code().to_owned());
            layer.push(
                match posting.layer {
                    Layer::Settled => "settled",
                    Layer::Pending => "pending",
                }
                .to_owned(),
            );
            dimensions.push(dimensions_json(&posting.dimensions));
            actor.push(entry.provenance().actor.as_ref().map(ToString::to_string));
            source.push(entry.provenance().source.as_ref().map(ToString::to_string));
            correlation.push(
                entry
                    .provenance()
                    .correlation
                    .as_ref()
                    .map(ToString::to_string),
            );
            document_id.push(entry.document().map(|d| d.id.to_string()));
            document_hash.push(
                entry
                    .document()
                    .and_then(|d| d.content_hash.as_ref())
                    .map(Hash::to_hex),
            );
            reverses.push(entry.reverses().map(|r| r.to_string()));
            original_booking_date.push(entry.original_booking_date().map(date32));
        }
    }

    let postings = log_index.len() as u64;
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from(period)),
        Arc::new(Int64Array::from(log_index)),
        Arc::new(StringArray::from(entry_id)),
        Arc::new(StringArray::from(content_hash)),
        Arc::new(StringArray::from(idempotency_key)),
        Arc::new(Date32Array::from(booking_date)),
        Arc::new(Date32Array::from(value_date)),
        Arc::new(StringArray::from(description)),
        Arc::new(StringArray::from(kind)),
        Arc::new(Int32Array::from(posting_index)),
        Arc::new(Int64Array::from(account_index)),
        Arc::new(StringArray::from(direction)),
        Arc::new(Int64Array::from(amount_minor)),
        Arc::new(StringArray::from(currency)),
        Arc::new(StringArray::from(layer)),
        Arc::new(StringArray::from(dimensions)),
        Arc::new(StringArray::from(actor)),
        Arc::new(StringArray::from(source)),
        Arc::new(StringArray::from(correlation)),
        Arc::new(StringArray::from(document_id)),
        Arc::new(StringArray::from(document_hash)),
        Arc::new(StringArray::from(reverses)),
        Arc::new(Date32Array::from(original_booking_date)),
    ];

    let batch = RecordBatch::try_new(Arc::new(batch_schema()?), columns)?;
    Ok((batch, postings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_has_a_column_for_every_field_written() {
        // The batch builder and the schema must agree, or `try_new` fails at run
        // time on data already read out of the ledger.
        assert_eq!(arrow_schema().fields().len(), 23);
    }

    #[test]
    fn the_archive_carries_every_field_the_content_hash_covers() {
        // The claim in this module's docs is that an auditor can verify the
        // archive without this crate. That only holds if every field the entry
        // hash is computed over is actually in the table.
        let schema = arrow_schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        for required in [
            "idempotency_key",
            "booking_date",
            "value_date",
            "description",
            "kind",
            "posting_index",
            "account_index",
            "direction",
            "amount_minor",
            "currency",
            "layer",
            "dimensions",
            "provenance_actor",
            "provenance_source",
            "provenance_correlation",
            "document_id",
            "document_content_hash",
            "reverses",
            "original_booking_date",
        ] {
            assert!(names.contains(&required), "{required} is not archived");
        }
    }

    #[test]
    fn dimensions_render_as_a_canonical_json_object() {
        use crate::Label;
        assert_eq!(dimensions_json(&Dimensions::none()), None);

        let dims = Dimensions::none()
            .with(
                Label::new("segment").expect("valid"),
                Label::new("Electricity").expect("valid"),
            )
            .expect("fits")
            .with(
                Label::new("activity").expect("valid"),
                Label::new("Network").expect("valid"),
            )
            .expect("fits");
        // Axis order, not insertion order.
        assert_eq!(
            dimensions_json(&dims).as_deref(),
            Some(r#"{"activity":"Network","segment":"Electricity"}"#)
        );

        let quoted = Dimensions::none()
            .with(
                Label::new("note").expect("valid"),
                Label::new(r#"a"b\c"#).expect("valid"),
            )
            .expect("fits");
        assert_eq!(
            dimensions_json(&quoted).as_deref(),
            Some(r#"{"note":"a\"b\\c"}"#)
        );
    }

    #[test]
    fn dates_convert_to_the_arrow_epoch() {
        assert_eq!(date32(time::macros::date!(1970 - 01 - 01)), 0);
        assert_eq!(date32(time::macros::date!(1970 - 01 - 02)), 1);
        assert_eq!(date32(time::macros::date!(1969 - 12 - 31)), -1);
        assert_eq!(date32(time::macros::date!(2026 - 03 - 15)), 20_527);
    }
}
