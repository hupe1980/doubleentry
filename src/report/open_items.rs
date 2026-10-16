//! Open items: the postings of an account not yet offset by clearings, with
//! what was applied and what remains — now, or as of a date.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use time::Date;

use super::query::Selection;
use crate::error::{IntegrityError, IntegrityKind};
use crate::model::{Amount, ClearingAction, ClearingEvent, EntryId, PostingRef, Side, pad};
use crate::store::PostingLine;

/// A posting with an amount still open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct OpenItem<const P: u8 = 2> {
    /// The entry's position in the log.
    pub position: u64,
    /// The entry.
    pub entry: EntryId,
    /// The posting's index in the entry.
    pub posting: u16,
    /// The booking date.
    pub booking_date: Date,
    /// The value date.
    pub value_date: Date,
    /// The side.
    pub side: Side,
    /// The posting's amount.
    pub original: Amount<P>,
    /// The amount cleared against it by clearings not reset.
    pub applied: Amount<P>,
    /// `original − applied`, positive.
    pub residual: Amount<P>,
}

impl<const P: u8> OpenItem<P> {
    /// The posting, as a clearing names it.
    #[must_use]
    pub const fn posting_ref(&self) -> PostingRef {
        PostingRef::at(self.entry, self.posting)
    }
}

impl<const P: u8> fmt::Display for OpenItem<P> {
    /// `2026-03-15 7/0 debit 119.00, applied 100.00, open 19.00`, honouring
    /// width, fill and alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        pad(
            f,
            &format!(
                "{} {}/{} {} {}, applied {}, open {}",
                self.booking_date,
                self.position,
                self.posting,
                self.side,
                self.original,
                self.applied,
                self.residual
            ),
        )
    }
}

/// An apply as the fold keeps it.
#[derive(Debug)]
struct Applied<const P: u8> {
    postings: [PostingRef; 2],
    amount: Amount<P>,
}

/// Folds one key's posting rows and the clearing records touching them into
/// its open items.
#[derive(Debug)]
pub(crate) struct OpenItemsFold<'a, const P: u8> {
    selection: &'a Selection,
    lines: Vec<PostingLine<P>>,
    applies: BTreeMap<u64, Applied<P>>,
    resets: BTreeSet<u64>,
}

impl<'a, const P: u8> OpenItemsFold<'a, P> {
    /// An empty fold.
    pub(crate) const fn new(selection: &'a Selection) -> Self {
        Self {
            selection,
            lines: Vec::new(),
            applies: BTreeMap::new(),
            resets: BTreeSet::new(),
        }
    }

    /// Keeps `line` when the selection counts it.
    pub(crate) fn feed(&mut self, line: PostingLine<P>) {
        if self.selection.selects(&line) {
            self.lines.push(line);
        }
    }

    /// Takes the clearing record at `position` into account, when it lies in
    /// the prefix and is dated on or before the window's end.
    pub(crate) fn clearing(&mut self, position: u64, event: &ClearingEvent<P>) {
        let in_force = position < self.selection.bound()
            && self.selection.end().is_none_or(|end| event.date <= end);
        if !in_force {
            return;
        }
        match &event.action {
            ClearingAction::Apply {
                debit,
                credit,
                amount,
                ..
            } => {
                self.applies.insert(
                    position,
                    Applied {
                        postings: [*debit, *credit],
                        amount: *amount,
                    },
                );
            }
            ClearingAction::Reset { clearing, .. } => {
                self.resets.insert(*clearing);
            }
        }
    }

    /// The open items, by (position, posting index).
    pub(crate) fn finish(self) -> Result<Vec<OpenItem<P>>, IntegrityError> {
        let mut applied: BTreeMap<PostingRef, Amount<P>> = BTreeMap::new();
        for (position, apply) in &self.applies {
            if self.resets.contains(position) {
                continue;
            }
            for posting in apply.postings {
                let total = applied.entry(posting).or_insert(Amount::ZERO);
                *total = total
                    .checked_add(apply.amount)
                    .map_err(|_| overcleared(*position))?;
            }
        }
        let mut items = Vec::new();
        for line in self.lines {
            let cleared = applied
                .get(&line.posting_ref())
                .copied()
                .unwrap_or(Amount::ZERO);
            let residual = line
                .amount
                .checked_sub(cleared)
                .map_err(|_| overcleared(line.position))?;
            if residual.is_negative() {
                return Err(overcleared(line.position));
            }
            if residual.is_positive() {
                items.push(OpenItem {
                    position: line.position,
                    entry: line.entry,
                    posting: line.index,
                    booking_date: line.booking,
                    value_date: line.value,
                    side: line.side,
                    original: line.amount,
                    applied: cleared,
                    residual,
                });
            }
        }
        items.sort_by_key(|item| (item.position, item.posting));
        Ok(items)
    }
}

/// More cleared against a posting than its amount: clearing records the
/// rule core would not have written.
const fn overcleared(position: u64) -> IntegrityError {
    IntegrityError::new(IntegrityKind::ClearingMismatch, Some(position))
}
