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
/// The value is stored as an `i64` count of minor units at scale `P`. At `P = 2`
/// the representable range spans roughly ±9.2 × 10¹⁶ minor units, which is ample
/// for both individual postings and cumulative balances.
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
    /// one major unit.
    pub const MAX_PRECISION: u8 = 18;
    /// Zero.
    pub const ZERO: Self = Self(0);
    /// The largest representable amount.
    pub const MAX: Self = Self(i64::MAX);
    /// The smallest representable amount.
    pub const MIN: Self = Self(i64::MIN);

    /// The number of minor units in one major unit, as `10^P`.
    pub const SCALE: i64 = pow10(P);

    /// Rejects a scale the representation cannot carry.
    ///
    /// Referenced by every constructor, so `Amount<19>` fails to compile rather
    /// than saturating `SCALE` and corrupting every conversion.
    const PRECISION_GUARD: () = assert!(
        P <= Self::MAX_PRECISION,
        "Amount<P> requires P <= 18; 10^19 exceeds i64::MAX"
    );

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

/// `10^exp`, saturating at `i64::MAX`.
const fn pow10(exp: u8) -> i64 {
    let mut acc: i64 = 1;
    let mut i = 0u8;
    while i < exp {
        match acc.checked_mul(10) {
            Some(v) => acc = v,
            None => return i64::MAX,
        }
        i = i.wrapping_add(1);
    }
    acc
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
}
