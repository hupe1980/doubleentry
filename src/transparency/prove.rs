//! What both façades share when they prove: checking a caller's checkpoint
//! against the ledger, and assembling a sealed-balance proof from the reads
//! each façade makes its own way.

use super::checkpoint::SignedCheckpoint;
use super::note::KeyName;
use super::tiles::TileId;
use crate::error::{Error, IntegrityError, IntegrityKind, QueryError};
use crate::log::{BalanceTree, Hash, InclusionProof, SealedBalanceProof, TreeHead};
use crate::model::{
    AccountHandle, Balance, Currency, Label, Layer, LedgerId, PeriodId, Record, Seal,
};

/// A period id as a query names it.
pub(crate) fn period_label(text: &str) -> Result<PeriodId, Error> {
    Label::new(text).map_err(|_| {
        QueryError::InvalidLabel {
            text: text.to_owned(),
        }
        .into()
    })
}

/// The head a caller's checkpoint claims for this ledger, once its origin
/// is the ledger's id and its size one the ledger (of `size` records) has
/// reached. The caller still compares the root at that size.
pub(crate) fn claimed_head(
    ledger: &LedgerId,
    checkpoint: &SignedCheckpoint,
    size: u64,
) -> Result<TreeHead, Error> {
    let origin = checkpoint.checkpoint().origin();
    if origin != &KeyName::from(ledger) {
        return Err(super::Error::ForeignCheckpoint {
            origin: origin.clone(),
        }
        .into());
    }
    let head = checkpoint.checkpoint().head();
    if head.size > size {
        return Err(super::Error::CheckpointMismatch.into());
    }
    Ok(head)
}

/// Refuses a checkpoint whose root is not the ledger's at its size.
pub(crate) fn same_root(claimed: TreeHead, actual: TreeHead) -> Result<(), Error> {
    if claimed == actual {
        Ok(())
    } else {
        Err(super::Error::CheckpointMismatch.into())
    }
}

/// The seal a seal leaf decodes to.
pub(crate) fn seal_record<const P: u8>(bytes: &[u8], position: u64) -> Result<Seal<P>, Error> {
    match Record::<P>::decode(bytes) {
        Ok(Record::Seal(seal)) => Ok(seal),
        _ => Err(IntegrityError::new(IntegrityKind::Undecodable, Some(position)).into()),
    }
}

/// The parts of a sealed-balance proof a façade reads.
pub(crate) struct SealedParts {
    pub(crate) seal_leaf: Vec<u8>,
    pub(crate) seal_proof: InclusionProof,
    pub(crate) account_leaf: Vec<u8>,
    pub(crate) account_proof: InclusionProof,
    pub(crate) handle: AccountHandle,
    pub(crate) currency: Currency,
    pub(crate) layer: Layer,
}

/// The sealed-balance proof of `parts`, with the balance tree rebuilt from
/// the seal's leaves for (currency, layer) — checked against the seal's
/// root — and the whole proof verified against `head` before it is handed
/// out.
pub(crate) fn sealed_balance_proof<const P: u8>(
    parts: SealedParts,
    seal: &Seal<P>,
    leaves: impl IntoIterator<Item = (AccountHandle, Balance<P>)>,
    head: &TreeHead,
) -> Result<SealedBalanceProof<P>, Error> {
    let position = parts.seal_proof.index;
    let mismatch = || {
        Error::from(IntegrityError::new(
            IntegrityKind::SealMismatch,
            Some(position),
        ))
    };
    let balance = match seal.tree(parts.currency, parts.layer) {
        None => None,
        Some(sealed) => {
            let mut tree = BalanceTree::new(parts.currency, parts.layer, seal.parts().accounts);
            for (handle, balance) in leaves {
                tree.set(handle, balance).map_err(|_| mismatch())?;
            }
            if tree.root() != sealed.root {
                return Err(mismatch());
            }
            Some(tree.prove(parts.handle).map_err(|_| mismatch())?)
        }
    };
    let proof = SealedBalanceProof {
        seal_leaf: parts.seal_leaf,
        seal_proof: parts.seal_proof,
        account_leaf: parts.account_leaf,
        account_proof: parts.account_proof,
        currency: parts.currency,
        layer: parts.layer,
        handle: parts.handle,
        balance,
    };
    proof.verify(head).map_err(|_| mismatch())?;
    Ok(proof)
}

/// The refusal of a tile beyond a log of `size` records.
pub(crate) fn tile_beyond(tile: &TileId, size: u64) -> Error {
    QueryError::PositionBeyondHead {
        position: tile.needs_size(),
        size,
    }
    .into()
}

/// A hash tile's bytes: its hashes, concatenated.
pub(crate) fn tile_bytes(hashes: &[Hash]) -> Vec<u8> {
    hashes.iter().flat_map(Hash::as_bytes).copied().collect()
}
