//! Balance assertions: claims from outside the ledger — a bank's closing
//! balance, an auditor's figure — checked against it. Every assertion is
//! evaluated; one that fails reports what was expected, what the ledger
//! holds and the difference, which never overflows.

use core::fmt;

use time::Date;

use super::query::{AccountName, BalanceQuery, DateBasis};
use crate::model::{AccountRef, Amount, Balance, Currency, Layer, ValueError, pad};

/// The point of the log an assertion is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum AssertAt {
    /// At the end of `date`: every posting dated on or before it, on
    /// `basis` (a bank's daily closing balance is by value date).
    EndOfDay {
        /// The day.
        date: Date,
        /// The date the day is compared with.
        basis: DateBasis,
    },
    /// After the record at this position.
    Position(u64),
}

impl fmt::Display for AssertAt {
    /// `at the end of 2026-03-31 by value date` or `after position 250`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndOfDay { date, basis } => write!(f, "at the end of {date} by {basis}"),
            Self::Position(position) => write!(f, "after position {position}"),
        }
    }
}

/// A claim about one account's balance: its net (debit positive) in one
/// currency and layer at one point.
///
/// ```
/// use doubleentry::prelude::*;
/// use doubleentry::{report::{AssertAt, AssertionOutcome, BalanceAssertion, DateBasis, Deviation}};
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
/// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
/// books.define_months(2026, 3..=3)?;
/// books.record(
///     Entry::new("sale-1", date!("2026-03-31"))
///         .debit("Assets:Bank", amount!("990.00"), EUR)
///         .credit("Income:Sales", amount!("990.00"), EUR),
/// )?;
/// let end_of_march = AssertAt::EndOfDay { date: date!("2026-03-31"), basis: DateBasis::Value };
/// let claim = BalanceAssertion::new("Assets:Bank", EUR, end_of_march, amount!("1000.00"));
/// let AssertionOutcome::Failed(failure) = books.check_assertion(&claim)? else {
///     panic!("the bank says 1000.00");
/// };
/// assert_eq!((failure.difference.direction, failure.difference.units), (Deviation::Shortfall, 1000));
/// assert_eq!(
///     failure.to_string(),
///     "Assets:Bank EUR settled at the end of 2026-03-31 by value date: expected 1000.00, actual 990.00, shortfall 10.00",
/// );
/// # Ok::<(), Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceAssertion<const P: u8 = 2> {
    account: AccountName,
    currency: Currency,
    layer: Layer,
    at: AssertAt,
    expected: Amount<P>,
}

impl<const P: u8> BalanceAssertion<P> {
    /// The claim that `account` has the net `expected` (debit positive) in
    /// `currency`, Settled, at `at`. A name that is no account is
    /// [`QueryError::UnknownAccount`](crate::error::QueryError::UnknownAccount)
    /// when the assertion is checked.
    pub fn new<A>(account: A, currency: Currency, at: AssertAt, expected: Amount<P>) -> Self
    where
        A: TryInto<AccountRef> + fmt::Display,
        A::Error: Into<ValueError>,
    {
        Self {
            account: AccountName::new(account),
            currency,
            layer: Layer::Settled,
            at,
            expected,
        }
    }

    /// The same claim about the Pending layer.
    #[must_use]
    pub fn pending(self) -> Self {
        Self {
            layer: Layer::Pending,
            ..self
        }
    }

    /// The account as it was named.
    #[must_use]
    pub fn account(&self) -> &str {
        self.account.text()
    }

    /// The currency.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// The layer.
    #[must_use]
    pub const fn layer(&self) -> Layer {
        self.layer
    }

    /// The point of the log.
    #[must_use]
    pub const fn at(&self) -> AssertAt {
        self.at
    }

    /// The net claimed.
    #[must_use]
    pub const fn expected(&self) -> Amount<P> {
        self.expected
    }

    /// The account name, for the façades' lookup.
    pub(crate) const fn name(&self) -> &AccountName {
        &self.account
    }

    /// The balance query whose answer the assertion is about.
    pub(crate) fn query(&self) -> BalanceQuery {
        let query = match self.at {
            AssertAt::EndOfDay { date, basis } => {
                let query = BalanceQuery::through(date);
                match basis {
                    DateBasis::Booking => query,
                    DateBasis::Value => query.by_value_date(),
                }
            }
            AssertAt::Position(position) => BalanceQuery::through_position(position),
        };
        match self.layer {
            Layer::Settled => query,
            Layer::Pending => query.pending(),
        }
    }

    /// Checks the claim against `actual`, the account's balance at the
    /// point. Infallible: the difference of any two amounts fits a `u128`.
    ///
    /// ```
    /// use doubleentry::{Amount, Balance, currency::EUR, report::{AssertAt, AssertionOutcome, BalanceAssertion, Deviation}};
    ///
    /// let actual = Balance::from_parts(Amount::MAX, Amount::ZERO, 1)?;
    /// let claim = BalanceAssertion::<2>::new("Assets:Bank", EUR, AssertAt::Position(1), Amount::MIN);
    /// let AssertionOutcome::Failed(failure) = claim.check(&actual) else { panic!() };
    /// assert_eq!(failure.difference.direction, Deviation::Excess);
    /// assert_eq!(failure.difference.units, 2 * i128::MAX.unsigned_abs());
    /// # Ok::<(), doubleentry::error::ValueError>(())
    /// ```
    #[must_use]
    pub fn check(&self, actual: &Balance<P>) -> AssertionOutcome<P> {
        let actual = actual.net();
        if actual == self.expected {
            return AssertionOutcome::Held { actual };
        }
        let direction = if actual > self.expected {
            Deviation::Excess
        } else {
            Deviation::Shortfall
        };
        AssertionOutcome::Failed(AssertionFailure {
            assertion: self.clone(),
            actual,
            difference: Difference {
                direction,
                units: actual.to_minor().abs_diff(self.expected.to_minor()),
            },
        })
    }
}

/// Whether the ledger holds more or less than an assertion claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Deviation {
    /// The actual net is above the expected one.
    Excess,
    /// The actual net is below the expected one.
    Shortfall,
}

impl fmt::Display for Deviation {
    /// `excess` or `shortfall`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        pad(
            f,
            match self {
                Self::Excess => "excess",
                Self::Shortfall => "shortfall",
            },
        )
    }
}

/// How far the actual net is from the expected one: a direction and a
/// magnitude in units of `10^-P`, which always fits (`|a − b| ≤ 2·i128::MAX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Difference<const P: u8 = 2> {
    /// Above or below.
    pub direction: Deviation,
    /// The magnitude, in units of `10^-P`.
    pub units: u128,
}

impl<const P: u8> fmt::Display for Difference<P> {
    /// `shortfall 10.00`: the direction and the magnitude with `P`
    /// decimals, honouring width, fill and alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.units.to_string();
        let scale = usize::from(P);
        let mut magnitude = "0".repeat(scale.saturating_add(1).saturating_sub(digits.len()));
        magnitude.push_str(&digits);
        if scale > 0 {
            magnitude.insert(magnitude.len().saturating_sub(scale), '.');
        }
        pad(f, &format!("{} {magnitude}", self.direction))
    }
}

/// The outcome of checking one [`BalanceAssertion`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AssertionOutcome<const P: u8 = 2> {
    /// The ledger holds what was claimed.
    Held {
        /// The net held.
        actual: Amount<P>,
    },
    /// It does not.
    Failed(AssertionFailure<P>),
}

impl<const P: u8> AssertionOutcome<P> {
    /// Whether the claim held.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        matches!(self, Self::Held { .. })
    }
}

impl<const P: u8> fmt::Display for AssertionOutcome<P> {
    /// `held at 1000.00`, or the failure; honouring width, fill and
    /// alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held { actual } => pad(f, &format!("held at {actual}")),
            Self::Failed(failure) => fmt::Display::fmt(failure, f),
        }
    }
}

/// A claim the ledger does not bear out: what was expected, what it holds,
/// and the difference.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AssertionFailure<const P: u8 = 2> {
    /// The claim.
    pub assertion: BalanceAssertion<P>,
    /// The net the ledger holds.
    pub actual: Amount<P>,
    /// The actual net relative to the expected one.
    pub difference: Difference<P>,
}

impl<const P: u8> fmt::Display for AssertionFailure<P> {
    /// `Assets:Bank EUR settled at the end of 2026-03-31 by value date:
    /// expected 1000.00, actual 990.00, shortfall 10.00`, honouring width,
    /// fill and alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let claim = &self.assertion;
        pad(
            f,
            &format!(
                "{} {} {} {}: expected {}, actual {}, {}",
                claim.account(),
                claim.currency,
                claim.layer,
                claim.at,
                claim.expected,
                self.actual,
                self.difference
            ),
        )
    }
}
