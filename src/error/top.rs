//! [`Error`], the crate's one error type, and [`Result`].

use core::fmt;

use super::{BackendError, ConfigError, IntegrityError, QueryError};
use crate::error::Rejection;
use crate::log::ProofError;
use crate::model::{AmountError, DecodeError, SealOrderError, ValueError, pad};
use crate::transparency;

/// Everything a ledger operation can fail with.
///
/// ```
/// use doubleentry::{Error, Rejection};
///
/// let error = Error::from(Rejection::TooFewPostings { count: 1 });
/// assert_eq!(error.rejection().map(Rejection::code), Some(101));
/// assert!(!error.is_transient());
/// assert_eq!(error.to_string(), "rejected: [101] an entry needs at least two postings, found 1");
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub enum Error {
    /// The ledger refused the input. Final for this input: do not retry it
    /// unchanged.
    Rejected(Rejection),
    /// The ledger refused draft `index` of an atomic batch
    /// ([`Journal::record_batch`](crate::Journal::record_batch)); nothing of
    /// the batch was recorded. Final for this batch: do not retry it
    /// unchanged.
    BatchRejected {
        /// The refused draft's index in the batch.
        index: usize,
        /// Why it was refused.
        rejection: Rejection,
    },
    /// A standalone value constructor failed. Never returned by a ledger
    /// operation: a malformed draft is [`Error::Rejected`].
    Value(ValueError),
    /// Money parsing or arithmetic failed. Never returned by a ledger
    /// operation.
    Amount(AmountError),
    /// A read asked an impossible question.
    Query(QueryError),
    /// A transparency artefact — a note, checkpoint, cosignature, policy,
    /// proof, bundle or verifier step — was refused. Final for this input.
    Transparency(transparency::Error),
    /// The ledger's own stored data fails verification. Never retry;
    /// investigate.
    Integrity(IntegrityError),
    /// The backend failed. See [`is_transient`](Self::is_transient) and
    /// [`may_have_applied`](Self::may_have_applied).
    Backend(BackendError),
    /// The store holds another ledger, this ledger at another scale, or a
    /// database of another schema: a configuration mistake (a wrong URL,
    /// schema name, ledger id or `P`), not damaged data. Nothing was read or
    /// changed; fix the configuration.
    Config(ConfigError),
}

/// The same line as `Display` — `rejected: [427] …` — so a `main` returning
/// this error prints its code and message; `{:#?}` shows the structure.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !f.alternate() {
            return fmt::Display::fmt(self, f);
        }
        match self {
            Self::Rejected(rejection) => f.debug_tuple("Rejected").field(rejection).finish(),
            Self::BatchRejected { index, rejection } => f
                .debug_struct("BatchRejected")
                .field("index", index)
                .field("rejection", rejection)
                .finish(),
            Self::Value(error) => f.debug_tuple("Value").field(error).finish(),
            Self::Amount(error) => f.debug_tuple("Amount").field(error).finish(),
            Self::Query(error) => f.debug_tuple("Query").field(error).finish(),
            Self::Transparency(error) => f.debug_tuple("Transparency").field(error).finish(),
            Self::Integrity(error) => f.debug_tuple("Integrity").field(error).finish(),
            Self::Backend(error) => f.debug_tuple("Backend").field(error).finish(),
            Self::Config(error) => f.debug_tuple("Config").field(error).finish(),
        }
    }
}

fn position_suffix(position: Option<u64>) -> String {
    position.map_or_else(String::new, |position| format!(" at position {position}"))
}

/// One line — `rejected: …`, `query: …`, `integrity failure at position 7:
/// …` — honouring width, fill and alignment and never cut short.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Rejected(rejection) => format!("rejected: {rejection}"),
            Self::BatchRejected { index, rejection } => {
                format!("rejected: draft {index} of the batch: {rejection}")
            }
            Self::Value(error) => format!("invalid value: {error}"),
            Self::Amount(error) => format!("amount: {error}"),
            Self::Query(error) => format!("query: {error}"),
            Self::Transparency(error) => format!("transparency: {error}"),
            Self::Integrity(error) => format!(
                "integrity failure{}: {error}",
                position_suffix(error.position())
            ),
            Self::Backend(error) => format!("backend failure: {error}"),
            Self::Config(error) => format!("configuration: {error}"),
        };
        pad(f, &text)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Rejected(rejection) | Self::BatchRejected { rejection, .. } => rejection,
            Self::Value(error) => error,
            Self::Amount(error) => error,
            Self::Query(error) => error,
            Self::Transparency(error) => error,
            Self::Integrity(error) => error,
            Self::Backend(error) => error,
            Self::Config(error) => error,
        })
    }
}

/// The crate's result type.
pub type Result<T, E = Error> = core::result::Result<T, E>;

impl Error {
    /// Whether retrying the same operation may succeed: only a transient
    /// backend failure.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Backend(backend) if backend.is_transient())
    }

    /// Whether the operation may have been applied although it failed: only a
    /// backend failure whose outcome is unknown. Retry with the same draft —
    /// idempotency makes the retry safe.
    #[must_use]
    pub fn may_have_applied(&self) -> bool {
        matches!(self, Self::Backend(backend) if backend.may_have_applied())
    }

    /// The stable rejection code, when the ledger refused the input — the
    /// number an HTTP handler or a log line keys on.
    ///
    /// ```
    /// use doubleentry::{Error, Rejection};
    ///
    /// let error = Error::from(Rejection::TooFewPostings { count: 1 });
    /// assert_eq!(error.code(), Some(101));
    /// assert_eq!(format!("{error:?}"), "rejected: [101] an entry needs at least two postings, found 1");
    /// ```
    #[must_use]
    pub fn code(&self) -> Option<u16> {
        self.rejection().map(Rejection::code)
    }

    /// The rejection, when the ledger refused the input (a single operation
    /// or a draft of a batch).
    #[must_use]
    pub fn rejection(&self) -> Option<&Rejection> {
        match self {
            Self::Rejected(rejection) | Self::BatchRejected { rejection, .. } => Some(rejection),
            _ => None,
        }
    }
}

impl From<Rejection> for Error {
    fn from(rejection: Rejection) -> Self {
        Self::Rejected(rejection)
    }
}

impl From<ValueError> for Error {
    fn from(error: ValueError) -> Self {
        Self::Value(error)
    }
}

impl From<AmountError> for Error {
    fn from(error: AmountError) -> Self {
        Self::Amount(error)
    }
}

impl From<QueryError> for Error {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}

impl From<transparency::Error> for Error {
    fn from(error: transparency::Error) -> Self {
        Self::Transparency(error)
    }
}

impl From<ProofError> for Error {
    /// A proof that does not verify: [`transparency::Error::Proof`].
    fn from(error: ProofError) -> Self {
        Self::Transparency(error.into())
    }
}

impl From<DecodeError> for Error {
    /// Proven bytes that do not decode: [`transparency::Error::Decode`].
    fn from(error: DecodeError) -> Self {
        Self::Transparency(error.into())
    }
}

impl From<SealOrderError> for Error {
    /// A seal that does not follow its predecessor:
    /// [`transparency::Error::SealOrder`].
    fn from(error: SealOrderError) -> Self {
        Self::Transparency(error.into())
    }
}

impl From<ConfigError> for Error {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<IntegrityError> for Error {
    fn from(error: IntegrityError) -> Self {
        Self::Integrity(error)
    }
}

impl From<BackendError> for Error {
    fn from(error: BackendError) -> Self {
        Self::Backend(error)
    }
}
