//! Konto/Gegenkonto: the deterministic pairing of an n-leg entry's debits
//! with its credits, per (currency, layer), that re-sums to every posting.

use std::collections::VecDeque;

use crate::model::{Amount, Currency, Layer, PostingRecord, Side};

/// One pair of an entry: `amount` of the debit posting `debit` against the
/// credit posting `credit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Pair<const P: u8 = 2> {
    /// The pair's number in the entry, from 0.
    pub ordinal: u16,
    /// The debit posting's index.
    pub debit: u16,
    /// The credit posting's index.
    pub credit: u16,
    /// The amount (positive).
    pub amount: Amount<P>,
}

/// The Konto/Gegenkonto pairs of an entry's postings (in record order).
///
/// Per (currency, layer) group, in order of first appearance:
///
/// 1. each debit, in posting order, pairs with the first unused credit of
///    the identical amount;
/// 2. the remaining debits and credits are merged greedily — the first open
///    debit with the first open credit for the smaller remaining amount,
///    advancing whichever reaches zero.
///
/// The pairs come sorted by (debit index, credit index), numbered from 0.
/// Each posting's pairs sum exactly to its amount, a group of `d` debits
/// and `c` credits has at most `d + c − 1` pairs, no pair joins two
/// currencies or layers, and a reversal pairs as the original swapped.
///
/// ```
/// use doubleentry::export::pairs;
/// use doubleentry::{AccountHandle, Dimensions, Layer, Side, amount, currency::EUR, record::PostingRecord};
///
/// let leg = |handle, side, amount| PostingRecord::<2> {
///     handle: AccountHandle::new(handle).unwrap(),
///     side,
///     amount,
///     currency: EUR,
///     layer: Layer::Settled,
///     dimensions: Dimensions::new(),
/// };
/// // A sales invoice: receivable D 119.00 / revenue C 100.00 / VAT C 19.00.
/// let invoice = [
///     leg(1, Side::Debit, amount!("119.00")),
///     leg(2, Side::Credit, amount!("100.00")),
///     leg(3, Side::Credit, amount!("19.00")),
/// ];
/// let found: Vec<_> = pairs(&invoice).iter().map(|p| (p.debit, p.credit, p.amount)).collect();
/// assert_eq!(found, [(0, 1, amount!("100.00")), (0, 2, amount!("19.00"))]);
/// ```
#[must_use]
pub fn pairs<const P: u8>(postings: &[PostingRecord<P>]) -> Vec<Pair<P>> {
    let mut groups: Vec<Group<P>> = Vec::new();
    for (index, posting) in (0u16..).zip(postings) {
        let key = (posting.currency, posting.layer);
        if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
            group.add(index, posting);
        } else {
            let mut group = Group {
                key,
                debits: Vec::new(),
                credits: Vec::new(),
            };
            group.add(index, posting);
            groups.push(group);
        }
    }
    let mut found: Vec<(u16, u16, Amount<P>)> = Vec::new();
    for group in groups {
        group.pair(&mut found);
    }
    found.sort_by_key(|(debit, credit, _)| (*debit, *credit));
    (0u16..)
        .zip(found)
        .map(|(ordinal, (debit, credit, amount))| Pair {
            ordinal,
            debit,
            credit,
            amount,
        })
        .collect()
}

/// The debits and credits of one (currency, layer), with their indexes.
struct Group<const P: u8> {
    key: (Currency, Layer),
    debits: Vec<(u16, Amount<P>)>,
    credits: Vec<(u16, Amount<P>)>,
}

impl<const P: u8> Group<P> {
    /// Adds the posting at `index`.
    fn add(&mut self, index: u16, posting: &PostingRecord<P>) {
        match posting.side {
            Side::Debit => self.debits.push((index, posting.amount)),
            Side::Credit => self.credits.push((index, posting.amount)),
        }
    }

    /// Appends the group's pairs: exact matches first, then the greedy
    /// merge of what remains.
    fn pair(mut self, found: &mut Vec<(u16, u16, Amount<P>)>) {
        for debit in &mut self.debits {
            let exact = self
                .credits
                .iter_mut()
                .find(|credit| credit.1.is_positive() && credit.1 == debit.1);
            if let Some(credit) = exact {
                found.push((debit.0, credit.0, debit.1));
                debit.1 = Amount::ZERO;
                credit.1 = Amount::ZERO;
            }
        }
        let open = |legs: Vec<(u16, Amount<P>)>| -> VecDeque<(u16, Amount<P>)> {
            legs.into_iter().filter(|leg| leg.1.is_positive()).collect()
        };
        let (mut debits, mut credits) = (open(self.debits), open(self.credits));
        while let (Some(debit), Some(credit)) = (debits.front_mut(), credits.front_mut()) {
            let amount = debit.1.min(credit.1);
            found.push((debit.0, credit.0, amount));
            debit.1 = less(debit.1, amount);
            credit.1 = less(credit.1, amount);
            if !debit.1.is_positive() {
                debits.pop_front();
            }
            if !credit.1.is_positive() {
                credits.pop_front();
            }
        }
    }
}

/// `amount − part` for `0 < part ≤ amount`: total, since the difference
/// lies in `0..amount`.
fn less<const P: u8>(amount: Amount<P>, part: Amount<P>) -> Amount<P> {
    amount.checked_sub(part).unwrap_or(Amount::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountHandle, Dimensions, EUR, USD};

    fn leg(side: Side, minor: i128) -> PostingRecord<2> {
        PostingRecord {
            handle: AccountHandle::new(1).unwrap(),
            side,
            amount: Amount::from_minor(minor).unwrap(),
            currency: EUR,
            layer: Layer::Settled,
            dimensions: Dimensions::new(),
        }
    }

    fn found(postings: &[PostingRecord<2>]) -> Vec<(u16, u16, i128)> {
        pairs(postings)
            .iter()
            .map(|pair| (pair.debit, pair.credit, pair.amount.to_minor()))
            .collect()
    }

    #[test]
    fn exact_matches_come_before_the_greedy_merge() {
        // Two payments against two invoices.
        let legs = [
            leg(Side::Debit, 50),
            leg(Side::Debit, 100),
            leg(Side::Credit, 100),
            leg(Side::Credit, 50),
        ];
        assert_eq!(found(&legs), [(0, 3, 50), (1, 2, 100)]);
        // D 70 A, D 30 B / C 50 X, C 50 Y.
        let legs = [
            leg(Side::Debit, 70),
            leg(Side::Debit, 30),
            leg(Side::Credit, 50),
            leg(Side::Credit, 50),
        ];
        assert_eq!(found(&legs), [(0, 2, 50), (0, 3, 20), (1, 3, 30)]);
    }

    #[test]
    fn one_debit_many_credits_and_repeated_amounts() {
        let legs = [
            leg(Side::Credit, 10),
            leg(Side::Debit, 30),
            leg(Side::Credit, 10),
            leg(Side::Credit, 10),
        ];
        assert_eq!(found(&legs), [(1, 0, 10), (1, 2, 10), (1, 3, 10)]);
        let legs = [
            leg(Side::Debit, 10),
            leg(Side::Debit, 10),
            leg(Side::Credit, 10),
            leg(Side::Credit, 10),
        ];
        assert_eq!(found(&legs), [(0, 2, 10), (1, 3, 10)]);
        let ordinals: Vec<u16> = pairs(&legs).iter().map(|pair| pair.ordinal).collect();
        assert_eq!(ordinals, [0, 1]);
    }

    #[test]
    fn groups_never_mix_currencies_or_layers() {
        let mut dollars = leg(Side::Credit, 100);
        dollars.currency = USD;
        let mut pending = leg(Side::Credit, 100);
        pending.layer = Layer::Pending;
        let legs = [
            leg(Side::Debit, 100),
            dollars,
            leg(Side::Credit, 100),
            pending,
        ];
        assert_eq!(found(&legs), [(0, 2, 100)]);
    }
}
