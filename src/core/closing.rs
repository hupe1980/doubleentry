//! Year-end closing: drafts that zero every income and expense account per
//! currency and dimension slice into an equity account.
//!
//! Each slice — (account, currency, exact dimension set) with a non-zero
//! Settled net through the closing date — gets one posting on the side that
//! flattens it, with the slice's dimensions, so a segment's result zeroes
//! too and a required-dimension policy is met. Per (currency, dimension set)
//! one equity posting with the same dimensions balances the group. The
//! drafts are packed to the posting and leaf limits — each measured with its
//! own key — and keyed `{key}/1`, `{key}/2`, …; recording them is an
//! ordinary `record`. Open holds (the Pending layer) carry over.

use std::collections::{BTreeMap, BTreeSet};

use time::Date;

use super::state::StateView;
use crate::error::Rejection;
use crate::model::{
    AccountHandle, AccountKind, AccountRef, Amount, Currency, Dimensions, Entry, IdempotencyKey,
    Layer, MAX_LEAF_BYTES, Posting, Side,
};

/// A closing posting on one account.
struct Leg<const P: u8> {
    handle: AccountHandle,
    side: Side,
    amount: Amount<P>,
}

impl<const P: u8> Leg<P> {
    fn posting(&self, currency: Currency, dimensions: &Dimensions) -> Posting<P> {
        match self.side {
            Side::Debit => Posting::debit(self.handle, self.amount, currency),
            Side::Credit => Posting::credit(self.handle, self.amount, currency),
        }
        .dimensions(dimensions.clone())
    }
}

/// The key of draft `number` (100 when it is too long).
fn draft_key(key: &str, number: usize) -> Result<IdempotencyKey, Rejection> {
    IdempotencyKey::new(format!("{key}/{number}"))
        .map_err(|reason| Rejection::InvalidKey { reason })
}

/// Builds the year-end closing drafts: 100 → 302, 303 → 429 → 430.
pub(crate) fn close_year<const P: u8>(
    view: &impl StateView<P>,
    key: &str,
    on: Date,
    equity: &AccountRef,
) -> Result<Vec<Entry<P>>, Rejection> {
    draft_key(key, 1)?;
    let account = match equity {
        AccountRef::Path(path) => view.account_by_path(path),
        AccountRef::Handle(handle) => view.account(*handle),
    }
    .ok_or_else(|| Rejection::UnknownAccount {
        account: equity.clone(),
    })?;
    if !account.is_leaf {
        return Err(Rejection::NotALeaf {
            account: account.path().clone(),
        });
    }
    if matches!(account.kind(), AccountKind::Income | AccountKind::Expense) {
        return Err(Rejection::ClosingEquityInScope {
            account: account.path().clone(),
        });
    }
    let slices = view.pnl_slices(on);
    // Every account is looked up before any is judged, so one plan round
    // discovers all of them, however many accounts close.
    let closing: BTreeSet<AccountHandle> = slices
        .iter()
        .filter(|slice| !slice.balance.net().is_zero())
        .map(|slice| slice.handle)
        .collect();
    let accounts: BTreeMap<AccountHandle, _> = closing
        .iter()
        .map(|handle| {
            let open = view
                .account_terms(*handle, on)
                .is_some_and(|terms| terms.window.contains(on));
            (*handle, (view.account(*handle), open))
        })
        .collect();
    for (handle, (account, open)) in &accounts {
        let closed = account.as_ref().ok_or(Rejection::UnknownAccount {
            account: AccountRef::Handle(*handle),
        })?;
        if !open {
            return Err(Rejection::ClosingAccountNotOpen {
                account: closed.path().clone(),
                date: on,
            });
        }
    }
    let mut groups: BTreeMap<(Currency, Dimensions), Vec<Leg<P>>> = BTreeMap::new();
    for slice in slices {
        let net = slice.balance.net();
        if net.is_zero() {
            continue;
        }
        groups
            .entry((slice.currency, slice.dimensions))
            .or_default()
            .push(Leg {
                handle: slice.handle,
                side: if net.is_positive() {
                    Side::Credit
                } else {
                    Side::Debit
                },
                amount: net.abs(),
            });
    }
    let drafts = pack(groups, account.handle(), on, key)?;
    drafts
        .into_iter()
        .enumerate()
        .map(|(index, postings)| {
            let key = draft_key(key, index.saturating_add(1))?;
            Ok(postings
                .into_iter()
                .fold(Entry::new(key, on), Entry::posting))
        })
        .collect()
}

/// Packs the groups' legs, each group followed by its equity leg, into
/// drafts within the posting and leaf limits. A group split across drafts
/// gets an equity leg in each part.
fn pack<const P: u8>(
    groups: BTreeMap<(Currency, Dimensions), Vec<Leg<P>>>,
    equity: AccountHandle,
    on: Date,
    key: &str,
) -> Result<Vec<Vec<Posting<P>>>, Rejection> {
    let base = Entry::<P>::new(draft_key(key, 1)?, on);
    let base_len = base.encoded_len();
    let len_of = |posting: &Posting<P>| {
        base.clone()
            .posting(posting.clone())
            .encoded_len()
            .saturating_sub(base_len)
    };
    // A draft is measured with its own key: `{key}/10` is a byte longer
    // than `{key}/1`, `{key}/100` two.
    let empty_of = |number: usize| -> Result<usize, Rejection> {
        Ok(Entry::<P>::new(draft_key(key, number)?, on).encoded_len())
    };
    let mut drafts: Vec<Vec<Posting<P>>> = Vec::new();
    let mut current: Vec<Posting<P>> = Vec::new();
    let mut current_len = empty_of(1)?;
    for ((currency, dimensions), legs) in groups {
        let equity_len = len_of(&equity_leg(equity, Amount::ZERO, currency, &dimensions));
        let mut legs = legs.into_iter().peekable();
        while legs.peek().is_some() {
            let mut part = Vec::new();
            let mut part_len = 0usize;
            let mut total = Amount::<P>::ZERO;
            while let Some(leg) = legs.peek() {
                let posting = leg.posting(currency, &dimensions);
                let len = len_of(&posting);
                let count = current.len().saturating_add(part.len()).saturating_add(2);
                let bytes = current_len
                    .saturating_add(part_len)
                    .saturating_add(len)
                    .saturating_add(equity_len);
                if count > Entry::<P>::MAX_POSTINGS || bytes > MAX_LEAF_BYTES {
                    break;
                }
                total = match leg.side {
                    Side::Debit => total.checked_sub(leg.amount),
                    Side::Credit => total.checked_add(leg.amount),
                }
                .map_err(|_| Rejection::TotalOutOfRange {
                    currency,
                    layer: Layer::Settled,
                })?;
                part.push(posting);
                part_len = part_len.saturating_add(len);
                legs.next();
            }
            if part.is_empty() {
                if current.is_empty() {
                    // One leg and its equity leg do not fit an empty draft.
                    return Err(Rejection::RecordTooLarge {
                        bytes: current_len.saturating_add(equity_len.saturating_mul(2)),
                        max: MAX_LEAF_BYTES,
                    });
                }
                drafts.push(std::mem::take(&mut current));
                current_len = empty_of(drafts.len().saturating_add(1))?;
                continue;
            }
            if !total.is_zero() {
                part.push(equity_leg(equity, total, currency, &dimensions));
                part_len = part_len.saturating_add(equity_len);
            }
            current.extend(part);
            current_len = current_len.saturating_add(part_len);
            if legs.peek().is_some() {
                drafts.push(std::mem::take(&mut current));
                current_len = empty_of(drafts.len().saturating_add(1))?;
            }
        }
    }
    if !current.is_empty() {
        drafts.push(current);
    }
    Ok(drafts)
}

/// The equity posting that balances a group whose closing legs sum to
/// `total` (credits positive): a debit for a positive total, a credit
/// otherwise.
fn equity_leg<const P: u8>(
    equity: AccountHandle,
    total: Amount<P>,
    currency: Currency,
    dimensions: &Dimensions,
) -> Posting<P> {
    if total.is_negative() {
        Posting::credit(equity, total.abs(), currency)
    } else {
        Posting::debit(equity, total, currency)
    }
    .dimensions(dimensions.clone())
}
