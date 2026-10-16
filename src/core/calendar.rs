//! The calendar: periods defined back to back, without gaps.
//!
//! The first period may start on any day; every later one starts the day
//! after the last one ends. A period is **open** until a seal record for it
//! exists and **sealed** from then on — there is no other state, and no way
//! to change it but sealing. The **watermark** is the last day of the last
//! sealed period: nothing can be dated on or before it.

use core::ops::RangeInclusive;

use time::{Date, Month};

use super::state::{PeriodView, StateView};
use super::writeset::{Done, IndexRow, Outcome, WriteSet};
use crate::error::{LabelField, Rejection};
use crate::model::{PeriodDefinition, PeriodId, Record, ValueError};

/// A period id (107 when it is not a label).
pub(crate) fn period_id(id: &str) -> Result<PeriodId, Rejection> {
    PeriodId::new(id).map_err(|reason| Rejection::InvalidLabel {
        field: LabelField::PeriodId,
        reason,
    })
}

/// The periods of `months` of `year`: (id `YYYY-MM`, first day, last day);
/// 107 for a month outside `1..=12` or a day that cannot be represented.
pub(crate) fn months(
    year: i32,
    months: RangeInclusive<u8>,
) -> Result<Vec<(String, Date, Date)>, Rejection> {
    let invalid = || Rejection::InvalidLabel {
        field: LabelField::PeriodId,
        reason: ValueError::OutOfRange,
    };
    months
        .map(|month| {
            let of = Month::try_from(month).map_err(|_| invalid())?;
            let start = Date::from_calendar_date(year, of, 1).map_err(|_| invalid())?;
            let end = Date::from_calendar_date(year, of, of.length(year)).map_err(|_| invalid())?;
            Ok((format!("{year:04}-{month:02}"), start, end))
        })
        .collect()
}

/// 300 and 301: `date` lies in a defined period that has no seal.
pub(crate) fn open_on<const P: u8>(
    period: Option<&PeriodView<P>>,
    date: Date,
) -> Result<(), Rejection> {
    let period = period.ok_or(Rejection::NoPeriod { date })?;
    if period.sealed.is_some() {
        return Err(Rejection::PeriodSealed {
            period: period.id.clone(),
            date,
        });
    }
    Ok(())
}

/// Defines a period: 107, 121 → identical repeat → 425 → 426.
pub(crate) fn define<const P: u8>(
    view: &impl StateView<P>,
    id: &str,
    start: Date,
    end: Date,
) -> Result<Outcome<P>, Rejection> {
    let id = period_id(id)?;
    let definition = PeriodDefinition::new(id.clone(), start, end)
        .map_err(|_| Rejection::InvalidPeriodRange { start, end })?;
    let existing = view.period(&id);
    let header = view.header();
    if let Some(period) = existing {
        if period.start == start && period.end == end {
            return Ok(Outcome::Unchanged(Done::Period(id)));
        }
        return Err(Rejection::PeriodExists {
            period: id,
            start: period.start,
            end: period.end,
        });
    }
    if let Some(last) = header.last_period {
        // A calendar that already reaches the last representable day has no
        // next period; any start is a gap.
        let expected_start = last.end.next_day().unwrap_or(last.end);
        if last.end.next_day() != Some(start) {
            return Err(Rejection::PeriodGap {
                expected_start,
                start,
            });
        }
    }
    let position = header.size;
    Ok(Outcome::from(
        WriteSet::of(
            position,
            Record::Period(definition),
            Done::Period(id.clone()),
        )
        .with(IndexRow::Period {
            id,
            start,
            end,
            position,
        }),
    ))
}
