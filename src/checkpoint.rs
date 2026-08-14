//! Checkpoints and balance assertions.
//!
//! Both are *claims about balances that the journal can check*, and they exist
//! for opposite reasons.
//!
//! A [`Checkpoint`] is an optimisation. A balance is defined as a fold over the
//! journal, which is correct and linear in history; a checkpoint records the
//! fold up to a point so later reads start from there. Because that trades a
//! definition for a cache, a checkpoint carries the tree head it was taken
//! against, which does double duty: it *names* the prefix the balance covers and
//! *pins* the history that prefix belongs to. So a checkpoint cannot be quietly
//! reused against a log that has changed, and the journal can always re-derive
//! it.
//!
//! A [`BalanceAssertion`] is the opposite: a claim from *outside* the ledger — a
//! bank statement, a counterparty confirmation, an ERP export — checked against
//! the fold. It catches the divergence that reconciliation exists to find, and
//! it is the cheapest such mechanism there is.

use time::Date;

use crate::balance::{Balance, BalanceKey};
use crate::merkle::TreeHead;
use crate::money::{Amount, MoneyError};
use crate::posting::Direction;

/// A recorded balance over a known prefix of the log.
///
/// The prefix is the tree head's [`size`](TreeHead::size) — there is no separate
/// position field, because two fields that must agree are two fields that can
/// disagree. A checkpoint says: *after these `size` entries, whose Merkle root
/// was this, the account held this balance.*
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Checkpoint<const P: u8> {
    /// What the balance is for.
    pub key: BalanceKey,
    /// The balance after the first [`Self::size`] entries.
    pub balance: Balance<P>,
    /// The tree head the checkpoint was taken against.
    ///
    /// Pins the checkpoint to one specific history *and* names the prefix it
    /// covers. A checkpoint whose head no longer matches the log it is being
    /// used with is stale by construction, not by convention.
    pub tree_head: TreeHead,
}

impl<const P: u8> Checkpoint<P> {
    /// Creates a checkpoint over the prefix `tree_head` commits to.
    #[must_use]
    pub fn new(key: BalanceKey, balance: Balance<P>, tree_head: TreeHead) -> Self {
        Self {
            key,
            balance,
            tree_head,
        }
    }

    /// Number of entries folded into the balance.
    ///
    /// Zero for a checkpoint taken over an empty log, which is a perfectly
    /// ordinary checkpoint: it says the account had not moved yet, and it stays
    /// true however far the log grows afterwards.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.tree_head.size
    }

    /// True when this checkpoint was taken against `head`.
    #[must_use]
    pub fn matches_head(&self, head: &TreeHead) -> bool {
        self.tree_head == *head
    }
}

/// Why a checkpoint failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CheckpointError {
    /// The recorded balance disagrees with a fold over the journal.
    #[error("checkpoint balance does not match the journal")]
    BalanceMismatch,
    /// The checkpoint covers more entries than the journal holds.
    #[error("checkpoint covers {size} entries, beyond the journal")]
    SizeOutOfRange {
        /// The prefix size claimed.
        size: u64,
    },
    /// The checkpoint was taken against a different history.
    #[error("checkpoint tree head does not match the journal at that size")]
    HeadMismatch,
    /// Folding the journal overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// Which state of the books an assertion is evaluated against.
///
/// The two are genuinely different questions, and picking the wrong one is the
/// classic reconciliation mistake. A log position is exact and reproducible but
/// only meaningful inside this system. A date is what every external source
/// speaks — a bank statement says "as at 31 March", not "after 4,812 entries" —
/// and it folds by *booking date* regardless of when an entry was recorded, so a
/// backdated entry lands where it economically belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AssertAt {
    /// Everything recorded so far.
    Now,
    /// The first `size` entries in log order.
    ///
    /// Zero asserts against an empty ledger.
    Prefix {
        /// Number of entries folded in.
        size: u64,
    },
    /// Every entry booked on or before `date`, whenever it was recorded.
    OnDate {
        /// The last booking date folded in, inclusive.
        date: Date,
    },
}

/// A claim that an account holds a particular balance at a point in the books.
///
/// The expected value is a **signed net**, debit positive, because that is the
/// form an external source reports: a statement says what the balance is, not
/// how much moved in each direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BalanceAssertion<const P: u8> {
    /// What is being asserted about.
    pub key: BalanceKey,
    /// Which state of the books to evaluate against.
    pub at: AssertAt,
    /// The expected signed net, debit positive.
    pub expected: Amount<P>,
}

impl<const P: u8> BalanceAssertion<P> {
    /// Asserts a signed net, debit positive, against the current state.
    #[must_use]
    pub fn net(key: BalanceKey, expected: Amount<P>) -> Self {
        Self {
            key,
            at: AssertAt::Now,
            expected,
        }
    }

    /// Asserts a magnitude on a given side.
    pub fn on_side(
        key: BalanceKey,
        direction: Direction,
        magnitude: Amount<P>,
    ) -> Result<Self, MoneyError> {
        let expected = match direction {
            Direction::Debit => magnitude,
            Direction::Credit => magnitude.checked_neg()?,
        };
        Ok(Self::net(key, expected))
    }

    /// Evaluates the assertion over the first `size` entries.
    #[must_use]
    pub fn over_prefix(mut self, size: u64) -> Self {
        self.at = AssertAt::Prefix { size };
        self
    }

    /// Evaluates the assertion over everything booked on or before `date`.
    ///
    /// This is the form to reconcile against an external statement.
    #[must_use]
    pub fn on_date(mut self, date: Date) -> Self {
        self.at = AssertAt::OnDate { date };
        self
    }

    /// Compares the assertion against an actual balance.
    pub fn check(&self, actual: &Balance<P>) -> Result<AssertionOutcome<P>, MoneyError> {
        let actual_net = actual.signed_net()?;
        if actual_net == self.expected {
            Ok(AssertionOutcome::Held { actual: *actual })
        } else {
            Ok(AssertionOutcome::Failed {
                expected: self.expected,
                actual: actual_net,
                difference: actual_net.checked_sub(self.expected)?,
            })
        }
    }
}

/// The result of checking a [`BalanceAssertion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AssertionOutcome<const P: u8> {
    /// The ledger agrees with the claim.
    Held {
        /// The balance found, with both gross totals.
        actual: Balance<P>,
    },
    /// The ledger disagrees.
    Failed {
        /// What was claimed, as a signed net.
        expected: Amount<P>,
        /// What the ledger holds, as a signed net.
        actual: Amount<P>,
        /// `actual - expected`; the amount unaccounted for.
        difference: Amount<P>,
    },
}

impl<const P: u8> AssertionOutcome<P> {
    /// True when the ledger agreed.
    #[must_use]
    pub const fn held(&self) -> bool {
        matches!(self, Self::Held { .. })
    }
}

impl<const P: u8> std::fmt::Display for AssertionOutcome<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held { actual } => {
                write!(
                    f,
                    "held (debits {}, credits {})",
                    actual.debits, actual.credits
                )
            }
            Self::Failed {
                expected,
                actual,
                difference,
            } => write!(
                f,
                "failed: expected {expected}, found {actual}, off by {difference}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountId;
    use crate::hash::Hash;
    use crate::money::Currency;
    use crate::posting::Layer;

    type Eur = Amount<2>;

    fn key() -> BalanceKey {
        BalanceKey {
            account: AccountId::from_index(0),
            currency: Currency::EUR,
            layer: Layer::Settled,
        }
    }

    fn balance(debits: i64, credits: i64) -> Balance<2> {
        let mut b = Balance::<2>::ZERO;
        b.add(Direction::Debit, Eur::from_minor(debits))
            .expect("ok");
        b.add(Direction::Credit, Eur::from_minor(credits))
            .expect("ok");
        b
    }

    fn head(size: u64, byte: u8) -> TreeHead {
        TreeHead {
            size,
            root: Hash::from_bytes([byte; 32]),
        }
    }

    #[test]
    fn an_assertion_that_matches_holds() {
        let a = BalanceAssertion::net(key(), Eur::from_minor(100));
        let outcome = a.check(&balance(100, 0)).expect("ok");
        assert!(outcome.held());
    }

    #[test]
    fn an_assertion_reports_the_difference_when_it_fails() {
        let a = BalanceAssertion::net(key(), Eur::from_minor(100));
        let outcome = a.check(&balance(130, 0)).expect("ok");
        assert_eq!(
            outcome,
            AssertionOutcome::Failed {
                expected: Eur::from_minor(100),
                actual: Eur::from_minor(130),
                difference: Eur::from_minor(30),
            }
        );
        assert!(!outcome.held());
    }

    #[test]
    fn an_assertion_matches_the_net_not_the_gross() {
        // 500 in each direction nets to zero, which is what a statement reports.
        let a = BalanceAssertion::net(key(), Eur::ZERO);
        assert!(a.check(&balance(500, 500)).expect("ok").held());
    }

    #[test]
    fn a_credit_side_assertion_is_a_negative_net() {
        let a =
            BalanceAssertion::on_side(key(), Direction::Credit, Eur::from_minor(250)).expect("ok");
        assert_eq!(a.expected, Eur::from_minor(-250));
        assert!(a.check(&balance(0, 250)).expect("ok").held());
    }

    #[test]
    fn a_debit_side_assertion_is_a_positive_net() {
        let a =
            BalanceAssertion::on_side(key(), Direction::Debit, Eur::from_minor(250)).expect("ok");
        assert_eq!(a.expected, Eur::from_minor(250));
        assert!(a.check(&balance(250, 0)).expect("ok").held());
    }

    #[test]
    fn a_checkpoint_knows_which_history_it_belongs_to() {
        let cp = Checkpoint::new(key(), balance(100, 0), head(10, 1));
        assert_eq!(
            cp.size(),
            10,
            "the head names the prefix; nothing else does"
        );
        assert!(cp.matches_head(&head(10, 1)));
        // Same size, different contents: a rewritten history.
        assert!(!cp.matches_head(&head(10, 2)));
        // Same contents, different size: the log has grown.
        assert!(!cp.matches_head(&head(11, 1)));
    }

    #[test]
    fn an_assertion_carries_the_point_it_is_about() {
        use time::macros::date;
        let a = BalanceAssertion::net(key(), Eur::from_minor(100));
        assert_eq!(a.at, AssertAt::Now);
        assert_eq!(a.over_prefix(0).at, AssertAt::Prefix { size: 0 });
        assert_eq!(
            a.on_date(date!(2026 - 03 - 31)).at,
            AssertAt::OnDate {
                date: date!(2026 - 03 - 31)
            }
        );
    }

    #[test]
    fn assertion_display_is_operator_readable() {
        let a = BalanceAssertion::net(key(), Eur::from_minor(100));
        let failed = a.check(&balance(130, 0)).expect("ok");
        assert_eq!(
            failed.to_string(),
            "failed: expected 1.00, found 1.30, off by 0.30"
        );
    }
}
