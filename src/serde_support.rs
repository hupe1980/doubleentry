//! Serde plumbing for validated types.
//!
//! Deriving `Deserialize` on a newtype whose constructor validates its input
//! defeats the validation: the derived implementation writes the inner field
//! directly. Any invariant the type advertises would then hold for values the
//! program built and not for values it read back, which is the worse of the two
//! cases — the value came from outside.
//!
//! Every validated type in this crate therefore round-trips through its own
//! constructor.

/// Implements serde for a newtype exposing `new(impl Into<String>) -> Result`
/// and `as_str()`, routing deserialisation through the constructor.
macro_rules! validating_string_serde {
    ($ty:ty) => {
        #[cfg(feature = "serde")]
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        #[cfg(feature = "serde")]
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
                Self::new(s.into_owned()).map_err(serde::de::Error::custom)
            }
        }
    };
}

pub(crate) use validating_string_serde;
