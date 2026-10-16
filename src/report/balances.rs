//! Balances and the trial balance: a balance is the gross `(debit, credit,
//! postings)` of the postings a [`Selection`] counts; a trial balance is one
//! row per (account, currency) with at least one of them.

use core::fmt;
use std::collections::BTreeMap;

use super::accounts::AccountBook;
use super::query::{DateBasis, Selection};
use crate::core::BalanceKey;
use crate::error::{IntegrityError, IntegrityKind, OverflowContext, QueryError};
use crate::model::{AccountHandle, AccountKind, AccountPath, Balance, Currency, Label, Layer};
use crate::store::PostingLine;

/// A posting row set whose sum leaves the range a maintained balance stays
/// in: the store's rows are not the ledger's.
pub(crate) const fn inconsistent(position: u64) -> IntegrityError {
    IntegrityError::new(IntegrityKind::BalanceMismatch, Some(position))
}

/// Folds one key's posting rows, in log order, into the balance of the rows
/// a selection counts.
#[derive(Debug)]
pub(crate) struct BalanceFold<'a, const P: u8> {
    selection: &'a Selection,
    balance: Balance<P>,
}

impl<'a, const P: u8> BalanceFold<'a, P> {
    /// An empty fold.
    pub(crate) const fn new(selection: &'a Selection) -> Self {
        Self {
            selection,
            balance: Balance::ZERO,
        }
    }

    /// Counts `line` when the selection does.
    pub(crate) fn feed(&mut self, line: &PostingLine<P>) -> Result<(), IntegrityError> {
        if self.selection.selects(line) {
            self.balance = self
                .balance
                .post(line.side, line.amount)
                .map_err(|_| inconsistent(line.position))?;
        }
        Ok(())
    }

    /// The balance.
    pub(crate) const fn finish(self) -> Balance<P> {
        self.balance
    }
}

/// One (account, currency) of a [`TrialBalance`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct TrialBalanceRow<const P: u8 = 2> {
    /// The account.
    pub account: AccountHandle,
    /// Its path.
    pub path: AccountPath,
    /// Its kind.
    pub kind: AccountKind,
    /// Its class at the head the report was read at.
    pub class: Option<Label>,
    /// The currency.
    pub currency: Currency,
    /// The gross balance of the selected postings (at least one).
    pub balance: Balance<P>,
}

/// One row per (account, currency) with at least one selected posting, in
/// (path, currency) order, read in one layer and on one date basis.
///
/// Without a dimension filter every currency's debits equal its credits —
/// date and position windows select whole entries. A dimension filter may
/// split an entry, and [`is_balanced`](Self::is_balanced) then says so
/// truthfully: a filtered trial balance is a slice.
///
/// ```
/// use doubleentry::prelude::*;
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
/// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
/// books.define_months(2026, 3..=3)?;
/// books.record(
///     Entry::new("sale-1", date!("2026-03-15"))
///         .debit("Assets:Bank", amount!("1190.00"), EUR)
///         .credit("Income:Sales", amount!("1190.00"), EUR),
/// )?;
/// let trial = books.trial_balance(&BalanceQuery::period("2026-03"))?;
/// assert!(trial.is_balanced());
/// assert_eq!(trial.rows().len(), 2);
/// assert_eq!(trial.totals(EUR)?.debit(), amount!("1190.00"));
/// print!("{trial}");
/// # Ok::<(), Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrialBalance<const P: u8 = 2> {
    layer: Layer,
    basis: DateBasis,
    filtered: bool,
    rows: Vec<TrialBalanceRow<P>>,
    /// Each declared currency's exponent at the head read: the decimals its
    /// amounts are printed with.
    exponents: BTreeMap<Currency, u8>,
    /// Every account registered at the head read, for a rollup's nodes.
    accounts: BTreeMap<AccountPath, (AccountHandle, AccountKind)>,
}

impl<const P: u8> TrialBalance<P> {
    /// The trial balance of `balances` (the selection's balance of each key
    /// with postings), with the master data of `accounts`.
    pub(crate) fn of(
        selection: &Selection,
        balances: impl IntoIterator<Item = (BalanceKey, Balance<P>)>,
        accounts: &AccountBook,
        exponents: BTreeMap<Currency, u8>,
    ) -> Result<Self, IntegrityError> {
        let mut rows = BTreeMap::new();
        for (key, balance) in balances {
            if key.layer != selection.layer() || balance.is_zero() {
                continue;
            }
            let account = accounts
                .get(key.handle)
                .ok_or_else(|| IntegrityError::new(IntegrityKind::MissingRecord, None))?;
            rows.insert(
                (account.path().clone(), key.currency),
                TrialBalanceRow {
                    account: key.handle,
                    path: account.path().clone(),
                    kind: account.kind(),
                    class: account.class().cloned(),
                    currency: key.currency,
                    balance,
                },
            );
        }
        Ok(Self {
            layer: selection.layer(),
            basis: selection.basis(),
            filtered: selection.is_filtered(),
            rows: rows.into_values().collect(),
            exponents,
            accounts: accounts.by_path(),
        })
    }

    /// The rows, by (path, currency).
    #[must_use]
    pub fn rows(&self) -> &[TrialBalanceRow<P>] {
        &self.rows
    }

    /// The row of `path` in `currency`, if it has selected postings.
    #[must_use]
    pub fn get(&self, path: &AccountPath, currency: Currency) -> Option<&TrialBalanceRow<P>> {
        self.rows
            .iter()
            .find(|row| row.path == *path && row.currency == currency)
    }

    /// The layer read.
    #[must_use]
    pub const fn layer(&self) -> Layer {
        self.layer
    }

    /// The date basis of its window.
    #[must_use]
    pub const fn basis(&self) -> DateBasis {
        self.basis
    }

    /// Whether a dimension clause narrowed it (it then need not balance).
    #[must_use]
    pub const fn is_filtered(&self) -> bool {
        self.filtered
    }

    /// The declared exponent of `currency` at the head read — the decimal
    /// places its amounts are printed with; `None` when undeclared.
    #[must_use]
    pub fn exponent(&self, currency: Currency) -> Option<u8> {
        self.exponents.get(&currency).copied()
    }

    /// `amount` at `currency`'s exponent.
    fn show(&self, currency: Currency, amount: crate::model::Amount<P>) -> String {
        amount
            .display_at(self.exponent(currency).unwrap_or(P))
            .to_string()
    }

    /// The currencies with rows, ascending.
    pub fn currencies(&self) -> impl Iterator<Item = Currency> + '_ {
        let mut currencies: Vec<Currency> = self.rows.iter().map(|row| row.currency).collect();
        currencies.sort_unstable();
        currencies.dedup();
        currencies.into_iter()
    }

    /// The component-wise sum of every row in `currency` (zero without
    /// rows).
    ///
    /// # Errors
    ///
    /// [`QueryError::Overflow`] when a sum does not fit an amount.
    pub fn totals(&self, currency: Currency) -> Result<Balance<P>, QueryError> {
        self.rows
            .iter()
            .filter(|row| row.currency == currency)
            .try_fold(Balance::ZERO, |total, row| total.checked_add(&row.balance))
            .map_err(|_| QueryError::Overflow {
                context: OverflowContext::Totals(currency),
            })
    }

    /// Whether every currency's debits equal its credits.
    #[must_use]
    pub fn is_balanced(&self) -> bool {
        self.currencies().all(|currency| {
            self.totals(currency)
                .is_ok_and(|total| total.debit() == total.credit())
        })
    }

    /// Every account registered at the head the report was read at, by
    /// path.
    pub(crate) const fn accounts(&self) -> &BTreeMap<AccountPath, (AccountHandle, AccountKind)> {
        &self.accounts
    }
}

impl<const P: u8> fmt::Display for TrialBalance<P> {
    /// A table of the rows — account, currency, debit, credit — and a total
    /// per currency, its columns aligned (width is ignored: the report
    /// aligns its own columns). Amounts are written at their currency's
    /// declared exponent (JPY `15000`, EUR `15000.00`).
    ///
    /// ```text
    /// trial balance, settled, by booking date
    /// account       currency    debit   credit
    /// Assets:Bank   EUR       1190.00     0.00
    /// Income:Sales  EUR          0.00  1190.00
    /// total         EUR       1190.00  1190.00
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut table: Vec<[String; 4]> = vec![[
            String::from("account"),
            String::from("currency"),
            String::from("debit"),
            String::from("credit"),
        ]];
        for row in &self.rows {
            table.push([
                row.path.to_string(),
                row.currency.to_string(),
                self.show(row.currency, row.balance.debit()),
                self.show(row.currency, row.balance.credit()),
            ]);
        }
        for currency in self.currencies() {
            let (debit, credit) = match self.totals(currency) {
                Ok(total) => (
                    self.show(currency, total.debit()),
                    self.show(currency, total.credit()),
                ),
                Err(_) => (String::from("overflow"), String::from("overflow")),
            };
            table.push([String::from("total"), currency.to_string(), debit, credit]);
        }
        let filtered = if self.filtered { ", filtered" } else { "" };
        writeln!(
            f,
            "trial balance, {}, by {}{filtered}",
            self.layer, self.basis
        )?;
        write_table(f, &table, &[false, false, true, true])
    }
}

/// Writes `rows` as aligned columns, two spaces apart: right-aligned where
/// `right` says so, left-aligned otherwise; no trailing spaces.
pub(crate) fn write_table<const N: usize>(
    f: &mut fmt::Formatter<'_>,
    rows: &[[String; N]],
    right: &[bool; N],
) -> fmt::Result {
    let mut widths = [0usize; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in rows {
        let mut line = String::new();
        for (column, ((cell, width), right)) in row.iter().zip(widths).zip(right).enumerate() {
            if column > 0 {
                line.push_str("  ");
            }
            let missing = width.saturating_sub(cell.chars().count());
            if *right {
                line.extend(core::iter::repeat_n(' ', missing));
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.extend(core::iter::repeat_n(' ', missing));
            }
        }
        writeln!(f, "{}", line.trim_end())?;
    }
    Ok(())
}
