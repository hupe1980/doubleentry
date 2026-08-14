//! Open-item clearing.
//!
//! A receivable is raised by one posting and settled by another. Knowing *which*
//! settled *which* is what turns a running balance into a list of open items —
//! and it is a ledger concern, not an application one, because it has to agree
//! with the postings exactly.
//!
//! Nothing is mutated. Clearing records which postings offset which, and by how
//! much; the postings themselves never change. A posting's **residual** is its
//! amount less everything applied to it, and it is *open* while the residual is
//! positive.
//!
//! # Partial application
//!
//! Applying less than the full amount leaves both sides open for the remainder,
//! which is the behaviour a partial payment needs: the invoice stays visible as
//! partly settled rather than vanishing or being rewritten.
//!
//! Booking the shortfall as a fresh item instead — clearing the original in full
//! and raising a new one for the difference — is an ordinary entry the
//! application posts. The engine does not choose between the two policies.
//!
//! # Resetting
//!
//! A clearing can be reset when it turns out to have matched the wrong items.
//! The reset is a new record that releases the applied amounts; the original
//! clearing remains in the register, because an assignment that was made and
//! withdrawn is itself part of the audit trail.

use std::collections::BTreeMap;

use time::Date;
use uuid::Uuid;

use crate::account::AccountId;
use crate::balance::{Balance, BalanceKey};
use crate::entry::EntryId;
use crate::money::{Amount, Currency, MoneyError};
use crate::posting::{Direction, Layer, Posting};

/// A reference to one posting inside one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PostingRef {
    /// The entry holding the posting.
    pub entry: EntryId,
    /// Zero-based position within that entry's postings.
    pub index: u16,
}

impl PostingRef {
    /// Creates a reference.
    #[must_use]
    pub const fn new(entry: EntryId, index: u16) -> Self {
        Self { entry, index }
    }
}

impl std::fmt::Display for PostingRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.entry, self.index)
    }
}

/// Identifier for a clearing record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ClearingId(Uuid);

impl ClearingId {
    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Generates a fresh time-ordered identifier.
    ///
    /// Reads the clock, so it is not part of the deterministic path; the engine
    /// never calls it.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7()) // purity-exempt: identity, not ledger state
    }
}

impl std::fmt::Display for ClearingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// One posting and the amount of it this clearing applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ClearedItem<const P: u8> {
    /// The posting being applied against.
    pub posting: PostingRef,
    /// How much of it this clearing consumes. A magnitude, never negative.
    pub applied: Amount<P>,
}

/// A record that a set of postings offset one another.
///
/// Scoped to one `(account, currency, layer)` — the same key a balance and an
/// open-item list are reported against. Anything wider would let a clearing
/// relate postings that never appear in the same statement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Clearing<const P: u8> {
    /// Identifier.
    pub id: ClearingId,
    /// The account whose items are being cleared.
    pub account: AccountId,
    /// The currency the items are in.
    pub currency: Currency,
    /// Whether settled movements or reservations are being cleared.
    ///
    /// A reservation and a settled payment are different claims on the same
    /// account: netting one against the other would report an open item as
    /// closed while the money had not moved. Both layers clear the same way;
    /// they just do not clear against each other.
    pub layer: Layer,
    /// The date the assignment was made.
    pub cleared_on: Date,
    /// The postings and the amounts applied to each.
    pub items: Vec<ClearedItem<P>>,
}

impl<const P: u8> Clearing<P> {
    /// The balance key this clearing operates on.
    #[must_use]
    pub fn key(&self) -> BalanceKey {
        BalanceKey {
            account: self.account,
            currency: self.currency,
            layer: self.layer,
        }
    }

    /// Starts a settled clearing on one account and currency.
    #[must_use]
    pub fn new(id: ClearingId, key: BalanceKey, cleared_on: Date) -> Self {
        Self {
            id,
            account: key.account,
            currency: key.currency,
            layer: key.layer,
            cleared_on,
            items: Vec::new(),
        }
    }

    /// Applies `applied` of `posting` in this clearing.
    #[must_use]
    pub fn apply(mut self, posting: PostingRef, applied: Amount<P>) -> Self {
        self.items.push(ClearedItem { posting, applied });
        self
    }
}

/// Something that can resolve a [`PostingRef`] to the posting it names.
pub trait PostingLookup<const P: u8> {
    /// Returns the posting, or `None` if the reference does not resolve.
    fn posting(&self, reference: PostingRef) -> Option<&Posting<P>>;
}

/// Failure recording or resetting a clearing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClearingError {
    /// Fewer than two items: a clearing must relate at least two postings.
    #[error("a clearing needs at least two items, got {count}")]
    TooFewItems {
        /// Number of items supplied.
        count: usize,
    },
    /// The same posting appeared twice in one clearing.
    #[error("posting {posting} appears more than once in the same clearing")]
    DuplicateItem {
        /// The repeated reference.
        posting: PostingRef,
    },
    /// A referenced posting does not exist.
    #[error("posting {posting} does not resolve")]
    UnknownPosting {
        /// The unresolvable reference.
        posting: PostingRef,
    },
    /// A referenced posting is on a different account.
    #[error("posting {posting} is not on account {expected}")]
    WrongAccount {
        /// The offending reference.
        posting: PostingRef,
        /// The account the clearing is for.
        expected: AccountId,
    },
    /// A referenced posting is in a different currency.
    #[error("posting {posting} is not in {expected}")]
    WrongCurrency {
        /// The offending reference.
        posting: PostingRef,
        /// The currency the clearing is for.
        expected: Currency,
    },
    /// A referenced posting is in a different layer.
    #[error("posting {posting} is not in the {expected} layer")]
    WrongLayer {
        /// The offending reference.
        posting: PostingRef,
        /// The layer the clearing is for.
        expected: Layer,
    },
    /// An applied amount was zero or negative.
    #[error("posting {posting} was applied a non-positive amount")]
    NonPositiveApplication {
        /// The offending reference.
        posting: PostingRef,
    },
    /// Applying this much would exceed what the posting still has open.
    ///
    /// Amounts are minor units at `scale`, so the variant stays independent of
    /// the ledger's compile-time precision.
    #[error(
        "applying {requested_minor} to {posting} exceeds its residual of \
         {residual_minor} (scale {scale})"
    )]
    OverApplied {
        /// The offending reference.
        posting: PostingRef,
        /// The amount requested, in minor units.
        requested_minor: i64,
        /// The amount still open, in minor units.
        residual_minor: i64,
        /// Decimal places the minor units are expressed in.
        scale: u8,
    },
    /// The debit and credit sides of the clearing did not match.
    #[error(
        "clearing is unbalanced at scale {scale}: debits {debits_minor}, \
         credits {credits_minor}"
    )]
    Unbalanced {
        /// Total applied on the debit side, in minor units.
        debits_minor: i64,
        /// Total applied on the credit side, in minor units.
        credits_minor: i64,
        /// Decimal places the minor units are expressed in.
        scale: u8,
    },
    /// The clearing identifier was already used.
    #[error("clearing {id} is already recorded")]
    DuplicateId {
        /// The offending identifier.
        id: ClearingId,
    },
    /// The clearing being reset does not exist.
    #[error("clearing {id} is not recorded")]
    UnknownClearing {
        /// The missing identifier.
        id: ClearingId,
    },
    /// The clearing being reset has already been reset.
    #[error("clearing {id} has already been reset")]
    AlreadyReset {
        /// The offending identifier.
        id: ClearingId,
    },
    /// Arithmetic overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// What a clearing did, recorded in order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClearingEvent<const P: u8> {
    /// Items were assigned to one another.
    Cleared(Clearing<P>),
    /// An earlier assignment was released.
    Reset {
        /// The clearing being released.
        clearing: ClearingId,
        /// When it was released.
        on: Date,
    },
}

/// A posting with something still outstanding on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenItem<const P: u8> {
    /// Which posting.
    pub posting: PostingRef,
    /// Which side it falls on.
    pub direction: Direction,
    /// The posting's original amount.
    pub original: Amount<P>,
    /// How much has been applied to it.
    pub applied: Amount<P>,
    /// What remains open.
    pub residual: Amount<P>,
}

/// The append-only record of clearings, and the open items it implies.
///
/// Applied amounts are derived by replaying the events, so a reset genuinely
/// releases what it released and nothing is edited in place.
#[derive(Debug, Clone)]
pub struct ClearingRegister<const P: u8> {
    events: Vec<ClearingEvent<P>>,
    /// Applied amount per posting, derived from `events`.
    applied: BTreeMap<PostingRef, Amount<P>>,
    /// Clearings that have been reset.
    reset: BTreeMap<ClearingId, Date>,
    /// Index of every recorded clearing.
    by_id: BTreeMap<ClearingId, usize>,
}

impl<const P: u8> Default for ClearingRegister<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const P: u8> ClearingRegister<P> {
    /// Creates an empty register.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            applied: BTreeMap::new(),
            reset: BTreeMap::new(),
            by_id: BTreeMap::new(),
        }
    }

    /// How much has been applied to a posting.
    #[must_use]
    pub fn applied_to(&self, posting: PostingRef) -> Amount<P> {
        self.applied.get(&posting).copied().unwrap_or(Amount::ZERO)
    }

    /// What remains open on a posting.
    pub fn residual_of(
        &self,
        posting: PostingRef,
        lookup: &impl PostingLookup<P>,
    ) -> Result<Amount<P>, ClearingError> {
        let found = lookup
            .posting(posting)
            .ok_or(ClearingError::UnknownPosting { posting })?;
        Ok(found.amount.checked_sub(self.applied_to(posting))?)
    }

    /// Records a clearing after checking every rule.
    pub fn clear(
        &mut self,
        clearing: Clearing<P>,
        lookup: &impl PostingLookup<P>,
    ) -> Result<(), ClearingError> {
        if self.by_id.contains_key(&clearing.id) {
            return Err(ClearingError::DuplicateId { id: clearing.id });
        }
        if clearing.items.len() < 2 {
            return Err(ClearingError::TooFewItems {
                count: clearing.items.len(),
            });
        }

        let mut seen: BTreeMap<PostingRef, ()> = BTreeMap::new();
        let mut sides = Balance::<P>::ZERO;

        for item in &clearing.items {
            if seen.insert(item.posting, ()).is_some() {
                return Err(ClearingError::DuplicateItem {
                    posting: item.posting,
                });
            }
            if !item.applied.is_positive() {
                return Err(ClearingError::NonPositiveApplication {
                    posting: item.posting,
                });
            }

            let posting = lookup
                .posting(item.posting)
                .ok_or(ClearingError::UnknownPosting {
                    posting: item.posting,
                })?;
            if posting.account != clearing.account {
                return Err(ClearingError::WrongAccount {
                    posting: item.posting,
                    expected: clearing.account,
                });
            }
            if posting.currency != clearing.currency {
                return Err(ClearingError::WrongCurrency {
                    posting: item.posting,
                    expected: clearing.currency,
                });
            }
            if posting.layer != clearing.layer {
                return Err(ClearingError::WrongLayer {
                    posting: item.posting,
                    expected: clearing.layer,
                });
            }

            let residual = posting.amount.checked_sub(self.applied_to(item.posting))?;
            if item.applied > residual {
                return Err(ClearingError::OverApplied {
                    posting: item.posting,
                    requested_minor: item.applied.to_minor(),
                    residual_minor: residual.to_minor(),
                    scale: P,
                });
            }

            sides.add(posting.direction, item.applied)?;
        }

        if !sides.is_balanced() {
            return Err(ClearingError::Unbalanced {
                debits_minor: sides.debits.to_minor(),
                credits_minor: sides.credits.to_minor(),
                scale: P,
            });
        }

        let mut staged = Vec::with_capacity(clearing.items.len());
        for item in &clearing.items {
            let updated = self.applied_to(item.posting).checked_add(item.applied)?;
            staged.push((item.posting, updated));
        }
        for (posting, updated) in staged {
            self.applied.insert(posting, updated);
        }
        self.by_id.insert(clearing.id, self.events.len());
        self.events.push(ClearingEvent::Cleared(clearing));
        Ok(())
    }

    /// Releases a clearing, reopening the items it had assigned.
    pub fn reset(&mut self, id: ClearingId, on: Date) -> Result<(), ClearingError> {
        let Some(index) = self.by_id.get(&id).copied() else {
            return Err(ClearingError::UnknownClearing { id });
        };
        if self.reset.contains_key(&id) {
            return Err(ClearingError::AlreadyReset { id });
        }
        let Some(ClearingEvent::Cleared(clearing)) = self.events.get(index) else {
            return Err(ClearingError::UnknownClearing { id });
        };

        let items = clearing.items.clone();
        let mut staged = Vec::with_capacity(items.len());
        for item in &items {
            let updated = self.applied_to(item.posting).checked_sub(item.applied)?;
            staged.push((item.posting, updated));
        }
        for (posting, updated) in staged {
            self.applied.insert(posting, updated);
        }
        self.reset.insert(id, on);
        self.events.push(ClearingEvent::Reset { clearing: id, on });
        Ok(())
    }

    /// True when a clearing has been released.
    #[must_use]
    pub fn is_reset(&self, id: ClearingId) -> bool {
        self.reset.contains_key(&id)
    }

    /// The clearing with a given identifier.
    #[must_use]
    pub fn get(&self, id: ClearingId) -> Option<&Clearing<P>> {
        match self.by_id.get(&id).and_then(|i| self.events.get(*i)) {
            Some(ClearingEvent::Cleared(c)) => Some(c),
            _ => None,
        }
    }

    /// Every recorded event, in order.
    #[must_use]
    pub fn events(&self) -> &[ClearingEvent<P>] {
        &self.events
    }

    /// Number of clearings recorded, including any since reset.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// True when nothing has been cleared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Computes the open items among `candidates`.
    ///
    /// Ordered by posting reference, so the result is reproducible.
    pub fn open_items(
        &self,
        candidates: impl IntoIterator<Item = PostingRef>,
        lookup: &impl PostingLookup<P>,
    ) -> Result<Vec<OpenItem<P>>, ClearingError> {
        let mut out = Vec::new();
        for reference in candidates {
            let Some(posting) = lookup.posting(reference) else {
                return Err(ClearingError::UnknownPosting { posting: reference });
            };
            let applied = self.applied_to(reference);
            let residual = posting.amount.checked_sub(applied)?;
            if residual.is_positive() {
                out.push(OpenItem {
                    posting: reference,
                    direction: posting.direction,
                    original: posting.amount,
                    applied,
                    residual,
                });
            }
        }
        out.sort_by_key(|i| i.posting);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    type Eur = Amount<2>;

    /// A hand-built posting table, so these tests exercise clearing alone.
    struct Table {
        postings: BTreeMap<PostingRef, Posting<2>>,
    }

    impl PostingLookup<2> for Table {
        fn posting(&self, reference: PostingRef) -> Option<&Posting<2>> {
            self.postings.get(&reference)
        }
    }

    fn account() -> AccountId {
        AccountId::from_index(0)
    }

    fn table(entries: &[(u16, Direction, i64)]) -> (Table, Vec<PostingRef>) {
        let entry = EntryId::generate();
        let mut postings = BTreeMap::new();
        let mut refs = Vec::new();
        for (index, direction, amount) in entries {
            let reference = PostingRef::new(entry, *index);
            postings.insert(
                reference,
                Posting::new(
                    account(),
                    *direction,
                    Eur::from_minor(*amount),
                    Currency::EUR,
                ),
            );
            refs.push(reference);
        }
        (Table { postings }, refs)
    }

    fn key(layer: Layer) -> BalanceKey {
        BalanceKey {
            account: account(),
            currency: Currency::EUR,
            layer,
        }
    }

    fn clearing(items: &[(PostingRef, i64)]) -> Clearing<2> {
        clearing_in(Layer::Settled, items)
    }

    fn clearing_in(layer: Layer, items: &[(PostingRef, i64)]) -> Clearing<2> {
        items.iter().fold(
            Clearing::new(ClearingId::generate(), key(layer), date!(2026 - 03 - 15)),
            |c, (posting, applied)| c.apply(*posting, Eur::from_minor(*applied)),
        )
    }

    #[test]
    fn a_full_clearing_closes_both_items() {
        let (t, r) = table(&[(0, Direction::Debit, 1000), (1, Direction::Credit, 1000)]);
        let mut reg = ClearingRegister::<2>::new();
        reg.clear(clearing(&[(r[0], 1000), (r[1], 1000)]), &t)
            .expect("clears");

        let open = reg.open_items(r.iter().copied(), &t).expect("ok");
        assert!(open.is_empty(), "nothing should remain open");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn a_partial_payment_leaves_both_sides_open() {
        let (t, r) = table(&[(0, Direction::Debit, 1000), (1, Direction::Credit, 400)]);
        let mut reg = ClearingRegister::<2>::new();
        reg.clear(clearing(&[(r[0], 400), (r[1], 400)]), &t)
            .expect("clears");

        let open = reg.open_items(r.iter().copied(), &t).expect("ok");
        assert_eq!(open.len(), 1, "the invoice remains partly open");
        let item = open.first().expect("present");
        assert_eq!(item.posting, r[0]);
        assert_eq!(item.original, Eur::from_minor(1000));
        assert_eq!(item.applied, Eur::from_minor(400));
        assert_eq!(item.residual, Eur::from_minor(600));
    }

    #[test]
    fn successive_partial_payments_accumulate() {
        let (t, r) = table(&[
            (0, Direction::Debit, 1000),
            (1, Direction::Credit, 400),
            (2, Direction::Credit, 600),
        ]);
        let mut reg = ClearingRegister::<2>::new();
        reg.clear(clearing(&[(r[0], 400), (r[1], 400)]), &t)
            .expect("first payment");
        reg.clear(clearing(&[(r[0], 600), (r[2], 600)]), &t)
            .expect("second payment");

        assert_eq!(reg.applied_to(r[0]), Eur::from_minor(1000));
        assert!(
            reg.open_items(r.iter().copied(), &t)
                .expect("ok")
                .is_empty()
        );
    }

    #[test]
    fn over_application_is_refused() {
        let (t, r) = table(&[(0, Direction::Debit, 1000), (1, Direction::Credit, 1000)]);
        let mut reg = ClearingRegister::<2>::new();
        reg.clear(clearing(&[(r[0], 600), (r[1], 600)]), &t)
            .expect("first");

        let err = reg
            .clear(clearing(&[(r[0], 500), (r[1], 500)]), &t)
            .expect_err("only 400 remains");
        assert!(matches!(err, ClearingError::OverApplied { .. }));

        // The failed attempt applied nothing.
        assert_eq!(reg.applied_to(r[0]), Eur::from_minor(600));
    }

    #[test]
    fn an_unbalanced_clearing_is_refused() {
        let (t, r) = table(&[(0, Direction::Debit, 1000), (1, Direction::Credit, 1000)]);
        let mut reg = ClearingRegister::<2>::new();
        let err = reg
            .clear(clearing(&[(r[0], 1000), (r[1], 900)]), &t)
            .expect_err("sides differ");
        assert!(matches!(err, ClearingError::Unbalanced { .. }));
        assert_eq!(reg.applied_to(r[0]), Eur::ZERO);
    }

    #[test]
    fn same_side_items_alone_cannot_clear() {
        let (t, r) = table(&[(0, Direction::Debit, 500), (1, Direction::Debit, 500)]);
        let mut reg = ClearingRegister::<2>::new();
        assert!(matches!(
            reg.clear(clearing(&[(r[0], 500), (r[1], 500)]), &t),
            Err(ClearingError::Unbalanced { .. })
        ));
    }

    #[test]
    fn a_reset_reopens_the_items() {
        let (t, r) = table(&[(0, Direction::Debit, 1000), (1, Direction::Credit, 1000)]);
        let mut reg = ClearingRegister::<2>::new();
        let c = clearing(&[(r[0], 1000), (r[1], 1000)]);
        let id = c.id;
        reg.clear(c, &t).expect("clears");
        assert!(
            reg.open_items(r.iter().copied(), &t)
                .expect("ok")
                .is_empty()
        );

        reg.reset(id, date!(2026 - 04 - 01)).expect("resets");
        let open = reg.open_items(r.iter().copied(), &t).expect("ok");
        assert_eq!(open.len(), 2, "both items reopen");
        assert!(reg.is_reset(id));

        // The withdrawn assignment stays in the record.
        assert_eq!(reg.events().len(), 2);
        assert!(reg.get(id).is_some());
    }

    #[test]
    fn a_clearing_cannot_be_reset_twice() {
        let (t, r) = table(&[(0, Direction::Debit, 100), (1, Direction::Credit, 100)]);
        let mut reg = ClearingRegister::<2>::new();
        let c = clearing(&[(r[0], 100), (r[1], 100)]);
        let id = c.id;
        reg.clear(c, &t).expect("clears");
        reg.reset(id, date!(2026 - 04 - 01)).expect("resets");
        assert!(matches!(
            reg.reset(id, date!(2026 - 04 - 02)),
            Err(ClearingError::AlreadyReset { .. })
        ));
    }

    #[test]
    fn resetting_an_unknown_clearing_is_an_error() {
        let mut reg = ClearingRegister::<2>::new();
        assert!(matches!(
            reg.reset(ClearingId::generate(), date!(2026 - 04 - 01)),
            Err(ClearingError::UnknownClearing { .. })
        ));
    }

    #[test]
    fn reset_then_reclear_works() {
        let (t, r) = table(&[
            (0, Direction::Debit, 1000),
            (1, Direction::Credit, 1000),
            (2, Direction::Credit, 1000),
        ]);
        let mut reg = ClearingRegister::<2>::new();

        // Matched to the wrong payment, then corrected.
        let wrong = clearing(&[(r[0], 1000), (r[1], 1000)]);
        let wrong_id = wrong.id;
        reg.clear(wrong, &t).expect("clears");
        reg.reset(wrong_id, date!(2026 - 04 - 01)).expect("resets");
        reg.clear(clearing(&[(r[0], 1000), (r[2], 1000)]), &t)
            .expect("re-clears against the right item");

        assert_eq!(reg.applied_to(r[1]), Eur::ZERO);
        assert_eq!(reg.applied_to(r[2]), Eur::from_minor(1000));
    }

    #[test]
    fn structural_rules_are_enforced() {
        let (t, r) = table(&[(0, Direction::Debit, 100), (1, Direction::Credit, 100)]);
        let mut reg = ClearingRegister::<2>::new();

        assert!(matches!(
            reg.clear(clearing(&[(r[0], 100)]), &t),
            Err(ClearingError::TooFewItems { count: 1 })
        ));
        assert!(matches!(
            reg.clear(clearing(&[(r[0], 50), (r[0], 50)]), &t),
            Err(ClearingError::DuplicateItem { .. })
        ));
        assert!(matches!(
            reg.clear(clearing(&[(r[0], 0), (r[1], 0)]), &t),
            Err(ClearingError::NonPositiveApplication { .. })
        ));

        let ghost = PostingRef::new(EntryId::generate(), 9);
        assert!(matches!(
            reg.clear(clearing(&[(r[0], 100), (ghost, 100)]), &t),
            Err(ClearingError::UnknownPosting { .. })
        ));
    }

    #[test]
    fn items_on_another_account_or_currency_are_refused() {
        let entry = EntryId::generate();
        let a = PostingRef::new(entry, 0);
        let b = PostingRef::new(entry, 1);
        let mut postings = BTreeMap::new();
        postings.insert(
            a,
            Posting::new(
                account(),
                Direction::Debit,
                Eur::from_minor(100),
                Currency::EUR,
            ),
        );
        postings.insert(
            b,
            Posting::new(
                AccountId::from_index(7),
                Direction::Credit,
                Eur::from_minor(100),
                Currency::EUR,
            ),
        );
        let t = Table { postings };
        let mut reg = ClearingRegister::<2>::new();
        assert!(matches!(
            reg.clear(clearing(&[(a, 100), (b, 100)]), &t),
            Err(ClearingError::WrongAccount { .. })
        ));

        let mut postings = BTreeMap::new();
        postings.insert(
            a,
            Posting::new(
                account(),
                Direction::Debit,
                Eur::from_minor(100),
                Currency::EUR,
            ),
        );
        postings.insert(
            b,
            Posting::new(
                account(),
                Direction::Credit,
                Eur::from_minor(100),
                Currency::USD,
            ),
        );
        let t = Table { postings };
        assert!(matches!(
            reg.clear(clearing(&[(a, 100), (b, 100)]), &t),
            Err(ClearingError::WrongCurrency { .. })
        ));
    }

    #[test]
    fn a_duplicate_clearing_id_is_refused() {
        let (t, r) = table(&[(0, Direction::Debit, 1000), (1, Direction::Credit, 1000)]);
        let mut reg = ClearingRegister::<2>::new();
        let mut first = clearing(&[(r[0], 400), (r[1], 400)]);
        let id = first.id;
        reg.clear(first.clone(), &t).expect("clears");

        first.id = id;
        first.items = vec![
            ClearedItem {
                posting: r[0],
                applied: Eur::from_minor(100),
            },
            ClearedItem {
                posting: r[1],
                applied: Eur::from_minor(100),
            },
        ];
        assert!(matches!(
            reg.clear(first, &t),
            Err(ClearingError::DuplicateId { .. })
        ));
    }

    #[test]
    fn reservations_clear_against_reservations() {
        let entry = EntryId::generate();
        let a = PostingRef::new(entry, 0);
        let b = PostingRef::new(entry, 1);
        let mut postings = BTreeMap::new();
        for (reference, direction) in [(a, Direction::Debit), (b, Direction::Credit)] {
            postings.insert(
                reference,
                Posting::new(account(), direction, Eur::from_minor(50), Currency::EUR)
                    .in_layer(Layer::Pending),
            );
        }
        let t = Table { postings };
        let mut reg = ClearingRegister::<2>::new();
        assert!(
            reg.clear(clearing_in(Layer::Pending, &[(a, 50), (b, 50)]), &t)
                .is_ok()
        );
    }

    #[test]
    fn a_reservation_does_not_clear_against_a_settled_movement() {
        // Netting the two would report an open item closed while the money had
        // not moved.
        let entry = EntryId::generate();
        let reserved = PostingRef::new(entry, 0);
        let settled = PostingRef::new(entry, 1);
        let mut postings = BTreeMap::new();
        postings.insert(
            reserved,
            Posting::new(
                account(),
                Direction::Debit,
                Eur::from_minor(50),
                Currency::EUR,
            )
            .in_layer(Layer::Pending),
        );
        postings.insert(
            settled,
            Posting::new(
                account(),
                Direction::Credit,
                Eur::from_minor(50),
                Currency::EUR,
            ),
        );
        let t = Table { postings };
        let mut reg = ClearingRegister::<2>::new();
        assert!(matches!(
            reg.clear(
                clearing_in(Layer::Settled, &[(reserved, 50), (settled, 50)]),
                &t
            ),
            Err(ClearingError::WrongLayer { .. })
        ));
    }
}
