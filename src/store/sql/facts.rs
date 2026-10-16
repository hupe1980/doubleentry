//! `Read` → `Fact`: the reads of one plan round, answered
//! inside the write transaction exactly as the in-memory state answers them
//! — one round trip per kind of read, whatever the number of keys.

use std::collections::BTreeMap;

use time::Date;

use super::codec::{self, Row};
use super::dialect::{Batch, Value};
use super::write::period_view;
use super::{Dialect, Session};
use crate::core::{
    AccountView, BalanceKey, ClearingView, EntryMeta, EntryView, Fact, HoldView, LatestTerms,
    Movement, Read, Slice, Terms, Versioned, Window,
};
use crate::log::nodes::stored_index;
use crate::log::{Hash, leaf_hash};
use crate::model::{
    AccountAction, AccountEvent, AccountHandle, AccountPath, Amount, Balance, BalanceLimit,
    ClearingAction, Currency, EntryId, IdempotencyKey, Label, LedgerId, LedgerPolicy, PeriodId,
    PostingRef, Record, Side,
};
use crate::store::{Leaf, StoreError};

/// The record a (kind, bytes) column pair at `position` holds.
fn record<const P: u8>(row: &Row, kind: usize, position: u64) -> Result<Record<P>, StoreError> {
    Leaf {
        position,
        kind: row.record_kind(kind)?,
        bytes: row.bytes(kind.saturating_add(1))?.to_vec(),
    }
    .record()
}

fn mismatch(what: &'static str, position: Option<u64>) -> StoreError {
    StoreError::integrity("write", what, position)
}

/// The reads of one round, grouped by kind: each group is one batched
/// lookup.
#[derive(Default)]
struct Groups {
    entry_by_key: Vec<IdempotencyKey>,
    account_by_path: Vec<AccountPath>,
    account: Vec<AccountHandle>,
    account_terms: Vec<(AccountHandle, Date)>,
    currency: Vec<(Currency, Date)>,
    currency_latest: Vec<Currency>,
    policy: Vec<Date>,
    period_at: Vec<Date>,
    period: Vec<PeriodId>,
    entry: Vec<EntryId>,
    reversed_by: Vec<EntryId>,
    hold: Vec<EntryId>,
    outstanding: Vec<(AccountHandle, Currency, Side)>,
    balance: Vec<BalanceKey>,
    clearing_by_key: Vec<IdempotencyKey>,
    clearing_at: Vec<u64>,
    applied: Vec<PostingRef>,
}

/// Runs a batched lookup over `keys`: each key with the rows found for it.
async fn keyed<D: Dialect, K>(
    session: &mut Session<'_, D>,
    lookup: &Batch,
    keys: Vec<K>,
    encode: impl Fn(&K) -> Result<Vec<Value>, StoreError>,
) -> Result<Vec<(K, Vec<Row>)>, StoreError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let encoded = keys.iter().map(encode).collect::<Result<Vec<_>, _>>()?;
    let rows = session.keyed(lookup, encoded).await?;
    Ok(keys.into_iter().zip(rows).collect())
}

/// Answers the reads of one plan round.
///
/// Every record handed to the core is checked first: an entry's,
/// hold's or clearing's row names the record it holds (the id derives from
/// the leaf's key, the key or position is the one asked for), and those
/// leaves hash to the leaf hashes the tree stores at their positions — one
/// batched node read, only in rounds that load such records.
pub(crate) async fn load<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    ledger: &LedgerId,
    reads: Vec<Read>,
) -> Result<Vec<Fact<P>>, StoreError> {
    let sql = session.sql;
    let (s, k) = (&sql.shared, &sql.keyed);
    let mut facts = Vec::with_capacity(reads.len());
    // (position, leaf hash) of every record loaded for the core.
    let mut leaves: Vec<(u64, Hash)> = Vec::new();
    let mut groups = Groups::default();
    for read in reads {
        match read {
            Read::EntryByKey(key) => groups.entry_by_key.push(key),
            Read::AccountByPath(path) => groups.account_by_path.push(path),
            Read::Account(handle) => groups.account.push(handle),
            Read::AccountTerms(handle, on) => groups.account_terms.push((handle, on)),
            Read::Currency(code, on) => groups.currency.push((code, on)),
            Read::CurrencyLatest(code) => groups.currency_latest.push(code),
            Read::Policy(on) => groups.policy.push(on),
            Read::PeriodAt(date) => groups.period_at.push(date),
            Read::Period(id) => groups.period.push(id),
            Read::Entry(id) => groups.entry.push(id),
            Read::ReversedBy(id) => groups.reversed_by.push(id),
            Read::OutstandingHoldDebits(handle, currency) => {
                groups.outstanding.push((handle, currency, Side::Debit));
            }
            Read::OutstandingHoldCredits(handle, currency) => {
                groups.outstanding.push((handle, currency, Side::Credit));
            }
            Read::Balance(key) => groups.balance.push(key),
            Read::ClearingByKey(key) => groups.clearing_by_key.push(key),
            Read::ClearingAt(position) => groups.clearing_at.push(position),
            Read::Applied(posting) => groups.applied.push(posting),
            Read::Hold(id) => groups.hold.push(id),
            Read::PolicyLatest => {
                let row = session.first(&s.policy_latest, Vec::new()).await?;
                facts.push(Fact::PolicyLatest(
                    row.map(|row| policy::<P>(&row)).transpose()?,
                ));
            }
            Read::FirstUnsealed => {
                let row = session.first(&s.first_unsealed, Vec::new()).await?;
                facts.push(Fact::FirstUnsealed(
                    row.map(|row| period_view(&row, 0)).transpose()?,
                ));
            }
            Read::AllBalances => facts.push(all_balances(session).await?),
            Read::PostingsAfter(date) => facts.push(postings_after(session, date).await?),
            Read::PnlSlices(date) => {
                facts.push(Fact::PnlSlices(date, pnl_slices(session, date).await?));
            }
        }
    }
    let bytes = |bytes: &[u8]| Ok(vec![codec::bytes(bytes)]);
    for (key, rows) in keyed(session, &k.entry_by_key, groups.entry_by_key, |key| {
        bytes(key.as_str().as_bytes())
    })
    .await?
    {
        let meta = match rows.first() {
            Some(row) => Some(EntryMeta {
                position: row.u64(0)?,
                id: row.entry_id(1)?,
                fingerprint: row.fingerprint(2)?,
            }),
            None => None,
        };
        if let Some(meta) = &meta
            && meta.id != EntryId::derive(ledger, &key)
        {
            return Err(mismatch(
                "an entry's id does not derive from its key",
                Some(meta.position),
            ));
        }
        facts.push(Fact::EntryByKey(key, meta));
    }
    for (path, rows) in keyed(
        session,
        &k.account_by_path,
        groups.account_by_path,
        |path| Ok(vec![codec::text(&path.to_string())]),
    )
    .await?
    {
        let view = rows.first().map(account::<P>).transpose()?;
        if view.as_ref().is_some_and(|view| *view.path() != path) {
            return Err(mismatch("an account row names another path", None));
        }
        facts.push(Fact::AccountByPath(path, view));
    }
    for (handle, rows) in keyed(session, &k.account_by_handle, groups.account, |handle| {
        Ok(vec![codec::handle(*handle)])
    })
    .await?
    {
        let view = rows.first().map(account::<P>).transpose()?;
        facts.push(Fact::Account(handle, view));
    }
    for ((handle, on), rows) in keyed(session, &k.account_terms, groups.account_terms, |key| {
        Ok(vec![codec::handle(key.0), codec::date(key.1)])
    })
    .await?
    {
        facts.push(Fact::AccountTerms(handle, on, terms(&rows)?));
    }
    for ((code, on), rows) in keyed(session, &k.currency_at, groups.currency, |key| {
        Ok(vec![codec::text(key.0.as_str()), codec::date(key.1)])
    })
    .await?
    {
        let exponent = rows.first().map(|row| row.u8(0)).transpose()?;
        facts.push(Fact::Currency(code, on, exponent));
    }
    for (code, rows) in keyed(
        session,
        &k.currency_latest,
        groups.currency_latest,
        |code| Ok(vec![codec::text(code.as_str())]),
    )
    .await?
    {
        let latest = match rows.first() {
            Some(row) => Some(Versioned {
                value: row.u8(0)?,
                effective: row.date(1)?,
                position: row.u64(2)?,
            }),
            None => None,
        };
        facts.push(Fact::CurrencyLatest(code, latest));
    }
    for (on, rows) in keyed(session, &k.policy_at, groups.policy, |on| {
        Ok(vec![codec::date(*on)])
    })
    .await?
    {
        let policy = match rows.first() {
            Some(row) => policy::<P>(row)?.value,
            None => LedgerPolicy::default(),
        };
        facts.push(Fact::Policy(on, policy));
    }
    for (date, rows) in keyed(session, &k.period_at, groups.period_at, |date| {
        Ok(vec![codec::date(*date)])
    })
    .await?
    {
        let period = match rows.first() {
            Some(row) => Some(period_view(row, 0)?).filter(|view| date <= view.end),
            None => None,
        };
        facts.push(Fact::PeriodAt(date, period));
    }
    for (id, rows) in keyed(session, &k.period_by_id, groups.period, |id| {
        Ok(vec![codec::text(id.as_str())])
    })
    .await?
    {
        let period = rows.first().map(|row| period_view(row, 0)).transpose()?;
        facts.push(Fact::Period(id, period));
    }
    for (id, rows) in keyed(session, &k.entry_by_id, groups.entry, |id| {
        bytes(id.as_bytes())
    })
    .await?
    {
        let view = match rows.first() {
            Some(row) => {
                let position = row.u64(0)?;
                match record::<P>(row, 1, position)? {
                    Record::Entry(record) if record.id(ledger) == id => {
                        leaves.push((position, leaf_hash(row.bytes(2)?)));
                        Some(EntryView { position, record })
                    }
                    _ => {
                        return Err(mismatch(
                            "an entry row names another record",
                            Some(position),
                        ));
                    }
                }
            }
            None => None,
        };
        facts.push(Fact::Entry(id, view));
    }
    for (id, rows) in keyed(session, &k.reversed_by, groups.reversed_by, |id| {
        bytes(id.as_bytes())
    })
    .await?
    {
        let by = rows.first().map(|row| row.entry_id(0)).transpose()?;
        facts.push(Fact::ReversedBy(id, by));
    }
    for (id, rows) in keyed(session, &k.hold, groups.hold, |id| bytes(id.as_bytes())).await? {
        let view = match rows.first() {
            Some(row) => {
                let position = row.u64(0)?;
                let Record::Entry(record) = record::<P>(row, 1, position)? else {
                    return Err(mismatch("a hold row names another record", Some(position)));
                };
                if record.id(ledger) != id {
                    return Err(mismatch("a hold row names another record", Some(position)));
                }
                leaves.push((position, leaf_hash(row.bytes(2)?)));
                let open = HoldView::open(id, position, &record).ok_or_else(|| {
                    mismatch("a hold row names an entry that is no hold", Some(position))
                })?;
                Some(match row.resolution(3)? {
                    Some((by, how)) => open.resolved(by, how),
                    None => open,
                })
            }
            None => None,
        };
        facts.push(Fact::Hold(id, view));
    }
    for ((handle, currency, side), rows) in
        keyed(session, &k.outstanding, groups.outstanding, |key| {
            Ok(vec![codec::handle(key.0), codec::text(key.1.as_str())])
        })
        .await?
    {
        let column = match side {
            Side::Debit => 0,
            Side::Credit => 1,
        };
        let amount = rows
            .first()
            .map(|row| row.amount(column))
            .transpose()?
            .unwrap_or(Amount::ZERO);
        facts.push(match side {
            Side::Debit => Fact::OutstandingHoldDebits(handle, currency, amount),
            Side::Credit => Fact::OutstandingHoldCredits(handle, currency, amount),
        });
    }
    for (key, rows) in keyed(session, &k.balance, groups.balance, |key| {
        Ok(codec::key(*key).to_vec())
    })
    .await?
    {
        let balance = rows
            .first()
            .map(|row| row.balance(0))
            .transpose()?
            .unwrap_or(Balance::ZERO);
        facts.push(Fact::Balance(key, balance));
    }
    for (key, rows) in keyed(session, &k.clearing_by_key, groups.clearing_by_key, |key| {
        bytes(key.as_str().as_bytes())
    })
    .await?
    {
        let view = rows.first().map(clearing).transpose()?;
        if let Some(view) = &view {
            let named = match &view.event.action {
                ClearingAction::Apply { key: found, .. }
                | ClearingAction::Reset { key: found, .. } => *found == key,
            };
            if !named {
                return Err(mismatch(
                    "a clearing row names another record",
                    Some(view.position),
                ));
            }
            leaves.push((view.position, clearing_leaf(&rows)?));
        }
        facts.push(Fact::ClearingByKey(key, view));
    }
    for (position, rows) in keyed(session, &k.clearing_at, groups.clearing_at, |position| {
        Ok(vec![codec::int(*position)?])
    })
    .await?
    {
        let view = rows.first().map(clearing).transpose()?;
        if let Some(view) = &view {
            if view.position != position {
                return Err(mismatch(
                    "a clearing row names another record",
                    Some(view.position),
                ));
            }
            leaves.push((view.position, clearing_leaf(&rows)?));
        }
        facts.push(Fact::ClearingAt(position, view));
    }
    for (posting, rows) in keyed(session, &k.applied, groups.applied, |posting| {
        Ok(vec![
            codec::bytes(posting.entry.as_bytes()),
            Value::Int(i64::from(posting.index)),
        ])
    })
    .await?
    {
        let amount = rows
            .first()
            .map(|row| row.amount(0))
            .transpose()?
            .unwrap_or(Amount::ZERO);
        facts.push(Fact::Applied(posting, amount));
    }
    verify_leaves(session, leaves).await?;
    Ok(facts)
}

/// The leaf hash of a clearing row's (kind, bytes) at columns 1 and 2.
fn clearing_leaf(rows: &[Row]) -> Result<Hash, StoreError> {
    let row = rows
        .first()
        .ok_or_else(|| mismatch("a clearing row is missing", None))?;
    Ok(leaf_hash(row.bytes(2)?))
}

/// Checks that each loaded record's leaf hashes to the leaf hash the tree
/// stores at its position: one batched read of the level-0 nodes.
async fn verify_leaves<D: Dialect>(
    session: &mut Session<'_, D>,
    leaves: Vec<(u64, Hash)>,
) -> Result<(), StoreError> {
    if leaves.is_empty() {
        return Ok(());
    }
    let sql = session.sql;
    let keys = leaves
        .iter()
        .map(|(position, _)| {
            let node = stored_index(0, *position)
                .map_err(|_| mismatch("a record lies beyond the tree", Some(*position)))?;
            Ok(vec![codec::int(node)?])
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let found = session.keyed(&sql.keyed.nodes, keys).await?;
    for ((position, hash), rows) in leaves.into_iter().zip(found) {
        if rows.first().map(|row| row.hash(0)).transpose()? != Some(hash) {
            return Err(StoreError::integrity_of(
                "write",
                crate::error::IntegrityKind::NodeMismatch,
                "a record's leaf does not hash to the stored leaf hash",
                Some(position),
            ));
        }
    }
    Ok(())
}

/// `AllBalances` (sealing), in key order.
async fn all_balances<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
) -> Result<Fact<P>, StoreError> {
    let sql = session.sql;
    let rows = session.fetch(&sql.shared.all_balances, Vec::new()).await?;
    let mut balances = rows
        .iter()
        .map(|row| Ok((row.key(0)?, row.balance(3)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;
    balances.sort_by_key(|(key, _)| *key);
    Ok(Fact::AllBalances(balances))
}

/// `PostingsAfter` (sealing), by day.
async fn postings_after<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    date: Date,
) -> Result<Fact<P>, StoreError> {
    let sql = session.sql;
    let rows = session
        .fetch(&sql.shared.postings_after, vec![codec::date(date)])
        .await?;
    let movements = rows
        .iter()
        .map(|row| {
            Ok(Movement {
                key: row.key(0)?,
                side: row.side(3)?,
                amount: row.amount(4)?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(Fact::PostingsAfter(date, movements))
}

/// An account row (handle, register kind and bytes, has-descendants, last
/// booking date, then effective, position and value of the latest window,
/// limit and class) as the core's view.
fn account<const P: u8>(row: &Row) -> Result<AccountView, StoreError> {
    let handle = row.handle(0)?;
    let registration = match record::<P>(row, 1, 0)? {
        Record::Account(AccountEvent {
            action: AccountAction::Register(registration),
            ..
        }) if registration.handle == handle => registration,
        _ => return Err(mismatch("an account row names another record", None)),
    };
    if row.is_null(6)? || row.is_null(10)? || row.is_null(13)? {
        return Err(mismatch("an account has no version of a field", None));
    }
    let class_position = row.u64(13)?;
    Ok(AccountView {
        registration,
        latest: LatestTerms {
            window: Versioned {
                value: Window {
                    open_from: row.date(7)?,
                    open_to: row.opt_date(8)?,
                },
                effective: row.date(5)?,
                position: row.u64(6)?,
            },
            limit: Versioned {
                value: row.limit(11)?,
                effective: row.date(9)?,
                position: row.u64(10)?,
            },
            class: Versioned {
                value: row
                    .opt_text(14)?
                    .map(Label::new)
                    .transpose()
                    .map_err(|_| mismatch("an account class is invalid", Some(class_position)))?,
                effective: row.date(12)?,
                position: class_position,
            },
        },
        is_leaf: row.int(3)? == 0,
        last_booked: row.opt_date(4)?,
    })
}

/// The terms in force: one row per field (field, open from, open to, limit
/// code, class); `None` unless all three are there.
fn terms(rows: &[Row]) -> Result<Option<Terms>, StoreError> {
    let (mut window, mut limit, mut class): (
        Option<Window>,
        Option<BalanceLimit>,
        Option<Option<Label>>,
    ) = (None, None, None);
    for row in rows {
        match row.int(0)? {
            0 => {
                window = Some(Window {
                    open_from: row.date(1)?,
                    open_to: row.opt_date(2)?,
                });
            }
            1 => limit = Some(row.limit(3)?),
            _ => {
                class = Some(
                    row.opt_text(4)?
                        .map(Label::new)
                        .transpose()
                        .map_err(|_| mismatch("an account class is invalid", None))?,
                );
            }
        }
    }
    Ok(match (window, limit, class) {
        (Some(window), Some(limit), Some(class)) => Some(Terms {
            class,
            window,
            limit,
        }),
        _ => None,
    })
}

/// A policy row (position, kind, bytes) as a version.
fn policy<const P: u8>(row: &Row) -> Result<Versioned<LedgerPolicy>, StoreError> {
    let position = row.u64(0)?;
    match record::<P>(row, 1, position)? {
        Record::Policy(event) => Ok(Versioned {
            value: event.policy,
            effective: event.effective,
            position,
        }),
        _ => Err(mismatch(
            "a policy row names another record",
            Some(position),
        )),
    }
}

/// A clearing row (position, kind, bytes, reset position).
fn clearing<const P: u8>(row: &Row) -> Result<ClearingView<P>, StoreError> {
    let position = row.u64(0)?;
    match record::<P>(row, 1, position)? {
        Record::Clearing(event) => Ok(ClearingView {
            position,
            event,
            reset_by: row.opt_u64(3)?,
        }),
        _ => Err(mismatch(
            "a clearing row names another record",
            Some(position),
        )),
    }
}

/// `PnlSlices`: the maintained slices of income and expense accounts minus
/// their Settled postings booked after `date`, in Rust with checked
/// arithmetic; slices left without postings are dropped.
async fn pnl_slices<D: Dialect, const P: u8>(
    session: &mut Session<'_, D>,
    date: Date,
) -> Result<Vec<Slice<P>>, StoreError> {
    let sql = session.sql;
    let rows = session.fetch(&sql.shared.pnl_slices, Vec::new()).await?;
    let mut slices = BTreeMap::new();
    for row in &rows {
        let key = (row.handle(0)?, row.currency(1)?, row.text(2)?.to_owned());
        slices.insert(key, (row.dimensions(2)?, row.balance::<P>(3)?));
    }
    let later = session
        .fetch(&sql.shared.pnl_after, vec![codec::date(date)])
        .await?;
    let broken = || mismatch("a slice does not hold its postings", None);
    for row in &later {
        let key = (row.handle(0)?, row.currency(1)?, row.text(2)?.to_owned());
        let (_, balance) = slices.get_mut(&key).ok_or_else(broken)?;
        let amount: Amount<P> = row.amount(4)?;
        let (mut debit, mut credit) = (balance.debit(), balance.credit());
        match row.side(3)? {
            Side::Debit => debit = debit.checked_sub(amount).map_err(|_| broken())?,
            Side::Credit => credit = credit.checked_sub(amount).map_err(|_| broken())?,
        }
        let postings = balance.postings().checked_sub(1).ok_or_else(broken)?;
        *balance = Balance::from_parts(debit, credit, postings).map_err(|_| broken())?;
    }
    let mut result: Vec<Slice<P>> = slices
        .into_iter()
        .filter(|(_, (_, balance))| !balance.is_zero())
        .map(|((handle, currency, _), (dimensions, balance))| Slice {
            handle,
            currency,
            dimensions,
            balance,
        })
        .collect();
    result.sort_by(|a, b| {
        (a.handle, a.currency, &a.dimensions).cmp(&(b.handle, b.currency, &b.dimensions))
    });
    Ok(result)
}
