//! [`AccountPath`]: the account tree's names.

use core::fmt;
use core::str::FromStr;

use super::{Decode, DecodeError, Encode, Label, Reader, Sink, ValueError, Writer};

/// An account's place in the account tree: 1–16 `:`-separated segments.
///
/// Each segment is a [`Label`] without `:`, so a path prints one way and
/// parses back to itself. Ordering is segment-wise, so an account's
/// descendants sort directly after it.
///
/// ```
/// use doubleentry::AccountPath;
///
/// let cash: AccountPath = "Assets:Cash".parse()?;
/// let petty = cash.child("Petty")?;
/// assert!(cash.is_ancestor_of(&petty));
/// assert_eq!(petty.to_string(), "Assets:Cash:Petty");
/// assert!(cash < petty && petty < "Assets:Cash2".parse()?);
/// assert!(AccountPath::from_segments(["Assets:Cash"]).is_err());
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountPath {
    segments: Vec<Label>,
}

impl AccountPath {
    /// The most segments in a path.
    pub const MAX_DEPTH: usize = 16;
    /// The segment separator.
    pub const SEPARATOR: char = ':';

    /// Parses `Segment:Segment:…`.
    ///
    /// # Errors
    ///
    /// [`ValueError::Empty`] for an empty path or segment,
    /// [`ValueError::TooDeep`] above [`MAX_DEPTH`](Self::MAX_DEPTH) segments,
    /// or a segment's [`Label`] error.
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        Self::from_segments(text.split(Self::SEPARATOR))
    }

    /// A path from its segments.
    ///
    /// # Errors
    ///
    /// [`ValueError::SeparatorInSegment`] for a segment containing `:`, and
    /// the errors of [`parse`](Self::parse).
    pub fn from_segments<I, S>(segments: I) -> Result<Self, ValueError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut labels = Vec::new();
        for segment in segments {
            if labels.len() == Self::MAX_DEPTH {
                return Err(ValueError::TooDeep {
                    max: Self::MAX_DEPTH,
                });
            }
            labels.push(segment_label(segment.as_ref())?);
        }
        if labels.is_empty() {
            return Err(ValueError::Empty);
        }
        Ok(Self { segments: labels })
    }

    /// The segments, root first.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = &str> {
        self.segments.iter().map(Label::as_str)
    }

    /// The number of segments.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    /// The parent path; `None` for a top-level account.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        match self.segments.split_last() {
            Some((_, parent)) if !parent.is_empty() => Some(Self {
                segments: parent.to_vec(),
            }),
            _ => None,
        }
    }

    /// The path one level below this one.
    ///
    /// # Errors
    ///
    /// As [`from_segments`](Self::from_segments) for the new segment and depth.
    pub fn child(&self, segment: &str) -> Result<Self, ValueError> {
        if self.segments.len() >= Self::MAX_DEPTH {
            return Err(ValueError::TooDeep {
                max: Self::MAX_DEPTH,
            });
        }
        let mut segments = self.segments.clone();
        segments.push(segment_label(segment)?);
        Ok(Self { segments })
    }

    /// Whether `other` lies strictly below this path.
    #[must_use]
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        other.segments.len() > self.segments.len() && other.segments.starts_with(&self.segments)
    }

    /// The joined text's byte length.
    fn text_len(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.as_str().len())
            .fold(self.segments.len().saturating_sub(1), usize::saturating_add)
    }
}

fn segment_label(segment: &str) -> Result<Label, ValueError> {
    if segment.contains(AccountPath::SEPARATOR) {
        return Err(ValueError::SeparatorInSegment);
    }
    Label::new(segment)
}

impl fmt::Display for AccountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = String::with_capacity(self.text_len());
        for (index, segment) in self.segments.iter().enumerate() {
            if index > 0 {
                text.push(Self::SEPARATOR);
            }
            text.push_str(segment.as_str());
        }
        super::encoding::pad(f, &text)
    }
}

impl fmt::Debug for AccountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountPath({:?})", self.to_string())
    }
}

impl FromStr for AccountPath {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl TryFrom<&str> for AccountPath {
    type Error = ValueError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::parse(text)
    }
}

impl TryFrom<String> for AccountPath {
    type Error = ValueError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl Encode for AccountPath {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.len(self.text_len());
        for (index, segment) in self.segments.iter().enumerate() {
            if index > 0 {
                w.u8(b':');
            }
            w.fixed(segment.as_str().as_bytes());
        }
    }
}

impl Decode for AccountPath {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        Self::parse(r.nfc_str(field)?).map_err(|error| DecodeError::Value { field, error })
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for AccountPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for AccountPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::serde_str::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_encoding_is_the_joined_text() {
        let path = AccountPath::parse("Assets:Cash").unwrap();
        assert_eq!(path.encoded_len(), 8 + 11);
        assert_eq!(path.to_bytes()[8..], *b"Assets:Cash");
    }
}
