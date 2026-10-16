//! Report and export agreement: on the simulation's ledgers, every query
//! form, statement, rollup, open-item list, assertion and the export's row
//! sequence (at page sizes 1, 7 and 1 000) are the same from the store as
//! from the reference [`Journal`].

use core::fmt::Debug;

use super::model::{Check, SplitMix64};
use super::simulation::{AXES, VALUES, month_start, period_id, plus};
use crate::error::Error;
use crate::export::{ExportRow, ExportScope};
use crate::model::Amount;
use crate::report::{AssertAt, BalanceAssertion, DateBasis};
use crate::store::Store;
use crate::{BalanceQuery, Journal, Ledger};

/// The same answer, or the same error, from both.
pub(super) fn same<T: PartialEq + Debug>(
    what: &str,
    expected: Result<T, Error>,
    actual: Result<T, Error>,
) -> Check {
    let expected = expected.map_err(|error| error.to_string());
    let actual = actual.map_err(|error| error.to_string());
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "{what}: the reference answered {expected:?}, the store {actual:?}"
        ))
    }
}

/// A random query of every form: window, basis, layer, clauses and pin.
fn query<const P: u8>(
    journal: &Journal<P>,
    random: &mut SplitMix64,
) -> Result<BalanceQuery, String> {
    let size = journal.head().size;
    let day = |random: &mut SplitMix64| {
        let first = month_start(0).ok_or("no first month")?;
        let offset = i64::try_from(random.below(500)).map_err(|error| error.to_string())?;
        Ok::<_, String>(plus(first, offset.saturating_sub(30)))
    };
    let mut query = match random.below(6) {
        0 => BalanceQuery::all(),
        1 => BalanceQuery::through_position(random.below(size.saturating_add(2))),
        2 => BalanceQuery::through(day(random)?),
        3 => {
            let (a, b) = (day(random)?, day(random)?);
            BalanceQuery::between(a.min(b), a.max(b))
        }
        4 => BalanceQuery::period(
            period_id(u32::try_from(random.below(26)).unwrap_or(0)).unwrap_or_default(),
        ),
        _ => BalanceQuery::between(day(random)?, day(random)?),
    };
    if random.chance(40) {
        query = query.by_value_date();
    }
    if random.chance(10) {
        query = query.pending();
    }
    for _ in 0..random.below(3) {
        let axis = random.pick(&AXES).copied().unwrap_or("region");
        query = if random.chance(50) {
            query.matching(axis, random.pick(&VALUES).copied().unwrap_or("eu"))
        } else {
            query.missing(axis)
        };
    }
    if random.chance(25) {
        let head = journal
            .head_at(random.between(1, size))
            .map_err(|error| error.to_string())?;
        query = query.pinned(head);
    }
    Ok(query)
}

/// Random reads of every report on the store, against the reference.
pub(crate) async fn report_checks<S: Store<P>, const P: u8>(
    books: &Ledger<S, P>,
    journal: &Journal<P>,
    random: &mut SplitMix64,
) -> Check {
    let unfiltered = journal
        .trial_balance(&BalanceQuery::all())
        .map_err(|error| error.to_string())?;
    let currencies: Vec<_> = unfiltered.currencies().collect();
    for _ in 0..3 {
        let query = query(journal, random)?;
        let what = |report: &str| format!("{report} of {query:?}");
        same(
            &what("the trial balance"),
            journal.trial_balance(&query),
            books.trial_balance(&query).await,
        )?;
        if let Some(currency) = random.pick(&currencies).copied() {
            same(
                &what("the rollup"),
                journal.rollup(currency, &query),
                books.rollup(currency, &query).await,
            )?;
        }
        let Some(row) = random.pick(unfiltered.rows()) else {
            continue;
        };
        let (account, currency) = (&row.path, row.currency);
        same(
            &what(&format!("the balance of {account}")),
            journal.balance(account, currency, &query),
            books.balance(account, currency, &query).await,
        )?;
        same(
            &what(&format!("the statement of {account}")),
            journal.statement(account, currency, &query),
            books.statement(account, currency, &query).await,
        )?;
        same(
            &what(&format!("the open items of {account}")),
            journal.open_items(account, currency, &query),
            books.open_items(account, currency, &query).await,
        )?;
        let net = row.balance.net();
        let off = net.checked_add(Amount::from_minor(1).map_err(|error| error.to_string())?);
        let at = AssertAt::EndOfDay {
            date: month_start(u32::try_from(random.below(18)).unwrap_or(0)).ok_or("no month")?,
            basis: if random.chance(50) {
                DateBasis::Booking
            } else {
                DateBasis::Value
            },
        };
        let claims = [
            BalanceAssertion::new(
                account,
                currency,
                AssertAt::Position(random.below(journal.head().size)),
                net,
            ),
            BalanceAssertion::new(account, currency, at, off.unwrap_or(net)),
            BalanceAssertion::new(account, currency, at, net).pending(),
        ];
        same(
            &format!("the assertions on {account}"),
            journal.check_assertions(&claims),
            books.check_assertions(&claims).await,
        )?;
    }
    Ok(())
}

/// The export's rows from the store, at page sizes 1, 7 and 1 000, against
/// the reference's.
pub(crate) async fn export_checks<S: Store<P>, const P: u8>(
    books: &Ledger<S, P>,
    journal: &Journal<P>,
) -> Check {
    let head = journal.head();
    let expected: Vec<ExportRow> = journal
        .export(ExportScope::All, Some(head))
        .and_then(Iterator::collect)
        .map_err(|error| error.to_string())?;
    for limit in [1, 7, 1_000] {
        let mut export = books
            .export(ExportScope::All, Some(head))
            .await
            .map_err(|error| error.to_string())?;
        let mut found = Vec::with_capacity(expected.len());
        while let Some(page) = export
            .next_page(limit)
            .await
            .map_err(|error| error.to_string())?
        {
            found.extend(page);
        }
        if found != expected {
            let at = found
                .iter()
                .zip(&expected)
                .position(|(found, expected)| found != expected)
                .unwrap_or(found.len().min(expected.len()));
            return Err(format!(
                "the export at page size {limit} differs from row {at}: expected {:?}, got {:?}",
                expected.get(at),
                found.get(at)
            ));
        }
    }
    crate::export::verify_with_precision::<P>(expected, &head).map_err(|error| error.to_string())
}
