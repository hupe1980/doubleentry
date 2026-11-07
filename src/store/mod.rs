//! Where a ledger is kept: the [`Store`] contract, the in-memory
//! [`MemoryStore`] and the one SQL implementation [`SqlStore`].
//!
//! A store decides nothing. Its [`write`](Store::write) takes the ledger's
//! single-writer lock inside one transaction, hands the rule core a bounded
//! [`Snapshot`] of exactly the state the operation needs (asking [`plan`]
//! which facts), runs [`execute`], persists the [`WriteSet`] and commits. Its
//! reads answer from indexes, never by scanning history, each in one read
//! snapshot that never waits for the writer.
//!
//! - **One ledger per store**: one PostgreSQL schema, one SQLite file, one
//!   memory state. PostgreSQL stores of many schemas share one connection
//!   pool (`SqlStore::with_schema`).
//! - **Create once, open always**: [`create`](Store::create) writes the
//!   genesis record (and, on SQL, the schema); calling it again with the same
//!   genesis is a no-op, so every replica may call it at start-up.
//!   [`open`](Store::open) never changes anything and refuses a database of
//!   another schema or ledger. There are no migrations.
//! - **Locks**: SQLite `BEGIN IMMEDIATE` (one writer per file); PostgreSQL
//!   `READ COMMITTED` pinned per transaction plus a transaction advisory
//!   lock keyed by the schema and the ledger id, so two schemas never wait
//!   for each other — even holding the same ledger id. Readers take no lock.
//! - **Audit**: everything a store keeps is part of its projection
//!   ([`ProjectionPart`]: every table, every column) and is compared with
//!   the replay of its log; reads that hand a stored record on verify it
//!   first (its id derives from its key, its leaf hashes to the stored leaf
//!   hash).
//! - **Erasure**: [`erase_memo`](Store::erase_memo) removes an entry's memo
//!   text and salt (and its draft fingerprint) without changing any hash.
//!   When to erase is the caller's decision, tied to the end of the
//!   retention period.
//!
//! `Ledger<S>` drives a store; implementers of a new store check it with the
//! public conformance suite (`doubleentry::testing`, feature `testing`).

mod error;
mod memory;
mod projection;
pub(crate) mod read;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod sql;

use core::future::Future;

pub use error::{StoreError, StoreErrorKind};
pub use memory::MemoryStore;
#[cfg(feature = "testing")]
pub(crate) use memory::honest;
pub(crate) use memory::{MemView, apply};
pub use projection::{
    Cell, ProjectionPage, ProjectionPart, ProjectionRead, ProjectionRow, StoreProjection,
    load_projection,
};
pub(crate) use projection::{cells, each_row, head_at, page_of};
pub use read::{
    AccountRow, BalanceRead, BalanceScope, EntryRef, Erasure, HoldRead, HoldRow, KindFilter, Leaf,
    LeafRead, LedgerInfo, Memo, Position, PostingCursor, PostingLine, PostingOrder, PostingRead,
    SealRow, StoredEntry,
};
pub(crate) use read::{bound, checked_limit, paged};
#[cfg(feature = "postgres")]
pub use sql::Postgres;
#[cfg(feature = "sqlite")]
pub use sql::Sqlite;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use sql::{Dialect, SqlOptions, SqlStore};

// The store protocol of the rule core — the operation, its outcome, the
// write set, the read plan and every view a `Fact` carries — the replay an
// audit runs, and the log's node numbering: everything an implementer of a
// third-party store needs, one path each.
pub use crate::core::{
    AccountTerm, AccountView, BalanceDelta, BalanceKey, ChangeInput, ClearingView, Done, EntryMeta,
    EntryView, Fact, Header, HoldEffect, HoldView, IndexRow, LatestTerms, MemoPlain, Movement, Op,
    Outcome, Outstanding, PeriodView, PostingRow, Read, Replayer, Slice, Snapshot, StateView,
    Terms, Versioned, Window, WriteSet, execute, plan,
};
pub use crate::log::nodes::{
    AppendPlan, ConsistencyPlan, InclusionPlan, MAX_TREE_SIZE, RootPlan, split_stored,
    stored_count, stored_index,
};
pub use crate::log::tree::Tree;

use crate::error::Rejection;
use crate::log::Hash;
use crate::model::{
    AccountHandle, AccountRef, Balance, Currency, EntryId, Genesis, Layer, PeriodId,
};
use crate::{HoldCursor, Page, PageRead};

/// The identifier of the schema this crate writes, recorded in every store
/// it creates; any other identifier is refused.
pub const SCHEMA_ID: &str = "doubleentry/2";

/// The error of a write whose read plan did not converge — a bug guard,
/// never a silent "absent".
pub(crate) fn plan_did_not_converge() -> StoreError {
    StoreError::backend_message("write", "the read plan did not converge")
}

/// The backend contract: where one ledger is kept.
///
/// Eighteen primitives: the lifecycle ([`create`](Self::create),
/// [`open`](Self::open)), the write pipeline ([`write`](Self::write)), the
/// log, the indexes, the values and the one erasable datum. Proofs at any
/// historical size, statements, reports and audit are built once over these
/// by the façade. Every method returns a `Send` future; implementers may
/// write `async fn`. Every method fails only with [`StoreError`].
///
/// A store is a cheap handle (a pool, an `Arc`) and is used generically as
/// `Ledger<S>`; it is not a trait object. To choose a store at run time,
/// wrap the candidates in an enum of your own:
///
/// ```
/// # #[cfg(feature = "sqlite")]
/// # mod example {
/// use doubleentry::store::*;
/// use doubleentry::{AccountHandle, AccountRef, Balance, Currency, EntryId, Layer, PeriodId, log::Hash, record::Genesis, store::BalanceKey};
/// use doubleentry::{HoldCursor, Page, PageRead, Rejection};
///
/// /// The store this service was configured with.
/// #[derive(Clone)]
/// pub enum AnyStore {
///     Memory(MemoryStore),
///     Sqlite(SqlStore<Sqlite>),
/// }
///
/// macro_rules! delegate {
///     ($self:ident, $store:ident => $call:expr) => {
///         match $self {
///             AnyStore::Memory($store) => $call.await,
///             AnyStore::Sqlite($store) => $call.await,
///         }
///     };
/// }
///
/// impl Store<2> for AnyStore {
///     async fn create(&self, genesis: &Genesis) -> Result<LedgerInfo, StoreError> {
///         delegate!(self, s => s.create(genesis))
///     }
///     async fn open(&self) -> Result<LedgerInfo, StoreError> {
///         delegate!(self, s => s.open())
///     }
///     async fn write(&self, op: &Op<2>) -> Result<Result<Done<2>, Rejection>, StoreError> {
///         delegate!(self, s => s.write(op))
///     }
///     async fn size(&self) -> Result<u64, StoreError> {
///         delegate!(self, s => s.size())
///     }
///     async fn read_leaves(&self, read: &LeafRead) -> Result<Page<Leaf, Position>, StoreError> {
///         delegate!(self, s => s.read_leaves(read))
///     }
///     async fn read_nodes(&self, ids: &[u64]) -> Result<Vec<Hash>, StoreError> {
///         delegate!(self, s => s.read_nodes(ids))
///     }
///     async fn seals(&self, through: Option<u64>) -> Result<Vec<SealRow>, StoreError> {
///         delegate!(self, s => s.seals(through))
///     }
///     async fn accounts(&self, refs: &[AccountRef]) -> Result<Vec<Option<AccountRow>>, StoreError> {
///         delegate!(self, s => s.accounts(refs))
///     }
///     async fn account_records(
///         &self,
///         handle: AccountHandle,
///         page: PageRead<Position>,
///     ) -> Result<Page<Leaf, Position>, StoreError> {
///         delegate!(self, s => s.account_records(handle, page))
///     }
///     async fn entry(
///         &self,
///         by: EntryRef<'_>,
///         through: Option<u64>,
///     ) -> Result<Option<StoredEntry>, StoreError> {
///         delegate!(self, s => s.entry(by, through))
///     }
///     async fn holds(&self, read: &HoldRead) -> Result<Page<HoldRow<2>, HoldCursor>, StoreError> {
///         delegate!(self, s => s.holds(read))
///     }
///     async fn clearings(
///         &self,
///         key: BalanceKey,
///         page: PageRead<Position>,
///     ) -> Result<Page<Leaf, Position>, StoreError> {
///         delegate!(self, s => s.clearings(key, page))
///     }
///     async fn projection(&self, read: &ProjectionRead) -> Result<ProjectionPage<2>, StoreError> {
///         delegate!(self, s => s.projection(read))
///     }
///     async fn seal_nodes(
///         &self,
///         period: &PeriodId,
///         currency: Currency,
///         layer: Layer,
///         ids: &[(u32, u32)],
///     ) -> Result<Option<Vec<Option<Hash>>>, StoreError> {
///         delegate!(self, s => s.seal_nodes(period, currency, layer, ids))
///     }
///     async fn balances(
///         &self,
///         read: &BalanceRead,
///     ) -> Result<Page<(BalanceKey, Balance), BalanceKey>, StoreError> {
///         delegate!(self, s => s.balances(read))
///     }
///     async fn postings(
///         &self,
///         read: &PostingRead,
///     ) -> Result<Page<PostingLine<2>, PostingCursor>, StoreError> {
///         delegate!(self, s => s.postings(read))
///     }
///     async fn memo(&self, id: EntryId) -> Result<Option<Memo>, StoreError> {
///         delegate!(self, s => s.memo(id))
///     }
///     async fn erase_memo(&self, id: EntryId) -> Result<Erasure, StoreError> {
///         delegate!(self, s => s.erase_memo(id))
///     }
/// }
/// # }
/// ```
pub trait Store<const P: u8 = 2>: Clone + Send + Sync + 'static {
    /// Creates the ledger: writes `genesis` at position 0 (on SQL, after the
    /// schema). On a store that already holds exactly this genesis it
    /// succeeds without writing.
    ///
    /// # Errors
    ///
    /// `LedgerMismatch` when the store holds another ledger or scale,
    /// `SchemaMismatch` when it holds another schema.
    fn create(
        &self,
        genesis: &Genesis,
    ) -> impl Future<Output = Result<LedgerInfo, StoreError>> + Send;

    /// Opens the ledger, changing nothing.
    ///
    /// # Errors
    ///
    /// `NotCreated` before [`create`](Self::create), `SchemaMismatch` for
    /// another schema, `LedgerMismatch` for another scale `P`, `Integrity`
    /// when the genesis does not match its record.
    fn open(&self) -> impl Future<Output = Result<LedgerInfo, StoreError>> + Send;

    /// Runs one operation through the rule core inside one transaction under
    /// the ledger's lock: header, read plan (at most [`Op::max_rounds`] rounds), execute,
    /// persist, commit. A rejection or an operation already in effect writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// The store's failure; the core's refusal is the inner `Err`.
    fn write(
        &self,
        op: &Op<P>,
    ) -> impl Future<Output = Result<Result<Done<P>, Rejection>, StoreError>> + Send;

    /// The number of committed records.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn size(&self) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// Leaves in position order, optionally of some kinds only.
    ///
    /// # Errors
    ///
    /// The store's failure; `Backend` for a limit outside `1..=1000`.
    fn read_leaves(
        &self,
        read: &LeafRead,
    ) -> impl Future<Output = Result<Page<Leaf, Position>, StoreError>> + Send;

    /// Stored tree nodes (Go `tlog` numbering) in the order asked, of any
    /// committed size: what every proof at every historical size is built
    /// from.
    ///
    /// # Errors
    ///
    /// `Integrity` for a node missing below the stored count.
    fn read_nodes(&self, ids: &[u64])
    -> impl Future<Output = Result<Vec<Hash>, StoreError>> + Send;

    /// Every seal below `through`, in position order.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn seals(
        &self,
        through: Option<u64>,
    ) -> impl Future<Output = Result<Vec<SealRow>, StoreError>> + Send;

    /// Accounts by path or handle, in the order asked (`None`: unknown).
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn accounts(
        &self,
        refs: &[AccountRef],
    ) -> impl Future<Output = Result<Vec<Option<AccountRow>>, StoreError>> + Send;

    /// An account's records (its registration and changes), by position.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn account_records(
        &self,
        handle: AccountHandle,
        page: PageRead<Position>,
    ) -> impl Future<Output = Result<Page<Leaf, Position>, StoreError>> + Send;

    /// An entry by id or key, recorded below `through`.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn entry(
        &self,
        by: EntryRef<'_>,
        through: Option<u64>,
    ) -> impl Future<Output = Result<Option<StoredEntry>, StoreError>> + Send;

    /// Holds: one by id, the open ones of an account, or the open ones due
    /// by a date — from an index of open holds only.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn holds(
        &self,
        read: &HoldRead,
    ) -> impl Future<Output = Result<Page<HoldRow<P>, HoldCursor>, StoreError>> + Send;

    /// The clearing records touching a posting of one balance, by position.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn clearings(
        &self,
        key: BalanceKey,
        page: PageRead<Position>,
    ) -> impl Future<Output = Result<Page<Leaf, Position>, StoreError>> + Send;

    /// One page of one table, as [`ProjectionPart`] defines its rows, for
    /// [`load_projection`] and the audit: everything the store keeps, read
    /// as it is (the page's `size` says at which log size).
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn projection(
        &self,
        read: &ProjectionRead,
    ) -> impl Future<Output = Result<ProjectionPage<P>, StoreError>> + Send;

    /// Stored nodes of the balance tree the seal of `period` holds for
    /// (`currency`, `layer`), by `(level, index)` in the order asked: the
    /// aligned subtrees with a non-zero leaf ([`ProjectionPart::SealNodes`]);
    /// `None` for any other (a zero subtree). A sealed-balance proof reads
    /// `O(log² accounts)` of them instead of every leaf of the tree.
    ///
    /// The default answers `Ok(None)` — "not served": proofs then rebuild
    /// the tree from the seal's leaves. A store still keeps the nodes its
    /// projection holds.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn seal_nodes(
        &self,
        period: &PeriodId,
        currency: Currency,
        layer: Layer,
        ids: &[(u32, u32)],
    ) -> impl Future<Output = Result<Option<Vec<Option<Hash>>>, StoreError>> + Send {
        let _ = (period, currency, layer, ids);
        async { Ok(None) }
    }

    /// Balances now, after a position, or as sealed — never by summing.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn balances(
        &self,
        read: &BalanceRead,
    ) -> impl Future<Output = Result<Page<(BalanceKey, Balance<P>), BalanceKey>, StoreError>> + Send;

    /// A page of one balance's postings with filters — a range scan of an
    /// index, never a sort of the account's history.
    ///
    /// # Errors
    ///
    /// The store's failure; `Backend` for more than eight dimension filters.
    fn postings(
        &self,
        read: &PostingRead,
    ) -> impl Future<Output = Result<Page<PostingLine<P>, PostingCursor>, StoreError>> + Send;

    /// An entry's memo; `None` for an unknown entry.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn memo(&self, id: EntryId) -> impl Future<Output = Result<Option<Memo>, StoreError>> + Send;

    /// Erases an entry's memo text and salt and its draft fingerprint,
    /// changing no leaf, node, index or balance.
    ///
    /// # Errors
    ///
    /// The store's failure.
    fn erase_memo(&self, id: EntryId) -> impl Future<Output = Result<Erasure, StoreError>> + Send;
}
