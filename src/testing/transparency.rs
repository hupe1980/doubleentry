//! Transparency agreement: on the simulation's ledgers, the store renders
//! the same tiles, entry bundles and proof bundles — record, entry and
//! sealed balance — byte for byte as the reference [`Journal`].

use super::model::{Check, SplitMix64};
use super::reports::same;
use crate::model::{AccountHandle, EUR, Layer, Record, RecordKind};
use crate::store::Store;
use crate::transparency::{Checkpoint, KeyName, SignedCheckpoint, TileId};
use crate::{Journal, Ledger};

/// The journal's head as a checkpoint the façades accept: they check the
/// origin, size and root, never a signature, so one syntactically valid
/// line stands in for the log's.
fn unsigned<const P: u8>(journal: &Journal<P>) -> Result<SignedCheckpoint, String> {
    let origin = KeyName::from(journal.id());
    let text = Checkpoint::new(origin.clone(), journal.head()).text();
    SignedCheckpoint::parse(format!("{text}\n\u{2014} {origin} AAAAAAA=\n").as_bytes())
        .map_err(|error| error.to_string())
}

/// Every tile and entry bundle of the log, a random record bundle, a random
/// entry bundle and a random sealed-balance bundle, from both.
pub(super) async fn transparency_checks<S: Store<P>, const P: u8>(
    books: &Ledger<S, P>,
    journal: &Journal<P>,
    random: &mut SplitMix64,
) -> Check {
    let size = journal.head().size;
    for tile in TileId::changed(0, size) {
        same(
            &format!("tile {tile}"),
            journal.tile(tile),
            books.tile(tile).await,
        )?;
        if let Some(entries) = tile.entry_bundle() {
            same(
                &format!("entry bundle {entries}"),
                journal.tile_entries(entries),
                books.tile_entries(entries).await,
            )?;
        }
    }
    let checkpoint = unsigned(journal)?;
    let position = random.below(size);
    same(
        &format!("the record bundle of {position}"),
        journal.bundle_record(position, &checkpoint),
        books.bundle_record(position, &checkpoint).await,
    )?;
    let mut entries = Vec::new();
    let mut seals = Vec::new();
    for leaf in journal.leaves() {
        match RecordKind::from_u8(leaf.bytes.first().copied().unwrap_or(0)) {
            Some(RecordKind::Entry) => entries.push(leaf.bytes),
            Some(RecordKind::Seal) => seals.push(leaf.bytes),
            _ => {}
        }
    }
    if let Some(Ok(Record::Entry(entry))) = random
        .pick(&entries)
        .map(|bytes| Record::<P>::decode(bytes))
    {
        let id = entry.id(journal.id());
        same(
            &format!("the entry bundle of {id}"),
            journal.bundle_entry(id, &checkpoint),
            books.bundle_entry(id, &checkpoint).await,
        )?;
    }
    if let Some(Ok(Record::Seal(seal))) =
        random.pick(&seals).map(|bytes| Record::<P>::decode(bytes))
    {
        let parts = seal.parts();
        let currency = random.pick(&parts.trees).map_or(EUR, |tree| tree.currency);
        let layer = if random.chance(50) {
            Layer::Settled
        } else {
            Layer::Pending
        };
        let handle = u32::try_from(random.below(u64::from(parts.accounts).saturating_add(1)))
            .ok()
            .and_then(AccountHandle::new)
            .ok_or("no handle")?;
        same(
            &format!(
                "the {currency} {layer:?} balance bundle of {handle} in {}",
                parts.period
            ),
            journal.bundle_balance(handle, currency, layer, parts.period.as_str(), &checkpoint),
            books
                .bundle_balance(handle, currency, layer, parts.period.as_str(), &checkpoint)
                .await,
        )?;
    }
    Ok(())
}
