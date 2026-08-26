//! Balances and trial balances.
//!
//! A balance carries the gross debit total and the gross credit total, not just
//! the net. A trial balance that reports only the net cannot answer how much
//! moved through an account, and the gross totals cannot be reconstructed from
//! the net afterwards.

use std::collections::BTreeMap;

use time::Date;

use crate::account::AccountId;
use crate::dimensions::DimensionFilter;
use crate::money::{Amount, Currency, MoneyError};
use crate::posting::{Direction, Layer, Posting};

/// Which of an entry's two dates a date-bounded query folds by.
///
/// An entry carries both, and they answer different questions. Neither is a
/// default that is right everywhere, so a query says which one it means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DateBasis {
    /// The date the entry was **booked into the books**.
    ///
    /// The default, and the only basis the engine itself uses. A period covers
    /// booking dates, a seal's closing balance is cumulative through a booking
    /// date, and the sealed watermark freezes booking dates — so this is what a
    /// closing balance, a trial balance and a set of financial statements mean.
    #[default]
    Booking,
    /// The date the money is treated as having **moved**.
    ///
    /// What an outside party speaks. A bank statement, an interest accrual and a
    /// cash-position report are all value-dated: an invoice booked on the 28th
    /// with value the 2nd of the next month is in this month's books and next
    /// month's cash.
    ///
    /// Reporting on this basis is a read: it changes nothing about which period
    /// an entry belongs to, what a seal committed to, or what may be booked
    /// next. Otherwise a value date could reopen a sealed period.
    Value,
}

impl std::fmt::Display for DateBasis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Booking => "booking date",
            Self::Value => "value date",
        })
    }
}

/// The two dates an entry carries.
///
/// Passed together to [`BalanceQuery::includes_entry`] so the query picks the one
/// its [`DateBasis`] names. Two bare `Date` arguments in a row can be swapped
/// without the compiler noticing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryDates {
    /// When the entry was booked into the books.
    pub booking: Date,
    /// When the money is treated as having moved.
    pub value: Date,
}

impl EntryDates {
    /// Both dates the same, which is the default an entry is built with.
    #[must_use]
    pub const fn on(date: Date) -> Self {
        Self {
            booking: date,
            value: date,
        }
    }

    /// The date `basis` names.
    #[must_use]
    pub const fn by(&self, basis: DateBasis) -> Date {
        match basis {
            DateBasis::Booking => self.booking,
            DateBasis::Value => self.value,
        }
    }
}

/// Which postings a balance report folds.
///
/// Every field narrows; [`BalanceQuery::all`] folds everything.
///
/// # Two orderings, and they are not interchangeable
///
/// [`over_prefix`](Self::over_prefix) counts **entries in log order** — the same
/// number a [`TreeHead::size`](crate::TreeHead::size) carries, so it is the form
/// to pair with a proof.
///
/// [`through`](Self::through) and [`between`](Self::between) fold by date, so a
/// backdated entry lands where it economically belongs. That is the form to
/// reconcile against anything outside the ledger, and what a period's closing
/// balance means.
///
/// The two differ whenever a period is sealed after the next has begun, which is
/// the normal case. Confusing them is the classic reconciliation mistake.
///
/// # And two dates
///
/// A date-bounded query folds by **booking date** unless
/// [`by_value_date`](Self::by_value_date) says otherwise — see [`DateBasis`].
/// The basis is a reporting choice and nothing more: it never moves an entry
/// between periods and never affects what a seal committed to.
///
/// ```
/// # use doubleentry::{BalanceQuery, DimensionFilter, Label};
/// # use time::macros::date;
/// // Everything.
/// let all = BalanceQuery::all();
///
/// // A period's activity, not the cumulative position — what a P&L needs.
/// let march = BalanceQuery::between(date!(2026 - 03 - 01), date!(2026 - 03 - 31));
///
/// // The same, sliced to one reporting axis.
/// let filter = DimensionFilter::any().matching(Label::new("segment")?, Label::new("Retail")?);
/// let retail_in_march = march.matching(&filter);
///
/// // The cash that actually moved in March, which is not the same set.
/// let cash_in_march = march.by_value_date();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BalanceQuery<'a> {
    prefix: Option<u64>,
    from: Option<Date>,
    to: Option<Date>,
    basis: DateBasis,
    dimensions: Option<&'a DimensionFilter>,
}

impl<'a> BalanceQuery<'a> {
    /// Every posting recorded.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// The first `size` entries in log order.
    ///
    /// A count, not an index: `0` is the empty ledger and `2` is the journal as
    /// it stood after two entries.
    #[must_use]
    pub fn over_prefix(size: u64) -> Self {
        Self {
            prefix: Some(size),
            ..Self::default()
        }
    }

    /// Everything booked on or before `end`.
    ///
    /// The cumulative position — what a period's closing balance means.
    #[must_use]
    pub fn through(end: Date) -> Self {
        Self {
            to: Some(end),
            ..Self::default()
        }
    }

    /// Everything booked between `start` and `end`, both inclusive.
    ///
    /// The period's **activity** rather than the position it leaves behind,
    /// which is the difference between an income statement and a balance sheet.
    #[must_use]
    pub fn between(start: Date, end: Date) -> Self {
        Self {
            from: Some(start),
            to: Some(end),
            ..Self::default()
        }
    }

    /// Narrows to entries booked on or after `start`.
    #[must_use]
    pub fn from(mut self, start: Date) -> Self {
        self.from = Some(start);
        self
    }

    /// Narrows to entries booked on or before `end`.
    #[must_use]
    pub fn to(mut self, end: Date) -> Self {
        self.to = Some(end);
        self
    }

    /// Narrows to the first `size` entries in log order.
    #[must_use]
    pub fn within_prefix(mut self, size: u64) -> Self {
        self.prefix = Some(size);
        self
    }

    /// Narrows to postings whose dimensions satisfy `filter`.
    #[must_use]
    pub fn matching(mut self, filter: &'a DimensionFilter) -> Self {
        self.dimensions = Some(filter);
        self
    }

    /// Folds the date bounds by **value date** rather than booking date.
    ///
    /// A reporting choice — see [`DateBasis::Value`]. It has no effect on a
    /// query with no date bounds.
    #[must_use]
    pub fn by_value_date(mut self) -> Self {
        self.basis = DateBasis::Value;
        self
    }

    /// Folds the date bounds by **booking date**, which is the default.
    #[must_use]
    pub fn by_booking_date(mut self) -> Self {
        self.basis = DateBasis::Booking;
        self
    }

    /// Which date the bounds are applied to.
    #[must_use]
    pub fn basis(&self) -> DateBasis {
        self.basis
    }

    /// The log-prefix bound, if one was set.
    #[must_use]
    pub fn prefix(&self) -> Option<u64> {
        self.prefix
    }

    /// The earliest booking date included, if one was set.
    #[must_use]
    pub fn start(&self) -> Option<Date> {
        self.from
    }

    /// The latest booking date included, if one was set.
    #[must_use]
    pub fn end(&self) -> Option<Date> {
        self.to
    }

    /// The dimension filter, if one was set and it constrains anything.
    #[must_use]
    pub fn dimensions(&self) -> Option<&'a DimensionFilter> {
        self.dimensions.filter(|f| !f.is_empty())
    }

    /// True when this query narrows nothing, so the answer is the whole ledger.
    ///
    /// The fast path a backend takes to serve a maintained total rather than
    /// re-folding: [`Journal`](crate::Journal) keeps current balances as entries
    /// arrive, and this is what says the maintained one is the answer.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.prefix.is_none()
            && self.from.is_none()
            && self.to.is_none()
            && self.dimensions().is_none()
    }

    /// The query an **opening** balance is folded over: everything this query
    /// selects that was booked strictly *before* its window opens.
    ///
    /// `None` when the query has no start date, because then there is nothing to
    /// carry in and the opening balance is zero. A statement scoped to March
    /// carries February in on its first line; an unscoped statement opens at
    /// nothing.
    ///
    /// Every *other* narrowing still applies — the log prefix and the dimension
    /// filter — because an opening balance for one reporting axis is over that
    /// axis. The upper date bound is dropped along with the lower one: it is
    /// implied, since every entry in the carry set is already below the window.
    ///
    /// The set is defined by **date**, never by log position. Entries are
    /// appended in recording order, so a position bound would fold a later-dated
    /// entry into the middle of the statement and leave a backdated one out of
    /// the opening.
    #[must_use]
    pub fn opening(&self) -> Option<Self> {
        // `previous_day` is `None` only at `Date::MIN`, where nothing can
        // precede the window and the opening balance is zero either way.
        let before = self.from?.previous_day()?;
        Some(Self {
            prefix: self.prefix,
            from: None,
            to: Some(before),
            basis: self.basis,
            dimensions: self.dimensions,
        })
    }

    /// True when the entry at `index` carrying `dates` is included.
    ///
    /// The date bounds are applied to whichever of the two [`basis`](Self::basis)
    /// names. The entry-level half of the predicate; the posting-level half is
    /// the dimension filter, which needs the posting.
    #[must_use]
    pub fn includes_entry(&self, index: u64, dates: EntryDates) -> bool {
        let on = dates.by(self.basis);
        self.prefix.is_none_or(|size| index < size)
            && self.from.is_none_or(|start| on >= start)
            && self.to.is_none_or(|end| on <= end)
    }

    /// True when `posting` satisfies the dimension filter, if there is one.
    #[must_use]
    pub fn includes_posting<const P: u8>(&self, posting: &Posting<P>) -> bool {
        self.dimensions()
            .is_none_or(|filter| filter.matches(&posting.dimensions))
    }
}

/// Gross debit and credit totals, and the net derived from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Balance<const P: u8> {
    /// Gross total of debit movements.
    pub debits: Amount<P>,
    /// Gross total of credit movements.
    pub credits: Amount<P>,
}

impl<const P: u8> Balance<P> {
    /// An empty balance.
    pub const ZERO: Self = Self {
        debits: Amount::ZERO,
        credits: Amount::ZERO,
    };

    /// Adds a movement on the given side.
    pub fn add(&mut self, direction: Direction, amount: Amount<P>) -> Result<(), MoneyError> {
        match direction {
            Direction::Debit => self.debits = self.debits.checked_add(amount)?,
            Direction::Credit => self.credits = self.credits.checked_add(amount)?,
        }
        Ok(())
    }

    /// The net balance and the side it falls on.
    ///
    /// A net of zero is reported as a debit of zero by convention; callers that
    /// care should test the magnitude rather than the side.
    pub fn net(&self) -> Result<(Direction, Amount<P>), MoneyError> {
        if self.debits >= self.credits {
            Ok((Direction::Debit, self.debits.checked_sub(self.credits)?))
        } else {
            Ok((Direction::Credit, self.credits.checked_sub(self.debits)?))
        }
    }

    /// The net as a signed amount, debit positive.
    pub fn signed_net(&self) -> Result<Amount<P>, MoneyError> {
        self.debits.checked_sub(self.credits)
    }

    /// True when debits and credits are equal.
    #[must_use]
    pub fn is_balanced(&self) -> bool {
        self.debits == self.credits
    }

    /// True when nothing has moved at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.debits.is_zero() && self.credits.is_zero()
    }

    /// Combines two balances.
    pub fn checked_add(&self, other: &Self) -> Result<Self, MoneyError> {
        Ok(Self {
            debits: self.debits.checked_add(other.debits)?,
            credits: self.credits.checked_add(other.credits)?,
        })
    }
}

/// The key a balance is reported against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceKey {
    /// The account.
    pub account: AccountId,
    /// The currency.
    pub currency: Currency,
    /// Settled or pending.
    pub layer: Layer,
}

/// Balances for a set of accounts, currencies, and layers.
///
/// Iteration order is deterministic, so any report or hash derived from a trial
/// balance is reproducible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrialBalance<const P: u8> {
    entries: BTreeMap<BalanceKey, Balance<P>>,
}

impl<const P: u8> TrialBalance<P> {
    /// Creates an empty trial balance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Accumulates a posting.
    pub fn apply(&mut self, posting: &Posting<P>) -> Result<(), MoneyError> {
        let key = BalanceKey {
            account: posting.account,
            currency: posting.currency,
            layer: posting.layer,
        };
        self.entries
            .entry(key)
            .or_default()
            .add(posting.direction, posting.amount)
    }

    /// Sets the balance for one key, replacing any existing value.
    ///
    /// Accumulating postings via [`TrialBalance::apply`] is the usual path; this
    /// is for reconstructing a balance set that was computed elsewhere.
    pub fn set(&mut self, key: BalanceKey, balance: Balance<P>) {
        self.entries.insert(key, balance);
    }

    /// Removes one key, returning the balance it held.
    ///
    /// For rebuilding a balance set, not for editing one: a trial balance with a
    /// key removed says the account never moved, which is a different claim from
    /// a balance of zero.
    pub fn remove(&mut self, key: &BalanceKey) -> Option<Balance<P>> {
        self.entries.remove(key)
    }

    /// The balance for one key, if anything has been posted to it.
    #[must_use]
    pub fn get(&self, key: &BalanceKey) -> Option<&Balance<P>> {
        self.entries.get(key)
    }

    /// The balance for one key, defaulting to zero.
    #[must_use]
    pub fn get_or_zero(&self, key: &BalanceKey) -> Balance<P> {
        self.entries.get(key).copied().unwrap_or(Balance::ZERO)
    }

    /// Every key and balance, in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&BalanceKey, &Balance<P>)> {
        self.entries.iter()
    }

    /// Number of populated keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing has been accumulated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Totals across all accounts for one currency and layer.
    ///
    /// In a consistent ledger the debit and credit totals are equal; that
    /// equality is the classic trial-balance check and is exposed here so a
    /// caller can assert it.
    pub fn totals(&self, currency: Currency, layer: Layer) -> Result<Balance<P>, MoneyError> {
        let mut acc = Balance::ZERO;
        for (key, balance) in &self.entries {
            if key.currency == currency && key.layer == layer {
                acc = acc.checked_add(balance)?;
            }
        }
        Ok(acc)
    }

    /// Every currency present, in deterministic order.
    #[must_use]
    pub fn currencies(&self) -> Vec<Currency> {
        let mut out: Vec<Currency> = self.entries.keys().map(|k| k.currency).collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Eur = Amount<2>;

    fn acct(i: u32) -> AccountId {
        AccountId::from_index(i)
    }

    #[test]
    fn tracks_gross_totals_separately_from_the_net() {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::from_minor(1000))
            .expect("no overflow");
        b.add(Direction::Credit, Eur::from_minor(1000))
            .expect("no overflow");

        // The net is zero, but a thousand moved in each direction.
        assert_eq!(b.signed_net().expect("no overflow"), Eur::ZERO);
        assert_eq!(b.debits, Eur::from_minor(1000));
        assert_eq!(b.credits, Eur::from_minor(1000));
        assert!(b.is_balanced());
        assert!(!b.is_empty());
    }

    #[test]
    fn distinguishes_no_activity_from_offsetting_activity() {
        let quiet = Balance::<2>::ZERO;
        let mut busy = Balance::<2>::ZERO;
        busy.add(Direction::Debit, Eur::from_minor(500))
            .expect("no overflow");
        busy.add(Direction::Credit, Eur::from_minor(500))
            .expect("no overflow");

        assert_eq!(
            quiet.signed_net().expect("ok"),
            busy.signed_net().expect("ok")
        );
        assert_ne!(quiet, busy);
        assert!(quiet.is_empty());
        assert!(!busy.is_empty());
    }

    #[test]
    fn net_reports_the_dominant_side() {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::from_minor(300)).expect("ok");
        b.add(Direction::Credit, Eur::from_minor(100)).expect("ok");
        assert_eq!(
            b.net().expect("ok"),
            (Direction::Debit, Eur::from_minor(200))
        );

        let mut c = Balance::<2>::ZERO;
        c.add(Direction::Credit, Eur::from_minor(300)).expect("ok");
        c.add(Direction::Debit, Eur::from_minor(100)).expect("ok");
        assert_eq!(
            c.net().expect("ok"),
            (Direction::Credit, Eur::from_minor(200))
        );
    }

    #[test]
    fn balance_addition_reports_overflow() {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::MAX).expect("ok");
        assert_eq!(
            b.add(Direction::Debit, Eur::from_minor(1)),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn trial_balance_separates_account_currency_and_layer() {
        let mut tb = TrialBalance::<2>::new();
        tb.apply(&Posting::debit(
            acct(0),
            Eur::from_minor(100),
            Currency::EUR,
        ))
        .expect("ok");
        tb.apply(&Posting::debit(
            acct(0),
            Eur::from_minor(100),
            Currency::USD,
        ))
        .expect("ok");
        tb.apply(
            &Posting::debit(acct(0), Eur::from_minor(100), Currency::EUR).in_layer(Layer::Pending),
        )
        .expect("ok");
        assert_eq!(tb.len(), 3);
        assert_eq!(tb.currencies(), vec![Currency::EUR, Currency::USD]);
    }

    #[test]
    fn trial_balance_totals_match_for_a_balanced_set() {
        let mut tb = TrialBalance::<2>::new();
        tb.apply(&Posting::debit(
            acct(0),
            Eur::from_minor(250),
            Currency::EUR,
        ))
        .expect("ok");
        tb.apply(&Posting::credit(
            acct(1),
            Eur::from_minor(250),
            Currency::EUR,
        ))
        .expect("ok");
        let totals = tb.totals(Currency::EUR, Layer::Settled).expect("ok");
        assert!(totals.is_balanced());
        assert_eq!(totals.debits, Eur::from_minor(250));
    }

    #[test]
    fn missing_keys_read_as_zero() {
        let tb = TrialBalance::<2>::new();
        let key = BalanceKey {
            account: acct(7),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        assert_eq!(tb.get(&key), None);
        assert_eq!(tb.get_or_zero(&key), Balance::ZERO);
    }

    #[test]
    fn iteration_order_is_deterministic() {
        let build = || {
            let mut tb = TrialBalance::<2>::new();
            for i in [3u32, 1, 2, 0] {
                tb.apply(&Posting::debit(acct(i), Eur::from_minor(1), Currency::EUR))
                    .expect("ok");
            }
            tb.iter().map(|(k, _)| k.account).collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
        assert_eq!(
            build(),
            vec![acct(0), acct(1), acct(2), acct(3)],
            "keys must iterate in account order"
        );
    }
}
