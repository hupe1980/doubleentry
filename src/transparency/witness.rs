//! The log side of C2SP `tlog-witness` `add-checkpoint`: the request
//! ([`AddCheckpoint`]) and — with the `witness` feature — reading the reply
//! ([`WitnessReply`]) and the retry-on-409 loop ([`WitnessSession`]).
//!
//! Sans-I/O: the caller posts the bytes, waits, backs off and persists the
//! size each witness holds; nothing here has a clock, a socket or a thread.

use core::fmt;

use super::Error;
use super::base64;
use super::checkpoint::{SignedCheckpoint, parse_decimal};
use super::error::Syntax;
use super::note::MAX_NOTE_BYTES;
use crate::log::{ConsistencyProof, Hash, MAX_PROOF_HASHES};

/// The largest `add-checkpoint` body [`AddCheckpoint::parse`] reads: a note
/// of [`MAX_NOTE_BYTES`] plus 4 096 bytes for the `old` line, 63 proof
/// lines (45 bytes each) and the blank line.
pub const MAX_REQUEST_BYTES: usize = MAX_NOTE_BYTES + 4_096;

/// An `add-checkpoint` request body: `old <N>`, one base64 line per
/// consistency-proof hash, a blank line, the signed checkpoint verbatim.
///
/// ```
/// use doubleentry::transparency::{AddCheckpoint, SignedCheckpoint};
///
/// let note = "example.com/ledgers/acme\n13\nqO9IRMjh1bpJyBHNuG6VeR9dMsp9lwmv2ij99l6UmlM=\n\n\
///     \u{2014} example.com/ledgers/acme w7bZlC5mfA06DpTi62mP0qMuD1FJW9mMCKfYVVpv/OlJLW9mUhkt2ZRdTgmGbyZqDPglkLsNrAPqKjiyB+w+8Jkh1gA=\n";
/// let signed = SignedCheckpoint::parse(note.as_bytes())?;
/// let first_contact = AddCheckpoint::new(&signed, 0, None)?;
/// assert_eq!(first_contact.to_bytes(), [b"old 0\n\n".as_slice(), note.as_bytes()].concat());
/// assert_eq!(AddCheckpoint::parse(&first_contact.to_bytes())?, first_contact);
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddCheckpoint {
    old: u64,
    proof: Vec<Hash>,
    checkpoint: SignedCheckpoint,
}

impl AddCheckpoint {
    /// The request that moves a witness holding `old` to `checkpoint`.
    ///
    /// `proof` is the consistency proof from `old` to the checkpoint's size;
    /// none is sent (and none may be given) when `old` is 0 or equals the
    /// size.
    ///
    /// # Errors
    ///
    /// [`Error::ProofShape`] for `old` above the size, a proof where none is
    /// sent, none where one is needed, or a proof between other sizes;
    /// [`Error::TooManyProofLines`] above 63 hashes.
    pub fn new(
        checkpoint: &SignedCheckpoint,
        old: u64,
        proof: Option<&ConsistencyProof>,
    ) -> Result<Self, Error> {
        let size = checkpoint.checkpoint().head().size;
        let path = match (needs_proof(old, size)?, proof) {
            (false, None) => Vec::new(),
            (false, Some(proof)) if proof.path.is_empty() => Vec::new(),
            (true, Some(proof)) if (proof.old_size, proof.new_size) == (old, size) => {
                proof.path.clone()
            }
            _ => return Err(Error::ProofShape),
        };
        if path.len() > MAX_PROOF_HASHES {
            return Err(Error::TooManyProofLines);
        }
        Ok(Self {
            old,
            proof: path,
            checkpoint: checkpoint.clone(),
        })
    }

    /// Parses a request body as a witness receives it: the inverse of
    /// [`to_bytes`](Self::to_bytes) for the syntax only. Whether `old` and
    /// the proof fit the checkpoint is the witness's to decide (400 or 422),
    /// so it is not checked here.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] above [`MAX_REQUEST_BYTES`], before anything is
    /// read; [`Error::Malformed`], [`Error::TooManyProofLines`] or the
    /// errors of [`SignedCheckpoint::parse`].
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Error::TooLarge {
                limit: MAX_REQUEST_BYTES,
            });
        }
        let mut lines = Lines::new(bytes);
        let old = lines
            .next_line()?
            .strip_prefix("old ")
            .and_then(parse_decimal)
            .ok_or(Error::Malformed {
                what: Syntax::Header,
                line: 1,
            })?;
        let proof = lines.hashes()?;
        let checkpoint = SignedCheckpoint::parse(lines.rest())?;
        Ok(Self {
            old,
            proof,
            checkpoint,
        })
    }

    /// The witness's size this request starts from.
    #[must_use]
    pub const fn old(&self) -> u64 {
        self.old
    }

    /// The consistency-proof hashes.
    #[must_use]
    pub fn proof(&self) -> &[Hash] {
        &self.proof
    }

    /// The signed checkpoint.
    #[must_use]
    pub const fn checkpoint(&self) -> &SignedCheckpoint {
        &self.checkpoint
    }

    /// The request body.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = format!("old {}\n", self.old).into_bytes();
        for hash in &self.proof {
            body.extend_from_slice(base64::encode(hash.as_bytes()).as_bytes());
            body.push(b'\n');
        }
        body.push(b'\n');
        body.extend_from_slice(&self.checkpoint.to_bytes());
        body
    }
}

/// Whether a request from `old` to `size` carries a proof.
fn needs_proof(old: u64, size: u64) -> Result<bool, Error> {
    if old > size {
        Err(Error::ProofShape)
    } else {
        Ok(old != 0 && old != size)
    }
}

/// A cursor over newline-terminated lines, for the line-based formats
/// that end in a signed note (requests, bundles).
pub(crate) struct Lines<'a> {
    bytes: &'a [u8],
    line: u32,
}

impl<'a> Lines<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, line: 0 }
    }

    /// The 1-based number of the line read last.
    pub(crate) const fn line(&self) -> u32 {
        self.line
    }

    /// The next line, without its newline.
    pub(crate) fn next_line(&mut self) -> Result<&'a str, Error> {
        self.line = self.line.saturating_add(1);
        let malformed = |what| Error::Malformed {
            what,
            line: self.line,
        };
        let end = self
            .bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(malformed(Syntax::MissingNewline))?;
        let (line, rest) = self.bytes.split_at(end);
        self.bytes = rest.get(1..).unwrap_or_default();
        let line = core::str::from_utf8(line).map_err(|_| malformed(Syntax::Utf8))?;
        if line.bytes().any(|byte| byte < 0x20) {
            return Err(malformed(Syntax::ControlChar));
        }
        Ok(line)
    }

    /// Hash lines up to and including the blank line that ends them.
    pub(crate) fn hashes(&mut self) -> Result<Vec<Hash>, Error> {
        let mut hashes = Vec::new();
        loop {
            let line = self.next_line()?;
            if line.is_empty() {
                return Ok(hashes);
            }
            if hashes.len() == MAX_PROOF_HASHES {
                return Err(Error::TooManyProofLines);
            }
            let hash = base64::decode_hash(line).ok_or(Error::Malformed {
                what: Syntax::Base64,
                line: self.line,
            })?;
            hashes.push(Hash::from_bytes(hash));
        }
    }

    /// Everything not read yet.
    pub(crate) const fn rest(&self) -> &'a [u8] {
        self.bytes
    }
}

impl fmt::Display for AddCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.to_bytes()))
    }
}

#[cfg(feature = "witness")]
pub use with_keys::{Next, WitnessReply, WitnessSession};

#[cfg(feature = "witness")]
mod with_keys {
    use super::{AddCheckpoint, Error};
    use crate::log::ConsistencyProof;
    use crate::transparency::checkpoint::{SignedCheckpoint, parse_decimal};
    use crate::transparency::error::WitnessError;
    use crate::transparency::note::{MAX_NOTE_BYTES, MAX_SIGNATURES, NoteSignature};
    use crate::transparency::signing::{Cosignature, KeyType, Verified, Vkey, verify_line};

    /// The media type of a 409 body.
    const SIZE_MEDIA_TYPE: &str = "text/x.tlog.size";

    /// The most bytes of a 409 body: a `u64` in decimal and a newline.
    const MAX_SIZE_BYTES: usize = 21;

    /// Reading a witness's reply to an `add-checkpoint` request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct WitnessReply;

    impl WitnessReply {
        /// The verified cosignature in a reply, or the outcome as an error.
        ///
        /// A 200 body is one to [`MAX_SIGNATURES`] signature lines, at most
        /// [`MAX_NOTE_BYTES`] in all, no two with one (key name, key ID) —
        /// all checked before any signature is; lines from other keys are
        /// ignored and the one line from `witness` (type `0x04`) must
        /// verify over `checkpoint`. A 409 must have media type
        /// `text/x.tlog.size` (compared case-insensitively, parameters
        /// ignored) and a body of one canonical decimal and a newline.
        ///
        /// # Errors
        ///
        /// [`Error::Witness`] with the outcome, or [`Error::WrongKeyType`] when `witness` is not a cosignature
        /// key.
        pub fn read(
            status: u16,
            content_type: Option<&str>,
            body: &[u8],
            checkpoint: &SignedCheckpoint,
            witness: &Vkey,
        ) -> Result<Cosignature, Error> {
            cosigner(witness)?;
            let outcome = match status {
                200 => return cosignature(body, checkpoint, witness),
                409 => conflict(content_type, body),
                400 => WitnessError::BadRequest,
                403 => WitnessError::Forbidden,
                404 => WitnessError::UnknownLog,
                422 => WitnessError::Unprocessable,
                429 => WitnessError::Pushback,
                status => WitnessError::Unexpected { status },
            };
            Err(outcome.into())
        }
    }

    /// Refuses a key that is not a witness's cosignature key.
    fn cosigner(witness: &Vkey) -> Result<(), Error> {
        match witness.key_type() {
            KeyType::CosignatureEd25519 => Ok(()),
            found => Err(Error::WrongKeyType {
                expected: KeyType::CosignatureEd25519,
                found,
            }),
        }
    }

    /// The size in a 409 reply, or why it is malformed.
    fn conflict(content_type: Option<&str>, body: &[u8]) -> WitnessError {
        let media_type = content_type
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        let size = media_type
            .filter(|media| media.eq_ignore_ascii_case(SIZE_MEDIA_TYPE))
            .filter(|_| body.len() <= MAX_SIZE_BYTES)
            .and_then(|_| core::str::from_utf8(body).ok())
            .and_then(|text| text.strip_suffix('\n'))
            .and_then(parse_decimal);
        size.map_or(WitnessError::MalformedReply, |size| {
            WitnessError::Conflict { size }
        })
    }

    /// The witness's verified cosignature in a 200 body.
    fn cosignature(
        body: &[u8],
        checkpoint: &SignedCheckpoint,
        witness: &Vkey,
    ) -> Result<Cosignature, Error> {
        let malformed = || Error::from(WitnessError::MalformedReply);
        if body.is_empty() || body.len() > MAX_NOTE_BYTES {
            return Err(malformed());
        }
        let text = core::str::from_utf8(body).map_err(|_| malformed())?;
        let lines = text.strip_suffix('\n').ok_or_else(malformed)?;
        // Every line is read and checked for shape and repetition first, so
        // a reply costs at most one signature verification.
        let mut read: Vec<NoteSignature> = Vec::new();
        for (at, line) in lines.split('\n').enumerate() {
            if read.len() == MAX_SIGNATURES {
                return Err(malformed());
            }
            let number = u32::try_from(at.saturating_add(1)).unwrap_or(u32::MAX);
            let line = NoteSignature::parse_line(line, number).map_err(|_| malformed())?;
            if read
                .iter()
                .any(|seen| (seen.name(), seen.key_id()) == (line.name(), line.key_id()))
            {
                return Err(malformed());
            }
            read.push(line);
        }
        let Some(line) = read.iter().find(|line| witness.signed(line)) else {
            return Err(WitnessError::NoCosignature {
                name: witness.name().clone(),
            }
            .into());
        };
        match verify_line(witness, line, checkpoint.note().text()) {
            Ok(Verified::Cosigner(cosignature)) => Ok(cosignature),
            _ => Err(WitnessError::BadCosignature {
                name: line.name().clone(),
                key_id: line.key_id(),
            }
            .into()),
        }
    }

    /// What to do after a reply.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum Next {
        /// The witness cosigned: attach the cosignature, and remember
        /// `witness_size` as the witness's size for this log.
        Cosigned {
            /// The verified cosignature.
            cosignature: Cosignature,
            /// The size the witness now holds for the log.
            witness_size: u64,
        },
        /// 409: the session adopted the witness's size; fetch the proof
        /// [`needed_proof`](WitnessSession::needed_proof) names and send
        /// [`request`](WitnessSession::request) again.
        Retry,
    }

    /// One checkpoint to one witness, through at most three requests: the
    /// first, and two retries after a 409 told it the witness's size
    /// (Tessera's limit).
    ///
    /// ```
    /// # #[cfg(feature = "testing")] {
    /// use doubleentry::transparency::{Checkpoint, KeyName, LogKey, Next, WitnessSession};
    /// use doubleentry::testing::TestWitness;
    /// use doubleentry::{LedgerId, TreeHead, log::empty_root};
    ///
    /// let ledger = LedgerId::new("example.com/ledgers/acme")?;
    /// let key = LogKey::from_seed(&ledger, [7; 32]);
    /// let head = TreeHead { size: 0, root: empty_root() };
    /// let signed = key.sign(&Checkpoint::new(KeyName::from(&ledger), head))?;
    /// let mut witness = TestWitness::new("witness.example/w1", [9; 32])?.log(key.vkey());
    ///
    /// let mut session = WitnessSession::new(signed.clone(), witness.vkey(), 0)?;
    /// assert_eq!(session.needed_proof(), None);
    /// let reply = witness.post(&session.request(None)?);
    /// let Next::Cosigned { cosignature, witness_size } =
    ///     session.receive(reply.status, reply.content_type, &reply.body)?
    /// else {
    ///     panic!("a first contact is cosigned");
    /// };
    /// assert_eq!(witness_size, 0);
    /// let cosigned = signed.with_cosignature(&cosignature)?;
    /// assert_eq!(cosigned.verify(&[key.vkey(), witness.vkey()])?.len(), 2);
    /// # }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[derive(Debug, Clone)]
    pub struct WitnessSession {
        checkpoint: SignedCheckpoint,
        witness: Vkey,
        old: u64,
        conflicts: Vec<u64>,
        finished: bool,
    }

    /// The most 409s a session takes before it gives up.
    const MAX_CONFLICTS: usize = 3;

    impl WitnessSession {
        /// A session sending `checkpoint` to the witness with key `witness`
        /// (type `0x04`), which last held `old` for this log (0 if
        /// unknown).
        ///
        /// # Errors
        ///
        /// [`Error::WrongKeyType`] for a key that is not a cosignature
        /// key; [`Error::ProofShape`] for `old` above the checkpoint's
        /// size.
        pub fn new(checkpoint: SignedCheckpoint, witness: Vkey, old: u64) -> Result<Self, Error> {
            cosigner(&witness)?;
            if old > checkpoint.checkpoint().head().size {
                return Err(Error::ProofShape);
            }
            Ok(Self {
                checkpoint,
                witness,
                old,
                conflicts: Vec::new(),
                finished: false,
            })
        }

        /// The size the next request starts from.
        #[must_use]
        pub const fn old(&self) -> u64 {
            self.old
        }

        /// The `(old, new)` sizes of the consistency proof the next request
        /// needs; `None` when `old` is 0 or the checkpoint's size.
        #[must_use]
        pub fn needed_proof(&self) -> Option<(u64, u64)> {
            let size = self.checkpoint.checkpoint().head().size;
            (self.old != 0 && self.old != size).then_some((self.old, size))
        }

        /// The next request body. Sending it again (after a transport
        /// failure) is safe: only a 409 counts towards the limit.
        ///
        /// # Errors
        ///
        /// The errors of [`AddCheckpoint::new`], or
        /// [`WitnessError::SessionFinished`].
        pub fn request(&mut self, proof: Option<&ConsistencyProof>) -> Result<Vec<u8>, Error> {
            self.live()?;
            Ok(AddCheckpoint::new(&self.checkpoint, self.old, proof)?.to_bytes())
        }

        /// Reads the reply to the last request.
        ///
        /// # Errors
        ///
        /// The outcome of [`WitnessReply::read`] for anything but a 200 or
        /// a 409 the session can retry; a 409 naming a size beyond the
        /// checkpoint is [`WitnessError::WitnessAhead`], one naming a size
        /// below an earlier 409's [`WitnessError::WitnessRegressed`] (the
        /// witness went backwards: a protocol error, not a retry), the third
        /// 409 [`WitnessError::TooManyConflicts`]. Every outcome but
        /// [`Next::Retry`] ends the session.
        ///
        /// A 409 naming the checkpoint's own size, followed by a 422, means
        /// the witness holds another root at that size — a possible fork
        /// the caller should look into; it is reported as
        /// [`WitnessError::Unprocessable`], as the witness answered.
        pub fn receive(
            &mut self,
            status: u16,
            content_type: Option<&str>,
            body: &[u8],
        ) -> Result<Next, Error> {
            self.live()?;
            let size = self.checkpoint.checkpoint().head().size;
            let read =
                WitnessReply::read(status, content_type, body, &self.checkpoint, &self.witness);
            let next = match read {
                Ok(cosignature) => Ok(Next::Cosigned {
                    cosignature,
                    witness_size: size,
                }),
                Err(Error::Witness(WitnessError::Conflict { size: witness })) => {
                    let previous = self.conflicts.iter().copied().max();
                    self.conflicts.push(witness);
                    if let Some(previous) = previous.filter(|previous| witness < *previous) {
                        Err(WitnessError::WitnessRegressed {
                            previous,
                            size: witness,
                        }
                        .into())
                    } else if witness > size {
                        Err(WitnessError::WitnessAhead {
                            witness,
                            checkpoint: size,
                        }
                        .into())
                    } else if self.conflicts.len() >= MAX_CONFLICTS {
                        Err(WitnessError::TooManyConflicts {
                            sizes: self.conflicts.clone(),
                        }
                        .into())
                    } else {
                        self.old = witness;
                        return Ok(Next::Retry);
                    }
                }
                Err(error) => Err(error),
            };
            self.finished = true;
            next
        }

        /// Refuses a call after the session ended.
        fn live(&self) -> Result<(), Error> {
            if self.finished {
                Err(WitnessError::SessionFinished.into())
            } else {
                Ok(())
            }
        }
    }
}
