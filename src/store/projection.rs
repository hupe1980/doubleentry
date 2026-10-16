//! What a store holds, as `audit` compares it with a replay of its log: every
//! table of the `doubleentry/1` schema as a canonical, ordered projection
//! ([`ProjectionPart`], [`ProjectionRow`]).
//!
//! A part is one table (the memos part also carries each entry's
//! fingerprint, which erasure changes together with the memo), its rows the
//! table's columns as [`SqlStore`](super::SqlStore) stores them — amounts as
//! 16-byte big-endian integers, dates as Julian day numbers, codes as
//! integers, dimension sets as text — in primary-key order. The replay
//! produces the same rows from the leaves alone, and the two are compared
//! byte for byte ([`ProjectionRow::to_bytes`]). Every column of every table
//! belongs to exactly one part; a unit test checks the part list against the
//! schema, so a table or column cannot be added without being audited.

use core::fmt;
use std::collections::BTreeMap;

use time::Date;

use super::read::{Page, PageRead};
use super::{Store, StoreError};
use crate::core::ResolutionKind;
use crate::error::IntegrityKind;
use crate::log::TreeHead;
use crate::log::nodes::RootPlan;
use crate::model::{
    AccountHandle, AccountKind, Amount, BalanceLimit, Currency, Dimensions, Layer, Side,
};

/// One column value of a [`ProjectionRow`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Cell {
    /// SQL `NULL`.
    Null,
    /// An integer.
    Int(i64),
    /// A byte string.
    Bytes(Vec<u8>),
    /// UTF-8 text.
    Text(String),
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Int(value) => write!(f, "{value}"),
            Self::Bytes(bytes) => {
                f.write_str("x'")?;
                for byte in bytes {
                    write!(f, "{byte:02x}")?;
                }
                f.write_str("'")
            }
            Self::Text(text) => write!(f, "{text:?}"),
        }
    }
}

/// One row of one [`ProjectionPart`]: its columns in the part's
/// [`columns`](ProjectionPart::columns) order, the primary key first.
///
/// ```
/// use doubleentry::store::{Cell, ProjectionRow};
///
/// let row = ProjectionRow::new(vec![Cell::Int(3), Cell::Text("EUR".into()), Cell::Null]);
/// assert_eq!(ProjectionRow::from_bytes(&row.to_bytes()), Some(row));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectionRow {
    cells: Vec<Cell>,
}

impl ProjectionRow {
    /// A row of `cells`.
    #[must_use]
    pub const fn new(cells: Vec<Cell>) -> Self {
        Self { cells }
    }

    /// The cells.
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// The canonical encoding the audit compares: per cell a tag (`0` NULL,
    /// `1` integer, `2` bytes, `3` text), then an integer's 8 big-endian
    /// bytes or a 4-byte big-endian length and the bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for cell in &self.cells {
            match cell {
                Cell::Null => out.push(0),
                Cell::Int(value) => {
                    out.push(1);
                    out.extend_from_slice(&value.to_be_bytes());
                }
                Cell::Bytes(bytes) => {
                    out.push(2);
                    out.extend_from_slice(&len(bytes.len()).to_be_bytes());
                    out.extend_from_slice(bytes);
                }
                Cell::Text(text) => {
                    out.push(3);
                    out.extend_from_slice(&len(text.len()).to_be_bytes());
                    out.extend_from_slice(text.as_bytes());
                }
            }
        }
        out
    }

    /// A row from its [`to_bytes`](Self::to_bytes) encoding; `None` for
    /// bytes that are not one.
    #[must_use]
    pub fn from_bytes(mut bytes: &[u8]) -> Option<Self> {
        let mut cells = Vec::new();
        while let Some((&tag, rest)) = bytes.split_first() {
            bytes = rest;
            cells.push(match tag {
                0 => Cell::Null,
                1 => {
                    let (value, rest) = bytes.split_first_chunk::<8>()?;
                    bytes = rest;
                    Cell::Int(i64::from_be_bytes(*value))
                }
                2 | 3 => {
                    let (length, rest) = bytes.split_first_chunk::<4>()?;
                    let length = usize::try_from(u32::from_be_bytes(*length)).ok()?;
                    let (value, rest) = rest.split_at_checked(length)?;
                    bytes = rest;
                    if tag == 2 {
                        Cell::Bytes(value.to_vec())
                    } else {
                        Cell::Text(String::from_utf8(value.to_vec()).ok()?)
                    }
                }
                _ => return None,
            });
        }
        Some(Self { cells })
    }

    /// The row's primary key in `part`: its first cells.
    #[must_use]
    pub fn key(&self, part: ProjectionPart) -> Self {
        Self {
            cells: self.cells.iter().take(part.key_len()).cloned().collect(),
        }
    }
}

/// `(1, x'ab', "EUR", NULL)`.
impl fmt::Display for ProjectionRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        for (index, cell) in self.cells.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{cell}")?;
        }
        f.write_str(")")
    }
}

/// A byte length as the 4 bytes of the canonical encoding (no value of a
/// store reaches 4 GiB).
fn len(length: usize) -> u32 {
    u32::try_from(length).unwrap_or(u32::MAX)
}

/// The type of a projected column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ty {
    /// An integer column.
    Int,
    /// A byte column.
    Bytes,
    /// A text column.
    Text,
}

/// One projected column: its table, its name and its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Column {
    /// The table.
    pub(crate) table: &'static str,
    /// The column.
    pub(crate) name: &'static str,
    /// Its type.
    pub(crate) ty: Ty,
}

/// A column, as a constant expression (so a part's layout is promoted).
macro_rules! col {
    ($table:literal, $name:literal, $ty:expr) => {
        Column {
            table: $table,
            name: $name,
            ty: $ty,
        }
    };
}

use Ty::{Bytes as B, Int as I, Text as T};

/// One part of a store's projection: one table of the `doubleentry/1`
/// schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ProjectionPart {
    /// `meta`: the schema id, ledger id, scale and genesis leaf hash.
    Meta,
    /// `leaves`: the log. The audit replays these rows.
    Leaves,
    /// `nodes`: every stored tree node.
    Nodes,
    /// `accounts`: registrations by handle.
    Accounts,
    /// `account_versions`: every version of every account field.
    AccountVersions,
    /// `currency_versions`: every currency declaration.
    CurrencyVersions,
    /// `policy_versions`: every policy record.
    PolicyVersions,
    /// `periods`: the calendar.
    Periods,
    /// `entries`: the entry index (key, id, ordinal, reversal link).
    Entries,
    /// `memos`, with each entry's draft fingerprint: the erasable side data,
    /// one row per entry (and per memo row without an entry).
    Memos,
    /// `postings`: every posting with its balance after.
    Postings,
    /// `posting_dims`: the dimension index of postings.
    PostingDims,
    /// `holds`: the hold index, open and resolved.
    Holds,
    /// `seals`: the sealed periods.
    Seals,
    /// `seal_leaves`: the non-zero leaves of every seal balance tree.
    SealLeaves,
    /// `seal_nodes`: the stored nodes of every seal balance tree.
    SealNodes,
    /// `clearings`: clearing records by key.
    Clearings,
    /// `clearing_resets`: which apply each reset released.
    ClearingResets,
    /// `clearing_refs`: the balances each clearing record touches.
    ClearingRefs,
    /// `balances`: the maintained balances.
    Balances,
    /// `cleared`: the amounts cleared against postings.
    Cleared,
    /// `outstanding`: the amounts open holds reserve.
    Outstanding,
    /// `slices`: the Settled balance of each (account, currency, dimension
    /// set).
    Slices,
}

impl ProjectionPart {
    /// Every part, in the order an audit compares them.
    pub const ALL: [Self; 23] = [
        Self::Meta,
        Self::Leaves,
        Self::Nodes,
        Self::Accounts,
        Self::AccountVersions,
        Self::CurrencyVersions,
        Self::PolicyVersions,
        Self::Periods,
        Self::Entries,
        Self::Memos,
        Self::Postings,
        Self::PostingDims,
        Self::Holds,
        Self::Seals,
        Self::SealLeaves,
        Self::SealNodes,
        Self::Clearings,
        Self::ClearingResets,
        Self::ClearingRefs,
        Self::Balances,
        Self::Cleared,
        Self::Outstanding,
        Self::Slices,
    ];

    /// The table the part projects.
    #[must_use]
    pub const fn table(self) -> &'static str {
        match self {
            Self::Meta => "meta",
            Self::Leaves => "leaves",
            Self::Nodes => "nodes",
            Self::Accounts => "accounts",
            Self::AccountVersions => "account_versions",
            Self::CurrencyVersions => "currency_versions",
            Self::PolicyVersions => "policy_versions",
            Self::Periods => "periods",
            Self::Entries => "entries",
            Self::Memos => "memos",
            Self::Postings => "postings",
            Self::PostingDims => "posting_dims",
            Self::Holds => "holds",
            Self::Seals => "seals",
            Self::SealLeaves => "seal_leaves",
            Self::SealNodes => "seal_nodes",
            Self::Clearings => "clearings",
            Self::ClearingResets => "clearing_resets",
            Self::ClearingRefs => "clearing_refs",
            Self::Balances => "balances",
            Self::Cleared => "cleared",
            Self::Outstanding => "outstanding",
            Self::Slices => "slices",
        }
    }

    /// The projected columns, `table.column`, the primary key first.
    #[must_use]
    pub fn columns(self) -> Vec<String> {
        self.layout()
            .iter()
            .map(|column| format!("{}.{}", column.table, column.name))
            .collect()
    }

    /// How many leading columns form the primary key (the row order).
    #[must_use]
    pub const fn key_len(self) -> usize {
        match self {
            Self::Meta
            | Self::Leaves
            | Self::Nodes
            | Self::Accounts
            | Self::PolicyVersions
            | Self::Periods
            | Self::Entries
            | Self::Memos
            | Self::Holds
            | Self::Seals
            | Self::Clearings
            | Self::ClearingResets => 1,
            Self::CurrencyVersions | Self::Postings | Self::Cleared | Self::Outstanding => 2,
            Self::AccountVersions | Self::PostingDims | Self::Balances | Self::Slices => 3,
            Self::SealLeaves | Self::ClearingRefs => 4,
            Self::SealNodes => 5,
        }
    }

    /// Whether the part changes without the log growing: memo erasure
    /// changes the memos part (and only it).
    #[must_use]
    pub const fn is_erasable(self) -> bool {
        matches!(self, Self::Memos)
    }

    /// The columns of the part.
    pub(crate) const fn layout(self) -> &'static [Column] {
        match self {
            Self::Meta => &[
                col!("meta", "singleton", I),
                col!("meta", "schema_id", T),
                col!("meta", "ledger_id", T),
                col!("meta", "amount_scale", I),
                col!("meta", "genesis", B),
            ],
            Self::Leaves => &[
                col!("leaves", "position", I),
                col!("leaves", "kind", I),
                col!("leaves", "bytes", B),
            ],
            Self::Nodes => &[col!("nodes", "id", I), col!("nodes", "hash", B)],
            Self::Accounts => &[
                col!("accounts", "handle", I),
                col!("accounts", "path", T),
                col!("accounts", "kind", I),
                col!("accounts", "position", I),
            ],
            Self::AccountVersions => &[
                col!("account_versions", "handle", I),
                col!("account_versions", "field", I),
                col!("account_versions", "position", I),
                col!("account_versions", "effective", I),
                col!("account_versions", "open_from", I),
                col!("account_versions", "open_to", I),
                col!("account_versions", "limit_code", I),
                col!("account_versions", "class", T),
            ],
            Self::CurrencyVersions => &[
                col!("currency_versions", "code", T),
                col!("currency_versions", "position", I),
                col!("currency_versions", "exponent", I),
                col!("currency_versions", "effective", I),
            ],
            Self::PolicyVersions => &[
                col!("policy_versions", "position", I),
                col!("policy_versions", "effective", I),
            ],
            Self::Periods => &[
                col!("periods", "period_id", T),
                col!("periods", "first_day", I),
                col!("periods", "last_day", I),
                col!("periods", "position", I),
            ],
            Self::Entries => &[
                col!("entries", "position", I),
                col!("entries", "ordinal", I),
                col!("entries", "entry_id", B),
                col!("entries", "idem_key", B),
                col!("entries", "reverses", B),
            ],
            Self::Memos => &[
                col!("entries", "position", I),
                col!("entries", "fingerprint", B),
                col!("memos", "position", I),
                col!("memos", "salt", B),
                col!("memos", "text", T),
            ],
            Self::Postings => &[
                col!("postings", "position", I),
                col!("postings", "idx", I),
                col!("postings", "handle", I),
                col!("postings", "currency", T),
                col!("postings", "layer", I),
                col!("postings", "side", I),
                col!("postings", "amount", B),
                col!("postings", "booking", I),
                col!("postings", "value_date", I),
                col!("postings", "dimensions", T),
                col!("postings", "debit_after", B),
                col!("postings", "credit_after", B),
                col!("postings", "count_after", I),
            ],
            Self::PostingDims => &[
                col!("posting_dims", "position", I),
                col!("posting_dims", "idx", I),
                col!("posting_dims", "axis", T),
                col!("posting_dims", "value", T),
            ],
            Self::Holds => &[
                col!("holds", "position", I),
                col!("holds", "entry_id", B),
                col!("holds", "booking", I),
                col!("holds", "expires", I),
                col!("holds", "debit_handle", I),
                col!("holds", "credit_handle", I),
                col!("holds", "currency", T),
                col!("holds", "amount", B),
                col!("holds", "resolved_by", B),
                col!("holds", "how", I),
            ],
            Self::Seals => &[col!("seals", "period_id", T), col!("seals", "position", I)],
            Self::SealLeaves => &[
                col!("seal_leaves", "period_id", T),
                col!("seal_leaves", "currency", T),
                col!("seal_leaves", "layer", I),
                col!("seal_leaves", "handle", I),
                col!("seal_leaves", "debit", B),
                col!("seal_leaves", "credit", B),
                col!("seal_leaves", "postings", I),
            ],
            Self::SealNodes => &[
                col!("seal_nodes", "period_id", T),
                col!("seal_nodes", "currency", T),
                col!("seal_nodes", "layer", I),
                col!("seal_nodes", "level", I),
                col!("seal_nodes", "idx", I),
                col!("seal_nodes", "hash", B),
            ],
            Self::Clearings => &[
                col!("clearings", "position", I),
                col!("clearings", "idem_key", B),
            ],
            Self::ClearingResets => &[
                col!("clearing_resets", "clearing", I),
                col!("clearing_resets", "position", I),
            ],
            Self::ClearingRefs => &[
                col!("clearing_refs", "position", I),
                col!("clearing_refs", "handle", I),
                col!("clearing_refs", "currency", T),
                col!("clearing_refs", "layer", I),
            ],
            Self::Balances => &[
                col!("balances", "handle", I),
                col!("balances", "currency", T),
                col!("balances", "layer", I),
                col!("balances", "debit", B),
                col!("balances", "credit", B),
                col!("balances", "postings", I),
            ],
            Self::Cleared => &[
                col!("cleared", "position", I),
                col!("cleared", "idx", I),
                col!("cleared", "applied", B),
            ],
            Self::Outstanding => &[
                col!("outstanding", "handle", I),
                col!("outstanding", "currency", T),
                col!("outstanding", "debit", B),
                col!("outstanding", "credit", B),
            ],
            Self::Slices => &[
                col!("slices", "handle", I),
                col!("slices", "currency", T),
                col!("slices", "dimensions", T),
                col!("slices", "debit", B),
                col!("slices", "credit", B),
                col!("slices", "postings", I),
            ],
        }
    }

    /// The integrity kind a difference in this part is reported as.
    pub(crate) const fn integrity_kind(self) -> IntegrityKind {
        match self {
            Self::Meta => IntegrityKind::GenesisMismatch,
            Self::Nodes => IntegrityKind::NodeMismatch,
            Self::Postings | Self::PostingDims | Self::Balances | Self::Slices => {
                IntegrityKind::BalanceMismatch
            }
            Self::Holds | Self::Outstanding => IntegrityKind::HoldMismatch,
            Self::Clearings | Self::ClearingResets | Self::ClearingRefs | Self::Cleared => {
                IntegrityKind::ClearingMismatch
            }
            Self::Seals | Self::SealLeaves | Self::SealNodes => IntegrityKind::SealMismatch,
            Self::Entries => IntegrityKind::EntryCountMismatch,
            Self::Leaves
            | Self::Accounts
            | Self::AccountVersions
            | Self::CurrencyVersions
            | Self::PolicyVersions
            | Self::Periods
            | Self::Memos => IntegrityKind::ReplayMismatch,
        }
    }
}

impl fmt::Display for ProjectionPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.table())
    }
}

/// A request for one page of one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRead {
    /// The part.
    pub part: ProjectionPart,
    /// The page; the cursor is opaque (the `next` of the previous page: the
    /// last row's key, [`ProjectionRow::to_bytes`] encoded). A projection
    /// page reads the part as it is: `through` is not applied, and the
    /// page's `size` says which log size it describes.
    pub page: PageRead<Vec<u8>>,
}

/// One page of one part. Rows are scale-free; `P` is the store's scale,
/// kept so a store's signature names its own.
pub type ProjectionPage<const P: u8 = 2> = Page<ProjectionRow, Vec<u8>>;

/// The column encodings a store uses, as [`Cell`]s: the one
/// definition both the SQL store and the replay's projection use.
pub(crate) mod cells {
    use super::{
        AccountHandle, AccountKind, Amount, BalanceLimit, Cell, Currency, Date, Dimensions, Layer,
        ResolutionKind, Side,
    };

    /// Separates a dimension's axis from its value (labels hold no control
    /// characters).
    pub(crate) const AXIS_VALUE: char = '\u{1f}';
    /// Separates the pairs of a dimension set.
    pub(crate) const PAIRS: char = '\u{1e}';

    /// A count or position (saturating at `i64::MAX`, beyond any store).
    pub(crate) fn int(value: u64) -> Cell {
        Cell::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }

    /// A date as its Julian day number.
    pub(crate) fn date(date: Date) -> Cell {
        Cell::Int(i64::from(date.to_julian_day()))
    }

    /// An optional date.
    pub(crate) fn opt_date(date: Option<Date>) -> Cell {
        date.map_or(Cell::Null, self::date)
    }

    /// An amount as 16 bytes, big-endian two's complement.
    pub(crate) fn amount<const P: u8>(amount: Amount<P>) -> Cell {
        Cell::Bytes(amount.to_minor().to_be_bytes().to_vec())
    }

    /// Text.
    pub(crate) fn text(text: &str) -> Cell {
        Cell::Text(text.to_owned())
    }

    /// Bytes.
    pub(crate) fn bytes(bytes: &[u8]) -> Cell {
        Cell::Bytes(bytes.to_vec())
    }

    /// An account handle.
    pub(crate) fn handle(handle: AccountHandle) -> Cell {
        Cell::Int(i64::from(handle.get()))
    }

    /// A currency code.
    pub(crate) fn currency(currency: Currency) -> Cell {
        text(currency.as_str())
    }

    /// How a hold was resolved, as its code.
    pub(crate) const fn how(how: ResolutionKind) -> Cell {
        Cell::Int(match how {
            ResolutionKind::Posted => 0,
            ResolutionKind::Voided => 1,
            ResolutionKind::Expired => 2,
        })
    }

    /// A layer's code.
    pub(crate) const fn layer(layer: Layer) -> Cell {
        Cell::Int(match layer {
            Layer::Settled => 0,
            Layer::Pending => 1,
        })
    }

    /// A side's code.
    pub(crate) const fn side(side: Side) -> Cell {
        Cell::Int(match side {
            Side::Debit => 0,
            Side::Credit => 1,
        })
    }

    /// An account kind's code.
    pub(crate) const fn kind(kind: AccountKind) -> Cell {
        Cell::Int(match kind {
            AccountKind::Asset => 0,
            AccountKind::Liability => 1,
            AccountKind::Equity => 2,
            AccountKind::Income => 3,
            AccountKind::Expense => 4,
        })
    }

    /// A balance limit's code.
    pub(crate) const fn limit(limit: BalanceLimit) -> Cell {
        Cell::Int(match limit {
            BalanceLimit::Unlimited => 0,
            BalanceLimit::NoCreditBalance => 1,
            BalanceLimit::NoDebitBalance => 2,
        })
    }

    /// A dimension set as text: `axis US value` pairs joined by `RS`, in the
    /// set's (byte) order.
    pub(crate) fn dimensions(dimensions: &Dimensions) -> String {
        let mut encoded = String::new();
        for (axis, value) in dimensions.iter() {
            if !encoded.is_empty() {
                encoded.push(PAIRS);
            }
            encoded.push_str(axis.as_str());
            encoded.push(AXIS_VALUE);
            encoded.push_str(value.as_str());
        }
        encoded
    }
}

/// Every part of a store at one log size: the claim
/// [`Replayer::finish`](super::Replayer::finish) compares with a replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreProjection {
    head: Option<TreeHead>,
    parts: BTreeMap<ProjectionPart, Vec<ProjectionRow>>,
}

impl StoreProjection {
    /// A projection of a log with `head`, every part empty.
    #[must_use]
    pub fn new(head: TreeHead) -> Self {
        Self {
            head: Some(head),
            parts: BTreeMap::new(),
        }
    }

    /// The tree head the store's nodes assemble at its size.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        self.head.unwrap_or(TreeHead {
            size: 0,
            root: crate::log::empty_root(),
        })
    }

    /// The log size the projection describes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.head().size
    }

    /// The rows of one part, in key order.
    #[must_use]
    pub fn rows(&self, part: ProjectionPart) -> &[ProjectionRow] {
        self.parts.get(&part).map_or(&[], Vec::as_slice)
    }

    /// The rows of one part, to change (a test's tampering).
    pub fn rows_mut(&mut self, part: ProjectionPart) -> &mut Vec<ProjectionRow> {
        self.parts.entry(part).or_default()
    }
}

/// The tree head of the log at `size`, assembled from the store's nodes.
///
/// # Errors
///
/// The store's failure, or `Integrity` when its nodes do not assemble.
pub(crate) async fn head_at<S: Store<P>, const P: u8>(
    store: &S,
    size: u64,
) -> Result<TreeHead, StoreError> {
    let plan = RootPlan::new(size).map_err(|_| node_failure(size))?;
    let hashes = store.read_nodes(plan.ids()).await?;
    let root = plan.assemble(&hashes).map_err(|_| node_failure(size))?;
    Ok(TreeHead { size, root })
}

fn node_failure(size: u64) -> StoreError {
    StoreError::integrity_of(
        "read_nodes",
        IntegrityKind::NodeMismatch,
        "the stored nodes do not assemble a root",
        Some(size),
    )
}

/// Every page of one part, each passed to `row`; `Ok(false)` when a page
/// saw another log size than `size` (the ledger grew meanwhile).
pub(crate) async fn each_row<S: Store<P>, const P: u8>(
    store: &S,
    part: ProjectionPart,
    size: u64,
    mut row: impl FnMut(ProjectionRow) -> Result<(), crate::Error>,
) -> Result<bool, crate::Error> {
    let mut page = PageRead::first(PageRead::<()>::MAX_LIMIT).through(size);
    loop {
        let read = store
            .projection(&ProjectionRead {
                part,
                page: page.clone(),
            })
            .await?;
        if read.size != size {
            return Ok(false);
        }
        for item in read.items {
            row(item)?;
        }
        match read.next {
            Some(next) => page = page.after(next),
            None => return Ok(true),
        }
    }
}

/// Reads every part of `store` at log size `through` (default: its size
/// now), paging each one.
///
/// `Ok(None)` when a page saw another log size than `through` (the ledger
/// grew): call again with its new size.
///
/// # Errors
///
/// The first failing read.
pub async fn load_projection<S: Store<P>, const P: u8>(
    store: &S,
    through: Option<u64>,
) -> Result<Option<StoreProjection>, crate::Error> {
    let size = match through {
        Some(through) => through,
        None => store.size().await?,
    };
    let mut projection = StoreProjection::new(head_at(store, size).await?);
    for part in ProjectionPart::ALL {
        let mut rows = Vec::new();
        let complete = each_row(store, part, size, |row| {
            rows.push(row);
            Ok(())
        })
        .await?;
        if !complete {
            return Ok(None);
        }
        projection.parts.insert(part, rows);
    }
    Ok(Some(projection))
}

/// The rows of a part after the opaque cursor `after`, at most `limit + 1`
/// of `rows` (in key order), as a page: what an in-memory store answers.
pub(crate) fn page_of(
    part: ProjectionPart,
    rows: impl Iterator<Item = ProjectionRow>,
    page: &PageRead<Vec<u8>>,
    size: u64,
) -> Result<ProjectionPage, StoreError> {
    let limit = page.checked_limit("projection")?;
    let after = match &page.after {
        Some(bytes) => Some(ProjectionRow::from_bytes(bytes).ok_or_else(|| {
            StoreError::backend_message("projection", "the cursor is not one this store returned")
        })?),
        None => None,
    };
    let wanted = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .saturating_add(1);
    let rows: Vec<(Vec<u8>, ProjectionRow)> = rows
        .filter(|row| after.as_ref().is_none_or(|after| row.key(part) > *after))
        .take(wanted)
        .map(|row| (row.key(part).to_bytes(), row))
        .collect();
    Ok(super::paged(rows, limit, size, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every column of every table of the schema belongs to exactly one
    /// part: a table or column added to `schema.sql` without joining the
    /// projection fails here.
    #[test]
    fn the_parts_cover_every_column_of_the_schema_once() {
        let schema = include_str!("sql/schema.sql");
        let mut columns = Vec::new();
        let mut table = None;
        for line in schema.lines() {
            if let Some(rest) = line.strip_prefix("CREATE TABLE {{s}}") {
                table = rest.split_whitespace().next();
                continue;
            }
            if line.starts_with(')') {
                table = None;
                continue;
            }
            let Some(table) = table else { continue };
            let Some(column) = line
                .strip_prefix("    ")
                .and_then(|rest| rest.split_whitespace().next())
            else {
                continue;
            };
            if column == "CHECK" || column == "PRIMARY" || column == "FOREIGN" {
                continue;
            }
            columns.push(format!("{table}.{column}"));
        }
        let mut covered: Vec<String> = ProjectionPart::ALL
            .iter()
            .flat_map(|part| part.columns())
            .collect();
        columns.sort();
        covered.sort();
        // A column appears twice only as the key the memos part joins on.
        covered.dedup();
        assert_eq!(covered, columns);
        let mut tables: Vec<&str> = ProjectionPart::ALL.iter().map(|p| p.table()).collect();
        tables.sort_unstable();
        tables.dedup();
        assert_eq!(tables.len(), ProjectionPart::ALL.len());
        for part in ProjectionPart::ALL {
            assert!(part.key_len() <= part.layout().len(), "{part}");
        }
    }

    #[test]
    fn rows_round_trip_through_their_encoding() {
        let row = ProjectionRow::new(vec![
            Cell::Null,
            Cell::Int(-1),
            Cell::Bytes(vec![0, 1]),
            Cell::Text("é".into()),
        ]);
        assert_eq!(ProjectionRow::from_bytes(&row.to_bytes()), Some(row));
        assert_eq!(ProjectionRow::from_bytes(&[9]), None);
        assert_eq!(ProjectionRow::from_bytes(&[1, 0]), None);
    }
}
