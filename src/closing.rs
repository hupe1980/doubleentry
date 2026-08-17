//! Year-end closing entries.
//!
//! Closing zeroes the accounts whose balances belong to one period only —
//! typically income and expense — by posting their opposite and moving the net
//! to equity. It is bookkeeping mechanics rather than reporting, so it belongs
//! in the engine; *which* equity account receives the result is a chart-of-
//! accounts decision and stays with the caller.
//!
//! The result is a set of postings, not an entry. They still go through
//! validation like anything else, which is where the period, the accounts, and
//! the balance invariant are checked.

use crate::account::{AccountId, AccountKind, AccountRegistry};
use crate::balance::TrialBalance;
use crate::money::{Amount, Currency, MoneyError};
use crate::posting::{Direction, Layer, Posting};

/// Failure generating closing entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClosingError {
    /// The equity account is not registered.
    #[error("equity account {account} is not registered")]
    UnknownEquityAccount {
        /// The account named.
        account: AccountId,
    },
    /// The equity account was itself selected for closing, which would make the
    /// result depend on the order the accounts were visited.
    #[error("equity account {account} is itself in scope for closing")]
    EquityInScope {
        /// The account named.
        account: AccountId,
    },
    /// Arithmetic overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// Builds the postings that close every account of the given kinds into
/// `equity`.
///
/// One posting per account with a non-zero net, on the side that flattens it,
/// followed by one balancing posting on `equity` per currency. Accounts that
/// already net to zero are skipped: a posting of zero carries no information and
/// would be rejected by validation anyway.
///
/// The postings balance per currency by construction, so adding them to a draft
/// and sealing it succeeds unless something else about the draft is wrong.
///
/// Returns an empty vector when there is nothing to close, which is what a
/// second run over an already-closed period produces.
///
/// # Scope is decided by [`AccountKind`], which is optional
///
/// Only accounts carrying a `kind` in `kinds` are closed. An account left
/// without one is **silently out of scope** — [`AccountKind`] is reporting
/// metadata the engine never requires, so there is nothing here to reject.
/// A revenue account registered without `.with_kind(AccountKind::Income)`
/// therefore survives the close carrying its balance, and the mistake surfaces
/// as an income statement that will not zero.
///
/// Classify every account you intend to close, and assert on the result: after
/// booking these postings, the net of each in-scope account is zero. That is one
/// line against a trial balance and it is the only check that distinguishes
/// "nothing to close" from "nothing was in scope".
pub fn closing_postings<const P: u8>(
    trial_balance: &TrialBalance<P>,
    accounts: &AccountRegistry,
    kinds: &[AccountKind],
    equity: AccountId,
    layer: Layer,
) -> Result<Vec<Posting<P>>, ClosingError> {
    if accounts.get(equity).is_none() {
        return Err(ClosingError::UnknownEquityAccount { account: equity });
    }
    if let Some(kind) = accounts.get(equity).and_then(|a| a.kind)
        && kinds.contains(&kind)
    {
        return Err(ClosingError::EquityInScope { account: equity });
    }

    // Accumulated per currency, signed with debit positive. Ordered so the
    // equity legs come out in a reproducible sequence.
    let mut offsets: std::collections::BTreeMap<Currency, Amount<P>> =
        std::collections::BTreeMap::new();
    let mut postings = Vec::new();

    for (key, balance) in trial_balance.iter() {
        if key.layer != layer || key.account == equity {
            continue;
        }
        let Some(account) = accounts.get(key.account) else {
            continue;
        };
        let Some(kind) = account.kind else {
            continue;
        };
        if !kinds.contains(&kind) {
            continue;
        }

        let (side, magnitude) = balance.net()?;
        if magnitude.is_zero() {
            continue;
        }

        // Post the opposite side to flatten the account …
        postings.push(Posting {
            account: key.account,
            direction: side.inverse(),
            amount: magnitude,
            currency: key.currency,
            layer,
            // Closing flattens an account's whole balance across every
            // dimension it was sliced by, so there is no one slice to carry.
            dimensions: crate::dimensions::Dimensions::none(),
        });

        // … and carry the net to equity.
        let signed = match side {
            Direction::Debit => magnitude,
            Direction::Credit => magnitude.checked_neg()?,
        };
        let slot = offsets.entry(key.currency).or_insert(Amount::ZERO);
        *slot = slot.checked_add(signed)?;
    }

    for (currency, net) in offsets {
        if net.is_zero() {
            continue;
        }
        let (direction, magnitude) = if net.is_positive() {
            (Direction::Debit, net)
        } else {
            (Direction::Credit, net.checked_neg()?)
        };
        postings.push(Posting {
            account: equity,
            direction,
            amount: magnitude,
            currency,
            layer,
            dimensions: crate::dimensions::Dimensions::none(),
        });
    }

    Ok(postings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, AccountPath};
    use crate::balance::BalanceKey;
    use time::macros::date;

    type Eur = Amount<2>;

    struct Fixture {
        accounts: AccountRegistry,
        revenue: AccountId,
        expense: AccountId,
        equity: AccountId,
        cash: AccountId,
    }

    fn register(accounts: &mut AccountRegistry, path: &str, kind: AccountKind) -> AccountId {
        accounts
            .register(
                Account::new(
                    AccountPath::parse(path).expect("valid"),
                    date!(2026 - 01 - 01),
                )
                .with_kind(kind),
            )
            .expect("registers")
    }

    impl Fixture {
        fn new() -> Self {
            let mut accounts = AccountRegistry::new();
            let revenue = register(&mut accounts, "Income:Sales", AccountKind::Income);
            let expense = register(&mut accounts, "Expense:Rent", AccountKind::Expense);
            let equity = register(&mut accounts, "Equity:Retained", AccountKind::Equity);
            let cash = register(&mut accounts, "Assets:Cash", AccountKind::Asset);
            Self {
                accounts,
                revenue,
                expense,
                equity,
                cash,
            }
        }
    }

    fn tb(rows: &[(AccountId, Direction, i64)]) -> TrialBalance<2> {
        let mut tb = TrialBalance::<2>::new();
        for (account, direction, amount) in rows {
            tb.apply(&Posting::new(
                *account,
                *direction,
                Eur::from_minor(*amount),
                Currency::EUR,
            ))
            .expect("ok");
        }
        tb
    }

    fn balance_of(postings: &[Posting<2>]) -> (Eur, Eur) {
        let mut debits = Eur::ZERO;
        let mut credits = Eur::ZERO;
        for p in postings {
            match p.direction {
                Direction::Debit => debits = debits.checked_add(p.amount).expect("ok"),
                Direction::Credit => credits = credits.checked_add(p.amount).expect("ok"),
            }
        }
        (debits, credits)
    }

    #[test]
    fn closing_flattens_income_and_expense_into_equity() {
        let f = Fixture::new();
        // Revenue 1000 credit, rent 300 debit: profit of 700.
        let trial = tb(&[
            (f.revenue, Direction::Credit, 1000),
            (f.expense, Direction::Debit, 300),
        ]);

        let postings = closing_postings(
            &trial,
            &f.accounts,
            &[AccountKind::Income, AccountKind::Expense],
            f.equity,
            Layer::Settled,
        )
        .expect("closes");

        let (debits, credits) = balance_of(&postings);
        assert_eq!(debits, credits, "closing postings must balance");

        // Applying them flattens both accounts.
        let mut after = trial.clone();
        for p in &postings {
            after.apply(p).expect("ok");
        }
        for account in [f.revenue, f.expense] {
            let key = BalanceKey {
                account,
                currency: Currency::EUR,
                layer: Layer::Settled,
            };
            assert_eq!(
                after.get_or_zero(&key).signed_net().expect("ok"),
                Eur::ZERO,
                "account should be flat after closing"
            );
        }

        // The profit lands in equity, on the credit side.
        let equity_key = BalanceKey {
            account: f.equity,
            currency: Currency::EUR,
            layer: Layer::Settled,
        };
        assert_eq!(
            after.get_or_zero(&equity_key).net().expect("ok"),
            (Direction::Credit, Eur::from_minor(700))
        );
    }

    #[test]
    fn a_loss_lands_on_the_other_side() {
        let f = Fixture::new();
        let trial = tb(&[
            (f.revenue, Direction::Credit, 200),
            (f.expense, Direction::Debit, 500),
        ]);
        let postings = closing_postings(
            &trial,
            &f.accounts,
            &[AccountKind::Income, AccountKind::Expense],
            f.equity,
            Layer::Settled,
        )
        .expect("closes");

        let equity_leg = postings
            .iter()
            .find(|p| p.account == f.equity)
            .expect("equity leg present");
        assert_eq!(equity_leg.direction, Direction::Debit);
        assert_eq!(equity_leg.amount, Eur::from_minor(300));
    }

    #[test]
    fn balance_sheet_accounts_are_left_alone() {
        let f = Fixture::new();
        let trial = tb(&[
            (f.cash, Direction::Debit, 5000),
            (f.revenue, Direction::Credit, 5000),
        ]);
        let postings = closing_postings(
            &trial,
            &f.accounts,
            &[AccountKind::Income, AccountKind::Expense],
            f.equity,
            Layer::Settled,
        )
        .expect("closes");

        assert!(
            postings.iter().all(|p| p.account != f.cash),
            "assets carry forward and must not be closed"
        );
    }

    #[test]
    fn closing_twice_produces_nothing() {
        let f = Fixture::new();
        let trial = tb(&[
            (f.revenue, Direction::Credit, 1000),
            (f.expense, Direction::Debit, 300),
        ]);
        let kinds = [AccountKind::Income, AccountKind::Expense];

        let first =
            closing_postings(&trial, &f.accounts, &kinds, f.equity, Layer::Settled).expect("ok");
        let mut after = trial;
        for p in &first {
            after.apply(p).expect("ok");
        }

        let second =
            closing_postings(&after, &f.accounts, &kinds, f.equity, Layer::Settled).expect("ok");
        assert!(
            second.is_empty(),
            "an already-closed period closes to nothing"
        );
    }

    #[test]
    fn accounts_that_already_net_to_zero_are_skipped() {
        let f = Fixture::new();
        // Gross activity in both directions, zero net.
        let trial = tb(&[
            (f.revenue, Direction::Credit, 400),
            (f.revenue, Direction::Debit, 400),
        ]);
        let postings = closing_postings(
            &trial,
            &f.accounts,
            &[AccountKind::Income],
            f.equity,
            Layer::Settled,
        )
        .expect("ok");
        assert!(postings.is_empty());
    }

    #[test]
    fn each_currency_closes_independently() {
        let f = Fixture::new();
        let mut trial = TrialBalance::<2>::new();
        trial
            .apply(&Posting::new(
                f.revenue,
                Direction::Credit,
                Eur::from_minor(1000),
                Currency::EUR,
            ))
            .expect("ok");
        trial
            .apply(&Posting::new(
                f.revenue,
                Direction::Credit,
                Eur::from_minor(700),
                Currency::USD,
            ))
            .expect("ok");

        let postings = closing_postings(
            &trial,
            &f.accounts,
            &[AccountKind::Income],
            f.equity,
            Layer::Settled,
        )
        .expect("ok");

        let equity_legs: Vec<_> = postings.iter().filter(|p| p.account == f.equity).collect();
        assert_eq!(equity_legs.len(), 2, "one equity leg per currency");

        for currency in [Currency::EUR, Currency::USD] {
            let (d, c) = balance_of(
                &postings
                    .iter()
                    .filter(|p| p.currency == currency)
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            assert_eq!(d, c, "{currency} must balance on its own");
        }
    }

    #[test]
    fn accounts_without_a_kind_are_left_alone() {
        let mut accounts = AccountRegistry::new();
        let untyped = accounts
            .register_path("Misc:Unclassified", date!(2026 - 01 - 01))
            .expect("registers");
        let equity = register(&mut accounts, "Equity:Retained", AccountKind::Equity);

        let trial = tb(&[(untyped, Direction::Credit, 500)]);
        let postings = closing_postings(
            &trial,
            &accounts,
            &[AccountKind::Income, AccountKind::Expense],
            equity,
            Layer::Settled,
        )
        .expect("ok");
        assert!(postings.is_empty());
    }

    #[test]
    fn a_bad_equity_target_is_refused() {
        let f = Fixture::new();
        let trial = tb(&[(f.revenue, Direction::Credit, 100)]);

        assert!(matches!(
            closing_postings(
                &trial,
                &f.accounts,
                &[AccountKind::Income],
                AccountId::from_index(999),
                Layer::Settled,
            ),
            Err(ClosingError::UnknownEquityAccount { .. })
        ));

        // Closing equity into itself has no well-defined answer.
        assert!(matches!(
            closing_postings(
                &trial,
                &f.accounts,
                &[AccountKind::Income, AccountKind::Equity],
                f.equity,
                Layer::Settled,
            ),
            Err(ClosingError::EquityInScope { .. })
        ));
    }

    #[test]
    fn only_the_named_layer_is_closed() {
        let f = Fixture::new();
        let mut trial = TrialBalance::<2>::new();
        trial
            .apply(
                &Posting::new(
                    f.revenue,
                    Direction::Credit,
                    Eur::from_minor(900),
                    Currency::EUR,
                )
                .in_layer(Layer::Pending),
            )
            .expect("ok");

        let postings = closing_postings(
            &trial,
            &f.accounts,
            &[AccountKind::Income],
            f.equity,
            Layer::Settled,
        )
        .expect("ok");
        assert!(postings.is_empty(), "reservations are not earnings");
    }
}
