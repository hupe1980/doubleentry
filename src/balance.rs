//! Balances and trial balances.
//!
//! A balance carries the gross debit total and the gross credit total, not just
//! the net. A trial balance that reports only the net cannot answer how much
//! moved through an account, and the gross totals cannot be reconstructed from
//! the net afterwards.

use std::collections::BTreeMap;

use crate::account::AccountId;
use crate::money::{Amount, Currency, MoneyError};
use crate::posting::{Direction, Layer, Posting};

/// Gross debit and credit totals, and the net derived from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Balance<const P: u8> {
    /// Gross total of debit movements.
    pub debits: Amount<P>,
    /// Gross total of credit movements.
    pub credits: Amount<P>,
}

impl<const P: u8> Balance<P> {
    /// An empty balance.
    pub const ZERO: Self = Self {
        debits: Amount::ZERO,
        credits: Amount::ZERO,
    };

    /// Adds a movement on the given side.
    pub fn add(&mut self, direction: Direction, amount: Amount<P>) -> Result<(), MoneyError> {
        match direction {
            Direction::Debit => self.debits = self.debits.checked_add(amount)?,
            Direction::Credit => self.credits = self.credits.checked_add(amount)?,
        }
        Ok(())
    }

    /// The net balance and the side it falls on.
    ///
    /// A net of zero is reported as a debit of zero by convention; callers that
    /// care should test the magnitude rather than the side.
    pub fn net(&self) -> Result<(Direction, Amount<P>), MoneyError> {
        if self.debits >= self.credits {
            Ok((Direction::Debit, self.debits.checked_sub(self.credits)?))
        } else {
            Ok((Direction::Credit, self.credits.checked_sub(self.debits)?))
        }
    }

    /// The net as a signed amount, debit positive.
    pub fn signed_net(&self) -> Result<Amount<P>, MoneyError> {
        self.debits.checked_sub(self.credits)
    }

    /// True when debits and credits are equal.
    #[must_use]
    pub fn is_balanced(&self) -> bool {
        self.debits == self.credits
    }

    /// True when nothing has moved at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.debits.is_zero() && self.credits.is_zero()
    }

    /// Combines two balances.
    pub fn checked_add(&self, other: &Self) -> Result<Self, MoneyError> {
        Ok(Self {
            debits: self.debits.checked_add(other.debits)?,
            credits: self.credits.checked_add(other.credits)?,
        })
    }
}

/// The key a balance is reported against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceKey {
    /// The account.
    pub account: AccountId,
    /// The currency.
    pub currency: Currency,
    /// Settled or pending.
    pub layer: Layer,
}

/// Balances for a set of accounts, currencies, and layers.
///
/// Iteration order is deterministic, so any report or hash derived from a trial
/// balance is reproducible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrialBalance<const P: u8> {
    entries: BTreeMap<BalanceKey, Balance<P>>,
}

impl<const P: u8> TrialBalance<P> {
    /// Creates an empty trial balance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Accumulates a posting.
    pub fn apply(&mut self, posting: &Posting<P>) -> Result<(), MoneyError> {
        let key = BalanceKey {
            account: posting.account,
            currency: posting.currency,
            layer: posting.layer,
        };
        self.entries
            .entry(key)
            .or_default()
            .add(posting.direction, posting.amount)
    }

    /// Sets the balance for one key, replacing any existing value.
    ///
    /// Accumulating postings via [`TrialBalance::apply`] is the usual path; this
    /// is for reconstructing a balance set that was computed elsewhere.
    pub fn set(&mut self, key: BalanceKey, balance: Balance<P>) {
        self.entries.insert(key, balance);
    }

    /// Removes one key, returning the balance it held.
    ///
    /// For rebuilding a balance set, not for editing one: a trial balance with a
    /// key removed says the account never moved, which is a different claim from
    /// a balance of zero.
    pub fn remove(&mut self, key: &BalanceKey) -> Option<Balance<P>> {
        self.entries.remove(key)
    }

    /// The balance for one key, if anything has been posted to it.
    #[must_use]
    pub fn get(&self, key: &BalanceKey) -> Option<&Balance<P>> {
        self.entries.get(key)
    }

    /// The balance for one key, defaulting to zero.
    #[must_use]
    pub fn get_or_zero(&self, key: &BalanceKey) -> Balance<P> {
        self.entries.get(key).copied().unwrap_or(Balance::ZERO)
    }

    /// Every key and balance, in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&BalanceKey, &Balance<P>)> {
        self.entries.iter()
    }

    /// Number of populated keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing has been accumulated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Totals across all accounts for one currency and layer.
    ///
    /// In a consistent ledger the debit and credit totals are equal; that
    /// equality is the classic trial-balance check and is exposed here so a
    /// caller can assert it.
    pub fn totals(&self, currency: Currency, layer: Layer) -> Result<Balance<P>, MoneyError> {
        let mut acc = Balance::ZERO;
        for (key, balance) in &self.entries {
            if key.currency == currency && key.layer == layer {
                acc = acc.checked_add(balance)?;
            }
        }
        Ok(acc)
    }

    /// Every currency present, in deterministic order.
    #[must_use]
    pub fn currencies(&self) -> Vec<Currency> {
        let mut out: Vec<Currency> = self.entries.keys().map(|k| k.currency).collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Eur = Amount<2>;

    fn acct(i: u32) -> AccountId {
        AccountId::from_index(i)
    }

    #[test]
    fn tracks_gross_totals_separately_from_the_net() {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::from_minor(1000))
            .expect("no overflow");
        b.add(Direction::Credit, Eur::from_minor(1000))
            .expect("no overflow");

        // The net is zero, but a thousand moved in each direction.
        assert_eq!(b.signed_net().expect("no overflow"), Eur::ZERO);
        assert_eq!(b.debits, Eur::from_minor(1000));
        assert_eq!(b.credits, Eur::from_minor(1000));
        assert!(b.is_balanced());
        assert!(!b.is_empty());
    }

    #[test]
    fn distinguishes_no_activity_from_offsetting_activity() {
        let quiet = Balance::<2>::ZERO;
        let mut busy = Balance::<2>::ZERO;
        busy.add(Direction::Debit, Eur::from_minor(500))
            .expect("no overflow");
        busy.add(Direction::Credit, Eur::from_minor(500))
            .expect("no overflow");

        assert_eq!(
            quiet.signed_net().expect("ok"),
            busy.signed_net().expect("ok")
        );
        assert_ne!(quiet, busy);
        assert!(quiet.is_empty());
        assert!(!busy.is_empty());
    }

    #[test]
    fn net_reports_the_dominant_side() {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::from_minor(300)).expect("ok");
        b.add(Direction::Credit, Eur::from_minor(100)).expect("ok");
        assert_eq!(
            b.net().expect("ok"),
            (Direction::Debit, Eur::from_minor(200))
        );

        let mut c = Balance::<2>::ZERO;
        c.add(Direction::Credit, Eur::from_minor(300)).expect("ok");
        c.add(Direction::Debit, Eur::from_minor(100)).expect("ok");
        assert_eq!(
            c.net().expect("ok"),
            (Direction::Credit, Eur::from_minor(200))
        );
    }

    #[test]
    fn balance_addition_reports_overflow() {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::MAX).expect("ok");
        assert_eq!(
            b.add(Direction::Debit, Eur::from_minor(1)),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn trial_balance_separates_account_currency_and_layer() {
        let mut tb = TrialBalance::<2>::new();
        tb.apply(&Posting::debit(
            acct(0),
            Eur::from_minor(100),
            Currency::EUR,
        ))
        .expect("ok");
        tb.apply(&Posting::debit(
            acct(0),
            Eur::from_minor(100),
            Currency::USD,
        ))
        .expect("ok");
        tb.apply(
            &Posting::debit(acct(0), Eur::from_minor(100), Currency::EUR).in_layer(Layer::Pending),
        )
        .expect("ok");
        assert_eq!(tb.len(), 3);
        assert_eq!(tb.currencies(), vec![Currency::EUR, Currency::USD]);
    }

    #[test]
    fn trial_balance_totals_match_for_a_balanced_set() {
        let mut tb = TrialBalance::<2>::new();
        tb.apply(&Posting::debit(
            acct(0),
            Eur::from_minor(250),
            Currency::EUR,
        ))
        .expect("ok");
        tb.apply(&Posting::credit(
            acct(1),
            Eur::from_minor(250),
            Currency::EUR,
        ))
        .expect("ok");
        let totals = tb.totals(Currency::EUR, Layer::Settled).expect("ok");
        assert!(totals.is_balanced());
        assert_eq!(totals.debits, Eur::from_minor(250));
    }

    #[test]
    fn missing_keys_read_as_zero() {
        let tb = TrialBalance::<2>::new();
        let key = BalanceKey {
            account: acct(7),
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        assert_eq!(tb.get(&key), None);
        assert_eq!(tb.get_or_zero(&key), Balance::ZERO);
    }

    #[test]
    fn iteration_order_is_deterministic() {
        let build = || {
            let mut tb = TrialBalance::<2>::new();
            for i in [3u32, 1, 2, 0] {
                tb.apply(&Posting::debit(acct(i), Eur::from_minor(1), Currency::EUR))
                    .expect("ok");
            }
            tb.iter().map(|(k, _)| k.account).collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
        assert_eq!(
            build(),
            vec![acct(0), acct(1), acct(2), acct(3)],
            "keys must iterate in account order"
        );
    }
}
