//! [`Label`]: short validated names.

use core::fmt;
use core::str::FromStr;

use super::{Decode, DecodeError, Encode, Reader, Sink, ValueError, Writer, normalize_label};

/// A short name: 1–64 Unicode scalar values in NFC, no whitespace at either
/// edge, no control character and no invisible character
/// ([`ValueError::InvisibleCharacter`]).
///
/// Labels name dimension axes and values, entry kinds, actors, document
/// types, account classes and period ids. Construction normalises to NFC, so
/// two spellings of "Café" are one label; edge whitespace is refused, not
/// trimmed.
///
/// ```
/// use doubleentry::Label;
///
/// assert_eq!(Label::new("Cafe\u{301}")?, Label::new("Caf\u{e9}")?);
/// assert!(Label::new(" padded").is_err());
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(String);

/// A period's id (`"2026-03"`, `"FY2026"`): a [`Label`].
pub type PeriodId = Label;

impl Label {
    /// The most Unicode scalar values in a label.
    pub const MAX_CHARS: usize = 64;

    /// Validates and NFC-normalises a label.
    ///
    /// # Errors
    ///
    /// [`ValueError::Empty`], [`ValueError::TooLong`] above
    /// [`MAX_CHARS`](Self::MAX_CHARS), [`ValueError::EdgeWhitespace`],
    /// [`ValueError::ControlCharacter`] or [`ValueError::InvisibleCharacter`].
    pub fn new(text: impl AsRef<str>) -> Result<Self, ValueError> {
        normalize_label(text.as_ref(), Self::MAX_CHARS).map(Self)
    }

    /// The label as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        super::encoding::pad(f, &self.0)
    }
}

impl fmt::Debug for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Label({:?})", self.0)
    }
}

impl FromStr for Label {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl TryFrom<&str> for Label {
    type Error = ValueError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl TryFrom<String> for Label {
    type Error = ValueError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl AsRef<str> for Label {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Encode for Label {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.str(&self.0);
    }
}

impl Decode for Label {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        Self::new(r.nfc_str(field)?).map_err(|error| DecodeError::Value { field, error })
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Label {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Label {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::serde_str::deserialize(deserializer)
    }
}
