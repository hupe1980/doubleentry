//! The stage driver: [`execute`] decides any [`Op`]; entries run
//! structural → idempotency → context → references → limits.

use time::Date;

use super::context::{self, Leg};
use super::master::AccountSpec;
use super::state::StateView;
use super::writeset::{Done, IndexRow, MemoPlain, Outcome, Recorded, WriteSet};
use super::{
    batch, calendar, clearing, closing, holds, limits, master, references, seal, structural,
};
use crate::error::Rejection;
use crate::model::{
    AccountRef, Amount, BalanceLimit, Currency, Entry, EntryParts, EntryRecord, Fingerprint,
    Genesis, LedgerPolicy, MemoCommitment, PostingRecord, PostingRef, Record, ValueError,
    record_fingerprint,
};

/// One requested write, as raw caller input: the core validates every field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op<const P: u8> {
    /// Create the ledger: the genesis record at position 0. On a ledger that
    /// exists it changes nothing.
    Genesis,
    /// Record an entry draft. A memo must carry its salt: the façades draw
    /// one when the caller did not.
    Record(Box<Entry<P>>),
    /// Record entry drafts atomically, in order, each against the state the
    /// ones before it leave. The outcome is `Batch` (or `Unchanged`
    /// when every draft replays), or `Unchanged(Done::BatchRejected)` for a
    /// refused draft — then nothing is written.
    Batch(Vec<Entry<P>>),
    /// Register an account.
    Register {
        /// The account.
        spec: AccountSpec,
        /// Who registers it.
        actor: String,
    },
    /// Change one field of an account.
    Change {
        /// The account.
        account: AccountRef,
        /// The field and its new value.
        change: ChangeInput,
        /// From when it applies.
        effective: Date,
        /// Who changes it.
        actor: String,
    },
    /// Declare a currency, or raise its exponent.
    Currency {
        /// The currency.
        code: Currency,
        /// Its number of decimal places (at most `P`).
        exponent: u8,
        /// From when it applies.
        effective: Date,
        /// Who declares it.
        actor: String,
    },
    /// Set the ledger policy.
    Policy {
        /// The policy.
        policy: LedgerPolicy,
        /// From when it applies.
        effective: Date,
        /// Who sets it.
        actor: String,
    },
    /// Define the next period of the calendar.
    Period {
        /// Its id.
        id: String,
        /// Its first day.
        start: Date,
        /// Its last day.
        end: Date,
    },
    /// Clear an amount of a debit posting against a credit posting.
    Clear {
        /// The clearing's idempotency key.
        key: String,
        /// The debit posting.
        debit: PostingRef,
        /// The credit posting.
        credit: PostingRef,
        /// The amount.
        amount: Amount<P>,
        /// The clearing date.
        date: Date,
    },
    /// Reset (release) a clearing.
    ResetClearing {
        /// The reset's idempotency key.
        key: String,
        /// The position of the clearing's apply record.
        clearing: u64,
        /// The reset date.
        date: Date,
    },
    /// Seal a period.
    Seal {
        /// The period's id.
        period: String,
    },
    /// Build the year-end closing drafts through `on` into `equity`. Writes
    /// nothing: the outcome is `Unchanged(Done::Closing(drafts))`, to be
    /// recorded in order.
    CloseYear {
        /// The drafts' key prefix (`{key}/1`, `{key}/2`, …).
        key: String,
        /// The closing date.
        on: Date,
        /// The equity account the results close into.
        equity: AccountRef,
    },
}

impl<const P: u8> Op<P> {
    /// How many rounds of [`plan`](super::plan) a store may run before it
    /// gives up. Each round discovers every independent read; the deepest
    /// chain (a hold Post) needs four, and a batch may need the bound once
    /// per draft.
    #[must_use]
    pub fn max_rounds(&self) -> usize {
        const ROUNDS: usize = 8;
        match self {
            Self::Batch(drafts) => ROUNDS.saturating_mul(drafts.len().saturating_add(1)),
            _ => ROUNDS,
        }
    }
}

/// The field an account change sets, as raw caller input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChangeInput {
    /// The last open day (`None`: open-ended again).
    OpenTo(Option<Date>),
    /// The balance limit.
    Limit(BalanceLimit),
    /// The class (`None`: no class).
    Class(Option<String>),
}

/// Decides one operation against the ledger `view`: `Append` with the one
/// record to write, `Batch` for [`Op::Batch`], `Unchanged` when it is already
/// in effect, or the [`Rejection`] with the highest precedence. Pure: equal
/// views and operations give equal outcomes.
///
/// # Errors
///
/// The operation's first violated rule, in stage order and, within a stage,
/// in a fixed order.
pub fn execute<const P: u8>(view: &impl StateView<P>, op: &Op<P>) -> Result<Outcome<P>, Rejection> {
    match op {
        Op::Genesis => Ok(genesis(view)),
        Op::Record(draft) => check(view, draft),
        Op::Batch(drafts) => Ok(batch::batch(view, drafts)),
        Op::Register { spec, actor } => master::register(view, spec, actor),
        Op::Change {
            account,
            change,
            effective,
            actor,
        } => master::change(view, account, change, *effective, actor),
        Op::Currency {
            code,
            exponent,
            effective,
            actor,
        } => master::currency(view, *code, *exponent, *effective, actor),
        Op::Policy {
            policy,
            effective,
            actor,
        } => master::policy(view, policy, *effective, actor),
        Op::Period { id, start, end } => calendar::define(view, id, *start, *end),
        Op::Clear {
            key,
            debit,
            credit,
            amount,
            date,
        } => clearing::apply(view, key, *debit, *credit, *amount, *date),
        Op::ResetClearing {
            key,
            clearing,
            date,
        } => clearing::reset(view, key, *clearing, *date),
        Op::Seal { period } => seal::seal(view, period),
        Op::CloseYear { key, on, equity } => closing::close_year(view, key, *on, equity)
            .map(|drafts| Outcome::Unchanged(Done::Closing(drafts))),
    }
}

/// Every structural violation of a draft, in precedence order — what a form
/// would list before submitting. Reads no state; an empty list does not mean
/// the draft will be recorded. A memo without a salt is not listed: the
/// façade that records the draft supplies the salt.
///
/// ```
/// use doubleentry::{Entry, amount, validate, currency::EUR};
/// use time::macros::date;
///
/// let draft: Entry = Entry::new("", date!(2026 - 03 - 15)).debit("Assets:Cash", amount!("1.00"), EUR);
/// let codes: Vec<u16> = validate(&draft).iter().map(|r| r.code()).collect();
/// assert_eq!(codes, [100, 101, 106]);
/// ```
#[must_use]
pub fn validate<const P: u8>(draft: &Entry<P>) -> Vec<Rejection> {
    let mut found = structural::entry(draft);
    found.retain(|rejection| !matches!(rejection, Rejection::MemoSaltMissing));
    found
}

/// A façade's account argument (a path, a handle, …) as an [`AccountRef`];
/// a path that does not parse is the structural rejection 103.
pub(crate) fn account_ref<A>(account: A) -> Result<AccountRef, Rejection>
where
    A: TryInto<AccountRef>,
    A::Error: Into<ValueError>,
{
    account.try_into().map_err(|reason| Rejection::InvalidPath {
        posting: None,
        reason: reason.into(),
    })
}

/// The genesis record on an empty view; nothing on a ledger that exists.
fn genesis<const P: u8>(view: &impl StateView<P>) -> Outcome<P> {
    let header = view.header();
    if header.size > 0 {
        return Outcome::Unchanged(Done::Genesis);
    }
    Outcome::from(WriteSet::of(
        0,
        Record::Genesis(Genesis {
            ledger: header.ledger,
        }),
        Done::Genesis,
    ))
}

/// Decides an entry draft.
pub(crate) fn check<const P: u8>(
    view: &impl StateView<P>,
    draft: &Entry<P>,
) -> Result<Outcome<P>, Rejection> {
    if let Some(first) = structural::entry(draft).into_iter().next() {
        return Err(first);
    }
    // After stage 1 the draft has a key and every account.
    let key = draft.key().ok_or(Rejection::InvalidKey {
        reason: ValueError::Empty,
    })?;
    // Stage 2: idempotency, before any state-dependent rule. The fingerprint
    // a ledger keeps names every account by handle (an audit recomputes it
    // from the leaf), so the draft's accounts are resolved first; a path no
    // account has cannot be the recorded draft's.
    if let Some(meta) = view.entry_by_key(key) {
        let handles: Option<Vec<_>> = draft
            .postings()
            .iter()
            .map(|posting| match posting.account()? {
                AccountRef::Path(path) => view.account_by_path(path).map(|a| a.handle()),
                AccountRef::Handle(handle) => Some(*handle),
            })
            .collect();
        let fingerprint = handles.and_then(|handles| draft.resolved_fingerprint(&handles).ok());
        return if fingerprint.is_some() && meta.fingerprint == fingerprint {
            Ok(Outcome::Unchanged(Done::Entry(Recorded::new(
                meta.id,
                meta.position,
                true,
            ))))
        } else {
            Err(Rejection::KeyConflict {
                position: meta.position,
            })
        };
    }
    let legs = draft
        .postings()
        .iter()
        .enumerate()
        .map(|(posting, p)| {
            Ok(Leg {
                account: p.account().cloned().ok_or(Rejection::InvalidPath {
                    posting: Some(structural::index(posting)),
                    reason: ValueError::Empty,
                })?,
                amount: p.amount(),
                currency: p.currency(),
                layer: p.get_layer(),
                dimensions: p.get_dimensions(),
            })
        })
        .collect::<Result<Vec<_>, Rejection>>()?;
    let booking = draft.booking_date();
    let facts = context::gather(view, booking, &legs, draft.get_reverses());
    context::check(&facts, booking, draft.get_value_date(), &legs)?;
    // The record the draft resolves to; every later stage checks the record.
    let memo = match (draft.memo_text(), draft.memo_salt()) {
        (Some(text), Some(salt)) => Some(MemoPlain {
            salt: *salt,
            text: text.to_owned(),
        }),
        (Some(_), None) => return Err(Rejection::MemoSaltMissing),
        (None, _) => None,
    };
    let postings = legs
        .iter()
        .zip(&facts.accounts)
        .zip(draft.postings())
        .map(|((leg, account), posting)| {
            let account = account.as_ref().ok_or_else(|| Rejection::UnknownAccount {
                account: leg.account.clone(),
            })?;
            Ok(PostingRecord {
                handle: account.handle(),
                side: posting.side(),
                amount: leg.amount,
                currency: leg.currency,
                layer: leg.layer,
                dimensions: leg.dimensions.clone(),
            })
        })
        .collect::<Result<Vec<_>, Rejection>>()?;
    let record = EntryRecord::new(EntryParts {
        key: key.clone(),
        booking_date: booking,
        value_date: draft.get_value_date(),
        captured_at: draft.get_captured_at(),
        kind: draft.get_kind().cloned(),
        memo: memo
            .as_ref()
            .map(|memo| MemoCommitment::compute(&memo.salt, &memo.text)),
        documents: draft.documents().to_vec(),
        postings,
        reverses: draft.get_reverses(),
        hold: draft.get_hold().copied(),
        rates: draft.rates().to_vec(),
    })
    .map_err(|error| structural::record_error(error, draft.encoded_len()))?;
    let parts = record.parts();
    let fingerprint = record_fingerprint(
        parts,
        &parts.postings,
        memo.as_ref().map(|memo| memo.text.as_str()),
    );
    finish(view, &facts, record, memo, Some(fingerprint))
}

/// The fingerprint of the draft a stored entry record was recorded from, as
/// a ledger keeps it: recomputed from the record (a hold resolution's draft
/// carried no postings) and its memo text; `None` when the record commits to
/// a memo whose text is gone (erased).
pub(crate) fn stored_fingerprint<const P: u8>(
    record: &EntryRecord<P>,
    memo: Option<&MemoPlain>,
) -> Option<Fingerprint> {
    let parts = record.parts();
    if parts.memo.is_some() && memo.is_none() {
        return None;
    }
    let postings: &[PostingRecord<P>] = if holds::is_resolution(parts.hold.as_ref()) {
        &[]
    } else {
        &parts.postings
    };
    Some(record_fingerprint(
        parts,
        postings,
        memo.map(|memo| memo.text.as_str()),
    ))
}

/// Decides a stored entry record during replay: the same stages, run on the
/// record, with the draft's fingerprint recomputed ([`stored_fingerprint`]).
/// A key already in use answers `Unchanged`.
pub(crate) fn replay<const P: u8>(
    view: &impl StateView<P>,
    record: EntryRecord<P>,
    memo: Option<MemoPlain>,
) -> Result<Outcome<P>, Rejection> {
    let fingerprint = stored_fingerprint(&record, memo.as_ref());
    if let Some(first) = structural::record(&record).into_iter().next() {
        return Err(first);
    }
    let parts = record.parts();
    if let Some(meta) = view.entry_by_key(&parts.key) {
        return Ok(Outcome::Unchanged(Done::Entry(Recorded::new(
            meta.id,
            meta.position,
            true,
        ))));
    }
    // A resolution's postings are derived from its hold in stage 4, and
    // checked there: stage 3 sees what its draft carried, none.
    let legs: Vec<Leg<'_, P>> = if holds::is_resolution(parts.hold.as_ref()) {
        Vec::new()
    } else {
        context::legs(&parts.postings)
    };
    let facts = context::gather(view, parts.booking_date, &legs, parts.reverses);
    context::check(&facts, parts.booking_date, parts.value_date, &legs)?;
    finish(view, &facts, record, memo, fingerprint)
}

/// Stages 4 and 5 on the resolved record, then its write set.
fn finish<const P: u8>(
    view: &impl StateView<P>,
    facts: &context::EntryFacts<P>,
    record: EntryRecord<P>,
    memo: Option<MemoPlain>,
    fingerprint: Option<Fingerprint>,
) -> Result<Outcome<P>, Rejection> {
    references::check(facts, &record)?;
    let holds::Checked {
        record,
        effect,
        derived,
        outstanding,
    } = holds::check(view, record)?;
    // A resolution's postings were derived by the hold rules, which read
    // what stage 5 needs of them.
    let facts = derived.as_ref().unwrap_or(facts);
    let booked = limits::check(view, facts, &record, &outstanding)?;
    let header = view.header();
    let position = header.size;
    let id = record.id(&header.ledger);
    let parts = record.parts();
    let mut index = vec![IndexRow::EntryKey {
        key: parts.key.clone(),
        id,
        position,
        fingerprint,
    }];
    if let Some(target) = parts.reverses {
        index.push(IndexRow::Reversal { target, by: id });
    }
    let record = Record::Entry(record);
    Ok(Outcome::from(WriteSet {
        position,
        leaf: record.to_leaf_bytes(),
        record,
        memo,
        done: Done::Entry(Recorded::new(id, position, false)),
        postings: booked.rows,
        balances: booked.deltas,
        index,
        hold: effect,
        outstanding,
    }))
}
