//! Sealing a period: the seal record commits, per (currency, layer), to a
//! tree whose leaf `i` is account `i`'s gross balance through the period's
//! end.
//!
//! Later periods may already hold entries, so a leaf is the maintained
//! balance minus the postings booked after the end. Every earlier period is
//! sealed and nothing can be booked on or before the end afterwards, so the
//! difference is exactly the balance through the end. Totals are checked
//! sums of the leaves; across consecutive seals they never decrease.

use std::collections::BTreeMap;

use super::calendar::period_id;
use super::state::{BalanceKey, StateView};
use super::structural::record_error;
use super::writeset::{Done, IndexRow, Outcome, SealReceipt, WriteSet};
use crate::error::Rejection;
use crate::log::BalanceTree;
use crate::model::{
    AccountHandle, Amount, Balance, Currency, Encode, Layer, Record, Seal, SealParts, Side,
};

/// Seals a period: 107 → 311 → already sealed → 427 → 500, 111.
pub(crate) fn seal<const P: u8>(
    view: &impl StateView<P>,
    period: &str,
) -> Result<Outcome<P>, Rejection> {
    let id = period_id(period)?;
    let found = view
        .period(&id)
        .ok_or_else(|| Rejection::UnknownPeriod { period: id.clone() })?;
    if let Some(receipt) = found.sealed {
        return Ok(Outcome::Unchanged(Done::Sealed(receipt)));
    }
    if let Some(before) = found
        .start
        .previous_day()
        .and_then(|day| view.period_at(day))
        && before.sealed.is_none()
    {
        // Seals follow the calendar, so the earliest unsealed period is at
        // or before the predecessor.
        let earliest = view
            .first_unsealed()
            .map_or_else(|| before.id.clone(), |first| first.id);
        return Err(Rejection::PredecessorNotSealed {
            predecessor: before.id,
            earliest,
        });
    }
    let header = view.header();
    let mut leaves: BTreeMap<BalanceKey, Balance<P>> = view.all_balances().into_iter().collect();
    for movement in view.postings_after(found.end) {
        let key = movement.key;
        let leaf = leaves.get(&key).copied().unwrap_or(Balance::ZERO);
        let before =
            unpost(leaf, movement.side, movement.amount).ok_or_else(|| inconsistent(key))?;
        leaves.insert(key, before);
    }
    let mut groups: BTreeMap<(Currency, Layer), Vec<(AccountHandle, Balance<P>)>> = BTreeMap::new();
    for (key, leaf) in leaves {
        if !leaf.is_zero() {
            groups
                .entry((key.currency, key.layer))
                .or_default()
                .push((key.handle, leaf));
        }
    }
    let mut trees = Vec::with_capacity(groups.len());
    for (&(currency, layer), leaves) in &groups {
        let overflow = || Rejection::BalanceOverflow {
            account: None,
            currency,
            layer,
        };
        let mut tree = BalanceTree::new(currency, layer, header.accounts);
        for &(handle, leaf) in leaves {
            tree.set(handle, leaf).map_err(|_| overflow())?;
        }
        trees.push(tree.head().map_err(|_| overflow())?);
    }
    let position = header.size;
    let parts = SealParts {
        period: id.clone(),
        start: found.start,
        end: found.end,
        through: position.saturating_sub(1),
        accounts: header.accounts,
        trees,
    };
    let bytes = parts.encoded_len().saturating_add(1);
    let seal = Seal::new(parts).map_err(|error| record_error(error, bytes))?;
    let receipt = SealReceipt {
        period: id.clone(),
        position,
        seal: seal.clone(),
    };
    let write =
        WriteSet::of(position, Record::Seal(seal), Done::Sealed(receipt)).with(IndexRow::Sealed {
            period: id.clone(),
            position,
        });
    Ok(Outcome::from(groups.into_iter().fold(
        write,
        |write, ((currency, layer), leaves)| {
            write.with(IndexRow::SealTree {
                period: id.clone(),
                currency,
                layer,
                leaves,
            })
        },
    )))
}

/// A balance without one of its postings; `None` when the posting is not
/// part of it.
fn unpost<const P: u8>(balance: Balance<P>, side: Side, amount: Amount<P>) -> Option<Balance<P>> {
    let (mut debit, mut credit) = (balance.debit(), balance.credit());
    match side {
        Side::Debit => debit = debit.checked_sub(amount).ok()?,
        Side::Credit => credit = credit.checked_sub(amount).ok()?,
    }
    Balance::from_parts(debit, credit, balance.postings().checked_sub(1)?).ok()
}

/// A view whose postings after the end do not add up to its balances (or
/// that names an account beyond its count) cannot be sealed; the refusal is
/// the overflow of the figures it would commit to.
const fn inconsistent(key: BalanceKey) -> Rejection {
    Rejection::BalanceOverflow {
        account: None,
        currency: key.currency,
        layer: key.layer,
    }
}
