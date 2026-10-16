//! Every read, once: balances, the trial balance, statements, the rollup,
//! open items and balance assertions.
//!
//! Each read is a **plan plus a pure fold**. A [`BalanceQuery`] resolves
//! against one log prefix into a selection and picks the cheapest store
//! read — a maintained balance, the balance after a position, or a fold of
//! the key's posting rows; the fold applies the whole selection to every
//! row again, so a store that filters less is still right. The façades
//! (`Journal`, `Ledger`) only fetch pages and feed the folds here, so every
//! store answers every read identically, and SQL never adds money.

pub(crate) mod accounts;
mod assertion;
mod balances;
mod open_items;
pub(crate) mod query;
mod rollup;
mod statement;

pub use assertion::{
    AssertAt, AssertionFailure, AssertionOutcome, BalanceAssertion, Deviation, Difference,
};
pub use balances::{TrialBalance, TrialBalanceRow};
pub use open_items::OpenItem;
pub(crate) use query::BalanceQuery;
pub use query::DateBasis;
pub use rollup::{Rollup, RollupNode};
pub use statement::{Statement, StatementLine};

pub(crate) use accounts::AccountBook;
pub(crate) use balances::BalanceFold;
pub(crate) use open_items::OpenItemsFold;
pub(crate) use query::{AccountName, Plan, Selection};
pub(crate) use statement::StatementFold;
