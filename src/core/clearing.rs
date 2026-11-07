//! Clearings: open items matched as records of the log.
//!
//! An apply clears an amount of a Settled debit posting against a Settled
//! credit posting of the same balance (account, currency, layer), dated no
//! earlier than either (a Pending posting is a hold's reservation, not an
//! open item: 419); a reset releases an apply, dated no earlier than it.
//! Both are leaves, so an applied amount is a fold of the log and never an
//! edited row.

use time::Date;

use super::calendar;
use super::state::{BalanceKey, ClearingView, EntryView, StateView};
use super::writeset::{Cleared, Done, IndexRow, Outcome, WriteSet};
use crate::error::{Rejection, ScaledAmount};
use crate::model::{
    Amount, ClearingAction, ClearingEvent, IdempotencyKey, Layer, PostingRecord, PostingRef,
    Record, Side,
};

/// A clearing's key (100 when it is not a valid idempotency key).
fn clearing_key(key: &str) -> Result<IdempotencyKey, Rejection> {
    IdempotencyKey::new(key).map_err(|reason| Rejection::InvalidKey { reason })
}

/// Stage 2 for clearings: an identical record under the key is a replay,
/// any other is a conflict.
fn replayed<const P: u8>(
    existing: Option<ClearingView<P>>,
    event: &ClearingEvent<P>,
) -> Result<Option<Outcome<P>>, Rejection> {
    match existing {
        Some(existing) if existing.event == *event => Ok(Some(Outcome::Unchanged(Done::Cleared(
            Cleared::new(existing.position, true),
        )))),
        Some(existing) => Err(Rejection::KeyConflict {
            position: existing.position,
        }),
        None => Ok(None),
    }
}

/// The posting a reference names, with its entry's booking date (410).
fn posting<const P: u8>(
    entry: Option<&EntryView<P>>,
    reference: PostingRef,
) -> Result<(&PostingRecord<P>, Date), Rejection> {
    entry
        .and_then(|entry| {
            let parts = entry.record.parts();
            let posting = parts.postings.get(usize::from(reference.index))?;
            Some((posting, parts.booking_date))
        })
        .ok_or(Rejection::ClearingTargetUnknown { posting: reference })
}

const fn key_of<const P: u8>(posting: &PostingRecord<P>) -> BalanceKey {
    BalanceKey {
        handle: posting.handle,
        currency: posting.currency,
        layer: posting.layer,
    }
}

/// Clears `amount` of a debit posting against a credit posting: 100, 120 →
/// replay or 200 → 300, 301 → 410 … 415, 419.
pub(crate) fn apply<const P: u8>(
    view: &impl StateView<P>,
    key: &str,
    debit: PostingRef,
    credit: PostingRef,
    amount: Amount<P>,
    date: Date,
) -> Result<Outcome<P>, Rejection> {
    let key = clearing_key(key)?;
    if !amount.is_positive() {
        return Err(Rejection::NonPositiveClearingAmount);
    }
    let existing = view.clearing_by_key(&key);
    let period = view.period_at(date);
    let entries = [view.entry(debit.entry), view.entry(credit.entry)];
    let applied = [view.applied(debit), view.applied(credit)];
    let event = ClearingEvent {
        action: ClearingAction::Apply {
            key: key.clone(),
            debit,
            credit,
            amount,
        },
        date,
    };
    if let Some(replay) = replayed(existing, &event)? {
        return Ok(replay);
    }
    calendar::open_on(period.as_ref(), date)?;
    let [debit_entry, credit_entry] = &entries;
    let cleared = [
        (debit, posting(debit_entry.as_ref(), debit)?, Side::Debit),
        (
            credit,
            posting(credit_entry.as_ref(), credit)?,
            Side::Credit,
        ),
    ];
    for &(reference, (posting, _), expected) in &cleared {
        if posting.side != expected {
            return Err(Rejection::ClearingSideMismatch {
                posting: reference,
                expected,
            });
        }
    }
    let [(_, (debit_posting, _), _), (_, (credit_posting, _), _)] = &cleared;
    let (debit_key, credit_key) = (key_of(debit_posting), key_of(credit_posting));
    if debit_key != credit_key {
        return Err(Rejection::ClearingKeyMismatch {
            debit: debit_key,
            credit: credit_key,
        });
    }
    // A date before every declaration of the currency precedes the postings
    // too, so 415 refuses it; the precision is checked where one is in force.
    if let Some(exponent) = view.currency(debit_key.currency, date)
        && !amount.fits_exponent(exponent)
    {
        return Err(Rejection::ClearingPrecision {
            currency: debit_key.currency,
            exponent,
        });
    }
    for (&(reference, (posting, _), _), applied) in cleared.iter().zip(applied) {
        let open = posting.amount.checked_sub(applied).unwrap_or(Amount::ZERO);
        if amount > open {
            return Err(Rejection::OverApplied {
                posting: reference,
                open: ScaledAmount::from(open),
                requested: ScaledAmount::from(amount),
            });
        }
    }
    for &(reference, (_, booked), _) in &cleared {
        if date < booked {
            return Err(Rejection::ClearingBeforePosting {
                posting: reference,
                booked,
            });
        }
    }
    // Both postings are of one balance (412), so of one layer: a Pending
    // posting is a hold's reservation, never an open item.
    if debit_key.layer != Layer::Settled {
        return Err(Rejection::ClearingNotSettled { posting: debit });
    }
    let position = view.header().size;
    Ok(Outcome::from(
        WriteSet::of(
            position,
            Record::Clearing(event),
            Done::Cleared(Cleared::new(position, false)),
        )
        .with(IndexRow::ClearingKey { key, position })
        .with(IndexRow::Applied {
            posting: debit,
            delta: amount,
        })
        .with(IndexRow::Applied {
            posting: credit,
            delta: amount,
        }),
    ))
}

/// Resets the clearing applied at `clearing`: 100 → replay or 200 → 300,
/// 301 → 416, 417, 418.
pub(crate) fn reset<const P: u8>(
    view: &impl StateView<P>,
    key: &str,
    clearing: u64,
    date: Date,
) -> Result<Outcome<P>, Rejection> {
    let key = clearing_key(key)?;
    let existing = view.clearing_by_key(&key);
    let period = view.period_at(date);
    let target = view.clearing_at(clearing);
    let event = ClearingEvent {
        action: ClearingAction::Reset {
            key: key.clone(),
            clearing,
        },
        date,
    };
    if let Some(replay) = replayed(existing, &event)? {
        return Ok(replay);
    }
    calendar::open_on(period.as_ref(), date)?;
    let unknown = Rejection::ClearingUnknown { clearing };
    let target = target.ok_or_else(|| unknown.clone())?;
    let ClearingAction::Apply {
        debit,
        credit,
        amount,
        ..
    } = target.event.action
    else {
        return Err(unknown);
    };
    if let Some(by) = target.reset_by {
        return Err(Rejection::ClearingAlreadyReset { clearing, by });
    }
    if date < target.event.date {
        return Err(Rejection::ResetBeforeClearing {
            cleared_on: target.event.date,
        });
    }
    let position = view.header().size;
    let released = amount.negate();
    Ok(Outcome::from(
        WriteSet::of(
            position,
            Record::Clearing(event),
            Done::Cleared(Cleared::new(position, false)),
        )
        .with(IndexRow::ClearingKey { key, position })
        .with(IndexRow::Applied {
            posting: debit,
            delta: released,
        })
        .with(IndexRow::Applied {
            posting: credit,
            delta: released,
        })
        .with(IndexRow::ClearingReset { clearing, position }),
    ))
}
