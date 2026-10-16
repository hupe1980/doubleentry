//! C2SP `tlog-checkpoint`: [`Checkpoint`] and [`SignedCheckpoint`].

use core::fmt;

use super::Error;
use super::base64;
use super::error::Syntax;
use super::note::{KeyName, Note, NoteSignature};
use crate::log::{Hash, TreeHead};

/// A log's head as checkpoint text: origin, tree size, root and — only when
/// read from another log — extension lines.
///
/// ```
/// use doubleentry::transparency::{Checkpoint, KeyName};
/// use doubleentry::{TreeHead, log::empty_root};
///
/// let origin = KeyName::new("example.com/ledgers/acme")?;
/// let checkpoint = Checkpoint::new(origin, TreeHead { size: 0, root: empty_root() });
/// assert_eq!(
///     checkpoint.text(),
///     "example.com/ledgers/acme\n0\n47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\n",
/// );
/// assert_eq!(Checkpoint::parse_text(&checkpoint.text())?, checkpoint);
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Checkpoint {
    origin: KeyName,
    head: TreeHead,
    extensions: Vec<String>,
}

impl Checkpoint {
    /// The checkpoint of `head` for `origin`, without extension lines (this
    /// crate never writes one).
    #[must_use]
    pub const fn new(origin: KeyName, head: TreeHead) -> Self {
        Self {
            origin,
            head,
            extensions: Vec::new(),
        }
    }

    /// The log's origin.
    #[must_use]
    pub const fn origin(&self) -> &KeyName {
        &self.origin
    }

    /// The tree size and root.
    #[must_use]
    pub const fn head(&self) -> TreeHead {
        self.head
    }

    /// The extension lines, verbatim.
    #[must_use]
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }

    /// The note text: origin, size, base64 root and extension lines, each
    /// ending in a newline.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!(
            "{}\n{}\n{}\n",
            self.origin,
            self.head.size,
            base64::encode(self.head.root.as_bytes())
        );
        for line in &self.extensions {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    /// Parses checkpoint text: at least three non-empty lines — origin,
    /// canonical decimal size, canonical base64 of a 32-byte root — then
    /// non-empty extension lines, every line ending in a newline.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] naming the rule and line, or [`Error::BadName`]
    /// for the origin.
    pub fn parse_text(text: &str) -> Result<Self, Error> {
        let malformed = |what, line| Error::Malformed { what, line };
        if text.bytes().any(|byte| byte < 0x20 && byte != b'\n') {
            return Err(malformed(Syntax::ControlChar, 0));
        }
        let body = text
            .strip_suffix('\n')
            .ok_or(malformed(Syntax::MissingNewline, 1))?;
        let lines: Vec<&str> = body.split('\n').collect();
        if let Some(at) = lines.iter().position(|line| line.is_empty()) {
            return Err(malformed(Syntax::EmptyLine, line_number(at)));
        }
        let [origin, size, root, extensions @ ..] = lines.as_slice() else {
            return Err(malformed(Syntax::EmptyLine, line_number(lines.len())));
        };
        let origin = KeyName::new(*origin)?;
        let size = parse_decimal(size).ok_or(malformed(Syntax::Decimal, 2))?;
        let root = base64::decode(root).ok_or(malformed(Syntax::Base64, 3))?;
        let root: [u8; 32] = root
            .try_into()
            .map_err(|_| malformed(Syntax::RootLength, 3))?;
        Ok(Self {
            origin,
            head: TreeHead {
                size,
                root: Hash::from_bytes(root),
            },
            extensions: extensions.iter().map(|line| (*line).to_owned()).collect(),
        })
    }
}

impl fmt::Display for Checkpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

/// The 1-based line number of a 0-based index.
fn line_number(index: usize) -> u32 {
    u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX)
}

/// A canonical ASCII decimal (no sign, no leading zero but `0`) that fits
/// a `u64`.
pub(crate) fn parse_decimal(text: &str) -> Option<u64> {
    let canonical = match text.as_bytes() {
        [b'0'] => true,
        [first, rest @ ..] => (b'1'..=b'9').contains(first) && rest.iter().all(u8::is_ascii_digit),
        [] => false,
    };
    if canonical { text.parse().ok() } else { None }
}

/// A checkpoint and its signatures, kept byte for byte: what a log
/// publishes, a witness cosigns and an auditor verifies.
///
/// ```
/// use doubleentry::transparency::SignedCheckpoint;
///
/// let bytes = "example.com/ledgers/acme\n13\nqO9IRMjh1bpJyBHNuG6VeR9dMsp9lwmv2ij99l6UmlM=\n\n\
///     \u{2014} example.com/ledgers/acme w7bZlC5mfA06DpTi62mP0qMuD1FJW9mMCKfYVVpv/OlJLW9mUhkt2ZRdTgmGbyZqDPglkLsNrAPqKjiyB+w+8Jkh1gA=\n";
/// let signed = SignedCheckpoint::parse(bytes.as_bytes())?;
/// assert_eq!(signed.checkpoint().head().size, 13);
/// assert_eq!(signed.to_bytes(), bytes.as_bytes());
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SignedCheckpoint {
    checkpoint: Checkpoint,
    note: Note,
}

impl SignedCheckpoint {
    /// Parses a signed checkpoint: a strict signed note whose text is a
    /// checkpoint. Signatures are not checked here; see
    /// [`verify`](Self::verify) (feature `witness`).
    ///
    /// # Errors
    ///
    /// The errors of [`Note::parse`] and [`Checkpoint::parse_text`].
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let note = Note::parse(bytes)?;
        let checkpoint = Checkpoint::parse_text(note.text())?;
        Ok(Self { checkpoint, note })
    }

    /// A checkpoint with its signature lines.
    #[cfg(feature = "witness")]
    pub(crate) fn from_parts(
        checkpoint: Checkpoint,
        signatures: Vec<NoteSignature>,
    ) -> Result<Self, Error> {
        let note = Note::new(checkpoint.text(), signatures)?;
        Ok(Self { checkpoint, note })
    }

    /// The checkpoint.
    #[must_use]
    pub const fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    /// The signature lines, in order.
    #[must_use]
    pub fn signatures(&self) -> &[NoteSignature] {
        self.note.signatures()
    }

    /// The note: the checkpoint text and its signatures.
    #[must_use]
    pub const fn note(&self) -> &Note {
        &self.note
    }

    /// The bytes: identical to the parsed input.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.note.to_bytes()
    }
}

impl fmt::Display for SignedCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.note, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_are_canonical() {
        for (text, value) in [
            ("0", Some(0)),
            ("7", Some(7)),
            ("18446744073709551615", Some(u64::MAX)),
        ] {
            assert_eq!(parse_decimal(text), value, "{text}");
        }
        for bad in [
            "",
            "00",
            "07",
            "+7",
            "-1",
            " 7",
            "7 ",
            "18446744073709551616",
            "1e3",
            "٣",
        ] {
            assert_eq!(parse_decimal(bad), None, "{bad}");
        }
    }

    #[test]
    fn checkpoint_text_rules() {
        let root = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        assert!(Checkpoint::parse_text(&format!("o\n0\n{root}\n")).is_ok());
        let with_extension = format!("o\n0\n{root}\next one\n");
        assert_eq!(
            Checkpoint::parse_text(&with_extension).unwrap().text(),
            with_extension
        );
        for (text, what) in [
            (format!("o\n0\n{root}"), Syntax::MissingNewline),
            (format!("o\n0\n{root}\n\n"), Syntax::EmptyLine),
            ("o\n0\n".to_owned(), Syntax::EmptyLine),
            (format!("o\n00\n{root}\n"), Syntax::Decimal),
            ("o\n0\nAAAA\n".to_owned(), Syntax::RootLength),
            (format!("o\n0\n{root}x\n"), Syntax::Base64),
            (format!("o\n0\n{root}\n\t\n"), Syntax::ControlChar),
        ] {
            assert!(
                matches!(Checkpoint::parse_text(&text), Err(Error::Malformed { what: w, .. }) if w == what),
                "{text:?}"
            );
        }
        assert_eq!(
            Checkpoint::parse_text(&format!("a b\n0\n{root}\n")),
            Err(Error::BadName)
        );
    }
}
