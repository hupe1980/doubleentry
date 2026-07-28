//! Posting dimensions.
//!
//! Dimensions are typed tags carried by every posting, orthogonal to the account
//! path. The account path carries the general-ledger structure; dimensions carry
//! the axes reporting needs to slice by.
//!
//! Encoding every axis into the path instead — `Grid:Electricity:HV:Revenue` —
//! multiplies the account count by the product of the axes and freezes the
//! reporting dimensions at design time. Keeping them separate lets a trial
//! balance group by any axis without restructuring the tree.
//!
//! The engine treats dimension values as opaque. It stores them, folds them into
//! the entry hash so they are covered by tamper evidence, and groups by them on
//! request. It never interprets them, and it ships no vocabulary.

use crate::canonical::{Canonical, CanonicalWriter};
use crate::serde_support::validating_string_serde;

/// Maximum length of a dimension value.
pub const MAX_LABEL_LEN: usize = 64;

/// Failure constructing a dimension value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DimensionError {
    /// The value was empty.
    #[error("dimension value is empty")]
    Empty,
    /// The value exceeded [`MAX_LABEL_LEN`].
    #[error("dimension value exceeds {MAX_LABEL_LEN} characters")]
    TooLong,
    /// The value contained a control character.
    #[error("dimension value contains a control character")]
    ControlCharacter,
}

/// A validated, bounded dimension value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(String);

validating_string_serde!(Label);

impl Label {
    /// Validates and wraps a value.
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

impl Canonical for Label {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.str(&self.0);
    }
}

macro_rules! dimension {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Label);

        validating_string_serde!($name);

        impl $name {
            /// Validates and wraps a value.
            pub fn new(s: impl Into<String>) -> Result<Self, DimensionError> {
                Label::new(s).map(Self)
            }

            /// The underlying value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl Canonical for $name {
            fn encode(&self, w: &mut CanonicalWriter) {
                self.0.encode(w);
            }
        }
    };
}

dimension! {
    /// The line of business a posting belongs to.
    ///
    /// Where a regulator requires accounts to be kept per activity as though each
    /// were carried out by a separate company, this is the axis that separates
    /// them, and a ledger may be configured to require it on every posting.
    ActivityId
}

dimension! {
    /// A product or commodity division.
    SegmentId
}

dimension! {
    /// A cost centre, project, or other internal allocation target.
    CostObjectId
}

dimension! {
    /// The counterparty on the other side of the posting.
    PartyId
}

/// The dimension tuple attached to a posting.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Dimensions {
    /// Line of business.
    pub activity: Option<ActivityId>,
    /// Product or commodity division.
    pub segment: Option<SegmentId>,
    /// Cost centre, project, or allocation target.
    pub cost_object: Option<CostObjectId>,
    /// Counterparty.
    pub party: Option<PartyId>,
}

impl Dimensions {
    /// An empty dimension tuple.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Sets the activity.
    #[must_use]
    pub fn with_activity(mut self, v: ActivityId) -> Self {
        self.activity = Some(v);
        self
    }

    /// Sets the segment.
    #[must_use]
    pub fn with_segment(mut self, v: SegmentId) -> Self {
        self.segment = Some(v);
        self
    }

    /// Sets the cost object.
    #[must_use]
    pub fn with_cost_object(mut self, v: CostObjectId) -> Self {
        self.cost_object = Some(v);
        self
    }

    /// Sets the counterparty.
    #[must_use]
    pub fn with_party(mut self, v: PartyId) -> Self {
        self.party = Some(v);
        self
    }

    /// True when no dimension is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.activity.is_none()
            && self.segment.is_none()
            && self.cost_object.is_none()
            && self.party.is_none()
    }
}

impl Canonical for Dimensions {
    fn encode(&self, w: &mut CanonicalWriter) {
        // Fixed field order: the encoding must not depend on which fields are set.
        w.option(self.activity.as_ref(), |w, v| v.encode(w));
        w.option(self.segment.as_ref(), |w, v| v.encode(w));
        w.option(self.cost_object.as_ref(), |w, v| v.encode(w));
        w.option(self.party.as_ref(), |w, v| v.encode(w));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_values() {
        assert!(ActivityId::new("Network").is_ok());
        assert_eq!(ActivityId::new(""), Err(DimensionError::Empty));
        assert_eq!(
            ActivityId::new("x".repeat(MAX_LABEL_LEN + 1)),
            Err(DimensionError::TooLong)
        );
        assert_eq!(
            ActivityId::new("bad\nvalue"),
            Err(DimensionError::ControlCharacter)
        );
    }

    #[test]
    fn accepts_maximum_length() {
        assert!(ActivityId::new("x".repeat(MAX_LABEL_LEN)).is_ok());
    }

    #[test]
    fn empty_dimensions_report_empty() {
        assert!(Dimensions::none().is_empty());
        let d = Dimensions::none().with_activity(ActivityId::new("N").expect("valid"));
        assert!(!d.is_empty());
    }

    #[test]
    fn encoding_distinguishes_which_field_is_set() {
        let by_activity = Dimensions::none().with_activity(ActivityId::new("X").expect("valid"));
        let by_segment = Dimensions::none().with_segment(SegmentId::new("X").expect("valid"));
        assert_ne!(
            by_activity.to_canonical_bytes(),
            by_segment.to_canonical_bytes()
        );
    }

    #[test]
    fn encoding_is_order_independent_and_stable() {
        let a = Dimensions::none()
            .with_activity(ActivityId::new("N").expect("valid"))
            .with_segment(SegmentId::new("E").expect("valid"));
        let b = Dimensions::none()
            .with_segment(SegmentId::new("E").expect("valid"))
            .with_activity(ActivityId::new("N").expect("valid"));
        assert_eq!(a.to_canonical_bytes(), b.to_canonical_bytes());
    }

    #[test]
    fn different_dimension_types_do_not_collide() {
        let a = ActivityId::new("X").expect("valid");
        let s = SegmentId::new("X").expect("valid");
        // Same inner bytes; separation comes from field position, not the value.
        assert_eq!(a.to_canonical_bytes(), s.to_canonical_bytes());
        let da = Dimensions::none().with_activity(a);
        let ds = Dimensions::none().with_segment(s);
        assert_ne!(da.to_canonical_bytes(), ds.to_canonical_bytes());
    }
}
