//! Accounting periods and their lifecycle.
//!
//! A period is a bounded range of booking dates with a state. Sealing a period
//! stops further postings inside it; a correction that belongs to a sealed period
//! is booked in the current open period instead, carrying a reference to the
//! original date. That is the only treatment compatible with an append-only log:
//! reopening a period would mean rewriting history that has already been
//! committed to.

use std::collections::BTreeMap;

use time::Date;

use crate::canonical::{Canonical, CanonicalWriter};
use crate::dimensions::{DimensionError, Label};
use crate::serde_support::validating_string_serde;

/// Where a period sits in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PeriodState {
    /// Accepts postings.
    Open,
    /// Undergoing verification; no new postings accepted.
    Closing,
    /// Closed and committed to. Immutable.
    Sealed,
}

impl PeriodState {
    /// True when postings may be booked into the period.
    #[must_use]
    pub const fn accepts_postings(self) -> bool {
        matches!(self, Self::Open)
    }
}

impl std::fmt::Display for PeriodState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Closing => "closing",
            Self::Sealed => "sealed",
        })
    }
}

/// Identifier for a period, such as `"2026-03"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeriodId(Label);

validating_string_serde!(PeriodId);

impl PeriodId {
    /// Validates and wraps an identifier.
    pub fn new(s: impl Into<String>) -> Result<Self, DimensionError> {
        Label::new(s).map(Self)
    }

    /// The underlying identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for PeriodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl Canonical for PeriodId {
    fn encode(&self, w: &mut CanonicalWriter) {
        self.0.encode(w);
    }
}

/// Identifies one ledger.
///
/// A ledger is the isolation boundary: its own log, its own dense index space,
/// its own Merkle tree, its own seal chain, its own accounts. Nothing crosses
/// between two of them.
///
/// That is deliberately stronger than a filter column. A seal commits to one
/// entity's history, so a shared log would have each tenant's seal committing to
/// every other tenant's entries — and an inclusion proof shown to one auditor
/// would reveal how many entries the others hold. Where books must be separable
/// for legal or contractual reasons, the separation has to be structural.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerId(Label);

validating_string_serde!(LedgerId);

impl LedgerId {
    /// Validates and wraps an identifier.
    pub fn new(s: impl Into<String>) -> Result<Self, DimensionError> {
        Label::new(s).map(Self)
    }

    /// The underlying identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for LedgerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl Canonical for LedgerId {
    fn encode(&self, w: &mut CanonicalWriter) {
        self.0.encode(w);
    }
}

/// Failure defining a period.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeriodError {
    /// The end date preceded the start date.
    #[error("period {id} ends on {end} before it starts on {start}")]
    EndBeforeStart {
        /// The offending period.
        id: PeriodId,
        /// Start date.
        start: Date,
        /// End date.
        end: Date,
    },
    /// The range overlapped an already-defined period.
    #[error("period {id} overlaps existing period {existing}")]
    Overlap {
        /// The period being added.
        id: PeriodId,
        /// The period it collides with.
        existing: PeriodId,
    },
    /// A period with this identifier already exists.
    #[error("period {id} is already defined")]
    Duplicate {
        /// The duplicate identifier.
        id: PeriodId,
    },
    /// No period with this identifier exists.
    #[error("period {id} is not defined")]
    Unknown {
        /// The missing identifier.
        id: PeriodId,
    },
    /// The requested lifecycle transition is not permitted.
    #[error("period {id} cannot move from {from} to {to}")]
    InvalidTransition {
        /// The period.
        id: PeriodId,
        /// Current state.
        from: PeriodState,
        /// Requested state.
        to: PeriodState,
    },
}

/// A bounded range of booking dates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Period {
    /// Identifier.
    pub id: PeriodId,
    /// First booking date in the period.
    pub start: Date,
    /// Last booking date in the period, inclusive.
    pub end: Date,
    /// Lifecycle state.
    pub state: PeriodState,
}

impl Period {
    /// Creates an open period.
    pub fn new(id: PeriodId, start: Date, end: Date) -> Result<Self, PeriodError> {
        if end < start {
            return Err(PeriodError::EndBeforeStart { id, start, end });
        }
        Ok(Self {
            id,
            start,
            end,
            state: PeriodState::Open,
        })
    }

    /// True when `date` falls inside the period.
    #[must_use]
    pub fn contains(&self, date: Date) -> bool {
        date >= self.start && date <= self.end
    }

    /// True when the ranges intersect.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

/// The set of periods a ledger recognises.
///
/// An empty calendar imposes no restriction: a ledger that does not manage
/// periods accepts any booking date. Dates outside every defined period are
/// likewise unrestricted — only an explicitly closing or sealed period blocks a
/// posting.
#[derive(Debug, Clone, Default)]
pub struct PeriodCalendar {
    periods: BTreeMap<PeriodId, Period>,
}

impl PeriodCalendar {
    /// Creates an empty calendar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines a new period.
    pub fn define(&mut self, period: Period) -> Result<(), PeriodError> {
        if self.periods.contains_key(&period.id) {
            return Err(PeriodError::Duplicate { id: period.id });
        }
        if let Some(existing) = self.periods.values().find(|p| p.overlaps(&period)) {
            return Err(PeriodError::Overlap {
                id: period.id.clone(),
                existing: existing.id.clone(),
            });
        }
        self.periods.insert(period.id.clone(), period);
        Ok(())
    }

    /// The period containing `date`, if any.
    #[must_use]
    pub fn period_on(&self, date: Date) -> Option<&Period> {
        self.periods.values().find(|p| p.contains(date))
    }

    /// The state governing `date`.
    ///
    /// Returns [`PeriodState::Open`] when no period covers the date.
    #[must_use]
    pub fn state_on(&self, date: Date) -> PeriodState {
        self.period_on(date).map_or(PeriodState::Open, |p| p.state)
    }

    /// True when a posting may be booked on `date`.
    #[must_use]
    pub fn accepts(&self, date: Date) -> bool {
        self.state_on(date).accepts_postings()
    }

    /// Looks up a period.
    #[must_use]
    pub fn get(&self, id: &PeriodId) -> Option<&Period> {
        self.periods.get(id)
    }

    /// Moves a period to the next state.
    ///
    /// Permitted transitions are `Open → Closing`, `Closing → Sealed`, and
    /// `Closing → Open` to abandon a close that failed verification. A sealed
    /// period never transitions again.
    pub fn transition(&mut self, id: &PeriodId, to: PeriodState) -> Result<(), PeriodError> {
        let period = self
            .periods
            .get_mut(id)
            .ok_or_else(|| PeriodError::Unknown { id: id.clone() })?;
        let permitted = matches!(
            (period.state, to),
            (PeriodState::Open, PeriodState::Closing)
                | (PeriodState::Closing, PeriodState::Sealed)
                | (PeriodState::Closing, PeriodState::Open)
        );
        if !permitted {
            return Err(PeriodError::InvalidTransition {
                id: id.clone(),
                from: period.state,
                to,
            });
        }
        period.state = to;
        Ok(())
    }

    /// Every period, in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = &Period> {
        self.periods.values()
    }

    /// Number of defined periods.
    #[must_use]
    pub fn len(&self) -> usize {
        self.periods.len()
    }

    /// True when no periods are defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.periods.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn pid(s: &str) -> PeriodId {
        PeriodId::new(s).expect("valid identifier")
    }

    fn march() -> Period {
        Period::new(pid("2026-03"), date!(2026 - 03 - 01), date!(2026 - 03 - 31))
            .expect("valid range")
    }

    #[test]
    fn rejects_inverted_ranges() {
        assert!(matches!(
            Period::new(pid("bad"), date!(2026 - 03 - 31), date!(2026 - 03 - 01)),
            Err(PeriodError::EndBeforeStart { .. })
        ));
    }

    #[test]
    fn contains_is_inclusive_at_both_ends() {
        let p = march();
        assert!(p.contains(date!(2026 - 03 - 01)));
        assert!(p.contains(date!(2026 - 03 - 31)));
        assert!(!p.contains(date!(2026 - 02 - 28)));
        assert!(!p.contains(date!(2026 - 04 - 01)));
    }

    #[test]
    fn empty_calendar_accepts_everything() {
        let c = PeriodCalendar::new();
        assert!(c.accepts(date!(2026 - 03 - 15)));
        assert_eq!(c.state_on(date!(2026 - 03 - 15)), PeriodState::Open);
    }

    #[test]
    fn dates_outside_every_period_are_unrestricted() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        c.transition(&pid("2026-03"), PeriodState::Closing)
            .expect("ok");
        c.transition(&pid("2026-03"), PeriodState::Sealed)
            .expect("ok");
        assert!(!c.accepts(date!(2026 - 03 - 15)));
        assert!(c.accepts(date!(2026 - 04 - 15)));
    }

    #[test]
    fn rejects_overlapping_periods() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        let overlapping = Period::new(
            pid("2026-03b"),
            date!(2026 - 03 - 15),
            date!(2026 - 04 - 15),
        )
        .expect("valid range");
        assert!(matches!(
            c.define(overlapping),
            Err(PeriodError::Overlap { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_identifiers() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        assert!(matches!(
            c.define(march()),
            Err(PeriodError::Duplicate { .. })
        ));
    }

    #[test]
    fn adjacent_periods_do_not_overlap() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        let april = Period::new(pid("2026-04"), date!(2026 - 04 - 01), date!(2026 - 04 - 30))
            .expect("valid range");
        assert!(c.define(april).is_ok());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn lifecycle_follows_the_permitted_transitions() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        let id = pid("2026-03");

        // Cannot seal directly from open.
        assert!(matches!(
            c.transition(&id, PeriodState::Sealed),
            Err(PeriodError::InvalidTransition { .. })
        ));

        c.transition(&id, PeriodState::Closing)
            .expect("open to closing");
        // A failed close may be abandoned.
        c.transition(&id, PeriodState::Open)
            .expect("closing to open");
        c.transition(&id, PeriodState::Closing)
            .expect("open to closing");
        c.transition(&id, PeriodState::Sealed)
            .expect("closing to sealed");

        // Sealed is terminal.
        for to in [PeriodState::Open, PeriodState::Closing, PeriodState::Sealed] {
            assert!(matches!(
                c.transition(&id, to),
                Err(PeriodError::InvalidTransition { .. })
            ));
        }
    }

    #[test]
    fn closing_periods_stop_accepting_postings() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        c.transition(&pid("2026-03"), PeriodState::Closing)
            .expect("ok");
        assert!(!c.accepts(date!(2026 - 03 - 15)));
    }

    #[test]
    fn unknown_period_transition_is_an_error() {
        let mut c = PeriodCalendar::new();
        assert!(matches!(
            c.transition(&pid("nope"), PeriodState::Closing),
            Err(PeriodError::Unknown { .. })
        ));
    }
}
