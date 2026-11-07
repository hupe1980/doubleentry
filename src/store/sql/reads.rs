//! The read primitives: each in one read transaction that takes no lock,
//! reads the size first and reports it, and honours its `through` bound.

use super::codec::{self, Row};
use super::dialect::{Mode, Statement, Value};
use super::{Dialect, Session, SqlStore};
use crate::core::BalanceKey;
use crate::error::IntegrityKind;
use crate::log::Hash;
use crate::log::nodes::stored_count;
use crate::model::{
    AccountHandle, AccountRef, Amount, Balance, Currency, EntryId, Layer, PeriodId,
};
use crate::store::{
    AccountRow, BalanceRead, BalanceScope, Cell, EntryRef, HoldCursor, HoldRead, HoldRow,
    KindFilter, Leaf, LeafRead, Memo, Page, PageRead, Position, PostingCursor, PostingLine,
    PostingOrder, PostingRead, ProjectionPage, ProjectionRead, ProjectionRow, SealRow, StoreError,
    StoredEntry, bound, checked_limit, paged,
};

/// A leaf from a (position, kind, bytes) row.
fn leaf(row: &mut Row) -> Result<Leaf, StoreError> {
    Ok(Leaf {
        position: row.u64(0)?,
        kind: row.record_kind(1)?,
        bytes: row.take_bytes(2)?,
    })
}

/// One more than a page's limit, as a bound parameter: the row that says
/// more follow.
fn fetch_limit(limit: u32) -> Value {
    Value::Int(i64::from(limit).saturating_add(1))
}

/// An exclusive position cursor as a parameter (`-1`: from the start).
fn after(cursor: Option<Position>) -> Result<Value, StoreError> {
    cursor.map_or(Ok(Value::Int(-1)), codec::int)
}

/// Runs a read in one read transaction of `operation`, passing the size it
/// sees; with feature `tracing`, inside a debug span named after it.
macro_rules! read {
    ($store:expr, $operation:literal, |$session:ident, $size:ident| $body:expr) => {{
        let run = async {
            let mut $session = $store.session(Mode::Read, $operation).await?;
            let $size = $session.size().await?;
            let result = $body;
            $session.commit().await?;
            Ok::<_, StoreError>(result)
        };
        #[cfg(feature = "tracing")]
        let run = tracing::Instrument::instrument(
            run,
            tracing::debug_span!("read", operation = $operation),
        );
        run.await
    }};
}

pub(crate) async fn read_leaves<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    read: &LeafRead,
) -> Result<Page<Leaf, Position>, StoreError> {
    let limit = checked_limit(read.limit, "read_leaves")?;
    read!(store, "read_leaves", |session, size| {
        let sql = session.sql;
        let through = bound(read.through, size);
        let (from, to) = (codec::int(read.from)?, codec::int(through)?);
        let mut rows = match &read.kinds {
            None => {
                session
                    .fetch(&sql.shared.leaves_from, vec![from, to, fetch_limit(limit)])
                    .await?
            }
            Some(kinds) => {
                let mut merged = Vec::new();
                for kind in kinds {
                    let args = vec![
                        Value::Int(i64::from(*kind as u8)),
                        from.clone(),
                        to.clone(),
                        fetch_limit(limit),
                    ];
                    merged.extend(session.fetch(&sql.shared.leaves_of_kind, args).await?);
                }
                let mut keyed = merged
                    .into_iter()
                    .map(|row| Ok((row.u64(0)?, row)))
                    .collect::<Result<Vec<_>, StoreError>>()?;
                keyed.sort_by_key(|(position, _)| *position);
                keyed.dedup_by_key(|(position, _)| *position);
                keyed.into_iter().map(|(_, row)| row).collect()
            }
        };
        rows.truncate(
            usize::try_from(limit)
                .unwrap_or(usize::MAX)
                .saturating_add(1),
        );
        let rows = rows
            .iter_mut()
            .map(|row| {
                let leaf = leaf(row)?;
                Ok((leaf.position.saturating_add(1), leaf))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        paged(rows, limit, size, through)
    })
}

pub(crate) async fn read_nodes<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    ids: &[u64],
) -> Result<Vec<Hash>, StoreError> {
    read!(store, "read_nodes", |session, size| {
        let sql = session.sql;
        let keys = ids
            .iter()
            .map(|id| Ok(vec![codec::int(*id)?]))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let found = session.keyed(&sql.keyed.nodes, keys).await?;
        let count = stored_count(size).unwrap_or(0);
        ids.iter()
            .zip(found)
            .map(|(id, rows)| match rows.first() {
                Some(row) => row.hash(0),
                None if *id < count => Err(StoreError::integrity_of(
                    "read_nodes",
                    IntegrityKind::NodeMismatch,
                    "a stored node is missing",
                    None,
                )),
                None => Err(StoreError::backend_message(
                    "read_nodes",
                    "a node beyond the log was asked for",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?
    })
}

pub(crate) async fn seals<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    through: Option<u64>,
) -> Result<Vec<SealRow>, StoreError> {
    read!(store, "seals", |session, size| {
        let sql = session.sql;
        let through = codec::int(bound(through, size))?;
        let mut rows = session.fetch(&sql.shared.seals, vec![through]).await?;
        rows.iter_mut()
            .map(|row| {
                Ok(SealRow {
                    period: row.period(0)?,
                    position: row.u64(1)?,
                    leaf: Leaf {
                        position: row.u64(1)?,
                        kind: row.record_kind(2)?,
                        bytes: row.take_bytes(3)?,
                    },
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?
    })
}

/// An account row (handle, path, kind, position).
fn account_row(row: &Row) -> Result<AccountRow, StoreError> {
    Ok(AccountRow {
        handle: row.handle(0)?,
        path: row.path(1)?,
        kind: row.kind(2)?,
        position: row.u64(3)?,
    })
}

pub(crate) async fn accounts<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    refs: &[AccountRef],
) -> Result<Vec<Option<AccountRow>>, StoreError> {
    read!(store, "accounts", |session, _size| {
        let sql = session.sql;
        // One batched lookup per kind of reference.
        let (mut paths, mut handles) = (Vec::new(), Vec::new());
        for (at, account) in refs.iter().enumerate() {
            match account {
                AccountRef::Path(path) => paths.push((at, vec![codec::text(&path.to_string())])),
                AccountRef::Handle(handle) => handles.push((at, vec![codec::handle(*handle)])),
            }
        }
        let mut found: Vec<Option<AccountRow>> = refs.iter().map(|_| None).collect();
        for (lookup, keys) in [
            (&sql.keyed.account_rows_by_path, paths),
            (&sql.keyed.account_rows_by_handle, handles),
        ] {
            let (slots, keys): (Vec<usize>, Vec<_>) = keys.into_iter().unzip();
            for (slot, rows) in slots.into_iter().zip(session.keyed(lookup, keys).await?) {
                if let (Some(target), Some(row)) = (found.get_mut(slot), rows.first()) {
                    *target = Some(account_row(row)?);
                }
            }
        }
        found
    })
}

pub(crate) async fn account_records<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    handle: AccountHandle,
    page: &PageRead<Position>,
) -> Result<Page<Leaf, Position>, StoreError> {
    let limit = page.checked_limit("account_records")?;
    read!(store, "account_records", |session, size| {
        let sql = session.sql;
        let through = bound(page.through, size);
        let args = vec![
            codec::handle(handle),
            after(page.after)?,
            codec::int(through)?,
            fetch_limit(limit),
        ];
        let mut rows = session.fetch(&sql.shared.account_records, args).await?;
        let rows = rows
            .iter_mut()
            .map(|row| {
                let leaf = leaf(row)?;
                Ok((leaf.position, leaf))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        paged(rows, limit, size, through)
    })
}

pub(crate) async fn entry<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    by: EntryRef<'_>,
    through: Option<u64>,
) -> Result<Option<StoredEntry>, StoreError> {
    read!(store, "entry", |session, size| {
        let sql = session.sql;
        let through = codec::int(bound(through, size))?;
        let row = match by {
            EntryRef::Id(id) => {
                session
                    .first(
                        &sql.shared.stored_entry_by_id,
                        vec![codec::bytes(id.as_bytes()), through],
                    )
                    .await?
            }
            EntryRef::Key(key) => {
                session
                    .first(
                        &sql.shared.stored_entry_by_key,
                        vec![codec::bytes(key.as_str().as_bytes()), through],
                    )
                    .await?
            }
        };
        match row {
            Some(mut row) => {
                let position = row.u64(0)?;
                Some(StoredEntry {
                    position,
                    id: row.entry_id(1)?,
                    key: row.idempotency_key(2)?,
                    fingerprint: row.fingerprint(3)?,
                    leaf: Leaf {
                        position,
                        kind: row.record_kind(4)?,
                        bytes: row.take_bytes(5)?,
                    },
                })
            }
            None => None,
        }
    })
}

/// A hold row (position, entry id, booking, expires, debit, credit,
/// currency, amount, resolved by, how).
fn hold<const P: u8>(row: &Row) -> Result<HoldRow<P>, StoreError> {
    let resolution = row.resolution(8)?;
    Ok(HoldRow {
        position: row.u64(0)?,
        id: row.entry_id(1)?,
        booking: row.date(2)?,
        expires: row.opt_date(3)?,
        debit: row.handle(4)?,
        credit: row.handle(5)?,
        currency: row.currency(6)?,
        amount: row.amount(7)?,
        resolution,
    })
}

pub(crate) async fn holds<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    read: &HoldRead,
) -> Result<Page<HoldRow<P>, HoldCursor>, StoreError> {
    let limit = match read {
        HoldRead::ById { .. } => 1,
        HoldRead::OpenByAccount { page, .. } | HoldRead::Due { page, .. } => {
            page.checked_limit("holds")?
        }
    };
    read!(store, "holds", |session, size| {
        let sql = session.sql;
        let (rows, through) = match read {
            HoldRead::ById { id, through } => {
                let through = bound(*through, size);
                let args = vec![codec::bytes(id.as_bytes()), codec::int(through)?];
                (session.fetch(&sql.shared.hold_by_id, args).await?, through)
            }
            HoldRead::OpenByAccount { handle, page } => {
                let through = bound(page.through, size);
                let args = vec![
                    codec::handle(*handle),
                    after(page.after.map(|(_, position)| position))?,
                    codec::int(through)?,
                    fetch_limit(limit),
                ];
                (
                    session
                        .fetch(&sql.shared.holds_open_by_account, args)
                        .await?,
                    through,
                )
            }
            HoldRead::Due { on_or_before, page } => {
                let through = bound(page.through, size);
                let (expires, position) = match page.after {
                    Some((expires, position)) => (
                        expires.map_or(Value::Int(i64::MIN), codec::date),
                        codec::int(position)?,
                    ),
                    None => (Value::Int(i64::MIN), Value::Int(-1)),
                };
                let args = vec![
                    codec::date(*on_or_before),
                    expires,
                    position,
                    codec::int(through)?,
                    fetch_limit(limit),
                ];
                (session.fetch(&sql.shared.holds_due, args).await?, through)
            }
        };
        let due = matches!(read, HoldRead::Due { .. });
        let rows = rows
            .iter()
            .map(|row| {
                let hold = hold::<P>(row)?;
                let cursor = (if due { hold.expires } else { None }, hold.position);
                Ok((cursor, hold))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        paged(rows, limit, size, through)
    })
}

pub(crate) async fn clearings<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    key: BalanceKey,
    page: &PageRead<Position>,
) -> Result<Page<Leaf, Position>, StoreError> {
    let limit = page.checked_limit("clearings")?;
    read!(store, "clearings", |session, size| {
        let sql = session.sql;
        let through = bound(page.through, size);
        let mut args = codec::key(key).to_vec();
        args.extend([after(page.after)?, codec::int(through)?, fetch_limit(limit)]);
        let mut rows = session.fetch(&sql.shared.clearings_of_key, args).await?;
        let rows = rows
            .iter_mut()
            .map(|row| {
                let leaf = leaf(row)?;
                Ok((leaf.position, leaf))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        paged(rows, limit, size, through)
    })
}

/// A malformed opaque cursor.
fn bad_cursor() -> StoreError {
    StoreError::backend_message("projection", "the cursor is not one this store returned")
}

/// A returned value as a projected cell.
fn cell(value: Value) -> Cell {
    match value {
        Value::Int(value) => Cell::Int(value),
        Value::Bytes(bytes) => Cell::Bytes(bytes),
        Value::Text(text) => Cell::Text(text),
        Value::Null(_) => Cell::Null,
    }
}

/// A key cell as a bound parameter.
fn parameter(cell: Cell) -> Result<Value, StoreError> {
    match cell {
        Cell::Int(value) => Ok(Value::Int(value)),
        Cell::Bytes(bytes) => Ok(Value::Bytes(bytes)),
        Cell::Text(text) => Ok(Value::Text(text)),
        _ => Err(bad_cursor()),
    }
}

/// A row's position cell.
fn position_of(row: &ProjectionRow) -> i64 {
    match row.cells().first() {
        Some(Cell::Int(position)) => *position,
        _ => i64::MAX,
    }
}

pub(crate) async fn projection<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    read: &ProjectionRead,
) -> Result<ProjectionPage, StoreError> {
    let limit = read.page.checked_limit("projection")?;
    let part = read.part;
    let key = match &read.page.after {
        Some(bytes) => {
            let row = ProjectionRow::from_bytes(bytes).ok_or_else(bad_cursor)?;
            if row.cells().len() != part.key_len() {
                return Err(bad_cursor());
            }
            Some(
                row.cells()
                    .iter()
                    .cloned()
                    .map(parameter)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
        None => None,
    };
    read!(store, "projection", |session, size| {
        let sql = session.sql;
        let pages = sql
            .part(part)
            .ok_or_else(|| StoreError::backend_message("projection", "an unknown part"))?;
        let run = |statement: &Statement, mut args: Vec<Value>| {
            args.push(fetch_limit(limit));
            (statement.clone(), args)
        };
        let (statement, args) = match &key {
            Some(key) => run(&pages.after, key.clone()),
            None => run(&pages.first, Vec::new()),
        };
        let mut rows: Vec<ProjectionRow> = session
            .fetch_values(&statement, args)
            .await?
            .into_iter()
            .map(|values| ProjectionRow::new(values.into_iter().map(cell).collect()))
            .collect();
        if let Some((first, after)) = &pages.memos {
            // Memo rows without an entry row (the foreign key forbids them):
            // merged in by position, so the part holds every memos row too.
            let (statement, args) = match &key {
                Some(key) => run(after, key.clone()),
                None => run(first, Vec::new()),
            };
            let memos = session.fetch_values(&statement, args).await?;
            let wanted = usize::try_from(limit)
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            // Both scans are complete up to the last position of a full one.
            let mut bound = i64::MAX;
            if rows.len() == wanted {
                bound = bound.min(rows.last().map_or(i64::MAX, position_of));
            }
            let orphans: Vec<ProjectionRow> = memos
                .into_iter()
                .map(|values| {
                    let [position, salt, text]: [Value; 3] =
                        values.try_into().map_err(|_| bad_cursor())?;
                    let position = cell(position);
                    Ok(ProjectionRow::new(vec![
                        position.clone(),
                        Cell::Null,
                        position,
                        cell(salt),
                        cell(text),
                    ]))
                })
                .collect::<Result<_, StoreError>>()?;
            if orphans.len() == wanted {
                bound = bound.min(orphans.last().map_or(i64::MAX, position_of));
            }
            let entries: std::collections::BTreeSet<i64> = rows.iter().map(position_of).collect();
            rows.extend(
                orphans
                    .into_iter()
                    .filter(|row| !entries.contains(&position_of(row))),
            );
            rows.retain(|row| position_of(row) <= bound);
            rows.sort_by_key(position_of);
        }
        let rows = rows
            .into_iter()
            .map(|row| (row.key(part).to_bytes(), row))
            .collect();
        paged(rows, limit, size, size)
    })
}

pub(crate) async fn seal_nodes<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    period: &PeriodId,
    currency: Currency,
    layer: Layer,
    ids: &[(u32, u32)],
) -> Result<Vec<Option<Hash>>, StoreError> {
    read!(store, "seal_nodes", |session, _size| {
        let sql = session.sql;
        let keys = ids
            .iter()
            .map(|(level, index)| {
                vec![
                    codec::text(period.as_str()),
                    codec::text(currency.as_str()),
                    codec::layer(layer),
                    Value::Int(i64::from(*level)),
                    Value::Int(i64::from(*index)),
                ]
            })
            .collect();
        session
            .keyed(&sql.keyed.seal_nodes, keys)
            .await?
            .iter()
            .map(|rows| rows.first().map(|row| row.hash(0)).transpose())
            .collect::<Result<Vec<_>, StoreError>>()?
    })
}

/// A balance key's columns as a cursor (`-1, '', -1`: from the start).
fn key_cursor(after: Option<BalanceKey>) -> Vec<Value> {
    match after {
        Some(key) => codec::key(key).to_vec(),
        None => vec![Value::Int(-1), Value::Text(String::new()), Value::Int(-1)],
    }
}

/// The balances of `keys` in `scope`, in order, zero when absent: one
/// batched lookup.
async fn balances_of<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    scope: &BalanceScope,
    keys: &[BalanceKey],
) -> Result<Vec<(BalanceKey, Balance<P>)>, StoreError> {
    let sql = session.sql;
    let (lookup, rows) = match scope {
        BalanceScope::Current => (
            &sql.keyed.balance,
            keys.iter().map(|key| codec::key(*key).to_vec()).collect(),
        ),
        BalanceScope::At(position) => {
            let below = codec::int(*position)?;
            (
                &sql.keyed.balance_at,
                keys.iter()
                    .map(|key| {
                        let mut args = codec::key(*key).to_vec();
                        args.push(below.clone());
                        args
                    })
                    .collect(),
            )
        }
        BalanceScope::Sealed(period) => (
            &sql.keyed.sealed_balance,
            keys.iter()
                .map(|key| {
                    let [handle, currency, layer] = codec::key(*key);
                    vec![codec::text(period.as_str()), currency, layer, handle]
                })
                .collect(),
        ),
    };
    let found = session.keyed(lookup, rows).await?;
    keys.iter()
        .zip(found)
        .map(|(key, rows)| {
            let balance = rows
                .first()
                .map(|row| row.balance(0))
                .transpose()?
                .unwrap_or(Balance::ZERO);
            Ok((*key, balance))
        })
        .collect()
}

pub(crate) async fn balances<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    read: &BalanceRead,
) -> Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError> {
    let limit = if read.keys.is_some() {
        PageRead::<()>::MAX_LIMIT
    } else {
        read.page.checked_limit("balances")?
    };
    read!(store, "balances", |session, size| {
        let sql = session.sql;
        let s = &sql.shared;
        let through = bound(read.page.through, size);
        if let Some(keys) = &read.keys {
            Page {
                items: balances_of::<D, P>(&mut session, &read.scope, keys).await?,
                next: None,
                size,
                through,
            }
        } else {
            let mut rows: Vec<(BalanceKey, (BalanceKey, Balance<P>))> = Vec::new();
            match &read.scope {
                BalanceScope::Current => {
                    let mut args = key_cursor(read.page.after);
                    args.push(fetch_limit(limit));
                    for row in session.fetch(&s.balances_after_key, args).await? {
                        let key = row.key(0)?;
                        rows.push((key, (key, row.balance(3)?)));
                    }
                }
                BalanceScope::At(position) => {
                    // One statement per page: every key after the cursor
                    // with its last balance below the position.
                    let mut args = key_cursor(read.page.after);
                    args.extend([codec::int(*position)?, fetch_limit(limit)]);
                    for row in session.fetch(&s.balances_at_after_key, args).await? {
                        let key = row.key(0)?;
                        rows.push((key, (key, row.balance(3)?)));
                    }
                }
                BalanceScope::Sealed(period) => {
                    let mut args = vec![codec::text(period.as_str())];
                    match read.page.after {
                        Some(key) => {
                            let [handle, currency, layer] = codec::key(key);
                            args.extend([currency, layer, handle]);
                        }
                        None => args.extend([
                            Value::Text(String::new()),
                            Value::Int(-1),
                            Value::Int(-1),
                        ]),
                    }
                    args.push(fetch_limit(limit));
                    for row in session.fetch(&s.sealed_balances, args).await? {
                        let key = BalanceKey {
                            currency: row.currency(0)?,
                            layer: row.layer(1)?,
                            handle: row.handle(2)?,
                        };
                        rows.push((key, (key, row.balance(3)?)));
                    }
                }
            }
            paged(rows, limit, size, through)
        }
    })
}

pub(crate) async fn postings<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    read: &PostingRead,
) -> Result<Page<PostingLine<P>, PostingCursor>, StoreError> {
    let limit = read.page.checked_limit("postings")?;
    if read.dimensions.len() > PostingRead::MAX_DIMENSIONS {
        return Err(StoreError::backend_message(
            "postings",
            "more than eight dimension filters",
        ));
    }
    if matches!(&read.kinds, KindFilter::OneOf(kinds) if kinds.len() > KindFilter::MAX_KINDS) {
        return Err(StoreError::backend_message(
            "postings",
            "more than eight kind filters",
        ));
    }
    read!(store, "postings", |session, size| {
        let sql = session.sql;
        let through = bound(read.page.through, size);
        let statement = sql.postings_page(read);
        let mut args = codec::key(read.key).to_vec();
        args.push(codec::int(through)?);
        let cursor = read.page.after;
        match read.order {
            PostingOrder::Position => args.extend([
                cursor.map_or(Ok(Value::Int(-1)), |c| codec::int(c.position))?,
                Value::Int(cursor.map_or(-1, |c| i64::from(c.index))),
            ]),
            PostingOrder::Booking => args.extend([
                cursor
                    .and_then(|c| c.booking)
                    .map_or(Value::Int(i64::MIN), codec::date),
                cursor.map_or(Ok(Value::Int(-1)), |c| codec::int(c.position))?,
                Value::Int(cursor.map_or(-1, |c| i64::from(c.index))),
            ]),
        }
        if let Some(range) = &read.booking {
            args.extend([codec::date(*range.start()), codec::date(*range.end())]);
        }
        if let Some(range) = &read.value {
            args.extend([codec::date(*range.start()), codec::date(*range.end())]);
        }
        if let KindFilter::OneOf(kinds) = &read.kinds {
            args.extend(kinds.iter().map(|kind| codec::text(kind.as_str())));
        }
        for (axis, value) in &read.dimensions {
            args.extend([codec::text(axis.as_str()), codec::text(value.as_str())]);
        }
        args.push(fetch_limit(limit));
        let rows = session.fetch(&statement, args).await?;
        let lines = rows
            .iter()
            .map(|row| {
                let line = PostingLine {
                    position: row.u64(0)?,
                    index: row.u16(1)?,
                    entry: row.entry_id(2)?,
                    side: row.side(3)?,
                    amount: row.amount(4)?,
                    booking: row.date(5)?,
                    value: row.date(6)?,
                    dimensions: row.dimensions(7)?,
                    balance_after: row.balance(8)?,
                    cleared: match row.opt_bytes(11)? {
                        Some(_) => row.amount(11)?,
                        None => Amount::ZERO,
                    },
                    kind: row.opt_label(12)?,
                };
                let cursor = PostingCursor {
                    booking: (read.order == PostingOrder::Booking).then_some(line.booking),
                    position: line.position,
                    index: line.index,
                };
                Ok((cursor, line))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        paged(lines, limit, size, through)
    })
}

pub(crate) async fn memo<D: Dialect, const P: u8>(
    store: &SqlStore<D, P>,
    id: EntryId,
) -> Result<Option<Memo>, StoreError> {
    read!(store, "memo", |session, _size| {
        let sql = session.sql;
        let row = session
            .first(&sql.shared.memo, vec![codec::bytes(id.as_bytes())])
            .await?;
        match row {
            None => None,
            Some(row) if row.int(0)? == 0 => Some(Memo::None),
            Some(row) => Some(match row.opt_text(2)? {
                Some(text) => Memo::Kept {
                    text: text.to_owned(),
                    salt: row.salt(1)?,
                },
                None => Memo::Erased,
            }),
        }
    })
}
