//! [`BalanceQuery`]: which postings a read selects, resolved against one log
//! prefix into a [`Selection`] and planned onto the cheapest store read.

use core::fmt;

use time::Date;

use crate::PageRead;
use crate::core::BalanceKey;
use crate::error::QueryError;
use crate::log::TreeHead;
use crate::model::{AccountRef, Currency, Label, Layer, PeriodId, pad};
use crate::store::{AccountRow, KindFilter, PostingCursor, PostingLine, PostingOrder, PostingRead};

/// Which date of an entry a date window, a statement's order and its opening
/// balance read.
///
/// ```
/// use doubleentry::report::DateBasis;
///
/// assert_eq!(DateBasis::default(), DateBasis::Booking);
/// assert_eq!(format!("[{:>12}]", DateBasis::Value), "[  value date]");
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum DateBasis {
    /// The booking date: the books' own date (the default).
    #[default]
    Booking,
    /// The value date: the date the money moved, as a bank statement reads.
    Value,
}

impl fmt::Display for DateBasis {
    /// `booking date` or `value date`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        pad(
            f,
            match self {
                Self::Booking => "booking date",
                Self::Value => "value date",
            },
        )
    }
}

/// The question every balance read answers: which postings count.
///
/// A window — [`all`](Self::all), [`through_position`](Self::through_position),
/// [`through`](Self::through) a date (end of day inclusive),
/// [`between`](Self::between) two dates (both inclusive) or a defined
/// [`period`](Self::period) — refined by modifiers:
/// [`by_value_date`](Self::by_value_date) (booking date by default),
/// [`matching`](Self::matching) and [`missing`](Self::missing) dimension
/// clauses (all must hold), [`of_kind`](Self::of_kind) and
/// [`without_kind`](Self::without_kind) (the entry's kind),
/// [`pending`](Self::pending) (the Settled layer by default) and
/// [`pinned`](Self::pinned) to a tree head.
///
/// The builder is infallible: names are checked when the query runs, and a
/// bad one is [`QueryError::InvalidLabel`]. Every read evaluates one log
/// prefix — the pinned head's, or the head current when the read starts —
/// so appends during a read never mix into it.
///
/// ```
/// use doubleentry::prelude::*;
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
/// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
/// books.define_months(2026, 3..=4)?;
/// books.record(
///     Entry::new("sale-1", date!("2026-03-31"))
///         .value_date(date!("2026-04-02"))
///         .debit("Assets:Bank", amount!("100.00"), EUR)
///         .credit("Income:Sales", amount!("100.00"), EUR),
/// )?;
/// let march = BalanceQuery::through(date!("2026-03-31"));
/// assert_eq!(books.balance("Assets:Bank", EUR, &march)?.net(), amount!("100.00"));
/// let by_value = march.by_value_date(); // the bank has not seen it yet
/// assert_eq!(books.balance("Assets:Bank", EUR, &by_value)?.net(), amount!("0.00"));
/// # Ok::<(), Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceQuery {
    window: Window,
    basis: DateBasis,
    layer: Layer,
    clauses: Vec<Clause>,
    kinds: Vec<String>,
    without_kind: bool,
    pin: Option<TreeHead>,
}

/// A query's window, as given.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum Window {
    All,
    ThroughPosition(u64),
    Through(Date),
    Between(Date, Date),
    Period(String),
}

/// A dimension clause, as given: the axis equals the value, or the axis is
/// absent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Clause {
    axis: String,
    value: Option<String>,
}

impl Default for BalanceQuery {
    /// [`BalanceQuery::all`].
    fn default() -> Self {
        Self::all()
    }
}

impl BalanceQuery {
    fn window(window: Window) -> Self {
        Self {
            window,
            basis: DateBasis::Booking,
            layer: Layer::Settled,
            clauses: Vec::new(),
            kinds: Vec::new(),
            without_kind: false,
            pin: None,
        }
    }

    /// Every posting.
    #[must_use]
    pub fn all() -> Self {
        Self::window(Window::All)
    }

    /// The postings of the records at positions `0..=position`.
    ///
    /// A position the log has not reached is
    /// [`QueryError::PositionBeyondHead`], never "everything".
    #[must_use]
    pub fn through_position(position: u64) -> Self {
        Self::window(Window::ThroughPosition(position))
    }

    /// The postings dated on or before `date` (end of day inclusive).
    #[must_use]
    pub fn through(date: Date) -> Self {
        Self::window(Window::Through(date))
    }

    /// The postings dated from `start` to `end`, both inclusive. A `start`
    /// after `end` is [`QueryError::InvalidWindow`].
    #[must_use]
    pub fn between(start: Date, end: Date) -> Self {
        Self::window(Window::Between(start, end))
    }

    /// The postings dated within the defined period `id`. An id no period
    /// has is [`QueryError::UnknownPeriod`].
    #[must_use]
    pub fn period(id: impl AsRef<str>) -> Self {
        Self::window(Window::Period(id.as_ref().to_owned()))
    }

    /// Dates are value dates instead of booking dates.
    #[must_use]
    pub fn by_value_date(self) -> Self {
        Self {
            basis: DateBasis::Value,
            ..self
        }
    }

    /// Only postings whose dimension `axis` has `value`. Clauses are
    /// conjunctive: two values for one axis match nothing.
    #[must_use]
    pub fn matching(mut self, axis: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.clauses.push(Clause {
            axis: axis.as_ref().to_owned(),
            value: Some(value.as_ref().to_owned()),
        });
        self
    }

    /// Only postings without dimension `axis`.
    #[must_use]
    pub fn missing(mut self, axis: impl AsRef<str>) -> Self {
        self.clauses.push(Clause {
            axis: axis.as_ref().to_owned(),
            value: None,
        });
        self
    }

    /// Only postings of entries of `kind` (`invoice`, `payment`, …: what
    /// [`Entry::kind`](crate::Entry::kind) set). Repeated, it widens: any of
    /// the kinds counts. An entry without a kind never matches.
    ///
    /// ```
    /// use doubleentry::prelude::*;
    ///
    /// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    /// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    /// books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    /// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    /// books.define_months(2026, 1..=12)?;
    /// let book = |key: &str, kind: &str, debit: &str, credit: &str, eur| {
    ///     Entry::new(key, date!("2026-03-02"))
    ///         .kind(kind)
    ///         .debit(debit, eur, EUR)
    ///         .credit(credit, eur, EUR)
    /// };
    /// books.record(book("inv-1", "invoice", "Assets:Receivables", "Income:Sales", amount!("100.00")))?;
    /// books.record(book("pay-1", "payment", "Assets:Bank", "Assets:Receivables", amount!("60.00")))?;
    ///
    /// let year = BalanceQuery::between(date!("2026-01-01"), date!("2026-12-31"));
    /// let invoiced = books.balance("Assets:Receivables", EUR, &year.clone().of_kind("invoice"))?;
    /// assert_eq!(invoiced.debit(), amount!("100.00"));
    /// let paid = books.balance("Assets:Receivables", EUR, &year.of_kind("payment"))?;
    /// assert_eq!(paid.credit(), amount!("60.00"));
    /// # Ok::<(), Error>(())
    /// ```
    #[must_use]
    pub fn of_kind(mut self, kind: impl AsRef<str>) -> Self {
        self.kinds.push(kind.as_ref().to_owned());
        self
    }

    /// Only postings of entries recorded without a kind. With
    /// [`of_kind`](Self::of_kind) it matches nothing.
    #[must_use]
    pub fn without_kind(self) -> Self {
        Self {
            without_kind: true,
            ..self
        }
    }

    /// The Pending layer (holds) instead of the Settled one.
    #[must_use]
    pub fn pending(self) -> Self {
        Self {
            layer: Layer::Pending,
            ..self
        }
    }

    /// Evaluated against the log prefix of `head` — a signed checkpoint, for
    /// example. The store's root at `head.size` must be `head.root`
    /// ([`Error::Integrity`](crate::Error::Integrity) otherwise), and a head
    /// larger than the log is [`QueryError::PositionBeyondHead`].
    #[must_use]
    pub fn pinned(self, head: TreeHead) -> Self {
        Self {
            pin: Some(head),
            ..self
        }
    }

    /// The layer the query reads.
    #[must_use]
    pub const fn layer(&self) -> Layer {
        self.layer
    }

    /// The date basis of its windows.
    #[must_use]
    pub const fn basis(&self) -> DateBasis {
        self.basis
    }

    /// Validates the query against a log of `size` records: its window, its
    /// names, its pin and its position bound. What remains — the pin's root
    /// and the period's dates — is read by the façade before
    /// [`Prepared::select`].
    pub(crate) fn prepare(&self, size: u64) -> Result<Prepared, QueryError> {
        if let Window::Between(start, end) = self.window
            && start > end
        {
            return Err(QueryError::InvalidWindow { start, end });
        }
        let clauses = self
            .clauses
            .iter()
            .map(|clause| {
                Ok((
                    label(&clause.axis)?,
                    clause.value.as_deref().map(label).transpose()?,
                ))
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        let kinds = match (self.kinds.is_empty(), self.without_kind) {
            (true, false) => KindFilter::Any,
            (true, true) => KindFilter::Without,
            (false, without) => {
                let mut kinds = self
                    .kinds
                    .iter()
                    .map(|kind| label(kind))
                    .collect::<Result<Vec<_>, QueryError>>()?;
                kinds.sort_unstable();
                kinds.dedup();
                if without {
                    // A kind and no kind at once: nothing.
                    kinds.clear();
                }
                KindFilter::OneOf(kinds)
            }
        };
        let period = match &self.window {
            Window::Period(id) => Some(label(id)?),
            _ => None,
        };
        let limit = match self.pin {
            Some(pin) if pin.size > size => {
                return Err(QueryError::PositionBeyondHead {
                    position: pin.size,
                    size,
                });
            }
            Some(pin) => pin.size,
            None => size,
        };
        let bound = match self.window {
            Window::ThroughPosition(position) if position >= limit => {
                return Err(QueryError::PositionBeyondHead {
                    position,
                    size: limit,
                });
            }
            Window::ThroughPosition(position) => position.saturating_add(1),
            _ => limit,
        };
        let unfiltered = clauses.is_empty() && kinds == KindFilter::Any;
        let plan = match (&self.window, unfiltered, self.pin.is_some()) {
            (Window::All, true, false) => Plan::Maintained,
            (Window::All | Window::ThroughPosition(_), true, _) => Plan::AtPosition(bound),
            _ => Plan::Fold,
        };
        let (start, end) = match self.window {
            Window::Through(end) => (None, Some(end)),
            Window::Between(start, end) => (Some(start), Some(end)),
            Window::All | Window::ThroughPosition(_) | Window::Period(_) => (None, None),
        };
        Ok(Prepared {
            period,
            pin: self.pin,
            selection: Selection {
                bound,
                start,
                end,
                basis: self.basis,
                layer: self.layer,
                clauses,
                kinds,
                plan,
            },
        })
    }
}

/// A name of the query as a label.
fn label(text: &str) -> Result<Label, QueryError> {
    Label::new(text).map_err(|_| QueryError::InvalidLabel {
        text: text.to_owned(),
    })
}

/// A query checked against the log size: what the façade still has to read
/// before it is a [`Selection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    period: Option<PeriodId>,
    pin: Option<TreeHead>,
    selection: Selection,
}

impl Prepared {
    /// The head whose root the store must have at its size.
    pub(crate) const fn pin(&self) -> Option<TreeHead> {
        self.pin
    }

    /// The period whose dates the window is, to be looked up below
    /// [`bound`](Self::bound).
    pub(crate) const fn period(&self) -> Option<&PeriodId> {
        self.period.as_ref()
    }

    /// The records the read sees: those below this position.
    pub(crate) const fn bound(&self) -> u64 {
        self.selection.bound
    }

    /// The selection, given the period's (start, end) as found below the
    /// bound.
    pub(crate) fn select(self, period: Option<(Date, Date)>) -> Result<Selection, QueryError> {
        let mut selection = self.selection;
        if let Some(id) = self.period {
            let (start, end) = period.ok_or_else(|| QueryError::UnknownPeriod {
                id: id.as_str().to_owned(),
            })?;
            selection.start = Some(start);
            selection.end = Some(end);
        }
        Ok(selection)
    }
}

/// Which store read answers a balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// The maintained balance, now: an unbounded, unfiltered query.
    Maintained,
    /// The balance after every record below this position (`balance_after`
    /// of the key's last posting there): a position bound only.
    AtPosition(u64),
    /// A fold of the key's posting rows: dates or dimensions.
    Fold,
}

/// A query resolved against one log prefix: the predicate every fold
/// applies to every posting row, whatever a store filtered already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    bound: u64,
    start: Option<Date>,
    end: Option<Date>,
    basis: DateBasis,
    layer: Layer,
    clauses: Vec<(Label, Option<Label>)>,
    kinds: KindFilter,
    plan: Plan,
}

impl Selection {
    /// The records the read sees: those below this position.
    pub(crate) const fn bound(&self) -> u64 {
        self.bound
    }

    /// The store read that answers a balance.
    pub(crate) const fn plan(&self) -> Plan {
        self.plan
    }

    /// The layer.
    pub(crate) const fn layer(&self) -> Layer {
        self.layer
    }

    /// The date basis.
    pub(crate) const fn basis(&self) -> DateBasis {
        self.basis
    }

    /// The window's first day, if it has one.
    pub(crate) const fn start(&self) -> Option<Date> {
        self.start
    }

    /// The window's last day, if it has one.
    pub(crate) const fn end(&self) -> Option<Date> {
        self.end
    }

    /// Whether a dimension clause or a kind narrows the selection (a
    /// filtered trial balance need not balance).
    pub(crate) fn is_filtered(&self) -> bool {
        !self.clauses.is_empty() || self.kinds != KindFilter::Any
    }

    /// The date of `line` the window reads.
    pub(crate) const fn date_of<const P: u8>(&self, line: &PostingLine<P>) -> Date {
        match self.basis {
            DateBasis::Booking => line.booking,
            DateBasis::Value => line.value,
        }
    }

    /// Whether `line` is in the prefix, satisfies every dimension clause
    /// and is of a selected kind, whatever its date.
    pub(crate) fn filters<const P: u8>(&self, line: &PostingLine<P>) -> bool {
        line.position < self.bound
            && self
                .clauses
                .iter()
                .all(|(axis, value)| line.dimensions.get(axis) == value.as_ref())
            && self.kinds.keeps(line.kind.as_ref())
    }

    /// Whether the window holds `date`.
    pub(crate) fn holds(&self, date: Date) -> bool {
        self.start.is_none_or(|start| start <= date) && self.end.is_none_or(|end| date <= end)
    }

    /// Whether the read counts `line`: in the prefix, every clause, and its
    /// basis date in the window.
    pub(crate) fn selects<const P: u8>(&self, line: &PostingLine<P>) -> bool {
        self.filters(line) && self.holds(self.date_of(line))
    }

    /// One page of the key's postings in log order below the bound, already
    /// narrowed by what a store filters cheaply (the window's end and, up to
    /// [`PostingRead::MAX_DIMENSIONS`], the `matching` clauses); the fold
    /// applies [`selects`](Self::selects) again.
    pub(crate) fn posting_read(
        &self,
        key: BalanceKey,
        after: Option<PostingCursor>,
    ) -> PostingRead {
        let upto = self.end.map(|end| Date::MIN..=end);
        let (booking, value) = match self.basis {
            DateBasis::Booking => (upto, None),
            DateBasis::Value => (None, upto),
        };
        let matching: Vec<(Label, Label)> = self
            .clauses
            .iter()
            .filter_map(|(axis, value)| Some((axis.clone(), value.clone()?)))
            .collect();
        let dimensions = if matching.len() <= PostingRead::MAX_DIMENSIONS {
            matching
        } else {
            Vec::new()
        };
        let mut page = PageRead::first(PageRead::<()>::MAX_LIMIT).through(self.bound);
        page.after = after;
        let kinds = match &self.kinds {
            KindFilter::OneOf(kinds) if kinds.len() > KindFilter::MAX_KINDS => KindFilter::Any,
            kinds => kinds.clone(),
        };
        PostingRead {
            key,
            order: PostingOrder::Position,
            booking,
            value,
            dimensions,
            kinds,
            page,
        }
    }
}

/// An account as a read names it: the reference, when the name is one, and
/// the name as given (for [`QueryError::UnknownAccount`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountName {
    reference: Option<AccountRef>,
    text: String,
}

impl AccountName {
    /// The name of `account`.
    pub(crate) fn new<A>(account: A) -> Self
    where
        A: TryInto<AccountRef> + fmt::Display,
    {
        let text = account.to_string();
        Self {
            reference: account.try_into().ok(),
            text,
        }
    }

    /// The reference to look up; `None` for text that names no account.
    pub(crate) const fn reference(&self) -> Option<&AccountRef> {
        self.reference.as_ref()
    }

    /// The name as given.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The balance of `currency` in `layer` of the account `found` for this
    /// name, when it was registered below `bound`.
    pub(crate) fn key(
        &self,
        found: Option<AccountRow>,
        bound: u64,
        currency: Currency,
        layer: Layer,
    ) -> Result<(BalanceKey, AccountRow), QueryError> {
        let row =
            found
                .filter(|row| row.position < bound)
                .ok_or_else(|| QueryError::UnknownAccount {
                    account: self.text.clone(),
                })?;
        Ok((
            BalanceKey {
                handle: row.handle,
                currency,
                layer,
            },
            row,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Hash;
    use crate::model::{Amount, Balance, Dimensions, EntryId, Side};
    use crate::{currency::EUR, date};

    fn line(
        position: u64,
        booking: Date,
        value: Date,
        dimensions: &[(&str, &str)],
    ) -> PostingLine<2> {
        let mut set = Dimensions::new();
        for (axis, value) in dimensions {
            set = set
                .with(Label::new(axis).unwrap(), Label::new(value).unwrap())
                .unwrap();
        }
        PostingLine {
            position,
            index: 0,
            entry: EntryId::from_bytes([1; 16]),
            side: Side::Debit,
            amount: Amount::from_minor(100).unwrap(),
            booking,
            value,
            dimensions: set,
            balance_after: Balance::ZERO,
            cleared: Amount::ZERO,
            kind: None,
        }
    }

    fn select(query: &BalanceQuery, size: u64) -> Selection {
        query.prepare(size).unwrap().select(None).unwrap()
    }

    #[test]
    fn date_windows_read_the_basis_date_end_of_day_inclusive() {
        let march = line(5, date!("2026-03-31"), date!("2026-04-02"), &[]);
        let through = BalanceQuery::through(date!("2026-03-31"));
        assert!(select(&through, 10).selects(&march));
        assert!(!select(&through.clone().by_value_date(), 10).selects(&march));
        let between = BalanceQuery::between(date!("2026-04-01"), date!("2026-04-02"));
        assert!(!select(&between, 10).selects(&march));
        assert!(select(&between.by_value_date(), 10).selects(&march));
    }

    #[test]
    fn positions_bound_the_prefix_and_never_reach_past_the_head() {
        let early = line(3, date!("2026-03-01"), date!("2026-03-01"), &[]);
        let selection = select(&BalanceQuery::through_position(3), 10);
        assert_eq!(
            (selection.bound(), selection.plan()),
            (4, Plan::AtPosition(4))
        );
        assert!(selection.selects(&early));
        assert!(!select(&BalanceQuery::through_position(2), 10).selects(&early));
        assert_eq!(
            BalanceQuery::through_position(10).prepare(10),
            Err(QueryError::PositionBeyondHead {
                position: 10,
                size: 10
            })
        );
        let head = TreeHead {
            size: 12,
            root: Hash::from_bytes([0; 32]),
        };
        assert_eq!(
            BalanceQuery::all().pinned(head).prepare(10),
            Err(QueryError::PositionBeyondHead {
                position: 12,
                size: 10
            })
        );
        let head = TreeHead {
            size: 6,
            root: Hash::from_bytes([0; 32]),
        };
        assert_eq!(
            BalanceQuery::through_position(6).pinned(head).prepare(10),
            Err(QueryError::PositionBeyondHead {
                position: 6,
                size: 6
            })
        );
        assert_eq!(select(&BalanceQuery::all().pinned(head), 10).bound(), 6);
        assert_eq!(
            select(&BalanceQuery::through_position(4).pinned(head), 10).bound(),
            5
        );
    }

    #[test]
    fn clauses_are_conjunctive_and_contradictions_match_nothing() {
        let a = line(
            1,
            date!("2026-03-01"),
            date!("2026-03-01"),
            &[("cost_centre", "A")],
        );
        let none = line(2, date!("2026-03-01"), date!("2026-03-01"), &[]);
        let only_a = select(&BalanceQuery::all().matching("cost_centre", "A"), 10);
        assert!(only_a.selects(&a) && !only_a.selects(&none));
        let missing = select(&BalanceQuery::all().missing("cost_centre"), 10);
        assert!(!missing.selects(&a) && missing.selects(&none));
        for contradiction in [
            BalanceQuery::all()
                .matching("cost_centre", "A")
                .matching("cost_centre", "B"),
            BalanceQuery::all()
                .matching("cost_centre", "A")
                .missing("cost_centre"),
        ] {
            let selection = select(&contradiction, 10);
            assert!(!selection.selects(&a) && !selection.selects(&none));
            assert!(selection.is_filtered());
        }
    }

    #[test]
    fn invalid_questions_are_refused_when_the_query_runs() {
        assert_eq!(
            BalanceQuery::between(date!("2026-03-31"), date!("2026-03-01")).prepare(5),
            Err(QueryError::InvalidWindow {
                start: date!("2026-03-31"),
                end: date!("2026-03-01")
            })
        );
        assert_eq!(
            BalanceQuery::all().matching("cost centre ", "A").prepare(5),
            Err(QueryError::InvalidLabel {
                text: String::from("cost centre ")
            })
        );
        assert_eq!(
            BalanceQuery::period("").prepare(5),
            Err(QueryError::InvalidLabel {
                text: String::new()
            })
        );
        let prepared = BalanceQuery::period("2027-13").prepare(5).unwrap();
        assert_eq!(prepared.period().map(Label::as_str), Some("2027-13"));
        assert_eq!(
            prepared.select(None),
            Err(QueryError::UnknownPeriod {
                id: String::from("2027-13")
            })
        );
    }

    #[test]
    fn the_plan_is_the_cheapest_read_that_answers() {
        let head = TreeHead {
            size: 4,
            root: Hash::from_bytes([0; 32]),
        };
        let plans = [
            (BalanceQuery::all(), Plan::Maintained),
            (BalanceQuery::all().pending(), Plan::Maintained),
            (BalanceQuery::all().pinned(head), Plan::AtPosition(4)),
            (BalanceQuery::through_position(1), Plan::AtPosition(2)),
            (BalanceQuery::through(date!("2026-03-31")), Plan::Fold),
            (BalanceQuery::all().missing("region"), Plan::Fold),
            (
                BalanceQuery::through_position(1).matching("region", "eu"),
                Plan::Fold,
            ),
        ];
        for (query, plan) in plans {
            assert_eq!(select(&query, 10).plan(), plan, "{query:?}");
        }
    }

    #[test]
    fn a_store_is_asked_for_less_but_the_fold_decides() {
        let query = BalanceQuery::through(date!("2026-03-31"))
            .by_value_date()
            .matching("region", "eu")
            .missing("project");
        let read = select(&query, 10).posting_read(
            BalanceKey {
                handle: crate::model::AccountHandle::new(1).unwrap(),
                currency: EUR,
                layer: Layer::Settled,
            },
            None,
        );
        assert_eq!(read.booking, None);
        assert_eq!(read.value, Some(Date::MIN..=date!("2026-03-31")));
        assert_eq!(read.dimensions.len(), 1);
        assert_eq!(read.page.through, Some(10));
    }
}
