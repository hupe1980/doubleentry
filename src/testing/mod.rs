//! The public conformance suite (feature `testing`): whether a [`Store`]
//! keeps the contract is decided here, not by a reviewer — with the
//! canaries of [`BrokenStore`] (reads ignoring `through`, a missing
//! open-holds read, stale values, …) each caught by some check, and
//! [`independent_handles`] for a lock that must hold across processes.
//!
//! [`conformance`] runs, each on a fresh store from your factory:
//!
//! - **tables** — one row per rejection code (and precedence, replays,
//!   reversals, clearings, closing), each asserting that the store answers
//!   exactly what the in-memory reference [`Journal`](crate::Journal)
//!   answers, field for field; memo erasure; positions as leaf indexes;
//!   reads that honour `through`; an account's open holds;
//! - **simulation** — seeded swarm workloads of valid and mutated
//!   operations, retries of used keys, reads compared with an independent
//!   oracle at sampled steps, and at the end the store invariants and an
//!   audit;
//! - **concurrency** — real tasks racing appends, atomic batches (and one
//!   batch raced identically), identical retries, draws
//!   on limited accounts, a seal against an append into its period, a child
//!   account against a posting to its parent, memo erasures, resolutions of
//!   one hold, and holds reserving against one limited wallet;
//! - **independent handles** — [`independent_handles`]: the same races
//!   across two handles opened independently on one backing store, for a
//!   store whose lock must hold across processes (every SQL dialect runs it).
//!
//! The suite checks a store at the default scale (`P = 2`). A failure names
//! the check, the seed and the step, and how to reproduce it:
//! `DOUBLEENTRY_SEED=<seed> DOUBLEENTRY_CHECK=<check> cargo test …`.
//!
//! ```
//! use doubleentry::store::MemoryStore;
//! use doubleentry::testing::{Config, conformance_with};
//!
//! # tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
//! // A third-party store checks itself the same way: `conformance(|| async { MyStore::fresh().await })`.
//! let config = Config { check: Some(String::from("tables/2")), ..Config::default() };
//! let report = conformance_with(|| async { MemoryStore::new() }, config).await?;
//! assert!(report.checks.iter().all(|(name, _)| name.starts_with("tables/2")));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`BrokenStore`] is a canary the suite must catch; [`Flaky`] fails writes
//! the way a network does. With the `witness` feature, [`TestWitness`] is a
//! C2SP witness in memory for tests and examples — not a production
//! witness.

mod broken;
mod concurrency;
mod model;
mod reports;
mod simulation;
mod tables;
mod transparency;
#[cfg(feature = "witness")]
mod witness;

use core::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

pub use broken::{BrokenStore, Fault, Flaky};
#[cfg(feature = "witness")]
pub use witness::{Reply, TestWitness};

use crate::model::LedgerId;
use crate::store::Store;

/// The fixed seeds of a default run.
const SEEDS: [u64; 8] = [
    0x5EED_0001,
    0x5EED_0002,
    0x0DD5_EED5,
    0xC0FF_EE00,
    0x0BAD_CAFE,
    0x1234_5678,
    0x9E37_79B9,
    0xD1CE_D1CE,
];

/// What a run of the suite covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Only checks whose name starts with this (`DOUBLEENTRY_CHECK`).
    pub check: Option<String>,
    /// The simulation seeds (`DOUBLEENTRY_SEED=<n>` runs one,
    /// `DOUBLEENTRY_SEEDS=<k>` the first `k`).
    pub seeds: Vec<u64>,
    /// Operations per seed (`DOUBLEENTRY_STEPS`).
    pub steps: u32,
    /// Concurrent writers in the parallel-append check.
    pub writers: u32,
}

impl Default for Config {
    /// Every check, 8 seeds of 300 steps, 8 writers.
    fn default() -> Self {
        Self {
            check: None,
            seeds: SEEDS.to_vec(),
            steps: 300,
            writers: 8,
        }
    }
}

impl Config {
    /// The default, adjusted by `DOUBLEENTRY_SEED`, `DOUBLEENTRY_SEEDS`,
    /// `DOUBLEENTRY_STEPS` and `DOUBLEENTRY_CHECK`.
    #[must_use]
    pub fn from_env() -> Self {
        let number = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
        };
        let mut config = Self::default();
        if let Some(count) = number("DOUBLEENTRY_SEEDS") {
            let mut random = model::SplitMix64::new(0xD0B1_EE17);
            config.seeds = (0..count).map(|_| random.next()).collect();
        }
        if let Some(seed) = number("DOUBLEENTRY_SEED") {
            config.seeds = vec![seed];
        }
        if let Some(steps) = number("DOUBLEENTRY_STEPS").and_then(|steps| u32::try_from(steps).ok())
        {
            config.steps = steps;
        }
        config.check = std::env::var("DOUBLEENTRY_CHECK").ok();
        config
    }

    fn selects(&self, name: &str) -> bool {
        self.check
            .as_deref()
            .is_none_or(|prefix| name.starts_with(prefix))
    }
}

/// The checks that ran and passed, with their durations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Report {
    /// Each check's name and how long it took.
    pub checks: Vec<(String, Duration)>,
}

impl Report {
    /// The time all checks took.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.checks.iter().map(|(_, took)| *took).sum()
    }
}

/// The first check a store failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Failure {
    /// The check's name.
    pub check: String,
    /// The simulation seed, for a simulation check.
    pub seed: Option<u64>,
    /// The step that failed, when known.
    pub step: Option<u32>,
    /// What was expected and what the store did.
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed", self.check)?;
        if let Some(seed) = self.seed {
            write!(f, " at seed={seed}")?;
            if let Some(step) = self.step {
                write!(f, " step={step}")?;
            }
        }
        write!(f, ": {}\nreproduce: ", self.message)?;
        if let Some(seed) = self.seed {
            write!(f, "DOUBLEENTRY_SEED={seed} ")?;
        }
        write!(f, "DOUBLEENTRY_CHECK={} cargo test …", self.check)
    }
}

impl std::error::Error for Failure {}

/// Runs the whole suite against stores from `factory` (one fresh, empty
/// store per check, row and seed), adjusted by the environment
/// ([`Config::from_env`]).
///
/// # Errors
///
/// The first [`Failure`].
pub async fn conformance<S, F, Fut>(factory: F) -> Result<Report, Failure>
where
    S: Store<2>,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    conformance_with(factory, Config::from_env()).await
}

/// Runs the checks `config` selects against stores from `factory`.
///
/// # Errors
///
/// The first [`Failure`].
pub async fn conformance_with<S, F, Fut>(factory: F, config: Config) -> Result<Report, Failure>
where
    S: Store<2>,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    let mut suite = Suite {
        config,
        report: Report::default(),
        ledger: LedgerId::new("example.com/conformance").map_err(|error| Failure {
            check: String::from("setup"),
            seed: None,
            step: None,
            message: error.to_string(),
        })?,
    };
    let ledger = suite.ledger.clone();
    for row in tables::rows(&ledger) {
        let name = format!("tables/{}", row.name);
        if suite.config.selects(&name) {
            let store = factory().await;
            suite
                .check(&name, tables::run(store, &row, &ledger))
                .await?;
        }
    }
    suite
        .run(
            "tables/regress_f1_position_is_leaf_index",
            &factory,
            |store| tables::regress_f1_position_is_leaf_index(store, &ledger),
        )
        .await?;
    suite
        .run("tables/erasure", &factory, |store| {
            tables::erasure(store, &ledger)
        })
        .await?;
    suite
        .run("tables/reads_honour_through", &factory, |store| {
            tables::reads_honour_through(store, &ledger)
        })
        .await?;
    suite
        .run("tables/open_holds_by_account", &factory, |store| {
            tables::open_holds_by_account(store, &ledger)
        })
        .await?;
    suite
        .run(
            "simulation/regress_g7_seal_counts_late_booked_entries",
            &factory,
            |store| simulation::regress_g7_seal_counts_late_booked_entries(store, &ledger),
        )
        .await?;
    for seed in suite.config.seeds.clone() {
        let name = format!("simulation/{seed}");
        if suite.config.selects(&name) || suite.config.check.as_deref() == Some("simulation") {
            let started = Instant::now();
            let store = factory().await;
            simulation::simulate(store, seed, suite.config.steps, &ledger)
                .await
                .map_err(|(step, message)| Failure {
                    check: name.clone(),
                    seed: Some(seed),
                    step,
                    message,
                })?;
            suite.report.checks.push((name, started.elapsed()));
        }
    }
    let writers = suite.config.writers;
    suite
        .run("concurrency/parallel_appends", &factory, |store| {
            concurrency::parallel_appends(store, &ledger, writers)
        })
        .await?;
    suite
        .run("concurrency/parallel_batches", &factory, |store| {
            concurrency::parallel_batches(store, &ledger, writers)
        })
        .await?;
    suite
        .run("concurrency/batches_race_limit", &factory, |store| {
            concurrency::batches_race_limit(store, &ledger)
        })
        .await?;
    suite
        .run("concurrency/concurrent_replays", &factory, |store| {
            concurrency::concurrent_replays(store, &ledger)
        })
        .await?;
    suite
        .run(
            "concurrency/regress_g6_concurrent_limited_draws",
            &factory,
            |store| concurrency::regress_g6_concurrent_limited_draws(store, &ledger),
        )
        .await?;
    suite
        .run("concurrency/crossing_draws", &factory, |store| {
            concurrency::crossing_draws(store, &ledger)
        })
        .await?;
    suite
        .run(
            "concurrency/regress_f3_seal_races_append",
            &factory,
            |store| concurrency::regress_f3_seal_races_append(store, &ledger),
        )
        .await?;
    suite
        .run("concurrency/leaf_rule_race", &factory, |store| {
            concurrency::leaf_rule_race(store, &ledger)
        })
        .await?;
    suite
        .run("concurrency/erase_races_appends", &factory, |store| {
            concurrency::erase_races_appends(store, &ledger)
        })
        .await?;
    suite
        .run(
            "concurrency/holds_concurrent_resolution",
            &factory,
            |store| concurrency::holds_concurrent_resolution(store, &ledger),
        )
        .await?;
    suite
        .run(
            "concurrency/holds_concurrent_reservations_respect_limit",
            &factory,
            |store| concurrency::holds_concurrent_reservations_respect_limit(store, &ledger),
        )
        .await?;
    Ok(suite.report)
}

/// `independent_handles` (PoC: A3-S8): races two handles opened
/// independently on one backing store — `first` and `second` are two
/// connections or pools on the same database or file, never clones of one
/// handle — the way two processes share it: appends stay dense, limited
/// draws never overdraw, one hold resolves once, and the audit passes
/// through either handle. A store whose single-writer lock lives in its
/// process, not in the backing store, fails.
///
/// ```
/// # #[cfg(feature = "sqlite")]
/// # tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
/// use doubleentry::store::{SqlStore, Sqlite};
///
/// let dir = tempfile::tempdir()?;
/// let url = format!("sqlite://{}", dir.path().join("books.db").display());
/// let first = SqlStore::<Sqlite>::sqlite(&url).await?;
/// let second = SqlStore::<Sqlite>::sqlite(&url).await?;
/// doubleentry::testing::independent_handles(first, second).await?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// The [`Failure`] of the check `concurrency/independent_handles`.
pub async fn independent_handles<S: Store<2>>(first: S, second: S) -> Result<(), Failure> {
    let ledger = LedgerId::new("example.com/conformance").map_err(|error| Failure {
        check: String::from("setup"),
        seed: None,
        step: None,
        message: error.to_string(),
    })?;
    concurrency::independent_handles(first, second, &ledger)
        .await
        .map_err(|message| Failure {
            check: String::from("concurrency/independent_handles"),
            seed: None,
            step: None,
            message,
        })
}

/// A run in progress.
struct Suite {
    config: Config,
    report: Report,
    ledger: LedgerId,
}

impl Suite {
    /// Records the outcome of one check.
    async fn check(
        &mut self,
        name: &str,
        check: impl Future<Output = model::Check>,
    ) -> Result<(), Failure> {
        let started = Instant::now();
        check.await.map_err(|message| Failure {
            check: name.to_owned(),
            seed: None,
            step: None,
            message,
        })?;
        self.report
            .checks
            .push((name.to_owned(), started.elapsed()));
        Ok(())
    }

    /// Runs one check on a fresh store, when selected.
    async fn run<S, F, Fut, C, R>(
        &mut self,
        name: &str,
        factory: &F,
        check: C,
    ) -> Result<(), Failure>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = S>,
        C: FnOnce(S) -> R,
        R: Future<Output = model::Check>,
    {
        if !self.config.selects(name) {
            return Ok(());
        }
        let store = factory().await;
        self.check(name, check(store)).await
    }
}
