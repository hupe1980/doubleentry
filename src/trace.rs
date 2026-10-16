//! The façade spans (feature `tracing`): what an operation records on its
//! span once it has run. Never an amount, a memo, an account path, a
//! document reference or more than 64 bytes of a key; no events — errors are
//! returned, not logged.

use tracing::Span;

use crate::core::{AuditReport, Recorded, SealReceipt};
use crate::error::Error;
use crate::model::Entry;

/// The longest key prefix a span carries, in bytes.
const KEY_BYTES: usize = 64;

/// An idempotency key as a span field: its first 64 bytes, lossy UTF-8.
pub(crate) fn key(key: &str) -> String {
    let bytes = key.as_bytes();
    String::from_utf8_lossy(bytes.get(..KEY_BYTES).unwrap_or(bytes)).into_owned()
}

/// A draft's key as a span field (empty when it has none).
pub(crate) fn draft_key<const P: u8>(entry: &Entry<P>) -> String {
    entry
        .key()
        .map_or_else(String::new, |found| key(found.as_str()))
}

/// Records how a failed operation ended: `rejected` with its code, or
/// `failed`.
fn failed(span: &Span, error: &Error) {
    match error.rejection() {
        Some(rejection) => {
            span.record("outcome", "rejected");
            span.record("code", rejection.code());
        }
        None => {
            span.record("outcome", "failed");
        }
    }
}

/// Records a `record`: `new` or `replay` at its position, or the failure.
pub(crate) fn recorded(result: Result<&Recorded, &Error>) {
    let span = Span::current();
    match result {
        Ok(recorded) => {
            span.record("outcome", if recorded.replayed { "replay" } else { "new" });
            span.record("position", recorded.position);
        }
        Err(error) => failed(&span, error),
    }
}

/// Records a `record_batch`: `new` unless every draft replayed, the highest
/// position, or the failure (with the refused draft's index).
pub(crate) fn batch(result: Result<&Vec<Recorded>, &Error>) {
    let span = Span::current();
    match result {
        Ok(recorded) => {
            let new = recorded.iter().any(|receipt| !receipt.replayed);
            span.record("outcome", if new { "new" } else { "replay" });
            if let Some(last) = recorded.iter().map(|receipt| receipt.position).max() {
                span.record("position", last);
            }
        }
        Err(error) => {
            if let Error::BatchRejected { index, .. } = error {
                span.record("index", *index);
            }
            failed(&span, error);
        }
    }
}

/// Records a `clear` or `reset_clearing`: the record's position (a replay
/// is not told apart), or the failure.
pub(crate) fn cleared(result: Result<&u64, &Error>) {
    let span = Span::current();
    match result {
        Ok(position) => {
            span.record("outcome", "ok");
            span.record("position", *position);
        }
        Err(error) => failed(&span, error),
    }
}

/// Records the size a proof was built for.
pub(crate) fn proved(size: u64) {
    Span::current().record("size", size);
}

/// Records a `seal_period`: the seal's position, or the failure.
pub(crate) fn sealed<const P: u8>(result: Result<&SealReceipt<P>, &Error>) {
    let span = Span::current();
    match result {
        Ok(receipt) => {
            span.record("outcome", "ok");
            span.record("position", receipt.position);
        }
        Err(error) => failed(&span, error),
    }
}

/// Records an `audit` or `to_journal`: the records replayed, or the
/// failure.
pub(crate) fn audited(result: Result<&AuditReport, &Error>) {
    let span = Span::current();
    match result {
        Ok(report) => {
            span.record("outcome", "ok");
            span.record("records", report.size);
        }
        Err(error) => failed(&span, error),
    }
}
