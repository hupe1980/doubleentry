//! Postings — the atomic monetary movements that make up an entry.

use crate::account::AccountId;
use crate::canonical::{Canonical, CanonicalWriter};
use crate::dimensions::Dimensions;
use crate::money::{Amount, Currency, MoneyError};

/// Which side of the account a posting falls on.
///
/// The direction is explicit rather than encoded in the sign of the amount.
/// A signed net cannot express gross turnover: an account showing zero could
/// have seen no activity or a large volume in both directions, and a trial
/// balance is required to distinguish them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Direction {
    /// The left side.
    Debit,
    /// The right side.
    Credit,
}

impl Direction {
    /// The opposite side.
    #[must_use]
    pub const fn inverse(self) -> Self {
        match self {
            Self::Debit => Self::Credit,
            Self::Credit => Self::Debit,
        }
    }

    /// True for [`Direction::Debit`].
    #[must_use]
    pub const fn is_debit(self) -> bool {
        matches!(self, Self::Debit)
    }

    /// True for [`Direction::Credit`].
    #[must_use]
    pub const fn is_credit(self) -> bool {
        matches!(self, Self::Credit)
    }

    pub(crate) const fn discriminant(self) -> u8 {
        match self {
            Self::Debit => 0,
            Self::Credit => 1,
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Debit => "debit",
            Self::Credit => "credit",
        })
    }
}

/// Whether a posting has settled or is still reserved.
///
/// A pending posting reserves an amount without moving the settled balance. It
/// is later resolved by a further entry that either settles or releases it —
/// both of which are ordinary appends, so nothing is ever mutated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Layer {
    /// The amount has moved.
    #[default]
    Settled,
    /// The amount is reserved but has not moved.
    Pending,
}

impl Layer {
    pub(crate) const fn discriminant(self) -> u8 {
        match self {
            Self::Settled => 0,
            Self::Pending => 1,
        }
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Settled => "settled",
            Self::Pending => "pending",
        })
    }
}

/// A single movement on a single account.
///
/// The amount is a magnitude and must not be negative; the side is carried by
/// [`Posting::direction`]. A negative amount is rejected at entry validation
/// rather than being silently reinterpreted as the opposite direction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Posting<const P: u8> {
    /// The account being moved.
    pub account: AccountId,
    /// Which side of the account.
    pub direction: Direction,
    /// The magnitude of the movement.
    pub amount: Amount<P>,
    /// The currency of the movement.
    pub currency: Currency,
    /// Settled or reserved.
    pub layer: Layer,
    /// Reporting dimensions.
    pub dimensions: Dimensions,
}

impl<const P: u8> Posting<P> {
    /// Creates a settled posting with no dimensions.
    #[must_use]
    pub fn new(
        account: AccountId,
        direction: Direction,
        amount: Amount<P>,
        currency: Currency,
    ) -> Self {
        Self {
            account,
            direction,
            amount,
            currency,
            layer: Layer::Settled,
            dimensions: Dimensions::none(),
        }
    }

    /// Creates a settled debit.
    #[must_use]
    pub fn debit(account: AccountId, amount: Amount<P>, currency: Currency) -> Self {
        Self::new(account, Direction::Debit, amount, currency)
    }

    /// Creates a settled credit.
    #[must_use]
    pub fn credit(account: AccountId, amount: Amount<P>, currency: Currency) -> Self {
        Self::new(account, Direction::Credit, amount, currency)
    }

    /// Sets the layer.
    #[must_use]
    pub fn in_layer(mut self, layer: Layer) -> Self {
        self.layer = layer;
        self
    }

    /// Sets the dimensions.
    #[must_use]
    pub fn with_dimensions(mut self, dimensions: Dimensions) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Returns the same movement on the opposite side.
    ///
    /// This is how a reversal is built: the magnitude is untouched and the side
    /// flips, so no arithmetic is performed and no overflow is possible.
    #[must_use]
    pub fn inverted(&self) -> Self {
        Self {
            account: self.account,
            direction: self.direction.inverse(),
            amount: self.amount,
            currency: self.currency,
            layer: self.layer,
            dimensions: self.dimensions.clone(),
        }
    }

    /// The signed contribution of this posting, debit positive.
    ///
    /// Convenient for netting. Prefer [`Direction`] and magnitudes when gross
    /// turnover matters, since a signed total cannot reproduce it.
    pub fn signed(&self) -> Result<Amount<P>, MoneyError> {
        match self.direction {
            Direction::Debit => Ok(self.amount),
            Direction::Credit => self.amount.checked_neg(),
        }
    }
}

impl<const P: u8> Canonical for Posting<P> {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.u32(self.account.index());
        w.u8(self.direction.discriminant());
        w.i64(self.amount.to_minor());
        w.fixed(self.currency.as_bytes());
        w.u8(self.layer.discriminant());
        self.dimensions.encode(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountId;

    type Eur = Amount<2>;

    fn account() -> AccountId {
        // Handles are opaque to postings; any value works for these tests.
        AccountId::from_index(0)
    }

    #[test]
    fn direction_inverts() {
        assert_eq!(Direction::Debit.inverse(), Direction::Credit);
        assert_eq!(Direction::Credit.inverse(), Direction::Debit);
        assert!(Direction::Debit.is_debit());
        assert!(Direction::Credit.is_credit());
    }

    #[test]
    fn signed_contribution_follows_direction() {
        let d = Posting::<2>::debit(account(), Eur::from_minor(100), Currency::EUR);
        let c = Posting::<2>::credit(account(), Eur::from_minor(100), Currency::EUR);
        assert_eq!(d.signed().expect("no overflow"), Eur::from_minor(100));
        assert_eq!(c.signed().expect("no overflow"), Eur::from_minor(-100));
    }

    #[test]
    fn inverting_flips_the_side_and_keeps_the_magnitude() {
        let p = Posting::<2>::debit(account(), Eur::from_minor(250), Currency::EUR);
        let inv = p.inverted();
        assert_eq!(inv.direction, Direction::Credit);
        assert_eq!(inv.amount, p.amount);
        assert_eq!(inv.account, p.account);
    }

    #[test]
    fn inverting_twice_is_the_identity() {
        let p = Posting::<2>::credit(account(), Eur::from_minor(7), Currency::EUR)
            .in_layer(Layer::Pending);
        assert_eq!(p.inverted().inverted(), p);
    }

    #[test]
    fn default_layer_is_settled() {
        let p = Posting::<2>::debit(account(), Eur::from_minor(1), Currency::EUR);
        assert_eq!(p.layer, Layer::Settled);
    }

    #[test]
    fn encoding_distinguishes_direction() {
        let d = Posting::<2>::debit(account(), Eur::from_minor(100), Currency::EUR);
        let c = Posting::<2>::credit(account(), Eur::from_minor(100), Currency::EUR);
        assert_ne!(d.to_canonical_bytes(), c.to_canonical_bytes());
    }

    #[test]
    fn encoding_distinguishes_layer_and_currency() {
        let base = Posting::<2>::debit(account(), Eur::from_minor(100), Currency::EUR);
        let pending = base.clone().in_layer(Layer::Pending);
        let usd = Posting::<2>::debit(account(), Eur::from_minor(100), Currency::USD);
        assert_ne!(base.to_canonical_bytes(), pending.to_canonical_bytes());
        assert_ne!(base.to_canonical_bytes(), usd.to_canonical_bytes());
    }
}
