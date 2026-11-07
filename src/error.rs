//! The payloads [`Error`]'s variants carry. What each variant means and
//! whether to retry it: the [crate-level table](crate#errors).
//!
//! A [`StoreError`] folds into it: stored data the ledger cannot trust
//! becomes [`Error::Integrity`], a store holding another ledger or schema
//! [`Error::Config`], every other store failure [`Error::Backend`].
//! The log's verifier errors ([`ProofError`](crate::log::ProofError),
//! [`DecodeError`](crate::record::DecodeError),
//! [`SealOrderError`](crate::record::SealOrderError)) fold into
//! [`Error::Transparency`].

pub(crate) mod rejection;
pub(crate) mod top;

use core::fmt;
use std::sync::Arc;

pub(crate) use rejection::Rejection;
pub use rejection::{LabelField, ScaledAmount, Stage};
pub(crate) use top::Error;

pub use crate::core::{AuditFailure, AuditFailureKind};
pub use crate::model::{AmountError, BuildError, Field, ValueError};

use time::Date;

use crate::model::{AccountPath, Currency, EntryId, LedgerId};
use crate::store::{StoreError, StoreErrorKind};

/// A read that asked an impossible question: fix the question, not the
/// ledger.
///
/// ```
/// use doubleentry::{AccountKind, BalanceQuery, Error, Journal, LedgerId, date, currency::EUR, error::QueryError};
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
/// let backwards = BalanceQuery::between(date!("2026-03-31"), date!("2026-03-01"));
/// let Err(Error::Query(error)) = books.balance("Assets:Bank", EUR, &backwards) else {
///     panic!("a window that ends before it starts is refused");
/// };
/// assert_eq!(error.to_string(), "the window starts on 2026-03-31, after its end 2026-03-01");
/// # Ok::<(), doubleentry::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// A date window whose start lies after its end.
    #[error("the window starts on {start}, after its end {end}")]
    InvalidWindow {
        /// The first day asked for.
        start: Date,
        /// The last day asked for.
        end: Date,
    },
    /// No period with this id was defined in the log prefix read.
    #[error("no period has the id {id}")]
    UnknownPeriod {
        /// The id asked for.
        id: String,
    },
    /// No account is registered under this path or handle in the log prefix
    /// read — or the text is no account path at all, or names a grouping
    /// node nobody registered (read a subtree with a rollup).
    #[error("no account {account} is registered")]
    UnknownAccount {
        /// The account as it was named.
        account: String,
    },
    /// A name in the query (a period id, a dimension axis or value) is not
    /// a valid label.
    #[error("{text:?} is not a valid label")]
    InvalidLabel {
        /// The text given.
        text: String,
    },
    /// No entry has this id.
    #[error("no entry has the id {id}")]
    UnknownEntry {
        /// The id asked for.
        id: EntryId,
    },
    /// The period has no seal yet, so nothing about its sealed state can be
    /// proven.
    #[error("the period {period} is not sealed")]
    NotSealed {
        /// The period asked for.
        period: String,
    },
    /// A position or tree size beyond the log: the log had `size` records
    /// when it was asked.
    #[error("position {position} is beyond the head of size {size}")]
    PositionBeyondHead {
        /// The position or size asked for.
        position: u64,
        /// The log's size.
        size: u64,
    },
    /// A consistency proof between sizes that do not bound a prefix: `old`
    /// is 0 (nothing to be consistent with) or larger than `new`.
    #[error("no consistency proof from size {old} to size {new}")]
    InvalidSizes {
        /// The old size.
        old: u64,
        /// The new size.
        new: u64,
    },
    /// A sum across accounts does not fit an amount. Per-account balances
    /// are bounded by the write path; totals of many accounts are checked
    /// and refused, never wrapped.
    #[error("an amount cannot hold {context}")]
    Overflow {
        /// Which sum.
        context: OverflowContext,
    },
}

/// Which sum across accounts overflowed ([`QueryError::Overflow`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OverflowContext {
    /// The trial-balance totals of one currency.
    #[error("the totals in {0}")]
    Totals(Currency),
    /// A rollup node's subtree total.
    #[error("the subtree total of {0}")]
    Subtree(AccountPath),
}

/// A backend failure, opaque apart from what a caller decides on: whether a
/// retry may succeed and whether the operation may have been applied.
///
/// ```
/// use doubleentry::error::BackendError;
///
/// let io = std::io::Error::other("connection reset");
/// let failure = BackendError::transient(io).outcome_unknown();
/// assert!(failure.is_transient() && failure.may_have_applied());
/// assert_eq!(failure.to_string(), "connection reset (transient) (may have applied)");
/// ```
#[derive(Clone)]
pub struct BackendError {
    source: Arc<dyn std::error::Error + Send + Sync>,
    transient: bool,
    may_have_applied: bool,
}

impl BackendError {
    /// A failure that will recur and was definitely not applied.
    pub fn permanent(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            source: Arc::new(source),
            transient: false,
            may_have_applied: false,
        }
    }

    /// A failure a retry may get past.
    pub fn transient(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            source: Arc::new(source),
            transient: true,
            may_have_applied: false,
        }
    }

    /// The same failure, marked as possibly applied (for example a commit
    /// whose acknowledgement was lost).
    #[must_use]
    pub fn outcome_unknown(self) -> Self {
        Self {
            may_have_applied: true,
            ..self
        }
    }

    /// Whether a retry may succeed.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        self.transient
    }

    /// Whether the operation may have been applied.
    #[must_use]
    pub const fn may_have_applied(&self) -> bool {
        self.may_have_applied
    }
}

impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendError")
            .field("source", &self.source)
            .field("transient", &self.transient)
            .field("may_have_applied", &self.may_have_applied)
            .finish()
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)?;
        if self.transient {
            f.write_str(" (transient)")?;
        }
        if self.may_have_applied {
            f.write_str(" (may have applied)")?;
        }
        Ok(())
    }
}

impl std::error::Error for BackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// The ledger's own data fails verification: a record, node, balance or seal
/// that the rule core would not have produced. Never retried: stop writing,
/// investigate [`position`](Self::position), restore and audit.
///
/// ```
/// use doubleentry::{Error, error::{IntegrityError, IntegrityKind}};
///
/// let error = Error::from(IntegrityError::new(IntegrityKind::NodeMismatch, Some(41)));
/// let Error::Integrity(integrity) = &error else { unreachable!() };
/// assert_eq!((integrity.kind(), integrity.position()), (IntegrityKind::NodeMismatch, Some(41)));
/// assert!(!error.is_transient());
/// assert!(error.to_string().starts_with("integrity failure at position 41: "));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityError {
    kind: IntegrityKind,
    position: Option<u64>,
    audit: Option<Box<AuditFailure>>,
}

impl IntegrityError {
    /// A failure of `kind`, at a log position when one is known.
    #[must_use]
    pub const fn new(kind: IntegrityKind, position: Option<u64>) -> Self {
        Self {
            kind,
            position,
            audit: None,
        }
    }

    /// What failed.
    #[must_use]
    pub const fn kind(&self) -> IntegrityKind {
        self.kind
    }

    /// The log position of the first failing record, when known.
    #[must_use]
    pub const fn position(&self) -> Option<u64> {
        self.position
    }
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.audit {
            Some(audit) => write!(f, "{}", audit.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

impl std::error::Error for IntegrityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.audit
            .as_deref()
            .map(|audit| audit as &(dyn std::error::Error + 'static))
    }
}

impl From<AuditFailure> for IntegrityError {
    /// Classifies an audit failure; the failure itself stays available as the
    /// error's [`source`](std::error::Error::source).
    fn from(failure: AuditFailure) -> Self {
        let kind = match &failure.kind {
            AuditFailureKind::Undecodable(_)
            | AuditFailureKind::NonCanonical
            | AuditFailureKind::GenesisMissing => IntegrityKind::Undecodable,
            AuditFailureKind::GenesisMismatch => IntegrityKind::GenesisMismatch,
            AuditFailureKind::SealMismatch { .. } | AuditFailureKind::SealTotalsDecrease { .. } => {
                IntegrityKind::SealMismatch
            }
            AuditFailureKind::HeadMismatch => IntegrityKind::HeadMismatch,
            AuditFailureKind::ProjectionMismatch { part, .. } => part.integrity_kind(),
            AuditFailureKind::PositionMismatch { .. } => IntegrityKind::MissingRecord,
            AuditFailureKind::WouldReject(_)
            | AuditFailureKind::NotProduced
            | AuditFailureKind::HandleMismatch { .. }
            | AuditFailureKind::MemoMismatch
            | AuditFailureKind::FingerprintMismatch => IntegrityKind::ReplayMismatch,
        };
        Self {
            kind,
            position: failure.position,
            audit: Some(Box::new(failure)),
        }
    }
}

impl From<StoreError> for Error {
    /// `Integrity` is stored data the ledger cannot trust
    /// ([`Error::Integrity`]); `SchemaMismatch`, `LedgerMismatch`,
    /// `ForeignSchema` and the invalid schema and role names are a store
    /// configured for something else ([`Error::Config`]); everything
    /// else is a [`BackendError`] keeping both flags, with the store error as
    /// its source.
    fn from(error: StoreError) -> Self {
        match error.kind() {
            StoreErrorKind::Integrity => {
                IntegrityError::new(error.integrity_kind(), error.position()).into()
            }
            StoreErrorKind::SchemaMismatch { found } => Self::Config(ConfigError::SchemaMismatch {
                found: found.clone(),
            }),
            StoreErrorKind::LedgerMismatch => Self::Config(ConfigError::LedgerMismatch {
                expected: None,
                found: None,
            }),
            StoreErrorKind::ForeignSchema { schema, tables } => {
                Self::Config(ConfigError::ForeignSchema {
                    schema: schema.clone(),
                    tables: tables.clone(),
                })
            }
            StoreErrorKind::InvalidSchemaName { name } => {
                Self::Config(ConfigError::InvalidSchemaName { name: name.clone() })
            }
            StoreErrorKind::InvalidRoleName { name } => {
                Self::Config(ConfigError::InvalidRoleName { name: name.clone() })
            }
            _ => {
                let applied = error.may_have_applied();
                let backend = if error.is_transient() {
                    BackendError::transient(error)
                } else {
                    BackendError::permanent(error)
                };
                Self::Backend(if applied {
                    backend.outcome_unknown()
                } else {
                    backend
                })
            }
        }
    }
}

impl From<AuditFailure> for Error {
    fn from(failure: AuditFailure) -> Self {
        Self::Integrity(failure.into())
    }
}

impl From<BuildError> for Error {
    /// The stage-1 [`Rejection`] a draft with this deferred error gets from
    /// `record`.
    fn from(error: BuildError) -> Self {
        Self::Rejected(crate::core::build_rejection(&error))
    }
}

/// What kind of stored data failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum IntegrityKind {
    /// The tree head differs from the one the leaves produce.
    #[error("the tree head does not match the leaves")]
    HeadMismatch,
    /// A stored tree node differs from the one the leaves produce.
    #[error("a stored tree node does not match the leaves")]
    NodeMismatch,
    /// A record the ledger refers to is missing.
    #[error("a record is missing")]
    MissingRecord,
    /// A stored record does not decode.
    #[error("a stored record does not decode")]
    Undecodable,
    /// A maintained balance differs from the replayed one.
    #[error("a maintained balance does not match the replay")]
    BalanceMismatch,
    /// A seal differs from the one the rule core computes.
    #[error("a seal does not match the replay")]
    SealMismatch,
    /// A clearing differs from the replayed one.
    #[error("a clearing does not match the replay")]
    ClearingMismatch,
    /// A hold's state, or an amount open holds reserve, differs from the
    /// replayed one.
    #[error("a hold does not match the replay")]
    HoldMismatch,
    /// The number of entries differs from the replayed one.
    #[error("the entry count does not match the replay")]
    EntryCountMismatch,
    /// The genesis record differs from the one the ledger's meta data names.
    #[error("the genesis record does not match the ledger")]
    GenesisMismatch,
    /// A record or index the rule core would not have produced.
    #[error("a record or index the rule core would not have produced")]
    ReplayMismatch,
}

/// A store configured for something other than what was opened
/// ([`Error::Config`]): fix the URL, schema name, ledger id or scale, not the
/// data.
///
/// ```
/// use doubleentry::store::MemoryStore;
/// use doubleentry::{Error, Ledger, LedgerId};
///
/// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
/// let store = MemoryStore::new();
/// Ledger::new(store.clone(), LedgerId::new("example.com/acme")?).await?;
/// let wrong = Ledger::new(store, LedgerId::new("example.com/other")?).await;
/// let Err(Error::Config(error)) = wrong else { panic!("another ledger id is a configuration error") };
/// assert_eq!(
///     error.to_string(),
///     "the store holds the ledger example.com/acme (P = 2), not example.com/other (P = 2)"
/// );
/// # Ok::<(), Error>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The store holds another ledger, or this ledger at another scale `P`.
    #[error("{}", ledger_mismatch(expected.as_deref(), found.as_deref()))]
    LedgerMismatch {
        /// The ledger asked for (`id (P = n)`), when known.
        expected: Option<String>,
        /// The ledger the store holds (`id (P = n)`), when known.
        found: Option<String>,
    },
    /// The database was written by another schema (or by no version of
    /// this crate).
    #[error(
        "the database holds the schema {found:?}, not {:?}",
        crate::store::SCHEMA_ID
    )]
    SchemaMismatch {
        /// The schema identifier found.
        found: String,
    },
    /// The schema (PostgreSQL) or database file (SQLite) holds tables but no
    /// doubleentry ledger; nothing was created or changed there. Use an
    /// empty schema (or file) for the ledger.
    #[error("{}", foreign_schema(schema, tables))]
    ForeignSchema {
        /// The schema (`main` for a SQLite file).
        schema: String,
        /// The tables found, sorted.
        tables: Vec<String>,
    },
    /// The configured schema name is not `[A-Za-z_][A-Za-z0-9_]{0,62}`;
    /// nothing was connected.
    #[error("the schema name {name:?} is not [A-Za-z_][A-Za-z0-9_]{{0,62}}")]
    InvalidSchemaName {
        /// The name refused.
        name: String,
    },
    /// A role name is not `[A-Za-z_][A-Za-z0-9_]{0,62}`; nothing was run.
    #[error("the role name {name:?} is not [A-Za-z_][A-Za-z0-9_]{{0,62}}")]
    InvalidRoleName {
        /// The name refused.
        name: String,
    },
}

/// The tables shown in a [`ConfigError::ForeignSchema`] message.
const SHOWN_TABLES: usize = 8;

/// `the schema "s" holds tables that are not a doubleentry ledger: a, b (and
/// N more)`, naming a doubleentry 0.7 ledger when its `ledger_meta` is there.
pub(crate) fn foreign_schema(schema: &str, tables: &[String]) -> String {
    let shown: Vec<&str> = tables
        .iter()
        .take(SHOWN_TABLES)
        .map(String::as_str)
        .collect();
    let mut text = format!(
        "the schema {schema:?} holds tables that are not a doubleentry ledger: {}",
        shown.join(", ")
    );
    if let Some(more) = tables
        .len()
        .checked_sub(SHOWN_TABLES)
        .filter(|more| *more > 0)
    {
        text.push_str(&format!(" (and {more} more)"));
    }
    if tables.iter().any(|table| table == "ledger_meta") {
        text.push_str(
            "; it is a doubleentry 0.7 ledger, which this release cannot read: \
             keep it with 0.7 and create this ledger in another schema",
        );
    }
    text
}

impl ConfigError {
    /// The store holds `found` (id, scale) where `expected` was asked for.
    pub(crate) fn ledgers(expected: (&LedgerId, u8), found: (&LedgerId, u8)) -> Self {
        Self::LedgerMismatch {
            expected: Some(format!("{} (P = {})", expected.0, expected.1)),
            found: Some(format!("{} (P = {})", found.0, found.1)),
        }
    }
}

fn ledger_mismatch(expected: Option<&str>, found: Option<&str>) -> String {
    match (expected, found) {
        (Some(expected), Some(found)) => {
            format!("the store holds the ledger {found}, not {expected}")
        }
        _ => String::from("the store holds another ledger or scale"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::AuditFailureKind as K;
    use crate::model::Layer;
    use crate::store::ProjectionPart as Part;

    fn kind_of(kind: K) -> IntegrityKind {
        IntegrityError::from(AuditFailure {
            position: Some(3),
            kind,
        })
        .kind()
    }

    /// Every audit failure folds into its integrity kind, projection parts
    /// included.
    #[test]
    fn audit_failures_map_to_their_integrity_kind() {
        let label = crate::model::Label::new("2026-03").unwrap_or_else(|_| unreachable!());
        let cases = [
            (K::NonCanonical, IntegrityKind::Undecodable),
            (K::GenesisMissing, IntegrityKind::Undecodable),
            (K::GenesisMismatch, IntegrityKind::GenesisMismatch),
            (
                K::SealMismatch { period: label },
                IntegrityKind::SealMismatch,
            ),
            (
                K::SealTotalsDecrease {
                    currency: crate::model::EUR,
                    layer: Layer::Settled,
                },
                IntegrityKind::SealMismatch,
            ),
            (K::HeadMismatch, IntegrityKind::HeadMismatch),
            (
                K::PositionMismatch { expected: 4 },
                IntegrityKind::MissingRecord,
            ),
            (K::NotProduced, IntegrityKind::ReplayMismatch),
            (K::MemoMismatch, IntegrityKind::ReplayMismatch),
            (K::FingerprintMismatch, IntegrityKind::ReplayMismatch),
        ];
        for (failure, expected) in cases {
            assert_eq!(kind_of(failure.clone()), expected, "{failure}");
        }
        let parts = [
            (Part::Meta, IntegrityKind::GenesisMismatch),
            (Part::Nodes, IntegrityKind::NodeMismatch),
            (Part::Balances, IntegrityKind::BalanceMismatch),
            (Part::Holds, IntegrityKind::HoldMismatch),
            (Part::Outstanding, IntegrityKind::HoldMismatch),
            (Part::ClearingResets, IntegrityKind::ClearingMismatch),
            (Part::SealNodes, IntegrityKind::SealMismatch),
            (Part::Entries, IntegrityKind::EntryCountMismatch),
            (Part::Accounts, IntegrityKind::ReplayMismatch),
        ];
        for (part, expected) in parts {
            let failure = K::ProjectionMismatch {
                part,
                expected: None,
                found: None,
            };
            assert_eq!(kind_of(failure), expected, "{part}");
        }
    }

    /// `Debug` is the `Display` line, so `main` prints the code.
    #[test]
    fn debug_shows_the_code_and_the_message() {
        let error = Error::from(Rejection::TooFewPostings { count: 1 });
        assert_eq!(
            format!("{error:?}"),
            "rejected: [101] an entry needs at least two postings, found 1"
        );
        assert!(format!("{error:#?}").starts_with("Rejected("));
        assert_eq!(error.code(), Some(101));
        let config = Error::from(ConfigError::SchemaMismatch {
            found: String::from("other/1"),
        });
        assert_eq!(
            format!("{config:?}"),
            "configuration: the database holds the schema \"other/1\", not \"doubleentry/2\""
        );
        assert_eq!(config.code(), None);
    }

    #[test]
    fn a_foreign_schema_names_its_tables_and_a_0_7_ledger() {
        let tables = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        let one = ConfigError::ForeignSchema {
            schema: String::from("ledger"),
            tables: tables(&["invoices"]),
        };
        assert_eq!(
            one.to_string(),
            "the schema \"ledger\" holds tables that are not a doubleentry ledger: invoices"
        );
        let many = ConfigError::ForeignSchema {
            schema: String::from("main"),
            tables: tables(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]),
        };
        assert!(
            many.to_string()
                .ends_with(": a, b, c, d, e, f, g, h (and 2 more)"),
            "{many}"
        );
        let old = ConfigError::ForeignSchema {
            schema: String::from("doubleentry"),
            tables: tables(&["accounts", "ledger_meta"]),
        };
        assert_eq!(
            old.to_string(),
            "the schema \"doubleentry\" holds tables that are not a doubleentry ledger: \
             accounts, ledger_meta; it is a doubleentry 0.7 ledger, which this release cannot \
             read: keep it with 0.7 and create this ledger in another schema"
        );
        let error = Error::from(crate::store::StoreError::foreign_schema(
            "open",
            String::from("main"),
            tables(&["x"]),
        ));
        assert!(matches!(
            error,
            Error::Config(ConfigError::ForeignSchema { .. })
        ));
        assert!(!error.is_transient() && !error.may_have_applied());
    }
}
