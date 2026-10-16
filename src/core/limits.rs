//! Stage 5 for entries: overflow, then balance limits.
//!
//! Limits are symmetric (TigerBeetle semantics). An account limited
//! `NoDebitBalance` has the exposure `settled debit + outstanding hold debits
//! − settled credit`; `NoCreditBalance` mirrors it with credits. An entry is
//! refused only when the exposure it leaves is positive **and** worse than
//! before, so a repayment into an account beyond a tightened limit passes.
//! The limit is the account's latest one, whatever the booking date. Hold
//! releases (Void, Expire) are never refused by a limit.

use std::collections::BTreeMap;

use super::context::EntryFacts;
use super::holds;
use super::state::{BalanceKey, StateView};
use super::writeset::{BalanceDelta, Outstanding, PostingRow};
use crate::error::{Rejection, ScaledAmount};
use crate::model::{Amount, Balance, BalanceLimit, EntryRecord, Layer};

use super::structural::index;

/// An entry's postings as rows, and the deltas of the balances it touches.
pub(crate) struct Booked<const P: u8> {
    pub(crate) rows: Vec<PostingRow<P>>,
    pub(crate) deltas: Vec<BalanceDelta<P>>,
}

/// One touched balance: before the entry, after it, and the difference.
#[derive(Clone, Copy)]
struct Moved<const P: u8> {
    before: Balance<P>,
    after: Balance<P>,
    delta: Balance<P>,
}

/// The limits stage: 500 for any gross figure that would overflow, then 501
/// in (account, currency) order.
pub(crate) fn check<const P: u8>(
    view: &impl StateView<P>,
    facts: &EntryFacts<P>,
    entry: &EntryRecord<P>,
    outstanding: &[Outstanding<P>],
) -> Result<Booked<P>, Rejection> {
    let parts = entry.parts();
    let mut moved: BTreeMap<BalanceKey, Moved<P>> = BTreeMap::new();
    let mut rows = Vec::with_capacity(parts.postings.len());
    for (posting, leg) in parts.postings.iter().enumerate() {
        let key = BalanceKey {
            handle: leg.handle,
            currency: leg.currency,
            layer: leg.layer,
        };
        let balance = facts.balances.get(&key).copied().unwrap_or(Balance::ZERO);
        let slot = moved.entry(key).or_insert(Moved {
            before: balance,
            after: balance,
            delta: Balance::ZERO,
        });
        let overflow = || overflow(facts, key);
        slot.after = slot
            .after
            .post(leg.side, leg.amount)
            .map_err(|_| overflow())?;
        slot.delta = slot
            .delta
            .post(leg.side, leg.amount)
            .map_err(|_| overflow())?;
        rows.push(PostingRow {
            index: index(posting),
            key,
            side: leg.side,
            amount: leg.amount,
            booking: parts.booking_date,
            value: parts.value_date,
            dimensions: leg.dimensions.clone(),
            balance_after: slot.after,
        });
    }
    if !holds::is_release(parts.hold.as_ref()) {
        for (&(handle, currency), &before_holds) in &facts.outstanding {
            // Outstanding amounts are gathered for limited accounts only.
            let Some(account) = facts.account(handle) else {
                continue;
            };
            let limit = account.latest.limit.value;
            let key = BalanceKey {
                handle,
                currency,
                layer: Layer::Settled,
            };
            let settled = moved.get(&key).copied().unwrap_or_else(|| {
                let balance = facts.balances.get(&key).copied().unwrap_or(Balance::ZERO);
                Moved {
                    before: balance,
                    after: balance,
                    delta: Balance::ZERO,
                }
            });
            let after_holds = holds::outstanding_after(view, outstanding, handle, currency);
            let overflow = || overflow(facts, key);
            let before = exposure(limit, settled.before, before_holds).ok_or_else(overflow)?;
            let after = exposure(limit, settled.after, after_holds).ok_or_else(overflow)?;
            if after.is_positive() && after.exceeds(&before) {
                return Err(Rejection::LimitExceeded {
                    account: account.path().clone(),
                    currency,
                    limit,
                    shortfall: after.shortfall::<P>(),
                });
            }
        }
    }
    let deltas = moved
        .into_iter()
        .map(|(key, moved)| BalanceDelta {
            key,
            delta: moved.delta,
        })
        .collect();
    Ok(Booked { rows, deltas })
}

/// How far a balance and its outstanding holds stand on the side a limit
/// forbids, kept as its two parts — the net on that side (in
/// `[−MAX, MAX]`) and the holds on it (in `[0, MAX]`) — because their sum
/// may leave the amount range. Comparisons are exact over the whole
/// `[−MAX, 2·MAX]`, so an exposure beyond the amount range never refuses an
/// entry that lowers it.
#[derive(Debug, Clone, Copy)]
struct Exposure {
    /// The net balance on the forbidden side, in minor units.
    net: i128,
    /// The outstanding holds on that side, in minor units.
    holds: i128,
}

impl Exposure {
    /// Whether the exposure is above zero: a breach.
    const fn is_positive(&self) -> bool {
        // With `net ≤ 0` the sum cannot overflow (`holds ≥ 0`).
        self.net > 0 || self.net.saturating_add(self.holds) > 0
    }

    /// Whether it is larger than `other`: `net − other.net > other.holds −
    /// holds`, the right side in `[−MAX, MAX]`; when the left side overflows
    /// its sign alone decides, since it then exceeds any right side.
    fn exceeds(&self, other: &Self) -> bool {
        let holds = other.holds.saturating_sub(self.holds);
        match self.net.checked_sub(other.net) {
            Some(net) => net > holds,
            None => self.net > other.net,
        }
    }

    /// The breach as a reported amount (saturated beyond the range).
    fn shortfall<const P: u8>(&self) -> ScaledAmount {
        ScaledAmount {
            minor: self.net.saturating_add(self.holds),
            scale: P,
        }
    }
}

/// The [`Exposure`] of a balance and its outstanding holds under `limit`;
/// `None` when the net leaves the amount range (it cannot: both sides lie
/// in `[0, MAX]`).
fn exposure<const P: u8>(
    limit: BalanceLimit,
    settled: Balance<P>,
    (hold_debits, hold_credits): (Amount<P>, Amount<P>),
) -> Option<Exposure> {
    let net = settled.debit().checked_sub(settled.credit()).ok()?;
    Some(match limit {
        BalanceLimit::Unlimited => Exposure { net: 0, holds: 0 },
        BalanceLimit::NoDebitBalance => Exposure {
            net: net.to_minor(),
            holds: hold_debits.to_minor(),
        },
        BalanceLimit::NoCreditBalance => Exposure {
            net: net.negate().to_minor(),
            holds: hold_credits.to_minor(),
        },
    })
}

fn overflow<const P: u8>(facts: &EntryFacts<P>, key: BalanceKey) -> Rejection {
    Rejection::BalanceOverflow {
        account: facts
            .account(key.handle)
            .map(|account| account.path().clone()),
        currency: key.currency,
        layer: key.layer,
    }
}
