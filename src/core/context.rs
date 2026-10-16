//! Stage 3 for entries: the ledger's state on the booking date.
//!
//! The facts an entry needs are gathered in one pass before any is judged
//! ([`gather`]): a store answering through a [`Snapshot`](super::Snapshot)
//! then learns every independent read in the same planning round.

use std::collections::BTreeMap;

use time::Date;

use super::state::{AccountView, BalanceKey, EntryView, PeriodView, StateView, Terms};
use crate::error::Rejection;
use crate::model::{
    AccountHandle, AccountRef, Amount, Balance, BalanceLimit, Currency, Dimensions, EntryId, Layer,
    LedgerPolicy, PostingRecord,
};

use super::calendar;
use super::structural::index;

/// One posting of an entry as the context stage reads it.
#[derive(Debug, Clone)]
pub(crate) struct Leg<'a, const P: u8> {
    pub(crate) account: AccountRef,
    pub(crate) amount: Amount<P>,
    pub(crate) currency: Currency,
    pub(crate) layer: Layer,
    pub(crate) dimensions: &'a Dimensions,
}

/// A record's postings as legs, accounts by handle.
pub(crate) fn legs<const P: u8>(postings: &[PostingRecord<P>]) -> Vec<Leg<'_, P>> {
    postings
        .iter()
        .map(|posting| Leg {
            account: AccountRef::Handle(posting.handle),
            amount: posting.amount,
            currency: posting.currency,
            layer: posting.layer,
            dimensions: &posting.dimensions,
        })
        .collect()
}

/// A reversal's target, as found.
#[derive(Debug, Clone)]
pub(crate) struct Target<const P: u8> {
    pub(crate) id: EntryId,
    pub(crate) entry: Option<EntryView<P>>,
    pub(crate) reversed_by: Option<EntryId>,
}

/// Every fact the context, reference and limit stages of one entry read.
#[derive(Debug, Clone)]
pub(crate) struct EntryFacts<const P: u8> {
    period: Option<PeriodView<P>>,
    /// The account of each leg, in leg order.
    pub(crate) accounts: Vec<Option<AccountView>>,
    terms: BTreeMap<AccountHandle, Option<Terms>>,
    currencies: BTreeMap<Currency, Option<u8>>,
    policy: LedgerPolicy,
    pub(crate) target: Option<Target<P>>,
    pub(crate) balances: BTreeMap<BalanceKey, Balance<P>>,
    /// Outstanding hold (debits, credits) of each limited (account,
    /// currency).
    pub(crate) outstanding: BTreeMap<(AccountHandle, Currency), (Amount<P>, Amount<P>)>,
}

impl<const P: u8> EntryFacts<P> {
    /// The resolved account with a handle.
    pub(crate) fn account(&self, handle: AccountHandle) -> Option<&AccountView> {
        self.accounts
            .iter()
            .flatten()
            .find(|account| account.handle() == handle)
    }
}

/// Reads everything an entry booked on `booking` with these legs can need.
pub(crate) fn gather<const P: u8>(
    view: &impl StateView<P>,
    booking: Date,
    legs: &[Leg<'_, P>],
    reverses: Option<EntryId>,
) -> EntryFacts<P> {
    let accounts: Vec<Option<AccountView>> = legs
        .iter()
        .map(|leg| match &leg.account {
            AccountRef::Path(path) => view.account_by_path(path),
            AccountRef::Handle(handle) => view.account(*handle),
        })
        .collect();
    let mut terms = BTreeMap::new();
    let mut balances = BTreeMap::new();
    let mut outstanding = BTreeMap::new();
    for (leg, account) in legs.iter().zip(&accounts) {
        let Some(account) = account else { continue };
        let handle = account.handle();
        terms
            .entry(handle)
            .or_insert_with(|| view.account_terms(handle, booking));
        let mut keys = vec![BalanceKey {
            handle,
            currency: leg.currency,
            layer: leg.layer,
        }];
        if account.latest.limit.value != BalanceLimit::Unlimited {
            keys.push(BalanceKey {
                handle,
                currency: leg.currency,
                layer: Layer::Settled,
            });
            outstanding
                .entry((handle, leg.currency))
                .or_insert_with(|| {
                    (
                        view.outstanding_hold_debits(handle, leg.currency),
                        view.outstanding_hold_credits(handle, leg.currency),
                    )
                });
        }
        for key in keys {
            balances.entry(key).or_insert_with(|| view.balance(key));
        }
    }
    let mut currencies = BTreeMap::new();
    for leg in legs {
        currencies
            .entry(leg.currency)
            .or_insert_with(|| view.currency(leg.currency, booking));
    }
    EntryFacts {
        period: view.period_at(booking),
        accounts,
        terms,
        currencies,
        policy: view.policy(booking),
        target: reverses.map(|id| Target {
            id,
            entry: view.entry(id),
            reversed_by: view.reversed_by(id),
        }),
        balances,
        outstanding,
    }
}

/// The context stage, checks in code order: 300, 301, 302, 303, 304, 305,
/// 306, 307, 308.
pub(crate) fn check<const P: u8>(
    facts: &EntryFacts<P>,
    booking: Date,
    value: Date,
    legs: &[Leg<'_, P>],
) -> Result<(), Rejection> {
    calendar::open_on(facts.period.as_ref(), booking)?;
    let mut accounts = Vec::with_capacity(legs.len());
    for (leg, account) in legs.iter().zip(&facts.accounts) {
        accounts.push(account.as_ref().ok_or_else(|| Rejection::UnknownAccount {
            account: leg.account.clone(),
        })?);
    }
    if let Some(account) = accounts.iter().find(|account| !account.is_leaf) {
        return Err(Rejection::NotALeaf {
            account: account.path().clone(),
        });
    }
    for account in &accounts {
        let open = facts
            .terms
            .get(&account.handle())
            .and_then(Option::as_ref)
            .is_some_and(|terms| terms.window.contains(booking));
        if !open {
            return Err(Rejection::AccountNotOpen {
                account: account.path().clone(),
                date: booking,
            });
        }
    }
    let mut exponents = Vec::with_capacity(legs.len());
    for leg in legs {
        let exponent = facts.currencies.get(&leg.currency).copied().flatten();
        exponents.push(exponent.ok_or(Rejection::UndeclaredCurrency {
            currency: leg.currency,
            date: booking,
        })?);
    }
    for (posting, (leg, exponent)) in legs.iter().zip(exponents).enumerate() {
        if !leg.amount.fits_exponent(exponent) {
            return Err(Rejection::ExcessPrecision {
                posting: index(posting),
                currency: leg.currency,
                exponent,
            });
        }
    }
    for (posting, leg) in legs.iter().enumerate() {
        if let Some(axis) = facts
            .policy
            .required_dimensions()
            .iter()
            .find(|axis| leg.dimensions.get(axis).is_none())
        {
            return Err(Rejection::MissingDimension {
                posting: index(posting),
                axis: axis.clone(),
            });
        }
    }
    if let Some(max) = facts.policy.max_value_date_drift_days() {
        let days = value.to_julian_day().abs_diff(booking.to_julian_day());
        if days > max {
            return Err(Rejection::ValueDateDrift { days, max });
        }
    }
    Ok(())
}
