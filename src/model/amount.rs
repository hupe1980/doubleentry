//! Exact money: [`Amount`], [`Rounding`], splits, and the gross [`Balance`].

use core::fmt;
use core::iter::Sum;
use core::str::FromStr;

use super::{Decode, DecodeError, Encode, Reader, Side, Sink, ValueError, Writer};

/// The most parts [`Amount::allocate`] and [`Amount::distribute`] split into.
pub(crate) const MAX_PARTS: usize = 65_536;

/// Why an amount operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AmountError {
    /// The result (or an intermediate product) is outside the symmetric range
    /// `±(2¹²⁷ − 1)` minor units.
    #[error("the amount is out of range")]
    Overflow,
    /// The text is not `-?[0-9]+(\.[0-9]{1,P})?`.
    #[error("not a decimal amount literal")]
    InvalidLiteral,
    /// More fraction digits than the scale allows (zeros included).
    #[error("more decimal places than the scale {scale} allows")]
    PrecisionLoss {
        /// The scale `P`.
        scale: u8,
    },
    /// The weights are empty or all zero.
    #[error("the weights are empty or all zero")]
    ZeroWeight,
    /// More parts than [`Amount::MAX_PARTS`].
    #[error("more than {max} parts")]
    TooManyParts {
        /// [`Amount::MAX_PARTS`].
        max: usize,
    },
    /// A ratio with a zero denominator.
    #[error("division by zero")]
    DivideByZero,
    /// An amount that must be positive is zero or negative.
    #[error("the amount must be positive")]
    NotPositive,
}

/// How a result that falls between two representable amounts is rounded.
///
/// There is no default: every operation that can produce a fraction of a
/// minor unit takes the mode as an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Rounding {
    /// To the nearest; ties away from zero.
    HalfUp,
    /// To the nearest; ties to the even neighbour (banker's rounding).
    HalfEven,
    /// To the nearest; ties toward zero.
    HalfDown,
    /// Toward zero (truncation).
    TowardZero,
    /// Away from zero.
    AwayFromZero,
    /// Toward negative infinity.
    Floor,
    /// Toward positive infinity.
    Ceiling,
}

/// An exact amount of money: an `i128` count of 10⁻ᴾ units.
///
/// The range is symmetric, `−(2¹²⁷ − 1) ..= 2¹²⁷ − 1` minor units, so
/// [`negate`](Self::negate) and [`abs`](Self::abs) are total. `P` is fixed at
/// compile time (default 2, at most 18). There are no arithmetic operators:
/// every fallible operation returns a [`Result`], and every operation that can
/// produce a fraction of a unit takes an explicit [`Rounding`].
///
/// Write a literal with [`amount!`](crate::amount), which is checked by the
/// compiler; parse text with [`FromStr`] (strict: excess digits are refused,
/// never rounded).
///
/// ```
/// use doubleentry::{Amount, Rounding, amount};
///
/// let net: Amount = amount!("1000.00");
/// let gross = net.checked_mul_ratio(119, 100, Rounding::HalfEven)?;
/// assert_eq!(gross, amount!("1190.00"));
/// assert_eq!(format!("{gross:>10}"), "   1190.00");
/// assert_eq!("1190.001".parse::<Amount>().is_err(), true);
/// # Ok::<(), doubleentry::error::AmountError>(())
/// ```
///
/// A scale above 18 does not compile:
///
/// ```compile_fail
/// let _ = doubleentry::Amount::<19>::ZERO;
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount<const P: u8 = 2>(i128);

impl<const P: u8> Amount<P> {
    /// The compile-time scale check, referenced by every constructor.
    const SCALE_OK: () = assert!(P <= 18, "P must be <= 18");

    /// 10^P, the number of minor units in one major unit.
    const UNIT: i128 = {
        let () = Self::SCALE_OK;
        pow10(P)
    };

    /// The largest scale.
    pub const MAX_PRECISION: u8 = 18;

    /// Zero.
    pub const ZERO: Self = {
        let () = Self::SCALE_OK;
        Self(0)
    };

    /// The largest amount, `2¹²⁷ − 1` minor units.
    pub const MAX: Self = {
        let () = Self::SCALE_OK;
        Self(i128::MAX)
    };

    /// The smallest amount, `−MAX` (`i128::MIN` is not representable).
    pub const MIN: Self = {
        let () = Self::SCALE_OK;
        Self(-i128::MAX)
    };

    /// An amount from minor units; `i128::MIN` is refused.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] for `i128::MIN`.
    pub const fn from_minor(minor: i128) -> Result<Self, AmountError> {
        let () = Self::SCALE_OK;
        if minor == i128::MIN {
            Err(AmountError::Overflow)
        } else {
            Ok(Self(minor))
        }
    }

    /// The count of minor units.
    #[must_use]
    pub const fn to_minor(self) -> i128 {
        self.0
    }

    /// An amount of whole major units.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] when `major × 10^P` is out of range.
    pub fn from_major(major: i128) -> Result<Self, AmountError> {
        major
            .checked_mul(Self::UNIT)
            .ok_or(AmountError::Overflow)
            .and_then(Self::from_minor)
    }

    /// Parses `-?[0-9]+(\.[0-9]{1,P})?` in a `const` context; the parser
    /// behind [`amount!`](crate::amount) and [`FromStr`].
    ///
    /// # Errors
    ///
    /// [`AmountError::InvalidLiteral`] for text outside the grammar,
    /// [`AmountError::PrecisionLoss`] for a fraction digit beyond `P` (zeros
    /// included), [`AmountError::Overflow`] above [`MAX`](Self::MAX).
    pub const fn parse_const(text: &str) -> Result<Self, AmountError> {
        let () = Self::SCALE_OK;
        let mut rest = text.as_bytes();
        let negative = if let [b'-', tail @ ..] = rest {
            rest = tail;
            true
        } else {
            false
        };
        let mut magnitude: i128 = 0;
        let mut overflow = false;
        let mut integer_digits = 0usize;
        while let [digit @ b'0'..=b'9', tail @ ..] = rest {
            magnitude = push_digit(magnitude, *digit, &mut overflow);
            integer_digits = integer_digits.saturating_add(1);
            rest = tail;
        }
        if integer_digits == 0 {
            return Err(AmountError::InvalidLiteral);
        }
        let mut fraction_digits: u8 = 0;
        let mut excess = false;
        if let [b'.', tail @ ..] = rest {
            rest = tail;
            while let [digit @ b'0'..=b'9', tail @ ..] = rest {
                if fraction_digits >= P {
                    excess = true;
                } else {
                    magnitude = push_digit(magnitude, *digit, &mut overflow);
                    fraction_digits = fraction_digits.saturating_add(1);
                }
                rest = tail;
            }
            if fraction_digits == 0 && !excess {
                return Err(AmountError::InvalidLiteral);
            }
        }
        if !rest.is_empty() {
            return Err(AmountError::InvalidLiteral);
        }
        if excess {
            return Err(AmountError::PrecisionLoss { scale: P });
        }
        while fraction_digits < P {
            magnitude = push_digit(magnitude, b'0', &mut overflow);
            fraction_digits = fraction_digits.saturating_add(1);
        }
        if overflow {
            return Err(AmountError::Overflow);
        }
        // `magnitude` lies in `0..=i128::MAX`, so its negation is exact.
        Ok(Self(if negative {
            magnitude.wrapping_neg()
        } else {
            magnitude
        }))
    }

    /// Parses a decimal amount (the same grammar as [`parse_const`](Self::parse_const)).
    ///
    /// # Errors
    ///
    /// As [`parse_const`](Self::parse_const).
    pub fn parse(text: &str) -> Result<Self, AmountError> {
        Self::parse_const(text)
    }

    /// `self + rhs`.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] outside the range.
    pub fn checked_add(self, rhs: Self) -> Result<Self, AmountError> {
        self.0
            .checked_add(rhs.0)
            .ok_or(AmountError::Overflow)
            .and_then(Self::from_minor)
    }

    /// `self − rhs`.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] outside the range.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, AmountError> {
        self.0
            .checked_sub(rhs.0)
            .ok_or(AmountError::Overflow)
            .and_then(Self::from_minor)
    }

    /// `self × factor`.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] outside the range.
    pub fn checked_mul_int(self, factor: i128) -> Result<Self, AmountError> {
        self.0
            .checked_mul(factor)
            .ok_or(AmountError::Overflow)
            .and_then(Self::from_minor)
    }

    /// `self × numerator / denominator`: the exact integer product, then one
    /// division rounded by `rounding`.
    ///
    /// ```
    /// use doubleentry::{Amount, Rounding, amount};
    ///
    /// let one: Amount = amount!("1.00");
    /// let third = one.checked_mul_ratio(1, 3, Rounding::HalfUp)?;
    /// assert_eq!(third, amount!("0.33"));
    /// # Ok::<(), doubleentry::error::AmountError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`AmountError::DivideByZero`] for a zero denominator;
    /// [`AmountError::Overflow`] when the product leaves the `i128` range
    /// (even if the quotient would fit) or the result leaves the amount range.
    pub fn checked_mul_ratio(
        self,
        numerator: i128,
        denominator: i128,
        rounding: Rounding,
    ) -> Result<Self, AmountError> {
        if denominator == 0 {
            return Err(AmountError::DivideByZero);
        }
        let product = self.0.checked_mul(numerator).ok_or(AmountError::Overflow)?;
        div_rounded(product, denominator, rounding).and_then(Self::from_minor)
    }

    /// The same value at scale `Q`, rounded by `rounding` when `Q < P`.
    ///
    /// ```
    /// use doubleentry::{Amount, Rounding, amount};
    ///
    /// let price: Amount<4> = amount!("2.3450");
    /// let cents: Amount<2> = price.rescale(Rounding::HalfEven)?;
    /// assert_eq!(cents, amount!("2.34"));
    /// # Ok::<(), doubleentry::error::AmountError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] when scaling up leaves the range.
    pub fn rescale<const Q: u8>(self, rounding: Rounding) -> Result<Amount<Q>, AmountError> {
        let minor = if Q >= P {
            self.0
                .checked_mul(pow10(Q.saturating_sub(P)))
                .ok_or(AmountError::Overflow)?
        } else {
            div_rounded(self.0, pow10(P.saturating_sub(Q)), rounding)?
        };
        Amount::<Q>::from_minor(minor)
    }

    /// The sum of `amounts`, refusing overflow of any partial sum.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] when a partial sum leaves the range.
    pub fn checked_sum<I: IntoIterator<Item = Self>>(amounts: I) -> Result<Self, AmountError> {
        amounts.into_iter().try_fold(Self::ZERO, Self::checked_add)
    }

    /// `−self` (total: the range is symmetric).
    #[must_use]
    pub const fn negate(self) -> Self {
        // `self.0 != i128::MIN` by construction, so the negation is exact.
        Self(self.0.wrapping_neg())
    }

    /// `|self|` (total: the range is symmetric).
    #[must_use]
    pub const fn abs(self) -> Self {
        // `self.0 != i128::MIN` by construction, so the absolute value is exact.
        Self(self.0.wrapping_abs())
    }

    /// Whether the amount is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether the amount is above zero.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// Whether the amount is below zero.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// Whether the amount has no non-zero digit beyond `exponent` decimals —
    /// whether it can be posted in a currency declared with that exponent.
    /// Always `false` when `exponent > P`.
    ///
    /// ```
    /// use doubleentry::{Amount, amount};
    ///
    /// let yen: Amount = amount!("120.00");
    /// assert!(yen.fits_exponent(0));
    /// let cents: Amount = amount!("120.50");
    /// assert!(!cents.fits_exponent(0));
    /// assert!(cents.fits_exponent(1));
    /// ```
    #[must_use]
    pub const fn fits_exponent(self, exponent: u8) -> bool {
        if exponent > P {
            return false;
        }
        matches!(
            self.0.checked_rem(pow10(P.saturating_sub(exponent))),
            Some(0)
        )
    }

    /// Splits the amount in proportion to `weights` by the largest-remainder
    /// method: the parts re-sum exactly, zero weights receive zero, and the
    /// leftover minor units go to the largest remainders, ties to the lowest
    /// index.
    ///
    /// ```
    /// use doubleentry::{Amount, amount};
    ///
    /// let hundred: Amount = amount!("100.00");
    /// let parts = hundred.allocate(&[1, 1, 1])?;
    /// assert_eq!(parts, [amount!("33.34"), amount!("33.33"), amount!("33.33")]);
    /// # Ok::<(), doubleentry::error::AmountError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`AmountError::ZeroWeight`] for no weights or all-zero weights,
    /// [`AmountError::TooManyParts`] above [`Amount::MAX_PARTS`],
    /// [`AmountError::Overflow`] when the weights sum above `u64::MAX`.
    pub fn allocate(self, weights: &[u64]) -> Result<Vec<Self>, AmountError> {
        if weights.len() > MAX_PARTS {
            return Err(AmountError::TooManyParts { max: MAX_PARTS });
        }
        let total = weights
            .iter()
            .try_fold(0u64, |sum, weight| sum.checked_add(*weight))
            .ok_or(AmountError::Overflow)?;
        if total == 0 {
            return Err(AmountError::ZeroWeight);
        }
        let total = u128::from(total);
        let magnitude = self.0.unsigned_abs();
        // |amount| = q·Σw + r with r < Σw ≤ 2⁶⁴, so q·w ≤ |amount| and
        // r·w < 2¹²⁸: every share fits in u128 without a wider type.
        let q = magnitude
            .checked_div(total)
            .ok_or(AmountError::ZeroWeight)?;
        let r = magnitude
            .checked_rem(total)
            .ok_or(AmountError::ZeroWeight)?;
        let mut shares = Vec::with_capacity(weights.len());
        let mut remainders = Vec::with_capacity(weights.len());
        let mut assigned: u128 = 0;
        for weight in weights {
            let weight = u128::from(*weight);
            let spread = r.checked_mul(weight).ok_or(AmountError::Overflow)?;
            let share = q
                .checked_mul(weight)
                .and_then(|base| base.checked_add(spread.checked_div(total)?))
                .ok_or(AmountError::Overflow)?;
            assigned = assigned.checked_add(share).ok_or(AmountError::Overflow)?;
            shares.push(share);
            remainders.push(spread.checked_rem(total).ok_or(AmountError::Overflow)?);
        }
        let leftover = magnitude
            .checked_sub(assigned)
            .ok_or(AmountError::Overflow)?;
        let mut order: Vec<usize> = (0..weights.len()).collect();
        // Largest remainder first; `sort_by` is stable, so ties keep the lowest index first.
        order.sort_by(|a, b| remainders.get(*b).cmp(&remainders.get(*a)));
        let leftover = usize::try_from(leftover).map_err(|_| AmountError::Overflow)?;
        for index in order.into_iter().take(leftover) {
            if let Some(share) = shares.get_mut(index) {
                *share = share.checked_add(1).ok_or(AmountError::Overflow)?;
            }
        }
        shares
            .into_iter()
            .map(|share| self.signed_part(share))
            .collect()
    }

    /// Splits the amount into `parts` equal parts, the first ones one minor
    /// unit larger when it does not divide evenly (as
    /// [`allocate`](Self::allocate) with equal weights, without building a
    /// weight vector).
    ///
    /// ```
    /// use doubleentry::{Amount, amount};
    ///
    /// let parts: Vec<Amount> = amount!("-0.05").distribute(3)?;
    /// assert_eq!(parts, [amount!("-0.02"), amount!("-0.02"), amount!("-0.01")]);
    /// # Ok::<(), doubleentry::error::AmountError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`AmountError::ZeroWeight`] for zero parts, [`AmountError::TooManyParts`]
    /// above [`Amount::MAX_PARTS`].
    pub fn distribute(self, parts: usize) -> Result<Vec<Self>, AmountError> {
        if parts == 0 {
            return Err(AmountError::ZeroWeight);
        }
        if parts > MAX_PARTS {
            return Err(AmountError::TooManyParts { max: MAX_PARTS });
        }
        let count =
            u128::try_from(parts).map_err(|_| AmountError::TooManyParts { max: MAX_PARTS })?;
        let magnitude = self.0.unsigned_abs();
        let q = magnitude
            .checked_div(count)
            .ok_or(AmountError::ZeroWeight)?;
        let r = magnitude
            .checked_rem(count)
            .ok_or(AmountError::ZeroWeight)?;
        let r = usize::try_from(r).map_err(|_| AmountError::Overflow)?;
        let larger = q.checked_add(1).ok_or(AmountError::Overflow)?;
        (0..parts)
            .map(|index| self.signed_part(if index < r { larger } else { q }))
            .collect()
    }

    /// A part of `|self|` with the sign of `self`.
    fn signed_part(self, magnitude: u128) -> Result<Self, AmountError> {
        let part = i128::try_from(magnitude).map_err(|_| AmountError::Overflow)?;
        // `part ≤ |self| ≤ i128::MAX`, so the negation is exact.
        Self::from_minor(if self.0 < 0 {
            part.wrapping_neg()
        } else {
            part
        })
    }
}

/// `10^exponent` for `exponent ≤ 18` (the scale bound); larger exponents are
/// never requested.
const fn pow10(exponent: u8) -> i128 {
    let mut result: i128 = 1;
    let mut left = exponent;
    while left > 0 {
        result = match result.checked_mul(10) {
            Some(next) => next,
            None => return i128::MAX,
        };
        left = left.saturating_sub(1);
    }
    result
}

/// `magnitude × 10 + digit`, recording overflow instead of wrapping.
const fn push_digit(magnitude: i128, digit: u8, overflow: &mut bool) -> i128 {
    let value = digit.saturating_sub(b'0') as i128;
    match magnitude.checked_mul(10) {
        Some(shifted) => match shifted.checked_add(value) {
            Some(next) => next,
            None => {
                *overflow = true;
                0
            }
        },
        None => {
            *overflow = true;
            0
        }
    }
}

/// `numerator / denominator` rounded by `rounding`.
fn div_rounded(
    numerator: i128,
    denominator: i128,
    rounding: Rounding,
) -> Result<i128, AmountError> {
    // `checked_div` fails only for a zero denominator or `i128::MIN / −1`,
    // whose quotient is out of range anyway.
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(AmountError::Overflow)?;
    let remainder = numerator
        .checked_rem(denominator)
        .ok_or(AmountError::Overflow)?;
    if remainder == 0 {
        return Ok(quotient);
    }
    let negative = (numerator < 0) != (denominator < 0);
    let away = if negative {
        quotient.checked_sub(1)
    } else {
        quotient.checked_add(1)
    }
    .ok_or(AmountError::Overflow)?;
    // Compare |r| with |d| − |r| to place the exact quotient against the
    // midpoint without doubling (which could overflow).
    let r = remainder.unsigned_abs();
    let rest = denominator
        .unsigned_abs()
        .checked_sub(r)
        .ok_or(AmountError::Overflow)?;
    let half = r.cmp(&rest);
    let round_away = match rounding {
        Rounding::TowardZero => false,
        Rounding::AwayFromZero => true,
        Rounding::Floor => negative,
        Rounding::Ceiling => !negative,
        Rounding::HalfUp => half.is_ge(),
        Rounding::HalfDown => half.is_gt(),
        Rounding::HalfEven => half.is_gt() || (half.is_eq() && quotient & 1 != 0),
    };
    Ok(if round_away { away } else { quotient })
}

impl<const P: u8> Default for Amount<P> {
    fn default() -> Self {
        Self::ZERO
    }
}

impl<const P: u8> fmt::Display for Amount<P> {
    /// `-?<integer>.<exactly P digits>` (integer only at `P = 0`), honouring
    /// width, fill, alignment and the `+` and `0` flags.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad_integral(self.0 >= 0, "", &unsigned_text(self.0.unsigned_abs(), P))
    }
}

/// `|value|` at `scale` decimal places: `<integer>.<exactly scale digits>`.
fn unsigned_text(magnitude: u128, scale: u8) -> String {
    let digits = magnitude.to_string();
    let scale = usize::from(scale);
    let mut text = String::with_capacity(digits.len().saturating_add(scale).saturating_add(2));
    let padding = scale.saturating_add(1).saturating_sub(digits.len());
    text.extend(core::iter::repeat_n('0', padding));
    text.push_str(&digits);
    if scale > 0 {
        text.insert(text.len().saturating_sub(scale), '.');
    }
    text
}

/// An amount written at a currency's exponent ([`Amount::display_at`]).
struct AtExponent<const P: u8> {
    amount: Amount<P>,
    exponent: u8,
}

impl<const P: u8> fmt::Display for AtExponent<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.amount.0;
        let exponent = self.exponent.min(P);
        let unit = pow10(P.saturating_sub(exponent));
        let (magnitude, scale) = if value.checked_rem(unit) == Some(0) {
            (
                value
                    .unsigned_abs()
                    .checked_div(unit.unsigned_abs())
                    .unwrap_or(0),
                exponent,
            )
        } else {
            (value.unsigned_abs(), P)
        };
        f.pad_integral(value >= 0, "", &unsigned_text(magnitude, scale))
    }
}

impl<const P: u8> Amount<P> {
    /// The most parts [`allocate`](Self::allocate) and
    /// [`distribute`](Self::distribute) split into.
    pub const MAX_PARTS: usize = MAX_PARTS;

    /// The amount written with `exponent` decimal places — a currency's
    /// declared exponent, so JPY reads `15000` rather than `15000.00`. A
    /// non-zero digit is never cut: an amount with more decimals than
    /// `exponent` (which validation keeps out of a currency's postings) is
    /// written at `P`, as [`Display`](fmt::Display) does; an exponent above
    /// `P` is `P`. Honours width, fill, alignment and the `+` and `0` flags.
    ///
    /// ```
    /// use doubleentry::{Amount, amount};
    ///
    /// let (yen, part, cents): (Amount, Amount, Amount) =
    ///     (amount!("15000.00"), amount!("-1.50"), amount!("0.25"));
    /// assert_eq!(yen.display_at(0).to_string(), "15000");
    /// assert_eq!(part.display_at(1).to_string(), "-1.5");
    /// assert_eq!(cents.display_at(0).to_string(), "0.25"); // never cut
    /// assert_eq!(format!("{:>7}", yen.display_at(0)), "  15000");
    /// ```
    #[must_use]
    pub fn display_at(self, exponent: u8) -> impl fmt::Display {
        AtExponent {
            amount: self,
            exponent,
        }
    }
}

impl<const P: u8> fmt::Debug for Amount<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Amount<{P}>({self})")
    }
}

impl<const P: u8> FromStr for Amount<P> {
    type Err = AmountError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse_const(text)
    }
}

impl<const P: u8> Sum<Amount<P>> for Result<Amount<P>, AmountError> {
    fn sum<I: Iterator<Item = Amount<P>>>(iter: I) -> Self {
        Amount::checked_sum(iter)
    }
}

impl<'a, const P: u8> Sum<&'a Amount<P>> for Result<Amount<P>, AmountError> {
    fn sum<I: Iterator<Item = &'a Amount<P>>>(iter: I) -> Self {
        Amount::checked_sum(iter.copied())
    }
}

impl<const P: u8> Encode for Amount<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.i128(self.0);
    }
}

impl<const P: u8> Decode for Amount<P> {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        Self::from_minor(r.i128()?).map_err(|_| DecodeError::Value {
            field,
            error: ValueError::OutOfRange,
        })
    }
}

#[cfg(feature = "serde")]
impl<const P: u8> serde::Serialize for Amount<P> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de, const P: u8> serde::Deserialize<'de> for Amount<P> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::serde_str::deserialize(deserializer)
    }
}

/// A compile-checked [`Amount`] literal; `P` is inferred from context.
///
/// The literal follows the strict grammar of [`Amount::parse_const`]; an
/// invalid literal is a compile error naming the problem, and the expansion
/// has no run-time panic path.
///
/// ```
/// use doubleentry::{Amount, amount};
///
/// fn takes_cents(a: Amount) -> i128 { a.to_minor() }
/// fn generic<const P: u8>() -> Amount<P> { amount!("1") }
/// const FEE: Amount = amount!("2.50");
///
/// assert_eq!(takes_cents(amount!("1190.00")), 119_000);
/// let later = amount!("0.10");
/// assert_eq!(takes_cents(later), 10);
/// assert_eq!(generic::<6>().to_minor(), 1_000_000);
/// assert_eq!(FEE.to_minor(), 250);
/// ```
///
/// More decimal places than the scale allows:
///
/// ```compile_fail
/// let _: doubleentry::Amount = doubleentry::amount!("1190.001");
/// ```
///
/// Not a number:
///
/// ```compile_fail
/// let _: doubleentry::Amount = doubleentry::amount!("12x");
/// ```
#[macro_export]
macro_rules! amount {
    ($literal:literal) => {
        const {
            // Total at run time: this panic can only fire while the inline
            // `const` block is evaluated, i.e. as a compile error.
            #[allow(clippy::panic)]
            let amount = match $crate::Amount::parse_const($literal) {
                ::core::result::Result::Ok(amount) => amount,
                ::core::result::Result::Err($crate::error::AmountError::PrecisionLoss {
                    ..
                }) => {
                    ::core::panic!(::core::concat!(
                        "amount!(",
                        ::core::stringify!($literal),
                        "): more decimal places than the scale P allows"
                    ))
                }
                ::core::result::Result::Err($crate::error::AmountError::Overflow) => {
                    ::core::panic!(::core::concat!(
                        "amount!(",
                        ::core::stringify!($literal),
                        "): out of range"
                    ))
                }
                ::core::result::Result::Err(_) => ::core::panic!(::core::concat!(
                    "amount!(",
                    ::core::stringify!($literal),
                    "): not a decimal amount (expected -?[0-9]+(.[0-9]+)?)"
                )),
            };
            amount
        }
    };
}

/// The gross T-account of one (account, currency, layer): debits and credits
/// kept apart, and the number of postings.
///
/// Both totals are non-negative and only grow; `postings = 0` exactly when
/// both are zero. Equality is gross: `7 // 5` and `2 // 0` have the same
/// [`net`](Self::net) but are different balances.
///
/// ```
/// use doubleentry::{Balance, Side, amount};
///
/// let b: Balance = Balance::ZERO.post(Side::Debit, amount!("7.00"))?.post(Side::Credit, amount!("5.00"))?;
/// assert_eq!(b.net(), amount!("2.00"));
/// assert_eq!(b.postings(), 2);
/// assert_ne!(b, Balance::ZERO.post(Side::Debit, amount!("2.00"))?);
/// # Ok::<(), doubleentry::error::AmountError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Balance<const P: u8 = 2> {
    debit: Amount<P>,
    credit: Amount<P>,
    postings: u64,
}

/// One line, `dr 420.00 cr 420.00 net 0.00 (2 postings)`, honouring width,
/// fill and alignment and never cut short.
///
/// ```
/// use doubleentry::{Amount, Balance, Side, amount};
///
/// let paid = Balance::<2>::ZERO.post(Side::Debit, amount!("420.00"))?;
/// assert_eq!(paid.to_string(), "dr 420.00 cr 0.00 net 420.00 (1 posting)");
/// assert_eq!(format!("[{paid:>44}]"), "[    dr 420.00 cr 0.00 net 420.00 (1 posting)]");
/// # Ok::<(), doubleentry::error::AmountError>(())
/// ```
impl<const P: u8> fmt::Display for Balance<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let postings = match self.postings {
            1 => String::from("1 posting"),
            count => format!("{count} postings"),
        };
        let text = format!(
            "dr {} cr {} net {} ({postings})",
            self.debit,
            self.credit,
            self.net()
        );
        super::encoding::pad(f, &text)
    }
}

impl<const P: u8> Balance<P> {
    /// No postings.
    pub const ZERO: Self = Self {
        debit: Amount::ZERO,
        credit: Amount::ZERO,
        postings: 0,
    };

    /// A balance from its parts.
    ///
    /// # Errors
    ///
    /// [`ValueError::OutOfRange`] for a negative total, or when `postings = 0`
    /// does not coincide with both totals being zero.
    pub fn from_parts(
        debit: Amount<P>,
        credit: Amount<P>,
        postings: u64,
    ) -> Result<Self, ValueError> {
        let empty = debit.is_zero() && credit.is_zero();
        if debit.is_negative() || credit.is_negative() || empty != (postings == 0) {
            return Err(ValueError::OutOfRange);
        }
        Ok(Self {
            debit,
            credit,
            postings,
        })
    }

    /// The gross debit total.
    #[must_use]
    pub const fn debit(&self) -> Amount<P> {
        self.debit
    }

    /// The gross credit total.
    #[must_use]
    pub const fn credit(&self) -> Amount<P> {
        self.credit
    }

    /// The number of postings.
    #[must_use]
    pub const fn postings(&self) -> u64 {
        self.postings
    }

    /// The balance after one more posting.
    ///
    /// # Errors
    ///
    /// [`AmountError::NotPositive`] for an amount ≤ 0; [`AmountError::Overflow`]
    /// when a total or the count leaves its range.
    pub fn post(self, side: Side, amount: Amount<P>) -> Result<Self, AmountError> {
        if !amount.is_positive() {
            return Err(AmountError::NotPositive);
        }
        let postings = self.postings.checked_add(1).ok_or(AmountError::Overflow)?;
        Ok(match side {
            Side::Debit => Self {
                debit: self.debit.checked_add(amount)?,
                postings,
                ..self
            },
            Side::Credit => Self {
                credit: self.credit.checked_add(amount)?,
                postings,
                ..self
            },
        })
    }

    /// The component-wise sum of two balances.
    ///
    /// # Errors
    ///
    /// [`AmountError::Overflow`] when a component leaves its range.
    pub fn checked_add(self, other: &Self) -> Result<Self, AmountError> {
        Ok(Self {
            debit: self.debit.checked_add(other.debit)?,
            credit: self.credit.checked_add(other.credit)?,
            postings: self
                .postings
                .checked_add(other.postings)
                .ok_or(AmountError::Overflow)?,
        })
    }

    /// `debit − credit` (total: both lie in `0..=MAX`).
    #[must_use]
    pub const fn net(&self) -> Amount<P> {
        // Both components lie in `0..=i128::MAX`, so the difference lies in
        // `−MAX..=MAX` and the subtraction is exact.
        Amount(self.debit.0.wrapping_sub(self.credit.0))
    }

    /// Whether two balances have the same net, however they got there.
    #[must_use]
    pub fn nets_equal(&self, other: &Self) -> bool {
        self.net() == other.net()
    }

    /// Whether the balance has no postings.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.postings == 0
    }
}

impl<const P: u8> Default for Balance<P> {
    fn default() -> Self {
        Self::ZERO
    }
}

impl<const P: u8> Encode for Balance<P> {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.value(&self.debit);
        w.value(&self.credit);
        w.u64(self.postings);
    }
}

impl<const P: u8> Decode for Balance<P> {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        let debit = r.value(field)?;
        let credit = r.value(field)?;
        let postings = r.u64()?;
        Self::from_parts(debit, credit, postings)
            .map_err(|error| DecodeError::Value { field, error })
    }
}

#[cfg(feature = "serde")]
mod balance_serde {
    use super::{Amount, Balance, ValueError};

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(rename = "Balance")]
    pub(super) struct Parts<const P: u8> {
        debit: Amount<P>,
        credit: Amount<P>,
        postings: u64,
    }

    impl<const P: u8> From<Balance<P>> for Parts<P> {
        fn from(balance: Balance<P>) -> Self {
            Self {
                debit: balance.debit,
                credit: balance.credit,
                postings: balance.postings,
            }
        }
    }

    impl<const P: u8> TryFrom<Parts<P>> for Balance<P> {
        type Error = ValueError;

        fn try_from(parts: Parts<P>) -> Result<Self, Self::Error> {
            Self::from_parts(parts.debit, parts.credit, parts.postings)
        }
    }

    impl<const P: u8> serde::Serialize for Balance<P> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            Parts::from(*self).serialize(serializer)
        }
    }

    impl<'de, const P: u8> serde::Deserialize<'de> for Balance<P> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Parts::deserialize(deserializer)?
                .try_into()
                .map_err(serde::de::Error::custom)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pow10_covers_every_scale() {
        assert_eq!(pow10(0), 1);
        assert_eq!(pow10(2), 100);
        assert_eq!(pow10(18), 1_000_000_000_000_000_000);
    }

    #[test]
    fn half_modes_compare_against_the_midpoint_without_doubling() {
        // A remainder above i128::MAX / 2 would overflow a doubling comparison.
        let d = i128::MAX;
        let n = d - 1;
        assert_eq!(div_rounded(n, d, Rounding::HalfUp), Ok(1));
        assert_eq!(div_rounded(n, d, Rounding::TowardZero), Ok(0));
        assert_eq!(
            div_rounded(i128::MIN, 2, Rounding::HalfUp),
            Ok(i128::MIN / 2)
        );
    }
}
