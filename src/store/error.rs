//! [`StoreError`]: the one error every [`Store`](super::Store) method fails
//! with.

use core::fmt;
use std::error::Error as StdError;

use crate::error::IntegrityKind;

/// The source of a store failure: a driver error, kept for
/// [`source()`](StdError::source) only.
type Source = Box<dyn StdError + Send + Sync + 'static>;

/// A store operation failed.
///
/// Opaque apart from what a caller decides on: the [`kind`](Self::kind),
/// whether a retry may succeed ([`is_transient`](Self::is_transient)), and
/// whether the operation may have been applied although it failed
/// ([`may_have_applied`](Self::may_have_applied) — only for failures raised
/// while or after the commit was sent). The driver error is reachable through
/// [`source()`](StdError::source); [`Display`](fmt::Display) names the
/// operation and the class of failure, never a bound value.
///
/// It converts into [`crate::Error`]: `Integrity` becomes
/// [`Error::Integrity`](crate::Error::Integrity), `SchemaMismatch` and
/// `LedgerMismatch` [`Error::Config`](crate::Error::Config), everything else
/// [`Error::Backend`](crate::Error::Backend) with both flags preserved.
///
/// ```
/// use doubleentry::store::{StoreError, StoreErrorKind};
///
/// let lost = StoreError::transient("write", std::io::Error::other("connection reset"))
///     .outcome_unknown();
/// assert_eq!(lost.kind(), &StoreErrorKind::Unavailable);
/// assert!(lost.is_transient() && lost.may_have_applied());
/// assert_eq!(lost.to_string(), "write: the database is unavailable");
///
/// let error = doubleentry::Error::from(lost);
/// assert!(error.is_transient() && error.may_have_applied());
/// ```
#[derive(Debug)]
pub struct StoreError {
    kind: StoreErrorKind,
    operation: &'static str,
    transient: bool,
    may_have_applied: bool,
    position: Option<u64>,
    integrity: IntegrityKind,
    what: &'static str,
    source: Option<Source>,
}

/// What kind of failure a [`StoreError`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreErrorKind {
    /// No ledger was created in this database (or schema) yet.
    NotCreated,
    /// The database holds another schema; nothing was read or changed.
    SchemaMismatch {
        /// The schema identifier found.
        found: String,
    },
    /// The database holds another ledger, or the same ledger at another
    /// scale.
    LedgerMismatch,
    /// Stored data fails verification. Never retry; investigate.
    Integrity,
    /// The database is unavailable for now (busy, a lock or connection
    /// timeout, a lost connection): a retry may succeed.
    Unavailable,
    /// Any other backend failure.
    Backend,
    /// The configured schema name is not an identifier the store accepts:
    /// `[A-Za-z_][A-Za-z0-9_]{0,62}`. Nothing was connected.
    InvalidSchemaName {
        /// The name refused.
        name: String,
    },
}

impl StoreError {
    fn new(kind: StoreErrorKind, operation: &'static str) -> Self {
        Self {
            kind,
            operation,
            transient: false,
            may_have_applied: false,
            position: None,
            integrity: IntegrityKind::Undecodable,
            what: "",
            source: None,
        }
    }

    /// A transient failure ([`StoreErrorKind::Unavailable`]) of `operation`:
    /// a retry may succeed. Definitely not applied unless marked with
    /// [`outcome_unknown`](Self::outcome_unknown).
    pub fn transient(
        operation: &'static str,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            transient: true,
            source: Some(Box::new(source)),
            ..Self::new(StoreErrorKind::Unavailable, operation)
        }
    }

    /// A permanent backend failure ([`StoreErrorKind::Backend`]) of
    /// `operation`.
    pub fn backend(operation: &'static str, source: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            source: Some(Box::new(source)),
            ..Self::new(StoreErrorKind::Backend, operation)
        }
    }

    /// Stored data of `operation` fails verification: `what` says how (a
    /// fixed text, never a stored value), `position` where when known.
    #[must_use]
    pub fn integrity(operation: &'static str, what: &'static str, position: Option<u64>) -> Self {
        Self::integrity_of(operation, IntegrityKind::Undecodable, what, position)
    }

    /// Like [`integrity`](Self::integrity), classified as `kind`.
    pub(crate) fn integrity_of(
        operation: &'static str,
        kind: IntegrityKind,
        what: &'static str,
        position: Option<u64>,
    ) -> Self {
        Self {
            position,
            integrity: kind,
            what,
            ..Self::new(StoreErrorKind::Integrity, operation)
        }
    }

    /// `operation` found no ledger.
    #[must_use]
    pub fn not_created(operation: &'static str) -> Self {
        Self::new(StoreErrorKind::NotCreated, operation)
    }

    /// `operation` found the schema identifier `found`.
    #[must_use]
    pub fn schema_mismatch(operation: &'static str, found: String) -> Self {
        Self::new(StoreErrorKind::SchemaMismatch { found }, operation)
    }

    /// `operation` found another ledger (or another scale).
    #[must_use]
    pub fn ledger_mismatch(operation: &'static str) -> Self {
        Self::new(StoreErrorKind::LedgerMismatch, operation)
    }

    /// A transient failure of `operation`; attach the cause with
    /// [`with_source`](Self::with_source).
    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    pub(crate) fn unavailable(operation: &'static str) -> Self {
        Self {
            transient: true,
            ..Self::new(StoreErrorKind::Unavailable, operation)
        }
    }

    /// A permanent backend failure of `operation`; attach the cause with
    /// [`with_source`](Self::with_source).
    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    pub(crate) fn failed(operation: &'static str) -> Self {
        Self::new(StoreErrorKind::Backend, operation)
    }

    /// The same failure, caused by `source`.
    #[must_use]
    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    pub(crate) fn with_source(self, source: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            source: Some(Box::new(source)),
            ..self
        }
    }

    /// The schema name `name` is refused at construction.
    #[cfg(feature = "postgres")]
    pub(crate) fn invalid_schema_name(name: &str) -> Self {
        Self::new(
            StoreErrorKind::InvalidSchemaName {
                name: name.to_owned(),
            },
            "connect",
        )
    }

    /// A permanent failure with a fixed message (no source).
    pub(crate) fn backend_message(operation: &'static str, what: &'static str) -> Self {
        Self {
            what,
            ..Self::new(StoreErrorKind::Backend, operation)
        }
    }

    /// The same failure, marked as possibly applied (raised while or after
    /// the commit was sent).
    #[must_use]
    pub fn outcome_unknown(self) -> Self {
        Self {
            may_have_applied: true,
            ..self
        }
    }

    /// What kind of failure this is.
    #[must_use]
    pub const fn kind(&self) -> &StoreErrorKind {
        &self.kind
    }

    /// Whether a retry may succeed.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        self.transient
    }

    /// Whether the operation may have been applied although it failed.
    /// Resolve it by retrying with the same draft: a committed entry comes
    /// back as a replay.
    #[must_use]
    pub const fn may_have_applied(&self) -> bool {
        self.may_have_applied
    }

    /// The operation that failed (`create`, `open`, `write`, `read_leaves`,
    /// …).
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// The log position the failure concerns, when known.
    #[must_use]
    pub const fn position(&self) -> Option<u64> {
        self.position
    }

    /// The integrity class this failure maps to in [`crate::Error`].
    pub(crate) const fn integrity_kind(&self) -> IntegrityKind {
        self.integrity
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.operation)?;
        match &self.kind {
            StoreErrorKind::NotCreated => f.write_str("no ledger was created here")?,
            StoreErrorKind::SchemaMismatch { found } => {
                write!(f, "the database holds schema {found:?}")?;
            }
            StoreErrorKind::LedgerMismatch => f.write_str("the database holds another ledger")?,
            StoreErrorKind::Integrity => f.write_str("stored data fails verification")?,
            StoreErrorKind::Unavailable => f.write_str("the database is unavailable")?,
            StoreErrorKind::Backend => f.write_str("backend failure")?,
            StoreErrorKind::InvalidSchemaName { name } => write!(
                f,
                "the schema name {name:?} is not [A-Za-z_][A-Za-z0-9_]{{0,62}}"
            )?,
        }
        if !self.what.is_empty() {
            write!(f, " ({})", self.what)?;
        }
        if let Some(position) = self.position {
            write!(f, " at position {position}")?;
        }
        Ok(())
    }
}

impl StdError for StoreError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn every_kind_folds_into_the_crate_error() {
        let integrity =
            StoreError::integrity_of("write", IntegrityKind::NodeMismatch, "a node", Some(7));
        match Error::from(integrity) {
            Error::Integrity(error) => assert_eq!(
                (error.kind(), error.position()),
                (IntegrityKind::NodeMismatch, Some(7))
            ),
            other => panic!("{other:?}"),
        }
        match Error::from(StoreError::schema_mismatch(
            "open",
            String::from("doubleentry/0"),
        )) {
            Error::Config(error) => assert_eq!(
                error,
                crate::error::ConfigError::SchemaMismatch {
                    found: String::from("doubleentry/0")
                }
            ),
            other => panic!("{other:?}"),
        }
        match Error::from(StoreError::ledger_mismatch("create")) {
            Error::Config(error) => {
                assert_eq!(error.to_string(), "the store holds another ledger or scale");
            }
            other => panic!("{other:?}"),
        }
        let io = || std::io::Error::other("reset");
        let transient = Error::from(StoreError::transient("write", io()));
        assert!(transient.is_transient() && !transient.may_have_applied());
        let lost = Error::from(StoreError::transient("write", io()).outcome_unknown());
        assert!(lost.is_transient() && lost.may_have_applied());
        let permanent = Error::from(StoreError::backend("write", io()));
        assert!(matches!(permanent, Error::Backend(_)) && !permanent.is_transient());
        let missing = Error::from(StoreError::not_created("open"));
        assert!(matches!(missing, Error::Backend(_)) && !missing.is_transient());
    }

    #[test]
    fn display_names_the_operation_never_a_value_and_keeps_the_source() {
        let error = StoreError::transient("write", std::io::Error::other("secret 1190.00"));
        assert_eq!(error.to_string(), "write: the database is unavailable");
        assert_eq!(error.operation(), "write");
        assert!(StdError::source(&error).is_some());
        let schema = StoreError::schema_mismatch("open", String::from("doubleentry/0"));
        assert_eq!(
            schema.to_string(),
            "open: the database holds schema \"doubleentry/0\""
        );
    }
}
