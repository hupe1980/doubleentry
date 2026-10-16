//! Holds: one reservation, ended once.
//!
//! A hold is an entry recorded with [`HoldAction::Open`]: exactly two Pending
//! postings, one debit and one credit, one currency and one amount. One later
//! entry resolves it — a Post (all of it, or part with the rest released), a
//! Void, or an Expire on or after its expiry date — and a resolution carries
//! no postings of its own: the core derives them from the hold
//! ([`derive_legs`]) and the record stores them, so every reader sees
//! ordinary postings. **The Pending layer belongs to holds**: every other
//! posting is Settled, so an account's Pending balance is what its open holds
//! reserve.
//!
//! The rules run inside the stages of an entry: the shape in stage 1
//! ([`structural`]), the hold a resolution names in stage 4 after the
//! reversal checks ([`check`]), and the reserved amounts in stage 5 (the
//! limits read [`outstanding_after`]). There is no clock: expiry compares the
//! hold's date with the resolution's booking date, both the caller's. A
//! hold's state is derived, never authority: [`HoldState::derive`] computes
//! it from the hold and resolution records alone.

use core::fmt;
use std::collections::BTreeMap;

use time::Date;

use super::context::{self, EntryFacts};
use super::state::StateView;
use super::writeset::Outstanding;
use crate::error::{Rejection, ScaledAmount};
use crate::model::{
    AccountHandle, AccountRef, Amount, Currency, EntryId, EntryParts, EntryRecord, HoldAction,
    Layer, LedgerId, MAX_LEAF_BYTES, PostAmount, PostingRecord, Side, pad,
};
use crate::store::HoldRow;

use super::structural::index;

/// An open or resolved hold as the core reads it: the hold index row with
/// the hold's two postings — what a store answers [`Read::Hold`](super::Read)
/// with.
///
/// ```
/// use doubleentry::store::HoldView;
/// use doubleentry::{AccountKind, Entry, Journal, LedgerId, ResolutionKind, amount, date, currency::EUR};
///
/// let mut books = Journal::new(LedgerId::new("example.com/payments")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
/// books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
/// books.define_months(2026, 11..=11)?;
/// let hold = books.record(
///     Entry::open_hold("auth-4711", date!("2026-11-20"))
///         .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
///         .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
/// )?;
/// let record = books.entry(hold.id).ok_or("no entry")?;
/// let view = HoldView::open(hold.id, hold.position, &record).ok_or("no hold")?;
/// assert_eq!((view.amount(), view.currency()), (amount!("100.00"), EUR));
/// assert_eq!(view.debit().handle.get(), 0);
///
/// let void = books.record(Entry::void_hold("void-4711", date!("2026-11-21"), hold.id))?;
/// let row = view.resolved(void.id, ResolutionKind::Voided).row();
/// assert_eq!(row.resolution, Some((void.id, ResolutionKind::Voided)));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldView<const P: u8> {
    /// The id of the entry that opened it.
    pub id: EntryId,
    /// That entry's position.
    pub position: u64,
    /// Its booking date.
    pub booking: Date,
    /// Its expiry date, if any.
    pub expires: Option<Date>,
    /// Its two Pending postings, in record order (one debit, one credit).
    pub legs: [PostingRecord<P>; 2],
    /// The entry that resolved it and how; `None` while open.
    pub resolution: Option<(EntryId, ResolutionKind)>,
}

impl<const P: u8> HoldView<P> {
    /// The open hold recorded as `record` (id `id`, at `position`); `None`
    /// when the record is not a hold of two postings, one debit and one
    /// credit.
    #[must_use]
    pub fn open(id: EntryId, position: u64, record: &EntryRecord<P>) -> Option<Self> {
        let parts = record.parts();
        let Some(HoldAction::Open { expires }) = parts.hold else {
            return None;
        };
        let [first, second] = <[PostingRecord<P>; 2]>::try_from(parts.postings.clone()).ok()?;
        (first.side != second.side).then_some(Self {
            id,
            position,
            booking: parts.booking_date,
            expires,
            legs: [first, second],
            resolution: None,
        })
    }

    /// The same hold, resolved by entry `by`.
    #[must_use]
    pub fn resolved(self, by: EntryId, how: ResolutionKind) -> Self {
        Self {
            resolution: Some((by, how)),
            ..self
        }
    }

    /// The debit posting.
    #[must_use]
    pub fn debit(&self) -> &PostingRecord<P> {
        self.leg(Side::Debit)
    }

    /// The credit posting.
    #[must_use]
    pub fn credit(&self) -> &PostingRecord<P> {
        self.leg(Side::Credit)
    }

    fn leg(&self, side: Side) -> &PostingRecord<P> {
        let [first, second] = &self.legs;
        if first.side == side { first } else { second }
    }

    /// The amount held.
    #[must_use]
    pub const fn amount(&self) -> Amount<P> {
        let [first, _] = &self.legs;
        first.amount
    }

    /// The currency.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        let [first, _] = &self.legs;
        first.currency
    }

    /// The hold as the hold index keeps it.
    #[must_use]
    pub fn row(&self) -> HoldRow<P> {
        HoldRow {
            id: self.id,
            position: self.position,
            booking: self.booking,
            expires: self.expires,
            debit: self.debit().handle,
            credit: self.credit().handle,
            currency: self.currency(),
            amount: self.amount(),
            resolution: self.resolution,
        }
    }
}

/// How an entry changes the hold index.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HoldEffect<const P: u8> {
    /// The entry opened this hold.
    Opened(HoldView<P>),
    /// The entry `by` resolved the hold `hold`.
    Resolved {
        /// The hold.
        hold: EntryId,
        /// The resolving entry.
        by: EntryId,
        /// How.
        how: ResolutionKind,
    },
}

/// How a hold was resolved.
///
/// ```
/// use doubleentry::ResolutionKind;
///
/// assert_eq!(ResolutionKind::Posted.to_string(), "posted");
/// assert_eq!(format!("{:>8}", ResolutionKind::Voided), "  voided");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ResolutionKind {
    /// Posted: the held amount (or part of it) was booked, the rest released.
    Posted,
    /// Voided: the whole amount was released.
    Voided,
    /// Expired: released on or after the hold's expiry date.
    Expired,
}

impl fmt::Display for ResolutionKind {
    /// `posted`, `voided` or `expired`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        pad(
            f,
            match self {
                Self::Posted => "posted",
                Self::Voided => "voided",
                Self::Expired => "expired",
            },
        )
    }
}

/// Where a hold stands.
///
/// ```
/// use doubleentry::{EntryId, HoldStatus, amount};
///
/// let by = EntryId::from_bytes([7; 16]);
/// let posted: HoldStatus = HoldStatus::Posted { by, amount: amount!("60.00") };
/// assert_eq!(posted.resolved_by(), Some(by));
/// assert_eq!(posted.to_string(), format!("posted 60.00 by {by}"));
/// assert_eq!(HoldStatus::<2>::Open.resolved_by(), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum HoldStatus<const P: u8 = 2> {
    /// Not resolved yet: the amount is reserved.
    Open,
    /// Posted by an entry: `amount` was settled, the rest released.
    Posted {
        /// The resolving entry.
        by: EntryId,
        /// The amount settled.
        amount: Amount<P>,
    },
    /// Voided by an entry.
    Voided {
        /// The resolving entry.
        by: EntryId,
    },
    /// Expired by an entry.
    Expired {
        /// The resolving entry.
        by: EntryId,
    },
}

impl<const P: u8> HoldStatus<P> {
    /// The entry that resolved the hold; `None` while open.
    #[must_use]
    pub const fn resolved_by(&self) -> Option<EntryId> {
        match self {
            Self::Open => None,
            Self::Posted { by, .. } | Self::Voided { by } | Self::Expired { by } => Some(*by),
        }
    }
}

impl<const P: u8> fmt::Display for HoldStatus<P> {
    /// `open`, `posted 60.00 by …`, `voided by …` or `expired by …`,
    /// honouring width, fill and alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Open => String::from("open"),
            Self::Posted { by, amount } => format!("posted {amount} by {by}"),
            Self::Voided { by } => format!("voided by {by}"),
            Self::Expired { by } => format!("expired by {by}"),
        };
        pad(f, &text)
    }
}

/// A hold and where it stands, derived from its records.
///
/// ```
/// use doubleentry::{AccountKind, Entry, HoldStatus, Journal, LedgerId, PostAmount, amount, date, currency::EUR};
///
/// let mut books = Journal::new(LedgerId::new("example.com/payments")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
/// books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
/// books.define_months(2026, 11..=11)?;
/// let hold = books.record(
///     Entry::open_hold("auth-4711", date!("2026-11-20"))
///         .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
///         .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
/// )?;
/// assert_eq!(books.hold(hold.id).map(|state| state.remaining()), Some(amount!("100.00")));
///
/// let capture = books.record(Entry::post_hold(
///     "capture-4711",
///     date!("2026-11-22"),
///     hold.id,
///     PostAmount::Partial(amount!("60.00")),
/// ))?;
/// let state = books.hold(hold.id).ok_or("no hold")?;
/// assert_eq!(state.status, HoldStatus::Posted { by: capture.id, amount: amount!("60.00") });
/// assert_eq!(state.remaining(), amount!("0.00"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct HoldState<const P: u8 = 2> {
    /// The id of the entry that opened it.
    pub id: EntryId,
    /// That entry's position.
    pub position: u64,
    /// Its booking date.
    pub booking: Date,
    /// Its expiry date, if any.
    pub expires: Option<Date>,
    /// The account debited.
    pub debit: AccountHandle,
    /// The account credited.
    pub credit: AccountHandle,
    /// The currency.
    pub currency: Currency,
    /// The amount held.
    pub amount: Amount<P>,
    /// Where it stands.
    pub status: HoldStatus<P>,
}

impl<const P: u8> HoldState<P> {
    /// The state of the hold recorded as `hold` at `position` in `ledger`,
    /// given the record of the entry that resolved it, if any. Pure: the
    /// state is a function of the two records, so a verifier holding them
    /// derives what the books report.
    ///
    /// `None` when `hold` is not a hold, or `resolution` does not resolve it.
    #[must_use]
    pub fn derive(
        ledger: &LedgerId,
        position: u64,
        hold: &EntryRecord<P>,
        resolution: Option<&EntryRecord<P>>,
    ) -> Option<Self> {
        let id = hold.id(ledger);
        let view = HoldView::open(id, position, hold)?;
        let held = view.amount();
        let row = view.row();
        let Some(resolution) = resolution else {
            return Some(Self::from_row(&row, held));
        };
        let by = resolution.id(ledger);
        let (target, how, posted) = match resolution.parts().hold? {
            HoldAction::Post { hold, amount } => {
                (hold, ResolutionKind::Posted, posted(held, amount))
            }
            HoldAction::Void { hold } => (hold, ResolutionKind::Voided, held),
            HoldAction::Expire { hold } => (hold, ResolutionKind::Expired, held),
            HoldAction::Open { .. } => return None,
        };
        (target == id).then(|| Self::from_row(&view.resolved(by, how).row(), posted))
    }

    /// The state of a hold index row; `posted` is the amount settled when
    /// the row says the hold was posted.
    pub(crate) fn from_row(row: &HoldRow<P>, posted: Amount<P>) -> Self {
        let status = match row.resolution {
            None => HoldStatus::Open,
            Some((by, ResolutionKind::Posted)) => HoldStatus::Posted { by, amount: posted },
            Some((by, ResolutionKind::Voided)) => HoldStatus::Voided { by },
            Some((by, ResolutionKind::Expired)) => HoldStatus::Expired { by },
        };
        Self {
            id: row.id,
            position: row.position,
            booking: row.booking,
            expires: row.expires,
            debit: row.debit,
            credit: row.credit,
            currency: row.currency,
            amount: row.amount,
            status,
        }
    }

    /// Whether the hold is still open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        matches!(self.status, HoldStatus::Open)
    }

    /// What the hold still reserves: its amount while open, zero once
    /// resolved (a partial post releases the rest).
    #[must_use]
    pub const fn remaining(&self) -> Amount<P> {
        if self.is_open() {
            self.amount
        } else {
            Amount::ZERO
        }
    }
}

impl<const P: u8> fmt::Display for HoldState<P> {
    /// `hold 1a2b… 100.00 EUR #0 → #1, open` (or the resolution), honouring
    /// width, fill and alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let expires = self
            .expires
            .map_or_else(String::new, |expires| format!(" until {expires}"));
        pad(
            f,
            &format!(
                "hold {} {} {} {} → {}{expires}, {}",
                self.id, self.amount, self.currency, self.debit, self.credit, self.status
            ),
        )
    }
}

/// The amount a post of `amount` settles from a hold of `held`.
pub(crate) const fn posted<const P: u8>(held: Amount<P>, amount: PostAmount<P>) -> Amount<P> {
    match amount {
        PostAmount::Full => held,
        PostAmount::Partial(amount) => amount,
    }
}

/// The hold rules of stage 1 for an entry with `hold`, booked on `booking`,
/// reversing `reverses`, with postings of these (layer, side). A stored
/// resolution (`derived`) carries the postings the core derived; a draft
/// carries none.
pub(crate) fn structural<const P: u8>(
    hold: Option<&HoldAction<P>>,
    booking: Date,
    reverses: Option<EntryId>,
    postings: &[(Layer, Side)],
    derived: bool,
) -> Vec<Rejection> {
    let mut found = Vec::new();
    match hold {
        Some(HoldAction::Open { expires }) => {
            let legs = matches!(
                postings,
                [(Layer::Pending, a), (Layer::Pending, b)] if a != b
            );
            if !legs {
                found.push(Rejection::HoldLegs);
            }
            if let Some(expires) = *expires
                && expires <= booking
            {
                found.push(Rejection::HoldExpiry { booking, expires });
            }
        }
        Some(HoldAction::Post { amount, .. }) => {
            if let PostAmount::Partial(amount) = amount
                && !amount.is_positive()
            {
                found.push(Rejection::PostAmountNotPositive);
            }
        }
        Some(HoldAction::Void { .. } | HoldAction::Expire { .. }) | None => {}
    }
    if is_resolution(hold) && !derived && !postings.is_empty() {
        found.push(Rejection::ResolutionPostings {
            count: postings.len(),
        });
    }
    if hold.is_none()
        && let Some(posting) = postings
            .iter()
            .position(|(layer, _)| *layer == Layer::Pending)
    {
        found.push(Rejection::PendingOutsideHold {
            posting: index(posting),
        });
    }
    if hold.is_some() && reverses.is_some() {
        found.push(Rejection::ReversalWithHoldAction);
    }
    found
}

/// Whether an entry is a hold resolution (Post, Void, Expire), whose postings
/// the core derives — so the two-postings minimum does not apply to it.
pub(crate) const fn is_resolution<const P: u8>(hold: Option<&HoldAction<P>>) -> bool {
    matches!(
        hold,
        Some(HoldAction::Post { .. } | HoldAction::Void { .. } | HoldAction::Expire { .. })
    )
}

/// Whether an entry only releases a hold (Void, Expire): such an entry is
/// never refused by a balance limit.
pub(crate) const fn is_release<const P: u8>(hold: Option<&HoldAction<P>>) -> bool {
    matches!(
        hold,
        Some(HoldAction::Void { .. } | HoldAction::Expire { .. })
    )
}

/// What the hold rules of stage 4 decided for an entry.
pub(crate) struct Checked<const P: u8> {
    /// The record to append: a resolution's carries its derived postings.
    pub(crate) record: EntryRecord<P>,
    /// The hold index change.
    pub(crate) effect: Option<HoldEffect<P>>,
    /// For a resolution, what stage 5 reads of its derived postings.
    pub(crate) derived: Option<EntryFacts<P>>,
    /// What open holds reserve after the entry, where it changes that.
    pub(crate) outstanding: Vec<Outstanding<P>>,
}

/// The hold rules of stage 4: a resolution's hold must exist (440), be a
/// hold (441), be open (442) and be booked no later than it (443); a Post
/// must come before the expiry date (444), respect the currency's exponent
/// (447) and the held amount (448), and find both accounts open (449); an
/// Expire needs an expiry date (445) that has come (446). Then the
/// resolution's postings are derived.
///
/// Every read the rules need is made before the first is judged, so a store
/// planning the reads learns them in one round.
pub(crate) fn check<const P: u8>(
    view: &impl StateView<P>,
    record: EntryRecord<P>,
) -> Result<Checked<P>, Rejection> {
    let header = view.header();
    let id = record.id(&header.ledger);
    let parts = record.parts();
    let booking = parts.booking_date;
    let Some(action) = parts.hold else {
        return Ok(Checked {
            record,
            effect: None,
            derived: None,
            outstanding: Vec::new(),
        });
    };
    let (target, how) = match action {
        HoldAction::Open { .. } => {
            // Stage 1 let only a hold of two postings, one per side, through.
            let opened = HoldView::open(id, header.size, &record).ok_or(Rejection::HoldLegs)?;
            let outstanding = outstanding(view, &opened, true)?;
            return Ok(Checked {
                record,
                effect: Some(HoldEffect::Opened(opened)),
                derived: None,
                outstanding,
            });
        }
        HoldAction::Post { hold, .. } => (hold, ResolutionKind::Posted),
        HoldAction::Void { hold } => (hold, ResolutionKind::Voided),
        HoldAction::Expire { hold } => (hold, ResolutionKind::Expired),
    };
    let Some(hold) = view.hold(target) else {
        return Err(match view.entry(target) {
            None => Rejection::HoldNotFound { hold: target },
            Some(_) => Rejection::NotAHold { entry: target },
        });
    };
    let postings = derive_legs(&hold, &action);
    let derived = context::gather(view, booking, &context::legs(&postings), None);
    let released = outstanding(view, &hold, false);
    if let Some((by, how)) = hold.resolution {
        return Err(Rejection::HoldResolved {
            hold: target,
            by,
            how,
        });
    }
    if booking < hold.booking {
        return Err(Rejection::ResolutionBeforeHold {
            hold: target,
            held_on: hold.booking,
        });
    }
    match action {
        HoldAction::Post { amount, .. } => post(view, &hold, amount, booking)?,
        HoldAction::Expire { .. } => {
            let expires = hold
                .expires
                .ok_or(Rejection::HoldNeverExpires { hold: target })?;
            if booking < expires {
                return Err(Rejection::HoldNotExpired {
                    hold: target,
                    expires,
                });
            }
        }
        HoldAction::Void { .. } | HoldAction::Open { .. } => {}
    }
    let outstanding = released?;
    // Only the size can fail: the parts came from a valid record, and a
    // resolution carries at most four postings.
    let record = EntryRecord::new(EntryParts {
        postings,
        ..record.into_parts()
    })
    .map_err(|error| super::structural::record_error(error, MAX_LEAF_BYTES))?;
    Ok(Checked {
        record,
        effect: Some(HoldEffect::Resolved {
            hold: target,
            by: id,
            how,
        }),
        derived: Some(derived),
        outstanding,
    })
}

/// The rules of a Post of `amount` booked on `booking`: 444, 447, 448, 449.
fn post<const P: u8>(
    view: &impl StateView<P>,
    hold: &HoldView<P>,
    amount: PostAmount<P>,
    booking: Date,
) -> Result<(), Rejection> {
    let target = hold.id;
    if let Some(expires) = hold.expires
        && booking >= expires
    {
        return Err(Rejection::HoldExpired {
            hold: target,
            expires,
        });
    }
    if let PostAmount::Partial(requested) = amount {
        let currency = hold.currency();
        // The currency was declared when the hold was booked, and a
        // declaration is never withdrawn.
        if let Some(exponent) = view.currency(currency, booking)
            && !requested.fits_exponent(exponent)
        {
            return Err(Rejection::PostAmountPrecision {
                hold: target,
                currency,
                exponent,
            });
        }
        if requested > hold.amount() {
            return Err(Rejection::PostExceedsHold {
                hold: target,
                held: ScaledAmount::from(hold.amount()),
                requested: ScaledAmount::from(requested),
            });
        }
    }
    for leg in &hold.legs {
        let open = view
            .account_terms(leg.handle, booking)
            .is_some_and(|terms| terms.window.contains(booking));
        if !open {
            return Err(Rejection::PostToClosedAccount {
                hold: target,
                account: view
                    .account(leg.handle)
                    .map_or(AccountRef::Handle(leg.handle), |account| {
                        AccountRef::Path(account.path().clone())
                    }),
                date: booking,
            });
        }
    }
    Ok(())
}

/// The postings a resolution of `hold` carries, in this order: the hold's
/// two postings released (side swapped, Pending), then for a Post the
/// amount posted settled (same side, Settled) — each with the hold
/// posting's account, currency and dimensions.
pub(crate) fn derive_legs<const P: u8>(
    hold: &HoldView<P>,
    action: &HoldAction<P>,
) -> Vec<PostingRecord<P>> {
    let release = hold.legs.iter().map(|leg| PostingRecord {
        side: leg.side.opposite(),
        layer: Layer::Pending,
        ..leg.clone()
    });
    let settle = match action {
        HoldAction::Post { amount, .. } => {
            let amount = posted(hold.amount(), *amount);
            hold.legs
                .iter()
                .map(|leg| PostingRecord {
                    amount,
                    layer: Layer::Settled,
                    ..leg.clone()
                })
                .collect()
        }
        HoldAction::Open { .. } | HoldAction::Void { .. } | HoldAction::Expire { .. } => Vec::new(),
    };
    release.chain(settle).collect()
}

/// The amounts open holds reserve on the accounts of `hold` once it is
/// `opened` (its amount added to its debit account's debits and its credit
/// account's credits) or resolved (removed), ascending by account.
///
/// # Errors
///
/// 500 when a reserved amount would leave the amount range.
fn outstanding<const P: u8>(
    view: &impl StateView<P>,
    hold: &HoldView<P>,
    opened: bool,
) -> Result<Vec<Outstanding<P>>, Rejection> {
    let (amount, currency) = (hold.amount(), hold.currency());
    let mut rows: BTreeMap<AccountHandle, (Amount<P>, Amount<P>)> = BTreeMap::new();
    for leg in &hold.legs {
        let (debits, credits) = rows.entry(leg.handle).or_insert_with(|| {
            (
                view.outstanding_hold_debits(leg.handle, currency),
                view.outstanding_hold_credits(leg.handle, currency),
            )
        });
        let total = match leg.side {
            Side::Debit => debits,
            Side::Credit => credits,
        };
        let changed = if opened {
            total.checked_add(amount)
        } else {
            total.checked_sub(amount)
        };
        *total = changed.map_err(|_| Rejection::BalanceOverflow {
            account: view
                .account(leg.handle)
                .map(|account| account.path().clone()),
            currency,
            layer: Layer::Pending,
        })?;
    }
    Ok(rows
        .into_iter()
        .map(|(handle, (debits, credits))| Outstanding {
            handle,
            currency,
            debits,
            credits,
        })
        .collect())
}

/// The amounts open holds reserve on (account, currency) once the entry is
/// applied, as (debits, credits): `after` where the entry changes them, the
/// view's otherwise.
pub(crate) fn outstanding_after<const P: u8>(
    view: &impl StateView<P>,
    after: &[Outstanding<P>],
    handle: AccountHandle,
    currency: Currency,
) -> (Amount<P>, Amount<P>) {
    after
        .iter()
        .find(|row| row.handle == handle && row.currency == currency)
        .map_or_else(
            || {
                (
                    view.outstanding_hold_debits(handle, currency),
                    view.outstanding_hold_credits(handle, currency),
                )
            },
            |row| (row.debits, row.credits),
        )
}
