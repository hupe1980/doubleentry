//! Stage 1 for entries: what a draft (or a stored entry record) breaks on its
//! own, before any state is read.
//!
//! Every violation is collected; the stage driver returns the one with the
//! lowest code, and [`validate`](super::validate) returns all of them.

use std::collections::BTreeMap;

use super::holds;
use crate::error::{LabelField, Rejection, ScaledAmount};
use crate::model::{
    Amount, BuildError, Currency, Entry, EntryRecord, Field, HoldAction, Layer, MAX_LEAF_BYTES,
    Side, ValueError,
};

/// A posting index as rejections report it (indexes above `u16::MAX` only
/// occur in drafts that are refused for their posting count anyway).
pub(crate) fn index(position: usize) -> u16 {
    u16::try_from(position).unwrap_or(u16::MAX)
}

/// The refusal of a record constructor. Its only reason left is the record's
/// size: the counts and orders it also checks are excluded by stage 1 and by
/// construction, and `bytes` is the size computed beforehand.
pub(crate) fn record_error(error: ValueError, bytes: usize) -> Rejection {
    match error {
        ValueError::TooLarge { max, found } => Rejection::RecordTooLarge { bytes: found, max },
        _ => Rejection::RecordTooLarge {
            bytes,
            max: MAX_LEAF_BYTES,
        },
    }
}

/// The rejection of a builder argument that failed validation.
pub(crate) fn build_error(error: &BuildError) -> Rejection {
    let reason = error.error;
    match error.field {
        Field::Key => Rejection::InvalidKey { reason },
        Field::Posting(posting) => Rejection::InvalidPath {
            posting: Some(index(posting)),
            reason,
        },
        Field::Dimension { posting } => Rejection::InvalidDimensions {
            posting: index(posting),
            reason,
        },
        Field::Kind => Rejection::InvalidLabel {
            field: LabelField::Kind,
            reason,
        },
        Field::Memo => Rejection::InvalidMemo { reason },
    }
}

/// Every structural violation of a draft, in code order.
pub(crate) fn entry<const P: u8>(draft: &Entry<P>) -> Vec<Rejection> {
    let mut found: Vec<Rejection> = draft.build_errors().iter().map(build_error).collect();
    found.extend(postings(
        draft.get_hold(),
        draft
            .postings()
            .iter()
            .map(|p| (p.currency(), p.get_layer(), p.side(), p.amount())),
    ));
    if draft.documents().len() > Entry::<P>::MAX_DOCUMENTS {
        found.push(Rejection::TooManyDocuments {
            count: draft.documents().len(),
            max: Entry::<P>::MAX_DOCUMENTS,
        });
    }
    if draft.rates().len() > Entry::<P>::MAX_RATES {
        found.push(Rejection::TooManyRates {
            count: draft.rates().len(),
            max: Entry::<P>::MAX_RATES,
        });
    }
    let bytes = draft.encoded_len();
    if bytes > MAX_LEAF_BYTES {
        found.push(Rejection::RecordTooLarge {
            bytes,
            max: MAX_LEAF_BYTES,
        });
    }
    if draft.memo_text().is_some() && draft.memo_salt().is_none() {
        found.push(Rejection::MemoSaltMissing);
    }
    let layers: Vec<(Layer, Side)> = draft
        .postings()
        .iter()
        .map(|p| (p.get_layer(), p.side()))
        .collect();
    found.extend(holds::structural(
        draft.get_hold(),
        draft.booking_date(),
        draft.get_reverses(),
        &layers,
        false,
    ));
    found.sort_by_key(Rejection::code);
    found
}

/// Every structural violation of a stored entry record (replay): what its
/// type does not already guarantee.
pub(crate) fn record<const P: u8>(entry: &EntryRecord<P>) -> Vec<Rejection> {
    let parts = entry.parts();
    let mut found = postings(
        parts.hold.as_ref(),
        parts
            .postings
            .iter()
            .map(|p| (p.currency, p.layer, p.side, p.amount)),
    );
    let layers: Vec<(Layer, Side)> = parts.postings.iter().map(|p| (p.layer, p.side)).collect();
    found.extend(holds::structural(
        parts.hold.as_ref(),
        parts.booking_date,
        parts.reverses,
        &layers,
        true,
    ));
    found.sort_by_key(Rejection::code);
    found
}

/// The posting rules: count, positive amounts, balance per (currency,
/// layer).
fn postings<const P: u8>(
    hold: Option<&HoldAction<P>>,
    legs: impl ExactSizeIterator<Item = (Currency, Layer, Side, Amount<P>)>,
) -> Vec<Rejection> {
    let mut found = Vec::new();
    let count = legs.len();
    if count < 2 && !holds::is_resolution(hold) {
        found.push(Rejection::TooFewPostings { count });
    }
    if count > Entry::<P>::MAX_POSTINGS {
        found.push(Rejection::TooManyPostings {
            count,
            max: Entry::<P>::MAX_POSTINGS,
        });
    }
    // Per (currency, layer): the debit and credit totals, `None` once a
    // total leaves the amount range.
    type Totals<const P: u8> = (Option<Amount<P>>, Option<Amount<P>>);
    let mut totals: BTreeMap<(Currency, Layer), Totals<P>> = BTreeMap::new();
    for (posting, (currency, layer, side, amount)) in legs.enumerate() {
        if !amount.is_positive() {
            found.push(Rejection::NonPositiveAmount {
                posting: index(posting),
            });
        }
        let (debit, credit) = totals
            .entry((currency, layer))
            .or_insert((Some(Amount::ZERO), Some(Amount::ZERO)));
        let total = match side {
            Side::Debit => debit,
            Side::Credit => credit,
        };
        *total = total.and_then(|sum| sum.checked_add(amount).ok());
    }
    let mut unbalanced = None;
    for ((currency, layer), totals) in totals {
        match totals {
            (Some(debit), Some(credit)) => {
                if debit != credit && unbalanced.is_none() {
                    unbalanced = Some(Rejection::Unbalanced {
                        currency,
                        layer,
                        debit: ScaledAmount::from(debit),
                        credit: ScaledAmount::from(credit),
                    });
                }
            }
            _ => found.push(Rejection::TotalOutOfRange { currency, layer }),
        }
    }
    found.extend(unbalanced);
    found
}
