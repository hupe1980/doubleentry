//! Write throughput per store, recorded and not
//! gated: entries per second through the async façade `Ledger` on
//! `MemoryStore`, on a SQLite file (WAL, `synchronous=FULL`, the store's own
//! settings) and — with the `postgres` feature and a Docker daemon — on
//! PostgreSQL 17 (`postgres:17-alpine`, default configuration) in one
//! container per bench process, one schema per scenario.
//!
//! Every scenario books into its own fresh ledger (EUR, nine accounts, the
//! months of 2026), built only when criterion's filter selects it, so
//! `just bench memory` and `just bench sqlite` never start Docker:
//!
//! | id | scenario |
//! |---|---|
//! | `throughput/<store>/S1-2-posting` | a 2-posting entry, one `record` call each |
//! | `throughput/<store>/S2-8-posting-2-dimensions` | an 8-posting entry, every posting with two dimensions, one `record` call each |
//! | `throughput/<store>/S3-batch/{1,10,100}` | `record_batch` of 1, 10 and 100 two-posting drafts (one atomic batch per call) |
//! | `throughput/<store>/S4-4-tasks` | four tokio tasks recording 2-posting entries into one ledger (the single writer per ledger) |
//! | `throughput-limit/<store>/S5-limited-draw/{10000,100000}` | one draw (`record`) on a limited account (`NoDebitBalance`) whose history already holds 10⁴ resp. 10⁵ draws |
//!
//! `<store>` is `memory`, `sqlite` or `postgres`; criterion reports
//! entries/s (`Throughput::Elements`) for S1–S4. S5 is not run through
//! criterion's adaptive loop, which would add tens of thousands of draws to
//! a history of 10⁴: after 100 untimed draws it times 11 batches of 40
//! draws per history (+5.4% on 10⁴, +0.5% on 10⁵) and prints the per-draw
//! median with min and max, entries/s, and the ratio of the two medians —
//! the maintained balance keeps it near 1 (published bound
//! ≤ 1.5 on a quiet machine). S5 honours the same filter (`--exact` and
//! `.*` included; other regex syntax is matched literally).
//!
//! `just bench throughput` (all stores) · `just bench memory` ·
//! `just bench sqlite` · `just bench postgres`, i.e.
//! `cargo bench --locked --all-features --bench throughput -- <filter>`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::cell::{Cell, OnceCell};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput};
use doubleentry::store::{MemoryStore, SqlStore, Sqlite, Store};
use doubleentry::{
    AccountKind, AccountSpec, Amount, BalanceLimit, Entry, Ledger, LedgerId, MemoSalt, Posting,
    currency::EUR, date,
};
use tokio::runtime::Runtime;

/// S5's untimed draws before the batches.
const S5_WARM_UP: u64 = 100;

/// S5's timed batches.
const S5_BATCHES: u64 = 11;

/// The draws in one S5 batch.
const S5_DRAWS: u64 = 40;

/// The histories S5 compares.
const S5_HISTORIES: [u64; 2] = [10_000, 100_000];

/// The drafts per `record_batch` call when building a history.
const BUILD_BATCH: u64 = 1_000;

/// A store the benches can make fresh ledgers in.
trait Bench: Store<2> + Clone + Send + Sync + 'static {}
impl<S: Store<2> + Clone + Send + Sync + 'static> Bench for S {}

fn one() -> Amount {
    Amount::from_minor(1_00).unwrap()
}

/// A fresh ledger in `store`: EUR, the accounts every scenario books on,
/// the limited wallet, the months of 2026.
async fn books<S: Bench>(store: S) -> Ledger<S> {
    let books = Ledger::new(store, LedgerId::new("example.com/bench").unwrap())
        .await
        .unwrap()
        .with_salt_source(|| MemoSalt([1; 16]));
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).await.unwrap();
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Assets:Bank", AccountKind::Asset),
        ("Assets:Card", AccountKind::Asset),
        ("Expenses:Rent", AccountKind::Expense),
        ("Income:Sales", AccountKind::Income),
        ("Income:Fees", AccountKind::Income),
        ("Income:Interest", AccountKind::Income),
        ("Income:Other", AccountKind::Income),
    ] {
        books.open(path, kind, on).await.unwrap();
    }
    books
        .register(
            AccountSpec::new("Liabilities:Wallet", AccountKind::Liability, on)
                .limit(BalanceLimit::NoDebitBalance),
        )
        .await
        .unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    books
}

/// S1/S3/S4: a 2-posting sale, key `{prefix}-{n}`.
fn two_posting(prefix: &str, n: u64) -> Entry {
    let debit = if n.is_multiple_of(2) {
        "Assets:Cash"
    } else {
        "Assets:Bank"
    };
    Entry::new(format!("{prefix}-{n}"), date!("2026-06-15"))
        .debit(debit, one(), EUR)
        .credit("Income:Sales", one(), EUR)
}

/// S2: four debits and four credits, each with two dimensions.
fn eight_posting(n: u64) -> Entry {
    let regions = ["eu", "us", "apac"];
    let channels = ["web", "store"];
    let mut entry = Entry::new(format!("s2-{n}"), date!("2026-06-15"));
    for (i, (debit, credit)) in [
        ("Assets:Cash", "Income:Sales"),
        ("Assets:Bank", "Income:Fees"),
        ("Assets:Card", "Income:Interest"),
        ("Expenses:Rent", "Income:Other"),
    ]
    .into_iter()
    .enumerate()
    {
        let region = regions[(n as usize + i) % regions.len()];
        let channel = channels[(n as usize + i) % channels.len()];
        entry = entry
            .posting(
                Posting::debit(debit, one(), EUR)
                    .dimension("region", region)
                    .dimension("channel", channel),
            )
            .posting(
                Posting::credit(credit, one(), EUR)
                    .dimension("region", region)
                    .dimension("channel", channel),
            );
    }
    entry
}

/// S5: one draw of 1.00 from the limited wallet.
fn draw(n: u64) -> Entry {
    Entry::new(format!("draw-{n}"), date!("2026-06-15"))
        .debit("Liabilities:Wallet", one(), EUR)
        .credit("Assets:Cash", one(), EUR)
}

/// Records `entry`, retrying a transient store failure (a busy SQLite file
/// under S4's concurrent writers); a retry of a recorded key replays.
async fn record<S: Bench>(books: &Ledger<S>, entry: Entry) {
    loop {
        match books.record(entry.clone()).await {
            Ok(_) => return,
            Err(error) if error.is_transient() => tokio::task::yield_now().await,
            Err(error) => panic!("record: {error}"),
        }
    }
}

/// A ledger whose wallet is funded and already drawn from `history` times.
async fn limited<S: Bench>(store: S, history: u64) -> Ledger<S> {
    let books = books(store).await;
    let fund = Amount::from_minor(1_000_000_000).unwrap();
    books
        .record(
            Entry::new("fund", date!("2026-06-01"))
                .debit("Assets:Cash", fund, EUR)
                .credit("Liabilities:Wallet", fund, EUR),
        )
        .await
        .unwrap();
    let mut n = 0;
    while n < history {
        let size = BUILD_BATCH.min(history - n);
        books.record_batch((n..n + size).map(draw)).await.unwrap();
        n += size;
    }
    books
}

/// Where fresh stores come from; each call is a new, empty store.
struct Stores<'a> {
    rt: &'a Runtime,
    dir: tempfile::TempDir,
    files: Cell<u32>,
    #[cfg(feature = "postgres")]
    postgres: postgres::Server,
}

impl<'a> Stores<'a> {
    fn new(rt: &'a Runtime) -> Self {
        Self {
            rt,
            dir: tempfile::tempdir().unwrap(),
            files: Cell::new(0),
            #[cfg(feature = "postgres")]
            postgres: postgres::Server::default(),
        }
    }

    fn memory(&self) -> MemoryStore {
        MemoryStore::new()
    }

    fn sqlite(&self) -> SqlStore<Sqlite> {
        let n = self.files.get();
        self.files.set(n + 1);
        let file = self.dir.path().join(format!("bench-{n}.db"));
        let url = format!("sqlite://{}?mode=rwc", file.display());
        self.rt.block_on(SqlStore::sqlite(&url)).unwrap()
    }
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

/// S1–S4 and S5 for one store; `fresh` makes an empty store and is called
/// only for the scenarios the filter selects.
fn scenarios<S: Bench>(c: &mut Criterion, rt: &Runtime, store: &str, fresh: &dyn Fn() -> S) {
    let mut group = c.benchmark_group(format!("throughput/{store}"));
    group.throughput(Throughput::Elements(1));

    let lazy = OnceCell::new();
    let next = Cell::new(0u64);
    group.bench_function("S1-2-posting", |b| {
        let books = lazy.get_or_init(|| rt.block_on(books(fresh())));
        b.to_async(rt).iter_custom(|iters| {
            let start = next.replace(next.get() + iters);
            async move {
                let started = Instant::now();
                for n in start..start + iters {
                    books.record(two_posting("s1", n)).await.unwrap();
                }
                started.elapsed()
            }
        });
    });

    let lazy = OnceCell::new();
    let next = Cell::new(0u64);
    group.bench_function("S2-8-posting-2-dimensions", |b| {
        let books = lazy.get_or_init(|| rt.block_on(books(fresh())));
        b.to_async(rt).iter_custom(|iters| {
            let start = next.replace(next.get() + iters);
            async move {
                let started = Instant::now();
                for n in start..start + iters {
                    books.record(eight_posting(n)).await.unwrap();
                }
                started.elapsed()
            }
        });
    });

    for size in [1u64, 10, 100] {
        group.throughput(Throughput::Elements(size));
        let lazy = OnceCell::new();
        let next = Cell::new(0u64);
        group.bench_with_input(BenchmarkId::new("S3-batch", size), &size, |b, &size| {
            let books = lazy.get_or_init(|| rt.block_on(books(fresh())));
            b.to_async(rt).iter_custom(|iters| {
                let start = next.replace(next.get() + iters * size);
                async move {
                    let drafts: Vec<Vec<Entry>> = (0..iters)
                        .map(|batch| {
                            let first = start + batch * size;
                            (first..first + size)
                                .map(|n| two_posting("s3", n))
                                .collect()
                        })
                        .collect();
                    let started = Instant::now();
                    for batch in drafts {
                        books.record_batch(batch).await.unwrap();
                    }
                    started.elapsed()
                }
            });
        });
    }

    group.throughput(Throughput::Elements(1));
    let lazy = OnceCell::new();
    let next = Cell::new(0u64);
    group.bench_function("S4-4-tasks", |b| {
        let books = lazy.get_or_init(|| rt.block_on(books(fresh())));
        b.to_async(rt).iter_custom(|iters| {
            let start = next.replace(next.get() + iters);
            async move {
                let started = Instant::now();
                let tasks: Vec<_> = (0..4)
                    .map(|task| {
                        let books = books.clone();
                        tokio::spawn(async move {
                            let mut n = start + task;
                            while n < start + iters {
                                record(&books, two_posting("s4", n)).await;
                                n += 4;
                            }
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await.unwrap();
                }
                started.elapsed()
            }
        });
    });
    group.finish();

    limit_draws(rt, store, fresh);
}

/// S5: per history, a fixed protocol rather than criterion's adaptive loop
/// — every draw grows the history, and criterion would add tens of
/// thousands of draws to a history of 10⁴. [`S5_WARM_UP`] draws, then
/// [`S5_BATCHES`] timed batches of [`S5_DRAWS`] draws, one `record` each;
/// the per-draw median, min and max over the batches, and the ratio of the
/// two histories' medians.
fn limit_draws<S: Bench>(rt: &Runtime, store: &str, fresh: &dyn Fn() -> S) {
    let mut results = Vec::new();
    for history in S5_HISTORIES {
        let id = format!("throughput-limit/{store}/S5-limited-draw/{history}");
        match selection(&id) {
            Selection::Skip => continue,
            Selection::List => {
                println!("{id}: benchmark");
                continue;
            }
            Selection::Run => {}
        }
        let started = Instant::now();
        let books = rt.block_on(limited(fresh(), history));
        let built = started.elapsed();
        let mut per_draw = rt.block_on(async {
            let mut n = history;
            for _ in 0..S5_WARM_UP {
                books.record(draw(n)).await.unwrap();
                n += 1;
            }
            let mut per_draw = Vec::new();
            for _ in 0..S5_BATCHES {
                let started = Instant::now();
                for _ in 0..S5_DRAWS {
                    books.record(draw(n)).await.unwrap();
                    n += 1;
                }
                per_draw.push(started.elapsed() / S5_DRAWS as u32);
            }
            per_draw
        });
        per_draw.sort_unstable();
        let median = median(&per_draw);
        let added = S5_WARM_UP + S5_BATCHES * S5_DRAWS;
        println!(
            "{id}\n    history {history} draws (built in {built:.1?}), {S5_BATCHES} batches of {S5_DRAWS} draws \
             after {S5_WARM_UP} warm-up draws (history ends at {}, +{:.1}%)\n    \
             per draw: [{:.2?} {median:.2?} {:.2?}] (min median max) = {:.0} entries/s",
            history + added,
            added as f64 * 100.0 / history as f64,
            per_draw[0],
            per_draw[per_draw.len() - 1],
            1.0 / median.as_secs_f64(),
        );
        results.push((history, median));
    }
    if let [(small, low), (large, high)] = results[..] {
        println!(
            "throughput-limit/{store}/S5-ratio\n    t({large}) / t({small}) = {:.2} (published bound ≤ 1.5)",
            high.as_secs_f64() / low.as_secs_f64()
        );
    }
}

/// Whether criterion's filter (the first positional argument; `--exact`
/// honoured; `.*` the only regex syntax understood, otherwise a substring)
/// selects `id`, and whether `--list` asks only for the names.
fn selection(id: &str) -> Selection {
    /// Criterion's options that take a value (criterion 0.8.2).
    const VALUED: [&str; 19] = [
        "-c",
        "--color",
        "-s",
        "--save-baseline",
        "-b",
        "--baseline",
        "--baseline-lenient",
        "--format",
        "--profile-time",
        "--load-baseline",
        "--sample-size",
        "--warm-up-time",
        "--measurement-time",
        "--nresamples",
        "--noise-threshold",
        "--confidence-level",
        "--significance-level",
        "--plotting-backend",
        "--output-format",
    ];
    let (mut filter, mut exact, mut list) = (None, false, false);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exact" => exact = true,
            "--list" => list = true,
            flag if VALUED.contains(&flag) => {
                args.next();
            }
            flag if flag.starts_with('-') => {}
            _ if filter.is_none() => filter = Some(arg),
            _ => {}
        }
    }
    let selected = match filter {
        None => true,
        Some(filter) if exact => filter == id,
        Some(filter) => {
            let mut rest = id;
            filter.split(".*").all(|part| match rest.find(part) {
                Some(at) => {
                    rest = &rest[at + part.len()..];
                    true
                }
                None => false,
            })
        }
    };
    match (selected, list) {
        (false, _) => Selection::Skip,
        (true, true) => Selection::List,
        (true, false) => Selection::Run,
    }
}

enum Selection {
    Skip,
    List,
    Run,
}

fn median(sorted: &[Duration]) -> Duration {
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut c = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4))
        .configure_from_args();
    let stores = Stores::new(&rt);
    scenarios(&mut c, &rt, "memory", &|| stores.memory());
    scenarios(&mut c, &rt, "sqlite", &|| stores.sqlite());
    #[cfg(feature = "postgres")]
    scenarios(&mut c, &rt, "postgres", &|| stores.postgres.store(&rt));
    c.final_summary();
    #[cfg(feature = "postgres")]
    stores.postgres.stop(&rt);
}
