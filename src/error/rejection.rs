//! [`Rejection`]: why the rule core refused a write, with a stable code.

use core::fmt;

use time::Date;

use crate::core::{BalanceKey, ResolutionKind};
use crate::model::{
    AccountHandle, AccountPath, AccountRef, Amount, BalanceLimit, Currency, EntryId, Label, Layer,
    PeriodId, PostingRef, Side, ValueError, pad,
};

/// Why the rule core refused a write.
///
/// One enum for every operation and every façade: `Journal::record`,
/// `Ledger::record` and every store return the same `Rejection` for the same
/// input. Each variant has a stable numeric [`code`](Self::code) — codes are
/// append-only and a retired code is never reused — and belongs to one
/// [`Stage`]. The checks run stage by stage (structural → idempotency →
/// context → references → limits), and within a stage in a fixed order, so
/// one input always gets the same refusal when it breaks several rules.
///
/// | Codes | Stage | Checks |
/// |---|---|---|
/// | 100–123 | [`Stage::Structural`] | the input alone: key, postings, balance, labels, sizes |
/// | 140–145 | [`Stage::Structural`] | the shape of a hold or a hold resolution |
/// | 200 | [`Stage::Idempotency`] | a key already used for other content |
/// | 300–311 | [`Stage::Context`] | the ledger's state on the booking date: periods, accounts, currencies, policy, watermark |
/// | 400–431 | [`Stage::References`] | the records an input names: reversal targets, cleared postings, master data |
/// | 440–450 | [`Stage::References`] | the hold a resolution names, and reversals of holds |
/// | 500–501 | [`Stage::Limits`] | overflow and balance limits |
///
/// Codes 146–149 and 451–459 are reserved for holds; 112 is unassigned.
///
/// ```
/// use doubleentry::{Rejection, error::Stage};
///
/// let refusal = Rejection::TooFewPostings { count: 1 };
/// assert_eq!(refusal.code(), 101);
/// assert_eq!(refusal.stage(), Stage::Structural);
/// assert_eq!(refusal.to_string(), "[101] an entry needs at least two postings, found 1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Rejection {
    // ── Structural (100–199) ────────────────────────────────────────────────
    /// 100: the idempotency key is not 1–128 bytes of text without control
    /// characters.
    InvalidKey {
        /// Why the key was refused.
        reason: ValueError,
    },
    /// 101: an entry has fewer than two postings.
    TooFewPostings {
        /// The number of postings.
        count: usize,
    },
    /// 102: an entry has more postings than allowed.
    TooManyPostings {
        /// The number of postings.
        count: usize,
        /// The maximum.
        max: usize,
    },
    /// 103: an account path does not parse.
    InvalidPath {
        /// The posting whose account is invalid; `None` outside an entry.
        posting: Option<u16>,
        /// Why the path was refused.
        reason: ValueError,
    },
    /// 104: a posting's amount is zero or negative.
    NonPositiveAmount {
        /// The posting.
        posting: u16,
    },
    /// 105: a posting's dimensions are invalid.
    InvalidDimensions {
        /// The posting.
        posting: u16,
        /// Why the dimensions were refused.
        reason: ValueError,
    },
    /// 106: debits and credits differ for one (currency, layer).
    Unbalanced {
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
        /// The debit total.
        debit: ScaledAmount,
        /// The credit total.
        credit: ScaledAmount,
    },
    /// 107: a label is invalid.
    InvalidLabel {
        /// Which label.
        field: LabelField,
        /// Why the label was refused.
        reason: ValueError,
    },
    /// 108: the memo is empty or too long.
    InvalidMemo {
        /// Why the memo was refused.
        reason: ValueError,
    },
    /// 109: an entry has more documents than allowed.
    TooManyDocuments {
        /// The number of documents.
        count: usize,
        /// The maximum.
        max: usize,
    },
    /// 110: an entry has more exchange rates than allowed.
    TooManyRates {
        /// The number of rates.
        count: usize,
        /// The maximum.
        max: usize,
    },
    /// 111: the record would encode to more than `MAX_LEAF_BYTES`.
    RecordTooLarge {
        /// The encoded size.
        bytes: usize,
        /// The maximum.
        max: usize,
    },
    /// 113: an entry's debit or credit total for one (currency, layer) is
    /// beyond the amount range.
    TotalOutOfRange {
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
    },
    /// 114: a memo reached the core without its salt. The façades supply one;
    /// a store or caller driving the core directly must too.
    MemoSaltMissing,
    /// 120: a clearing amount is zero or negative.
    NonPositiveClearingAmount,
    /// 121: a period ends before it starts.
    InvalidPeriodRange {
        /// The first day.
        start: Date,
        /// The last day.
        end: Date,
    },
    /// 122: an account's open window ends before it starts.
    InvalidOpenWindow {
        /// The first open day.
        open_from: Date,
        /// The last open day.
        open_to: Date,
    },
    /// 123: a currency exponent above the ledger's scale `P`.
    ExponentTooLarge {
        /// The requested exponent.
        exponent: u8,
        /// The ledger's scale `P`.
        max: u8,
    },
    /// 140: a hold does not have exactly two Pending postings, one debit and
    /// one credit.
    HoldLegs,
    /// 141: a hold expires on or before its own booking date.
    HoldExpiry {
        /// The hold's booking date.
        booking: Date,
        /// Its expiry date.
        expires: Date,
    },
    /// 142: a hold resolution carries postings; the core derives them.
    ResolutionPostings {
        /// The number of postings in the draft.
        count: usize,
    },
    /// 143: a Pending posting in an entry that neither opens nor resolves a
    /// hold.
    PendingOutsideHold {
        /// The first Pending posting.
        posting: u16,
    },
    /// 144: an entry both reverses an entry and acts on a hold.
    ReversalWithHoldAction,
    /// 145: a partial post of a hold is zero or negative.
    PostAmountNotPositive,

    // ── Idempotency (200–299) ───────────────────────────────────────────────
    /// 200: the key was used for different content.
    KeyConflict {
        /// The position of the record recorded under the key.
        position: u64,
    },

    // ── Context (300–399) ───────────────────────────────────────────────────
    /// 300: the date lies in no defined period.
    NoPeriod {
        /// The date.
        date: Date,
    },
    /// 301: the date lies in a sealed period.
    PeriodSealed {
        /// The period.
        period: PeriodId,
        /// The date.
        date: Date,
    },
    /// 302: the account is not registered.
    UnknownAccount {
        /// The account as named.
        account: AccountRef,
    },
    /// 303: the account has sub-accounts and cannot be posted to.
    NotALeaf {
        /// The account.
        account: AccountPath,
    },
    /// 304: the account is not open on the date.
    AccountNotOpen {
        /// The account.
        account: AccountPath,
        /// The date.
        date: Date,
    },
    /// 305: the currency is not declared on the date.
    UndeclaredCurrency {
        /// The currency.
        currency: Currency,
        /// The date.
        date: Date,
    },
    /// 306: an amount has more decimal places than its currency allows.
    ExcessPrecision {
        /// The posting.
        posting: u16,
        /// The currency.
        currency: Currency,
        /// The currency's exponent.
        exponent: u8,
    },
    /// 307: a posting lacks a dimension the policy requires.
    MissingDimension {
        /// The posting.
        posting: u16,
        /// The missing axis.
        axis: Label,
    },
    /// 308: the value date is further from the booking date than the policy
    /// allows.
    ValueDateDrift {
        /// The distance in days.
        days: u32,
        /// The policy's bound.
        max: u32,
    },
    /// 309: a master-data change takes effect on or before the end of the
    /// last sealed period.
    AtOrBelowWatermark {
        /// The requested effective date.
        effective: Date,
        /// The end of the last sealed period.
        watermark: Date,
    },
    /// 310: a change takes effect before the latest change of the same field.
    EffectiveDateRegresses {
        /// The requested effective date.
        effective: Date,
        /// The latest effective date of the field.
        latest: Date,
    },
    /// 311: the period is not defined.
    UnknownPeriod {
        /// The period id.
        period: PeriodId,
    },

    // ── References (400–499) ────────────────────────────────────────────────
    /// 400: the entry a reversal names does not exist.
    ReversalTargetUnknown {
        /// The named entry.
        target: EntryId,
    },
    /// 401: the target was already reversed.
    AlreadyReversed {
        /// The target.
        target: EntryId,
        /// The existing reversal.
        by: EntryId,
    },
    /// 402: the reversal is dated before its target.
    ReversalBeforeTarget {
        /// The target.
        target: EntryId,
        /// The target's booking date.
        booked: Date,
    },
    /// 403: the reversal does not mirror its target.
    NotAMirror {
        /// The target.
        target: EntryId,
        /// The first posting that differs; `None` when the counts differ.
        posting: Option<u16>,
    },
    /// 410: a cleared posting does not exist.
    ClearingTargetUnknown {
        /// The posting as named.
        posting: PostingRef,
    },
    /// 411: a cleared posting is on the wrong side.
    ClearingSideMismatch {
        /// The posting.
        posting: PostingRef,
        /// The side it must have.
        expected: Side,
    },
    /// 412: the two cleared postings are on different balances.
    ClearingKeyMismatch {
        /// The debit posting's balance.
        debit: BalanceKey,
        /// The credit posting's balance.
        credit: BalanceKey,
    },
    /// 413: the clearing amount has more decimal places than the currency
    /// allows.
    ClearingPrecision {
        /// The currency.
        currency: Currency,
        /// The currency's exponent.
        exponent: u8,
    },
    /// 414: the clearing amount exceeds what is still open on a posting.
    OverApplied {
        /// The posting.
        posting: PostingRef,
        /// The open amount.
        open: ScaledAmount,
        /// The requested amount.
        requested: ScaledAmount,
    },
    /// 415: the clearing is dated before a cleared posting.
    ClearingBeforePosting {
        /// The posting.
        posting: PostingRef,
        /// Its booking date.
        booked: Date,
    },
    /// 416: no clearing was applied at the position.
    ClearingUnknown {
        /// The position named.
        clearing: u64,
    },
    /// 417: the clearing was already reset.
    ClearingAlreadyReset {
        /// The clearing's position.
        clearing: u64,
        /// The reset's position.
        by: u64,
    },
    /// 418: the reset is dated before the clearing.
    ResetBeforeClearing {
        /// The clearing's date.
        cleared_on: Date,
    },
    /// 419: a cleared posting is Pending — a hold's reservation, not an open
    /// item; only Settled postings are cleared.
    ClearingNotSettled {
        /// The Pending posting.
        posting: PostingRef,
    },
    /// 420: the path is registered with different master data.
    AccountExists {
        /// The path.
        path: AccountPath,
        /// Its handle.
        handle: AccountHandle,
    },
    /// 421: a parent account already has postings.
    ParentHasPostings {
        /// The parent with postings.
        parent: AccountPath,
    },
    /// 422: the class cannot change once the account has postings.
    ClassFrozen {
        /// The account.
        account: AccountPath,
    },
    /// 423: the account has postings after the requested last day.
    PostingsAfterClose {
        /// The account.
        account: AccountPath,
        /// Its latest booking date.
        last_booked: Date,
    },
    /// 424: a currency's exponent cannot be lowered.
    ExponentLowered {
        /// The currency.
        currency: Currency,
        /// Its current exponent.
        current: u8,
        /// The requested exponent.
        requested: u8,
    },
    /// 425: the period id is defined with other dates.
    PeriodExists {
        /// The period id.
        period: PeriodId,
        /// Its first day.
        start: Date,
        /// Its last day.
        end: Date,
    },
    /// 426: the period does not start the day after the last period ends.
    PeriodGap {
        /// The day after the last period.
        expected_start: Date,
        /// The requested first day.
        start: Date,
    },
    /// 427: the period before it is not sealed. Seal from `earliest` on —
    /// or let [`seal_through`](crate::Journal::seal_through) seal every open
    /// period up to it in order.
    PredecessorNotSealed {
        /// The open predecessor.
        predecessor: PeriodId,
        /// The earliest period without a seal: the one to seal first.
        earliest: PeriodId,
    },
    /// 428: every account handle is in use.
    TooManyAccounts,
    /// 429: the equity account of a year-end close is itself closed by it.
    ClosingEquityInScope {
        /// The equity account named.
        account: AccountPath,
    },
    /// 430: an account with a balance to close is not open on the closing date.
    ClosingAccountNotOpen {
        /// The account.
        account: AccountPath,
        /// The closing date.
        date: Date,
    },
    /// 431: a policy would take effect on or before the latest booking date
    /// already recorded, so it would govern entries checked under another
    /// policy.
    PolicyBeforeBookings {
        /// The requested effective date.
        effective: Date,
        /// The latest booking date of any posting.
        last_booking: Date,
    },
    /// 440: the entry a hold resolution names does not exist.
    HoldNotFound {
        /// The named entry.
        hold: EntryId,
    },
    /// 441: the entry a hold resolution names is not a hold.
    NotAHold {
        /// The named entry.
        entry: EntryId,
    },
    /// 442: the hold was already resolved — every hold resolves once.
    HoldResolved {
        /// The hold.
        hold: EntryId,
        /// The entry that resolved it.
        by: EntryId,
        /// How it was resolved.
        how: ResolutionKind,
    },
    /// 443: the resolution is dated before its hold.
    ResolutionBeforeHold {
        /// The hold.
        hold: EntryId,
        /// The hold's booking date.
        held_on: Date,
    },
    /// 444: a post on or after the hold's expiry date.
    HoldExpired {
        /// The hold.
        hold: EntryId,
        /// Its expiry date.
        expires: Date,
    },
    /// 445: an expiry of a hold without an expiry date.
    HoldNeverExpires {
        /// The hold.
        hold: EntryId,
    },
    /// 446: an expiry before the hold's expiry date.
    HoldNotExpired {
        /// The hold.
        hold: EntryId,
        /// Its expiry date.
        expires: Date,
    },
    /// 447: a partial post with more decimal places than the hold's currency
    /// allows; amounts are never rounded.
    PostAmountPrecision {
        /// The hold.
        hold: EntryId,
        /// The hold's currency.
        currency: Currency,
        /// The currency's exponent on the resolution's booking date.
        exponent: u8,
    },
    /// 448: a partial post above the held amount.
    PostExceedsHold {
        /// The hold.
        hold: EntryId,
        /// The amount held.
        held: ScaledAmount,
        /// The amount the post asked for.
        requested: ScaledAmount,
    },
    /// 449: a post on a day one of the hold's accounts is not open (a void
    /// or an expiry is still accepted).
    PostToClosedAccount {
        /// The hold.
        hold: EntryId,
        /// The account that is not open.
        account: AccountRef,
        /// The post's booking date.
        date: Date,
    },
    /// 450: a reversal of a hold or of a hold resolution; a reservation ends
    /// with a void, a mistaken capture with a new entry.
    HoldNotReversible {
        /// The named entry.
        target: EntryId,
    },

    // ── Limits (500–599) ────────────────────────────────────────────────────
    /// 500: a gross total or count would leave its range.
    BalanceOverflow {
        /// The account; `None` for a seal's totals.
        account: Option<AccountPath>,
        /// The currency.
        currency: Currency,
        /// The layer.
        layer: Layer,
    },
    /// 501: the entry leaves an account beyond its balance limit, and worse
    /// than before.
    LimitExceeded {
        /// The account.
        account: AccountPath,
        /// The currency.
        currency: Currency,
        /// The limit in force.
        limit: BalanceLimit,
        /// How far the entry would leave the account beyond the limit.
        shortfall: ScaledAmount,
    },
}

/// One line, honouring width, fill and alignment and never cut short.
impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::InvalidKey { reason } => format!("[100] invalid idempotency key: {reason}"),
            Self::TooFewPostings { count } => {
                format!("[101] an entry needs at least two postings, found {count}")
            }
            Self::TooManyPostings { count, max } => {
                format!("[102] an entry has {count} postings, more than the maximum of {max}")
            }
            Self::InvalidPath { posting, reason } => format!(
                "[103] invalid account path{}: {reason}",
                posting_suffix(posting)
            ),
            Self::NonPositiveAmount { posting } => {
                format!("[104] posting {posting} has an amount that is not positive")
            }
            Self::InvalidDimensions { posting, reason } => {
                format!("[105] posting {posting} has invalid dimensions: {reason}")
            }
            Self::Unbalanced {
                currency,
                layer,
                debit,
                credit,
            } => format!(
                "[106] {currency} {layer:?} does not balance: debits {debit}, credits {credit}"
            ),
            Self::InvalidLabel { field, reason } => format!("[107] invalid {field}: {reason}"),
            Self::InvalidMemo { reason } => format!("[108] invalid memo: {reason}"),
            Self::TooManyDocuments { count, max } => {
                format!("[109] an entry has {count} documents, more than the maximum of {max}")
            }
            Self::TooManyRates { count, max } => {
                format!("[110] an entry has {count} rates, more than the maximum of {max}")
            }
            Self::RecordTooLarge { bytes, max } => {
                format!("[111] the record would be {bytes} bytes, more than the maximum of {max}")
            }
            Self::TotalOutOfRange { currency, layer } => format!(
                "[113] the {currency} {layer:?} total of the entry is beyond the amount range"
            ),
            Self::MemoSaltMissing => String::from("[114] the memo has no salt"),
            Self::NonPositiveClearingAmount => {
                String::from("[120] the clearing amount is not positive")
            }
            Self::InvalidPeriodRange { start, end } => {
                format!("[121] the period ends on {end}, before it starts on {start}")
            }
            Self::InvalidOpenWindow { open_from, open_to } => {
                format!("[122] the account closes on {open_to}, before it opens on {open_from}")
            }
            Self::ExponentTooLarge { exponent, max } => {
                format!("[123] currency exponent {exponent} exceeds the ledger scale {max}")
            }
            Self::HoldLegs => String::from(
                "[140] a hold needs exactly two pending postings, one debit and one credit",
            ),
            Self::HoldExpiry { booking, expires } => {
                format!("[141] the hold expires on {expires}, not after its booking date {booking}")
            }
            Self::ResolutionPostings { count } => {
                format!("[142] a hold resolution carries no postings of its own, found {count}")
            }
            Self::PendingOutsideHold { posting } => format!(
                "[143] posting {posting} is pending, but the entry opens or resolves no hold"
            ),
            Self::ReversalWithHoldAction => {
                String::from("[144] an entry cannot both reverse an entry and act on a hold")
            }
            Self::PostAmountNotPositive => {
                String::from("[145] a partial post of a hold must be positive")
            }
            Self::KeyConflict { position } => format!(
                "[200] the key was already used, for different content, at position {position}"
            ),
            Self::NoPeriod { date } => format!("[300] {date} lies in no defined period"),
            Self::PeriodSealed { period, date } => {
                format!("[301] {date} lies in the sealed period {period}")
            }
            Self::UnknownAccount { account } => {
                format!("[302] unknown account {}", account_ref(account))
            }
            Self::NotALeaf { account } => {
                format!("[303] {account} has sub-accounts and cannot be posted to")
            }
            Self::AccountNotOpen { account, date } => {
                format!("[304] {account} is not open on {date}")
            }
            Self::UndeclaredCurrency { currency, date } => {
                format!("[305] {currency} is not declared on {date}")
            }
            Self::ExcessPrecision {
                posting,
                currency,
                exponent,
            } => format!(
                "[306] posting {posting} has more than {exponent} decimal places for {currency}"
            ),
            Self::MissingDimension { posting, axis } => {
                format!("[307] posting {posting} lacks the required dimension {axis}")
            }
            Self::ValueDateDrift { days, max } => format!(
                "[308] the value date is {days} days from the booking date, more than {max}"
            ),
            Self::AtOrBelowWatermark {
                effective,
                watermark,
            } => format!("[309] {effective} is on or before the sealed watermark {watermark}"),
            Self::EffectiveDateRegresses { effective, latest } => {
                format!("[310] {effective} is before the latest change, effective {latest}")
            }
            Self::UnknownPeriod { period } => format!("[311] unknown period {period}"),
            Self::ReversalTargetUnknown { target } => {
                format!("[400] the reversal target {target} does not exist")
            }
            Self::AlreadyReversed { target, by } => {
                format!("[401] {target} was already reversed by {by}")
            }
            Self::ReversalBeforeTarget { target, booked } => {
                format!("[402] the reversal is dated before {target}, booked on {booked}")
            }
            Self::NotAMirror { target, posting } => format!(
                "[403] the reversal does not mirror {target}{}",
                posting_suffix(posting)
            ),
            Self::ClearingTargetUnknown { posting } => {
                format!("[410] the posting {} does not exist", posting_ref(posting))
            }
            Self::ClearingSideMismatch { posting, expected } => format!(
                "[411] the posting {} is not a {expected:?} posting",
                posting_ref(posting)
            ),
            Self::ClearingKeyMismatch { debit, credit } => format!(
                "[412] the cleared postings are on different balances: {debit} and {credit}"
            ),
            Self::ClearingPrecision { currency, exponent } => format!(
                "[413] the clearing amount has more than {exponent} decimal places for {currency}"
            ),
            Self::OverApplied {
                posting,
                open,
                requested,
            } => format!(
                "[414] {requested} exceeds the {open} still open on {}",
                posting_ref(posting)
            ),
            Self::ClearingBeforePosting { posting, booked } => format!(
                "[415] the clearing is dated before {}, booked on {booked}",
                posting_ref(posting)
            ),
            Self::ClearingUnknown { clearing } => {
                format!("[416] no clearing was applied at position {clearing}")
            }
            Self::ClearingAlreadyReset { clearing, by } => format!(
                "[417] the clearing at position {clearing} was already reset at position {by}"
            ),
            Self::ResetBeforeClearing { cleared_on } => {
                format!("[418] the reset is dated before the clearing of {cleared_on}")
            }
            Self::ClearingNotSettled { posting } => format!(
                "[419] the posting {} is Pending: only Settled postings are cleared",
                posting_ref(posting)
            ),
            Self::AccountExists { path, handle } => format!(
                "[420] {path} is already registered, as {handle}, with different master data"
            ),
            Self::ParentHasPostings { parent } => {
                format!("[421] {parent} has postings and cannot get sub-accounts")
            }
            Self::ClassFrozen { account } => {
                format!("[422] the class of {account} is fixed: it has postings")
            }
            Self::PostingsAfterClose {
                account,
                last_booked,
            } => format!("[423] {account} has postings up to {last_booked}"),
            Self::ExponentLowered {
                currency,
                current,
                requested,
            } => format!(
                "[424] the exponent of {currency} is {current} and cannot be lowered to {requested}"
            ),
            Self::PeriodExists { period, start, end } => {
                format!("[425] the period {period} is already defined as {start} to {end}")
            }
            Self::PeriodGap {
                expected_start,
                start,
            } => format!("[426] the next period must start on {expected_start}, not {start}"),
            Self::PredecessorNotSealed {
                predecessor,
                earliest,
            } => format!(
                "[427] the preceding period {predecessor} is not sealed; seal from {earliest} on \
                 (seal_through seals every open period in order)"
            ),
            Self::TooManyAccounts => {
                String::from("[428] the ledger has the maximum number of accounts")
            }
            Self::ClosingEquityInScope { account } => format!(
                "[429] {account} is an income or expense account and cannot take the result"
            ),
            Self::ClosingAccountNotOpen { account, date } => {
                format!("[430] {account} has a balance to close but is not open on {date}")
            }
            Self::PolicyBeforeBookings {
                effective,
                last_booking,
            } => format!(
                "[431] a policy effective {effective} would govern entries already booked up to {last_booking}"
            ),
            Self::HoldNotFound { hold } => format!("[440] the hold {hold} does not exist"),
            Self::NotAHold { entry } => format!("[441] the entry {entry} is not a hold"),
            Self::HoldResolved { hold, by, how } => {
                format!("[442] the hold {hold} was already {how} by {by}")
            }
            Self::ResolutionBeforeHold { hold, held_on } => {
                format!("[443] the resolution is dated before the hold {hold}, booked on {held_on}")
            }
            Self::HoldExpired { hold, expires } => {
                format!("[444] the hold {hold} expired on {expires} and can no longer be posted")
            }
            Self::HoldNeverExpires { hold } => {
                format!("[445] the hold {hold} has no expiry date and cannot expire")
            }
            Self::HoldNotExpired { hold, expires } => {
                format!("[446] the hold {hold} does not expire before {expires}")
            }
            Self::PostAmountPrecision {
                hold,
                currency,
                exponent,
            } => format!(
                "[447] the post of the hold {hold} has more than {exponent} decimal places for {currency}"
            ),
            Self::PostExceedsHold {
                hold,
                held,
                requested,
            } => format!("[448] the post of {requested} exceeds the {held} held by {hold}"),
            Self::PostToClosedAccount {
                hold,
                account,
                date,
            } => format!(
                "[449] the hold {hold} cannot be posted: {} is not open on {date}",
                account_ref(account)
            ),
            Self::HoldNotReversible { target } => {
                format!("[450] {target} acts on a hold and cannot be reversed")
            }
            Self::BalanceOverflow {
                account,
                currency,
                layer,
            } => format!(
                "[500] the {currency} {layer:?} totals{} would overflow",
                account_suffix(account)
            ),
            Self::LimitExceeded {
                account,
                currency,
                shortfall,
                ..
            } => format!("[501] limit exceeded on {account} in {currency}: short by {shortfall}"),
        };
        pad(f, &text)
    }
}

impl std::error::Error for Rejection {}

impl Rejection {
    /// The stable numeric code; the hundreds give the [`Stage`].
    #[must_use]
    pub const fn code(&self) -> u16 {
        match self {
            Self::InvalidKey { .. } => 100,
            Self::TooFewPostings { .. } => 101,
            Self::TooManyPostings { .. } => 102,
            Self::InvalidPath { .. } => 103,
            Self::NonPositiveAmount { .. } => 104,
            Self::InvalidDimensions { .. } => 105,
            Self::Unbalanced { .. } => 106,
            Self::InvalidLabel { .. } => 107,
            Self::InvalidMemo { .. } => 108,
            Self::TooManyDocuments { .. } => 109,
            Self::TooManyRates { .. } => 110,
            Self::RecordTooLarge { .. } => 111,
            Self::TotalOutOfRange { .. } => 113,
            Self::MemoSaltMissing => 114,
            Self::NonPositiveClearingAmount => 120,
            Self::InvalidPeriodRange { .. } => 121,
            Self::InvalidOpenWindow { .. } => 122,
            Self::ExponentTooLarge { .. } => 123,
            Self::HoldLegs => 140,
            Self::HoldExpiry { .. } => 141,
            Self::ResolutionPostings { .. } => 142,
            Self::PendingOutsideHold { .. } => 143,
            Self::ReversalWithHoldAction => 144,
            Self::PostAmountNotPositive => 145,
            Self::KeyConflict { .. } => 200,
            Self::NoPeriod { .. } => 300,
            Self::PeriodSealed { .. } => 301,
            Self::UnknownAccount { .. } => 302,
            Self::NotALeaf { .. } => 303,
            Self::AccountNotOpen { .. } => 304,
            Self::UndeclaredCurrency { .. } => 305,
            Self::ExcessPrecision { .. } => 306,
            Self::MissingDimension { .. } => 307,
            Self::ValueDateDrift { .. } => 308,
            Self::AtOrBelowWatermark { .. } => 309,
            Self::EffectiveDateRegresses { .. } => 310,
            Self::UnknownPeriod { .. } => 311,
            Self::ReversalTargetUnknown { .. } => 400,
            Self::AlreadyReversed { .. } => 401,
            Self::ReversalBeforeTarget { .. } => 402,
            Self::NotAMirror { .. } => 403,
            Self::ClearingTargetUnknown { .. } => 410,
            Self::ClearingSideMismatch { .. } => 411,
            Self::ClearingKeyMismatch { .. } => 412,
            Self::ClearingPrecision { .. } => 413,
            Self::OverApplied { .. } => 414,
            Self::ClearingBeforePosting { .. } => 415,
            Self::ClearingUnknown { .. } => 416,
            Self::ClearingAlreadyReset { .. } => 417,
            Self::ResetBeforeClearing { .. } => 418,
            Self::ClearingNotSettled { .. } => 419,
            Self::AccountExists { .. } => 420,
            Self::ParentHasPostings { .. } => 421,
            Self::ClassFrozen { .. } => 422,
            Self::PostingsAfterClose { .. } => 423,
            Self::ExponentLowered { .. } => 424,
            Self::PeriodExists { .. } => 425,
            Self::PeriodGap { .. } => 426,
            Self::PredecessorNotSealed { .. } => 427,
            Self::TooManyAccounts => 428,
            Self::ClosingEquityInScope { .. } => 429,
            Self::ClosingAccountNotOpen { .. } => 430,
            Self::PolicyBeforeBookings { .. } => 431,
            Self::HoldNotFound { .. } => 440,
            Self::NotAHold { .. } => 441,
            Self::HoldResolved { .. } => 442,
            Self::ResolutionBeforeHold { .. } => 443,
            Self::HoldExpired { .. } => 444,
            Self::HoldNeverExpires { .. } => 445,
            Self::HoldNotExpired { .. } => 446,
            Self::PostAmountPrecision { .. } => 447,
            Self::PostExceedsHold { .. } => 448,
            Self::PostToClosedAccount { .. } => 449,
            Self::HoldNotReversible { .. } => 450,
            Self::BalanceOverflow { .. } => 500,
            Self::LimitExceeded { .. } => 501,
        }
    }

    /// The stage that refused the input.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        match self {
            Self::InvalidKey { .. }
            | Self::TooFewPostings { .. }
            | Self::TooManyPostings { .. }
            | Self::InvalidPath { .. }
            | Self::NonPositiveAmount { .. }
            | Self::InvalidDimensions { .. }
            | Self::Unbalanced { .. }
            | Self::InvalidLabel { .. }
            | Self::InvalidMemo { .. }
            | Self::TooManyDocuments { .. }
            | Self::TooManyRates { .. }
            | Self::RecordTooLarge { .. }
            | Self::TotalOutOfRange { .. }
            | Self::MemoSaltMissing
            | Self::NonPositiveClearingAmount
            | Self::InvalidPeriodRange { .. }
            | Self::InvalidOpenWindow { .. }
            | Self::ExponentTooLarge { .. }
            | Self::HoldLegs
            | Self::HoldExpiry { .. }
            | Self::ResolutionPostings { .. }
            | Self::PendingOutsideHold { .. }
            | Self::ReversalWithHoldAction
            | Self::PostAmountNotPositive => Stage::Structural,
            Self::KeyConflict { .. } => Stage::Idempotency,
            Self::NoPeriod { .. }
            | Self::PeriodSealed { .. }
            | Self::UnknownAccount { .. }
            | Self::NotALeaf { .. }
            | Self::AccountNotOpen { .. }
            | Self::UndeclaredCurrency { .. }
            | Self::ExcessPrecision { .. }
            | Self::MissingDimension { .. }
            | Self::ValueDateDrift { .. }
            | Self::AtOrBelowWatermark { .. }
            | Self::EffectiveDateRegresses { .. }
            | Self::UnknownPeriod { .. } => Stage::Context,
            Self::ReversalTargetUnknown { .. }
            | Self::AlreadyReversed { .. }
            | Self::ReversalBeforeTarget { .. }
            | Self::NotAMirror { .. }
            | Self::ClearingTargetUnknown { .. }
            | Self::ClearingSideMismatch { .. }
            | Self::ClearingKeyMismatch { .. }
            | Self::ClearingPrecision { .. }
            | Self::OverApplied { .. }
            | Self::ClearingBeforePosting { .. }
            | Self::ClearingUnknown { .. }
            | Self::ClearingAlreadyReset { .. }
            | Self::ResetBeforeClearing { .. }
            | Self::ClearingNotSettled { .. }
            | Self::AccountExists { .. }
            | Self::ParentHasPostings { .. }
            | Self::ClassFrozen { .. }
            | Self::PostingsAfterClose { .. }
            | Self::ExponentLowered { .. }
            | Self::PeriodExists { .. }
            | Self::PeriodGap { .. }
            | Self::PredecessorNotSealed { .. }
            | Self::TooManyAccounts
            | Self::ClosingEquityInScope { .. }
            | Self::ClosingAccountNotOpen { .. }
            | Self::PolicyBeforeBookings { .. }
            | Self::HoldNotFound { .. }
            | Self::NotAHold { .. }
            | Self::HoldResolved { .. }
            | Self::ResolutionBeforeHold { .. }
            | Self::HoldExpired { .. }
            | Self::HoldNeverExpires { .. }
            | Self::HoldNotExpired { .. }
            | Self::PostAmountPrecision { .. }
            | Self::PostExceedsHold { .. }
            | Self::PostToClosedAccount { .. }
            | Self::HoldNotReversible { .. } => Stage::References,
            Self::BalanceOverflow { .. } | Self::LimitExceeded { .. } => Stage::Limits,
        }
    }
}

fn posting_suffix(posting: &Option<u16>) -> String {
    posting.map_or_else(String::new, |posting| format!(" at posting {posting}"))
}

fn account_suffix(account: &Option<AccountPath>) -> String {
    account
        .as_ref()
        .map_or_else(String::new, |account| format!(" of {account}"))
}

fn account_ref(account: &AccountRef) -> String {
    match account {
        AccountRef::Path(path) => path.to_string(),
        AccountRef::Handle(handle) => handle.to_string(),
    }
}

fn posting_ref(posting: &PostingRef) -> String {
    format!("{}/{}", posting.entry, posting.index)
}

/// The stages of the rule core, in the order they run.
///
/// A stage runs only when every earlier stage passed; the stage of a
/// [`Rejection`] is the hundreds of its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Stage {
    /// The input alone (codes 1xx).
    Structural,
    /// The idempotency key (2xx).
    Idempotency,
    /// The ledger's state on the input's date (3xx).
    Context,
    /// The records the input names (4xx).
    References,
    /// Overflow and balance limits (5xx).
    Limits,
}

/// Which label of an input a [`Rejection::InvalidLabel`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum LabelField {
    /// An entry's kind.
    Kind,
    /// An account's class.
    Class,
    /// A period id.
    PeriodId,
    /// The actor of a master-data change.
    Actor,
}

impl fmt::Display for LabelField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Kind => "entry kind",
            Self::Class => "account class",
            Self::PeriodId => "period id",
            Self::Actor => "actor",
        })
    }
}

/// An amount carried by a [`Rejection`]: minor units and their scale, so one
/// `Rejection` type serves ledgers of every scale `P`.
///
/// ```
/// use doubleentry::{Amount, amount, error::ScaledAmount};
///
/// let minus_thirty: Amount = amount!("-30.00");
/// let shortfall = ScaledAmount::from(minus_thirty);
/// assert_eq!((shortfall.minor, shortfall.scale), (-3000, 2));
/// assert_eq!(shortfall.to_string(), "-30.00");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScaledAmount {
    /// The value in minor units (`value × 10^scale`).
    pub minor: i128,
    /// The number of decimal places.
    pub scale: u8,
}

impl<const P: u8> From<Amount<P>> for ScaledAmount {
    fn from(amount: Amount<P>) -> Self {
        Self {
            minor: amount.to_minor(),
            scale: P,
        }
    }
}

impl fmt::Display for ScaledAmount {
    /// `minor / 10^scale` with exactly `scale` decimals, sign first.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.minor < 0 { "-" } else { "" };
        let scale = usize::from(self.scale);
        if scale == 0 {
            return write!(f, "{sign}{}", self.minor.unsigned_abs());
        }
        let digits = format!(
            "{:0>width$}",
            self.minor.unsigned_abs(),
            width = scale.saturating_add(1)
        );
        let split = digits.len().saturating_sub(scale);
        match digits.split_at_checked(split) {
            Some((whole, fraction)) => write!(f, "{sign}{whole}.{fraction}"),
            None => write!(f, "{sign}{digits}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_amounts_print_every_decimal() {
        let cases = [
            (0, 2, "0.00"),
            (5, 2, "0.05"),
            (-5, 2, "-0.05"),
            (3000, 2, "30.00"),
            (-3000, 0, "-3000"),
            (1, 18, "0.000000000000000001"),
            (i128::MIN, 0, "-170141183460469231731687303715884105728"),
        ];
        for (minor, scale, text) in cases {
            assert_eq!(ScaledAmount { minor, scale }.to_string(), text);
        }
    }
}
