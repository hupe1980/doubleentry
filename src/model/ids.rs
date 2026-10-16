//! Identities: [`LedgerId`], [`IdempotencyKey`], [`EntryId`], [`AccountHandle`].

use core::fmt;
use core::str::FromStr;

use super::{Decode, DecodeError, Encode, Reader, Sink, ValueError, Writer, hex};
use crate::log::hash::{TAG_ENTRY_ID, tagged};

/// A ledger's identity: 1–255 bytes, each printable ASCII (`0x21–0x7E`)
/// except `+`.
///
/// Usable verbatim as a C2SP checkpoint origin and note key name, by
/// convention a domain-style name such as `example.com/acme`.
///
/// ```
/// use doubleentry::LedgerId;
///
/// assert!(LedgerId::new("example.com/acme").is_ok());
/// assert!(LedgerId::new("has space").is_err());
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerId(String);

impl LedgerId {
    /// The most bytes in a ledger id.
    pub const MAX_BYTES: usize = 255;

    /// Validates a ledger id.
    ///
    /// # Errors
    ///
    /// [`ValueError::Empty`], [`ValueError::TooLong`] above 255 bytes, or
    /// [`ValueError::InvalidCharacter`] for a space, `+`, a control or a
    /// non-ASCII character.
    pub fn new(text: impl Into<String>) -> Result<Self, ValueError> {
        let text = text.into();
        if text.is_empty() {
            return Err(ValueError::Empty);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(ValueError::TooLong {
                max: Self::MAX_BYTES,
                found: text.len(),
            });
        }
        if let Some(found) = text.chars().find(|c| !matches!(c, '!'..='~') || *c == '+') {
            return Err(ValueError::InvalidCharacter { found });
        }
        Ok(Self(text))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The caller's identity for an entry — typically the source transaction id:
/// 1–128 bytes of UTF-8 without control characters, compared byte-exactly
/// (never normalised).
///
/// The key is committed in the entry's leaf and the [`EntryId`] is derived
/// from it, so a retry with the same key names the same entry.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// The most bytes in a key.
    pub const MAX_BYTES: usize = 128;

    /// Validates a key.
    ///
    /// # Errors
    ///
    /// [`ValueError::Empty`], [`ValueError::TooLong`] above 128 bytes, or
    /// [`ValueError::ControlCharacter`].
    pub fn new(text: impl Into<String>) -> Result<Self, ValueError> {
        let text = text.into();
        if text.is_empty() {
            return Err(ValueError::Empty);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(ValueError::TooLong {
                max: Self::MAX_BYTES,
                found: text.len(),
            });
        }
        if text.chars().any(char::is_control) {
            return Err(ValueError::ControlCharacter);
        }
        Ok(Self(text))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! text_id {
    ($($ty:ident),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                super::encoding::pad(f, &self.0)
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($ty), "({:?})"), self.0)
            }
        }

        impl FromStr for $ty {
            type Err = ValueError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::new(text)
            }
        }

        impl TryFrom<&str> for $ty {
            type Error = ValueError;

            fn try_from(text: &str) -> Result<Self, Self::Error> {
                Self::new(text)
            }
        }

        impl TryFrom<String> for $ty {
            type Error = ValueError;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::new(text)
            }
        }

        impl AsRef<str> for $ty {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Encode for $ty {
            fn encode<S: Sink>(&self, w: &mut Writer<S>) {
                w.str(&self.0);
            }
        }

        impl Decode for $ty {
            fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
                Self::new(r.str(field)?).map_err(|error| DecodeError::Value { field, error })
            }
        }

        #[cfg(feature = "serde")]
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        #[cfg(feature = "serde")]
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                super::serde_str::deserialize(deserializer)
            }
        }
    )*};
}

text_id!(LedgerId, IdempotencyKey);

/// An entry's identity: 16 bytes derived from the ledger and the entry's
/// [`IdempotencyKey`], never generated.
///
/// `EntryId::derive(ledger, key)` is the first 16 bytes of
/// `SHA-256(u64 len(tag) ‖ "doubleentry/1/entry-id" ‖ str(ledger) ‖ str(key))`:
/// deterministic, with no clock and no randomness, and committed because the
/// key is. `Display` prints 32 lowercase hex digits; `FromStr` accepts either
/// case.
///
/// ```
/// use doubleentry::{EntryId, IdempotencyKey, LedgerId};
///
/// let id = EntryId::derive(&LedgerId::new("example.com/acme")?, &IdempotencyKey::new("invoice-2026-0001")?);
/// assert_eq!(id.to_string(), "bab3f09bfedcc7d793e973e78dbe4bd4");
/// assert_eq!("BAB3F09BFEDCC7D793E973E78DBE4BD4".parse::<EntryId>()?, id);
/// # Ok::<(), doubleentry::error::ValueError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId([u8; 16]);

impl EntryId {
    /// Derives the id of the entry with `key` in `ledger`.
    #[must_use]
    pub fn derive(ledger: &LedgerId, key: &IdempotencyKey) -> Self {
        let mut payload = Writer::new(Vec::new());
        payload.value(ledger);
        payload.value(key);
        let digest = tagged(TAG_ENTRY_ID, &payload.into_inner());
        let mut id = [0u8; 16];
        for (slot, byte) in id.iter_mut().zip(digest.as_bytes()) {
            *slot = *byte;
        }
        Self(id)
    }

    /// An id from its 16 bytes — for decoding and references, not for new
    /// entries.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex::write(f, &self.0)
    }
}

impl fmt::Debug for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntryId({self})")
    }
}

impl FromStr for EntryId {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        hex::parse(text).map(Self)
    }
}

impl Encode for EntryId {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.fixed(&self.0);
    }
}

impl Decode for EntryId {
    fn decode(r: &mut Reader<'_>, _field: &'static str) -> Result<Self, DecodeError> {
        r.array().map(Self)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for EntryId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for EntryId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::serde_str::deserialize(deserializer)
    }
}

/// An account's number in its ledger: the count of registrations before it.
///
/// Handles index the seal balance trees and are committed in the account's
/// register record, so one inclusion proof names the account behind a handle.
/// The largest handle is `u32::MAX − 1`, so the number of accounts always
/// fits a `u32`.
///
/// ```
/// use doubleentry::AccountHandle;
///
/// assert_eq!(AccountHandle::new(7).map(AccountHandle::get), Some(7));
/// assert_eq!(AccountHandle::new(u32::MAX), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct AccountHandle(u32);

impl AccountHandle {
    /// The largest handle.
    pub const MAX: u32 = u32::MAX - 1;

    /// A handle; `None` above [`MAX`](Self::MAX).
    #[must_use]
    pub const fn new(index: u32) -> Option<Self> {
        if index > Self::MAX {
            None
        } else {
            Some(Self(index))
        }
    }

    /// The handle's number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for AccountHandle {
    /// `#` and the handle's number (`#7`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        super::encoding::pad(f, &format!("#{}", self.0))
    }
}

impl Encode for AccountHandle {
    fn encode<S: Sink>(&self, w: &mut Writer<S>) {
        w.u32(self.0);
    }
}

impl Decode for AccountHandle {
    fn decode(r: &mut Reader<'_>, field: &'static str) -> Result<Self, DecodeError> {
        Self::new(r.u32()?).ok_or(DecodeError::Value {
            field,
            error: ValueError::OutOfRange,
        })
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for AccountHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        Self::new(u32::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom(ValueError::OutOfRange))
    }
}
