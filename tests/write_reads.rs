//! The reads a write makes do not grow with history, on the in-memory and
//! the SQLite store, counted from the stores' `write` spans.
//! PoC: `perf_history_scaling`. One test in its own binary: the span
//! collector is process-wide.
#![cfg(all(feature = "tracing", feature = "testing"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod store_checks;

use doubleentry::store::MemoryStore;

#[tokio::test]
async fn regress_qs7_write_reads_do_not_grow_with_history() {
    store_checks::regress_qs7_write_reads_do_not_grow_with_history(MemoryStore::new(), 10_000)
        .await;
    #[cfg(feature = "sqlite")]
    {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("books.db").display());
        let store = doubleentry::store::SqlStore::<doubleentry::store::Sqlite>::sqlite(&url)
            .await
            .unwrap();
        store_checks::regress_qs7_write_reads_do_not_grow_with_history(store, 10_000).await;
    }
}
