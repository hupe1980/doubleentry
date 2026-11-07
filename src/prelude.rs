//! The everyday vocabulary in one import: the façades, the one error, the
//! values and drafts of drafting, recording and reading, the five common
//! currencies and the two literal macros.
//!
//! ```
//! use doubleentry::prelude::*;
//!
//! let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
//! books.declare_currency(EUR, 2, date!("2026-01-01"))?;
//! books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
//! books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
//! books.define_months(2026, 3..=3)?;
//! let recorded: Recorded = books.record(
//!     Entry::new("invoice-2026-0001", date!("2026-03-15"))
//!         .debit("Assets:Cash", amount!("1190.00"), EUR)
//!         .credit("Income:Sales", amount!("1190.00"), EUR),
//! )?;
//! assert!(!recorded.replayed);
//! # Ok::<(), Error>(())
//! ```
//!
//! Everything else — stores, proofs beyond [`InclusionProof`](crate::log::InclusionProof),
//! report types beyond [`BalanceQuery`], transparency, testing — is imported
//! by name from its one path.
//! The prelude is the only second path to any item.

pub use crate::currency::{CHF, EUR, GBP, JPY, USD};
pub use crate::{
    AccountHandle, AccountKind, AccountPath, Amount, Balance, BalanceLimit, BalanceQuery, Currency,
    Date,
};
pub use crate::{
    BatchItem, Clearing, Document, Entry, EntryId, Label, Layer, LedgerId, PostingRef, Rate,
    Recorded, Rounding, Side,
};
pub use crate::{Error, Journal, Ledger, Rejection, Result};
pub use crate::{amount, date};
