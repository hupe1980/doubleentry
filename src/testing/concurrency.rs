//! The concurrency checks: real tasks on the caller's runtime
//! racing writes on one ledger. Which racer wins is unknown; how many win,
//! and that nothing is lost, duplicated, deadlocked or overdrawn, is exact.

use std::future::Future;
use std::time::Duration;

use time::Date;

use super::model::Check;
use crate::error::{Error, Rejection};
use crate::model::{
    AccountKind, Amount, BalanceLimit, EUR, Entry, LedgerId, MemoSalt, PostAmount, Posting,
};
use crate::store::Store;
use crate::{AccountSpec, Ledger, Recorded};

/// How long a check may take before it counts as deadlocked.
const DEADLINE: Duration = Duration::from_secs(120);

/// January 2026, the month every check books into.
fn day(day: u8) -> Result<Date, String> {
    Date::from_calendar_date(2026, time::Month::January, day).map_err(|error| error.to_string())
}

fn eur<const P: u8>(units: i128) -> Result<Amount<P>, String> {
    let scale = 10i128.checked_pow(u32::from(P)).ok_or("scale")?;
    Amount::from_minor(units.checked_mul(scale).ok_or("amount")?).map_err(|error| error.to_string())
}

/// A ledger with EUR, cash, sales, two limited wallets and the months of
/// 2026.
async fn books<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Result<Ledger<S, P>, String> {
    let books = Ledger::<S, P>::with_precision(store, ledger.clone())
        .await
        .map_err(|error| error.to_string())?
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([7; 16]));
    let on = day(1)?;
    let fail = |error: Error| error.to_string();
    books
        .declare_currency(EUR, P.min(2), on)
        .await
        .map_err(fail)?;
    books
        .open("Assets:Cash", AccountKind::Asset, on)
        .await
        .map_err(fail)?;
    books
        .open("Income:Sales", AccountKind::Income, on)
        .await
        .map_err(fail)?;
    for wallet in ["Liabilities:Wallet:A", "Liabilities:Wallet:B"] {
        books
            .register(
                AccountSpec::new(wallet, AccountKind::Liability, on)
                    .limit(BalanceLimit::NoDebitBalance),
            )
            .await
            .map_err(fail)?;
    }
    books.define_months(2026, 1..=12).await.map_err(fail)?;
    Ok(books)
}

/// Runs `check` within the deadline.
async fn within(check: impl Future<Output = Check>) -> Check {
    tokio::time::timeout(DEADLINE, check)
        .await
        .map_err(|_| String::from("did not finish within the deadline (deadlock?)"))?
}

/// Spawns one task per future and collects their results in order.
async fn race<T: Send + 'static>(
    tasks: Vec<impl Future<Output = T> + Send + 'static>,
) -> Result<Vec<T>, String> {
    let handles: Vec<_> = tasks.into_iter().map(tokio::spawn).collect();
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(
            handle
                .await
                .map_err(|error| format!("a racing task failed: {error}"))?,
        );
    }
    Ok(results)
}

fn sale<const P: u8>(key: &str, amount: Amount<P>, on: Date) -> Entry<P> {
    Entry::new(key, on)
        .debit("Assets:Cash", amount, EUR)
        .credit("Income:Sales", amount, EUR)
}

/// `writers` × 25 distinct entries at once: all recorded at dense positions,
/// and the audit passes.
pub(crate) async fn parallel_appends<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
    writers: u32,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let start = books.head().await.map_err(|error| error.to_string())?.size;
        let one = eur::<P>(1)?;
        let on = day(15)?;
        let tasks = (0..writers)
            .map(|writer| {
                let books = books.clone();
                async move {
                    let mut recorded = Vec::new();
                    for n in 0..25 {
                        recorded.push(
                            books
                                .record(sale(&format!("w{writer}-{n}"), one, on))
                                .await?,
                        );
                    }
                    Ok::<_, Error>(recorded)
                }
            })
            .collect();
        let mut positions = Vec::new();
        for result in race(tasks).await? {
            positions.extend(
                result
                    .map_err(|error| format!("a write failed: {error}"))?
                    .iter()
                    .map(|r| r.position),
            );
        }
        positions.sort_unstable();
        let expected: Vec<u64> =
            (start..start.saturating_add(u64::from(writers).saturating_mul(25))).collect();
        if positions != expected {
            return Err(format!("positions are not dense: {positions:?}"));
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// 16 identical drafts at once: one write, 16 equal receipts.
pub(crate) async fn concurrent_replays<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let draft = sale("same", eur::<P>(5)?, day(15)?);
        let tasks = (0..16)
            .map(|_| {
                let (books, draft) = (books.clone(), draft.clone());
                async move { books.record(draft).await }
            })
            .collect();
        let results: Vec<Recorded> = race(tasks)
            .await?
            .into_iter()
            .collect::<Result<_, _>>()
            .map_err(|error: Error| format!("a write failed: {error}"))?;
        let firsts = results.iter().filter(|recorded| !recorded.replayed).count();
        let same = results
            .windows(2)
            .all(|pair| matches!(pair, [a, b] if (a.id, a.position) == (b.id, b.position)));
        if firsts != 1 || !same {
            return Err(format!(
                "expected one write and 16 equal receipts, got {results:?}"
            ));
        }
        Ok(())
    })
    .await
}

/// A draw on a wallet: debit the wallet, credit cash.
fn draw<const P: u8>(key: &str, wallet: &str, amount: Amount<P>, on: Date) -> Entry<P> {
    Entry::new(key, on)
        .debit(wallet, amount, EUR)
        .credit("Assets:Cash", amount, EUR)
}

/// Funds a wallet: debit cash, credit the wallet.
fn fund<const P: u8>(key: &str, wallet: &str, amount: Amount<P>, on: Date) -> Entry<P> {
    Entry::new(key, on)
        .debit("Assets:Cash", amount, EUR)
        .credit(wallet, amount, EUR)
}

/// `regress_g6_concurrent_limited_draws` (PoC: G6): 16 draws of 100.00
/// against a wallet holding 1 000.00 that may not go negative: exactly ten
/// recorded, six refused with the limit rejection, no draw failing with a
/// deadlock or serialisation error, and the books audit.
// PoC: G6 poc_g6_deferred_concurrent_draws_deadlock_read_committed
pub(crate) async fn regress_g6_concurrent_limited_draws<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let wallet = "Liabilities:Wallet:A";
        books
            .record(fund("fund", wallet, eur::<P>(1000)?, day(2)?))
            .await
            .map_err(|error| error.to_string())?;
        let hundred = eur::<P>(100)?;
        let on = day(15)?;
        let tasks = (0..16)
            .map(|n| {
                let books = books.clone();
                async move {
                    books
                        .record(draw(&format!("draw-{n}"), wallet, hundred, on))
                        .await
                }
            })
            .collect();
        let (mut recorded, mut refused) = (0u32, 0u32);
        for result in race(tasks).await? {
            match result {
                Ok(_) => recorded = recorded.saturating_add(1),
                Err(Error::Rejected(Rejection::LimitExceeded { .. })) => {
                    refused = refused.saturating_add(1)
                }
                Err(error) => return Err(format!("a draw failed: {error}")),
            }
        }
        if (recorded, refused) != (10, 6) {
            return Err(format!(
                "expected 10 draws and 6 refusals, got {recorded} and {refused}"
            ));
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// `writers` × 5 batches of 5 entries at once, and one identical batch
/// raced by eight tasks: every batch at contiguous positions, the positions
/// dense, the identical batch written once (seven replays with its
/// receipts), and the books audit.
pub(crate) async fn parallel_batches<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
    writers: u32,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let start = books.head().await.map_err(|error| error.to_string())?.size;
        let (one, on) = (eur::<P>(1)?, day(15)?);
        let same: Vec<Entry<P>> = (0..5)
            .map(|n| sale(&format!("same-{n}"), one, on))
            .collect();
        let mut tasks: Vec<_> = (0..writers)
            .map(|writer| {
                let books = books.clone();
                tokio::spawn(async move {
                    let mut batches = Vec::new();
                    for b in 0..5 {
                        let drafts = (0..5).map(|n| sale(&format!("w{writer}-{b}-{n}"), one, on));
                        batches.push(books.record_batch(drafts).await?);
                    }
                    Ok::<_, Error>(batches)
                })
            })
            .collect();
        tasks.extend((0..8).map(|_| {
            let (books, same) = (books.clone(), same.clone());
            tokio::spawn(async move { Ok(vec![books.record_batch(same).await?]) })
        }));
        let (mut positions, mut identical) = (Vec::new(), Vec::new());
        for (task, handle) in tasks.into_iter().enumerate() {
            let batches = handle
                .await
                .map_err(|error| format!("a racing task failed: {error}"))?
                .map_err(|error| format!("a batch failed: {error}"))?;
            for recorded in batches {
                let first = recorded.first().map_or(0, |receipt| receipt.position);
                let contiguous = recorded
                    .iter()
                    .zip(first..)
                    .all(|(receipt, position)| receipt.position == position);
                if !contiguous {
                    return Err(format!("a batch is not contiguous: {recorded:?}"));
                }
                if task >= usize::try_from(writers).unwrap_or(usize::MAX) {
                    identical.push(recorded);
                } else {
                    positions.extend(recorded.iter().map(|receipt| receipt.position));
                }
            }
        }
        let written: Vec<_> = identical
            .iter()
            .filter(|recorded| recorded.iter().all(|receipt| !receipt.replayed))
            .collect();
        let replays = identical
            .iter()
            .filter(|recorded| recorded.iter().all(|receipt| receipt.replayed))
            .count();
        let Some(original) = written.first() else {
            return Err(String::from("the identical batch was never written"));
        };
        let equal = identical.iter().all(|recorded| {
            recorded
                .iter()
                .zip(original.iter())
                .all(|(a, b)| (a.id, a.position) == (b.id, b.position))
        });
        if written.len() != 1 || replays != 7 || !equal {
            return Err(format!(
                "expected one write and seven replays of the identical batch, got {identical:?}"
            ));
        }
        positions.extend(original.iter().map(|receipt| receipt.position));
        positions.sort_unstable();
        let total = u64::from(writers).saturating_mul(25).saturating_add(5);
        let expected: Vec<u64> = (start..start.saturating_add(total)).collect();
        if positions != expected {
            return Err(format!("positions are not dense: {positions:?}"));
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// Eight batches of two draws of 100.00 each, at once, against a wallet
/// holding 1 000.00 that may not go negative: exactly five recorded, three
/// refused at a draft with the limit rejection and nothing of them written,
/// the wallet at 0.00.
pub(crate) async fn batches_race_limit<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let wallet = "Liabilities:Wallet:A";
        books
            .record(fund("fund", wallet, eur::<P>(1000)?, day(2)?))
            .await
            .map_err(|error| error.to_string())?;
        let (hundred, on) = (eur::<P>(100)?, day(15)?);
        let tasks = (0..8)
            .map(|n| {
                let books = books.clone();
                async move {
                    let drafts =
                        (0..2).map(|d| draw(&format!("draw-{n}-{d}"), wallet, hundred, on));
                    (n, books.record_batch(drafts).await)
                }
            })
            .collect();
        let (mut recorded, mut refused) = (0u32, Vec::new());
        for (n, result) in race(tasks).await? {
            match result {
                Ok(_) => recorded = recorded.saturating_add(1),
                Err(Error::BatchRejected {
                    rejection: Rejection::LimitExceeded { .. },
                    ..
                }) => refused.push(n),
                Err(error) => return Err(format!("a batch failed: {error}")),
            }
        }
        if (recorded, refused.len()) != (5, 3) {
            return Err(format!(
                "expected 5 batches and 3 refusals, got {recorded} and {}",
                refused.len()
            ));
        }
        for n in refused {
            for d in 0..2 {
                let key = format!("draw-{n}-{d}");
                let found = books
                    .entry_by_key(&key)
                    .await
                    .map_err(|error| error.to_string())?;
                if found.is_some() {
                    return Err(format!("{key} of a refused batch was written"));
                }
            }
        }
        let balance = books
            .balance(wallet, EUR, &crate::BalanceQuery::all())
            .await
            .map_err(|error| error.to_string())?;
        if !balance.net().is_zero() {
            return Err(format!("the wallet ends at {}, not 0.00", balance.net()));
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// Draws crossing between two limited wallets in both directions at once:
/// only limit refusals, never a deadlock, and the books audit.
pub(crate) async fn crossing_draws<S: Store<P>, const P: u8>(store: S, ledger: &LedgerId) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let (a, b) = ("Liabilities:Wallet:A", "Liabilities:Wallet:B");
        for (key, wallet) in [("fund-a", a), ("fund-b", b)] {
            books
                .record(fund(key, wallet, eur::<P>(300)?, day(2)?))
                .await
                .map_err(|error| error.to_string())?;
        }
        let hundred = eur::<P>(100)?;
        let on = day(15)?;
        let tasks = (0u32..16)
            .map(|n| {
                let books = books.clone();
                let (from, to) = if n.checked_rem(2) == Some(0) {
                    (a, b)
                } else {
                    (b, a)
                };
                async move {
                    let entry = Entry::new(format!("cross-{n}"), on)
                        .posting(Posting::debit(from, hundred, EUR))
                        .posting(Posting::credit(to, hundred, EUR));
                    books.record(entry).await
                }
            })
            .collect();
        for result in race(tasks).await? {
            match result {
                Ok(_) | Err(Error::Rejected(Rejection::LimitExceeded { .. })) => {}
                Err(error) => return Err(format!("a crossing draw failed: {error}")),
            }
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// `regress_f3_seal_races_append` (PoC: F3): a period sealed while an entry
/// booked into it is recorded, 50 times (a month each): the entry is in the
/// log before the seal (and counted by it) or refused as booked into a
/// sealed period — never accepted after the seal.
// PoC: F3 poc_f3_append_into_sealed_period_is_accepted
pub(crate) async fn regress_f3_seal_races_append<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        for year in 2027..=2030 {
            books
                .define_months(year, 1..=12)
                .await
                .map_err(|error| error.to_string())?;
        }
        let one = eur::<P>(1)?;
        for repetition in 0u8..50 {
            let year = 2026i32.saturating_add(i32::from(repetition.checked_div(12).unwrap_or(0)));
            let month = repetition.checked_rem(12).unwrap_or(0).saturating_add(1);
            let period = format!("{year}-{month:02}");
            let on = Date::from_calendar_date(
                year,
                time::Month::try_from(month).map_err(|error| error.to_string())?,
                15,
            )
            .map_err(|error| error.to_string())?;
            let (sealer, writer) = (books.clone(), books.clone());
            let seal = tokio::spawn(async move { sealer.seal_period(period).await });
            let entry = tokio::spawn(async move {
                writer
                    .record(sale(&format!("race-{repetition}"), one, on))
                    .await
            });
            let seal = seal
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| format!("the seal failed: {error}"))?;
            match entry.await.map_err(|error| error.to_string())? {
                Ok(recorded) if recorded.position < seal.position => {}
                Ok(recorded) => {
                    return Err(format!(
                        "an entry at {} was accepted after the seal at {}",
                        recorded.position, seal.position
                    ));
                }
                Err(Error::Rejected(Rejection::PeriodSealed { .. })) => {}
                Err(error) => return Err(format!("the entry failed: {error}")),
            }
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// Registering a child account races a posting to its parent: never both.
pub(crate) async fn leaf_rule_race<S: Store<P>, const P: u8>(store: S, ledger: &LedgerId) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let one = eur::<P>(1)?;
        let on = day(15)?;
        for n in 0..8 {
            let parent = format!("Assets:Parent{n}");
            books
                .open(&parent, AccountKind::Asset, day(1)?)
                .await
                .map_err(|error| error.to_string())?;
            let (opener, writer) = (books.clone(), books.clone());
            let child = format!("{parent}:Child");
            let open =
                tokio::spawn(async move { opener.open(child, AccountKind::Asset, on).await });
            let entry = Entry::new(format!("parent-{n}"), on)
                .debit(parent.as_str(), one, EUR)
                .credit("Income:Sales", one, EUR);
            let post = tokio::spawn(async move { writer.record(entry).await });
            let opened = open.await.map_err(|error| error.to_string())?;
            let posted = post.await.map_err(|error| error.to_string())?;
            if opened.is_ok() && posted.is_ok() {
                return Err(format!("{parent} got both a child and a posting"));
            }
            for result in [opened.map(drop), posted.map(drop)] {
                match result {
                    Ok(()) | Err(Error::Rejected(_)) => {}
                    Err(error) => return Err(format!("a racer failed: {error}")),
                }
            }
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// Memo erasures racing appends: everything completes, the books audit.
pub(crate) async fn erase_races_appends<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let one = eur::<P>(1)?;
        let on = day(15)?;
        let mut ids = Vec::new();
        for n in 0..8 {
            let recorded = books
                .record(sale(&format!("memo-{n}"), one, on).memo(format!("personal {n}")))
                .await
                .map_err(|error| error.to_string())?;
            ids.push(recorded.id);
        }
        let mut tasks = Vec::new();
        for (n, id) in ids.into_iter().enumerate() {
            let (eraser, writer) = (books.clone(), books.clone());
            tasks.push(tokio::spawn(async move { eraser.erase_memo(id).await }));
            tasks.push(tokio::spawn(async move {
                writer
                    .record(sale(&format!("after-{n}"), one, on))
                    .await
                    .map(drop)
            }));
        }
        for task in tasks {
            task.await
                .map_err(|error| error.to_string())?
                .map_err(|error| format!("a racer failed: {error}"))?;
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// How often each hold race runs.
const HOLD_RACES: u32 = 10;

/// `holds_concurrent_resolution`: one hold, 16 resolutions with distinct
/// keys (posts, voids and expiries) and 4 retries of one of them, all at
/// once, 100 times: exactly one resolution is recorded, every other distinct
/// key is refused with `HoldResolved` naming the hold and the winner, and
/// each retry is the winner's replay or the same refusal.
pub(crate) async fn holds_concurrent_resolution<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let ten = eur::<P>(10)?;
        let (on, expires) = (day(15)?, day(20)?);
        for round in 0..HOLD_RACES {
            let hold = books
                .record(
                    Entry::open_hold(format!("hold-{round}"), on)
                        .expires(expires)
                        .debit("Assets:Cash", ten, EUR)
                        .credit("Income:Sales", ten, EUR),
                )
                .await
                .map_err(|error| error.to_string())?;
            let resolution = |n: u32| {
                let key = format!("resolve-{round}-{n}");
                match n % 3 {
                    0 => Entry::post_hold(key, on, hold.id, PostAmount::Full),
                    1 => Entry::void_hold(key, on, hold.id),
                    _ => Entry::expire_hold(key, expires, hold.id),
                }
            };
            let drafts: Vec<Entry<P>> = (0..16)
                .map(resolution)
                .chain((0..4).map(|_| resolution(0)))
                .collect();
            let tasks = drafts
                .into_iter()
                .map(|draft| {
                    let books = books.clone();
                    async move { books.record(draft).await }
                })
                .collect();
            let results = race(tasks).await?;
            let winners: Vec<Recorded> = results
                .iter()
                .filter_map(|result| result.as_ref().ok().filter(|r| !r.replayed).copied())
                .collect();
            let [winner] = winners.as_slice() else {
                return Err(format!("round {round}: {} resolutions won", winners.len()));
            };
            for result in &results {
                match result {
                    Ok(recorded) if recorded.id == winner.id => {}
                    Err(Error::Rejected(Rejection::HoldResolved {
                        hold: target, by, ..
                    })) if *target == hold.id && *by == winner.id => {}
                    other => {
                        return Err(format!(
                            "round {round}: {other:?} beside the winner {}",
                            winner.id
                        ));
                    }
                }
            }
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// `holds_concurrent_reservations_respect_limit` (PoC: G2): 16 holds of
/// 10.00 at once on a wallet funded with 100.00 that may not go negative,
/// 100 times on a fresh wallet: exactly ten recorded and six refused by the
/// limit — reservations count against it under any interleaving.
// PoC: G2 poc_g2_deferred_limit_breach_under_repeatable_read
pub(crate) async fn holds_concurrent_reservations_respect_limit<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let books = books(store, ledger).await?;
        let (ten, on) = (eur::<P>(10)?, day(15)?);
        for round in 0..HOLD_RACES {
            let wallet = format!("Liabilities:Wallet:H{round}");
            books
                .register(
                    AccountSpec::new(&wallet, AccountKind::Liability, day(1)?)
                        .limit(BalanceLimit::NoDebitBalance),
                )
                .await
                .map_err(|error| error.to_string())?;
            books
                .record(fund(
                    &format!("fund-{round}"),
                    &wallet,
                    eur::<P>(100)?,
                    day(2)?,
                ))
                .await
                .map_err(|error| error.to_string())?;
            let tasks = (0..16)
                .map(|n| {
                    let books = books.clone();
                    let hold = Entry::open_hold(format!("reserve-{round}-{n}"), on)
                        .debit(wallet.as_str(), ten, EUR)
                        .credit("Assets:Cash", ten, EUR);
                    async move { books.record(hold).await }
                })
                .collect();
            let (mut recorded, mut refused) = (0u32, 0u32);
            for result in race(tasks).await? {
                match result {
                    Ok(_) => recorded = recorded.saturating_add(1),
                    Err(Error::Rejected(Rejection::LimitExceeded { .. })) => {
                        refused = refused.saturating_add(1);
                    }
                    Err(error) => return Err(format!("round {round}: a hold failed: {error}")),
                }
            }
            if (recorded, refused) != (10, 6) {
                return Err(format!(
                    "round {round}: expected 10 holds and 6 refusals, got {recorded} and {refused}"
                ));
            }
        }
        books
            .audit()
            .await
            .map_err(|error| format!("audit: {error}"))?;
        Ok(())
    })
    .await
}

/// `independent_handles` (PoC: A3-S8): two handles opened independently on
/// one backing store — separate pools, as two processes share a database,
/// never clones of one handle — race appends, limited draws and the
/// resolutions of one hold. Positions stay dense, the wallet is never
/// overdrawn, the hold resolves once, and the audit passes through either
/// handle: the single-writer lock is the database's, not a process's.
pub(crate) async fn independent_handles<S: Store<P>, const P: u8>(
    first: S,
    second: S,
    ledger: &LedgerId,
) -> Check {
    within(async {
        let a = books(first, ledger).await?;
        let b = Ledger::<S, P>::with_precision(second, ledger.clone())
            .await
            .map_err(|error| error.to_string())?
            .as_actor("setup")
            .with_salt_source(|| MemoSalt([7; 16]));
        let pick = |n: u32| if n % 2 == 1 { b.clone() } else { a.clone() };
        // Appends: four writers per handle.
        let start = a.head().await.map_err(|error| error.to_string())?.size;
        let (one, on) = (eur::<P>(1)?, day(15)?);
        let tasks = (0..8u32)
            .map(|writer| {
                let books = pick(writer);
                async move {
                    let mut positions = Vec::new();
                    for n in 0..20 {
                        positions.push(
                            books
                                .record(sale(&format!("i{writer}-{n}"), one, on))
                                .await?
                                .position,
                        );
                    }
                    Ok::<_, Error>(positions)
                }
            })
            .collect();
        let mut positions = Vec::new();
        for result in race(tasks).await? {
            positions.extend(result.map_err(|error| format!("a write failed: {error}"))?);
        }
        positions.sort_unstable();
        let expected: Vec<u64> = (start..start.saturating_add(160)).collect();
        if positions != expected {
            return Err(format!("positions are not dense: {positions:?}"));
        }
        // Limited draws: 16 of 100.00 against 1 000.00, from both handles.
        let wallet = "Liabilities:Wallet:A";
        a.record(fund("i-fund", wallet, eur::<P>(1000)?, day(2)?))
            .await
            .map_err(|error| error.to_string())?;
        let hundred = eur::<P>(100)?;
        let tasks = (0..16u32)
            .map(|n| {
                let books = pick(n);
                async move {
                    books
                        .record(draw(&format!("i-draw-{n}"), wallet, hundred, on))
                        .await
                }
            })
            .collect();
        let mut recorded = 0u32;
        for result in race(tasks).await? {
            match result {
                Ok(_) => recorded = recorded.saturating_add(1),
                Err(Error::Rejected(Rejection::LimitExceeded { .. })) => {}
                Err(error) => return Err(format!("a draw failed: {error}")),
            }
        }
        if recorded != 10 {
            return Err(format!("{recorded} of 16 draws were recorded, expected 10"));
        }
        // One hold, resolved from both handles at once: once.
        let hold = a
            .record(
                Entry::open_hold("i-hold", on)
                    .debit("Assets:Cash", one, EUR)
                    .credit("Income:Sales", one, EUR),
            )
            .await
            .map_err(|error| error.to_string())?;
        let tasks = (0..8u32)
            .map(|n| {
                let books = pick(n);
                async move {
                    books
                        .record(Entry::void_hold(format!("i-void-{n}"), on, hold.id))
                        .await
                }
            })
            .collect();
        let won = race(tasks)
            .await?
            .iter()
            .filter(|result| result.is_ok())
            .count();
        if won != 1 {
            return Err(format!("{won} resolutions of one hold won"));
        }
        for books in [a, b] {
            books
                .audit()
                .await
                .map_err(|error| format!("audit: {error}"))?;
        }
        Ok(())
    })
    .await
}
