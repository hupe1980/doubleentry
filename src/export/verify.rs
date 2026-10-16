//! Checking an export without trusting the exporter: every entry rebuilt to
//! its leaf hash, every memo re-committed, every pair re-derived, and the
//! root rebuilt from the leaf rows.

use super::pairs::pairs;
use super::rows::{DocumentRow, EntryRow, ExportRow, HoldKind, PairRow, PostingRow, RateRow};
use crate::log::{Hash, TreeHead, leaf_hash, root_of};
use crate::model::{
    Amount, Document, EntryParts, EntryRecord, HoldAction, MemoCommitment, PostAmount,
    PostingRecord, Rate, Record,
};

/// Why export rows do not verify against a head.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The rows are not an export: out of order, incomplete, or holding a
    /// value no record can have.
    #[error("the rows of position {position} are malformed: {reason}")]
    Malformed {
        /// The position of the record the rows describe.
        position: u64,
        /// What is wrong.
        reason: &'static str,
    },
    /// An entry's rows rebuild to another leaf hash than its leaf row's.
    #[error("the entry at position {position} does not rebuild to its leaf hash")]
    EntryMismatch {
        /// The entry's position.
        position: u64,
    },
    /// A memo's text and salt do not commit to the entry's commitment.
    #[error("the memo of the entry at position {position} does not match its commitment")]
    MemoMismatch {
        /// The entry's position.
        position: u64,
    },
    /// An entry's pair rows are not the pairing of its posting rows.
    #[error("the pairs of the entry at position {position} are not its postings' pairs")]
    PairMismatch {
        /// The entry's position.
        position: u64,
    },
    /// The leaf rows do not rebuild the head: another size or root.
    #[error("the leaf rows do not rebuild the head of size {size}")]
    RootMismatch {
        /// The head's size.
        size: u64,
    },
}

/// `units / 10^(P − exponent)`: an amount at scale `P` in minor units of a
/// currency's exponent; `None` unless exact.
pub(crate) fn to_minor<const P: u8>(units: i128, exponent: u8) -> Option<i128> {
    let factor = 10i128.checked_pow(u32::from(P.checked_sub(exponent)?))?;
    (units.checked_rem(factor)? == 0).then(|| units.checked_div(factor))?
}

/// `minor × 10^(P − exponent)`: minor units of a currency's exponent as an
/// amount at scale `P`; `None` when it does not fit.
pub(crate) fn from_minor<const P: u8>(minor: i128, exponent: u8) -> Option<Amount<P>> {
    let factor = 10i128.checked_pow(u32::from(P.checked_sub(exponent)?))?;
    Amount::from_minor(minor.checked_mul(factor)?).ok()
}

/// The exponent a partial post amount is written in: that of the entry's
/// first posting row; the ledger scale `P` without postings.
pub(crate) fn post_exponent<const P: u8>(postings: &[PostingRow]) -> u8 {
    postings.first().map_or(P, |row| row.exponent)
}

/// The entry record an entry's rows describe.
fn record<const P: u8>(
    entry: &EntryRow,
    postings: &[PostingRow],
    documents: &[DocumentRow],
    rates: &[RateRow],
) -> Result<EntryRecord<P>, VerifyError> {
    let malformed = |reason| VerifyError::Malformed {
        position: entry.position,
        reason,
    };
    let counted = postings.len() == usize::from(entry.posting_count)
        && documents.len() == usize::from(entry.document_count)
        && rates.len() == usize::from(entry.rate_count);
    if !counted {
        return Err(malformed("the child rows do not match the entry's counts"));
    }
    let in_order = (0u16..).zip(postings).all(|(i, row)| row.index == i)
        && (0u16..).zip(documents).all(|(i, row)| row.ordinal == i)
        && (0u16..).zip(rates).all(|(i, row)| row.ordinal == i);
    if !in_order {
        return Err(malformed("the child rows are not numbered in order"));
    }
    let post_exponent = post_exponent::<P>(postings);
    let postings = postings
        .iter()
        .map(|row| {
            Ok(PostingRecord {
                handle: row.account,
                side: row.side,
                amount: from_minor(row.minor_units, row.exponent)
                    .ok_or_else(|| malformed("a posting amount does not fit the scale"))?,
                currency: row.currency,
                layer: row.layer,
                dimensions: row.dimensions.clone(),
            })
        })
        .collect::<Result<Vec<_>, VerifyError>>()?;
    let documents = documents
        .iter()
        .map(|row| {
            let document = Document::new(row.doc_type.clone(), row.reference.clone())
                .map_err(|_| malformed("a document reference is invalid"))?;
            let document = match row.date {
                Some(date) => document.dated(date),
                None => document,
            };
            Ok(match row.sha256 {
                Some(digest) => document.with_sha256(digest),
                None => document,
            })
        })
        .collect::<Result<Vec<_>, VerifyError>>()?;
    let rates = rates
        .iter()
        .map(|row| {
            Rate::new(row.base, row.quote, row.mantissa, row.exponent)
                .map_err(|_| malformed("a rate is invalid"))
        })
        .collect::<Result<Vec<_>, VerifyError>>()?;
    let hold = match (entry.hold_action, entry.hold_target) {
        (None, None) => None,
        (Some(HoldKind::Open), None) => Some(HoldAction::Open {
            expires: entry.hold_expires,
        }),
        (Some(HoldKind::Post), Some(hold)) => Some(HoldAction::Post {
            hold,
            amount: match entry.post_amount_minor {
                None => PostAmount::Full,
                Some(minor) => PostAmount::Partial(
                    from_minor(minor, post_exponent)
                        .ok_or_else(|| malformed("a post amount does not fit the scale"))?,
                ),
            },
        }),
        (Some(HoldKind::Void), Some(hold)) => Some(HoldAction::Void { hold }),
        (Some(HoldKind::Expire), Some(hold)) => Some(HoldAction::Expire { hold }),
        _ => return Err(malformed("the hold columns contradict each other")),
    };
    EntryRecord::new(EntryParts {
        key: entry.key.clone(),
        booking_date: entry.booking_date,
        value_date: entry.value_date,
        captured_at: entry.captured_at,
        kind: entry.kind.clone(),
        memo: entry.memo_commitment,
        documents,
        postings,
        reverses: entry.reverses,
        hold,
        rates,
    })
    .map_err(|_| malformed("the entry is not a valid record"))
}

/// The canonical leaf bytes (`0x06 ‖ canonical(entry)`) an entry's rows
/// describe — the bytes whose RFC 6962 leaf hash is the entry's.
///
/// Posting amounts are rebuilt as `minor_units × 10^(P − exponent)`;
/// documents and rates in their ordinal order.
///
/// # Errors
///
/// [`VerifyError::Malformed`] when the rows are incomplete, out of order or
/// hold a value no entry record can have.
pub fn rebuild_entry<const P: u8>(
    entry: &EntryRow,
    postings: &[PostingRow],
    documents: &[DocumentRow],
    rates: &[RateRow],
) -> Result<Vec<u8>, VerifyError> {
    Ok(Record::Entry(record::<P>(entry, postings, documents, rates)?).to_leaf_bytes())
}

/// An entry row and its child rows, collected in export order.
struct Group {
    entry: EntryRow,
    postings: Vec<PostingRow>,
    pairs: Vec<PairRow>,
    documents: Vec<DocumentRow>,
    rates: Vec<RateRow>,
}

impl Group {
    /// Checks the group against the leaf rows read so far.
    fn check<const P: u8>(&self, leaves: &[Hash]) -> Result<(), VerifyError> {
        let position = self.entry.position;
        let record = record::<P>(&self.entry, &self.postings, &self.documents, &self.rates)?;
        let rebuilt = leaf_hash(&Record::Entry(record.clone()).to_leaf_bytes());
        let leaf = usize::try_from(position).ok().and_then(|at| leaves.get(at));
        if rebuilt != self.entry.leaf_hash || leaf != Some(&rebuilt) {
            return Err(VerifyError::EntryMismatch { position });
        }
        if let (Some(commitment), Some(text), Some(salt)) = (
            self.entry.memo_commitment,
            &self.entry.memo_text,
            &self.entry.memo_salt,
        ) && MemoCommitment::compute(salt, text) != commitment
        {
            return Err(VerifyError::MemoMismatch { position });
        }
        let expected: Vec<_> = pairs(&record.parts().postings)
            .iter()
            .map(|pair| (pair.ordinal, pair.debit, pair.credit, pair.amount))
            .collect();
        let found: Vec<_> = self
            .pairs
            .iter()
            .map(|row| {
                (
                    row.ordinal,
                    row.debit_index,
                    row.credit_index,
                    from_minor::<P>(row.minor_units, row.exponent),
                )
            })
            .map(|(ordinal, debit, credit, amount)| amount.map(|a| (ordinal, debit, credit, a)))
            .collect::<Option<_>>()
            .unwrap_or_default();
        if found != expected {
            return Err(VerifyError::PairMismatch { position });
        }
        Ok(())
    }
}

/// Checks export rows against the head they were pinned to: the leaf rows
/// must be every record's, in order, and rebuild `head`'s root; every entry
/// must rebuild from its rows to its leaf hash, its pair rows must be its
/// postings' pairs, and every memo whose text and salt are present must
/// re-commit to its commitment.
///
/// A scoped export verifies the same way: its leaf rows are always complete.
///
/// ```
/// use doubleentry::export::{ExportScope, verify};
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
///         .credit("Income:Sales", amount!("1190.00"), EUR)
///         .memo("first sale"),
/// )?;
/// let head = books.head();
/// let rows = books.export(ExportScope::All, Some(head))?.collect::<Result<Vec<_>>>()?;
/// verify(rows, &head)?; // a ledger of scale P = 2; else verify_with_precision::<P>
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// The first [`VerifyError`] found.
pub fn verify(
    rows: impl IntoIterator<Item = ExportRow>,
    head: &TreeHead,
) -> Result<(), VerifyError> {
    verify_with_precision::<2>(rows, head)
}

/// [`verify`] for a ledger of scale `P` other than 2.
///
/// # Errors
///
/// The first [`VerifyError`] found.
pub fn verify_with_precision<const P: u8>(
    rows: impl IntoIterator<Item = ExportRow>,
    head: &TreeHead,
) -> Result<(), VerifyError> {
    let mut leaves: Vec<Hash> = Vec::new();
    let mut group: Option<Group> = None;
    let misplaced = |position, reason| VerifyError::Malformed { position, reason };
    for row in rows {
        match row {
            ExportRow::Posting(row) => match &mut group {
                Some(group) if group.entry.entry == row.entry => group.postings.push(row),
                _ => return Err(misplaced(row.position, "a posting row outside its entry")),
            },
            ExportRow::Pair(row) => match &mut group {
                Some(group) if group.entry.entry == row.entry => group.pairs.push(row),
                _ => return Err(misplaced(row.position, "a pair row outside its entry")),
            },
            ExportRow::Document(row) => match &mut group {
                Some(group) if group.entry.entry == row.entry => group.documents.push(row),
                _ => return Err(misplaced(row.position, "a document row outside its entry")),
            },
            ExportRow::Rate(row) => match &mut group {
                Some(group) if group.entry.entry == row.entry => group.rates.push(row),
                _ => return Err(misplaced(row.position, "a rate row outside its entry")),
            },
            other => {
                if let Some(done) = group.take() {
                    done.check::<P>(&leaves)?;
                }
                match other {
                    ExportRow::Leaf(leaf) => {
                        if usize::try_from(leaf.position).ok() != Some(leaves.len()) {
                            return Err(misplaced(leaf.position, "a leaf row out of order"));
                        }
                        leaves.push(leaf.leaf_hash);
                    }
                    ExportRow::Entry(entry) => {
                        group = Some(Group {
                            entry,
                            postings: Vec::new(),
                            pairs: Vec::new(),
                            documents: Vec::new(),
                            rates: Vec::new(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(done) = group {
        done.check::<P>(&leaves)?;
    }
    if u64::try_from(leaves.len()).ok() != Some(head.size) || root_of(&leaves) != head.root {
        return Err(VerifyError::RootMismatch { size: head.size });
    }
    Ok(())
}
