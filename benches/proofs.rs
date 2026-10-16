//! Proof and audit costs per store, recorded and not
//! gated, through the async façade `Ledger` on `MemoryStore`, on a SQLite
//! file (WAL, `synchronous=FULL`) and — with the `postgres` feature and a
//! Docker daemon — on PostgreSQL 17 (`postgres:17-alpine`, default
//! configuration, one container per bench process).
//!
//! Each store gets one ledger of 10⁶ two-posting entries (built with
//! `record_batch` of 1 000 drafts, all in January 2026, which is then
//! sealed), built only when criterion's filter selects one of its
//! scenarios, so `just bench memory` and `just bench sqlite` never start
//! Docker:
//!
//! | id | scenario |
//! |---|---|
//! | `proofs/<store>/P1-inclusion` | `prove_entry` of an entry against the head (1 024 entries spread over the log, in turn) |
//! | `proofs/<store>/P2-consistency` | `prove_consistency` from an earlier size (1 024 sizes spread over the log, in turn) to the head |
//! | `proofs/<store>/P3-sealed-balance` | `prove_balance` of `Assets:Cash`, EUR, settled, in the sealed period `2026-01` |
//! | `proofs-audit/<store>/A1-audit` | `audit()`: the full replay of the log (the entries plus 19 setup and seal records), reported as entries/s |
//!
//! `<store>` is `memory`, `sqlite` or `postgres`. `DOUBLEENTRY_BENCH_ENTRIES`
//! replaces the 10⁶ entries (smoke runs); the size used and the build time
//! are printed.
//!
//! `just bench proofs` (all stores) · `just bench 'proofs.*/sqlite'`, i.e.
//! `cargo bench --locked --all-features --bench proofs -- <filter>`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::cell::{Cell, OnceCell};
use std::time::{Duration, Instant};

use criterion::{Criterion, SamplingMode, Throughput};
use doubleentry::store::{MemoryStore, SqlStore, Sqlite, Store};
use doubleentry::{
    AccountKind, Amount, Entry, EntryId, Layer, Ledger, LedgerId, MemoSalt, currency::EUR, date,
};
use tokio::runtime::Runtime;

/// The drafts per `record_batch` call while building.
const BATCH: u64 = 1_000;

/// How many entries (and earlier sizes) P1 and P2 take in turn.
const SPREAD: u64 = 1_024;

/// A store the benches can make a fresh ledger in.
trait Bench: Store<2> + Clone + Send + Sync + 'static {}
impl<S: Store<2> + Clone + Send + Sync + 'static> Bench for S {}

/// The entries per ledger: 10⁶, or `DOUBLEENTRY_BENCH_ENTRIES`.
fn entries() -> u64 {
    std::env::var("DOUBLEENTRY_BENCH_ENTRIES").map_or(1_000_000, |n| {
        n.parse().expect("DOUBLEENTRY_BENCH_ENTRIES: a count")
    })
}

/// A built ledger: the entries P1 proves, the sizes P2 proves from, and the
/// head size.
struct Built<S> {
    books: Ledger<S>,
    ids: Vec<EntryId>,
    sizes: Vec<u64>,
    size: u64,
}

/// `entries` two-posting sales in January 2026 in a fresh ledger, January
/// sealed.
async fn build<S: Bench>(store: S, entries: u64) -> Built<S> {
    let books = Ledger::new(store, LedgerId::new("example.com/bench").unwrap())
        .await
        .unwrap()
        .with_salt_source(|| MemoSalt([1; 16]));
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).await.unwrap();
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Assets:Bank", AccountKind::Asset),
        ("Income:Sales", AccountKind::Income),
        ("Income:Fees", AccountKind::Income),
    ] {
        books.open(path, kind, on).await.unwrap();
    }
    books.define_months(2026, 1..=12).await.unwrap();
    let amount: Amount = Amount::from_minor(1_00).unwrap();
    let step = (entries / SPREAD).max(1);
    let mut ids = Vec::new();
    let mut sizes = Vec::new();
    let mut n = 0;
    while n < entries {
        let size = std::env::var("XB")
            .map_or(BATCH, |b| b.parse().unwrap())
            .min(entries - n);
        let drafts = (n..n + size).map(|n| {
            let (debit, credit) = match n % 4 {
                0 => ("Assets:Cash", "Income:Sales"),
                1 => ("Assets:Bank", "Income:Sales"),
                2 => ("Assets:Cash", "Income:Fees"),
                _ => ("Assets:Bank", "Income:Fees"),
            };
            Entry::new(format!("e-{n}"), date!("2026-01-15"))
                .debit(debit, amount, EUR)
                .credit(credit, amount, EUR)
        });
        let recorded = books.record_batch(drafts).await.unwrap();
        for (k, receipt) in (n..).zip(recorded) {
            if k % step == 0 {
                ids.push(receipt.id);
                sizes.push(receipt.position + 1);
            }
        }
        n += size;
    }
    books.seal_period("2026-01").await.unwrap();
    let size = books.head().await.unwrap().size;
    // Genesis, EUR, four accounts, twelve months, the entries, the seal.
    assert_eq!(size, entries + 19);
    Built {
        books,
        ids,
        sizes,
        size,
    }
}

/// Where fresh stores come from.
struct Stores<'a> {
    rt: &'a Runtime,
    dir: tempfile::TempDir,
    #[cfg(feature = "postgres")]
    postgres: postgres::Server,
}

#[cfg(feature = "postgres")]
mod postgres {
    use std::cell::{Cell, OnceCell};

    use doubleentry::store::{Postgres, SqlOptions, SqlStore};
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, ImageExt};
    use testcontainers_modules::postgres::Postgres as Image;
    use tokio::runtime::Runtime;

    /// `postgres:17-alpine`, started on the first store asked for.
    #[derive(Default)]
    pub struct Server {
        running: OnceCell<(ContainerAsync<Image>, String)>,
        schemas: Cell<u32>,
    }

    impl Server {
        /// A fresh schema on the (lazily started) server.
        pub fn store(&self, rt: &Runtime) -> SqlStore<Postgres> {
            let (_, url) = self.running.get_or_init(|| {
                rt.block_on(async {
                    let container = Image::default()
                        .with_tag("17-alpine")
                        .start()
                        .await
                        .expect("the PostgreSQL benchmarks need a running Docker daemon");
                    let port = container.get_host_port_ipv4(5432).await.unwrap();
                    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
                    (container, url)
                })
            });
            let n = self.schemas.get();
            self.schemas.set(n + 1);
            let options = SqlOptions {
                schema: format!("bench_{n}"),
                ..SqlOptions::default()
            };
            rt.block_on(SqlStore::postgres_with(url, options)).unwrap()
        }

        /// Removes the container, if one was started.
        pub fn stop(self, rt: &Runtime) {
            if let Some((container, _)) = self.running.into_inner() {
                rt.block_on(container.rm()).unwrap();
            }
        }
    }
}

/// P1–P3 and A1 for one store; `fresh` makes the empty store, called only
/// when the filter selects one of the store's scenarios.
fn scenarios<S: Bench>(c: &mut Criterion, rt: &Runtime, store: &str, fresh: &dyn Fn() -> S) {
    let lazy: OnceCell<Built<S>> = OnceCell::new();
    let built = || {
        lazy.get_or_init(|| {
            let entries = entries();
            let started = Instant::now();
            let built = rt.block_on(build(fresh(), entries));
            println!(
                "proofs {store}: {entries} entries ({} records) built and January sealed in {:.1?}",
                built.size,
                started.elapsed()
            );
            built
        })
    };

    let mut group = c.benchmark_group(format!("proofs/{store}"));
    let turn = Cell::new(0usize);
    group.bench_function("P1-inclusion", |b| {
        let built = built();
        b.to_async(rt).iter(|| {
            let id = built.ids[turn.replace(turn.get() + 1) % built.ids.len()];
            async move { built.books.prove_entry(id).await.unwrap() }
        });
    });
    let turn = Cell::new(0usize);
    group.bench_function("P2-consistency", |b| {
        let built = built();
        b.to_async(rt).iter(|| {
            let from = built.sizes[turn.replace(turn.get() + 1) % built.sizes.len()];
            async move {
                built
                    .books
                    .prove_consistency(from, built.size)
                    .await
                    .unwrap()
            }
        });
    });
    group.bench_function("P3-sealed-balance", |b| {
        let built = built();
        b.to_async(rt).iter(|| async {
            built
                .books
                .prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-01")
                .await
                .unwrap()
        });
    });
    group.finish();

    let mut group = c.benchmark_group(format!("proofs-audit/{store}"));
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .throughput(Throughput::Elements(entries()));
    group.bench_function("A1-audit", |b| {
        let built = built();
        b.to_async(rt).iter(|| async {
            let report = built.books.audit().await.unwrap();
            assert_eq!(report.size, built.size);
            report
        });
    });
    group.finish();
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut c = Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4))
        .configure_from_args();
    let stores = Stores {
        rt: &rt,
        dir: tempfile::tempdir().unwrap(),
        #[cfg(feature = "postgres")]
        postgres: postgres::Server::default(),
    };
    scenarios(&mut c, &rt, "memory", &MemoryStore::new);
    scenarios(&mut c, &rt, "sqlite", &|| {
        let url = format!(
            "sqlite://{}?mode=rwc",
            stores.dir.path().join("proofs.db").display()
        );
        stores
            .rt
            .block_on(SqlStore::<Sqlite>::sqlite(&url))
            .unwrap()
    });
    #[cfg(feature = "postgres")]
    scenarios(&mut c, &rt, "postgres", &|| stores.postgres.store(&rt));
    c.final_summary();
    #[cfg(feature = "postgres")]
    stores.postgres.stop(&rt);
}
