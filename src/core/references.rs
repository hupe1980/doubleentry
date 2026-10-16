//! Stage 4 for entries: the reversal target an entry names (the hold a
//! resolution names is checked next, by the hold rules).
//!
//! A reversal names its target by its committed id. It must find the target
//! (400), which must not act on a hold (450: a reservation ends with a void,
//! a mistaken capture with a new entry), be the only reversal of it (401),
//! be dated no earlier (402), and mirror it exactly: the same postings in
//! the same order with every side swapped (403). A reversal may itself be
//! reversed.

use super::context::EntryFacts;
use crate::error::Rejection;
use time::Date;

use crate::model::{Entry, EntryId, EntryRecord, Posting, PostingRecord, Side};

use super::structural::index;

/// The reversal rules: 400 → 450 → 401 → 402 → 403.
pub(crate) fn check<const P: u8>(
    facts: &EntryFacts<P>,
    entry: &EntryRecord<P>,
) -> Result<(), Rejection> {
    if let Some(target) = &facts.target {
        let original = target
            .entry
            .as_ref()
            .ok_or(Rejection::ReversalTargetUnknown { target: target.id })?;
        if original.record.parts().hold.is_some() {
            return Err(Rejection::HoldNotReversible { target: target.id });
        }
        if let Some(by) = target.reversed_by {
            return Err(Rejection::AlreadyReversed {
                target: target.id,
                by,
            });
        }
        let booked = original.record.parts().booking_date;
        if entry.parts().booking_date < booked {
            return Err(Rejection::ReversalBeforeTarget {
                target: target.id,
                booked,
            });
        }
        mirrors(target.id, &original.record, entry)?;
    }
    Ok(())
}

/// Whether `reversal` mirrors `original`: same postings, sides swapped.
fn mirrors<const P: u8>(
    target: EntryId,
    original: &EntryRecord<P>,
    reversal: &EntryRecord<P>,
) -> Result<(), Rejection> {
    let (original, reversal) = (&original.parts().postings, &reversal.parts().postings);
    if original.len() != reversal.len() {
        return Err(Rejection::NotAMirror {
            target,
            posting: None,
        });
    }
    match original
        .iter()
        .zip(reversal)
        .position(|(original, reversal)| mirror(original) != *reversal)
    {
        Some(posting) => Err(Rejection::NotAMirror {
            target,
            posting: Some(index(posting)),
        }),
        None => Ok(()),
    }
}

/// A posting with its side swapped.
fn mirror<const P: u8>(posting: &PostingRecord<P>) -> PostingRecord<P> {
    PostingRecord {
        side: posting.side.opposite(),
        ..posting.clone()
    }
}

/// The draft that reverses entry `target`, recorded as `original`: its
/// postings in order with every side swapped, by handle, booked on `on` under
/// `key`. Building it checks no rule; recording it checks them all.
pub(crate) fn reversal<const P: u8>(
    target: EntryId,
    original: &EntryRecord<P>,
    key: &str,
    on: Date,
) -> Entry<P> {
    original.parts().postings.iter().map(mirror).fold(
        Entry::new(key, on).reverses(target),
        |draft, posting| {
            let leg = match posting.side {
                Side::Debit => Posting::debit(posting.handle, posting.amount, posting.currency),
                Side::Credit => Posting::credit(posting.handle, posting.amount, posting.currency),
            };
            draft.posting(leg.dimensions(posting.dimensions.clone()))
        },
    )
}
