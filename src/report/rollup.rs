//! Hierarchical reports: a trial balance of one currency and layer folded up
//! the account tree, each node holding everything beneath it.
//!
//! - **Grouping nodes are inferred.** `Assets` heads `Assets:Bank` whether
//!   or not anybody registered it; such a node has no handle and no kind,
//!   and owns nothing.
//! - **One currency per report.** Take one rollup per
//!   [`TrialBalance::currencies`] item.
//! - **The roots sum to the trial balance**, never the nodes: a parent's
//!   subtree already holds its children.

use core::fmt;
use std::collections::BTreeMap;

use super::balances::TrialBalance;
use crate::error::{OverflowContext, QueryError};
use crate::model::{AccountHandle, AccountKind, AccountPath, Balance, Currency, Layer};

/// One node of a [`Rollup`]: what was posted to its path itself, and to
/// everything beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct RollupNode<const P: u8 = 2> {
    /// The node's path.
    pub path: AccountPath,
    /// The account registered at the path; `None` for an inferred grouping
    /// node.
    pub account: Option<AccountHandle>,
    /// The registered account's kind; `None` for an inferred grouping node.
    pub kind: Option<AccountKind>,
    /// The postings to this path itself.
    pub own: Balance<P>,
    /// The postings to this path and every path beneath it.
    pub subtree: Balance<P>,
}

impl<const P: u8> RollupNode<P> {
    /// The number of segments: 1 for a root.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.depth()
    }

    /// Whether an account is registered at the path.
    #[must_use]
    pub const fn is_registered(&self) -> bool {
        self.account.is_some()
    }
}

/// A trial balance of one currency and layer folded up the account tree:
/// one node per path prefix, in pre-order (a parent before its children,
/// siblings by path).
///
/// ```
/// use doubleentry::prelude::*;
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// books.declare_currency(EUR, 2, date!("2026-01-01"))?;
/// for path in ["Assets:Bank:Main", "Assets:Cash"] {
///     books.open(path, AccountKind::Asset, date!("2026-01-01"))?;
/// }
/// books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
/// books.define_months(2026, 3..=3)?;
/// books.record(
///     Entry::new("sale-1", date!("2026-03-15"))
///         .debit("Assets:Bank:Main", amount!("1000.00"), EUR)
///         .debit("Assets:Cash", amount!("190.00"), EUR)
///         .credit("Income:Sales", amount!("1190.00"), EUR),
/// )?;
/// let sheet = books.rollup(EUR, &BalanceQuery::all())?;
/// let assets = sheet.get(&"Assets".parse()?).expect("inferred from its children");
/// assert!(!assets.is_registered());
/// assert_eq!(assets.subtree.net(), amount!("1190.00"));
/// assert_eq!(sheet.to_depth(1).total()?, sheet.total()?);
/// assert_eq!(
///     sheet.to_string(),
///     "Assets     1190.00\n  Bank     1000.00\n    Main   1000.00\n  Cash      190.00\nIncome    -1190.00\n  Sales   -1190.00\n",
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rollup<const P: u8 = 2> {
    currency: Currency,
    exponent: u8,
    layer: Layer,
    nodes: Vec<RollupNode<P>>,
}

impl<const P: u8> Rollup<P> {
    /// Folds the rows of `trial` in `currency` up the path tree, in the
    /// trial balance's layer.
    ///
    /// # Errors
    ///
    /// [`QueryError::Overflow`] when a subtree total does not fit an
    /// amount.
    pub fn of(trial: &TrialBalance<P>, currency: Currency) -> Result<Self, QueryError> {
        let overflow = |path: &AccountPath| QueryError::Overflow {
            context: OverflowContext::Subtree(path.clone()),
        };
        let mut own: BTreeMap<AccountPath, Balance<P>> = BTreeMap::new();
        for row in trial.rows().iter().filter(|row| row.currency == currency) {
            own.insert(row.path.clone(), row.balance);
        }
        let mut subtree: BTreeMap<AccountPath, Balance<P>> = BTreeMap::new();
        for (path, balance) in &own {
            let mut node = Some(path.clone());
            while let Some(path) = node {
                let total = subtree.entry(path.clone()).or_insert(Balance::ZERO);
                *total = total.checked_add(balance).map_err(|_| overflow(&path))?;
                node = path.parent();
            }
        }
        let nodes = subtree
            .into_iter()
            .map(|(path, subtree)| {
                let registered = trial.accounts().get(&path);
                RollupNode {
                    account: registered.map(|(handle, _)| *handle),
                    kind: registered.map(|(_, kind)| *kind),
                    own: own.get(&path).copied().unwrap_or(Balance::ZERO),
                    subtree,
                    path,
                }
            })
            .collect();
        Ok(Self {
            currency,
            exponent: trial.exponent(currency).unwrap_or(P),
            layer: trial.layer(),
            nodes,
        })
    }

    /// The currency.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// The layer.
    #[must_use]
    pub const fn layer(&self) -> Layer {
        self.layer
    }

    /// Every node, in pre-order.
    #[must_use]
    pub fn nodes(&self) -> &[RollupNode<P>] {
        &self.nodes
    }

    /// The node at `path`.
    #[must_use]
    pub fn get(&self, path: &AccountPath) -> Option<&RollupNode<P>> {
        self.nodes.iter().find(|node| node.path == *path)
    }

    /// The top-level nodes.
    pub fn roots(&self) -> impl Iterator<Item = &RollupNode<P>> {
        self.nodes.iter().filter(|node| node.depth() == 1)
    }

    /// The same report cut to `depth` levels: shorter, not different — every
    /// remaining node keeps its totals (`0` leaves nothing).
    #[must_use]
    pub fn to_depth(&self, depth: usize) -> Self {
        Self {
            currency: self.currency,
            exponent: self.exponent,
            layer: self.layer,
            nodes: self
                .nodes
                .iter()
                .filter(|node| node.depth() <= depth)
                .cloned()
                .collect(),
        }
    }

    /// The sum of the roots' subtrees: the trial balance's totals in this
    /// currency.
    ///
    /// # Errors
    ///
    /// [`QueryError::Overflow`] when the sum does not fit an amount.
    pub fn total(&self) -> Result<Balance<P>, QueryError> {
        self.roots().try_fold(Balance::ZERO, |total, root| {
            total
                .checked_add(&root.subtree)
                .map_err(|_| QueryError::Overflow {
                    context: OverflowContext::Subtree(root.path.clone()),
                })
        })
    }
}

impl<const P: u8> fmt::Display for Rollup<P> {
    /// The indented report: one line per node in pre-order, its last path
    /// segment indented two spaces per level, its subtree net right-aligned
    /// in one column at the currency's exponent (width is ignored).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines: Vec<(String, String)> = self
            .nodes
            .iter()
            .map(|node| {
                let indent = "  ".repeat(node.depth().saturating_sub(1));
                let name = node.path.segments().last().unwrap_or_default();
                (
                    format!("{indent}{name}"),
                    node.subtree.net().display_at(self.exponent).to_string(),
                )
            })
            .collect();
        let names = lines.iter().map(|(name, _)| name.chars().count()).max();
        let amounts = lines.iter().map(|(_, amount)| amount.len()).max();
        for (name, amount) in &lines {
            let gap = names
                .unwrap_or(0)
                .saturating_sub(name.chars().count())
                .saturating_add(2)
                .saturating_add(amounts.unwrap_or(0).saturating_sub(amount.len()));
            writeln!(f, "{name}{}{amount}", " ".repeat(gap))?;
        }
        Ok(())
    }
}
