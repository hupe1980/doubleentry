//! `audit()`: a replay of the log through the rule core.
//!
//! Every leaf, from position 0, is decoded, turned back into the operation
//! that would have written it, decided by [`execute`] against the replayed
//! state, and must come out byte for byte as the leaf; an entry's draft
//! fingerprint is recomputed from its record and memo text. A record the
//! core would refuse, or would not have written, fails the audit at its
//! position. The replayed state then projects every table a store keeps
//! ([`ProjectionPart`]), each compared byte for byte with the store's.

use core::fmt;
use core::iter::Peekable;

use super::check::{self, ChangeInput, Op, execute};
use super::master::AccountSpec;
use super::state::{MemState, StateView};
use super::writeset::{IndexRow, MemoPlain, Outcome};
use crate::error::Rejection;
use crate::log::TreeHead;
use crate::model::{
    AccountAction, AccountChange, AccountHandle, AccountRef, ClearingAction, Currency, DecodeError,
    Fingerprint, Layer, LedgerId, MemoCommitment, PeriodId, Record, Seal, SealOrderError,
};
use crate::store::{ProjectionPart, ProjectionRow, StoreProjection};

/// One leaf of the log with the side data stored beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayLeaf<'a> {
    /// The leaf's position.
    pub position: u64,
    /// The leaf bytes.
    pub bytes: &'a [u8],
    /// An entry's memo text and salt, unless erased.
    pub memo: Option<&'a MemoPlain>,
    /// The fingerprint stored beside an entry, when known. Not committed:
    /// the replay recomputes it from the record and the memo text, and a
    /// fingerprint given here must equal it.
    pub fingerprint: Option<Fingerprint>,
}

/// What a passing audit covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditReport {
    /// The number of records.
    pub size: u64,
    /// The number of entries.
    pub entries: u64,
    /// The number of seals.
    pub seals: u64,
    /// The number of accounts.
    pub accounts: u64,
    /// The tree head the records produce.
    pub head: TreeHead,
}

/// The first point where a book differs from what the rule core produces.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("audit failed{}: {kind}", position_suffix(.position))]
#[non_exhaustive]
pub struct AuditFailure {
    /// The position of the first failing record; `None` for a difference
    /// in the store's projection.
    pub position: Option<u64>,
    /// What failed.
    pub kind: AuditFailureKind,
}

fn position_suffix(position: &Option<u64>) -> String {
    position.map_or_else(String::new, |position| format!(" at position {position}"))
}

/// What an audit found.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuditFailureKind {
    /// A leaf does not decode.
    #[error("the leaf does not decode: {0}")]
    Undecodable(DecodeError),
    /// A leaf decodes but is not its record's one encoding.
    #[error("the leaf is not in canonical form")]
    NonCanonical,
    /// Position 0 is not a genesis record.
    #[error("the log does not start with a genesis record")]
    GenesisMissing,
    /// The genesis names another ledger or scale.
    #[error("the genesis names another ledger or scale")]
    GenesisMismatch,
    /// A leaf was fed out of order.
    #[error("expected the leaf at position {expected}")]
    PositionMismatch {
        /// The position expected next.
        expected: u64,
    },
    /// The core refuses the record.
    #[error("the rule core refuses the record: {0}")]
    WouldReject(Rejection),
    /// The core would not have written the record (a repeat, or another
    /// encoding of the operation).
    #[error("the rule core would not have written the record")]
    NotProduced,
    /// A seal differs from the one the core computes.
    #[error("the seal of {period} does not match the replay")]
    SealMismatch {
        /// The period.
        period: PeriodId,
    },
    /// A registration carries another handle than the next one.
    #[error("the registration has handle {found}, expected {expected}")]
    HandleMismatch {
        /// The number of earlier registrations.
        expected: u32,
        /// The handle in the record.
        found: AccountHandle,
    },
    /// A memo's text and salt do not match its commitment.
    #[error("the memo does not match its commitment")]
    MemoMismatch,
    /// A seal's totals are below the previous seal's.
    #[error("the {currency} {layer:?} totals decrease from one seal to the next")]
    SealTotalsDecrease {
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
    },
    /// A stored draft fingerprint differs from the one the replay
    /// recomputes from the record and its memo text.
    #[error("the stored draft fingerprint does not match the replay")]
    FingerprintMismatch,
    /// The claimed tree head differs.
    #[error("the claimed tree head does not match the replay")]
    HeadMismatch,
    /// A table of the store differs from the replay's projection of it: the
    /// first row (in key order) where they part.
    #[error(
        "the {part} table does not match the replay: expected {}, found {}",
        show(.expected),
        show(.found)
    )]
    ProjectionMismatch {
        /// The table.
        part: ProjectionPart,
        /// The row the replay produces there; `None` when the store holds
        /// a row too many.
        expected: Option<Box<ProjectionRow>>,
        /// The row the store holds there; `None` when it lacks a row.
        found: Option<Box<ProjectionRow>>,
    },
}

fn show(row: &Option<Box<ProjectionRow>>) -> String {
    row.as_ref()
        .map_or_else(|| String::from("no row"), ToString::to_string)
}

/// Replays a log leaf by leaf into a fresh state.
///
/// ```
/// use doubleentry::{AccountKind, Journal, LedgerId};
/// use doubleentry::store::Replayer;
/// use time::macros::date;
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.open("Assets:Cash", AccountKind::Asset, date!(2026 - 01 - 01))?;
///
/// let mut replay = Replayer::<2>::new(books.id());
/// for leaf in books.leaves() {
///     replay.feed(leaf)?;
/// }
/// assert_eq!(replay.head(), books.head());
/// # Ok::<(), doubleentry::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Replayer<const P: u8> {
    state: MemState<P>,
    seals: u64,
    last_seal: Option<Seal<P>>,
}

impl<const P: u8> Replayer<P> {
    /// A replay of the ledger `ledger`, before its first leaf.
    #[must_use]
    pub fn new(ledger: &LedgerId) -> Self {
        Self {
            state: MemState::new(ledger.clone()),
            seals: 0,
            last_seal: None,
        }
    }

    /// The head of the leaves fed so far.
    #[must_use]
    pub fn head(&self) -> TreeHead {
        self.state.head()
    }

    /// Replays the next leaf.
    ///
    /// # Errors
    ///
    /// The first way the leaf differs from what the rule core writes at its
    /// position.
    pub fn feed(&mut self, leaf: ReplayLeaf<'_>) -> Result<(), AuditFailure> {
        let position = self.state.header().size;
        let fail = |kind| AuditFailure {
            position: Some(position),
            kind,
        };
        if leaf.position != position {
            return Err(fail(AuditFailureKind::PositionMismatch {
                expected: position,
            }));
        }
        let record = match Record::<P>::decode(leaf.bytes) {
            Ok(record) => record,
            Err(DecodeError::PrecisionMismatch { .. }) if position == 0 => {
                return Err(fail(AuditFailureKind::GenesisMismatch));
            }
            Err(error) => return Err(fail(AuditFailureKind::Undecodable(error))),
        };
        if record.to_leaf_bytes() != leaf.bytes {
            return Err(fail(AuditFailureKind::NonCanonical));
        }
        let outcome = match &record {
            Record::Genesis(genesis) if position == 0 => {
                if genesis.ledger != *self.state.ledger() {
                    return Err(fail(AuditFailureKind::GenesisMismatch));
                }
                execute(&self.state, &Op::Genesis)
            }
            _ if position == 0 => return Err(fail(AuditFailureKind::GenesisMissing)),
            Record::Entry(entry) => {
                let memo = leaf.memo.cloned();
                if let Some(memo) = &memo
                    && entry.parts().memo != Some(MemoCommitment::compute(&memo.salt, &memo.text))
                {
                    return Err(fail(AuditFailureKind::MemoMismatch));
                }
                check::replay(&self.state, entry.clone(), memo)
            }
            Record::Account(event) => {
                if let AccountAction::Register(registration) = &event.action {
                    let expected = self.state.header().accounts;
                    if registration.handle.get() != expected {
                        return Err(fail(AuditFailureKind::HandleMismatch {
                            expected,
                            found: registration.handle,
                        }));
                    }
                }
                execute(&self.state, &operation(&record))
            }
            _ => execute(&self.state, &operation(&record)),
        };
        let write = match outcome {
            Ok(Outcome::Append(write)) => *write,
            // A replay decides one record at a time: never a batch.
            Ok(Outcome::Unchanged(_) | Outcome::Batch { .. }) => {
                return Err(fail(AuditFailureKind::NotProduced));
            }
            Err(rejection) => return Err(fail(AuditFailureKind::WouldReject(rejection))),
        };
        if let Some(claimed) = leaf.fingerprint {
            let replayed = write.index.iter().find_map(|row| match row {
                IndexRow::EntryKey { fingerprint, .. } => Some(*fingerprint),
                _ => None,
            });
            if replayed != Some(Some(claimed)) {
                return Err(fail(AuditFailureKind::FingerprintMismatch));
            }
        }
        if write.leaf != leaf.bytes {
            return Err(fail(match &record {
                Record::Seal(seal) => AuditFailureKind::SealMismatch {
                    period: seal.parts().period.clone(),
                },
                _ => AuditFailureKind::NotProduced,
            }));
        }
        if let Record::Seal(seal) = &record {
            if let Some(earlier) = &self.last_seal {
                seal.check_follows(earlier).map_err(|error| {
                    fail(match error {
                        SealOrderError::TreeMissing { currency, layer }
                        | SealOrderError::TotalDecreased { currency, layer } => {
                            AuditFailureKind::SealTotalsDecrease { currency, layer }
                        }
                        _ => AuditFailureKind::SealMismatch {
                            period: seal.parts().period.clone(),
                        },
                    })
                })?;
            }
            self.seals = self.seals.saturating_add(1);
            self.last_seal = Some(seal.clone());
        }
        self.state
            .apply(write)
            .map_err(|_| fail(AuditFailureKind::NotProduced))
    }

    /// What a passing audit of the leaves fed so far reports.
    #[must_use]
    pub fn report(&self) -> AuditReport {
        let header = self.state.header();
        AuditReport {
            size: header.size,
            entries: header.entries,
            seals: self.seals,
            accounts: u64::from(header.accounts),
            head: self.state.head(),
        }
    }

    /// The projection a store holding exactly these leaves has: every part,
    /// in key order.
    #[must_use]
    pub fn projection(&self) -> StoreProjection {
        let mut projection = StoreProjection::new(self.state.head());
        for part in ProjectionPart::ALL {
            *projection.rows_mut(part) = self.state.rows(part).collect();
        }
        projection
    }

    /// Compares the replay with a store's projection: the tree head, then
    /// every part in [`ProjectionPart::ALL`] order, row by row.
    ///
    /// # Errors
    ///
    /// The first difference.
    pub fn finish(&self, claimed: &StoreProjection) -> Result<AuditReport, AuditFailure> {
        if claimed.head() != self.state.head() {
            return Err(AuditFailure {
                position: None,
                kind: AuditFailureKind::HeadMismatch,
            });
        }
        for part in ProjectionPart::ALL {
            let mut check = self.check_part(part);
            for row in claimed.rows(part) {
                check.feed(row.clone())?;
            }
            check.finish()?;
        }
        Ok(self.report())
    }

    /// Compares the replay with an in-memory state (a journal's) as
    /// [`finish`](Self::finish) compares it with a store's projection.
    pub(crate) fn finish_state(&self, state: &MemState<P>) -> Result<AuditReport, AuditFailure> {
        if state.head() != self.state.head() {
            return Err(AuditFailure {
                position: None,
                kind: AuditFailureKind::HeadMismatch,
            });
        }
        for part in ProjectionPart::ALL {
            let mut check = self.check_part(part);
            for row in state.rows(part) {
                check.feed(row)?;
            }
            check.finish()?;
        }
        Ok(self.report())
    }

    /// A comparison of one part, fed the store's rows in key order.
    pub(crate) fn check_part(&self, part: ProjectionPart) -> PartCheck<'_> {
        PartCheck {
            part,
            expected: self.state.rows(part).peekable(),
        }
    }

    /// The replayed state.
    pub(crate) fn into_state(self) -> MemState<P> {
        self.state
    }
}

/// One part's comparison: the replay's rows against a store's, fed in key
/// order (a page at a time), byte for byte.
pub(crate) struct PartCheck<'a> {
    part: ProjectionPart,
    expected: Peekable<Box<dyn Iterator<Item = ProjectionRow> + Send + 'a>>,
}

impl PartCheck<'_> {
    fn mismatch(
        &self,
        expected: Option<ProjectionRow>,
        found: Option<ProjectionRow>,
    ) -> AuditFailure {
        AuditFailure {
            position: None,
            kind: AuditFailureKind::ProjectionMismatch {
                part: self.part,
                expected: expected.map(Box::new),
                found: found.map(Box::new),
            },
        }
    }

    /// The store's next row.
    pub(crate) fn feed(&mut self, found: ProjectionRow) -> Result<(), AuditFailure> {
        match self.expected.next() {
            Some(expected) if expected.to_bytes() == found.to_bytes() => Ok(()),
            expected => Err(self.mismatch(expected, Some(found))),
        }
    }

    /// The store has no more rows.
    pub(crate) fn finish(mut self) -> Result<(), AuditFailure> {
        match self.expected.next() {
            None => Ok(()),
            Some(expected) => Err(self.mismatch(Some(expected), None)),
        }
    }
}

/// The operation that writes a master-data, calendar, clearing or seal
/// record; for genesis and entries (replayed through their own paths) the
/// genesis operation, which can never be appended there.
fn operation<const P: u8>(record: &Record<P>) -> Op<P> {
    match record {
        Record::Account(event) => match &event.action {
            AccountAction::Register(registration) => {
                let mut spec = AccountSpec::new(
                    registration.path.to_string(),
                    registration.kind,
                    registration.open_from,
                )
                .limit(registration.limit);
                if let Some(class) = &registration.class {
                    spec = spec.class(class);
                }
                if let Some(open_to) = registration.open_to {
                    spec = spec.open_to(open_to);
                }
                Op::Register {
                    spec,
                    actor: event.actor.to_string(),
                }
            }
            AccountAction::Change { handle, change } => Op::Change {
                account: AccountRef::Handle(*handle),
                change: match change {
                    AccountChange::OpenTo(open_to) => ChangeInput::OpenTo(*open_to),
                    AccountChange::Limit(limit) => ChangeInput::Limit(*limit),
                    AccountChange::Class(class) => {
                        ChangeInput::Class(class.as_ref().map(ToString::to_string))
                    }
                },
                effective: event.effective,
                actor: event.actor.to_string(),
            },
        },
        Record::Currency(declaration) => Op::Currency {
            code: declaration.code,
            exponent: declaration.exponent,
            effective: declaration.effective,
            actor: declaration.actor.to_string(),
        },
        Record::Policy(event) => Op::Policy {
            policy: event.policy.clone(),
            effective: event.effective,
            actor: event.actor.to_string(),
        },
        Record::Period(period) => Op::Period {
            id: period.id().to_string(),
            start: period.start(),
            end: period.end(),
        },
        Record::Clearing(event) => match &event.action {
            ClearingAction::Apply {
                key,
                debit,
                credit,
                amount,
            } => Op::Clear {
                key: key.to_string(),
                debit: *debit,
                credit: *credit,
                amount: *amount,
                date: event.date,
            },
            ClearingAction::Reset { key, clearing } => Op::ResetClearing {
                key: key.to_string(),
                clearing: *clearing,
                date: event.date,
            },
        },
        Record::Seal(seal) => Op::Seal {
            period: seal.parts().period.to_string(),
        },
        Record::Genesis(_) | Record::Entry(_) => Op::Genesis,
    }
}

/// A multi-line report in its own columns (width is ignored):
///
/// ```text
/// audit passed
///   records    41
///   entries    10
///   seals       1
///   accounts    2
///   root      <64 hex digits>
/// ```
impl fmt::Display for AuditReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let counts = [
            ("records", self.size),
            ("entries", self.entries),
            ("seals", self.seals),
            ("accounts", self.accounts),
        ];
        let width = counts
            .iter()
            .map(|(_, count)| count.to_string().len())
            .max()
            .unwrap_or(1);
        writeln!(f, "audit passed")?;
        for (label, count) in counts {
            writeln!(f, "  {label:<9}{count:>width$}")?;
        }
        write!(f, "  {:<9}{}", "root", self.head.root)
    }
}
