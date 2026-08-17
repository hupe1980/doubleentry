//! Accounting periods and their lifecycle.
//!
//! A period is a bounded range of booking dates with a state. Sealing a period
//! stops further postings inside it; a correction that belongs to a sealed period
//! is booked in the current open period instead, carrying a reference to the
//! original date. That is the only treatment compatible with an append-only log:
//! reopening a period would mean rewriting history that has already been
//! committed to.
//!
//! Sealing stops rather more than the period itself, and it has to. A seal
//! commits to a *cumulative* closing balance — every entry booked on or before
//! the period's last day — so that claim only holds while nothing can be added
//! at or before that day afterwards. The
//! [watermark](PeriodCalendar#the-sealed-watermark) is what makes it hold, and
//! [`PeriodCalendar::check_sealable`] is what keeps periods sealing in the order
//! that keeps it true.

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
    /// Sealing was attempted on a period that is not in [`PeriodState::Closing`].
    #[error("period {id} is {state}; only a closing period can be sealed")]
    NotClosing {
        /// The period.
        id: PeriodId,
        /// Its current state.
        state: PeriodState,
    },
    /// The period ends at or before the sealed watermark.
    ///
    /// Its books are already frozen by an earlier seal, so a closing balance
    /// taken now would be a second, conflicting commitment to the same dates.
    #[error(
        "period {id} ends on {ends}, at or before the sealed watermark of \
         {sealed_through}; periods are sealed in date order"
    )]
    SealedOutOfOrder {
        /// The period that cannot be sealed.
        id: PeriodId,
        /// Its last booking date.
        ends: Date,
        /// The greatest end date already sealed.
        sealed_through: Date,
    },
    /// An earlier period is still unsealed.
    ///
    /// A seal's closing balance is cumulative through its period's last day, so
    /// it is only a stable claim once every earlier day is frozen too.
    #[error("period {id} cannot be sealed while the earlier period {predecessor} is {state}")]
    UnsealedPredecessor {
        /// The period being sealed.
        id: PeriodId,
        /// The earlier period standing in the way.
        predecessor: PeriodId,
        /// That period's state.
        state: PeriodState,
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
/// periods accepts any booking date, and a date no period covers is likewise
/// unrestricted — **until something is sealed**.
///
/// # The sealed watermark
///
/// Sealing commits to a *cumulative* closing balance: every entry booked on or
/// before the period's last day, and nothing else. That claim is only stable if
/// nothing can be booked at or before that day afterwards, so
/// [`sealed_through`](Self::sealed_through) — the greatest end date of any
/// sealed period — closes the whole range below it, whether or not a period
/// covers each date.
///
/// Without the watermark the guarantee has two holes, and both are reached by
/// ordinary, individually legal writes rather than by tampering: sealing March
/// while February is still open, and booking into a gap the calendar never
/// defined. Either one restates a sealed period's closing balance after the
/// fact. [`Self::check_sealable`] closes the first; the watermark closes the
/// second.
///
/// Periods are kept indexed by start date as well as by identifier. Every
/// validated entry asks "what governs this booking date", and a calendar with a
/// decade of daily or weekly periods would otherwise answer it by scanning.
#[derive(Debug, Clone, Default)]
pub struct PeriodCalendar {
    periods: BTreeMap<PeriodId, Period>,
    /// Start date to identifier. Non-overlap makes this a total order on the
    /// periods, so the period covering a date is the last one starting at or
    /// before it — if that one reaches far enough.
    by_start: BTreeMap<Date, PeriodId>,
    /// Greatest end date over the sealed periods, maintained as they seal.
    ///
    /// Derived state, but cheap to keep exact: sealing is terminal and periods
    /// are never removed, so it only ever moves forward. Recomputing it per
    /// validated entry would make every booking a scan of the calendar.
    sealed_through: Option<Date>,
}

impl PeriodCalendar {
    /// Creates an empty calendar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds a calendar from stored periods, states included.
    ///
    /// The state matters: a sealed period that comes back open would accept
    /// postings into books that have already been committed to.
    ///
    /// **The [watermark](Self#the-sealed-watermark) is restored with them**, in
    /// any order and without a separate stored field. Each sealed period
    /// advances it as it is defined, and it only ever moves forward, so
    /// replaying a period table reconstructs it exactly. That is what makes a
    /// restart durable: without it the first restart would reopen every gap the
    /// seals had closed. [`Self::ensure`], which is the replay path a durable
    /// backend actually takes, carries it across too.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`PeriodCalendar::define`].
    pub fn from_periods(periods: impl IntoIterator<Item = Period>) -> Result<Self, PeriodError> {
        let mut calendar = Self::new();
        for period in periods {
            calendar.define(period)?;
        }
        Ok(calendar)
    }

    /// Defines a new period.
    ///
    /// The period is stored with whatever state it carries, so a calendar
    /// restored from a backend keeps its sealed periods sealed.
    ///
    /// # Errors
    ///
    /// Returns [`PeriodError::Duplicate`] for a repeated identifier and
    /// [`PeriodError::Overlap`] when the range meets one already defined — a
    /// booking date has to resolve to exactly one period.
    pub fn define(&mut self, period: Period) -> Result<(), PeriodError> {
        if self.periods.contains_key(&period.id) {
            return Err(PeriodError::Duplicate { id: period.id });
        }
        // With no overlaps among what is already defined, a new range can only
        // collide with its two neighbours by start date.
        let before = self
            .by_start
            .range(..=period.start)
            .next_back()
            .map(|(_, id)| id);
        let after = self.by_start.range(period.start..).next().map(|(_, id)| id);
        for neighbour in [before, after].into_iter().flatten() {
            if let Some(existing) = self.periods.get(neighbour)
                && existing.overlaps(&period)
            {
                return Err(PeriodError::Overlap {
                    id: period.id.clone(),
                    existing: existing.id.clone(),
                });
            }
        }

        self.by_start.insert(period.start, period.id.clone());
        self.mark_sealed_through(&period);
        self.periods.insert(period.id.clone(), period);
        Ok(())
    }

    /// Advances the watermark if `period` is sealed and reaches further than it.
    fn mark_sealed_through(&mut self, period: &Period) {
        if period.state == PeriodState::Sealed
            && self
                .sealed_through
                .is_none_or(|through| period.end > through)
        {
            self.sealed_through = Some(period.end);
        }
    }

    /// The last booking date the books are sealed through, if anything is.
    ///
    /// No entry may be booked on or before this date: doing so would restate a
    /// closing balance a seal has already committed to. See the type-level
    /// documentation for why this is a property of the calendar rather than of
    /// the individual periods.
    ///
    /// Derived, never stored: each sealed period advances it as it is defined,
    /// so [`from_periods`](Self::from_periods) and [`ensure`](Self::ensure)
    /// reconstruct it from a period table and a restart keeps every gap the
    /// seals had closed.
    #[must_use]
    pub fn sealed_through(&self) -> Option<Date> {
        self.sealed_through
    }

    /// Defines a period unless an identical definition is already present.
    ///
    /// For a backend replaying its period table on start-up: re-reading a
    /// definition it wrote itself must not be a duplicate-identifier error.
    /// A *different* range under the same identifier still is, because that
    /// would move the boundary of a period entries have already been booked
    /// into.
    ///
    /// # Errors
    ///
    /// Returns [`PeriodError::Duplicate`] when the identifier is already used
    /// for a different range, and [`PeriodError::Overlap`] as [`Self::define`]
    /// does.
    pub fn ensure(&mut self, period: Period) -> Result<(), PeriodError> {
        match self.periods.get(&period.id) {
            Some(existing) if existing.start == period.start && existing.end == period.end => {
                self.mark_sealed_through(&period);
                self.periods.insert(period.id.clone(), period);
                Ok(())
            }
            Some(_) => Err(PeriodError::Duplicate { id: period.id }),
            None => self.define(period),
        }
    }

    /// The period containing `date`, if any.
    #[must_use]
    pub fn period_on(&self, date: Date) -> Option<&Period> {
        let (_, id) = self.by_start.range(..=date).next_back()?;
        self.periods.get(id).filter(|p| p.contains(date))
    }

    /// The state governing `date`.
    ///
    /// The sealed watermark is consulted first, so every date at or before
    /// [`sealed_through`](Self::sealed_through) reports
    /// [`PeriodState::Sealed`] — including one that no defined period covers.
    /// A gap in the calendar below a seal is not an opening to book through; it
    /// is a range whose closing balance has already been committed to.
    ///
    /// Otherwise this is the covering period's own state, and
    /// [`PeriodState::Open`] when no period covers the date at all.
    #[must_use]
    pub fn state_on(&self, date: Date) -> PeriodState {
        if self.sealed_through.is_some_and(|through| date <= through) {
            return PeriodState::Sealed;
        }
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
        let sealed = period.clone();
        self.mark_sealed_through(&sealed);
        Ok(())
    }

    /// Checks that `id` may be sealed next, and returns its definition.
    ///
    /// The single place the sealing preconditions live, so the in-memory
    /// journal and every durable backend enforce the same rule rather than
    /// three copies of it that can drift.
    ///
    /// Three things must hold, and only the first is about the period itself:
    ///
    /// 1. It is in [`PeriodState::Closing`]. Stopping new postings is a
    ///    separate, earlier decision, so that verification runs against a set
    ///    that can no longer grow underneath it.
    /// 2. Every **earlier** defined period is already sealed. A seal's closing
    ///    balance is cumulative through its period's last day, so an earlier
    ///    period still accepting postings can restate it afterwards.
    /// 3. It ends **after** the sealed watermark. Sealing inside a range
    ///    already frozen would be a second, conflicting commitment to the same
    ///    dates.
    ///
    /// # Errors
    ///
    /// Returns [`PeriodError::Unknown`], [`PeriodError::NotClosing`],
    /// [`PeriodError::UnsealedPredecessor`], or
    /// [`PeriodError::SealedOutOfOrder`].
    pub fn check_sealable(&self, id: &PeriodId) -> Result<&Period, PeriodError> {
        let period = self
            .periods
            .get(id)
            .ok_or_else(|| PeriodError::Unknown { id: id.clone() })?;

        if period.state != PeriodState::Closing {
            return Err(PeriodError::NotClosing {
                id: id.clone(),
                state: period.state,
            });
        }

        // Periods are disjoint, so "earlier" is exactly "starts earlier": a
        // period that began before this one cannot reach into it without
        // overlapping, which `define` refuses. The index makes that a range
        // scan rather than a walk of the whole calendar, and it is free next to
        // what sealing already costs — the closing trial balance folds every
        // entry in the journal.
        if let Some(blocker) = self
            .by_start
            .range(..period.start)
            .filter_map(|(_, other)| self.periods.get(other))
            .find(|other| other.state != PeriodState::Sealed)
        {
            return Err(PeriodError::UnsealedPredecessor {
                id: id.clone(),
                predecessor: blocker.id.clone(),
                state: blocker.state,
            });
        }

        if let Some(through) = self.sealed_through
            && period.end <= through
        {
            return Err(PeriodError::SealedOutOfOrder {
                id: id.clone(),
                ends: period.end,
                sealed_through: through,
            });
        }

        Ok(period)
    }

    /// Every period, in start-date order.
    pub fn iter(&self) -> impl Iterator<Item = &Period> {
        self.by_start.values().filter_map(|id| self.periods.get(id))
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
    fn dates_after_every_period_are_unrestricted() {
        // Forward of the watermark a gap really is open: the books have not
        // been closed through those days, so nothing has committed to them.
        // Behind it they are not — see the watermark tests below.
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
    fn overlap_is_caught_from_either_side() {
        // The index only inspects the two neighbours by start date, so a range
        // that straddles an existing one has to be caught from both directions.
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");

        let starts_before = Period::new(
            pid("straddle-left"),
            date!(2026 - 02 - 15),
            date!(2026 - 03 - 10),
        )
        .expect("valid range");
        assert!(matches!(
            c.define(starts_before),
            Err(PeriodError::Overlap { .. })
        ));

        let starts_after = Period::new(
            pid("straddle-right"),
            date!(2026 - 03 - 20),
            date!(2026 - 04 - 10),
        )
        .expect("valid range");
        assert!(matches!(
            c.define(starts_after),
            Err(PeriodError::Overlap { .. })
        ));

        let swallows = Period::new(pid("swallow"), date!(2026 - 01 - 01), date!(2026 - 12 - 31))
            .expect("valid range");
        assert!(matches!(
            c.define(swallows),
            Err(PeriodError::Overlap { .. })
        ));

        let identical = Period::new(
            pid("same-range"),
            date!(2026 - 03 - 01),
            date!(2026 - 03 - 31),
        )
        .expect("valid range");
        assert!(matches!(
            c.define(identical),
            Err(PeriodError::Overlap { .. })
        ));
    }

    #[test]
    fn lookup_finds_the_right_period_among_many() {
        let mut c = PeriodCalendar::new();
        for month in 1u8..=12 {
            let start = time::Date::from_calendar_date(
                2026,
                time::Month::try_from(month).expect("valid month"),
                1,
            )
            .expect("valid date");
            let end = start
                .replace_day(start.month().length(2026))
                .expect("valid date");
            c.define(Period::new(pid(&format!("2026-{month:02}")), start, end).expect("valid"))
                .expect("defines");
        }
        assert_eq!(c.len(), 12);
        assert_eq!(
            c.period_on(date!(2026 - 07 - 04)).map(|p| p.id.to_string()),
            Some("2026-07".to_owned())
        );
        assert_eq!(
            c.period_on(date!(2026 - 12 - 31)).map(|p| p.id.to_string()),
            Some("2026-12".to_owned())
        );
        assert!(c.period_on(date!(2025 - 12 - 31)).is_none());
        assert!(c.period_on(date!(2027 - 01 - 01)).is_none());

        // Iteration follows the calendar, not the identifier spelling.
        let order: Vec<String> = c.iter().map(|p| p.id.to_string()).collect();
        assert_eq!(order.first().map(String::as_str), Some("2026-01"));
        assert_eq!(order.last().map(String::as_str), Some("2026-12"));
    }

    #[test]
    fn a_calendar_rebuilds_from_stored_periods_with_their_states() {
        let mut original = PeriodCalendar::new();
        original.define(march()).expect("defines");
        original
            .transition(&pid("2026-03"), PeriodState::Closing)
            .expect("ok");
        original
            .transition(&pid("2026-03"), PeriodState::Sealed)
            .expect("ok");

        let restored = PeriodCalendar::from_periods(original.iter().cloned()).expect("rebuilds");
        assert_eq!(
            restored.state_on(date!(2026 - 03 - 15)),
            PeriodState::Sealed
        );
        assert!(!restored.accepts(date!(2026 - 03 - 15)));
    }

    #[test]
    fn unknown_period_transition_is_an_error() {
        let mut c = PeriodCalendar::new();
        assert!(matches!(
            c.transition(&pid("nope"), PeriodState::Closing),
            Err(PeriodError::Unknown { .. })
        ));
    }

    /// Defines `id` over `start..=end` and drives it all the way to sealed.
    fn seal(c: &mut PeriodCalendar, id: &str, start: Date, end: Date) {
        c.define(Period::new(pid(id), start, end).expect("valid range"))
            .expect("defines");
        c.transition(&pid(id), PeriodState::Closing).expect("ok");
        c.transition(&pid(id), PeriodState::Sealed).expect("ok");
    }

    #[test]
    fn sealing_advances_the_watermark() {
        let mut c = PeriodCalendar::new();
        assert_eq!(c.sealed_through(), None);

        c.define(march()).expect("defines");
        assert_eq!(c.sealed_through(), None, "defining alone seals nothing");

        c.transition(&pid("2026-03"), PeriodState::Closing)
            .expect("ok");
        assert_eq!(c.sealed_through(), None, "nor does closing");

        c.transition(&pid("2026-03"), PeriodState::Sealed)
            .expect("ok");
        assert_eq!(c.sealed_through(), Some(date!(2026 - 03 - 31)));
    }

    #[test]
    fn the_watermark_seals_the_gaps_a_calendar_never_defined() {
        // The hole this closes: February is not a defined period at all, so it
        // used to report `Open` and accept postings — after March had sealed a
        // closing balance that folds every entry booked on or before 31 March.
        // One legal February booking would then restate a sealed period.
        let mut c = PeriodCalendar::new();
        seal(
            &mut c,
            "2026-03",
            date!(2026 - 03 - 01),
            date!(2026 - 03 - 31),
        );

        assert!(c.period_on(date!(2026 - 02 - 10)).is_none());
        assert_eq!(c.state_on(date!(2026 - 02 - 10)), PeriodState::Sealed);
        assert!(!c.accepts(date!(2026 - 02 - 10)));
        // Including dates long before any period was ever defined.
        assert!(!c.accepts(date!(2019 - 07 - 04)));
        // The last sealed day itself is closed; the day after is not.
        assert!(!c.accepts(date!(2026 - 03 - 31)));
        assert!(c.accepts(date!(2026 - 04 - 01)));
    }

    #[test]
    fn the_watermark_only_moves_forward() {
        let mut c = PeriodCalendar::new();
        seal(
            &mut c,
            "2026-04",
            date!(2026 - 04 - 01),
            date!(2026 - 04 - 30),
        );
        assert_eq!(c.sealed_through(), Some(date!(2026 - 04 - 30)));

        // A period defined afterwards, entirely behind the watermark, does not
        // drag it back — and abandoning a close cannot either, since only a
        // seal advances it and sealing is terminal.
        c.define(
            Period::new(pid("2026-02"), date!(2026 - 02 - 01), date!(2026 - 02 - 28))
                .expect("valid range"),
        )
        .expect("defines");
        assert_eq!(c.sealed_through(), Some(date!(2026 - 04 - 30)));

        c.define(
            Period::new(pid("2026-05"), date!(2026 - 05 - 01), date!(2026 - 05 - 31))
                .expect("valid range"),
        )
        .expect("defines");
        c.transition(&pid("2026-05"), PeriodState::Closing)
            .expect("ok");
        c.transition(&pid("2026-05"), PeriodState::Open)
            .expect("abandons the close");
        assert_eq!(c.sealed_through(), Some(date!(2026 - 04 - 30)));
    }

    #[test]
    fn a_restored_calendar_recovers_its_watermark() {
        // A backend replays its period table on start-up. If the watermark did
        // not come back with it, the first restart would reopen every gap the
        // seals had closed.
        let mut original = PeriodCalendar::new();
        seal(
            &mut original,
            "2026-03",
            date!(2026 - 03 - 01),
            date!(2026 - 03 - 31),
        );

        let restored = PeriodCalendar::from_periods(original.iter().cloned()).expect("rebuilds");
        assert_eq!(restored.sealed_through(), Some(date!(2026 - 03 - 31)));
        assert!(!restored.accepts(date!(2026 - 02 - 10)));

        // `ensure` is the replay path a durable backend actually takes, and it
        // has to carry the state across too.
        let mut replayed = PeriodCalendar::new();
        for period in original.iter() {
            replayed.ensure(period.clone()).expect("replays");
            replayed.ensure(period.clone()).expect("idempotently");
        }
        assert_eq!(replayed.sealed_through(), Some(date!(2026 - 03 - 31)));
    }

    #[test]
    fn only_a_closing_period_is_sealable() {
        let mut c = PeriodCalendar::new();
        c.define(march()).expect("defines");
        assert!(matches!(
            c.check_sealable(&pid("2026-03")),
            Err(PeriodError::NotClosing {
                state: PeriodState::Open,
                ..
            })
        ));
        assert!(matches!(
            c.check_sealable(&pid("nope")),
            Err(PeriodError::Unknown { .. })
        ));

        c.transition(&pid("2026-03"), PeriodState::Closing)
            .expect("ok");
        assert_eq!(
            c.check_sealable(&pid("2026-03")).expect("sealable").end,
            date!(2026 - 03 - 31)
        );
    }

    #[test]
    fn periods_seal_in_date_order() {
        // Sealing March while February is open would let a later, entirely
        // legal February booking restate March's closing balance.
        let mut c = PeriodCalendar::new();
        for (id, start, end) in [
            ("2026-02", date!(2026 - 02 - 01), date!(2026 - 02 - 28)),
            ("2026-03", date!(2026 - 03 - 01), date!(2026 - 03 - 31)),
        ] {
            c.define(Period::new(pid(id), start, end).expect("valid range"))
                .expect("defines");
        }
        c.transition(&pid("2026-03"), PeriodState::Closing)
            .expect("ok");

        assert!(matches!(
            c.check_sealable(&pid("2026-03")),
            Err(PeriodError::UnsealedPredecessor { ref predecessor, .. })
                if *predecessor == pid("2026-02")
        ));

        // Seal February first and March becomes sealable.
        c.transition(&pid("2026-02"), PeriodState::Closing)
            .expect("ok");
        c.transition(&pid("2026-02"), PeriodState::Sealed)
            .expect("ok");
        assert!(c.check_sealable(&pid("2026-03")).is_ok());
    }

    #[test]
    fn a_period_behind_the_watermark_can_never_be_sealed() {
        // The other half of the ordering rule. February was not defined when
        // March sealed, so there was no predecessor to complain about — but its
        // dates are frozen, and a closing balance for it now would be a second
        // commitment to days March already covered.
        let mut c = PeriodCalendar::new();
        seal(
            &mut c,
            "2026-03",
            date!(2026 - 03 - 01),
            date!(2026 - 03 - 31),
        );

        c.define(
            Period::new(pid("2026-02"), date!(2026 - 02 - 01), date!(2026 - 02 - 28))
                .expect("valid range"),
        )
        .expect("defines");
        c.transition(&pid("2026-02"), PeriodState::Closing)
            .expect("ok");

        assert!(matches!(
            c.check_sealable(&pid("2026-02")),
            Err(PeriodError::SealedOutOfOrder {
                ends,
                sealed_through,
                ..
            }) if ends == date!(2026 - 02 - 28) && sealed_through == date!(2026 - 03 - 31)
        ));
    }
}
