//! [`LedgerPolicy`]: the ledger-wide booking rules that are data.

use super::{Decode, DecodeError, Encode, Label, Reader, Sink, ValueError, Writer};

/// The ledger-wide policy: dimensions every posting must carry, and how far a
/// value date may drift from the booking date.
///
/// The default requires nothing and allows any drift. Currencies are not part
/// of the policy: each is declared with its exponent by its own record.
///
/// ```
/// use doubleentry::{Label, LedgerPolicy};
///
/// let policy = LedgerPolicy::new([Label::new("project")?, Label::new("cost-center")?], Some(30))?;
/// let required: Vec<&str> = policy.required_dimensions().iter().map(Label::as_str).collect();
/// assert_eq!(required, ["cost-center", "project"]);
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct LedgerPolicy {
    required_dimensions: Vec<Label>,
    max_value_date_drift_days: Option<u32>,
}

impl LedgerPolicy {
    /// A policy; the required dimensions are sorted.
    ///
    /// # Errors
    ///
    /// [`ValueError::Duplicate`] for an axis named twice,
    /// [`ValueError::TooManyDimensions`] for more than
    /// [`Dimensions::MAX_AXES`](crate::Dimensions::MAX_AXES).
    pub fn new(
        required_dimensions: impl IntoIterator<Item = Label>,
        max_value_date_drift_days: Option<u32>,
    ) -> Result<Self, ValueError> {
        let mut required: Vec<Label> = required_dimensions.into_iter().collect();
        if required.len() > super::Dimensions::MAX_AXES {
            return Err(ValueError::TooManyDimensions);
        }
        required.sort();
        if required
            .windows(2)
            .any(|pair| matches!(pair, [a, b] if a == b))
        {
            return Err(ValueError::Duplicate);
        }
        Ok(Self {
            required_dimensions: required,
            max_value_date_drift_days,
        })
    }

    /// The axes every posting must carry, ascending.
    #[must_use]
    pub fn required_dimensions(&self) -> &[Label] {
        &self.required_dimensions
    }

    /// The largest allowed distance between value and booking date, in days.
    #[must_use]
    pub const fn max_value_date_drift_days(&self) -> Option<u32> {
        self.max_value_date_drift_days
    }
}

impl Encode for LedgerPolicy {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.seq(&self.required_dimensions);
        w.opt(self.max_value_date_drift_days.as_ref());
    }
}

impl Decode for LedgerPolicy {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        let count = r.count(field, super::Dimensions::MAX_AXES)?;
        let mut required: Vec<Label> = Vec::new();
        for _ in 0..count {
            let label: Label = r.value(field)?;
            if required.last().is_some_and(|last| *last >= label) {
                return Err(DecodeError::NonCanonical { field });
            }
            required.push(label);
        }
        let drift = r.opt(field, Reader::u32)?;
        Ok(Self {
            required_dimensions: required,
            max_value_date_drift_days: drift,
        })
    }
}

#[cfg(feature = "serde")]
mod serde_impl {
    use super::{Label, LedgerPolicy};

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(rename = "LedgerPolicy")]
    struct Parts {
        required_dimensions: Vec<Label>,
        max_value_date_drift_days: Option<u32>,
    }

    impl serde::Serialize for LedgerPolicy {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            Parts {
                required_dimensions: self.required_dimensions.clone(),
                max_value_date_drift_days: self.max_value_date_drift_days,
            }
            .serialize(serializer)
        }
    }

    impl<'de> serde::Deserialize<'de> for LedgerPolicy {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let parts = Parts::deserialize(deserializer)?;
            Self::new(parts.required_dimensions, parts.max_value_date_drift_days)
                .map_err(serde::de::Error::custom)
        }
    }
}
