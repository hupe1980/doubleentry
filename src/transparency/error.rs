//! [`Error`]: every refusal of a transparency artefact, and the witness
//! outcomes it carries.

use core::fmt;

use super::checkpoint::SignedCheckpoint;
use super::note::KeyName;
use super::signing::KeyType;
use super::tiles::TileId;
use crate::log::{ConsistencyProof, ProofError, TreeHead};
use crate::model::{DecodeError, RecordKind, SealOrderError};

/// Why a note, checkpoint, key, policy, request, reply, bundle, tile or
/// verifier step was refused.
///
/// One variant per cause, so a caller (and a test) matches the variant and
/// never the message. It folds into [`crate::Error::Transparency`] with `?`.
///
/// ```
/// use doubleentry::transparency::{Error, Note, Syntax};
///
/// let refused = Note::parse(b"text\n\n\xe2\x80\x94 key AAAAAAB=\n");
/// assert!(matches!(refused, Err(Error::Malformed { what: Syntax::Base64, line: 3 })));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    // ── Text ────────────────────────────────────────────────────────────────
    /// The text breaks a rule of its format at a line (1-based; 0 when the
    /// input is not line-based, such as a tile path).
    #[error("malformed {what} at line {line}")]
    Malformed {
        /// Which rule.
        what: Syntax,
        /// The line it was broken on.
        line: u32,
    },
    /// The input is longer than its format allows.
    #[error("the input exceeds {limit} bytes")]
    TooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// A note with more than 100 signature lines.
    #[error("a note carries more than 100 signatures")]
    TooManySignatures,
    /// Two signature lines with the same key name and key ID.
    #[error("two signatures by {name} with key ID {}", hex(*.key_id))]
    DuplicateSignature {
        /// The key name.
        name: KeyName,
        /// The key ID.
        key_id: [u8; 4],
    },
    /// A key name or origin that is empty, longer than 255 bytes, or holds a
    /// Unicode space, `+` or a control character.
    #[error("not a valid key name or origin")]
    BadName,
    /// More than 63 proof hashes in a request or bundle.
    #[error("more than 63 proof hashes")]
    TooManyProofLines,

    // ── Keys ────────────────────────────────────────────────────────────────
    /// A verifier key of a signature type this crate does not verify.
    #[error("signature type 0x{0:02x} is not supported")]
    UnsupportedKeyType(u8),
    /// A key of the wrong type for its role: a log key where a witness key
    /// is needed, or the reverse.
    #[error("a {found} key where a {expected} key is needed")]
    WrongKeyType {
        /// The type the role needs.
        expected: KeyType,
        /// The type given.
        found: KeyType,
    },
    /// A verifier key whose key ID is not `SHA-256(name ‖ 0x0A ‖ type ‖ key)[:4]`.
    #[error("the key ID does not match the key")]
    KeyIdMismatch,
    /// An Ed25519 public key that does not decode or has small order.
    #[error("the Ed25519 key does not decode or has small order")]
    WeakKey,
    /// Two different keys with the same key name and key ID in one set of
    /// trusted keys (a policy, a verifier, a key list): a signature line
    /// could be checked against either, so the set is refused rather than
    /// the first key taken (Go's `VerifierList` reports the same ambiguity).
    #[error("two keys named {name} share the key ID {}", hex(*.key_id))]
    AmbiguousKey {
        /// The key name.
        name: KeyName,
        /// The shared key ID.
        key_id: [u8; 4],
    },
    /// A log key asked to sign a checkpoint of another origin.
    #[error("the key {key} cannot sign a checkpoint of {origin}")]
    KeyNameMismatch {
        /// The key's name.
        key: KeyName,
        /// The checkpoint's origin.
        origin: KeyName,
    },

    // ── Signatures and policy ───────────────────────────────────────────────
    /// No signature from a trusted key verifies.
    #[error("no signature from a trusted key")]
    NoTrustedSignature,
    /// A signature from a trusted key does not verify: the whole note is
    /// refused.
    #[error("the signature by {name} ({}) does not verify", hex(*.key_id))]
    BadSignature {
        /// The key name.
        name: KeyName,
        /// The key ID.
        key_id: [u8; 4],
    },
    /// A checkpoint of an origin the verifier has no log key for.
    #[error("no log key is known for the origin {0}")]
    UnknownOrigin(KeyName),
    /// No log whose key name is the checkpoint's origin signed it.
    #[error("the checkpoint's origin is not the name of a log that signed it")]
    OriginNotSigned,
    /// The policy's quorum is not met by the verified cosignatures.
    #[error("the quorum {quorum} is not met")]
    QuorumNotMet {
        /// The quorum's name in the policy.
        quorum: String,
    },
    /// A policy line breaks a rule of `tlog-policy`.
    #[error("policy line {line}: {rule}")]
    PolicyRule {
        /// The line (1-based; 0 for a policy built in code).
        line: u32,
        /// The rule.
        rule: PolicyRule,
    },

    // ── Proofs and the verifier ─────────────────────────────────────────────
    /// A size-0 checkpoint whose root is not the empty-tree root.
    #[error("a checkpoint of size 0 must carry the empty-tree root")]
    EmptyTreeRoot,
    /// A proof of the wrong shape for its sizes: a proof where none is
    /// allowed, none where one is needed, endpoints that do not match, an
    /// old size above the new one, or an index not below the size.
    #[error("the proof does not fit its sizes")]
    ProofShape,
    /// A bundle whose record is not of the kind it claims.
    #[error("expected a {expected:?} record, found {found:?}")]
    WrongRecordKind {
        /// The kind needed.
        expected: RecordKind,
        /// The kind found.
        found: RecordKind,
    },
    /// An inclusion, consistency or sealed-balance proof that does not
    /// verify. A consistency proof that does not recompute the offered
    /// root is this, never [`Error::Inconsistent`]: anyone on the path can
    /// corrupt a proof, so a bad one proves nothing about the log.
    #[error("proof: {0}")]
    Proof(#[source] ProofError),
    /// Proven bytes that are not a canonical record.
    #[error("record: {0}")]
    Decode(#[source] DecodeError),
    /// A seal that does not follow its predecessor.
    #[error("seal: {0}")]
    SealOrder(#[source] SealOrderError),
    /// A bundle against another head than the verifier trusts: advance the
    /// verifier first, or ask for a bundle at the trusted head.
    #[error("the bundle is at size {}, the trusted head at {}", .offered.size, trusted_size(.trusted.as_ref()))]
    NotAtTrustedHead {
        /// The trusted head (`None`: no head yet).
        trusted: Option<TreeHead>,
        /// The bundle's head.
        offered: TreeHead,
    },
    /// A checkpoint smaller than the trusted head.
    #[error("a checkpoint of size {} is older than the trusted size {}", .offered.size, .trusted.size)]
    Rollback {
        /// The trusted head.
        trusted: TreeHead,
        /// The head offered.
        offered: TreeHead,
    },
    /// A validly signed checkpoint of the trusted size with another root:
    /// the log showed two histories.
    #[error("fork: two roots at size {}", .0.trusted_head.size)]
    Fork(Box<ForkEvidence>),
    /// A validly signed larger checkpoint whose consistency proof
    /// recomputes the offered root exactly but, for the trusted size, a
    /// root other than the trusted one: the offered tree does not extend
    /// the trusted tree. A proof that does not recompute the offered root
    /// is [`Error::Proof`] instead, and a missing one [`Error::ProofShape`].
    #[error("inconsistent: the tree of size {} does not extend the trusted one", .0.offered.checkpoint().head().size)]
    Inconsistent(Box<ForkEvidence>),

    // ── Witnesses ───────────────────────────────────────────────────────────
    /// A witness answered with something other than a cosignature.
    #[error("witness: {0}")]
    Witness(#[source] WitnessError),

    // ── Tiles and checkpoints of a ledger ───────────────────────────────────
    /// A tile level above 63, or a width outside `1..=256`.
    #[error("no tile has this level or width")]
    InvalidTile,
    /// A tile of the wrong kind: an entry bundle where a hash tile is asked
    /// for, or the reverse.
    #[error("{tile} is not the kind of tile asked for")]
    WrongTileKind {
        /// The tile.
        tile: TileId,
    },
    /// A tile whose last hash lies beyond a tree of `size` leaves.
    #[error("{tile} lies beyond a tree of {size} leaves")]
    TileOutOfRange {
        /// The tile.
        tile: TileId,
        /// The tree size.
        size: u64,
    },
    /// A leaf longer than an entry bundle's 16-bit length prefix allows.
    #[error("the leaf at {position} has {len} bytes, more than 65535")]
    LeafTooLarge {
        /// The leaf's position.
        position: u64,
        /// Its length.
        len: usize,
    },
    /// A checkpoint of another log than this ledger.
    #[error("the checkpoint is of {origin}, not of this ledger")]
    ForeignCheckpoint {
        /// The checkpoint's origin.
        origin: KeyName,
    },
    /// A checkpoint of this ledger at a size it has not reached, or whose
    /// root differs from the ledger's root at that size.
    #[error("the checkpoint is not a head of this ledger")]
    CheckpointMismatch,
}

/// A head's size for a message, or `none`.
fn trusted_size(head: Option<&TreeHead>) -> String {
    head.map_or_else(|| String::from("none"), |head| head.size.to_string())
}

/// Eight lowercase hex digits: a key ID as vkeys write it.
pub(crate) fn hex(key_id: [u8; 4]) -> String {
    format!("{:08x}", u32::from_be_bytes(key_id))
}

/// Which rule of a text format was broken ([`Error::Malformed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Syntax {
    /// Bytes that are not UTF-8.
    Utf8,
    /// A control character other than newline.
    ControlChar,
    /// No blank line separates the text from the signatures.
    MissingSeparator,
    /// The input does not end in a newline.
    MissingNewline,
    /// No signature line after the blank line.
    NoSignature,
    /// A signature line that is not `— <name> <base64>`, or whose base64
    /// decodes to fewer than five bytes (key ID and signature).
    SignatureLine,
    /// Base64 that is not canonical RFC 4648 with padding.
    Base64,
    /// A number that is not a canonical ASCII decimal fitting 64 bits.
    Decimal,
    /// A hash that is not 32 bytes.
    RootLength,
    /// An empty line where a non-empty one is required.
    EmptyLine,
    /// A verifier key that is not `<name>+<8 hex>+<base64>`, or whose key
    /// has the wrong length.
    Vkey,
    /// A first line that is not the format's header (`old`, `index`, the
    /// tlog-proof header).
    Header,
    /// Bytes after the end of the format.
    TrailingData,
    /// A policy line that is not one of the four definitions.
    PolicyLine,
    /// A tile path that is not `tile/<L>/<N>[.p/<W>]` or
    /// `tile/entries/<N>[.p/<W>]`.
    TilePath,
    /// A bundle's `extra` data that is neither a record nor the balance-proof
    /// layout.
    ExtraLayout,
}

impl fmt::Display for Syntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Utf8 => "utf-8",
            Self::ControlChar => "control character",
            Self::MissingSeparator => "note (no blank line before the signatures)",
            Self::MissingNewline => "final newline",
            Self::NoSignature => "note (no signature line)",
            Self::SignatureLine => "signature line",
            Self::Base64 => "base64",
            Self::Decimal => "decimal",
            Self::RootLength => "hash length",
            Self::EmptyLine => "empty line",
            Self::Vkey => "verifier key",
            Self::Header => "header line",
            Self::TrailingData => "trailing data",
            Self::PolicyLine => "policy line",
            Self::TilePath => "tile path",
            Self::ExtraLayout => "extra data",
        })
    }
}

/// Which `tlog-policy` rule a policy breaks ([`Error::PolicyRule`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PolicyRule {
    /// An octet other than tab, newline, `0x20–0x7E` or `0x80–0xFF`.
    Charset,
    /// A line that does not start with `log`, `witness`, `group` or
    /// `quorum`, or has the wrong number of items.
    Line,
    /// `none` used as a witness or group name, or as a group member.
    ReservedName,
    /// A name given to [`PolicyBuilder`](super::PolicyBuilder) that is not
    /// one policy item: empty, or holding a space, tab, newline or other
    /// control character.
    Name,
    /// A witness or group name defined twice.
    DuplicateName,
    /// Two logs, or two witnesses, with the same public key.
    DuplicateKey,
    /// A witness whose Ed25519 public key is also a log's (under another
    /// signature type): the log could meet its own witness quorum.
    WitnessIsLog,
    /// Two different keys with the same key name and key ID: a signature
    /// line would be ambiguous.
    AmbiguousKey,
    /// A log key that is not `0x01`/`0x04`, or a witness key that is not
    /// `0x04`/`0x06`.
    KeyType,
    /// A threshold that is not `all`, `any` or a decimal `k` with
    /// `1 ≤ k ≤ n`.
    Threshold,
    /// A name used before its definition.
    Undefined,
    /// A name listed as a group member more than once.
    MemberTwice,
    /// No `quorum` line, or more than one.
    QuorumCount,
    /// More than 256 logs, witnesses or groups.
    TooMany,
}

impl fmt::Display for PolicyRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Charset => "an octet outside the policy character set",
            Self::Line => "not a log, witness, group or quorum definition",
            Self::ReservedName => "the name none is reserved",
            Self::Name => "a name is empty or holds whitespace or a control character",
            Self::DuplicateName => "a name is defined twice",
            Self::DuplicateKey => "a public key is listed twice",
            Self::WitnessIsLog => "a witness key is also a log key",
            Self::AmbiguousKey => "two keys share a key name and key ID",
            Self::KeyType => "a key of the wrong signature type",
            Self::Threshold => "the threshold is not all, any or 1 to n",
            Self::Undefined => "a name is used before its definition",
            Self::MemberTwice => "a name is a group member twice",
            Self::QuorumCount => "a policy needs exactly one quorum line",
            Self::TooMany => "more than 256 logs, witnesses or groups",
        })
    }
}

/// How a witness answered other than with a cosignature
/// ([`Error::Witness`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WitnessError {
    /// 409: the witness holds another size for the log; ask again from it.
    #[error("the witness holds size {size}")]
    Conflict {
        /// The witness's size.
        size: u64,
    },
    /// 400: the request was malformed or its old size above the checkpoint.
    #[error("400 bad request")]
    BadRequest,
    /// 403: no log signature the witness trusts verifies.
    #[error("403 the witness does not accept the log signature")]
    Forbidden,
    /// 404: the witness does not know the log.
    #[error("404 the witness does not know the log")]
    UnknownLog,
    /// 422: the consistency proof or the root was refused.
    #[error("422 the witness refused the proof or root")]
    Unprocessable,
    /// 429: the witness asks to be called again later.
    #[error("429 push-back, retry later")]
    Pushback,
    /// Any other status.
    #[error("unexpected status {status}")]
    Unexpected {
        /// The status.
        status: u16,
    },
    /// A reply that does not follow `tlog-witness`.
    #[error("malformed reply")]
    MalformedReply,
    /// A 200 without a line from the witness key.
    #[error("no cosignature from {name}")]
    NoCosignature {
        /// The witness key's name.
        name: KeyName,
    },
    /// A 200 whose line from the witness key does not verify.
    #[error("the cosignature by {name} ({}) does not verify", hex(*.key_id))]
    BadCosignature {
        /// The witness key's name.
        name: KeyName,
        /// The witness key's ID.
        key_id: [u8; 4],
    },
    /// The witness has seen a larger tree than the checkpoint: it cannot
    /// cosign this checkpoint, ever.
    #[error("the witness holds size {witness}, beyond the checkpoint's {checkpoint}")]
    WitnessAhead {
        /// The witness's size.
        witness: u64,
        /// The checkpoint's size.
        checkpoint: u64,
    },
    /// A 409 naming a smaller size than an earlier 409 of the same session:
    /// the witness went backwards, which no honest witness does.
    #[error("the witness went back from size {previous} to {size}")]
    WitnessRegressed {
        /// The largest size it reported before.
        previous: u64,
        /// The smaller size it reports now.
        size: u64,
    },
    /// Three requests ended in 409.
    #[error("three conflicts, at sizes {sizes:?}")]
    TooManyConflicts {
        /// The sizes the witness reported.
        sizes: Vec<u64>,
    },
    /// The session already ended.
    #[error("the session already ended")]
    SessionFinished,
}

/// The evidence of a fork ([`Error::Fork`]) or an inconsistency
/// ([`Error::Inconsistent`]): both signed checkpoints and the proof that was
/// offered.
///
/// Both notes are kept byte for byte. What a third party can check on its
/// own, given `trusted` (a pinned head has no note and so is no evidence):
///
/// - **Fork**: both checkpoints verify under the log key, have the same
///   size and different roots.
/// - **Inconsistent**: both checkpoints verify under the log key, and the
///   RFC 9162 consistency check of `proof` recomputes `offered`'s root
///   exactly but a root other than `trusted`'s for the smaller size. Every
///   proof hash feeds the offered root, so (SHA-256 being collision
///   resistant) only the offered tree's real nodes do that: its prefix is
///   not the trusted tree.
///
/// A proof that fails any other way (wrong shape, or not recomputing the
/// offered root) is never evidence; the verifier reports it as
/// [`Error::Proof`] or [`Error::ProofShape`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForkEvidence {
    /// The head the verifier trusted.
    pub trusted_head: TreeHead,
    /// The signed checkpoint behind the trusted head; `None` while the head
    /// is a pin or a restored head that came without its note.
    pub trusted: Option<SignedCheckpoint>,
    /// The checkpoint offered.
    pub offered: SignedCheckpoint,
    /// The consistency proof offered with it.
    pub proof: Option<ConsistencyProof>,
}

impl From<WitnessError> for Error {
    fn from(error: WitnessError) -> Self {
        Self::Witness(error)
    }
}

impl From<ProofError> for Error {
    fn from(error: ProofError) -> Self {
        Self::Proof(error)
    }
}

impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<SealOrderError> for Error {
    fn from(error: SealOrderError) -> Self {
        Self::SealOrder(error)
    }
}
