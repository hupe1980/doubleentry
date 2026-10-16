//! The write side: `create`, `open`, the write pipeline, what a write set
//! persists and memo erasure.

use std::collections::BTreeMap;

use super::codec::{self, Row};
use super::dialect::{Kind, Mode, Statement, Value, Write};
use super::statements::Catalogue;
use super::{Dialect, Session, SqlStore, facts, schema};
use crate::core::{
    AccountTerm, Done, Header, HoldEffect, IndexRow, Op, Outcome, PeriodView, PostingRow,
    SealReceipt, Snapshot, WriteSet, execute, plan,
};
use crate::error::{IntegrityKind, Rejection};
use crate::log::nodes::AppendPlan;
use crate::log::{BalanceTree, Hash, leaf_hash};
use crate::model::{Amount, Balance, EntryId, Genesis, Layer, LedgerId, Record};
use crate::store::{Erasure, LedgerInfo, SCHEMA_ID, StoreError, plan_did_not_converge};

/// A failure of the pipeline's own checks: stored data or a write set the
/// store cannot trust.
fn integrity(what: &'static str, position: Option<u64>) -> StoreError {
    StoreError::integrity_of("write", IntegrityKind::ReplayMismatch, what, position)
}

/// Creates the schema and writes the genesis record, or accepts an
/// identical existing ledger.
pub(crate) async fn create<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    genesis: &Genesis,
) -> Result<LedgerInfo, StoreError> {
    let mut session = store.session(Mode::Write, "create").await?;
    let sql = session.sql;
    session
        .lock(store.create_key())
        .await
        .map_err(|error| error.unwrap_or_else(|| StoreError::unavailable("create")))?;
    let probe = Statement::render(D::META_PROBE, "", &[Kind::Int]);
    let meta = vec![codec::text(&format!("{}meta", sql.qualifier))];
    let exists = match session.first(&probe, meta).await? {
        Some(row) => row.int(0)? > 0,
        None => false,
    };
    let leaf = Record::<P>::Genesis(genesis.clone()).to_leaf_bytes();
    if exists {
        let info = existing::<D, P>(&mut session).await?;
        let stored = stored_leaf(&mut session, 0).await?;
        session.rollback().await?;
        return if stored.as_deref() == Some(leaf.as_slice()) {
            Ok(info)
        } else {
            Err(StoreError::ledger_mismatch("create"))
        };
    }
    let mut ddl = D::PRELUDE.replace("{{q}}", sql.qualifier.trim_end_matches('.'));
    ddl.push_str(&schema::tables(&sql.qualifier, D::TOKENS));
    ddl.push_str(&schema::triggers(&sql.qualifier, D::TRIGGERS));
    session.script(ddl).await?;
    let header = Header {
        ledger: genesis.ledger.clone(),
        size: 0,
        accounts: 0,
        entries: 0,
        watermark: None,
        last_booking: None,
        last_period: None,
    };
    let write = match decide::<D, P>(&mut session, header.clone(), &Op::Genesis).await? {
        Ok(Outcome::Append(write)) => write,
        _ => return Err(integrity("the genesis was not written", Some(0))),
    };
    persist(&mut session, &header, &write, &mut Known::default()).await?;
    let genesis_hash = leaf_hash(&write.leaf);
    session
        .execute(
            &sql.shared.insert_meta,
            vec![
                codec::text(SCHEMA_ID),
                codec::text(genesis.ledger.as_str()),
                Value::Int(i64::from(P)),
                codec::bytes(genesis_hash.as_bytes()),
            ],
        )
        .await?;
    session.commit().await?;
    store.remember(genesis.ledger.clone());
    Ok(LedgerInfo {
        ledger: genesis.ledger.clone(),
        precision: P,
        size: 1,
        genesis: genesis_hash,
    })
}

/// The leaf bytes at `position`, if stored.
async fn stored_leaf<D: Dialect>(
    session: &mut Session<'_, D>,
    position: u64,
) -> Result<Option<Vec<u8>>, StoreError> {
    let sql = session.sql;
    match session
        .first(&sql.shared.leaf_bytes, vec![codec::int(position)?])
        .await?
    {
        Some(mut row) => Ok(Some(row.take_bytes(0)?)),
        None => Ok(None),
    }
}

/// The stored ledger's identity, checked against this schema and scale
/// before anything else is read.
async fn existing<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
) -> Result<LedgerInfo, StoreError> {
    let sql = session.sql;
    let operation = session.operation;
    let meta = session
        .first(&sql.shared.meta, Vec::new())
        .await?
        .ok_or_else(|| StoreError::not_created(operation))?;
    let found = meta.text(0)?;
    if found != SCHEMA_ID {
        return Err(StoreError::schema_mismatch(operation, found.to_owned()));
    }
    if meta.int(2)? != i64::from(P) {
        return Err(StoreError::ledger_mismatch(operation));
    }
    let ledger = LedgerId::new(meta.text(1)?)
        .map_err(|_| StoreError::integrity(operation, "the ledger id is invalid", None))?;
    let genesis = meta.hash(3)?;
    let leaf = stored_leaf(session, 0).await?;
    if leaf.as_deref().map(leaf_hash) != Some(genesis) {
        return Err(StoreError::integrity_of(
            operation,
            IntegrityKind::GenesisMismatch,
            "the genesis record does not match the meta row",
            Some(0),
        ));
    }
    Ok(LedgerInfo {
        ledger,
        precision: P,
        size: session.size().await?,
        genesis,
    })
}

/// Opens without DDL.
pub(crate) async fn open<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
) -> Result<LedgerInfo, StoreError> {
    let mut session = store.session(Mode::Read, "open").await?;
    let info = existing::<D, P>(&mut session).await?;
    session.commit().await?;
    store.remember(info.ledger.clone());
    Ok(info)
}

/// The write pipeline: lock, header, plan loop, execute, persist, commit.
pub(crate) async fn write<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    op: &Op<P>,
) -> Result<Result<Done<P>, Rejection>, StoreError> {
    // A lock that failed on a lost connection ran nothing of the write: it
    // is taken once more on a fresh transaction.
    let mut retried = false;
    let (mut session, ledger) = loop {
        let mut session = store.session(Mode::Write, "write").await?;
        let ledger = store.ledger(&mut session).await?;
        match session.lock(ledger.key).await {
            Ok(()) => break (session, ledger),
            Err(None) if !retried => retried = true,
            Err(error) => {
                return Err(error.unwrap_or_else(|| StoreError::unavailable("write")));
            }
        }
    };
    let header = header::<D, P>(&mut session, ledger.id).await?;
    #[cfg(feature = "tracing")]
    let span = tracing::debug_span!(
        "write",
        ledger = %header.ledger,
        position = header.size,
        kind = tracing::field::Empty,
        rounds = tracing::field::Empty,
        reads = tracing::field::Empty,
        rows = tracing::field::Empty,
    );
    let (decision, _rounds, _reads) = plan_loop::<D, P>(&mut session, header.clone(), op).await?;
    #[cfg(feature = "tracing")]
    {
        span.record("rounds", _rounds);
        span.record("reads", _reads);
        span.record("rows", session.rows);
    }
    match decision {
        Err(rejection) => {
            session.rollback().await?;
            Ok(Err(rejection))
        }
        Ok(Outcome::Unchanged(done)) => {
            session.rollback().await?;
            Ok(Ok(done))
        }
        Ok(Outcome::Append(write)) => {
            #[cfg(feature = "tracing")]
            span.record("kind", tracing::field::debug(write.record.kind()));
            persist(&mut session, &header, &write, &mut Known::default()).await?;
            session.commit().await?;
            Ok(Ok(write.done))
        }
        Ok(Outcome::Batch { writes, done }) => {
            // Each write set persisted against the header its predecessors
            // leave, all in this one transaction.
            let (mut header, mut known) = (header, Known::default());
            for write in &writes {
                persist(&mut session, &header, write, &mut known).await?;
                header.size = header.size.saturating_add(1);
                header.entries = header.entries.saturating_add(1);
            }
            session.commit().await?;
            Ok(Ok(done))
        }
    }
}

/// The write header (counts, watermark, last booking date, last period),
/// read under the lock in one round trip with the meta row.
async fn header<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    ledger: LedgerId,
) -> Result<Header<P>, StoreError> {
    let sql = session.sql;
    let row = session
        .first(&sql.shared.header, Vec::new())
        .await?
        .ok_or_else(|| integrity("the header is unreadable", None))?;
    // The ledger this store was opened on is still the one stored: a
    // database re-created or restored under a running process is refused,
    // never written with the old ledger id.
    if row.is_null(12)? {
        return Err(StoreError::not_created("write"));
    }
    let schema = row.text(12)?;
    if schema != SCHEMA_ID {
        return Err(StoreError::schema_mismatch("write", schema.to_owned()));
    }
    if row.text(13)? != ledger.as_str() || row.int(14)? != i64::from(P) {
        return Err(StoreError::ledger_mismatch("write"));
    }
    let accounts = u32::try_from(row.u64(1)?).map_err(|_| codec::out_of_range("write"))?;
    Ok(Header {
        ledger,
        size: row.u64(0)?,
        accounts,
        entries: row.u64(2)?,
        watermark: row.opt_date(3)?,
        last_booking: row.opt_date(4)?,
        last_period: if row.is_null(5)? {
            None
        } else {
            Some(period_view(&row, 5)?)
        },
    })
}

/// A period row from column `at` on (id, first day, last day, position,
/// seal position, seal kind, seal bytes) as the core's view.
pub(crate) fn period_view<const P: u8>(row: &Row, at: usize) -> Result<PeriodView<P>, StoreError> {
    let column = |offset: usize| at.saturating_add(offset);
    let id = row.period(column(0))?;
    let sealed = match row.opt_u64(column(4))? {
        Some(position) => {
            let leaf = crate::store::Leaf {
                position,
                kind: row.record_kind(column(5))?,
                bytes: row.bytes(column(6))?.to_vec(),
            };
            match leaf.record::<P>()? {
                Record::Seal(seal) => Some(SealReceipt {
                    period: id.clone(),
                    position,
                    seal,
                }),
                _ => return Err(integrity("a seal row names another record", Some(position))),
            }
        }
        None => None,
    };
    Ok(PeriodView {
        id,
        start: row.date(column(1))?,
        end: row.date(column(2))?,
        position: row.u64(column(3))?,
        sealed,
    })
}

/// Decides `op` on a snapshot of `header`, loading the facts the core asks
/// for. Returns the decision, the rounds and the facts loaded.
async fn plan_loop<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    header: Header<P>,
    op: &Op<P>,
) -> Result<(Result<Outcome<P>, Rejection>, usize, usize), StoreError> {
    let ledger = header.ledger.clone();
    let mut snapshot = Snapshot::new(header);
    let mut loaded = 0usize;
    let rounds = op.max_rounds();
    for round in 0..=rounds {
        let reads = plan(&snapshot, op);
        if reads.is_empty() {
            return Ok((execute(&snapshot, op), round, loaded));
        }
        if round == rounds {
            break;
        }
        loaded = loaded.saturating_add(reads.len());
        for fact in facts::load(session, &ledger, reads).await? {
            snapshot.insert(fact);
        }
    }
    Err(plan_did_not_converge())
}

/// The plan loop without its counts.
async fn decide<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    header: Header<P>,
    op: &Op<P>,
) -> Result<Result<Outcome<P>, Rejection>, StoreError> {
    Ok(plan_loop(session, header, op).await?.0)
}

/// What the write sets of one transaction already read or wrote: the nodes
/// and the Settled slices, so the write sets of a batch read each once.
#[derive(Default)]
struct Known<const P: u8> {
    nodes: BTreeMap<u64, Hash>,
    slices: BTreeMap<(u32, String, String), Balance<P>>,
}

/// Persists a write set after checking its position: the reads it needs
/// first (frontier nodes, cleared amounts, slices), then every row in one
/// [`apply`](Session::apply) — one statement on PostgreSQL — and last the
/// clearing references, which read the postings they name.
async fn persist<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    header: &Header<P>,
    write: &WriteSet<P>,
    known: &mut Known<P>,
) -> Result<(), StoreError> {
    let sql = session.sql;
    let position = write.position;
    let mut writes = Vec::new();
    if position != header.size {
        return Err(integrity(
            "the write set's position is not the size",
            Some(position),
        ));
    }
    let at = codec::int(position)?;
    let kind = write
        .leaf
        .first()
        .copied()
        .ok_or_else(|| integrity("the write set has no leaf", Some(position)))?;
    writes.push(Write::Row(
        &sql.shared.insert_leaf,
        vec![
            at.clone(),
            Value::Int(i64::from(kind)),
            codec::bytes(&write.leaf),
        ],
    ));
    let append =
        AppendPlan::new(position).map_err(|_| integrity("the log is full", Some(position)))?;
    let siblings = nodes(session, append.ids(), &mut known.nodes).await?;
    let completed = append
        .assemble(leaf_hash(&write.leaf), &siblings)
        .map_err(|_| integrity("a frontier node is missing", Some(position)))?;
    known.nodes.extend(completed.iter().copied());
    let rows = completed
        .iter()
        .map(|(id, hash)| Ok(vec![codec::int(*id)?, codec::bytes(hash.as_bytes())]))
        .collect::<Result<Vec<_>, StoreError>>()?;
    writes.push(Write::Rows(&sql.inserts.nodes, rows));
    let refs = index(sql, header, write, &mut writes)?;
    postings(sql, position, &write.postings, &mut writes)?;
    if let Some(memo) = &write.memo {
        writes.push(Write::Row(
            &sql.shared.insert_memo,
            vec![at, codec::bytes(&memo.salt.0), codec::text(&memo.text)],
        ));
    }
    match &write.hold {
        Some(HoldEffect::Opened(hold)) => writes.push(Write::Row(
            &sql.shared.insert_hold,
            vec![
                codec::int(hold.position)?,
                codec::bytes(hold.id.as_bytes()),
                codec::date(hold.booking),
                codec::opt_date(hold.expires),
                codec::handle(hold.debit().handle),
                codec::handle(hold.credit().handle),
                codec::text(hold.currency().as_str()),
                codec::amount(hold.amount()),
            ],
        )),
        Some(HoldEffect::Resolved { hold, by, how }) => writes.push(Write::Row(
            &sql.shared.resolve_hold,
            vec![
                codec::bytes(hold.as_bytes()),
                codec::bytes(by.as_bytes()),
                codec::how(*how),
            ],
        )),
        None => {}
    }
    maintain(session, write, &mut writes, &mut known.slices).await?;
    session.apply(writes).await?;
    for args in refs {
        session
            .execute(&sql.shared.insert_clearing_ref, args)
            .await?;
    }
    Ok(())
}

/// The stored nodes `ids`, in order: those `known` holds, the rest read in
/// one round trip (and then known).
async fn nodes<D: Dialect>(
    session: &mut Session<'_, D>,
    ids: &[u64],
    known: &mut BTreeMap<u64, Hash>,
) -> Result<Vec<Hash>, StoreError> {
    let sql = session.sql;
    let unknown: Vec<u64> = ids
        .iter()
        .copied()
        .filter(|id| !known.contains_key(id))
        .collect();
    let keys = unknown
        .iter()
        .map(|id| Ok(vec![codec::int(*id)?]))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let operation = session.operation;
    let missing = || {
        StoreError::integrity_of(
            operation,
            IntegrityKind::NodeMismatch,
            "a stored node is missing",
            None,
        )
    };
    let found = session.keyed(&sql.keyed.nodes, keys).await?;
    for (id, rows) in unknown.into_iter().zip(&found) {
        known.insert(id, rows.first().ok_or_else(missing)?.hash(0)?);
    }
    ids.iter()
        .map(|id| known.get(id).copied().ok_or_else(missing))
        .collect()
}

/// One row per index row, in write-set order; returns the arguments of
/// the clearing references, written after the rows they reference.
fn index<'s, const P: u8>(
    sql: &'s Catalogue,
    header: &Header<P>,
    write: &WriteSet<P>,
    writes: &mut Vec<Write<'s>>,
) -> Result<Vec<Vec<Value>>, StoreError> {
    let position = write.position;
    let mut refs = Vec::new();
    let mut versions = Vec::new();
    let mut seal_leaves = Vec::new();
    let mut seal_nodes = Vec::new();
    for row in &write.index {
        match row {
            IndexRow::EntryKey {
                key,
                id,
                position,
                fingerprint,
            } => {
                let reverses = write.index.iter().find_map(|row| match row {
                    IndexRow::Reversal { target, by } if by == id => Some(*target),
                    _ => None,
                });
                writes.push(Write::Row(
                    &sql.shared.insert_entry,
                    vec![
                        codec::int(*position)?,
                        codec::int(header.entries)?,
                        codec::bytes(id.as_bytes()),
                        codec::bytes(key.as_str().as_bytes()),
                        fingerprint
                            .map_or(Value::Null(Kind::Bytes), |f| codec::bytes(f.as_bytes())),
                        reverses.map_or(Value::Null(Kind::Bytes), |t| codec::bytes(t.as_bytes())),
                    ],
                ));
            }
            // Written with its entry's key row.
            IndexRow::Reversal { .. } => {}
            IndexRow::Account {
                registration,
                position,
            } => {
                if registration.handle.get() != header.accounts {
                    return Err(integrity(
                        "an account handle is not the next one",
                        Some(*position),
                    ));
                }
                writes.push(Write::Row(
                    &sql.shared.insert_account,
                    vec![
                        codec::handle(registration.handle),
                        codec::text(&registration.path.to_string()),
                        codec::kind(registration.kind),
                        codec::int(*position)?,
                    ],
                ));
            }
            IndexRow::AccountVersion {
                handle,
                term,
                effective,
                position,
            } => {
                let null = Value::Null(Kind::Int);
                let (field, open_from, open_to, limit, class) = match term {
                    AccountTerm::Window(window) => {
                        let [from, to] = codec::window(*window);
                        (0, from, to, null.clone(), Value::Null(Kind::Text))
                    }
                    AccountTerm::Limit(limit) => (
                        1,
                        null.clone(),
                        null.clone(),
                        codec::limit(*limit),
                        Value::Null(Kind::Text),
                    ),
                    AccountTerm::Class(class) => (
                        2,
                        null.clone(),
                        null.clone(),
                        null.clone(),
                        class
                            .as_ref()
                            .map_or(Value::Null(Kind::Text), |class| codec::text(class.as_str())),
                    ),
                };
                versions.push(vec![
                    codec::handle(*handle),
                    Value::Int(field),
                    codec::date(*effective),
                    codec::int(*position)?,
                    open_from,
                    open_to,
                    limit,
                    class,
                ]);
            }
            IndexRow::CurrencyVersion {
                code,
                exponent,
                effective,
                position,
            } => {
                writes.push(Write::Row(
                    &sql.shared.insert_currency,
                    vec![
                        codec::text(code.as_str()),
                        Value::Int(i64::from(*exponent)),
                        codec::date(*effective),
                        codec::int(*position)?,
                    ],
                ));
            }
            IndexRow::PolicyVersion {
                effective,
                position,
                ..
            } => {
                writes.push(Write::Row(
                    &sql.shared.insert_policy,
                    vec![codec::int(*position)?, codec::date(*effective)],
                ));
            }
            IndexRow::Period {
                id,
                start,
                end,
                position,
            } => {
                writes.push(Write::Row(
                    &sql.shared.insert_period,
                    vec![
                        codec::text(id.as_str()),
                        codec::date(*start),
                        codec::date(*end),
                        codec::int(*position)?,
                    ],
                ));
            }
            IndexRow::Sealed { period, position } => {
                writes.push(Write::Row(
                    &sql.shared.insert_seal,
                    vec![codec::text(period.as_str()), codec::int(*position)?],
                ));
            }
            IndexRow::SealTree {
                period,
                currency,
                layer,
                leaves,
            } => {
                let Record::Seal(seal) = &write.record else {
                    return Err(integrity("a seal tree without its seal", Some(position)));
                };
                let mut tree = BalanceTree::<P>::new(*currency, *layer, seal.parts().accounts);
                for (handle, balance) in leaves {
                    tree.set(*handle, *balance).map_err(|_| {
                        integrity("a seal leaf beyond the accounts", Some(position))
                    })?;
                    seal_leaves.push(vec![
                        codec::text(period.as_str()),
                        codec::text(currency.as_str()),
                        codec::layer(*layer),
                        codec::handle(*handle),
                        codec::amount(balance.debit()),
                        codec::amount(balance.credit()),
                        codec::int(balance.postings())?,
                    ]);
                }
                for (level, index, hash) in tree.stored_nodes() {
                    seal_nodes.push(vec![
                        codec::text(period.as_str()),
                        codec::text(currency.as_str()),
                        codec::layer(*layer),
                        Value::Int(i64::from(level)),
                        Value::Int(i64::from(index)),
                        codec::bytes(hash.as_bytes()),
                    ]);
                }
            }
            IndexRow::ClearingKey { key, position } => {
                writes.push(Write::Row(
                    &sql.shared.insert_clearing,
                    vec![
                        codec::int(*position)?,
                        codec::bytes(key.as_str().as_bytes()),
                    ],
                ));
            }
            IndexRow::Applied { posting, .. } => {
                refs.push(vec![
                    codec::int(position)?,
                    codec::bytes(posting.entry.as_bytes()),
                    Value::Int(i64::from(posting.index)),
                ]);
            }
            IndexRow::ClearingReset { clearing, position } => {
                writes.push(Write::Row(
                    &sql.shared.insert_reset,
                    vec![codec::int(*clearing)?, codec::int(*position)?],
                ));
            }
        }
    }
    writes.push(Write::Rows(&sql.inserts.account_versions, versions));
    writes.push(Write::Rows(&sql.inserts.seal_leaves, seal_leaves));
    writes.push(Write::Rows(&sql.inserts.seal_nodes, seal_nodes));
    Ok(refs)
}

/// Posting rows and their dimensions.
fn postings<'s, const P: u8>(
    sql: &'s Catalogue,
    position: u64,
    rows: &[PostingRow<P>],
    writes: &mut Vec<Write<'s>>,
) -> Result<(), StoreError> {
    let at = codec::int(position)?;
    let mut postings = Vec::with_capacity(rows.len());
    let mut dimensions = Vec::new();
    for row in rows {
        let index = Value::Int(i64::from(row.index));
        let [handle, currency, layer] = codec::key(row.key);
        postings.push(vec![
            at.clone(),
            index.clone(),
            handle,
            currency,
            layer,
            codec::side(row.side),
            codec::amount(row.amount),
            codec::date(row.booking),
            codec::date(row.value),
            codec::dimensions(&row.dimensions),
            codec::amount(row.balance_after.debit()),
            codec::amount(row.balance_after.credit()),
            codec::int(row.balance_after.postings())?,
        ]);
        for (axis, value) in row.dimensions.iter() {
            dimensions.push(vec![
                at.clone(),
                index.clone(),
                codec::text(axis.as_str()),
                codec::text(value.as_str()),
            ]);
        }
    }
    writes.push(Write::Rows(&sql.inserts.postings, postings));
    writes.push(Write::Rows(&sql.inserts.posting_dims, dimensions));
    Ok(())
}

/// The maintained values: balances from the last `balance_after` per key; cleared
/// amounts and slices read and summed in Rust (checked; overflow or a
/// negative result is integrity).
async fn maintain<'s, D: Dialect, const P: u8>(
    session: &mut Session<'s, D>,
    write: &WriteSet<P>,
    writes: &mut Vec<Write<'s>>,
    known: &mut BTreeMap<(u32, String, String), Balance<P>>,
) -> Result<(), StoreError> {
    let sql = session.sql;
    let position = write.position;
    let overflow = || integrity("a maintained value leaves its range", Some(position));
    // One row per touched key, the last posting's balance after.
    let mut balances = BTreeMap::new();
    for row in &write.postings {
        balances.insert(row.key, row.balance_after);
    }
    let rows = balances
        .into_iter()
        .map(|(key, balance)| {
            let [handle, currency, layer] = codec::key(key);
            Ok(vec![
                handle,
                currency,
                layer,
                codec::amount(balance.debit()),
                codec::amount(balance.credit()),
                codec::int(balance.postings())?,
            ])
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    writes.push(Write::Rows(&sql.inserts.balances, rows));
    // Cleared amounts, read for every cleared posting at once.
    let applied: Vec<_> = write
        .index
        .iter()
        .filter_map(|row| match row {
            IndexRow::Applied { posting, delta } => Some((*posting, *delta)),
            _ => None,
        })
        .collect();
    let keys = applied
        .iter()
        .map(|(posting, _)| {
            vec![
                codec::bytes(posting.entry.as_bytes()),
                Value::Int(i64::from(posting.index)),
            ]
        })
        .collect();
    let found = session.keyed(&sql.keyed.cleared, keys).await?;
    let mut cleared: BTreeMap<(i64, i64), Amount<P>> = BTreeMap::new();
    for ((posting, delta), rows) in applied.iter().zip(found) {
        let row = rows
            .first()
            .ok_or_else(|| integrity("a cleared posting's entry is missing", None))?;
        let key = (row.int(0)?, i64::from(posting.index));
        if cleared.contains_key(&key) {
            return Err(integrity(
                "a write set clears one posting twice",
                Some(position),
            ));
        }
        let current = if row.is_null(1)? {
            Amount::ZERO
        } else {
            row.amount::<P>(1)?
        };
        let total = current.checked_add(*delta).map_err(|_| overflow())?;
        if total.is_negative() {
            return Err(overflow());
        }
        cleared.insert(key, total);
    }
    let rows = cleared
        .into_iter()
        .map(|((entry, index), total)| {
            vec![Value::Int(entry), Value::Int(index), codec::amount(total)]
        })
        .collect();
    writes.push(Write::Rows(&sql.inserts.cleared, rows));
    // The Settled slices, read for every touched slice at once.
    let settled: Vec<_> = write
        .postings
        .iter()
        .filter(|row| row.key.layer == Layer::Settled)
        .filter_map(|row| match codec::dimensions(&row.dimensions) {
            Value::Text(dimensions) => Some((
                (
                    row.key.handle.get(),
                    row.key.currency.as_str().to_owned(),
                    dimensions,
                ),
                row,
            )),
            _ => None,
        })
        .collect();
    let touched: Vec<(u32, String, String)> = settled
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let unknown: Vec<(u32, String, String)> = touched
        .iter()
        .filter(|key| !known.contains_key(*key))
        .cloned()
        .collect();
    let keys = unknown
        .iter()
        .map(|(handle, currency, dimensions)| {
            vec![
                Value::Int(i64::from(*handle)),
                codec::text(currency),
                codec::text(dimensions),
            ]
        })
        .collect();
    let found = session.keyed(&sql.keyed.slices, keys).await?;
    for (key, rows) in unknown.into_iter().zip(found) {
        let balance = match rows.first() {
            Some(row) => row.balance::<P>(0)?,
            None => Balance::ZERO,
        };
        known.insert(key, balance);
    }
    let mut slices: BTreeMap<(u32, String, String), Balance<P>> = BTreeMap::new();
    for key in touched {
        let balance = known.get(&key).copied().ok_or_else(overflow)?;
        slices.insert(key, balance);
    }
    for (key, row) in settled {
        let slice = slices.get_mut(&key).ok_or_else(overflow)?;
        *slice = slice.post(row.side, row.amount).map_err(|_| overflow())?;
    }
    known.extend(slices.iter().map(|(key, balance)| (key.clone(), *balance)));
    let rows = slices
        .into_iter()
        .map(|((handle, currency, dimensions), balance)| {
            Ok(vec![
                Value::Int(i64::from(handle)),
                Value::Text(currency),
                Value::Text(dimensions),
                codec::amount(balance.debit()),
                codec::amount(balance.credit()),
                codec::int(balance.postings())?,
            ])
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    writes.push(Write::Rows(&sql.inserts.slices, rows));
    let rows = write
        .outstanding
        .iter()
        .map(|row| {
            vec![
                codec::handle(row.handle),
                codec::text(row.currency.as_str()),
                codec::amount(row.debits),
                codec::amount(row.credits),
            ]
        })
        .collect();
    writes.push(Write::Rows(&sql.inserts.outstanding, rows));
    Ok(())
}

/// Erases a memo and its entry's fingerprint in one short transaction
/// that takes no ledger lock.
pub(crate) async fn erase_memo<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    id: EntryId,
) -> Result<Erasure, StoreError> {
    let mut session = store.session(Mode::Write, "erase_memo").await?;
    let sql = session.sql;
    let key = codec::bytes(id.as_bytes());
    let erased = session
        .execute(&sql.shared.erase_memo, vec![key.clone()])
        .await?;
    if erased > 0 {
        session
            .execute(&sql.shared.erase_fingerprint, vec![key])
            .await?;
        session.commit().await?;
        return Ok(Erasure::Erased);
    }
    let found = session.first(&sql.shared.memo, vec![key]).await?;
    session.rollback().await?;
    Ok(match found {
        None => Erasure::UnknownEntry,
        Some(row) if row.int(0)? == 0 => Erasure::NoMemo,
        Some(_) => Erasure::AlreadyErased,
    })
}

#[cfg(test)]
mod tests {
    use crate::log::nodes::AppendPlan;
    use crate::log::{Tree, leaf_hash};

    /// A write set at another position, or registering a handle other than
    /// the next one, is integrity — nothing is written.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn write_set_with_wrong_handle_is_integrity() {
        use super::super::{SqlStore, Sqlite};
        use super::*;
        use crate::core::AccountSpec;
        use crate::model::{AccountHandle, AccountKind};
        use crate::store::{Store, StoreErrorKind};

        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("books.db").display());
        let store = SqlStore::<Sqlite, 2>::sqlite(&url).await.unwrap();
        let ledger = LedgerId::new("example.com/acme").unwrap();
        store
            .create(&Genesis {
                ledger: ledger.clone(),
            })
            .await
            .unwrap();
        let op = Op::Register {
            spec: AccountSpec::new(
                "Assets:Cash",
                AccountKind::Asset,
                time::macros::date!(2026 - 01 - 01),
            ),
            actor: String::from("setup"),
        };
        let mut session = store.session(Mode::Write, "write").await.unwrap();
        let header = header::<Sqlite, 2>(&mut session, ledger).await.unwrap();
        let Ok(Outcome::Append(write)) = decide(&mut session, header.clone(), &op).await.unwrap()
        else {
            panic!("the registration is accepted");
        };
        let mut moved = (*write).clone();
        moved.position += 1;
        let error = persist(&mut session, &header, &moved, &mut Known::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &StoreErrorKind::Integrity);
        let mut sparse = (*write).clone();
        for row in &mut sparse.index {
            if let IndexRow::Account { registration, .. } = row {
                registration.handle = AccountHandle::new(5).unwrap();
            }
        }
        let error = persist(&mut session, &header, &sparse, &mut Known::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &StoreErrorKind::Integrity);
        session.rollback().await.unwrap();
        assert_eq!(store.size().await.unwrap(), 1);
    }

    /// The append stores exactly the nodes the in-memory tree stores, for every
    /// size up to 300.
    #[test]
    fn appended_nodes_equal_the_tree() {
        let mut tree = Tree::new();
        let mut stored = Vec::new();
        for position in 0u64..300 {
            let leaf = leaf_hash(&position.to_be_bytes());
            let plan = AppendPlan::new(position).unwrap();
            let siblings: Vec<_> = plan.ids().iter().map(|id| stored[*id as usize]).collect();
            for (id, hash) in plan.assemble(leaf, &siblings).unwrap() {
                assert_eq!(id as usize, stored.len());
                stored.push(hash);
            }
            tree.push(leaf).unwrap();
            assert_eq!(tree.stored(), stored.as_slice());
        }
    }
}
