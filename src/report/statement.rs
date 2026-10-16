//! Account statements: an opening balance, the window's lines in date order
//! with a running balance, and a closing balance —
//! `opening + Σ lines = closing` on either date basis.

use core::fmt;

use time::Date;

use super::balances::{inconsistent, write_table};
use super::query::{DateBasis, Selection};
use crate::error::IntegrityError;
use crate::model::{
    AccountHandle, AccountPath, Amount, Balance, Currency, EntryId, Layer, Side, pad,
};
use crate::store::PostingLine;

/// One posting of a [`Statement`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct StatementLine<const P: u8 = 2> {
    /// The entry's position in the log.
    pub position: u64,
    /// The entry.
    pub entry: EntryId,
    /// The posting's index in the entry.
    pub posting: u16,
    /// The booking date.
    pub booking_date: Date,
    /// The value date.
    pub value_date: Date,
    /// The side.
    pub side: Side,
    /// The amount (positive).
    pub amount: Amount<P>,
    /// The balance after this line: the opening plus every line so far.
    pub running: Balance<P>,
}

impl<const P: u8> fmt::Display for StatementLine<P> {
    /// `2026-03-15 7/0 debit 100.00, balance 1100.00` (booking date,
    /// position/posting, side, amount, running net), honouring width, fill
    /// and alignment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        pad(
            f,
            &format!(
                "{} {}/{} {} {}, balance {}",
                self.booking_date,
                self.position,
                self.posting,
                self.side,
                self.amount,
                self.running.net()
            ),
        )
    }
}

/// An account's statement in one currency and layer over a window.
///
/// The lines are the selected postings dated within the window, ordered by
/// (basis date, position, posting index) — a backdated entry appears on its
/// date, not where it was recorded — and the opening balance holds the
/// selected postings dated before the window (zero for a window without a
/// start). A quiet window reads as unchanged: no lines and
/// `closing == opening`.
///
/// ```
/// use doubleentry::prelude::*;
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
/// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
/// books.define_months(2026, 2..=3)?;
/// for (key, day, amount) in [("feb", date!("2026-02-10"), amount!("1000.00")), ("mar", date!("2026-03-05"), amount!("100.00"))] {
///     books.record(
///         Entry::new(key, day).debit("Assets:Bank", amount, EUR).credit("Income:Sales", amount, EUR),
///     )?;
/// }
/// let march = books.statement("Assets:Bank", EUR, &BalanceQuery::period("2026-03"))?;
/// assert_eq!(march.opening.net(), amount!("1000.00"));
/// assert_eq!(march.lines.len(), 1);
/// assert_eq!(march.closing.net(), amount!("1100.00"));
/// print!("{march}");
/// # Ok::<(), Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Statement<const P: u8 = 2> {
    /// The account.
    pub account: AccountHandle,
    /// Its path.
    pub path: AccountPath,
    /// The currency.
    pub currency: Currency,
    /// The layer.
    pub layer: Layer,
    /// The date the window, the order and the opening read.
    pub basis: DateBasis,
    /// The window's first day (`None`: from the beginning).
    pub start: Option<Date>,
    /// The window's last day (`None`: to the head).
    pub end: Option<Date>,
    /// The selected postings dated before the window.
    pub opening: Balance<P>,
    /// The selected postings dated within the window, in date order.
    pub lines: Vec<StatementLine<P>>,
    /// `opening` plus every line.
    pub closing: Balance<P>,
    /// The currency's declared exponent at the head read: the decimal places
    /// the report prints amounts with.
    pub exponent: u8,
}

impl<const P: u8> fmt::Display for Statement<P> {
    /// A heading, the opening, one row per line and the closing, columns
    /// aligned (width is ignored), amounts at the currency's exponent.
    ///
    /// ```text
    /// Assets:Bank EUR, settled, by booking date, 2026-03-01 to 2026-03-31
    /// opening                                  1000.00
    /// 2026-03-05  5/0  debit   100.00          1100.00
    /// closing                                  1100.00
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let from = self
            .start
            .map_or_else(|| String::from("the beginning"), |d| d.to_string());
        let to = self
            .end
            .map_or_else(|| String::from("the head"), |d| d.to_string());
        writeln!(
            f,
            "{} {}, {}, by {}, {from} to {to}",
            self.path, self.currency, self.layer, self.basis
        )?;
        let blank = String::new;
        let mut table = vec![[
            String::from("opening"),
            blank(),
            blank(),
            blank(),
            self.opening.net().display_at(self.exponent).to_string(),
        ]];
        for line in &self.lines {
            let date = match self.basis {
                DateBasis::Booking => line.booking_date,
                DateBasis::Value => line.value_date,
            };
            table.push([
                date.to_string(),
                format!("{}/{}", line.position, line.posting),
                line.side.to_string(),
                line.amount.display_at(self.exponent).to_string(),
                line.running.net().display_at(self.exponent).to_string(),
            ]);
        }
        table.push([
            String::from("closing"),
            blank(),
            blank(),
            blank(),
            self.closing.net().display_at(self.exponent).to_string(),
        ]);
        write_table(f, &table, &[false, true, false, true, true])
    }
}

/// Folds one key's posting rows, in log order, into a statement.
#[derive(Debug)]
pub(crate) struct StatementFold<'a, const P: u8> {
    selection: &'a Selection,
    opening: Balance<P>,
    lines: Vec<(Date, PostingLine<P>)>,
}

impl<'a, const P: u8> StatementFold<'a, P> {
    /// An empty fold.
    pub(crate) const fn new(selection: &'a Selection) -> Self {
        Self {
            selection,
            opening: Balance::ZERO,
            lines: Vec::new(),
        }
    }

    /// Counts `line` into the opening, keeps it as a line, or leaves it.
    pub(crate) fn feed(&mut self, line: PostingLine<P>) -> Result<(), IntegrityError> {
        if !self.selection.filters(&line) {
            return Ok(());
        }
        let date = self.selection.date_of(&line);
        if self.selection.start().is_some_and(|start| date < start) {
            self.opening = self
                .opening
                .post(line.side, line.amount)
                .map_err(|_| inconsistent(line.position))?;
        } else if self.selection.holds(date) {
            self.lines.push((date, line));
        }
        Ok(())
    }

    /// The statement of `account` (`path`) in `currency`.
    pub(crate) fn finish(
        self,
        account: AccountHandle,
        path: AccountPath,
        currency: Currency,
        exponent: u8,
    ) -> Result<Statement<P>, IntegrityError> {
        let mut lines = self.lines;
        lines.sort_by_key(|(date, line)| (*date, line.position, line.index));
        let mut running = self.opening;
        let lines = lines
            .into_iter()
            .map(|(_, line)| {
                running = running
                    .post(line.side, line.amount)
                    .map_err(|_| inconsistent(line.position))?;
                Ok(StatementLine {
                    position: line.position,
                    entry: line.entry,
                    posting: line.index,
                    booking_date: line.booking,
                    value_date: line.value,
                    side: line.side,
                    amount: line.amount,
                    running,
                })
            })
            .collect::<Result<Vec<_>, IntegrityError>>()?;
        Ok(Statement {
            account,
            path,
            currency,
            exponent,
            layer: self.selection.layer(),
            basis: self.selection.basis(),
            start: self.selection.start(),
            end: self.selection.end(),
            opening: self.opening,
            lines,
            closing: running,
        })
    }
}
