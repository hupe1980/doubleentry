//! Posting dimensions.
//!
//! Dimensions are the axes reporting slices by, carried on every posting and
//! orthogonal to the account path. The account path carries the general-ledger
//! structure; dimensions carry everything else you need to group by.
//!
//! Encoding an axis into the path instead — `Grid:Electricity:HV:Revenue` —
//! multiplies the account count by the product of the axes and freezes the
//! reporting dimensions at design time. Keeping them separate lets a trial
//! balance group by any axis without restructuring the tree.
//!
//! # The axes are yours
//!
//! A [`Dimensions`] value is a small, ordered map from axis name to value, both
//! [`Label`]s. The engine ships no axis names, because there is no set that is
//! right for everyone: an energy utility separates by regulated activity, a fund
//! administrator by mandate, a marketplace by counterparty. Naming four of them
//! in the library would be a chart of accounts by another route — and would then
//! have to be worked around by everyone whose fifth axis matters.
//!
//! What the engine does is bound them ([`MAX_DIMENSIONS`] axes, [`MAX_LABEL_LEN`]
//! characters each), order them deterministically, fold them into the entry hash
//! so they are covered by tamper evidence, and — through
//! [`LedgerPolicy::required_dimensions`](crate::entry::LedgerPolicy::required_dimensions)
//! — let you insist that a posting carry the ones your books cannot be kept
//! without. It never interprets a value.
//!
//! ```
//! use doubleentry::{Dimensions, Label};
//!
//! let dims = Dimensions::none()
//!     .with(Label::new("activity")?, Label::new("Network")?)?
//!     .with(Label::new("segment")?, Label::new("Electricity")?)?;
//!
//! assert_eq!(dims.get("activity").map(Label::as_str), Some("Network"));
//! assert_eq!(dims.len(), 2);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;

use crate::canonical::{Canonical, CanonicalWriter};
use crate::serde_support::validating_string_serde;

/// Maximum length of an axis name or a dimension value, in characters.
pub const MAX_LABEL_LEN: usize = 64;

/// Maximum number of axes one posting may carry.
///
/// A bound rather than a limit anyone should reach. Dimensions are hashed into
/// every entry and written on every posting row, so an unbounded map would make
/// the size of a ledger a function of what a caller happened to attach; eight
/// independent reporting axes is already past what a chart of accounts can be
/// meaningfully sliced by.
pub const MAX_DIMENSIONS: usize = 8;

/// Failure constructing a label or a dimension set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DimensionError {
    /// The value was empty.
    #[error("label is empty")]
    Empty,
    /// The value exceeded [`MAX_LABEL_LEN`].
    #[error("label exceeds {MAX_LABEL_LEN} characters")]
    TooLong,
    /// The value contained a control character.
    #[error("label contains a control character")]
    ControlCharacter,
    /// More than [`MAX_DIMENSIONS`] axes were attached to one posting.
    #[error("a posting may carry at most {MAX_DIMENSIONS} dimensions")]
    TooManyDimensions,
}

/// A validated, bounded string: an axis name, a dimension value, an identifier.
///
/// The same type is used for every short opaque string in the engine — a
/// dimension axis, a dimension value, an entry kind, a period identifier, a
/// provenance actor — because they have the same rules and inventing a newtype
/// per position would give type safety over values that are, by design,
/// interchangeable opaque text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(String);

validating_string_serde!(Label);

impl Label {
    /// Validates and wraps a value.
    ///
    /// # Errors
    ///
    /// Rejects the empty string, anything longer than [`MAX_LABEL_LEN`]
    /// characters, and any control character. Control characters are refused
    /// because a label ends up in log lines, CSV exports and error messages, and
    /// an embedded newline or terminal escape turns one field into two.
    pub fn new(s: impl Into<String>) -> Result<Self, DimensionError> {
        let s = s.into();
        if s.is_empty() {
            return Err(DimensionError::Empty);
        }
        if s.chars().count() > MAX_LABEL_LEN {
            return Err(DimensionError::TooLong);
        }
        if s.chars().any(char::is_control) {
            return Err(DimensionError::ControlCharacter);
        }
        Ok(Self(s))
    }

    /// The underlying value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for Label {
    type Err = DimensionError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Lets a `Label`-keyed map be looked up by `&str` without allocating.
///
/// Sound because `Label`'s `Ord` and `Hash` are the inner `String`'s, which are
/// `str`'s — the borrowed and owned forms compare and hash identically, which is
/// what `Borrow` requires.
impl std::borrow::Borrow<str> for Label {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl Canonical for Label {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.str(&self.0);
    }
}

/// The axes attached to one posting.
///
/// Ordered by axis name, so the canonical encoding — and therefore the entry
/// hash — does not depend on the order the axes were set.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dimensions {
    axes: BTreeMap<Label, Label>,
}

impl Dimensions {
    /// No dimensions.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Attaches one axis, replacing any value already set for it.
    ///
    /// # Errors
    ///
    /// Returns [`DimensionError::TooManyDimensions`] when this would push the
    /// set past [`MAX_DIMENSIONS`]. Overwriting an axis already present never
    /// does.
    pub fn with(mut self, axis: Label, value: Label) -> Result<Self, DimensionError> {
        self.set(axis, value)?;
        Ok(self)
    }

    /// Attaches one axis in place, returning the value it replaced.
    ///
    /// # Errors
    ///
    /// Returns [`DimensionError::TooManyDimensions`] when this would push the
    /// set past [`MAX_DIMENSIONS`].
    pub fn set(&mut self, axis: Label, value: Label) -> Result<Option<Label>, DimensionError> {
        if !self.axes.contains_key(&axis) && self.axes.len() >= MAX_DIMENSIONS {
            return Err(DimensionError::TooManyDimensions);
        }
        Ok(self.axes.insert(axis, value))
    }

    /// Builds a dimension set from axis/value pairs.
    ///
    /// Later pairs win over earlier ones for the same axis.
    ///
    /// # Errors
    ///
    /// Returns [`DimensionError::TooManyDimensions`] when the pairs name more
    /// than [`MAX_DIMENSIONS`] distinct axes.
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (Label, Label)>,
    ) -> Result<Self, DimensionError> {
        let mut out = Self::none();
        for (axis, value) in pairs {
            out.set(axis, value)?;
        }
        Ok(out)
    }

    /// The value on `axis`, if it is set.
    #[must_use]
    pub fn get(&self, axis: &str) -> Option<&Label> {
        self.axes.get(axis)
    }

    /// True when `axis` is set.
    #[must_use]
    pub fn contains(&self, axis: &str) -> bool {
        self.get(axis).is_some()
    }

    /// Every axis and value, in axis order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Label, &Label)> {
        self.axes.iter()
    }

    /// Number of axes set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.axes.len()
    }

    /// True when no axis is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.axes.is_empty()
    }
}

impl<'a> IntoIterator for &'a Dimensions {
    type Item = (&'a Label, &'a Label);
    type IntoIter = std::collections::btree_map::Iter<'a, Label, Label>;
    fn into_iter(self) -> Self::IntoIter {
        self.axes.iter()
    }
}

impl Canonical for Dimensions {
    fn encode(&self, w: &mut CanonicalWriter) {
        // Counted, and in axis order: the encoding must be a function of the set,
        // not of the order the caller happened to build it in.
        w.seq(self.axes.iter(), |w, (axis, value)| {
            axis.encode(w);
            value.encode(w);
        });
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Dimensions {
    /// Serialised as a plain object, `{"activity": "Network"}`.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.axes, s)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Dimensions {
    /// Re-runs the cardinality check: a value read off a wire must satisfy the
    /// same bound as one that was constructed, or the bound guarantees nothing.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let axes = <BTreeMap<Label, Label> as serde::Deserialize>::deserialize(d)?;
        Self::from_pairs(axes).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> Label {
        Label::new(s).expect("valid label")
    }

    #[test]
    fn validates_labels() {
        assert!(Label::new("Network").is_ok());
        assert_eq!(Label::new(""), Err(DimensionError::Empty));
        assert_eq!(
            Label::new("x".repeat(MAX_LABEL_LEN + 1)),
            Err(DimensionError::TooLong)
        );
        assert_eq!(
            Label::new("bad\nvalue"),
            Err(DimensionError::ControlCharacter)
        );
        assert!(Label::new("x".repeat(MAX_LABEL_LEN)).is_ok());
    }

    #[test]
    fn empty_dimensions_report_empty() {
        assert!(Dimensions::none().is_empty());
        let d = Dimensions::none()
            .with(label("activity"), label("Network"))
            .expect("fits");
        assert!(!d.is_empty());
        assert_eq!(d.len(), 1);
        assert!(d.contains("activity"));
        assert!(!d.contains("segment"));
    }

    #[test]
    fn setting_an_axis_twice_replaces_it() {
        let d = Dimensions::none()
            .with(label("activity"), label("Network"))
            .expect("fits")
            .with(label("activity"), label("Supply"))
            .expect("fits");
        assert_eq!(d.len(), 1);
        assert_eq!(d.get("activity").map(Label::as_str), Some("Supply"));
    }

    #[test]
    fn the_axis_count_is_bounded() {
        let mut d = Dimensions::none();
        for i in 0..MAX_DIMENSIONS {
            d = d
                .with(label(&format!("axis{i}")), label("v"))
                .expect("fits");
        }
        assert_eq!(
            d.clone().with(label("one-too-many"), label("v")),
            Err(DimensionError::TooManyDimensions)
        );
        // Replacing an existing axis at the bound is still fine.
        assert!(d.with(label("axis0"), label("w")).is_ok());
    }

    #[test]
    fn encoding_is_order_independent_and_stable() {
        let a = Dimensions::none()
            .with(label("activity"), label("N"))
            .expect("fits")
            .with(label("segment"), label("E"))
            .expect("fits");
        let b = Dimensions::none()
            .with(label("segment"), label("E"))
            .expect("fits")
            .with(label("activity"), label("N"))
            .expect("fits");
        assert_eq!(a.to_canonical_bytes(), b.to_canonical_bytes());
    }

    #[test]
    fn encoding_distinguishes_the_axis_from_the_value() {
        // Same strings, swapped roles: the encoding must tell them apart.
        let forward = Dimensions::none()
            .with(label("a"), label("b"))
            .expect("fits");
        let backward = Dimensions::none()
            .with(label("b"), label("a"))
            .expect("fits");
        assert_ne!(forward.to_canonical_bytes(), backward.to_canonical_bytes());
    }

    #[test]
    fn encoding_distinguishes_a_split_axis_name() {
        // Length prefixes: ("ab","c") must not encode as ("a","bc").
        let one = Dimensions::none()
            .with(label("ab"), label("c"))
            .expect("fits");
        let other = Dimensions::none()
            .with(label("a"), label("bc"))
            .expect("fits");
        assert_ne!(one.to_canonical_bytes(), other.to_canonical_bytes());
    }

    #[test]
    fn an_absent_axis_is_not_an_empty_value() {
        let absent = Dimensions::none();
        let present = Dimensions::none()
            .with(label("activity"), label("-"))
            .expect("fits");
        assert_ne!(absent.to_canonical_bytes(), present.to_canonical_bytes());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trips_and_re_validates() {
        let d = Dimensions::none()
            .with(label("activity"), label("Network"))
            .expect("fits");
        let json = serde_json::to_string(&d).expect("serialises");
        assert_eq!(json, r#"{"activity":"Network"}"#);
        assert_eq!(
            serde_json::from_str::<Dimensions>(&json).expect("deserialises"),
            d
        );

        // Bounds are enforced on the way in, not merely on the way out.
        let overflowing: String = format!(
            "{{{}}}",
            (0..=MAX_DIMENSIONS)
                .map(|i| format!("\"axis{i}\":\"v\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(serde_json::from_str::<Dimensions>(&overflowing).is_err());
        assert!(serde_json::from_str::<Dimensions>(r#"{"":"v"}"#).is_err());
    }
}
