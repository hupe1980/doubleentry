//! C2SP `tlog-tiles` addressing: [`TileId`], tile paths, and which tiles
//! change between two tree sizes.
//!
//! Tile `N` at level `L` holds the Merkle tree hashes of leaves
//! `[(256N + i)·256^L, (256N + i + 1)·256^L)` for `i < W`: every one is a
//! perfect subtree, so it is the stored node at height `8L`, index
//! `256N + i` — tiles are read from stored nodes, never recomputed.

use core::fmt;

use super::Error;
use super::checkpoint::parse_decimal;
use super::error::Syntax;
use crate::log::nodes::stored_index;

/// Hashes per full tile.
const FULL: u16 = 256;

/// The highest tile level the path format allows.
const MAX_LEVEL: u8 = 63;

/// What a tile holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TileKind {
    /// Merkle tree hashes at a level (0: leaf hashes).
    Hashes {
        /// The level, 0–63.
        level: u8,
    },
    /// An entry bundle: the leaves of the level-0 tile of the same index,
    /// each prefixed by its big-endian 16-bit length.
    Entries,
}

/// A tile: its kind, index and width (256 for a full tile).
///
/// ```
/// use doubleentry::transparency::TileId;
///
/// let tile = TileId::hashes(0, 1_234_067, 256)?;
/// assert_eq!(tile.path(), "tile/0/x001/x234/067");
/// assert_eq!(TileId::parse("tile/0/x001/x234/067")?, tile);
/// assert_eq!(TileId::hashes(1, 1, 17)?.path(), "tile/1/001.p/17");
/// assert_eq!(tile.entry_bundle().map(|bundle| bundle.path()), Some("tile/entries/x001/x234/067".into()));
///
/// // A tree of 70 000 leaves: 273 full and one partial level-0 tile, one
/// // full and one partial level-1 tile, one partial level-2 tile.
/// let tiles = TileId::changed(0, 70_000);
/// assert_eq!(tiles.len(), 277);
/// assert_eq!(tiles.last().map(TileId::path), Some("tile/2/000.p/1".into()));
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileId {
    kind: TileKind,
    index: u64,
    width: u16,
}

impl TileId {
    /// The hash tile `index` at `level`, `width` hashes wide.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTile`] for a level above 63 or a width outside
    /// `1..=256`.
    pub fn hashes(level: u8, index: u64, width: u16) -> Result<Self, Error> {
        if level > MAX_LEVEL {
            return Err(Error::InvalidTile);
        }
        Self::checked(TileKind::Hashes { level }, index, width)
    }

    /// The entry bundle `index`, `width` leaves wide.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTile`] for a width outside `1..=256`.
    pub fn entries(index: u64, width: u16) -> Result<Self, Error> {
        Self::checked(TileKind::Entries, index, width)
    }

    fn checked(kind: TileKind, index: u64, width: u16) -> Result<Self, Error> {
        if (1..=FULL).contains(&width) {
            Ok(Self { kind, index, width })
        } else {
            Err(Error::InvalidTile)
        }
    }

    /// What the tile holds.
    #[must_use]
    pub const fn kind(&self) -> TileKind {
        self.kind
    }

    /// The tile's index within its level.
    #[must_use]
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// The number of hashes or leaves (256 for a full tile).
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// The tile's path below the log's prefix, e.g.
    /// `tile/0/x001/x234/067` or `tile/entries/005.p/17`.
    #[must_use]
    pub fn path(&self) -> String {
        let mut index = format!("{:03}", self.index.rem_euclid(1000));
        let mut rest = self.index;
        while rest >= 1000 {
            rest = rest.div_euclid(1000);
            index = format!("x{:03}/{index}", rest.rem_euclid(1000));
        }
        let level = match self.kind {
            TileKind::Hashes { level } => level.to_string(),
            TileKind::Entries => String::from("entries"),
        };
        if self.width == FULL {
            format!("tile/{level}/{index}")
        } else {
            format!("tile/{level}/{index}.p/{}", self.width)
        }
    }

    /// Parses a tile path: the exact inverse of [`path`](Self::path).
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] with [`Syntax::TilePath`].
    pub fn parse(path: &str) -> Result<Self, Error> {
        let malformed = Error::Malformed {
            what: Syntax::TilePath,
            line: 0,
        };
        let rest = path.strip_prefix("tile/").ok_or(malformed.clone())?;
        let (kind, rest) = rest.split_once('/').ok_or(malformed.clone())?;
        let kind = match kind {
            "entries" => TileKind::Entries,
            level => TileKind::Hashes {
                level: parse_decimal(level)
                    .and_then(|level| u8::try_from(level).ok())
                    .ok_or(malformed.clone())?,
            },
        };
        let (groups, width) = match rest.split_once(".p/") {
            Some((groups, width)) => (
                groups,
                parse_decimal(width)
                    .and_then(|width| u16::try_from(width).ok())
                    .filter(|width| *width < FULL)
                    .ok_or(malformed.clone())?,
            ),
            None => (rest, FULL),
        };
        let mut index: u64 = 0;
        let mut parts = groups.split('/').peekable();
        while let Some(group) = parts.next() {
            let digits = if parts.peek().is_some() {
                group.strip_prefix('x').ok_or(malformed.clone())?
            } else {
                group
            };
            if digits.len() != 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(malformed);
            }
            index = digits
                .parse::<u64>()
                .ok()
                .and_then(|group| index.checked_mul(1000)?.checked_add(group))
                .ok_or(malformed.clone())?;
        }
        let tile = match kind {
            TileKind::Hashes { level } => Self::hashes(level, index, width),
            TileKind::Entries => Self::entries(index, width),
        }
        .map_err(|_| malformed.clone())?;
        if tile.path() == path {
            Ok(tile)
        } else {
            Err(malformed)
        }
    }

    /// The hash tiles to (re)publish when a tree grows from `old` to `new`
    /// leaves: exactly Go `tlog.NewTiles(8, old, new)`. Empty unless
    /// `old < new`.
    #[must_use]
    pub fn changed(old: u64, new: u64) -> Vec<Self> {
        let mut tiles = Vec::new();
        if old >= new {
            return tiles;
        }
        for level in 0..=7u8 {
            let shift = u32::from(level).saturating_mul(8);
            let (old_n, new_n) = (
                old.checked_shr(shift).unwrap_or(0),
                new.checked_shr(shift).unwrap_or(0),
            );
            if new_n == 0 {
                break;
            }
            if old_n == new_n {
                continue;
            }
            let last = new_n.checked_shr(8).unwrap_or(0);
            for index in old_n.checked_shr(8).unwrap_or(0)..last {
                tiles.push(Self {
                    kind: TileKind::Hashes { level },
                    index,
                    width: FULL,
                });
            }
            let width = u16::from(new_n.to_le_bytes()[0]);
            if width > 0 {
                tiles.push(Self {
                    kind: TileKind::Hashes { level },
                    index: last,
                    width,
                });
            }
        }
        tiles
    }

    /// The entry bundle of a level-0 hash tile; `None` for any other tile.
    #[must_use]
    pub const fn entry_bundle(&self) -> Option<Self> {
        match self.kind {
            TileKind::Hashes { level: 0 } => Some(Self {
                kind: TileKind::Entries,
                index: self.index,
                width: self.width,
            }),
            _ => None,
        }
    }

    /// The smallest tree size that contains the tile:
    /// `(256·index + width)·256^level` (`u64::MAX` when that overflows).
    #[must_use]
    pub fn needs_size(&self) -> u64 {
        let level = match self.kind {
            TileKind::Hashes { level } => level,
            TileKind::Entries => 0,
        };
        self.index
            .checked_mul(u64::from(FULL))
            .and_then(|start| start.checked_add(u64::from(self.width)))
            .and_then(|end| {
                let scale = 1u64.checked_shl(u32::from(level).checked_mul(8)?)?;
                end.checked_mul(scale)
            })
            .unwrap_or(u64::MAX)
    }

    /// Refuses a tile beyond a tree of `size` leaves.
    ///
    /// # Errors
    ///
    /// [`Error::TileOutOfRange`].
    pub fn check_within(&self, size: u64) -> Result<(), Error> {
        if self.needs_size() <= size {
            Ok(())
        } else {
            Err(Error::TileOutOfRange { tile: *self, size })
        }
    }

    /// The stored nodes (Go `tlog` numbering, as
    /// [`Store::read_nodes`](crate::store::Store::read_nodes) takes them) a
    /// hash tile is read from, in tile order: height `8L`, indexes
    /// `256N .. 256N + W`. A tile's bytes are those hashes, concatenated.
    ///
    /// ```
    /// use doubleentry::store::Tree;
    /// use doubleentry::transparency::TileId;
    /// use doubleentry::log::leaf_hash;
    ///
    /// let mut tree = Tree::new();
    /// for leaf in [b"a", b"b", b"c"] {
    ///     tree.push(leaf_hash(leaf))?;
    /// }
    /// let tile = TileId::hashes(0, 0, 3)?;
    /// let bytes: Vec<u8> = tile
    ///     .node_ids(tree.size())?
    ///     .iter()
    ///     .flat_map(|id| tree.stored()[*id as usize].as_bytes().to_vec())
    ///     .collect();
    /// assert_eq!(&bytes[32..64], leaf_hash(b"b").as_bytes());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::WrongTileKind`] for an entry bundle;
    /// [`Error::TileOutOfRange`] beyond a tree of `size` leaves.
    pub fn node_ids(&self, size: u64) -> Result<Vec<u64>, Error> {
        let TileKind::Hashes { level } = self.kind else {
            return Err(Error::WrongTileKind { tile: *self });
        };
        self.check_within(size)?;
        let out_of_range = || Error::TileOutOfRange { tile: *self, size };
        let height = level.checked_mul(8).ok_or_else(out_of_range)?;
        let first = self
            .index
            .checked_mul(u64::from(FULL))
            .ok_or_else(out_of_range)?;
        (0..u64::from(self.width))
            .map(|offset| {
                first
                    .checked_add(offset)
                    .and_then(|index| stored_index(height, index).ok())
                    .ok_or_else(out_of_range)
            })
            .collect()
    }

    /// The leaf positions of an entry bundle: `256N .. 256N + W`.
    ///
    /// # Errors
    ///
    /// [`Error::WrongTileKind`] for a hash tile; [`Error::TileOutOfRange`]
    /// beyond a tree of `size` leaves.
    pub fn leaf_range(&self, size: u64) -> Result<core::ops::Range<u64>, Error> {
        if self.kind != TileKind::Entries {
            return Err(Error::WrongTileKind { tile: *self });
        }
        self.check_within(size)?;
        let out_of_range = || Error::TileOutOfRange { tile: *self, size };
        let start = self
            .index
            .checked_mul(u64::from(FULL))
            .ok_or_else(out_of_range)?;
        let end = start
            .checked_add(u64::from(self.width))
            .ok_or_else(out_of_range)?;
        Ok(start..end)
    }
}

impl fmt::Display for TileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path())
    }
}

/// An entry bundle from leaf bytes: each leaf prefixed by its big-endian
/// 16-bit length.
pub(crate) fn entry_bundle<'a>(
    leaves: impl IntoIterator<Item = (u64, &'a [u8])>,
) -> Result<Vec<u8>, Error> {
    let mut bundle = Vec::new();
    for (position, leaf) in leaves {
        let len = u16::try_from(leaf.len()).map_err(|_| Error::LeafTooLarge {
            position,
            len: leaf.len(),
        })?;
        bundle.extend_from_slice(&len.to_be_bytes());
        bundle.extend_from_slice(leaf);
    }
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip_and_refuse_variants() {
        for (tile, path) in [
            (TileId::hashes(0, 0, 1).unwrap(), "tile/0/000.p/1"),
            (TileId::hashes(0, 999, 256).unwrap(), "tile/0/999"),
            (
                TileId::hashes(3, 1000, 255).unwrap(),
                "tile/3/x001/000.p/255",
            ),
            (
                TileId::entries(1_234_067, 256).unwrap(),
                "tile/entries/x001/x234/067",
            ),
            (
                TileId::hashes(63, u64::MAX, 256).unwrap(),
                "tile/63/x018/x446/x744/x073/x709/x551/615",
            ),
        ] {
            assert_eq!(tile.path(), path);
            assert_eq!(TileId::parse(path), Ok(tile), "{path}");
        }
        for bad in [
            "tile/0/5",
            "tile/0/005.p/256",
            "tile/0/005.p/0",
            "tile/0/005.p/07",
            "tile/00/005",
            "tile/64/005",
            "tile/0/x000/005",
            "tile/0/001/005",
            "tile/0/x1/005",
            "tile/0/",
            "tile/data/005",
            "tile/0/005/",
            "tiles/0/005",
            "tile/0/x018/x446/x744/x073/x709/x551/616",
            "tile/0/005.p/1/x",
        ] {
            assert!(TileId::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(TileId::hashes(64, 0, 1), Err(Error::InvalidTile));
        assert_eq!(TileId::entries(0, 0), Err(Error::InvalidTile));
        assert_eq!(TileId::entries(0, 257), Err(Error::InvalidTile));
    }

    #[test]
    fn the_tlog_tiles_example() {
        let tiles = TileId::changed(0, 70_000);
        let count = |level: u8, width: u16| {
            tiles
                .iter()
                .filter(|tile| tile.kind == TileKind::Hashes { level } && tile.width == width)
                .count()
        };
        assert_eq!((count(0, 256), count(0, 112)), (273, 1));
        assert_eq!((count(1, 256), count(1, 17)), (1, 1));
        assert_eq!(count(2, 1), 1);
        assert_eq!(tiles.len(), 277);
        assert!(TileId::changed(256, 256).is_empty());
        assert_eq!(
            TileId::changed(0, 256)
                .iter()
                .map(TileId::path)
                .collect::<Vec<_>>(),
            ["tile/0/000", "tile/1/000.p/1"]
        );
    }

    #[test]
    fn sizes_and_node_plans() {
        let tile = TileId::hashes(1, 1, 17).unwrap();
        assert_eq!(tile.needs_size(), (256 + 17) * 256);
        assert!(tile.check_within(tile.needs_size()).is_ok());
        assert!(matches!(
            tile.check_within(tile.needs_size() - 1),
            Err(Error::TileOutOfRange { .. })
        ));
        assert_eq!(TileId::hashes(9, 1, 1).unwrap().needs_size(), u64::MAX);
        let ids = TileId::hashes(0, 0, 3).unwrap().node_ids(3).unwrap();
        assert_eq!(ids, [0, 1, 3]);
        assert!(TileId::entries(0, 3).unwrap().node_ids(3).is_err());
        assert_eq!(TileId::entries(1, 3).unwrap().leaf_range(259), Ok(256..259));
    }

    #[test]
    fn entry_bundles_prefix_lengths() {
        let bundle = entry_bundle([(0, b"ab".as_slice()), (1, b"".as_slice())]).unwrap();
        assert_eq!(bundle, [0, 2, b'a', b'b', 0, 0]);
        let long = vec![0u8; 65_536];
        assert_eq!(
            entry_bundle([(7, long.as_slice())]),
            Err(Error::LeafTooLarge {
                position: 7,
                len: 65_536
            })
        );
    }
}
