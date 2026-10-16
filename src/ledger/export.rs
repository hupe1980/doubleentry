//! [`ExportPages`]: a [`Ledger`]'s export, read page by page.

use std::collections::VecDeque;

use super::Ledger;
use crate::PageRead;
use crate::core::MemoPlain;
use crate::error::{Error, IntegrityError, IntegrityKind};
use crate::export::with_memo;
use crate::export::{ExportRow, Exporter};
use crate::store::{LeafRead, Memo, Store};

/// A [`Ledger`]'s export ([`Ledger::export`]): its rows, a page at a time,
/// pinned to the head it was opened at while the ledger grows.
#[derive(Debug)]
pub struct ExportPages<'a, S, const P: u8 = 2> {
    ledger: &'a Ledger<S, P>,
    exporter: Exporter<P>,
    next: u64,
    bound: u64,
    rows: VecDeque<ExportRow>,
}

impl<'a, S: Store<P>, const P: u8> ExportPages<'a, S, P> {
    /// The export of the records of `ledger` below `bound`.
    pub(crate) const fn new(ledger: &'a Ledger<S, P>, exporter: Exporter<P>, bound: u64) -> Self {
        Self {
            ledger,
            exporter,
            next: 0,
            bound,
            rows: VecDeque::new(),
        }
    }

    /// The next page of at most `limit` rows (`limit` is clamped to
    /// `1..=1000`); `None` once every row was read. Page sizes change where
    /// pages break, never the rows.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] for a record the rule core would not have
    /// written, or a leaf missing below the head; [`Error::Backend`].
    pub async fn next_page(&mut self, limit: u32) -> Result<Option<Vec<ExportRow>>, Error> {
        let limit = limit.clamp(1, PageRead::<()>::MAX_LIMIT);
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX);
        while self.rows.len() < wanted && self.next < self.bound {
            self.read(limit).await?;
        }
        if self.rows.is_empty() {
            return Ok(None);
        }
        let take = wanted.min(self.rows.len());
        Ok(Some(self.rows.drain(..take).collect()))
    }

    /// Feeds the next leaves (at most `limit`) to the exporter.
    async fn read(&mut self, limit: u32) -> Result<(), Error> {
        let store = &self.ledger.store;
        let page = store
            .read_leaves(&LeafRead {
                from: self.next,
                through: Some(self.bound),
                kinds: None,
                limit,
            })
            .await?;
        if page.items.is_empty() {
            return Err(IntegrityError::new(IntegrityKind::MissingRecord, Some(self.next)).into());
        }
        for leaf in page.items {
            let record = leaf.record::<P>()?;
            let memo = match with_memo(&self.ledger.ledger, &record) {
                Some(id) => match store.memo(id).await? {
                    Some(Memo::Kept { text, salt }) => Some(MemoPlain { salt, text }),
                    _ => None,
                },
                None => None,
            };
            self.rows
                .extend(self.exporter.feed(leaf.position, &record, memo.as_ref())?);
            self.next = leaf.position.saturating_add(1);
        }
        Ok(())
    }
}
