//! C2SP `tlog-proof@v1` bundles ([`ProofBundle`]) for one record or one
//! sealed balance, and what verifying one proves ([`Proven`]).
//!
//! A **record bundle**'s `extra` is the record's leaf bytes (`kind ‖
//! canonical record`, first byte `0x01`–`0x08`).
//!
//! A **sealed-balance bundle** is a tlog-proof for the seal record whose
//! `extra` is this canonical structure (fixed-width little-endian integers,
//! `u64` length prefixes, counted sequences — the record encoding):
//!
//! ```text
//! tag            u64 27 ‖ "doubleentry/balance-proof/1"   (first byte 0x1B: never a record kind)
//! seal_leaf      u64 len ‖ leaf bytes of the 0x08 seal record at `index`
//! currency       u64 len ‖ ISO 4217 code
//! layer          u8: 0 settled, 1 pending
//! handle         u32
//! balance        u8 0: the seal has no tree for (currency, layer) — the balance is zero
//!                u8 1 ‖ u64 41 ‖ balance leaf (0x81 ‖ i128 debit ‖ i128 credit ‖ u64 postings)
//!                     ‖ u64 count ‖ count × 32-byte path (leaf index = handle, tree size = the seal's account count)
//! account_pos    u64: the log position of the account's registration record
//! account_leaf   u64 len ‖ leaf bytes of that 0x02 registration (it carries the handle)
//! account_path   u64 count ‖ count × 32-byte inclusion path at account_pos, tree size = the checkpoint's
//! ```
//!
//! Each path is a plain RFC 6962 inclusion path, checkable by any verifier
//! against the root the layout names: the seal's against the checkpoint, the
//! balance leaf's against the seal's tree root for (currency, layer), the
//! registration's against the checkpoint. `extra` is untrusted until its
//! leaf hash is bound by the bundle's inclusion path.
//!
//! **A zero-balance bundle** (`balance` = `0`) binds the seal, the
//! registration and the fact that the seal has no tree for the bundle's
//! (currency, layer). The `currency` and `layer` bytes themselves are not
//! hashed or signed: they can be rewritten to any other (currency, layer)
//! without a tree and the bundle still verifies. The claim stays true (no
//! tree, no posting, zero balance), but it is no evidence that the currency
//! exists in the ledger. A balance with a tree is bound to its (currency,
//! layer) through the tree root the seal lists.

use core::fmt;

use super::Error;
use super::base64;
use super::checkpoint::{SignedCheckpoint, parse_decimal};
use super::error::Syntax;
use super::note::KeyName;
use super::witness::Lines;
use crate::log::{
    BalanceProof, Hash, InclusionProof, MAX_PROOF_HASHES, ProvenBalance, SealedBalanceProof,
    TreeHead, balance_leaf_bytes,
};
use crate::model::{
    AccountHandle, BALANCE_LEAF_KIND, Balance, Currency, Encode, EntryId, Layer, LedgerId,
    MAX_LEAF_BYTES, Reader, Record, RecordKind, Writer,
};

/// The first line of every bundle.
const HEADER: &str = "c2sp.org/tlog-proof@v1";

/// The tag that opens a sealed-balance bundle's `extra`.
pub const BALANCE_PROOF_TAG: &str = "doubleentry/balance-proof/1";

/// The length of a balance leaf.
const BALANCE_LEAF_BYTES: usize = 41;

/// The largest `extra` a bundle may carry, decoded: the sealed-balance
/// layout with a seal and a registration leaf of
/// [`MAX_LEAF_BYTES`](crate::model::MAX_LEAF_BYTES) each and two 63-hash
/// paths, with room for the fixed fields (a record bundle's `extra` is one
/// leaf, smaller still).
pub const MAX_EXTRA_BYTES: usize = 2 * MAX_LEAF_BYTES + 2 * MAX_PROOF_HASHES * 32 + 512;

/// The base64 length of [`MAX_EXTRA_BYTES`].
const MAX_EXTRA_BASE64: usize = MAX_EXTRA_BYTES.div_ceil(3) * 4;

/// The largest bundle [`ProofBundle::parse`] reads: a note of
/// [`MAX_NOTE_BYTES`](super::MAX_NOTE_BYTES), the base64 of
/// [`MAX_EXTRA_BYTES`], and 4 096 bytes for the header, `extra`/`index`
/// prefixes and 63 proof lines.
pub const MAX_BUNDLE_BYTES: usize = super::note::MAX_NOTE_BYTES + MAX_EXTRA_BASE64 + 4_096;

/// An offline-verifiable proof that a record (or one sealed balance) is in a
/// signed checkpoint: C2SP `tlog-proof@v1`.
///
/// `P` is the ledger's scale, which decoding the proven record needs;
/// [`parse`](ProofBundle::parse) reads a bundle of a `P = 2` ledger,
/// [`parse_with_precision`](Self::parse_with_precision) any other.
///
/// ```
/// use doubleentry::transparency::ProofBundle;
///
/// let bytes = "c2sp.org/tlog-proof@v1\nindex 0\n\n\
///     example.com/x\n1\nbWFkZSB1cCByb290IG9mIDMyIGJ5dGVzIGxvbmcuLi4=\n\n\
///     \u{2014} example.com/x AAAAAAE=\n";
/// let bundle = ProofBundle::parse(bytes.as_bytes())?;
/// assert_eq!((bundle.index(), bundle.extra()), (0, None));
/// assert_eq!(bundle.to_bytes(), bytes.as_bytes());
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofBundle<const P: u8 = 2> {
    extra: Option<Vec<u8>>,
    index: u64,
    path: Vec<Hash>,
    checkpoint: SignedCheckpoint,
}

impl ProofBundle<2> {
    /// Parses a bundle of a ledger of scale 2.
    ///
    /// # Errors
    ///
    /// As [`parse_with_precision`](Self::parse_with_precision).
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        Self::parse_with_precision(bytes)
    }
}

impl<const P: u8> ProofBundle<P> {
    /// A bundle from its parts.
    pub(crate) fn new(
        extra: Vec<u8>,
        proof: InclusionProof,
        checkpoint: SignedCheckpoint,
    ) -> Result<Self, Error> {
        if proof.path.len() > MAX_PROOF_HASHES {
            return Err(Error::TooManyProofLines);
        }
        Ok(Self {
            extra: Some(extra),
            index: proof.index,
            path: proof.path,
            checkpoint,
        })
    }

    /// Parses a bundle: the header, an optional `extra` line, `index`, 0–63
    /// hash lines, a blank line and the signed checkpoint, all canonical.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] for more than [`MAX_BUNDLE_BYTES`] (before
    /// anything is read) or an `extra` above [`MAX_EXTRA_BYTES`] (before it
    /// is decoded); [`Error::Malformed`], [`Error::TooManyProofLines`] or
    /// the errors of [`SignedCheckpoint::parse`].
    pub fn parse_with_precision(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err(Error::TooLarge {
                limit: MAX_BUNDLE_BYTES,
            });
        }
        let mut lines = Lines::new(bytes);
        let header = |line| Error::Malformed {
            what: Syntax::Header,
            line,
        };
        if lines.next_line()? != HEADER {
            return Err(header(1));
        }
        let mut line = lines.next_line()?;
        let extra = match line.strip_prefix("extra ") {
            Some(encoded) => {
                if encoded.len() > MAX_EXTRA_BASE64 {
                    return Err(Error::TooLarge {
                        limit: MAX_EXTRA_BYTES,
                    });
                }
                let extra = base64::decode(encoded).ok_or(Error::Malformed {
                    what: Syntax::Base64,
                    line: lines.line(),
                })?;
                if extra.len() > MAX_EXTRA_BYTES {
                    return Err(Error::TooLarge {
                        limit: MAX_EXTRA_BYTES,
                    });
                }
                line = lines.next_line()?;
                Some(extra)
            }
            None => None,
        };
        let index = line
            .strip_prefix("index ")
            .and_then(parse_decimal)
            .ok_or(header(lines.line()))?;
        let path = lines.hashes()?;
        let checkpoint = SignedCheckpoint::parse(lines.rest())?;
        Ok(Self {
            extra,
            index,
            path,
            checkpoint,
        })
    }

    /// The bytes; [`parse`](ProofBundle::parse) gives the same bundle back.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = format!("{HEADER}\n").into_bytes();
        if let Some(extra) = &self.extra {
            bytes.extend_from_slice(format!("extra {}\n", base64::encode(extra)).as_bytes());
        }
        bytes.extend_from_slice(format!("index {}\n", self.index).as_bytes());
        for hash in &self.path {
            bytes.extend_from_slice(base64::encode(hash.as_bytes()).as_bytes());
            bytes.push(b'\n');
        }
        bytes.push(b'\n');
        bytes.extend_from_slice(&self.checkpoint.to_bytes());
        bytes
    }

    /// The proven leaf's position.
    #[must_use]
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// The inclusion path, leaf to root.
    #[must_use]
    pub fn path(&self) -> &[Hash] {
        &self.path
    }

    /// The signed checkpoint the path leads to.
    #[must_use]
    pub const fn checkpoint(&self) -> &SignedCheckpoint {
        &self.checkpoint
    }

    /// The `extra` data — untrusted until verified.
    #[must_use]
    pub fn extra(&self) -> Option<&[u8]> {
        self.extra.as_deref()
    }

    /// The bundle's head: the checkpoint's size and root.
    fn head(&self) -> TreeHead {
        self.checkpoint.checkpoint().head()
    }

    /// Verifies the bundle against its own checkpoint's head, by hashes
    /// only — no signature is checked (see `verify`, feature `witness`, and
    /// [`Verifier::verify`](super::Verifier)).
    ///
    /// For a record bundle: the inclusion of `extra`'s leaf hash at `index`,
    /// then the record it decodes to. For a sealed-balance bundle: the
    /// seal's inclusion, the balance leaf at the handle in the seal's tree,
    /// and the registration that names the handle.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] ([`Syntax::ExtraLayout`]) for missing or
    /// unrecognised `extra`; [`Error::ProofShape`]; [`Error::Proof`];
    /// [`Error::Decode`]; [`Error::WrongRecordKind`];
    /// [`Error::ForeignCheckpoint`] when the origin is not a ledger id.
    pub fn verify_inclusion(&self) -> Result<Proven<P>, Error> {
        let head = self.head();
        if self.index >= head.size {
            return Err(Error::ProofShape);
        }
        let extra = self.extra.as_deref().ok_or_else(layout)?;
        let origin = self.checkpoint.checkpoint().origin().clone();
        let ledger = LedgerId::new(origin.as_str()).map_err(|_| Error::ForeignCheckpoint {
            origin: origin.clone(),
        })?;
        match extra.first() {
            Some(kind) if RecordKind::from_u8(*kind).is_some() => {
                let proof = self.inclusion(head.size);
                let record = proof.verify_record::<P>(&head, extra)?;
                let entry_id = match &record {
                    Record::Entry(entry) => Some(entry.id(&ledger)),
                    _ => None,
                };
                Ok(Proven::Record(ProvenRecord {
                    origin,
                    head,
                    position: self.index,
                    record,
                    entry_id,
                }))
            }
            _ => {
                let proof = decode_balance_extra::<P>(extra, self.inclusion(head.size))?;
                Ok(Proven::Balance(proof.verify(&head)?))
            }
        }
    }

    /// The bundle's inclusion proof at `size`.
    fn inclusion(&self, size: u64) -> InclusionProof {
        InclusionProof {
            index: self.index,
            size,
            path: self.path.clone(),
        }
    }
}

impl<const P: u8> fmt::Display for ProofBundle<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.to_bytes()))
    }
}

/// The refusal of `extra` data in no known layout.
const fn layout() -> Error {
    Error::Malformed {
        what: Syntax::ExtraLayout,
        line: 2,
    }
}

/// What a verified bundle proves.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Proven<const P: u8 = 2> {
    /// A record is in the log at a position.
    Record(ProvenRecord<P>),
    /// An account's sealed balance in one (currency, layer).
    Balance(ProvenBalance<P>),
}

/// A record proven to be in a log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProvenRecord<const P: u8 = 2> {
    /// The ledger (the checkpoint's origin).
    pub origin: KeyName,
    /// The head it is proven against.
    pub head: TreeHead,
    /// Its position.
    pub position: u64,
    /// The record, decoded from the proven leaf.
    pub record: Record<P>,
    /// For an entry, its id, recomputed from the ledger id and its
    /// idempotency key.
    pub entry_id: Option<EntryId>,
}

/// A sealed-balance proof as a bundle: the seal's inclusion as the bundle's
/// path, everything else in `extra`.
pub(crate) fn balance_bundle<const P: u8>(
    proof: SealedBalanceProof<P>,
    checkpoint: SignedCheckpoint,
) -> Result<ProofBundle<P>, Error> {
    let extra = encode_balance_extra(&proof);
    ProofBundle::new(extra, proof.seal_proof, checkpoint)
}

/// The `extra` of a sealed-balance bundle (module docs).
fn encode_balance_extra<const P: u8>(proof: &SealedBalanceProof<P>) -> Vec<u8> {
    let mut w = Writer::new(Vec::new());
    w.str(BALANCE_PROOF_TAG);
    bytes(&mut w, &proof.seal_leaf);
    w.value(&proof.currency);
    w.value(&proof.layer);
    w.value(&proof.handle);
    match &proof.balance {
        None => w.u8(0),
        Some(balance) => {
            w.u8(1);
            bytes(&mut w, &balance_leaf_bytes(&balance.balance));
            hashes(&mut w, &balance.path.path);
        }
    }
    w.u64(proof.account_proof.index);
    bytes(&mut w, &proof.account_leaf);
    hashes(&mut w, &proof.account_proof.path);
    w.into_inner()
}

fn bytes(w: &mut Writer<Vec<u8>>, bytes: &[u8]) {
    w.len(bytes.len());
    w.fixed(bytes);
}

fn hashes(w: &mut Writer<Vec<u8>>, hashes: &[Hash]) {
    w.len(hashes.len());
    for hash in hashes {
        hash.encode(w);
    }
}

/// The sealed-balance proof a bundle's `extra` and seal inclusion carry.
/// Every size comes from the bundle (the checkpoint) or the seal leaf, never
/// from `extra`.
fn decode_balance_extra<const P: u8>(
    extra: &[u8],
    seal_proof: InclusionProof,
) -> Result<SealedBalanceProof<P>, Error> {
    let mut r = Reader::new(extra);
    if r.str("balance-proof tag").ok() != Some(BALANCE_PROOF_TAG) {
        return Err(layout());
    }
    let seal_leaf = read_bytes(&mut r)?;
    let currency: Currency = r.value("currency")?;
    let layer: Layer = r.value("layer")?;
    let handle: AccountHandle = r.value("handle")?;
    let balance = match r.u8()? {
        0 => None,
        1 => {
            let leaf = read_bytes(&mut r)?;
            let path = read_hashes(&mut r)?;
            Some((leaf, path))
        }
        _ => return Err(layout()),
    };
    let account_position = r.u64()?;
    let account_leaf = read_bytes(&mut r)?;
    let account_path = read_hashes(&mut r)?;
    r.finish()?;
    let balance = match balance {
        None => None,
        Some((leaf, path)) => {
            let accounts = match Record::<P>::decode(&seal_leaf)? {
                Record::Seal(seal) => seal.parts().accounts,
                other => {
                    return Err(Error::WrongRecordKind {
                        expected: RecordKind::Seal,
                        found: other.kind(),
                    });
                }
            };
            Some(BalanceProof {
                currency,
                layer,
                handle,
                balance: decode_balance_leaf(&leaf)?,
                path: InclusionProof {
                    index: u64::from(handle.get()),
                    size: u64::from(accounts),
                    path,
                },
            })
        }
    };
    let size = seal_proof.size;
    Ok(SealedBalanceProof {
        seal_leaf,
        seal_proof,
        account_leaf,
        account_proof: InclusionProof {
            index: account_position,
            size,
            path: account_path,
        },
        currency,
        layer,
        handle,
        balance,
    })
}

fn read_bytes(r: &mut Reader<'_>) -> Result<Vec<u8>, Error> {
    let len = r.count("leaf", MAX_LEAF_BYTES)?;
    Ok(r.take(len)?.to_vec())
}

fn read_hashes(r: &mut Reader<'_>) -> Result<Vec<Hash>, Error> {
    let count = r.count("path", MAX_PROOF_HASHES)?;
    (0..count)
        .map(|_| r.value::<Hash>("path hash").map_err(Error::from))
        .collect()
}

/// A balance from its canonical leaf bytes (`0x81 ‖ debit ‖ credit ‖
/// postings`); anything that does not re-encode to the same bytes is
/// refused.
pub(crate) fn decode_balance_leaf<const P: u8>(leaf: &[u8]) -> Result<Balance<P>, Error> {
    let (&kind, body) = leaf.split_first().ok_or_else(layout)?;
    if kind != BALANCE_LEAF_KIND || leaf.len() != BALANCE_LEAF_BYTES {
        return Err(layout());
    }
    let mut r = Reader::new(body);
    let balance: Balance<P> = r.value("balance")?;
    r.finish()?;
    if balance_leaf_bytes(&balance).as_slice() == leaf {
        Ok(balance)
    } else {
        Err(layout())
    }
}

#[cfg(feature = "witness")]
mod with_keys {
    use super::{ProofBundle, Proven};
    use crate::transparency::Error;
    use crate::transparency::policy::Policy;

    impl<const P: u8> ProofBundle<P> {
        /// Verifies the bundle offline: the policy accepts its checkpoint
        /// (signatures and quorum), then
        /// [`verify_inclusion`](Self::verify_inclusion).
        ///
        /// # Errors
        ///
        /// The errors of [`Policy::verify`] and
        /// [`verify_inclusion`](Self::verify_inclusion).
        pub fn verify(&self, policy: &Policy) -> Result<Proven<P>, Error> {
            policy.verify(&self.checkpoint)?;
            self.verify_inclusion()
        }
    }
}
