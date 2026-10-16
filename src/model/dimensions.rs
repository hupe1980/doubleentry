//! [`Dimensions`]: the caller-named axes a posting is sliced by.

use std::collections::BTreeMap;

use super::{Decode, DecodeError, Encode, Label, Reader, Sink, ValueError, Writer};

/// At most eight `axis → value` labels on a posting, in ascending axis order.
///
/// The crate names no axes: cost centre, project or region are the caller's.
///
/// ```
/// use doubleentry::{Dimensions, Label};
///
/// let dims = Dimensions::new()
///     .with(Label::new("project")?, Label::new("alpha")?)?
///     .with(Label::new("cost-center")?, Label::new("100")?)?;
/// let axes: Vec<&str> = dims.iter().map(|(axis, _)| axis.as_str()).collect();
/// assert_eq!(axes, ["cost-center", "project"]);
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Dimensions(BTreeMap<Label, Label>);

impl Dimensions {
    /// The most axes on one posting.
    pub const MAX_AXES: usize = 8;

    /// No dimensions.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Adds a value for a new axis.
    ///
    /// # Errors
    ///
    /// [`ValueError::DuplicateAxis`] when the axis already has a value,
    /// [`ValueError::TooManyDimensions`] for a ninth axis.
    pub fn with(mut self, axis: Label, value: Label) -> Result<Self, ValueError> {
        if self.0.contains_key(&axis) {
            return Err(ValueError::DuplicateAxis);
        }
        if self.0.len() >= Self::MAX_AXES {
            return Err(ValueError::TooManyDimensions);
        }
        self.0.insert(axis, value);
        Ok(self)
    }

    /// The value of an axis.
    #[must_use]
    pub fn get(&self, axis: &Label) -> Option<&Label> {
        self.0.get(axis)
    }

    /// The `(axis, value)` pairs in ascending axis order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Label, &Label)> {
        self.0.iter()
    }

    /// The number of axes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no axes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Encode for Dimensions {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.len(self.0.len());
        for (axis, value) in &self.0 {
            w.value(axis);
            w.value(value);
        }
    }
}

impl Decode for Dimensions {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        let count = r.count(field, Self::MAX_AXES)?;
        let mut dimensions = BTreeMap::new();
        for _ in 0..count {
            let axis: Label = r.value(field)?;
            let value: Label = r.value(field)?;
            if dimensions
                .last_key_value()
                .is_some_and(|(last, _)| *last >= axis)
            {
                return Err(DecodeError::NonCanonical { field });
            }
            dimensions.insert(axis, value);
        }
        Ok(Self(dimensions))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Dimensions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(&self.0)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Dimensions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let map = BTreeMap::<Label, Label>::deserialize(deserializer)?;
        if map.len() > Self::MAX_AXES {
            return Err(D::Error::custom(ValueError::TooManyDimensions));
        }
        Ok(Self(map))
    }
}
