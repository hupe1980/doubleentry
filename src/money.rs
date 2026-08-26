//! Exact monetary arithmetic.
//!
//! [`Amount`] is a scaled integer carrying its precision as a const generic:
//! `Amount<2>` counts hundredths, `Amount<5>` counts hundred-thousandths. There
//! is no binary floating point anywhere in this module, and no decimal type whose
//! scale can vary at runtime — a value has exactly one representation, which is
//! what makes hashing a monetary amount meaningful.
//!
//! Every operation that can overflow is fallible. There are no panicking
//! arithmetic operators on [`Amount`]: `Add` and friends are deliberately not
//! implemented, because in a ledger an overflow is a condition to report, not a
//! process to abort.

use core::fmt;

/// Failure in a monetary computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MoneyError {
    /// The result did not fit in the underlying representation.
    #[error("monetary overflow")]
    Overflow,
    /// A split was requested across zero parts, or with weights summing to zero.
    #[error("cannot allocate across zero total weight")]
    ZeroWeight,
    /// The value carried more precision than the target scale can represent.
    #[error("value has more precision than scale {scale} can represent")]
    PrecisionLoss {
        /// The target scale.
        scale: u8,
    },
    /// A currency code was not three ASCII uppercase letters.
    #[error("invalid ISO 4217 currency code")]
    InvalidCurrency,
    /// A decimal string could not be parsed at the target scale.
    #[error("invalid monetary literal")]
    InvalidLiteral,
    /// A ratio was applied with a zero denominator.
    #[error("cannot apply a ratio with a zero denominator")]
    DivideByZero,
}

/// How a result that does not land on a whole minor unit is resolved.
///
/// Every operation in this module that can produce a fraction of a minor unit
/// takes one of these, because there is no default that is right everywhere and
/// picking one silently is how a ledger drifts. VAT in most of Europe is
/// [`HalfUp`](Rounding::HalfUp); IFRS/IAS interest accrual and much of finance
/// use [`HalfEven`](Rounding::HalfEven); a fee you may not overcharge is
/// [`Floor`](Rounding::Floor).
///
/// The names follow `java.math.RoundingMode` and Python's `decimal`, so a rule
/// written against a specification in either can be transcribed rather than
/// translated.
///
/// None of these conserve a total on their own. Splitting one amount into parts
/// that must re-sum exactly is [`Amount::allocate`], which is a different
/// problem and has an exact answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Rounding {
    /// Ties away from zero; otherwise to the nearer unit. Commercial rounding.
    ///
    /// The default, because it is what a person means by "round to the cent"
    /// and what most tax authorities specify.
    #[default]
    HalfUp,
    /// Ties to the nearer *even* minor unit; otherwise to the nearer unit.
    ///
    /// Banker's rounding. Unbiased over many roundings, which is why accrual
    /// and valuation standards prefer it: `HalfUp` drifts upward on a long run
    /// of ties.
    HalfEven,
    /// Ties toward zero; otherwise to the nearer unit.
    HalfDown,
    /// Always toward zero. Truncation.
    TowardZero,
    /// Always away from zero.
    AwayFromZero,
    /// Always toward negative infinity.
    Floor,
    /// Always toward positive infinity.
    Ceiling,
}

/// Divides `numerator` by `denominator`, resolving the remainder by `rounding`.
///
/// Exact in `i128`, so no intermediate can overflow for operands derived from
/// `i64`s: the largest product two of them can form is below `2^126`.
fn div_rounded(numerator: i128, denominator: i128, rounding: Rounding) -> Option<i128> {
    if denominator == 0 {
        return None;
    }
    // Normalise the divisor positive so the remainder's sign is the dividend's.
    let (n, d) = if denominator < 0 {
        (numerator.checked_neg()?, denominator.checked_neg()?)
    } else {
        (numerator, denominator)
    };

    let quotient = n.checked_div(d)?;
    let remainder = n.checked_rem(d)?;
    if remainder == 0 {
        return Some(quotient);
    }

    // One step further from zero, in the dividend's direction.
    let away = if n < 0 {
        quotient.checked_sub(1)?
    } else {
        quotient.checked_add(1)?
    };

    // `2 * |remainder|` against `d` decides which side of the midpoint we are
    // on, without dividing again and without a floating-point comparison.
    let doubled = remainder.checked_abs()?.checked_mul(2)?;
    Some(match rounding {
        Rounding::TowardZero => quotient,
        Rounding::AwayFromZero => away,
        Rounding::Floor => {
            if n < 0 {
                away
            } else {
                quotient
            }
        }
        Rounding::Ceiling => {
            if n < 0 {
                quotient
            } else {
                away
            }
        }
        Rounding::HalfUp => {
            if doubled >= d {
                away
            } else {
                quotient
            }
        }
        Rounding::HalfDown => {
            if doubled > d {
                away
            } else {
                quotient
            }
        }
        Rounding::HalfEven => match doubled.cmp(&d) {
            core::cmp::Ordering::Greater => away,
            core::cmp::Ordering::Less => quotient,
            core::cmp::Ordering::Equal => {
                if quotient.checked_rem(2)? == 0 {
                    quotient
                } else {
                    away
                }
            }
        },
    })
}

/// An ISO 4217 currency code.
///
/// The code is validated as three ASCII uppercase letters. Minor-unit exponents
/// are known for the currencies commonly encountered; [`Currency::minor_units`]
/// returns `None` for the rest rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Currency([u8; 3]);

#[cfg(feature = "serde")]
impl serde::Serialize for Currency {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Re-runs validation: a deserialised value must satisfy the same
        // invariants as a constructed one, or the type guarantees nothing.
        let s = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

impl Currency {
    /// Euro.
    pub const EUR: Self = Self(*b"EUR");
    /// US dollar.
    pub const USD: Self = Self(*b"USD");
    /// Pound sterling.
    pub const GBP: Self = Self(*b"GBP");
    /// Swiss franc.
    pub const CHF: Self = Self(*b"CHF");
    /// Japanese yen.
    pub const JPY: Self = Self(*b"JPY");

    /// Parses a three-letter ISO 4217 code.
    pub fn new(code: &str) -> Result<Self, MoneyError> {
        let bytes = code.as_bytes();
        let [a, b, c] = bytes else {
            return Err(MoneyError::InvalidCurrency);
        };
        if !a.is_ascii_uppercase() || !b.is_ascii_uppercase() || !c.is_ascii_uppercase() {
            return Err(MoneyError::InvalidCurrency);
        }
        Ok(Self([*a, *b, *c]))
    }

    /// The three-letter code.
    #[must_use]
    pub fn code(&self) -> &str {
        // The constructor guarantees ASCII, so this is valid UTF-8.
        core::str::from_utf8(&self.0).unwrap_or("???")
    }

    /// The raw code bytes, for canonical encoding.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 3] {
        &self.0
    }

    /// The number of decimal places in the currency's minor unit, when known.
    ///
    /// Returns `None` for codes this table does not cover; callers that need a
    /// scale should require one explicitly rather than defaulting to two.
    #[must_use]
    pub fn minor_units(&self) -> Option<u8> {
        match &self.0 {
            b"JPY" | b"KRW" | b"ISK" | b"CLP" | b"VND" | b"XAF" | b"XOF" | b"XPF" => Some(0),
            b"BHD" | b"IQD" | b"JOD" | b"KWD" | b"LYD" | b"OMR" | b"TND" => Some(3),
            b"EUR" | b"USD" | b"GBP" | b"CHF" | b"AUD" | b"CAD" | b"CNY" | b"CZK" | b"DKK"
            | b"HUF" | b"NOK" | b"NZD" | b"PLN" | b"RON" | b"SEK" | b"TRY" | b"ZAR" => Some(2),
            _ => None,
        }
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// An exact monetary magnitude with `P` decimal places.
///
/// The value is stored as an `i64` count of minor units at scale `P`.
///
/// # Range, and why it depends on `P`
///
/// The `i64` bounds the *minor units*, so raising the scale spends range on
/// precision rather than on magnitude. At the scales real books are kept in this
/// is not a constraint anyone meets; at the top of the permitted range it is
/// severe, and it is easier to see as a table than to derive:
///
/// | `P` | Largest major-unit value |
/// |---|---|
/// | 0 | ~9.2 × 10¹⁸ |
/// | 2 | ~9.2 × 10¹⁶ |
/// | 4 | ~9.2 × 10¹⁴ |
/// | 8 | ~9.2 × 10¹⁰ |
/// | 18 | ~9.2 |
///
/// So `Amount<2>` covers roughly 92 quadrillion currency units — ample for both
/// individual postings and cumulative balances — while `Amount<18>` compiles but
/// cannot represent ten of anything. [`Amount::MAX_PRECISION`] is the point past
/// which one major unit stops being representable at all, not a recommendation;
/// pick the scale your currency is *booked* in, which
/// [`Currency::minor_units`] will tell you for the codes it knows.
///
/// Overflow is never silent: every operation that can exceed the range returns
/// [`MoneyError::Overflow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount<const P: u8>(i64);

#[cfg(feature = "serde")]
impl<const P: u8> serde::Serialize for Amount<P> {
    /// Serialises as a decimal string such as `"1234.56"`.
    ///
    /// Never as a float, and never as the raw scaled integer: the integer is
    /// meaningless without knowing `P`, so a consumer reading it at the wrong
    /// scale would silently misread every amount by a factor of ten.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de, const P: u8> serde::Deserialize<'de> for Amount<P> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl<const P: u8> Amount<P> {
    /// Largest precision the `i64` representation can carry.
    ///
    /// `10^19` exceeds `i64::MAX`, so a scale above this cannot represent even
    /// one major unit. Scales approaching it are legal but increasingly useless
    /// — see the range table on [`Amount`].
    pub const MAX_PRECISION: u8 = 18;
    /// Zero.
    pub const ZERO: Self = Self::from_minor(0);
    /// The largest representable amount.
    pub const MAX: Self = Self::from_minor(i64::MAX);
    /// The smallest representable amount.
    pub const MIN: Self = Self::from_minor(i64::MIN);

    /// Rejects a scale the representation cannot carry.
    ///
    /// `10^19` exceeds `i64::MAX`, so a larger scale could not represent even one
    /// major unit. [`Amount::SCALE`] evaluates this, and every constructor and
    /// conversion goes through `SCALE`, so `Amount<19>` fails to compile rather
    /// than silently saturating and misreading every value by a factor of ten.
    const PRECISION_GUARD: () = assert!(
        P <= Self::MAX_PRECISION,
        "Amount<P> requires P <= 18; 10^19 exceeds i64::MAX"
    );

    /// The number of minor units in one major unit, as `10^P`.
    ///
    /// Evaluating this evaluates the precision guard, which is why every path
    /// that turns a number into an `Amount` reads it.
    pub const SCALE: i64 = {
        () = Self::PRECISION_GUARD;
        pow10(P)
    };

    /// Wraps a raw count of minor units at scale `P`.
    #[must_use]
    pub const fn from_minor(minor: i64) -> Self {
        () = Self::PRECISION_GUARD;
        Self(minor)
    }

    /// The raw count of minor units at scale `P`.
    #[must_use]
    pub const fn to_minor(self) -> i64 {
        self.0
    }

    /// Builds an amount from a whole number of major units.
    pub fn from_major(n: i64) -> Result<Self, MoneyError> {
        n.checked_mul(Self::SCALE)
            .map(Self)
            .ok_or(MoneyError::Overflow)
    }

    /// True when the amount is exactly zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// True when the amount is strictly negative.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// True when the amount is strictly positive.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// Adds two amounts, reporting overflow.
    pub fn checked_add(self, rhs: Self) -> Result<Self, MoneyError> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(MoneyError::Overflow)
    }

    /// Subtracts `rhs`, reporting overflow.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, MoneyError> {
        self.0
            .checked_sub(rhs.0)
            .map(Self)
            .ok_or(MoneyError::Overflow)
    }

    /// Negates, reporting overflow at [`Amount::MIN`].
    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        self.0.checked_neg().map(Self).ok_or(MoneyError::Overflow)
    }

    /// Absolute value, reporting overflow at [`Amount::MIN`].
    pub fn checked_abs(self) -> Result<Self, MoneyError> {
        self.0.checked_abs().map(Self).ok_or(MoneyError::Overflow)
    }

    /// Sums an iterator of amounts, reporting overflow.
    pub fn checked_sum(iter: impl IntoIterator<Item = Self>) -> Result<Self, MoneyError> {
        let mut acc = Self::ZERO;
        for a in iter {
            acc = acc.checked_add(a)?;
        }
        Ok(acc)
    }

    /// Multiplies by a whole number, reporting overflow.
    ///
    /// Exact: a quantity times a unit price lands on a minor unit by
    /// construction, so there is nothing to round and no rounding mode to pick.
    pub fn checked_mul_int(self, factor: i64) -> Result<Self, MoneyError> {
        self.0
            .checked_mul(factor)
            .map(Self)
            .ok_or(MoneyError::Overflow)
    }

    /// Applies the ratio `numerator / denominator`, rounding the remainder.
    ///
    /// The operation every tax, fee, interest and conversion calculation is:
    /// `net × 19 / 100`, `principal × days / 365`, `eur × rate / 10^k`. Doing it
    /// on [`to_minor`](Self::to_minor) by hand is the one place a caller of this
    /// crate would otherwise write unchecked arithmetic — the intermediate
    /// product overflows an `i64` long before either operand is unreasonable.
    ///
    /// Here it is exact in `i128` and only the *result* has to fit, so
    /// `Amount::<2>::MAX × 1 / 1_000_000` is an ordinary answer rather than an
    /// overflow.
    ///
    /// ```
    /// # use doubleentry::{Amount, Rounding};
    /// type Eur = Amount<2>;
    /// let net = Eur::parse("1000.00")?;
    ///
    /// // 19 % VAT.
    /// assert_eq!(net.checked_mul_ratio(19, 100, Rounding::HalfUp)?, Eur::parse("190.00")?);
    ///
    /// // The rounding mode is not decoration: 0.005 is a genuine tie.
    /// let odd = Eur::parse("0.01")?;
    /// assert_eq!(odd.checked_mul_ratio(1, 2, Rounding::HalfUp)?,   Eur::parse("0.01")?);
    /// assert_eq!(odd.checked_mul_ratio(1, 2, Rounding::HalfEven)?, Eur::parse("0.00")?);
    /// assert_eq!(odd.checked_mul_ratio(1, 2, Rounding::Floor)?,    Eur::parse("0.00")?);
    /// # Ok::<(), doubleentry::MoneyError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::DivideByZero`] for a zero denominator and
    /// [`MoneyError::Overflow`] when the rounded result does not fit.
    pub fn checked_mul_ratio(
        self,
        numerator: i64,
        denominator: i64,
        rounding: Rounding,
    ) -> Result<Self, MoneyError> {
        if denominator == 0 {
            return Err(MoneyError::DivideByZero);
        }
        let product = i128::from(self.0)
            .checked_mul(i128::from(numerator))
            .ok_or(MoneyError::Overflow)?;
        let scaled =
            div_rounded(product, i128::from(denominator), rounding).ok_or(MoneyError::Overflow)?;
        i64::try_from(scaled)
            .map(Self)
            .map_err(|_| MoneyError::Overflow)
    }

    /// Restates this amount at a different scale.
    ///
    /// Widening is exact. Narrowing drops precision the target cannot carry, so
    /// it takes a rounding mode — this is the one conversion in the crate that
    /// can lose information, and it says so in its signature.
    ///
    /// The pairing with [`checked_mul_ratio`](Self::checked_mul_ratio) is what
    /// makes a conversion at a published rate expressible: parse the rate at the
    /// precision it was published in, apply it, and restate the result in the
    /// scale the receiving books are kept in.
    ///
    /// ```
    /// # use doubleentry::{Amount, Rounding};
    /// // A rate quoted to six places, applied to euros booked to two.
    /// let eur = Amount::<2>::parse("1000.00")?;
    /// let rate = Amount::<6>::parse("1.087350")?;
    /// let usd: Amount<2> = eur
    ///     .rescale::<6>(Rounding::HalfEven)?
    ///     .checked_mul_ratio(rate.to_minor(), 1_000_000, Rounding::HalfEven)?
    ///     .rescale::<2>(Rounding::HalfEven)?;
    /// assert_eq!(usd, Amount::<2>::parse("1087.35")?);
    /// # Ok::<(), doubleentry::MoneyError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] when widening pushes the value out of
    /// range.
    pub fn rescale<const Q: u8>(self, rounding: Rounding) -> Result<Amount<Q>, MoneyError> {
        if Q >= P {
            let factor = pow10_checked(Q.saturating_sub(P)).ok_or(MoneyError::Overflow)?;
            self.0
                .checked_mul(factor)
                .map(Amount::<Q>::from_minor)
                .ok_or(MoneyError::Overflow)
        } else {
            let divisor = pow10_checked(P.saturating_sub(Q)).ok_or(MoneyError::Overflow)?;
            let scaled = div_rounded(i128::from(self.0), i128::from(divisor), rounding)
                .ok_or(MoneyError::Overflow)?;
            i64::try_from(scaled)
                .map(Amount::<Q>::from_minor)
                .map_err(|_| MoneyError::Overflow)
        }
    }

    /// Splits into `n` parts differing by at most one minor unit.
    ///
    /// The parts always re-sum to the original: no minor unit is created or lost.
    pub fn distribute(self, n: usize) -> Result<Vec<Self>, MoneyError> {
        if n == 0 {
            return Err(MoneyError::ZeroWeight);
        }
        self.allocate(&vec![1u64; n])
    }

    /// Splits proportionally to `weights` using the largest-remainder method.
    ///
    /// Leftover minor units go to the parts with the largest fractional
    /// remainder; ties are broken toward the lowest index, so the result is a
    /// deterministic function of the inputs. The parts always re-sum to the
    /// original exactly, which is the property that keeps proportional splits
    /// from leaking value.
    pub fn allocate(self, weights: &[u64]) -> Result<Vec<Self>, MoneyError> {
        if weights.is_empty() {
            return Err(MoneyError::ZeroWeight);
        }
        let mut total: u128 = 0;
        for w in weights {
            total = total
                .checked_add(u128::from(*w))
                .ok_or(MoneyError::Overflow)?;
        }
        if total == 0 {
            return Err(MoneyError::ZeroWeight);
        }

        // Work on the magnitude so that truncation always rounds toward zero in
        // the same direction regardless of sign, then restore the sign at the end.
        let negative = self.0 < 0;
        let magnitude = u128::from(self.0.unsigned_abs());

        let mut parts: Vec<u128> = Vec::with_capacity(weights.len());
        let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(weights.len());
        let mut assigned: u128 = 0;

        for (i, w) in weights.iter().enumerate() {
            let product = magnitude
                .checked_mul(u128::from(*w))
                .ok_or(MoneyError::Overflow)?;
            let share = product.checked_div(total).ok_or(MoneyError::ZeroWeight)?;
            let rem = product.checked_rem(total).ok_or(MoneyError::ZeroWeight)?;
            assigned = assigned.checked_add(share).ok_or(MoneyError::Overflow)?;
            parts.push(share);
            remainders.push((rem, i));
        }

        let mut leftover = magnitude
            .checked_sub(assigned)
            .ok_or(MoneyError::Overflow)?;

        // Largest remainder first; ties resolved by ascending index.
        remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, idx) in &remainders {
            if leftover == 0 {
                break;
            }
            if let Some(slot) = parts.get_mut(*idx) {
                *slot = slot.checked_add(1).ok_or(MoneyError::Overflow)?;
                leftover = leftover.checked_sub(1).ok_or(MoneyError::Overflow)?;
            }
        }

        parts
            .into_iter()
            .map(|p| {
                let v = i64::try_from(p).map_err(|_| MoneyError::Overflow)?;
                if negative {
                    v.checked_neg().map(Self).ok_or(MoneyError::Overflow)
                } else {
                    Ok(Self(v))
                }
            })
            .collect()
    }

    /// Parses a decimal literal such as `"-1234.56"` at scale `P`.
    ///
    /// Rejects inputs carrying more precision than `P` rather than rounding
    /// silently: at the point where an amount enters a ledger, a value that does
    /// not fit the booking scale is a defect upstream.
    pub fn parse(s: &str) -> Result<Self, MoneyError> {
        let s = s.trim();
        let (negative, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if digits.is_empty() {
            return Err(MoneyError::InvalidLiteral);
        }

        let (int_part, frac_part) = match digits.split_once('.') {
            Some((i, f)) => (i, f),
            None => (digits, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(MoneyError::InvalidLiteral);
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(MoneyError::InvalidLiteral);
        }

        let scale = usize::from(P);
        // Trailing zeros beyond the scale are not precision, so drop them first.
        let trimmed = frac_part.trim_end_matches('0');
        if trimmed.len() > scale {
            return Err(MoneyError::PrecisionLoss { scale: P });
        }

        let whole: i64 = if int_part.is_empty() {
            0
        } else {
            int_part.parse().map_err(|_| MoneyError::Overflow)?
        };

        let mut frac: i64 = 0;
        for i in 0..scale {
            let digit = frac_part
                .as_bytes()
                .get(i)
                .map_or(0, |b| i64::from(b.wrapping_sub(b'0')));
            frac = frac
                .checked_mul(10)
                .and_then(|f| f.checked_add(digit))
                .ok_or(MoneyError::Overflow)?;
        }

        let value = whole
            .checked_mul(Self::SCALE)
            .and_then(|w| w.checked_add(frac))
            .ok_or(MoneyError::Overflow)?;

        if negative {
            value.checked_neg().map(Self).ok_or(MoneyError::Overflow)
        } else {
            Ok(Self(value))
        }
    }
}

/// `10^exp`, or `None` when it exceeds `i64::MAX`.
const fn pow10_checked(exp: u8) -> Option<i64> {
    let mut acc: i64 = 1;
    let mut i = 0u8;
    while i < exp {
        match acc.checked_mul(10) {
            Some(v) => acc = v,
            None => return None,
        }
        i = i.wrapping_add(1);
    }
    Some(acc)
}

/// `10^exp`.
///
/// The clamping arm is unreachable for every scale that compiles: the precision
/// guard on [`Amount::SCALE`] rejects `P > 18` before this is evaluated. It
/// exists because a `const fn` may not panic here, and returning a wrong number
/// quietly is worse than returning a clamped one.
const fn pow10(exp: u8) -> i64 {
    match pow10_checked(exp) {
        Some(v) => v,
        None => i64::MAX,
    }
}

impl<const P: u8> fmt::Display for Amount<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = usize::from(P);
        if scale == 0 {
            return write!(f, "{}", self.0);
        }
        let negative = self.0 < 0;
        let magnitude = self.0.unsigned_abs();
        let divisor = Self::SCALE.unsigned_abs();
        let whole = magnitude.checked_div(divisor).unwrap_or(0);
        let frac = magnitude.checked_rem(divisor).unwrap_or(0);
        if negative {
            f.write_str("-")?;
        }
        write!(f, "{whole}.{frac:0>scale$}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Eur = Amount<2>;

    #[test]
    fn parses_and_displays_round_trip() {
        for s in ["0.00", "1.00", "-1.23", "1234.56", "0.07"] {
            let a = Eur::parse(s).expect("parses");
            assert_eq!(a.to_string(), s, "round trip for {s}");
        }
    }

    #[test]
    fn parse_accepts_shorter_fraction() {
        assert_eq!(Eur::parse("1.5").expect("parses"), Eur::from_minor(150));
        assert_eq!(Eur::parse("1").expect("parses"), Eur::from_minor(100));
    }

    #[test]
    fn parse_rejects_excess_precision() {
        assert_eq!(
            Eur::parse("1.234"),
            Err(MoneyError::PrecisionLoss { scale: 2 })
        );
    }

    #[test]
    fn parse_allows_insignificant_trailing_zeros() {
        assert_eq!(Eur::parse("1.2300").expect("parses"), Eur::from_minor(123));
    }

    #[test]
    fn parse_rejects_garbage() {
        for s in ["", "abc", "1.2.3", "-", "1,5"] {
            assert!(Eur::parse(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn arithmetic_reports_overflow_instead_of_panicking() {
        assert_eq!(
            Eur::MAX.checked_add(Eur::from_minor(1)),
            Err(MoneyError::Overflow)
        );
        assert_eq!(Eur::MIN.checked_neg(), Err(MoneyError::Overflow));
        assert_eq!(Eur::MIN.checked_abs(), Err(MoneyError::Overflow));
    }

    #[test]
    fn distribute_conserves_the_total() {
        let total = Eur::from_minor(100);
        let parts = total.distribute(3).expect("splits");
        assert_eq!(parts.len(), 3);
        assert_eq!(
            Eur::checked_sum(parts.iter().copied()).expect("sums"),
            total
        );
    }

    #[test]
    fn distribute_gives_leftover_to_leading_parts() {
        let parts = Eur::from_minor(100).distribute(3).expect("splits");
        assert_eq!(
            parts,
            vec![
                Eur::from_minor(34),
                Eur::from_minor(33),
                Eur::from_minor(33)
            ]
        );
    }

    #[test]
    fn allocate_uses_largest_remainder() {
        // 0.05 split 1:1:1 gives remainders that must land on the first two parts.
        let parts = Eur::from_minor(5).allocate(&[1, 1, 1]).expect("splits");
        assert_eq!(
            parts,
            vec![Eur::from_minor(2), Eur::from_minor(2), Eur::from_minor(1)]
        );
    }

    #[test]
    fn allocate_respects_weights() {
        let parts = Eur::from_minor(1000).allocate(&[1, 4]).expect("splits");
        assert_eq!(parts, vec![Eur::from_minor(200), Eur::from_minor(800)]);
    }

    #[test]
    fn allocate_conserves_negative_totals() {
        let total = Eur::from_minor(-100);
        let parts = total.allocate(&[1, 1, 1]).expect("splits");
        assert_eq!(
            Eur::checked_sum(parts.iter().copied()).expect("sums"),
            total
        );
    }

    #[test]
    fn allocate_rejects_zero_weight() {
        assert_eq!(
            Eur::from_minor(100).allocate(&[0, 0]),
            Err(MoneyError::ZeroWeight)
        );
        assert_eq!(
            Eur::from_minor(100).allocate(&[]),
            Err(MoneyError::ZeroWeight)
        );
        assert_eq!(
            Eur::from_minor(100).distribute(0),
            Err(MoneyError::ZeroWeight)
        );
    }

    #[test]
    fn multiplying_by_a_whole_number_is_exact() {
        assert_eq!(
            Eur::from_minor(199).checked_mul_int(3).expect("fits"),
            Eur::from_minor(597)
        );
        assert_eq!(Eur::MAX.checked_mul_int(2), Err(MoneyError::Overflow));
    }

    #[test]
    fn a_ratio_applies_the_requested_rounding() {
        // 0.005 is a genuine tie, and each mode resolves it differently.
        let one_cent = Eur::from_minor(1);
        let cases = [
            (Rounding::HalfUp, 1),
            (Rounding::HalfDown, 0),
            (Rounding::HalfEven, 0),
            (Rounding::TowardZero, 0),
            (Rounding::AwayFromZero, 1),
            (Rounding::Floor, 0),
            (Rounding::Ceiling, 1),
        ];
        for (mode, expected) in cases {
            assert_eq!(
                one_cent.checked_mul_ratio(1, 2, mode).expect("fits"),
                Eur::from_minor(expected),
                "{mode:?} on a positive tie"
            );
        }
    }

    #[test]
    fn rounding_is_symmetric_about_zero_where_it_should_be() {
        // Sign-symmetric modes must give mirrored answers; the directional ones
        // must not. Getting this backwards is the classic truncation bug.
        let minus = Eur::from_minor(-1);
        for mode in [
            Rounding::HalfUp,
            Rounding::HalfDown,
            Rounding::HalfEven,
            Rounding::TowardZero,
            Rounding::AwayFromZero,
        ] {
            let positive = Eur::from_minor(1)
                .checked_mul_ratio(1, 2, mode)
                .expect("fits");
            let negative = minus.checked_mul_ratio(1, 2, mode).expect("fits");
            assert_eq!(
                negative,
                positive.checked_neg().expect("fits"),
                "{mode:?} must not favour a sign"
            );
        }
        assert_eq!(
            minus
                .checked_mul_ratio(1, 2, Rounding::Floor)
                .expect("fits"),
            Eur::from_minor(-1),
        );
        assert_eq!(
            minus
                .checked_mul_ratio(1, 2, Rounding::Ceiling)
                .expect("fits"),
            Eur::ZERO,
        );
    }

    #[test]
    fn half_even_alternates_on_consecutive_ties() {
        // The property banker's rounding exists for: ties do not all go one way.
        // 0.5 → 0, 1.5 → 2, 2.5 → 2, 3.5 → 4, in whole minor units.
        let landed: Vec<i64> = (1..=7)
            .step_by(2)
            .map(|odd| {
                Amount::<0>::from_minor(odd)
                    .checked_mul_ratio(1, 2, Rounding::HalfEven)
                    .expect("fits")
                    .to_minor()
            })
            .collect();
        assert_eq!(landed, vec![0, 2, 2, 4]);
    }

    #[test]
    fn a_ratio_is_exact_in_the_intermediate() {
        // The whole point of the i128 intermediate: the product overflows an
        // i64 by a wide margin while the answer is unremarkable.
        assert_eq!(
            Eur::MAX
                .checked_mul_ratio(1_000_000, 1_000_000, Rounding::HalfUp)
                .expect("the result fits even though the product does not"),
            Eur::MAX
        );
    }

    #[test]
    fn a_ratio_reports_a_zero_denominator_and_an_unrepresentable_result() {
        assert_eq!(
            Eur::from_minor(1).checked_mul_ratio(1, 0, Rounding::HalfUp),
            Err(MoneyError::DivideByZero)
        );
        assert_eq!(
            Eur::MAX.checked_mul_ratio(2, 1, Rounding::HalfUp),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn rescaling_wider_is_exact_and_reversible() {
        let eur = Eur::parse("12.34").expect("parses");
        let wide = eur.rescale::<6>(Rounding::HalfEven).expect("widens");
        assert_eq!(wide, Amount::<6>::parse("12.340000").expect("parses"));
        assert_eq!(wide.rescale::<2>(Rounding::HalfEven).expect("narrows"), eur);
    }

    #[test]
    fn rescaling_narrower_rounds_and_can_overflow_widening() {
        let precise = Amount::<4>::parse("1.2367").expect("parses");
        assert_eq!(
            precise.rescale::<2>(Rounding::HalfUp).expect("narrows"),
            Eur::parse("1.24").expect("parses")
        );
        assert_eq!(
            precise.rescale::<2>(Rounding::TowardZero).expect("narrows"),
            Eur::parse("1.23").expect("parses")
        );
        // Widening spends range on precision, so it is the direction that fails.
        assert_eq!(
            Eur::MAX.rescale::<4>(Rounding::HalfUp),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn rescaling_to_the_same_scale_is_the_identity() {
        let eur = Eur::from_minor(-4321);
        assert_eq!(eur.rescale::<2>(Rounding::Floor).expect("no-op"), eur);
    }

    #[test]
    fn currency_validates_code() {
        assert_eq!(Currency::new("EUR").expect("valid"), Currency::EUR);
        assert!(Currency::new("eur").is_err());
        assert!(Currency::new("EU").is_err());
        assert!(Currency::new("EURO").is_err());
    }

    #[test]
    fn currency_knows_minor_units() {
        assert_eq!(Currency::EUR.minor_units(), Some(2));
        assert_eq!(Currency::JPY.minor_units(), Some(0));
        assert_eq!(Currency::new("XYZ").expect("valid").minor_units(), None);
    }

    #[test]
    fn zero_scale_displays_as_integer() {
        assert_eq!(Amount::<0>::from_minor(1234).to_string(), "1234");
    }

    #[test]
    fn the_range_shrinks_as_the_scale_grows() {
        // The documented table, checked. Raising the scale spends `i64` range on
        // precision, so a high scale is legal and nearly empty — which is worth
        // pinning, because it is the opposite of what "more precision" suggests.
        assert!(Amount::<2>::from_major(92_000_000_000_000_000).is_ok());
        assert!(Amount::<18>::from_major(9).is_ok());
        assert_eq!(Amount::<18>::from_major(10), Err(MoneyError::Overflow));
    }
}
