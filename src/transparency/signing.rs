//! Verifier keys ([`Vkey`]), cosignatures ([`Cosignature`]) and — with the
//! `witness` feature — the ledger's Ed25519 [`LogKey`] and every signature
//! check.
//!
//! Every Ed25519 verification is strict (`verify_strict`): a signature whose
//! `S` is not reduced, or whose `R` or public key `A` has small order, is
//! refused, and a public key that does not decode or has small order is
//! refused when the vkey is parsed.

use core::fmt;
use core::str::FromStr;

use sha2::{Digest, Sha256};

use super::Error;
use super::base64;
use super::error::Syntax;
use super::note::{KeyName, NoteSignature};

/// The signature type of a [`Vkey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum KeyType {
    /// `0x01`: an Ed25519 note signature over the checkpoint text — what a
    /// log signs with.
    Ed25519,
    /// `0x04`: a timestamped Ed25519 `cosignature/v1` — what a witness
    /// signs with.
    CosignatureEd25519,
}

impl KeyType {
    /// The signature type byte.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Ed25519 => 0x01,
            Self::CosignatureEd25519 => 0x04,
        }
    }

    /// The type of a signature type byte this crate verifies.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedKeyType`] naming any other byte.
    pub const fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            0x01 => Ok(Self::Ed25519),
            0x04 => Ok(Self::CosignatureEd25519),
            other => Err(Error::UnsupportedKeyType(other)),
        }
    }
}

impl fmt::Display for KeyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ed25519 => "Ed25519 (0x01)",
            Self::CosignatureEd25519 => "Ed25519 cosignature (0x04)",
        })
    }
}

/// `SHA-256(name ‖ 0x0A ‖ type ‖ key)[:4]`: the key ID signed-note
/// recommends and every C2SP signature type uses.
pub(crate) fn key_id(name: &str, key_type: u8, key: &[u8]) -> [u8; 4] {
    let digest: [u8; 32] = Sha256::new()
        .chain_update(name.as_bytes())
        .chain_update([0x0a, key_type])
        .chain_update(key)
        .finalize()
        .into();
    let [a, b, c, d, ..] = digest;
    [a, b, c, d]
}

/// A verifier key: `<name>+<8 hex key ID>+<base64(type ‖ key)>`.
///
/// Types `0x01` (a log's Ed25519 key) and `0x04` (a witness's Ed25519
/// cosignature key) are parsed; the key ID must be the one derived from the
/// name, type and key. With the `witness` feature a key that does not decode
/// or has small order is refused too.
///
/// ```
/// use doubleentry::transparency::{KeyType, Vkey};
///
/// let vkey: Vkey = "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs".parse()?;
/// assert_eq!(vkey.name().as_str(), "example.com/ledgers/acme");
/// assert_eq!(vkey.key_type(), KeyType::Ed25519);
/// assert_eq!(vkey.key_id(), [0xc3, 0xb6, 0xd9, 0x94]);
/// assert_eq!(vkey.to_string().parse::<Vkey>()?, vkey);
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Vkey {
    name: KeyName,
    key_id: [u8; 4],
    key_type: KeyType,
    key: [u8; 32],
}

impl Vkey {
    /// The key of `key_type` with these 32 bytes, named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::WeakKey`] (feature `witness`) for a key that does not
    /// decode or has small order.
    pub fn new(name: KeyName, key_type: KeyType, key: [u8; 32]) -> Result<Self, Error> {
        check_point(&key)?;
        Ok(Self {
            key_id: key_id(name.as_str(), key_type.byte(), &key),
            name,
            key_type,
            key,
        })
    }

    /// The key name.
    #[must_use]
    pub const fn name(&self) -> &KeyName {
        &self.name
    }

    /// The key ID.
    #[must_use]
    pub const fn key_id(&self) -> [u8; 4] {
        self.key_id
    }

    /// The signature type.
    #[must_use]
    pub const fn key_type(&self) -> KeyType {
        self.key_type
    }

    /// The 32-byte Ed25519 public key.
    #[must_use]
    pub const fn public_key(&self) -> [u8; 32] {
        self.key
    }

    /// The public key, borrowed.
    pub(crate) const fn key_bytes(&self) -> &[u8] {
        &self.key
    }

    /// Whether a signature line is this key's: same name and key ID.
    #[cfg(feature = "witness")]
    pub(crate) fn signed(&self, signature: &NoteSignature) -> bool {
        signature.name() == &self.name && signature.key_id() == self.key_id
    }

    /// The parts of vkey text: name, key ID, type byte and key bytes.
    pub(crate) fn split(text: &str) -> Result<(KeyName, [u8; 4], u8, Vec<u8>), Error> {
        let malformed = Error::Malformed {
            what: Syntax::Vkey,
            line: 1,
        };
        let mut parts = text.splitn(3, '+');
        let (Some(name), Some(id), Some(encoded)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed);
        };
        let name = KeyName::new(name)?;
        let id = parse_key_id(id).ok_or(malformed.clone())?;
        let raw = base64::decode(encoded).ok_or(Error::Malformed {
            what: Syntax::Base64,
            line: 1,
        })?;
        let (&key_type, key) = raw.split_first().ok_or(malformed)?;
        Ok((name, id, key_type, key.to_vec()))
    }
}

/// Eight lowercase hex digits as four bytes.
fn parse_key_id(text: &str) -> Option<[u8; 4]> {
    let canonical = text.len() == 8
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    canonical
        .then(|| u32::from_str_radix(text, 16).ok())
        .flatten()
        .map(u32::to_be_bytes)
}

impl FromStr for Vkey {
    type Err = Error;

    /// Parses vkey text.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] for the shape, [`Error::BadName`],
    /// [`Error::UnsupportedKeyType`] for a type other than `0x01`/`0x04`,
    /// [`Error::KeyIdMismatch`], or [`Error::WeakKey`] (feature `witness`).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (name, id, key_type, key) = Self::split(text)?;
        let key_type = KeyType::from_byte(key_type)?;
        let key: [u8; 32] = key.try_into().map_err(|_| Error::Malformed {
            what: Syntax::Vkey,
            line: 1,
        })?;
        let vkey = Self::new(name, key_type, key)?;
        if vkey.key_id == id {
            Ok(vkey)
        } else {
            Err(Error::KeyIdMismatch)
        }
    }
}

impl fmt::Display for Vkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut raw = Vec::with_capacity(33);
        raw.push(self.key_type.byte());
        raw.extend_from_slice(&self.key);
        write!(
            f,
            "{}+{}+{}",
            self.name,
            super::error::hex(self.key_id),
            base64::encode(&raw)
        )
    }
}

/// A witness's timestamped `cosignature/v1` over a checkpoint: an Ed25519
/// signature over `cosignature/v1\ntime <timestamp>\n` followed by the
/// checkpoint text, extension lines included.
///
/// The timestamp is returned, never judged: this crate has no clock, so
/// refusing a cosignature from the future is the caller's policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cosignature {
    name: KeyName,
    key_id: [u8; 4],
    timestamp: u64,
    signature: [u8; 64],
}

impl Cosignature {
    /// The witness's key name.
    #[must_use]
    pub const fn name(&self) -> &KeyName {
        &self.name
    }

    /// The witness's key ID.
    #[must_use]
    pub const fn key_id(&self) -> [u8; 4] {
        self.key_id
    }

    /// The witness's POSIX timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// The signature line, newline included.
    #[must_use]
    pub fn to_line(&self) -> String {
        self.note_signature().to_line()
    }

    /// As a note signature: key ID, then timestamp (big-endian) and the
    /// 64-byte signature.
    pub(crate) fn note_signature(&self) -> NoteSignature {
        let mut raw = Vec::with_capacity(72);
        raw.extend_from_slice(&self.timestamp.to_be_bytes());
        raw.extend_from_slice(&self.signature);
        NoteSignature::new(self.name.clone(), self.key_id, raw)
    }

    /// The parts of a cosignature line's signature: `timestamp ‖ signature`,
    /// exactly 72 bytes.
    #[cfg(feature = "witness")]
    fn from_signature(signature: &NoteSignature) -> Option<Self> {
        let (timestamp, rest) = signature.signature().split_first_chunk::<8>()?;
        let signature_bytes: [u8; 64] = rest.try_into().ok()?;
        Some(Self {
            name: signature.name().clone(),
            key_id: signature.key_id(),
            timestamp: u64::from_be_bytes(*timestamp),
            signature: signature_bytes,
        })
    }
}

/// The message a `cosignature/v1` signs.
#[cfg(feature = "witness")]
fn cosigned_message(timestamp: u64, text: &str) -> Vec<u8> {
    format!("cosignature/v1\ntime {timestamp}\n{text}").into_bytes()
}

/// One verified signature of a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verified {
    /// A log's Ed25519 (`0x01`) signature.
    Log {
        /// The key name.
        name: KeyName,
        /// The key ID.
        key_id: [u8; 4],
    },
    /// A cosigner's `cosignature/v1` (`0x04`), with its verified timestamp.
    Cosigner(Cosignature),
}

impl Verified {
    /// The key name of the signer.
    #[must_use]
    pub const fn name(&self) -> &KeyName {
        match self {
            Self::Log { name, .. } => name,
            Self::Cosigner(cosignature) => &cosignature.name,
        }
    }
}

#[cfg(not(feature = "witness"))]
fn check_point(_key: &[u8; 32]) -> Result<(), Error> {
    Ok(())
}

#[cfg(feature = "witness")]
fn check_point(key: &[u8; 32]) -> Result<(), Error> {
    match ed25519_dalek::VerifyingKey::from_bytes(key) {
        Ok(key) if !key.is_weak() => Ok(()),
        _ => Err(Error::WeakKey),
    }
}

#[cfg(feature = "witness")]
pub use with_keys::LogKey;

#[cfg(feature = "witness")]
mod with_keys {
    use core::fmt;
    use std::collections::BTreeMap;

    use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

    use super::{
        Cosignature, Error, KeyName, KeyType, NoteSignature, Verified, Vkey, cosigned_message,
    };
    use crate::log::empty_root;
    use crate::model::LedgerId;
    use crate::transparency::checkpoint::{Checkpoint, SignedCheckpoint};
    use crate::transparency::note::Note;

    /// A ledger's Ed25519 signing key, named by its ledger id: it signs the
    /// ledger's checkpoints as C2SP `0x01` note signatures.
    ///
    /// Built from a caller-supplied 32-byte seed (from a KMS, for example);
    /// this crate never draws randomness. Signing is deterministic. `Debug`
    /// shows the verifier key only, and the secret is zeroed when dropped.
    ///
    /// ```
    /// use doubleentry::LedgerId;
    /// use doubleentry::transparency::LogKey;
    ///
    /// let key = LogKey::from_seed(&LedgerId::new("example.com/ledgers/acme")?, [7; 32]);
    /// assert_eq!(
    ///     key.vkey().to_string(),
    ///     "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs",
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub struct LogKey {
        name: KeyName,
        key: SigningKey,
    }

    impl LogKey {
        /// The key of `ledger` from `seed`.
        #[must_use]
        pub fn from_seed(ledger: &LedgerId, seed: [u8; 32]) -> Self {
            Self {
                name: KeyName::from(ledger),
                key: SigningKey::from_bytes(&seed),
            }
        }

        /// The verifier key (type `0x01`) to publish.
        #[must_use]
        pub fn vkey(&self) -> Vkey {
            vkey(&self.name, KeyType::Ed25519, &self.key.verifying_key())
        }

        /// Signs a checkpoint of this key's ledger.
        ///
        /// # Errors
        ///
        /// [`Error::KeyNameMismatch`] when the checkpoint's origin is not
        /// this key's name.
        pub fn sign(&self, checkpoint: &Checkpoint) -> Result<SignedCheckpoint, Error> {
            if checkpoint.origin() != &self.name {
                return Err(Error::KeyNameMismatch {
                    key: self.name.clone(),
                    origin: checkpoint.origin().clone(),
                });
            }
            let signature = self.key.sign(checkpoint.text().as_bytes());
            let line = NoteSignature::new(
                self.name.clone(),
                self.vkey().key_id(),
                signature.to_bytes().to_vec(),
            );
            SignedCheckpoint::from_parts(checkpoint.clone(), vec![line])
        }
    }

    impl fmt::Debug for LogKey {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("LogKey")
                .field("vkey", &self.vkey().to_string())
                .finish_non_exhaustive()
        }
    }

    /// The vkey of `key`, typed `key_type`, named `name`. A dalek key always
    /// decodes; the key ID is derived.
    pub(crate) fn vkey(name: &KeyName, key_type: KeyType, key: &VerifyingKey) -> Vkey {
        Vkey {
            key_id: super::key_id(name.as_str(), key_type.byte(), key.as_bytes()),
            name: name.clone(),
            key_type,
            key: key.to_bytes(),
        }
    }

    /// A cosigner's `cosignature/v1` over `checkpoint` at `timestamp`, by
    /// `key` named `name`.
    #[cfg(feature = "testing")]
    pub(crate) fn cosign(
        name: &KeyName,
        key: &SigningKey,
        timestamp: u64,
        checkpoint: &Checkpoint,
    ) -> Cosignature {
        let message = cosigned_message(timestamp, &checkpoint.text());
        Cosignature {
            name: name.clone(),
            key_id: super::key_id(
                name.as_str(),
                KeyType::CosignatureEd25519.byte(),
                key.verifying_key().as_bytes(),
            ),
            timestamp,
            signature: key.sign(&message).to_bytes(),
        }
    }

    /// Whether `signature` by `key` verifies over `message` (strictly).
    fn strict(key: &Vkey, message: &[u8], signature: &[u8; 64]) -> bool {
        VerifyingKey::from_bytes(&key.key).is_ok_and(|verifying| {
            verifying
                .verify_strict(message, &Signature::from_bytes(signature))
                .is_ok()
        })
    }

    /// Verifies one signature line against the key it names; the line must
    /// be that key's.
    pub(crate) fn verify_line(
        key: &Vkey,
        line: &NoteSignature,
        text: &str,
    ) -> Result<Verified, Error> {
        let bad = || Error::BadSignature {
            name: line.name().clone(),
            key_id: line.key_id(),
        };
        match key.key_type {
            KeyType::Ed25519 => {
                let signature: &[u8; 64] = line.signature().try_into().map_err(|_| bad())?;
                if strict(key, text.as_bytes(), signature) {
                    Ok(Verified::Log {
                        name: line.name().clone(),
                        key_id: line.key_id(),
                    })
                } else {
                    Err(bad())
                }
            }
            KeyType::CosignatureEd25519 => {
                let cosignature = Cosignature::from_signature(line).ok_or_else(bad)?;
                let message = cosigned_message(cosignature.timestamp, text);
                if strict(key, &message, &cosignature.signature) {
                    Ok(Verified::Cosigner(cosignature))
                } else {
                    Err(bad())
                }
            }
        }
    }

    /// Refuses two different keys with one (key name, key ID).
    pub(crate) fn unambiguous<'a>(keys: impl IntoIterator<Item = &'a Vkey>) -> Result<(), Error> {
        let mut seen: BTreeMap<(&KeyName, [u8; 4]), &Vkey> = BTreeMap::new();
        for key in keys {
            match seen.insert((key.name(), key.key_id()), key) {
                Some(other) if other != key => {
                    return Err(Error::AmbiguousKey {
                        name: key.name().clone(),
                        key_id: key.key_id(),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    impl Note {
        /// Verifies the signatures against `keys` as signed-note requires:
        /// lines from unknown keys are ignored, a failing line from a known
        /// key refuses the whole note, and at least one known key must
        /// verify. A `0x04` key verifies a `cosignature/v1` over the text.
        /// Two different keys of one (key name, key ID) are refused before
        /// any line is looked at: a line naming that pair would be
        /// ambiguous (the same key twice is one key).
        ///
        /// ```
        /// use doubleentry::transparency::{Note, Vkey};
        ///
        /// let note = Note::parse(
        ///     "This is an example message.\n\n\u{2014} example.com/foo \
        ///      Uw2QOkn8srV1yJGh2VYRlL1Tnagv1YEq6TfXppzi2ONncAlTgK7Ztg1ERYNZXsYjOBH3mFXmRKuwHjG1Yu72IneyaQM=\n"
        ///         .as_bytes(),
        /// )?;
        /// let key: Vkey = "example.com/foo+530d903a+AekyeRrm56hApGFkyQR4ZCbV54Id2LKaANYcrnKv3U2k".parse()?;
        /// assert_eq!(note.verify(&[key])?.len(), 1);
        /// # Ok::<(), doubleentry::transparency::Error>(())
        /// ```
        ///
        /// # Errors
        ///
        /// [`Error::AmbiguousKey`], [`Error::BadSignature`] or
        /// [`Error::NoTrustedSignature`].
        pub fn verify(&self, keys: &[Vkey]) -> Result<Vec<Verified>, Error> {
            unambiguous(keys)?;
            let verified = self
                .signatures()
                .iter()
                .filter_map(|line| {
                    keys.iter()
                        .find(|key| key.signed(line))
                        .map(|key| (key, line))
                })
                .map(|(key, line)| verify_line(key, line, self.text()))
                .collect::<Result<Vec<_>, Error>>()?;
            if verified.is_empty() {
                Err(Error::NoTrustedSignature)
            } else {
                Ok(verified)
            }
        }
    }

    impl SignedCheckpoint {
        /// Verifies the signatures against `keys` as [`Note::verify`] does;
        /// a size-0 checkpoint must also carry the empty-tree root.
        ///
        /// ```
        /// use doubleentry::transparency::{Checkpoint, KeyName, LogKey, Verified};
        /// use doubleentry::{LedgerId, TreeHead, log::empty_root};
        ///
        /// let ledger = LedgerId::new("example.com/ledgers/acme")?;
        /// let key = LogKey::from_seed(&ledger, [7; 32]);
        /// let head = TreeHead { size: 0, root: empty_root() };
        /// let signed = key.sign(&Checkpoint::new(KeyName::from(&ledger), head))?;
        /// let verified = signed.verify(&[key.vkey()])?;
        /// assert!(matches!(&verified[..], [Verified::Log { .. }]));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        ///
        /// # Errors
        ///
        /// [`Error::EmptyTreeRoot`], [`Error::AmbiguousKey`],
        /// [`Error::BadSignature`] or [`Error::NoTrustedSignature`].
        pub fn verify(&self, keys: &[Vkey]) -> Result<Vec<Verified>, Error> {
            let head = self.checkpoint().head();
            if head.size == 0 && head.root != empty_root() {
                return Err(Error::EmptyTreeRoot);
            }
            self.note().verify(keys)
        }

        /// The checkpoint with `cosignature` attached: one more signature
        /// line, or the line from the same key replaced in place. The log's
        /// signature stays first.
        ///
        /// # Errors
        ///
        /// [`Error::TooManySignatures`] when the note already has 100 lines.
        pub fn with_cosignature(&self, cosignature: &Cosignature) -> Result<Self, Error> {
            let line = cosignature.note_signature();
            let mut lines = self.signatures().to_vec();
            match lines.iter_mut().find(|seen| {
                (seen.name(), seen.key_id()) == (cosignature.name(), cosignature.key_id())
            }) {
                Some(seen) => *seen = line,
                None => lines.push(line),
            }
            Self::from_parts(self.checkpoint().clone(), lines)
        }
    }
}

#[cfg(all(feature = "witness", feature = "testing"))]
pub(crate) use with_keys::{cosign, vkey};
#[cfg(feature = "witness")]
pub(crate) use with_keys::{unambiguous, verify_line};
