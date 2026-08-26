//! The signed-note wire format a tree head travels in.
//!
//! This is [C2SP `signed-note`](https://c2sp.org/signed-note) carrying a
//! [C2SP `tlog-checkpoint`](https://c2sp.org/tlog-checkpoint) body — the format
//! Certificate Transparency, the Go checksum database, Sigsum and sigstore all
//! speak. It is implemented here rather than invented because the entire value
//! of a witness is that it is somebody *else's*: a format only this crate
//! understands could only be cosigned by software only this crate ships, which
//! is not an independent check at all.
//!
//! # The shape
//!
//! ```text
//! acme-gmbh                                     ← origin
//! 4812                                          ← tree size
//! 3q2+7wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=  ← base64 root
//!                                               ← blank line
//! — witness.example +Ab3Cd4E BASE64SIGNATURE    ← zero or more signatures
//! ```
//!
//! Everything above the blank line is the **body**, and the body is what a
//! signature covers — bytes exactly as they appear, trailing newline included.
//! That is why [`SignedTreeHead::body`] hands back a `String` rather than the
//! parts: a verifier must sign what it read, not what it re-rendered.
//!
//! # Strict where it decides, verbatim where it does not
//!
//! A tree size with a leading zero, a root that is not 32 bytes, a non-canonical
//! base64 group, a missing trailing newline, a key name carrying a Unicode
//! space, or a signature line that is not `— name value` are all refused. Laxity
//! there is not tolerance, it is a second encoding of the same head — and a
//! signature covers the *text*, so two texts that mean one head are two heads
//! that carry one signature.
//!
//! Extension lines are the other half of the rule and go the other way: they
//! carry meaning this crate does not assign, so they are **kept verbatim** and
//! rendered back. Dropping them would make [`SignedTreeHead::body`] produce a
//! body nobody signed, and refusing them would mean refusing to cosign for any
//! log that uses one. This crate never writes one — the specification calls them
//! NOT RECOMMENDED, since monitors cannot audit them — but it reads them, which
//! is what lets it witness somebody else's log.
//!
//! # Cryptography is elsewhere
//!
//! Nothing in this module hashes or verifies anything. A [`NoteSignature`] is a
//! name, a key hash and an opaque blob; making one and checking one live behind
//! the `witness` feature in [`signing`](super::signing), so a caller who only
//! wants to *move* heads around pays for no cryptography.

use core::fmt;

use crate::hash::Hash;
use crate::merkle::TreeHead;

use super::base64;

/// The line that separates the body from the signatures.
const SIGNATURE_PREFIX: &str = "\u{2014} ";

/// True for a character that cannot appear in a note's space-delimited fields.
///
/// [C2SP `signed-note`](https://c2sp.org/signed-note) forbids Unicode spaces and
/// `+` in key names; the same applies to an origin, which is a line of the note
/// text. Control characters are excluded on top, since a newline would end the
/// line and a terminal escape would end up in somebody's log viewer.
///
/// `char::is_whitespace` rather than `== ' '` is the load-bearing part: U+00A0
/// and U+2003 are spaces to every other implementation of this format, and one
/// that this crate accepted and they did not would produce a note nobody else
/// can read.
fn is_forbidden_in_field(c: char) -> bool {
    c.is_whitespace() || c == '+' || c.is_control()
}

/// A key name: the second field of a signature line.
///
/// [C2SP `signed-note`](https://c2sp.org/signed-note): non-empty, and no Unicode
/// space or `+` (U+002B). Both exclusions are structural rather than stylistic —
/// a signature line is `— name value`, so a name carrying **any** space splits
/// one line into a name and a value that are not the ones written, and `+`
/// is excluded because key names appear in contexts that treat it specially.
///
/// Checking only ASCII `' '` would not be enough: U+00A0 and U+2003 are spaces
/// to every other implementation of this format and would be accepted here and
/// rejected there, which is the worst outcome — a note this crate produced that
/// nobody else can read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyName(String);

impl KeyName {
    /// Longest permitted key name, in bytes.
    pub const MAX_LEN: usize = 128;

    /// Validates and wraps a key name.
    ///
    /// # Errors
    ///
    /// Returns [`NoteError::MalformedKeyName`] for an empty name, one longer
    /// than [`Self::MAX_LEN`], or one containing a Unicode space, a `+`, or a
    /// control character.
    pub fn new(name: impl Into<String>) -> Result<Self, NoteError> {
        let name = name.into();
        if name.is_empty() || name.len() > Self::MAX_LEN || name.chars().any(is_forbidden_in_field)
        {
            return Err(NoteError::MalformedKeyName);
        }
        Ok(Self(name))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KeyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The log a head belongs to: the note's first line.
///
/// Distinct from [`LedgerId`](crate::LedgerId) on purpose, though a ledger's
/// identifier is the obvious thing to build one from. A `LedgerId` names books
/// *inside* one deployment; an origin has to be unique among every log a witness
/// tracks, which is a wider namespace and not this crate's to allocate. C2SP
/// suggests a schemaless URL — `example.com/ledgers/acme-gmbh` — for exactly
/// that reason.
///
/// Two logs sharing an origin is the failure this type exists to make
/// deliberate: a witness keys its state on the origin, so it would see one log's
/// growth as the other's fork and refuse both.
///
/// Spaces and `+` are refused for the same reason they are in a
/// [`KeyName`]: implementations of this format split lines on spaces, and one
/// that this crate accepted and they did not would produce a note nobody else
/// can read. The specification says an origin *should* be a schema-less URL; a
/// URL has neither, so nothing legitimate is excluded.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Origin(String);

impl Origin {
    /// Longest permitted origin, in bytes.
    pub const MAX_LEN: usize = 256;

    /// Validates and wraps an origin.
    ///
    /// # Errors
    ///
    /// Returns [`NoteError::MalformedOrigin`] for an empty origin, one longer
    /// than [`Self::MAX_LEN`], or one containing a Unicode space, a `+`, or a
    /// control character — a newline would end the line and forge a tree size.
    pub fn new(origin: impl Into<String>) -> Result<Self, NoteError> {
        let origin = origin.into();
        if origin.is_empty()
            || origin.len() > Self::MAX_LEN
            || origin.chars().any(is_forbidden_in_field)
        {
            return Err(NoteError::MalformedOrigin);
        }
        Ok(Self(origin))
    }

    /// The origin string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One signature line, uninterpreted.
///
/// The name and the four-byte key hash identify which key signed; `value` is
/// whatever that key's algorithm puts on the wire. For a plain Ed25519 note
/// signature that is 64 bytes; for a `cosignature/v1` it is an 8-byte big-endian
/// timestamp followed by 64 bytes of signature.
///
/// Deliberately opaque here. Deciding whether a signature is *good* needs a
/// public key and a hash function, and both live behind the `witness` feature —
/// see [`VerifyingKey`](super::signing::VerifyingKey).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSignature {
    /// The signing key's name.
    pub name: KeyName,
    /// First four bytes of the key's hash, as a cheap key selector.
    pub key_hash: [u8; 4],
    /// The algorithm-specific signature bytes.
    pub value: Vec<u8>,
}

impl NoteSignature {
    /// Renders the note line for this signature.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut blob = Vec::with_capacity(4usize.saturating_add(self.value.len()));
        blob.extend_from_slice(&self.key_hash);
        blob.extend_from_slice(&self.value);
        format!(
            "{SIGNATURE_PREFIX}{} {}\n",
            self.name,
            base64::encode(&blob)
        )
    }

    /// Parses one signature line, without its trailing newline.
    fn parse_line(line: &str) -> Result<Self, NoteError> {
        let rest = line
            .strip_prefix(SIGNATURE_PREFIX)
            .ok_or(NoteError::MalformedSignature)?;
        let (name, encoded) = rest.split_once(' ').ok_or(NoteError::MalformedSignature)?;
        let blob = base64::decode(encoded).ok_or(NoteError::MalformedSignature)?;
        let key_hash: [u8; 4] = blob
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(NoteError::MalformedSignature)?;
        let value = blob.get(4..).ok_or(NoteError::MalformedSignature)?.to_vec();
        if value.is_empty() {
            return Err(NoteError::MalformedSignature);
        }
        Ok(Self {
            name: KeyName::new(name)?,
            key_hash,
            value,
        })
    }
}

/// A tree head in the note format, with whatever signatures it has collected.
///
/// The artefact that actually travels: a log operator publishes one, a witness
/// adds its line and hands it back, and a verifier checks the lines against keys
/// it decided to trust before any of this started.
///
/// ```
/// # use doubleentry::witness::{Origin, SignedTreeHead};
/// # use doubleentry::{Hash, TreeHead};
/// let head = TreeHead { size: 4812, root: Hash::from_bytes([0x2a; 32]) };
/// let sth = SignedTreeHead::new(Origin::new("example.com/ledgers/acme-gmbh")?, head);
///
/// // The text is what a signature covers, so it round-trips exactly.
/// let text = sth.to_note();
/// assert_eq!(SignedTreeHead::parse(&text)?, sth);
/// assert!(text.starts_with("example.com/ledgers/acme-gmbh\n4812\n"));
/// # Ok::<(), doubleentry::witness::NoteError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTreeHead {
    /// Which log.
    pub origin: Origin,
    /// The head being attested to.
    pub head: TreeHead,
    /// Any further body lines, carried verbatim.
    ///
    /// The format permits lines after the root, with meanings this crate does
    /// not assign. They are **kept** rather than dropped, because a signature
    /// covers the body byte for byte: a parser that discarded them would render
    /// a different body from the one that was signed, and every signature on the
    /// note would stop verifying for a reason no error message would explain.
    ///
    /// Keeping them is also what lets this witness a log run by somebody else.
    /// The specification calls extension lines NOT RECOMMENDED — they are not
    /// auditable by monitors — so this crate never produces one, and refusing to
    /// *read* one would have meant refusing to cosign for any log that does.
    pub extensions: Vec<String>,
    /// The signatures collected so far, in the order they appear.
    pub signatures: Vec<NoteSignature>,
}

impl SignedTreeHead {
    /// A head with no extensions and no signatures yet.
    #[must_use]
    pub fn new(origin: Origin, head: TreeHead) -> Self {
        Self {
            origin,
            head,
            extensions: Vec::new(),
            signatures: Vec::new(),
        }
    }

    /// The signed body: origin, size, root, then any extension lines.
    ///
    /// **This is what a signature covers**, byte for byte, trailing newline
    /// included. Render it once and sign what you rendered.
    #[must_use]
    pub fn body(&self) -> String {
        let mut out = format!(
            "{}\n{}\n{}\n",
            self.origin,
            self.head.size,
            base64::encode(self.head.root.as_bytes())
        );
        for line in &self.extensions {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// The complete note: body, blank line, then one line per signature.
    #[must_use]
    pub fn to_note(&self) -> String {
        let mut out = self.body();
        out.push('\n');
        for signature in &self.signatures {
            out.push_str(&signature.to_line());
        }
        out
    }

    /// Adds a signature line.
    pub fn add_signature(&mut self, signature: NoteSignature) {
        self.signatures.push(signature);
    }

    /// True when some signature carries this key name and hash.
    ///
    /// A presence check, not a verification: it says a line claiming that key is
    /// on the note, which is what a client uses to decide whether it still needs
    /// to ask that witness. Whether the line is *good* is
    /// [`VerifyingKey::verify`](super::signing::VerifyingKey::verify)'s question.
    #[must_use]
    pub fn has_signature_from(&self, name: &KeyName, key_hash: [u8; 4]) -> bool {
        self.signatures
            .iter()
            .any(|s| s.name == *name && s.key_hash == key_hash)
    }

    /// Parses a note.
    ///
    /// # Errors
    ///
    /// Returns [`NoteError`] naming what was wrong. Every failure is a refusal
    /// to guess: there is exactly one note text per head, and accepting a second
    /// one would let two texts share a signature.
    pub fn parse(text: &str) -> Result<Self, NoteError> {
        // Split at the first blank line. `body` keeps its trailing newline,
        // because that is what was signed.
        let (body, rest) = text.split_once("\n\n").ok_or(NoteError::MissingSeparator)?;
        let mut lines = body.split('\n');
        let origin = Origin::new(lines.next().ok_or(NoteError::Truncated)?)?;

        let size_line = lines.next().ok_or(NoteError::Truncated)?;
        // Canonical decimal only: `04812` and `4812` must not be two notes for
        // one head.
        if size_line.is_empty()
            || !size_line.bytes().all(|b| b.is_ascii_digit())
            || (size_line.len() > 1 && size_line.starts_with('0'))
        {
            return Err(NoteError::MalformedSize);
        }
        let size: u64 = size_line.parse().map_err(|_| NoteError::MalformedSize)?;

        let root_line = lines.next().ok_or(NoteError::Truncated)?;
        let root_bytes = base64::decode(root_line).ok_or(NoteError::MalformedRoot)?;
        let root: [u8; Hash::LEN] = root_bytes
            .as_slice()
            .try_into()
            .map_err(|_| NoteError::MalformedRoot)?;

        // Extension lines carry meaning this crate does not assign. Kept
        // verbatim, because a signature covers the body byte for byte: dropping
        // them would make `body` render something nobody signed.
        let mut extensions = Vec::new();
        for line in lines {
            // The format forbids empty lines inside a body — one would be the
            // separator, so a note carrying it would parse as two notes.
            if line.is_empty() {
                return Err(NoteError::Truncated);
            }
            extensions.push(line.to_owned());
        }

        let mut signatures = Vec::new();
        for line in rest.split('\n') {
            if line.is_empty() {
                continue;
            }
            signatures.push(NoteSignature::parse_line(line)?);
        }

        Ok(Self {
            origin,
            head: TreeHead {
                size,
                root: Hash::from_bytes(root),
            },
            extensions,
            signatures,
        })
    }
}

/// Failure reading or building a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NoteError {
    /// The origin was empty, too long, or carried a control character.
    #[error("origin is empty, too long, or contains a control character")]
    MalformedOrigin,
    /// The key name was empty, too long, or carried a space or control character.
    #[error("key name is empty, too long, or contains a space or control character")]
    MalformedKeyName,
    /// The tree size was not a canonical decimal integer.
    #[error("tree size is not a canonical decimal integer")]
    MalformedSize,
    /// The root was not canonical base64 of exactly 32 bytes.
    #[error("root is not canonical base64 of 32 bytes")]
    MalformedRoot,
    /// A signature line was not `— name base64`, or its blob was too short.
    #[error("signature line is malformed")]
    MalformedSignature,
    /// The note had no blank line separating body from signatures.
    #[error("note has no blank line between its body and its signatures")]
    MissingSeparator,
    /// The body ended before its three required lines.
    #[error("note body ended early")]
    Truncated,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::new("example.com/ledgers/acme").expect("valid")
    }

    fn head(size: u64) -> TreeHead {
        TreeHead {
            size,
            root: Hash::from_bytes([7u8; 32]),
        }
    }

    #[test]
    fn a_note_round_trips_exactly() {
        let mut sth = SignedTreeHead::new(origin(), head(4812));
        sth.add_signature(NoteSignature {
            name: KeyName::new("witness.example").expect("valid"),
            key_hash: [1, 2, 3, 4],
            value: vec![9u8; 64],
        });
        let text = sth.to_note();
        assert_eq!(SignedTreeHead::parse(&text).expect("parses"), sth);
        assert_eq!(
            SignedTreeHead::parse(&text).expect("parses").to_note(),
            text
        );
    }

    #[test]
    fn the_body_is_the_first_three_lines_and_ends_in_a_newline() {
        let sth = SignedTreeHead::new(origin(), head(1));
        let body = sth.body();
        assert!(body.ends_with('\n'));
        assert_eq!(body.lines().count(), 3);
        assert!(sth.to_note().starts_with(&body));
    }

    #[test]
    fn an_unsigned_note_still_parses() {
        let sth = SignedTreeHead::new(origin(), head(0));
        assert_eq!(
            SignedTreeHead::parse(&sth.to_note()).expect("parses"),
            sth,
            "a head with no signatures yet is the normal starting state"
        );
    }

    #[test]
    fn a_non_canonical_size_is_two_notes_for_one_head_and_is_refused() {
        let text =
            "example.com/ledgers/acme\n04812\nBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\n\n";
        assert_eq!(SignedTreeHead::parse(text), Err(NoteError::MalformedSize));
    }

    #[test]
    fn a_root_of_the_wrong_width_is_refused() {
        let text = "example.com/ledgers/acme\n1\nZm9v\n\n";
        assert_eq!(SignedTreeHead::parse(text), Err(NoteError::MalformedRoot));
    }

    #[test]
    fn an_extension_line_is_kept_verbatim() {
        let text =
            "example.com/ledgers/acme\n1\nBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\nextra\n\n";
        let parsed = SignedTreeHead::parse(text).expect("parses");
        assert_eq!(parsed.extensions, vec!["extra".to_owned()]);
        assert_eq!(
            parsed.to_note(),
            text,
            "a signature covers the body byte for byte, so it has to round-trip"
        );
        assert!(
            parsed.body().ends_with("extra\n"),
            "dropping it would make `body` render something nobody signed"
        );
    }

    #[test]
    fn this_crate_never_produces_an_extension_line() {
        // NOT RECOMMENDED by the specification: monitors cannot audit them.
        // Read, never written.
        assert!(SignedTreeHead::new(origin(), head(1)).extensions.is_empty());
    }

    #[test]
    fn an_empty_line_inside_a_body_is_refused() {
        // It would be the separator, so the note would parse as two notes.
        let text = "example.com/ledgers/acme\n1\nBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\n\nextra\n\n";
        assert!(SignedTreeHead::parse(text).is_err());
    }

    #[test]
    fn a_key_name_may_not_carry_a_unicode_space_or_a_plus() {
        // U+00A0 is a space to every other implementation of this format.
        assert_eq!(
            KeyName::new("has\u{a0}nbsp"),
            Err(NoteError::MalformedKeyName)
        );
        assert_eq!(
            KeyName::new("has\u{2003}emsp"),
            Err(NoteError::MalformedKeyName)
        );
        assert_eq!(KeyName::new("has+plus"), Err(NoteError::MalformedKeyName));
        assert_eq!(
            Origin::new("has\u{a0}nbsp"),
            Err(NoteError::MalformedOrigin)
        );
        assert_eq!(Origin::new("has+plus"), Err(NoteError::MalformedOrigin));
        // And an ordinary schema-less URL still works.
        assert!(Origin::new("example.com/ledgers/acme-gmbh").is_ok());
        assert!(KeyName::new("witness.example.com/w1").is_ok());
    }

    #[test]
    fn a_note_without_a_blank_line_is_refused() {
        let text = "example.com/ledgers/acme\n1\nBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\n";
        assert_eq!(
            SignedTreeHead::parse(text),
            Err(NoteError::MissingSeparator)
        );
    }

    #[test]
    fn a_key_name_cannot_split_or_end_a_line() {
        assert_eq!(KeyName::new("has space"), Err(NoteError::MalformedKeyName));
        assert_eq!(
            KeyName::new("has\nnewline"),
            Err(NoteError::MalformedKeyName)
        );
        assert_eq!(KeyName::new(""), Err(NoteError::MalformedKeyName));
    }

    #[test]
    fn an_origin_cannot_forge_a_tree_size() {
        assert_eq!(Origin::new("acme\n999999"), Err(NoteError::MalformedOrigin));
    }
}
