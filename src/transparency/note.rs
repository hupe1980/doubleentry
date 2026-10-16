//! C2SP `signed-note`: [`KeyName`], [`NoteSignature`] and [`Note`].

use core::fmt;
use core::str::FromStr;

use super::Error;
use super::base64;
use super::error::Syntax;
use crate::model::LedgerId;

/// The em dash and space that open a signature line.
pub(crate) const SIGNATURE_PREFIX: &str = "\u{2014} ";

/// The largest note this crate reads, in bytes.
pub const MAX_NOTE_BYTES: usize = 1_000_000;

/// The most signature lines a note may carry.
pub const MAX_SIGNATURES: usize = 100;

/// A key name or log origin: 1–255 bytes of UTF-8 without a Unicode space,
/// `+` or a control character (c2sp.org/tlog-cosignature "cosigner name").
///
/// A [`LedgerId`] is always a valid key name, so a ledger's checkpoint
/// origin is its id.
///
/// ```
/// use doubleentry::LedgerId;
/// use doubleentry::transparency::KeyName;
///
/// let origin = KeyName::from(&LedgerId::new("example.com/ledgers/acme")?);
/// assert_eq!(origin.as_str(), "example.com/ledgers/acme");
/// assert!(KeyName::new("has space").is_err());
/// assert!(KeyName::new("a+b").is_err());
/// assert!(KeyName::new("x".repeat(256)).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyName(String);

impl KeyName {
    /// The most bytes in a key name.
    pub const MAX_BYTES: usize = 255;

    /// Validates a key name.
    ///
    /// # Errors
    ///
    /// [`Error::BadName`].
    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        if is_valid_name(&name) {
            Ok(Self(name))
        } else {
            Err(Error::BadName)
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The key-name rule, on text that is already UTF-8.
pub(crate) fn is_valid_name(name: &str) -> bool {
    (1..=KeyName::MAX_BYTES).contains(&name.len())
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c == '+' || c.is_control())
}

impl From<&LedgerId> for KeyName {
    fn from(ledger: &LedgerId) -> Self {
        // A ledger id is 1–255 bytes of printable ASCII without `+`.
        Self(ledger.as_str().to_owned())
    }
}

impl fmt::Display for KeyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for KeyName {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl AsRef<str> for KeyName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// One signature line of a note: `— <name> <base64(key ID ‖ signature)>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoteSignature {
    name: KeyName,
    key_id: [u8; 4],
    signature: Vec<u8>,
}

impl NoteSignature {
    /// A signature line from its parts; `signature` must not be empty.
    pub(crate) fn new(name: KeyName, key_id: [u8; 4], signature: Vec<u8>) -> Self {
        Self {
            name,
            key_id,
            signature,
        }
    }

    /// The key name.
    #[must_use]
    pub const fn name(&self) -> &KeyName {
        &self.name
    }

    /// The key ID: the first four bytes of the line's base64.
    #[must_use]
    pub const fn key_id(&self) -> [u8; 4] {
        self.key_id
    }

    /// The signature bytes after the key ID.
    #[must_use]
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }

    /// The line, newline included.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut raw = Vec::with_capacity(self.signature.len().saturating_add(4));
        raw.extend_from_slice(&self.key_id);
        raw.extend_from_slice(&self.signature);
        format!("{SIGNATURE_PREFIX}{} {}\n", self.name, base64::encode(&raw))
    }

    /// Parses one line without its newline; `line_no` names it in errors.
    pub(crate) fn parse_line(line: &str, line_no: u32) -> Result<Self, Error> {
        let malformed = |what| Error::Malformed {
            what,
            line: line_no,
        };
        let rest = line
            .strip_prefix(SIGNATURE_PREFIX)
            .ok_or(malformed(Syntax::SignatureLine))?;
        let (name, encoded) = rest
            .split_once(' ')
            .ok_or(malformed(Syntax::SignatureLine))?;
        let name = KeyName::new(name)?;
        let raw = base64::decode(encoded).ok_or(malformed(Syntax::Base64))?;
        match raw.split_first_chunk::<4>() {
            Some((key_id, signature)) if !signature.is_empty() => {
                Ok(Self::new(name, *key_id, signature.to_vec()))
            }
            _ => Err(malformed(Syntax::SignatureLine)),
        }
    }
}

/// A C2SP signed note: a text ending in a newline, a blank line, and 1–100
/// signature lines.
///
/// Parsing is strict and rendering exact: for every accepted input,
/// [`to_bytes`](Self::to_bytes) gives the input back. Refused: invalid
/// UTF-8, control characters other than newline, no blank line, no or more
/// than 100 signature lines, a malformed or non-canonical signature line,
/// a repeated (key name, key ID) pair, a missing final newline, more than
/// 1 000 000 bytes. Nothing here checks a signature; see
/// `SignedCheckpoint::verify` (feature `witness`).
///
/// ```
/// use doubleentry::transparency::Note;
///
/// let text = "This is an example message.\n\n\
///     \u{2014} example.com/foo Uw2QOkn8srV1yJGh2VYRlL1Tnagv1YEq6TfXppzi2ONncAlTgK7Ztg1ERYNZXsYjOBH3mFXmRKuwHjG1Yu72IneyaQM=\n";
/// let note = Note::parse(text.as_bytes())?;
/// assert_eq!(note.text(), "This is an example message.\n");
/// assert_eq!(note.signatures()[0].key_id(), [0x53, 0x0d, 0x90, 0x3a]);
/// assert_eq!(note.to_bytes(), text.as_bytes());
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Note {
    text: String,
    signatures: Vec<NoteSignature>,
}

impl Note {
    /// A note from a text ending in a newline and its signatures, checked as
    /// [`parse`](Self::parse) checks them.
    #[cfg(feature = "witness")]
    pub(crate) fn new(text: String, signatures: Vec<NoteSignature>) -> Result<Self, Error> {
        let mut bytes = text.into_bytes();
        bytes.push(b'\n');
        for signature in &signatures {
            bytes.extend_from_slice(signature.to_line().as_bytes());
        }
        Self::parse(&bytes)
    }

    /// Parses a note.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`], [`Error::Malformed`] naming the rule and line,
    /// [`Error::TooManySignatures`], [`Error::DuplicateSignature`] or
    /// [`Error::BadName`].
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_NOTE_BYTES {
            return Err(Error::TooLarge {
                limit: MAX_NOTE_BYTES,
            });
        }
        let all = core::str::from_utf8(bytes).map_err(|error| Error::Malformed {
            what: Syntax::Utf8,
            line: line_of(bytes, error.valid_up_to()),
        })?;
        if let Some(at) = all.bytes().position(|byte| byte < 0x20 && byte != b'\n') {
            return Err(Error::Malformed {
                what: Syntax::ControlChar,
                line: line_of(bytes, at),
            });
        }
        if !all.ends_with('\n') {
            return Err(Error::Malformed {
                what: Syntax::MissingNewline,
                line: line_of(bytes, bytes.len()),
            });
        }
        let split = all.rfind("\n\n").ok_or(Error::Malformed {
            what: Syntax::MissingSeparator,
            line: line_of(bytes, bytes.len()),
        })?;
        let (text, rest) = all.split_at(split.saturating_add(1));
        let block = rest.get(1..).unwrap_or_default();
        // The text's lines, the blank line, then the first signature line.
        let first = line_of(bytes, split.saturating_add(2));
        if block.is_empty() {
            return Err(Error::Malformed {
                what: Syntax::NoSignature,
                line: first,
            });
        }
        let mut signatures: Vec<NoteSignature> = Vec::new();
        for (offset, line) in block.split_terminator('\n').enumerate() {
            let line_no = first.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
            if signatures.len() == MAX_SIGNATURES {
                return Err(Error::TooManySignatures);
            }
            let signature = NoteSignature::parse_line(line, line_no)?;
            if signatures
                .iter()
                .any(|seen| (&seen.name, seen.key_id) == (&signature.name, signature.key_id))
            {
                return Err(Error::DuplicateSignature {
                    name: signature.name,
                    key_id: signature.key_id,
                });
            }
            signatures.push(signature);
        }
        Ok(Self {
            text: text.to_owned(),
            signatures,
        })
    }

    /// The signed text, final newline included.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The signature lines, in order.
    #[must_use]
    pub fn signatures(&self) -> &[NoteSignature] {
        &self.signatures
    }

    /// The note's bytes: exactly what was parsed.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.text.len().saturating_add(128));
        bytes.extend_from_slice(self.text.as_bytes());
        bytes.push(b'\n');
        for signature in &self.signatures {
            bytes.extend_from_slice(signature.to_line().as_bytes());
        }
        bytes
    }
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)?;
        f.write_str("\n")?;
        for signature in &self.signatures {
            f.write_str(&signature.to_line())?;
        }
        Ok(())
    }
}

/// The 1-based line of byte offset `at`.
pub(crate) fn line_of(bytes: &[u8], at: usize) -> u32 {
    let newlines = bytes
        .get(..at)
        .unwrap_or(bytes)
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    u32::try_from(newlines.saturating_add(1)).unwrap_or(u32::MAX)
}
