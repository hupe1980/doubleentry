//! The executable contract every backend must satisfy.
//!
//! A ledger is only as trustworthy as its weakest backend, and the ways a
//! backend can be subtly wrong — a read-then-write idempotency check that races,
//! a batch that half-lands, an index sequence with a gap — are exactly the ways
//! that produce no error and no symptom until an audit. Publishing the trait
//! without a way to check it would leave each implementor to guess.
//!
//! Run [`check_all`] against a fresh, empty store:
//!
//! ```
//! use doubleentry::LedgerId;
//! use doubleentry::storage::{MemoryStore, conformance};
//!
//! let store = MemoryStore::<2>::new(LedgerId::new("my-ledger")?);
//! let report = conformance::block_on(conformance::check_all(&store));
//! report.assert_passed();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Every check builds its own accounts and entries, so a backend needs to
//! provide nothing but an empty store.

use std::future::Future;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use time::macros::date;

use crate::account::{Account, AccountKind, AccountPath, AccountRegistry};
use crate::balance::BalanceKey;
use crate::clearing::{ClearedItem, Clearing, PostingRef};
use crate::entry::{Draft, Entry, EntryId, IdempotencyKey, LedgerPolicy, SealContext};
use crate::money::{Amount, Currency};
use crate::period::PeriodCalendar;
use crate::posting::Layer;
use crate::storage::{Cursor, EntryBatch, LedgerStore};
use crate::{AccountId, Balanced};

/// A ledger identifier for tests and examples.
#[must_use]
pub fn test_ledger() -> crate::period::LedgerId {
    crate::period::LedgerId::new("test").unwrap_or_else(|_| unreachable!("literal is valid"))
}

/// How many sequencing attempts a check makes before giving up.
///
/// Generous, because a pass that places nothing is not evidence that there is
/// nothing to place — see [`drain_sequencing`].
const SEQUENCING_ATTEMPTS: usize = 512;

/// Yields to the executor once, without depending on one.
///
/// The conformance suite has to let other tasks run — a backend whose watermark
/// is held back by an unrelated open transaction can only make progress once
/// that transaction ends — but it must not pick an async runtime on behalf of
/// the crate.
async fn yield_once() {
    struct YieldOnce(bool);
    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    YieldOnce(false).await;
}

/// Drives sequencing until every recorded entry has a position.
///
/// A single pass is not enough, and neither is "loop until a pass places
/// nothing". A backend advancing on a commit-order watermark declines to place
/// rows whose inserting transaction is still open, so a pass can legitimately
/// place zero and still have work outstanding. Sequencing is *eventually*
/// complete; a check that assumed otherwise would be testing a guarantee the
/// technique does not offer.
async fn drain_sequencing<const P: u8, S: LedgerStore<P>>(store: &S) -> Result<(), String> {
    let mut idle = 0u32;
    for _ in 0..SEQUENCING_ATTEMPTS {
        match store.sequence().await {
            Ok(0) => {
                idle = idle.saturating_add(1);
                // Several consecutive empty passes with nothing arriving is as
                // good a settling signal as this interface can give.
                if idle >= 8 {
                    return Ok(());
                }
            }
            Ok(_) => idle = 0,
            Err(e) => return Err(format!("sequence failed: {e}")),
        }
        yield_once().await;
    }
    Err("sequencing did not settle".to_owned())
}

/// Sequences until `id` has a position, or gives up.
async fn sequence_until_placed<const P: u8, S: LedgerStore<P>>(
    store: &S,
    id: EntryId,
) -> Result<Option<crate::journal::LogIndex>, String> {
    for _ in 0..SEQUENCING_ATTEMPTS {
        match store.get(id).await {
            Ok(Some(record)) if record.index.is_some() => return Ok(record.index),
            Ok(_) => {}
            Err(e) => return Err(format!("get failed: {e}")),
        }
        store
            .sequence()
            .await
            .map_err(|e| format!("sequence failed: {e}"))?;
        yield_once().await;
    }
    Ok(None)
}

/// Runs a future to completion on the current thread.
///
/// Provided so a backend can run the suite from an ordinary `#[test]` without
/// committing this crate — or the backend's test suite — to a particular async
/// runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Signal {
        woken: Mutex<bool>,
        ready: Condvar,
    }

    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let mut woken = self.woken.lock().unwrap_or_else(|e| e.into_inner());
            *woken = true;
            self.ready.notify_one();
        }
    }

    let signal = Arc::new(Signal {
        woken: Mutex::new(false),
        ready: Condvar::new(),
    });
    let waker = Waker::from(Arc::clone(&signal));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);

    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        let mut woken = signal.woken.lock().unwrap_or_else(|e| e.into_inner());
        while !*woken {
            woken = signal.ready.wait(woken).unwrap_or_else(|e| e.into_inner());
        }
        *woken = false;
    }
}

/// The outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// What was checked.
    pub name: &'static str,
    /// `None` when it passed; the reason when it did not.
    pub failure: Option<String>,
}

impl CheckResult {
    fn pass(name: &'static str) -> Self {
        Self {
            name,
            failure: None,
        }
    }

    fn fail(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            failure: Some(reason.into()),
        }
    }

    /// True when the check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

/// The result of a full conformance run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One entry per check, in the order they ran.
    pub checks: Vec<CheckResult>,
}

impl Report {
    /// True when every check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(CheckResult::passed)
    }

    /// The checks that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|c| !c.passed()).collect()
    }

    /// Panics with a readable summary unless every check passed.
    ///
    /// # Panics
    ///
    /// Panics when any check failed.
    pub fn assert_passed(&self) {
        assert!(self.passed(), "{self}");
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failures = self.failures();
        if failures.is_empty() {
            return write!(f, "all {} conformance checks passed", self.checks.len());
        }
        writeln!(
            f,
            "{} of {} conformance checks failed:",
            failures.len(),
            self.checks.len()
        )?;
        for check in failures {
            writeln!(
                f,
                "  - {}: {}",
                check.name,
                check.failure.as_deref().unwrap_or("")
            )?;
        }
        Ok(())
    }
}

/// Accounts and policy the checks post against.
struct Fixture {
    accounts: AccountRegistry,
    calendar: PeriodCalendar,
    policy: LedgerPolicy,
    left: AccountId,
    right: AccountId,
}

impl Fixture {
    fn new() -> Self {
        let mut accounts = AccountRegistry::new();
        let left = accounts
            .register_path("Conformance:Left", date!(2000 - 01 - 01))
            .unwrap_or(AccountId::from_index(0));
        let right = accounts
            .register_path("Conformance:Right", date!(2000 - 01 - 01))
            .unwrap_or(AccountId::from_index(1));
        Self {
            accounts,
            calendar: PeriodCalendar::new(),
            policy: LedgerPolicy::default(),
            left,
            right,
        }
    }

    fn ctx(&self) -> SealContext<'_> {
        SealContext {
            accounts: &self.accounts,
            calendar: &self.calendar,
            policy: &self.policy,
        }
    }

    /// A balanced entry moving `minor` from right to left.
    fn entry<const P: u8>(&self, key: &[u8], minor: i64) -> Option<Entry<Balanced, P>> {
        self.entry_with_id(EntryId::generate(), key, minor)
    }

    /// A balanced entry moving `minor` from left to right — the settling side.
    fn reversed_entry<const P: u8>(&self, key: &[u8], minor: i64) -> Option<Entry<Balanced, P>> {
        Entry::<Draft, P>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).ok()?,
            date!(2026 - 03 - 20),
        )
        .credit(self.left, Amount::<P>::from_minor(minor), Currency::EUR)
        .debit(self.right, Amount::<P>::from_minor(minor), Currency::EUR)
        .seal(&self.ctx())
        .ok()
    }

    /// A genuine reversal of `original`.
    fn reverse<const P: u8>(
        &self,
        original: &Entry<Balanced, P>,
        key: &[u8],
        on: time::Date,
    ) -> Option<Entry<Balanced, P>> {
        original
            .reverse(
                EntryId::generate(),
                IdempotencyKey::new(key.to_vec()).ok()?,
                on,
            )
            .seal(&self.ctx())
            .ok()
    }

    /// An entry that names `original` as reversed but posts something else.
    fn forged_reversal<const P: u8>(
        &self,
        original: &Entry<Balanced, P>,
        key: &[u8],
    ) -> Option<Entry<Balanced, P>> {
        Entry::<Draft, P>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).ok()?,
            date!(2026 - 04 - 05),
        )
        .reversing(original.id(), original.booking_date())
        .debit(self.right, Amount::<P>::from_minor(1), Currency::EUR)
        .credit(self.left, Amount::<P>::from_minor(1), Currency::EUR)
        .seal(&self.ctx())
        .ok()
    }

    /// A clearing over the given postings.
    fn clearing<const P: u8>(&self, items: &[(PostingRef, i64)]) -> Clearing<P> {
        Clearing {
            id: crate::clearing::ClearingId::generate(),
            account: self.left,
            currency: Currency::EUR,
            cleared_on: date!(2026 - 04 - 20),
            items: items
                .iter()
                .map(|(posting, applied)| ClearedItem {
                    posting: *posting,
                    applied: Amount::<P>::from_minor(*applied),
                })
                .collect(),
        }
    }

    fn entry_with_id<const P: u8>(
        &self,
        id: EntryId,
        key: &[u8],
        minor: i64,
    ) -> Option<Entry<Balanced, P>> {
        Entry::<Draft, P>::new(
            id,
            IdempotencyKey::new(key.to_vec()).ok()?,
            date!(2026 - 03 - 15),
        )
        .debit(self.left, Amount::<P>::from_minor(minor), Currency::EUR)
        .credit(self.right, Amount::<P>::from_minor(minor), Currency::EUR)
        .seal(&self.ctx())
        .ok()
    }

    /// An entry carrying a caller-defined `kind` label.
    fn entry_with_kind<const P: u8>(
        &self,
        key: &[u8],
        minor: i64,
        kind: &str,
    ) -> Option<Entry<Balanced, P>> {
        Entry::<Draft, P>::new(
            EntryId::generate(),
            IdempotencyKey::new(key.to_vec()).ok()?,
            date!(2026 - 03 - 15),
        )
        .debit(self.left, Amount::<P>::from_minor(minor), Currency::EUR)
        .credit(self.right, Amount::<P>::from_minor(minor), Currency::EUR)
        .with_kind(crate::dimensions::Label::new(kind).ok()?)
        .seal(&self.ctx())
        .ok()
    }
}

/// Runs every conformance check against an **empty** store.
///
/// Checks are independent but share the store, so a backend that fails an early
/// check may cascade; read the first failure first.
pub async fn check_all<const P: u8, S: LedgerStore<P>>(store: &S) -> Report {
    let mut checks = Vec::new();
    checks.push(check_starts_empty(store).await);
    checks.push(check_append_assigns_dense_indices(store).await);
    checks.push(check_reads_are_stable(store).await);
    checks.push(check_idempotent_replay(store).await);
    checks.push(check_idempotency_conflict(store).await);
    checks.push(check_batch_is_atomic(store).await);
    checks.push(check_pagination_covers_the_log(store).await);
    checks.push(check_balances_match_the_log(store).await);
    checks.push(check_proofs_verify(store).await);
    checks.push(check_reversal_rules(store).await);
    checks.push(check_clearing_rules(store).await);
    checks.push(check_open_items_track_residuals(store).await);
    checks.push(check_account_bindings_survive_a_restart(store).await);
    checks.push(check_kind_survives_a_round_trip(store).await);
    Report { checks }
}

/// An entry's `kind` label survives a store round-trip.
///
/// `kind` is part of the content hash, so a backend that fails to persist and
/// rehydrate it makes every kinded entry unreadable — [`get`](LedgerStore::get)
/// rehydrates through `adopt_verified`, which recomputes the hash and rejects a
/// mismatch. The check also asserts the statement line carries the kind, so a
/// caller can group by document type without a second lookup.
pub async fn check_kind_survives_a_round_trip<const P: u8, S: LedgerStore<P>>(
    store: &S,
) -> CheckResult {
    const NAME: &str = "entry kind survives a round-trip";
    let f = Fixture::new();
    let Some(entry) = f.entry_with_kind::<P>(b"kinded", 5150, "INVOICE") else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let id = entry.id();
    if let Err(e) = store.append(&EntryBatch::single(entry)).await {
        // A hash mismatch here is exactly the "kind not persisted" failure.
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }
    // The statement half of this check reads the log, which a deferred backend
    // does not place the entry into until the sequencer runs. Without this the
    // check would silently only ever hold for inline sequencing.
    match sequence_until_placed(store, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return CheckResult::fail(NAME, "the kinded entry was never sequenced"),
        Err(e) => return CheckResult::fail(NAME, e),
    }
    match store.get(id).await {
        Ok(Some(r)) => {
            let got = r.entry.kind().map(|k| k.as_str().to_owned());
            if got.as_deref() != Some("INVOICE") {
                return CheckResult::fail(
                    NAME,
                    format!("kind did not round-trip: expected Some(\"INVOICE\"), got {got:?}"),
                );
            }
        }
        Ok(None) => return CheckResult::fail(NAME, "a kinded entry was not found"),
        Err(e) => return CheckResult::fail(NAME, format!("get failed (hash mismatch?): {e}")),
    }

    // And the statement line exposes it.
    let key = BalanceKey {
        account: f.left,
        currency: Currency::EUR,
        layer: Layer::Settled,
    };
    match store.statement(key, Cursor::start()).await {
        Ok(page) => {
            let seen = page
                .lines
                .iter()
                .any(|l| l.kind.as_ref().map(|k| k.as_str()) == Some("INVOICE"));
            if seen {
                CheckResult::pass(NAME)
            } else {
                CheckResult::fail(NAME, "statement line did not carry the entry kind")
            }
        }
        Err(e) => CheckResult::fail(NAME, format!("statement failed: {e}")),
    }
}

/// Account handles read back exactly as they were written.
///
/// A handle is a position in registration order, and that position is written
/// into every posting row and into the trial balance leaves a seal commits to.
/// A backend that loses the binding — or reissues it from iteration order —
/// silently repoints history on the next restart, so the round trip is part of
/// the contract rather than a convenience.
pub async fn check_account_bindings_survive_a_restart<const P: u8, S: LedgerStore<P>>(
    store: &S,
) -> CheckResult {
    const NAME: &str = "account bindings survive a restart";

    // Start from whatever the store already holds, so the check extends an
    // existing binding set rather than assuming it owns index zero.
    let existing = match store.accounts().await {
        Ok(records) => records,
        Err(e) => return CheckResult::fail(NAME, format!("accounts failed: {e}")),
    };
    let mut registry = match AccountRegistry::from_records(existing) {
        Ok(r) => r,
        Err(e) => return CheckResult::fail(NAME, format!("stored bindings are unusable: {e}")),
    };

    // Paths deliberately out of lexical order, so a backend that rebuilds by
    // sorting paths rather than by stored index fails here.
    let mut expected = Vec::new();
    for path in ["Zzconf:Late", "Aaconf:Early", "Mmconf:Middle"] {
        match registry.register(
            Account::new(
                match AccountPath::parse(path) {
                    Ok(p) => p,
                    Err(e) => return CheckResult::fail(NAME, format!("bad fixture path: {e}")),
                },
                date!(2000 - 01 - 01),
            )
            .with_kind(AccountKind::Asset)
            .closing_on(date!(2030 - 12 - 31)),
        ) {
            Ok(id) => expected.push((id, path)),
            Err(e) => return CheckResult::fail(NAME, format!("fixture registration failed: {e}")),
        }
    }

    for record in registry.records() {
        if let Err(e) = store.register_account(&record).await {
            return CheckResult::fail(NAME, format!("register_account failed: {e}"));
        }
    }

    let stored = match store.accounts().await {
        Ok(records) => records,
        Err(e) => return CheckResult::fail(NAME, format!("accounts failed: {e}")),
    };

    for (id, path) in &expected {
        let Some(found) = stored.iter().find(|r| r.id == *id) else {
            return CheckResult::fail(NAME, format!("handle {id} for {path} was not stored"));
        };
        if found.account.path.to_string() != *path {
            return CheckResult::fail(
                NAME,
                format!(
                    "handle {id} came back as {}, not {path}",
                    found.account.path
                ),
            );
        }
        // Classification and closing date are part of the binding: validation
        // reads them, so a backend that drops them changes what may be posted.
        if found.account.kind != Some(AccountKind::Asset) {
            return CheckResult::fail(NAME, format!("handle {id} lost its kind"));
        }
        if found.account.closed_on != Some(date!(2030 - 12 - 31)) {
            return CheckResult::fail(NAME, format!("handle {id} lost its closing date"));
        }
    }

    // Rebuilding from the stored records must reproduce the registry exactly,
    // handles included — that is what makes a restart safe.
    let rebuilt = match AccountRegistry::from_records(stored) {
        Ok(r) => r,
        Err(e) => return CheckResult::fail(NAME, format!("registry would not rebuild: {e}")),
    };
    if rebuilt.commitment() != registry.commitment() {
        return CheckResult::fail(NAME, "rebuilt registry does not match the original");
    }

    CheckResult::pass(NAME)
}

/// Corrections follow the rules: at most one reversal, never of a reversal,
/// never a claim that does not actually invert.
pub async fn check_reversal_rules<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "reversal rules are enforced";
    let f = Fixture::new();

    let Some(original) = f.entry::<P>(b"rev-original", 1000) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    if let Err(e) = store.append(&EntryBatch::single(original.clone())).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }

    let Some(reversal) = f.reverse(&original, b"rev-first", date!(2026 - 04 - 01)) else {
        return CheckResult::fail(NAME, "reversal failed to seal");
    };
    let reversal_clone = reversal.clone();
    if let Err(e) = store.append(&EntryBatch::single(reversal)).await {
        return CheckResult::fail(NAME, format!("a valid reversal was rejected: {e}"));
    }

    // A second reversal of the same entry.
    let Some(second) = f.reverse(&original, b"rev-second", date!(2026 - 04 - 02)) else {
        return CheckResult::fail(NAME, "reversal failed to seal");
    };
    if store.append(&EntryBatch::single(second)).await.is_ok() {
        return CheckResult::fail(NAME, "an entry was reversed twice");
    }

    // A reversal of a reversal.
    let Some(chained) = f.reverse(&reversal_clone, b"rev-chained", date!(2026 - 04 - 03)) else {
        return CheckResult::fail(NAME, "reversal failed to seal");
    };
    if store.append(&EntryBatch::single(chained)).await.is_ok() {
        return CheckResult::fail(NAME, "a reversal was itself reversed");
    }

    // A claim that does not invert. Uses a *fresh* original that has not been
    // reversed, so this cannot pass on the at-most-once rule instead.
    let Some(untouched) = f.entry::<P>(b"rev-untouched", 500) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    if let Err(e) = store.append(&EntryBatch::single(untouched.clone())).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }
    let Some(forged) = f.forged_reversal::<P>(&untouched, b"rev-forged") else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    if store.append(&EntryBatch::single(forged)).await.is_ok() {
        return CheckResult::fail(
            NAME,
            "an entry claiming a reversal it does not perform was accepted",
        );
    }

    // Reversing an entry that is not in the store at all.
    let Some(absent) = f.entry::<P>(b"rev-absent", 700) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let Some(orphan) = f.reverse(&absent, b"rev-orphan", date!(2026 - 04 - 06)) else {
        return CheckResult::fail(NAME, "reversal failed to seal");
    };
    if store.append(&EntryBatch::single(orphan)).await.is_ok() {
        return CheckResult::fail(NAME, "a reversal of an unknown entry was accepted");
    }

    CheckResult::pass(NAME)
}

/// Clearing validates its own rules and never moves money.
pub async fn check_clearing_rules<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "clearing rules are enforced";
    let f = Fixture::new();

    let Some(invoice) = f.entry::<P>(b"clr-invoice", 1000) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let invoice_id = invoice.id();
    if let Err(e) = store.append(&EntryBatch::single(invoice)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }

    let Some(payment) = f.reversed_entry::<P>(b"clr-payment", 400) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let payment_id = payment.id();
    if let Err(e) = store.append(&EntryBatch::single(payment)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }

    let key = BalanceKey {
        account: f.left,
        currency: Currency::EUR,
        layer: Layer::Settled,
    };
    let before = match store.trial_balance(None).await {
        Ok(tb) => tb,
        Err(e) => return CheckResult::fail(NAME, format!("trial_balance failed: {e}")),
    };

    let invoice_ref = PostingRef::new(invoice_id, 0);
    let payment_ref = PostingRef::new(payment_id, 0);

    // Applying more than a posting has open.
    if store
        .clear(f.clearing::<P>(&[(invoice_ref, 9_000), (payment_ref, 9_000)]))
        .await
        .is_ok()
    {
        return CheckResult::fail(NAME, "an over-application was accepted");
    }

    // Sides that do not match.
    if store
        .clear(f.clearing::<P>(&[(invoice_ref, 400), (payment_ref, 300)]))
        .await
        .is_ok()
    {
        return CheckResult::fail(NAME, "an unbalanced clearing was accepted");
    }

    // The same posting twice in one clearing.
    if store
        .clear(f.clearing::<P>(&[(invoice_ref, 200), (invoice_ref, 200)]))
        .await
        .is_ok()
    {
        return CheckResult::fail(NAME, "a duplicated item was accepted");
    }

    // A single item cannot clear anything.
    if store
        .clear(f.clearing::<P>(&[(invoice_ref, 400)]))
        .await
        .is_ok()
    {
        return CheckResult::fail(NAME, "a one-sided clearing was accepted");
    }

    // A valid partial application.
    let valid = f.clearing::<P>(&[(invoice_ref, 400), (payment_ref, 400)]);
    let clearing_id = valid.id;
    if let Err(e) = store.clear(valid).await {
        return CheckResult::fail(NAME, format!("a valid clearing was rejected: {e}"));
    }

    // The same identifier again.
    let mut duplicate = f.clearing::<P>(&[(invoice_ref, 100), (payment_ref, 100)]);
    duplicate.id = clearing_id;
    if store.clear(duplicate).await.is_ok() {
        return CheckResult::fail(NAME, "a duplicate clearing identifier was accepted");
    }

    // Clearing is an assignment, never a movement.
    match store.trial_balance(None).await {
        Ok(after) if after == before => {}
        Ok(_) => return CheckResult::fail(NAME, "clearing changed a balance"),
        Err(e) => return CheckResult::fail(NAME, format!("trial_balance failed: {e}")),
    }

    // Releasing it, then releasing it again.
    if let Err(e) = store
        .reset_clearing(clearing_id, date!(2026 - 05 - 01))
        .await
    {
        return CheckResult::fail(NAME, format!("a valid reset was rejected: {e}"));
    }
    if store
        .reset_clearing(clearing_id, date!(2026 - 05 - 02))
        .await
        .is_ok()
    {
        return CheckResult::fail(NAME, "a clearing was reset twice");
    }
    if store
        .reset_clearing(
            crate::clearing::ClearingId::generate(),
            date!(2026 - 05 - 03),
        )
        .await
        .is_ok()
    {
        return CheckResult::fail(NAME, "an unknown clearing was reset");
    }

    let _ = key;
    CheckResult::pass(NAME)
}

/// Residuals reflect exactly what has been applied.
pub async fn check_open_items_track_residuals<const P: u8, S: LedgerStore<P>>(
    store: &S,
) -> CheckResult {
    if let Err(e) = drain_sequencing(store).await {
        return CheckResult::fail("sequencing", e);
    }
    const NAME: &str = "open items track residuals";
    let f = Fixture::new();

    let Some(invoice) = f.entry::<P>(b"oi-invoice", 1000) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let invoice_id = invoice.id();
    if let Err(e) = store.append(&EntryBatch::single(invoice)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }
    let Some(payment) = f.reversed_entry::<P>(b"oi-payment", 250) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let payment_id = payment.id();
    if let Err(e) = store.append(&EntryBatch::single(payment)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }

    let key = BalanceKey {
        account: f.left,
        currency: Currency::EUR,
        layer: Layer::Settled,
    };
    let invoice_ref = PostingRef::new(invoice_id, 0);
    let payment_ref = PostingRef::new(payment_id, 0);

    let clearing = f.clearing::<P>(&[(invoice_ref, 250), (payment_ref, 250)]);
    let clearing_id = clearing.id;
    if let Err(e) = store.clear(clearing).await {
        return CheckResult::fail(NAME, format!("a valid clearing was rejected: {e}"));
    }

    let open = match store.open_items(key).await {
        Ok(o) => o,
        Err(e) => return CheckResult::fail(NAME, format!("open_items failed: {e}")),
    };
    let Some(item) = open.iter().find(|i| i.posting == invoice_ref) else {
        return CheckResult::fail(NAME, "the partly-settled invoice is not open");
    };
    if item.applied != Amount::<P>::from_minor(250) {
        return CheckResult::fail(NAME, format!("applied is {}, expected 250", item.applied));
    }
    if item.residual != Amount::<P>::from_minor(750) {
        return CheckResult::fail(NAME, format!("residual is {}, expected 750", item.residual));
    }
    if open.iter().any(|i| i.posting == payment_ref) {
        return CheckResult::fail(NAME, "a fully applied posting is still open");
    }

    // Releasing reopens both.
    if let Err(e) = store
        .reset_clearing(clearing_id, date!(2026 - 05 - 10))
        .await
    {
        return CheckResult::fail(NAME, format!("reset failed: {e}"));
    }
    match store.open_items(key).await {
        Ok(after) => {
            let invoice_back = after
                .iter()
                .any(|i| i.posting == invoice_ref && i.residual == Amount::<P>::from_minor(1000));
            let payment_back = after.iter().any(|i| i.posting == payment_ref);
            if invoice_back && payment_back {
                CheckResult::pass(NAME)
            } else {
                CheckResult::fail(NAME, "a reset did not reopen both items")
            }
        }
        Err(e) => CheckResult::fail(NAME, format!("open_items failed: {e}")),
    }
}

/// A fresh store holds nothing and commits to the empty root.
pub async fn check_starts_empty<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "starts empty";
    match store.len().await {
        Ok(0) => {}
        Ok(n) => return CheckResult::fail(NAME, format!("expected an empty store, found {n}")),
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
    }
    match store.head().await {
        Ok(head) if head.size == 0 && head.root == crate::merkle::empty_root() => {
            CheckResult::pass(NAME)
        }
        Ok(head) => CheckResult::fail(NAME, format!("empty store has head {head:?}")),
        Err(e) => CheckResult::fail(NAME, format!("head failed: {e}")),
    }
}

/// Indices start at zero and increase by one, in append order.
pub async fn check_append_assigns_dense_indices<const P: u8, S: LedgerStore<P>>(
    store: &S,
) -> CheckResult {
    const NAME: &str = "append assigns dense, ordered indices";
    let f = Fixture::new();
    let start = match store.len().await {
        Ok(n) => n,
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
    };

    for i in 0..3u64 {
        let key = format!("dense-{i}");
        let Some(entry) = f.entry::<P>(key.as_bytes(), 100) else {
            return CheckResult::fail(NAME, "fixture entry failed to seal");
        };
        let recorded = match store.append(&EntryBatch::single(entry)).await {
            Ok(r) => r,
            Err(e) => return CheckResult::fail(NAME, format!("append failed: {e}")),
        };
        let Some(first) = recorded.first() else {
            return CheckResult::fail(NAME, "append returned no outcome");
        };
        if !first.is_new {
            return CheckResult::fail(NAME, "a fresh key was reported as a replay");
        }

        // A deferred backend has no position yet; sequencing must produce one.
        let placed = if first.index.is_some() {
            first.index
        } else {
            match sequence_until_placed(store, first.id).await {
                Ok(index) => index,
                Err(e) => return CheckResult::fail(NAME, e),
            }
        };

        let expected = start.saturating_add(i);
        match placed {
            Some(index) if index.get() == expected => {}
            Some(index) => {
                return CheckResult::fail(
                    NAME,
                    format!("expected index {expected}, got {}", index.get()),
                );
            }
            None => {
                return CheckResult::fail(NAME, "an entry has no position after sequencing");
            }
        }
    }
    CheckResult::pass(NAME)
}

/// The same record read twice is identical, by identifier and by page.
pub async fn check_reads_are_stable<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "reads are stable";
    let f = Fixture::new();
    let Some(entry) = f.entry::<P>(b"stable", 4242) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let id = entry.id();
    let expected_hash = entry.content_hash();

    if let Err(e) = store.append(&EntryBatch::single(entry)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }

    let first = match store.get(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return CheckResult::fail(NAME, "an appended entry was not found"),
        Err(e) => return CheckResult::fail(NAME, format!("get failed: {e}")),
    };
    if first.content_hash != expected_hash {
        return CheckResult::fail(NAME, "stored content hash differs from the entry's own");
    }

    match store.get(id).await {
        Ok(Some(second)) if second == first => CheckResult::pass(NAME),
        Ok(Some(_)) => CheckResult::fail(NAME, "two reads of one record disagreed"),
        Ok(None) => CheckResult::fail(NAME, "a record disappeared between reads"),
        Err(e) => CheckResult::fail(NAME, format!("get failed: {e}")),
    }
}

/// Re-appending identical content under the same key changes nothing.
pub async fn check_idempotent_replay<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "replay is idempotent";
    let f = Fixture::new();
    let Some(first) = f.entry::<P>(b"replay", 777) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let original = match store.append(&EntryBatch::single(first)).await {
        Ok(r) => match r.first().copied() {
            Some(r) => r,
            None => return CheckResult::fail(NAME, "append returned no outcome"),
        },
        Err(e) => return CheckResult::fail(NAME, format!("append failed: {e}")),
    };

    let before = match store.len().await {
        Ok(n) => n,
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
    };

    // A different entry identifier, but the same logical transaction.
    let Some(again) = f.entry::<P>(b"replay", 777) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    match store.append(&EntryBatch::single(again)).await {
        Ok(r) => {
            let Some(replay) = r.first() else {
                return CheckResult::fail(NAME, "append returned no outcome");
            };
            if replay.is_new {
                return CheckResult::fail(NAME, "a replay was reported as a new entry");
            }
            if replay.index != original.index || replay.id != original.id {
                return CheckResult::fail(NAME, "a replay returned a different record");
            }
        }
        Err(e) => return CheckResult::fail(NAME, format!("a safe replay was rejected: {e}")),
    }

    match store.len().await {
        Ok(after) if after == before => CheckResult::pass(NAME),
        Ok(after) => CheckResult::fail(
            NAME,
            format!("a replay appended: {before} entries became {after}"),
        ),
        Err(e) => CheckResult::fail(NAME, format!("len failed: {e}")),
    }
}

/// The same key with different content is refused, not overwritten.
pub async fn check_idempotency_conflict<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "conflicting key is refused";
    let f = Fixture::new();
    let Some(first) = f.entry::<P>(b"conflict", 100) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    if let Err(e) = store.append(&EntryBatch::single(first)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }
    let before = match store.len().await {
        Ok(n) => n,
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
    };

    let Some(different) = f.entry::<P>(b"conflict", 999) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    if store.append(&EntryBatch::single(different)).await.is_ok() {
        return CheckResult::fail(NAME, "a conflicting key was accepted");
    }

    match store.len().await {
        Ok(after) if after == before => CheckResult::pass(NAME),
        Ok(after) => CheckResult::fail(
            NAME,
            format!("a refused append still wrote: {before} became {after}"),
        ),
        Err(e) => CheckResult::fail(NAME, format!("len failed: {e}")),
    }
}

/// A batch that cannot land in full lands not at all.
pub async fn check_batch_is_atomic<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    const NAME: &str = "batches are atomic";
    let f = Fixture::new();

    // Poison the batch: the second entry reuses the first's key with different
    // content, so the batch must be refused as a whole.
    let Some(good) = f.entry::<P>(b"atomic-a", 100) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let Some(poison_first) = f.entry::<P>(b"atomic-b", 200) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let Some(poison_second) = f.entry::<P>(b"atomic-b", 300) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    let good_id = good.id();

    let Ok(batch) = EntryBatch::new(vec![good, poison_first, poison_second]) else {
        return CheckResult::fail(NAME, "batch construction failed");
    };

    let before = match store.len().await {
        Ok(n) => n,
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
    };

    if store.append(&batch).await.is_ok() {
        return CheckResult::fail(NAME, "a batch with a conflicting entry was accepted");
    }

    match store.len().await {
        Ok(after) if after != before => {
            return CheckResult::fail(
                NAME,
                format!("a refused batch partly landed: {before} became {after}"),
            );
        }
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
        Ok(_) => {}
    }

    match store.get(good_id).await {
        Ok(None) => CheckResult::pass(NAME),
        Ok(Some(_)) => CheckResult::fail(NAME, "the valid part of a refused batch was kept"),
        Err(e) => CheckResult::fail(NAME, format!("get failed: {e}")),
    }
}

/// Paging visits every record exactly once, in order.
pub async fn check_pagination_covers_the_log<const P: u8, S: LedgerStore<P>>(
    store: &S,
) -> CheckResult {
    if let Err(e) = drain_sequencing(store).await {
        return CheckResult::fail("sequencing", e);
    }
    const NAME: &str = "pagination covers the log";
    let expected = match store.len().await {
        Ok(n) => n,
        Err(e) => return CheckResult::fail(NAME, format!("len failed: {e}")),
    };

    let mut seen: Vec<u64> = Vec::new();
    let mut cursor = Some(Cursor::start().with_limit(2));
    let mut guard = 0u32;

    while let Some(c) = cursor {
        guard = guard.saturating_add(1);
        if u64::from(guard) > expected.saturating_add(16) {
            return CheckResult::fail(NAME, "pagination did not terminate");
        }
        match store.page(c).await {
            Ok(page) => {
                for record in &page.records {
                    match record.require_index() {
                        Ok(index) => seen.push(index.get()),
                        Err(e) => {
                            return CheckResult::fail(
                                NAME,
                                format!("a page returned an unsequenced entry: {e}"),
                            );
                        }
                    }
                }
                cursor = page.next;
            }
            Err(e) => return CheckResult::fail(NAME, format!("page failed: {e}")),
        }
    }

    let wanted: Vec<u64> = (0..expected).collect();
    if seen == wanted {
        CheckResult::pass(NAME)
    } else {
        CheckResult::fail(NAME, format!("expected indices {wanted:?}, paged {seen:?}"))
    }
}

/// Balances agree with a fold over the paged log.
pub async fn check_balances_match_the_log<const P: u8, S: LedgerStore<P>>(
    store: &S,
) -> CheckResult {
    if let Err(e) = drain_sequencing(store).await {
        return CheckResult::fail("sequencing", e);
    }
    const NAME: &str = "balances match the log";
    let f = Fixture::new();
    let key = BalanceKey {
        account: f.left,
        currency: Currency::EUR,
        layer: Layer::Settled,
    };

    let reported = match store.balance(key, None).await {
        Ok(b) => b,
        Err(e) => return CheckResult::fail(NAME, format!("balance failed: {e}")),
    };

    // Recompute from the log itself.
    let mut folded = crate::balance::Balance::<P>::ZERO;
    let mut cursor = Some(Cursor::start());
    while let Some(c) = cursor {
        match store.page(c).await {
            Ok(page) => {
                for record in &page.records {
                    for posting in record.entry.postings() {
                        if posting.account == key.account
                            && posting.currency == key.currency
                            && posting.layer == key.layer
                            && folded.add(posting.direction, posting.amount).is_err()
                        {
                            return CheckResult::fail(NAME, "folding the log overflowed");
                        }
                    }
                }
                cursor = page.next;
            }
            Err(e) => return CheckResult::fail(NAME, format!("page failed: {e}")),
        }
    }

    if folded == reported {
        CheckResult::pass(NAME)
    } else {
        CheckResult::fail(
            NAME,
            format!("store reported {reported:?}, the log folds to {folded:?}"),
        )
    }
}

/// Every record is provably included, and growth is provably append-only.
pub async fn check_proofs_verify<const P: u8, S: LedgerStore<P>>(store: &S) -> CheckResult {
    if let Err(e) = drain_sequencing(store).await {
        return CheckResult::fail("sequencing", e);
    }
    const NAME: &str = "proofs verify";
    let head = match store.head().await {
        Ok(h) => h,
        Err(e) => return CheckResult::fail(NAME, format!("head failed: {e}")),
    };
    if head.size == 0 {
        return CheckResult::fail(NAME, "no entries to prove");
    }

    let mut cursor = Some(Cursor::start());
    while let Some(c) = cursor {
        match store.page(c).await {
            Ok(page) => {
                for record in &page.records {
                    let Ok(index) = record.require_index() else {
                        return CheckResult::fail(NAME, "a page returned an unsequenced entry");
                    };
                    match store.prove_inclusion(index).await {
                        Ok(proof) => {
                            if !proof.verify(&record.content_hash, &head.root) {
                                return CheckResult::fail(
                                    NAME,
                                    format!("inclusion proof failed for index {index}"),
                                );
                            }
                        }
                        Err(e) => {
                            return CheckResult::fail(NAME, format!("prove_inclusion failed: {e}"));
                        }
                    }
                }
                cursor = page.next;
            }
            Err(e) => return CheckResult::fail(NAME, format!("page failed: {e}")),
        }
    }

    // Append one more, then prove the earlier head was a prefix.
    let f = Fixture::new();
    let Some(entry) = f.entry::<P>(b"proof-growth", 55) else {
        return CheckResult::fail(NAME, "fixture entry failed to seal");
    };
    if let Err(e) = store.append(&EntryBatch::single(entry)).await {
        return CheckResult::fail(NAME, format!("append failed: {e}"));
    }

    let grown = match store.head().await {
        Ok(h) => h,
        Err(e) => return CheckResult::fail(NAME, format!("head failed: {e}")),
    };
    match store.prove_consistency(head.size).await {
        Ok(proof) if proof.verify(&head.root, &grown.root) => CheckResult::pass(NAME),
        Ok(_) => CheckResult::fail(NAME, "consistency proof did not verify"),
        Err(e) => CheckResult::fail(NAME, format!("prove_consistency failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStore;

    #[test]
    fn the_memory_store_conforms() {
        let report = block_on(check_all(&MemoryStore::<2>::new(test_ledger())));
        report.assert_passed();
        assert_eq!(report.checks.len(), 14);
    }

    #[test]
    fn block_on_drives_a_future_to_completion() {
        assert_eq!(block_on(async { 1 + 1 }), 2);
    }

    #[test]
    fn a_report_renders_its_failures() {
        let report = Report {
            checks: vec![
                CheckResult::pass("fine"),
                CheckResult::fail("broken", "it did the wrong thing"),
            ],
        };
        assert!(!report.passed());
        assert_eq!(report.failures().len(), 1);
        let rendered = report.to_string();
        assert!(rendered.contains("broken"), "{rendered}");
        assert!(rendered.contains("it did the wrong thing"), "{rendered}");
    }

    #[test]
    fn a_passing_report_says_so() {
        let report = Report {
            checks: vec![CheckResult::pass("fine")],
        };
        assert_eq!(report.to_string(), "all 1 conformance checks passed");
    }
}
