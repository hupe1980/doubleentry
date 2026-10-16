//! [`Currency`] codes: registry-free, validated, `Copy`.

use core::fmt;
use core::str::FromStr;

use super::{Decode, DecodeError, Encode, Reader, Sink, ValueError, Writer};

/// The most characters in a currency code.
const MAX_LEN: usize = 16;

/// A currency code: 1–16 characters, each `A–Z` or `0–9`.
///
/// The code carries no exponent, name or registry entry: a ledger declares
/// each currency it uses together with its exponent. `Ord` is by text. No
/// case folding: `"eur"` is refused.
///
/// ```
/// use doubleentry::{Currency, currency, currency::EUR};
///
/// assert_eq!(Currency::new("EUR")?, EUR);
/// assert_eq!(currency!("USDC").as_str(), "USDC");
/// assert!("eur".parse::<Currency>().is_err());
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Currency {
    // `bytes` first: with the unused tail zeroed and every code byte above
    // zero, the derived order is the order of the text.
    bytes: [u8; MAX_LEN],
    len: u8,
}

impl Currency {
    /// Validates a currency code.
    ///
    /// # Errors
    ///
    /// [`ValueError::Empty`], [`ValueError::TooLong`] above 16 characters, or
    /// [`ValueError::InvalidCharacter`] for anything but `A–Z` and `0–9`.
    pub const fn new(code: &str) -> Result<Self, ValueError> {
        let mut bytes = [0u8; MAX_LEN];
        let mut free: &mut [u8] = &mut bytes;
        let mut rest = code.as_bytes();
        let mut len: u8 = 0;
        if rest.is_empty() {
            return Err(ValueError::Empty);
        }
        while let [byte, tail @ ..] = rest {
            if !matches!(byte, b'A'..=b'Z' | b'0'..=b'9') {
                return Err(ValueError::InvalidCharacter {
                    found: first_char(rest),
                });
            }
            let Some((slot, unused)) = free.split_first_mut() else {
                return Err(ValueError::TooLong {
                    max: MAX_LEN,
                    found: code.len(),
                });
            };
            *slot = *byte;
            free = unused;
            len = len.saturating_add(1);
            rest = tail;
        }
        Ok(Self { bytes, len })
    }

    /// The code as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        let (code, _) = self.bytes.split_at(usize::from(self.len).min(MAX_LEN));
        // Every byte of a code is ASCII by construction.
        core::str::from_utf8(code).unwrap_or_default()
    }

    /// An ISO 4217 constant from its three-letter name (every constant is
    /// checked against [`Currency::new`] by a test).
    const fn iso(code: &str) -> Self {
        match code.as_bytes() {
            [a, b, c] => Self {
                bytes: [*a, *b, *c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                len: 3,
            },
            _ => Self {
                bytes: [0; MAX_LEN],
                len: 0,
            },
        }
    }
}

/// The first character of `bytes` (valid UTF-8 from a `&str`), for errors.
const fn first_char(bytes: &[u8]) -> char {
    match bytes {
        [a, ..] if a.is_ascii() => *a as char,
        _ => char::REPLACEMENT_CHARACTER,
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        super::encoding::pad(f, self.as_str())
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Currency({})", self.as_str())
    }
}

impl FromStr for Currency {
    type Err = ValueError;

    fn from_str(code: &str) -> Result<Self, Self::Err> {
        Self::new(code)
    }
}

impl TryFrom<&str> for Currency {
    type Error = ValueError;

    fn try_from(code: &str) -> Result<Self, Self::Error> {
        Self::new(code)
    }
}

impl Encode for Currency {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.str(self.as_str());
    }
}

impl Decode for Currency {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        Self::new(r.str(field)?).map_err(|error| DecodeError::Value { field, error })
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Currency {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::serde_str::deserialize(deserializer)
    }
}

/// A compile-checked [`Currency`] code.
///
/// ```
/// use doubleentry::{Currency, currency};
///
/// const BTC: Currency = currency!("BTC");
/// assert_eq!(BTC.as_str(), "BTC");
/// ```
///
/// Lowercase is refused at compile time:
///
/// ```compile_fail
/// let _ = doubleentry::currency!("eur");
/// ```
#[macro_export]
macro_rules! currency {
    ($literal:literal) => {
        const {
            // Total at run time: this panic can only fire while the inline
            // `const` block is evaluated, i.e. as a compile error.
            #[allow(clippy::panic)]
            let currency = match $crate::Currency::new($literal) {
                ::core::result::Result::Ok(currency) => currency,
                ::core::result::Result::Err(_) => ::core::panic!(::core::concat!(
                    "currency!(",
                    ::core::stringify!($literal),
                    "): expected 1-16 characters A-Z or 0-9"
                )),
            };
            currency
        }
    };
}

macro_rules! iso_currencies {
    ($($code:ident: $name:literal),* $(,)?) => {
        $(
            #[doc = concat!("ISO 4217 `", stringify!($code), "` (", $name, ").")]
            pub const $code: Currency = Currency::iso(stringify!($code));
        )*
        #[cfg(test)]
        const ISO: &[Currency] = &[$($code),*];
    };
}

iso_currencies! {
    AUD: "Australian dollar", BHD: "Bahraini dinar", BRL: "Brazilian real",
    CAD: "Canadian dollar", CHF: "Swiss franc", CNY: "Chinese yuan",
    CZK: "Czech koruna", DKK: "Danish krone", EUR: "euro", GBP: "pound sterling",
    HKD: "Hong Kong dollar", HUF: "Hungarian forint", INR: "Indian rupee",
    JPY: "Japanese yen", KRW: "South Korean won", KWD: "Kuwaiti dinar",
    MXN: "Mexican peso", NOK: "Norwegian krone", NZD: "New Zealand dollar",
    PLN: "Polish złoty", SEK: "Swedish krona", SGD: "Singapore dollar",
    TRY: "Turkish lira", USD: "US dollar", ZAR: "South African rand",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_constant_is_a_valid_code_named_like_itself() {
        assert_eq!(ISO.len(), 25);
        for code in ISO {
            assert_eq!(Currency::new(code.as_str()), Ok(*code));
            assert_eq!(code.as_str().len(), 3);
        }
        assert_eq!(EUR.as_str(), "EUR");
        assert_eq!(ZAR.as_str(), "ZAR");
    }
}
