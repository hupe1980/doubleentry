//! Store checks shared by `tests/memory.rs`, `tests/sqlite.rs` and
//! `tests/postgres.rs`: the store acceptance scenarios and the bounded-write
//! regression, each generic over the store.
// Each store's test file uses the checks that apply to it.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::PageRead;
use doubleentry::store::{
    BalanceRead, BalanceScope, EntryRef, LeafRead, PostingCursor, PostingOrder, PostingRead, Store,
    StoreErrorKind,
};
use doubleentry::{
    AccountHandle, AccountKind, AccountSpec, Amount, BalanceLimit, Date, Entry, EntryId, Error,
    IdempotencyKey, Journal, Label, Layer, Ledger, LedgerId, MemoSalt, PostAmount, Posting,
    Rejection, Side,
    currency::EUR,
    date,
    log::{InclusionProof, leaf_hash},
    record::Genesis,
    store::BalanceKey,
};

/// The ledger the checks book into.
pub fn ledger() -> LedgerId {
    LedgerId::new("example.com/acme").unwrap()
}

pub fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

pub fn sale(key: &str, on: Date, amount: &str) -> Entry {
    Entry::new(key, on)
        .debit("Assets:Cash", eur(amount), EUR)
        .credit("Income:Sales", eur(amount), EUR)
}

pub fn id(key: &str) -> EntryId {
    EntryId::derive(&ledger(), &IdempotencyKey::new(key).unwrap())
}

/// EUR, cash, sales, a limited wallet, rent, retained earnings and the
/// months of 2026, on a journal and a ledger alike.
pub async fn books<S: Store<2>>(store: S) -> (Journal, Ledger<S>) {
    let mut journal = Journal::new(ledger())
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([7; 16]));
    let books = Ledger::new(store, ledger())
        .await
        .unwrap()
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([7; 16]));
    let on = date!("2026-01-01");
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Income:Sales", AccountKind::Income),
        ("Expenses:Rent", AccountKind::Expense),
        ("Equity:Retained", AccountKind::Equity),
    ] {
        assert_eq!(
            journal.open(path, kind, on).unwrap(),
            books.open(path, kind, on).await.unwrap()
        );
    }
    let wallet = AccountSpec::new("Liabilities:Wallet", AccountKind::Liability, on)
        .limit(BalanceLimit::NoDebitBalance);
    assert_eq!(
        journal.register(wallet.clone()).unwrap(),
        books.register(wallet).await.unwrap()
    );
    journal.declare_currency(EUR, 2, on).unwrap();
    books.declare_currency(EUR, 2, on).await.unwrap();
    journal.define_months(2026, 1..=12).unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    assert_eq!(journal.head(), books.head().await.unwrap());
    (journal, books)
}

/// US1-1: `create` then `open`: size 1, leaf 0 the genesis record.
pub async fn us1_create_then_open<S: Store<2>>(store: S) {
    let genesis = Genesis { ledger: ledger() };
    let created = store.create(&genesis).await.unwrap();
    assert_eq!(
        (created.ledger.clone(), created.precision, created.size),
        (ledger(), 2, 1)
    );
    assert_eq!(store.open().await.unwrap(), created);
    let leaves = store
        .read_leaves(&LeafRead {
            from: 0,
            through: None,
            kinds: None,
            limit: 10,
        })
        .await
        .unwrap();
    let genesis_leaf = Journal::new(ledger())
        .leaves()
        .next()
        .unwrap()
        .bytes
        .to_vec();
    assert_eq!(leaves.items.len(), 1);
    assert_eq!(leaves.items[0].bytes, genesis_leaf);
    assert_eq!(created.genesis, leaf_hash(&genesis_leaf));
}

/// US1-2: the same drafts give the same receipts and heads as the journal.
pub async fn us1_journal_parity<S: Store<2>>(store: S) {
    let (mut journal, books) = books(store).await;
    for (key, amount) in [("a", "10.00"), ("b", "20.00"), ("c", "30.00")] {
        let draft = sale(key, date!("2026-03-15"), amount).memo(format!("memo {key}"));
        assert_eq!(
            journal.record(draft.clone()).unwrap(),
            books.record(draft).await.unwrap()
        );
    }
    assert_eq!(journal.head(), books.head().await.unwrap());
}

/// US1-3: a refused draft is the core's exact rejection and writes nothing.
pub async fn us1_rejection_writes_nothing<S: Store<2>>(store: S) {
    let (mut journal, books) = books(store).await;
    let size = books.head().await.unwrap().size;
    let overdraw = Entry::new("overdraw", date!("2026-03-15"))
        .debit("Liabilities:Wallet", eur("10.00"), EUR)
        .credit("Assets:Cash", eur("10.00"), EUR);
    let expected = journal.record(overdraw.clone()).unwrap_err();
    let actual = books.record(overdraw).await.unwrap_err();
    assert!(matches!(
        actual.rejection(),
        Some(Rejection::LimitExceeded { .. })
    ));
    assert_eq!(expected.rejection(), actual.rejection());
    assert_eq!(books.head().await.unwrap().size, size);
}

/// US1-4: an identical retry replays; other content under the key conflicts.
pub async fn us1_replay_and_key_conflict<S: Store<2>>(store: S) {
    let (_, books) = books(store).await;
    let first = books
        .record(sale("k", date!("2026-03-15"), "10.00"))
        .await
        .unwrap();
    let size = books.head().await.unwrap().size;
    let again = books
        .record(sale("k", date!("2026-03-15"), "10.00"))
        .await
        .unwrap();
    assert!(again.replayed && (again.id, again.position) == (first.id, first.position));
    assert_eq!(books.head().await.unwrap().size, size);
    let conflict = books
        .record(sale("k", date!("2026-03-15"), "11.00"))
        .await
        .unwrap_err();
    assert_eq!(
        conflict.rejection(),
        Some(&Rejection::KeyConflict {
            position: first.position
        })
    );
}

/// US1-5 (first half): `open` before `create` is `NotCreated`, never
/// auto-created.
pub async fn us1_open_before_create_is_not_created<S: Store<2>>(store: S) {
    let error = store.open().await.unwrap_err();
    assert_eq!(error.kind(), &StoreErrorKind::NotCreated);
    assert_eq!(
        store.open().await.unwrap_err().kind(),
        &StoreErrorKind::NotCreated
    );
}

/// US1-6: another genesis is refused and leaf 0 kept; the identical one
/// succeeds without writing.
pub async fn us1_other_genesis_refused_identical_accepted<S: Store<2>>(store: S) {
    let genesis = Genesis { ledger: ledger() };
    let created = store.create(&genesis).await.unwrap();
    let other = Genesis {
        ledger: LedgerId::new("example.com/b").unwrap(),
    };
    assert_eq!(
        store.create(&other).await.unwrap_err().kind(),
        &StoreErrorKind::LedgerMismatch
    );
    assert_eq!(store.create(&genesis).await.unwrap(), created);
    assert_eq!(store.size().await.unwrap(), 1);
    // Through the façade: another ledger id is a configuration error.
    let error = Ledger::new(store, LedgerId::new("example.com/b").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error}");
}

/// US4-1: the balance after a position is the `balance_after` of the last
/// posting below it — no sum.
pub async fn us4_balance_at_a_position<S: Store<2>>(store: S) {
    let (_, books) = books(store.clone()).await;
    let mut positions = Vec::new();
    for (n, amount) in ["1.00", "2.00", "4.00"].into_iter().enumerate() {
        positions.push(
            books
                .record(sale(&format!("at-{n}"), date!("2026-03-15"), amount))
                .await
                .unwrap()
                .position,
        );
        books
            .record(
                Entry::new(format!("other-{n}"), date!("2026-03-15"))
                    .debit("Expenses:Rent", eur("1.00"), EUR)
                    .credit("Equity:Retained", eur("1.00"), EUR),
            )
            .await
            .unwrap();
    }
    let cash = BalanceKey {
        handle: AccountHandle::new(0).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
    };
    let balance = async |scope| {
        let read = BalanceRead {
            scope,
            keys: Some(vec![cash]),
            page: PageRead::first(1),
        };
        store.balances(&read).await.unwrap().items[0].1
    };
    let at = balance(BalanceScope::At(positions[1] + 1)).await;
    assert_eq!((at.debit(), at.postings()), (eur("3.00"), 2));
    let at_second = balance(BalanceScope::At(positions[1])).await;
    assert_eq!((at_second.debit(), at_second.postings()), (eur("1.00"), 1));
    let now = balance(BalanceScope::Current).await;
    assert_eq!((now.debit(), now.postings()), (eur("7.00"), 3));
}

/// US4-2: a postings page with a cursor, a booking range and two dimension
/// filters, in both orders, equals the naive filter.
pub async fn us4_postings_pages_with_filters<S: Store<2>>(store: S) {
    let (_, books) = books(store.clone()).await;
    let days = [
        date!("2026-03-20"),
        date!("2026-03-05"),
        date!("2026-03-12"),
        date!("2026-03-01"),
        date!("2026-03-15"),
        date!("2026-03-10"),
    ];
    for (n, day) in days.iter().enumerate() {
        let region = if n % 2 == 0 { "eu" } else { "us" };
        let posting = Posting::debit("Assets:Cash", eur("1.00"), EUR)
            .dimension("region", region)
            .dimension("team", "a");
        books
            .record(Entry::new(format!("p-{n}"), *day).posting(posting).credit(
                "Income:Sales",
                eur("1.00"),
                EUR,
            ))
            .await
            .unwrap();
    }
    let cash = BalanceKey {
        handle: AccountHandle::new(0).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
    };
    let filters = vec![
        (Label::new("region").unwrap(), Label::new("eu").unwrap()),
        (Label::new("team").unwrap(), Label::new("a").unwrap()),
    ];
    for order in [PostingOrder::Position, PostingOrder::Booking] {
        let mut found = Vec::new();
        let mut page: PageRead<PostingCursor> = PageRead::first(1);
        loop {
            let read = store
                .postings(&PostingRead {
                    key: cash,
                    order,
                    booking: Some(date!("2026-03-02")..=date!("2026-03-31")),
                    value: None,
                    dimensions: filters.clone(),
                    kinds: doubleentry::store::KindFilter::Any,
                    page: page.clone(),
                })
                .await
                .unwrap();
            assert!(read.items.len() <= 1);
            found.extend(read.items.into_iter().map(|line| (line.booking, line.side)));
            match read.next {
                Some(next) => page = page.after(next),
                None => break,
            }
        }
        // eu: postings 0 (03-20), 2 (03-12), 4 (03-15); all within the range.
        let expected = match order {
            PostingOrder::Position => vec![
                date!("2026-03-20"),
                date!("2026-03-12"),
                date!("2026-03-15"),
            ],
            _ => vec![
                date!("2026-03-12"),
                date!("2026-03-15"),
                date!("2026-03-20"),
            ],
        };
        assert_eq!(
            found,
            expected
                .into_iter()
                .map(|day| (day, Side::Debit))
                .collect::<Vec<_>>(),
            "{order:?}"
        );
    }
}

/// US4-3: a sealed period's balance-tree leaves in (currency, layer,
/// handle) order, each the balance through the period's end.
pub async fn us4_sealed_leaves_in_handle_order<S: Store<2>>(store: S) {
    let (_, books) = books(store.clone()).await;
    books
        .record(sale("jan", date!("2026-01-15"), "5.00"))
        .await
        .unwrap();
    books
        .record(sale("feb", date!("2026-02-15"), "7.00"))
        .await
        .unwrap();
    books
        .record(sale("late-jan", date!("2026-01-31"), "1.00"))
        .await
        .unwrap();
    books.seal_period("2026-01").await.unwrap();
    let read = BalanceRead {
        scope: BalanceScope::Sealed(Label::new("2026-01").unwrap()),
        keys: None,
        page: PageRead::first(1000),
    };
    let leaves = store.balances(&read).await.unwrap().items;
    let handles: Vec<u32> = leaves.iter().map(|(key, _)| key.handle.get()).collect();
    assert_eq!(handles, [0, 1]);
    assert_eq!(leaves[0].1.debit(), eur("6.00"));
    assert_eq!(leaves[1].1.credit(), eur("6.00"));
}

/// US4-5: inclusion and consistency proofs at 50 historical sizes, built
/// from `read_nodes`, verify.
pub async fn us4_proofs_at_historical_sizes<S: Store<2>>(store: S) {
    let (_, books) = books(store.clone()).await;
    for n in 0..60 {
        books
            .record(sale(&format!("proof-{n}"), date!("2026-03-15"), "1.00"))
            .await
            .unwrap();
    }
    let size = store.size().await.unwrap();
    let leaves = store
        .read_leaves(&LeafRead {
            from: 0,
            through: None,
            kinds: None,
            limit: 1000,
        })
        .await
        .unwrap()
        .items;
    let mut tree = doubleentry::store::Tree::new();
    for leaf in &leaves {
        tree.push(leaf_hash(&leaf.bytes)).unwrap();
    }
    let mut seed = 17u64;
    for _ in 0..50 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let old = 1 + (seed >> 33) % size;
        let index = (seed >> 13) % old;
        let plan = doubleentry::store::InclusionPlan::new(index, old).unwrap();
        let proof: InclusionProof = plan
            .assemble(&store.read_nodes(plan.ids()).await.unwrap())
            .unwrap();
        proof
            .verify(
                &leaf_hash(&leaves[index as usize].bytes),
                &tree.head_at(old).unwrap(),
            )
            .unwrap();
        let plan = doubleentry::store::ConsistencyPlan::new(old, size).unwrap();
        let proof = plan
            .assemble(&store.read_nodes(plan.ids()).await.unwrap())
            .unwrap();
        proof
            .verify(&tree.head_at(old).unwrap(), &tree.head())
            .unwrap();
    }
}

/// US4-7: pages read with one `through` while entries are appended see
/// exactly the postings below it.
pub async fn us4_pages_pinned_to_through<S: Store<2>>(store: S) {
    let (_, books) = books(store.clone()).await;
    for n in 0..5 {
        books
            .record(sale(&format!("before-{n}"), date!("2026-03-15"), "1.00"))
            .await
            .unwrap();
    }
    let cash = BalanceKey {
        handle: AccountHandle::new(0).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
    };
    let read = |page| PostingRead {
        key: cash,
        order: PostingOrder::Position,
        booking: None,
        value: None,
        dimensions: Vec::new(),
        kinds: doubleentry::store::KindFilter::Any,
        page,
    };
    let first = store.postings(&read(PageRead::first(2))).await.unwrap();
    let through = first.through;
    for n in 0..5 {
        books
            .record(sale(&format!("after-{n}"), date!("2026-03-15"), "1.00"))
            .await
            .unwrap();
    }
    let mut lines = first.items;
    let mut next = first.next;
    while let Some(cursor) = next {
        let page = store
            .postings(&read(PageRead::first(2).after(cursor).through(through)))
            .await
            .unwrap();
        assert_eq!(page.through, through);
        lines.extend(page.items);
        next = page.next;
    }
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|line| line.position < through));
}

/// The stored entry under `key`, by key and by id alike.
pub async fn entry_by_key_and_id<S: Store<2>>(
    store: &S,
    key: &str,
) -> doubleentry::store::StoredEntry {
    let key = IdempotencyKey::new(key).unwrap();
    let by_key = store
        .entry(EntryRef::Key(&key), None)
        .await
        .unwrap()
        .unwrap();
    let by_id = store
        .entry(EntryRef::Id(by_key.id), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_key, by_id);
    by_key
}

/// Captures the `reads` and `rows` fields of every `write` span.
#[cfg(feature = "tracing")]
pub mod spans {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    /// Per `write` span: (facts read, rows read).
    #[derive(Clone, Default)]
    pub struct Counts(pub Arc<Mutex<Vec<(u64, u64)>>>);

    /// The process-wide collector (installed once; a test binary using it
    /// runs one test, so no other test shares the spans).
    pub fn counts() -> &'static Counts {
        static COUNTS: std::sync::OnceLock<Counts> = std::sync::OnceLock::new();
        COUNTS.get_or_init(|| {
            let counts = Counts::default();
            tracing::subscriber::set_global_default(counts.clone()).unwrap();
            counts
        })
    }

    struct Fields<'a>(&'a mut (u64, u64));

    impl Visit for Fields<'_> {
        fn record_u64(&mut self, field: &Field, value: u64) {
            match field.name() {
                "reads" => self.0.0 = value,
                "rows" => self.0.1 = value,
                _ => {}
            }
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.record_u64(field, value as u64);
        }
        fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
    }

    impl Subscriber for Counts {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, attributes: &Attributes<'_>) -> Id {
            let mut spans = self.0.lock().unwrap();
            if attributes.metadata().name() == "write" {
                spans.push((0, 0));
                Id::from_u64(spans.len() as u64)
            } else {
                Id::from_u64(u64::MAX)
            }
        }
        fn record(&self, id: &Id, values: &Record<'_>) {
            if id.into_u64() == u64::MAX {
                return;
            }
            let mut spans = self.0.lock().unwrap();
            let slot = &mut spans[id.into_u64() as usize - 1];
            values.record(&mut Fields(slot));
        }
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }
}

/// `regress_qs7_write_reads_do_not_grow_with_history` (PoC: Q-S7
/// `perf_history_scaling`): the facts (and SQL rows) a draw on a limited
/// account reads are the same for its `count`-th draw as for its 10th —
/// limits read maintained balances, never the history.
#[cfg(feature = "tracing")]
// PoC: Q-S7 perf_history_scaling
pub async fn regress_qs7_write_reads_do_not_grow_with_history<S: Store<2>>(store: S, count: usize) {
    let counts = spans::counts();
    counts.0.lock().unwrap().clear();
    let (_, books) = books(store).await;
    books
        .record(
            Entry::new("fund", date!("2026-01-02"))
                .debit("Assets:Cash", eur("1000000.00"), EUR)
                .credit("Liabilities:Wallet", eur("1000000.00"), EUR),
        )
        .await
        .unwrap();
    let mut tenth = None;
    for n in 1..=count {
        let draw = Entry::new(format!("draw-{n}"), date!("2026-01-15"))
            .debit("Liabilities:Wallet", eur("1.00"), EUR)
            .credit("Assets:Cash", eur("1.00"), EUR);
        books.record(draw).await.unwrap();
        let last = *counts.0.lock().unwrap().last().unwrap();
        if n == 10 {
            tenth = Some(last);
        }
        if n == count {
            assert!(last.0 > 0, "the write span records its reads");
            assert_eq!(
                Some(last),
                tenth,
                "the {count}th draw reads what the 10th read"
            );
        }
    }
}

/// A ledger with a row in every table the façade can fill: master data of
/// every kind, a policy, entries with dimensions and memos, a reversal, a
/// clearing and its reset, and a sealed period. Returns the ledger and the
/// position of entry `inv`.
pub async fn populated<S: Store<2>>(store: S) -> (Ledger<S>, u64) {
    let (_, books) = books(store).await;
    books
        .declare_currency(doubleentry::currency::USD, 2, date!("2026-01-01"))
        .await
        .unwrap();
    books
        .set_policy(
            doubleentry::LedgerPolicy::new([], Some(30)).unwrap(),
            date!("2026-01-01"),
        )
        .await
        .unwrap();
    books
        .set_class("Expenses:Rent", Some("office"), date!("2026-01-01"))
        .await
        .unwrap();
    let inv = books
        .record(
            Entry::new("inv", date!("2026-01-10"))
                .posting(
                    Posting::debit("Assets:Cash", eur("100.00"), EUR).dimension("region", "eu"),
                )
                .credit("Income:Sales", eur("100.00"), EUR)
                .memo("Max Mustermann"),
        )
        .await
        .unwrap();
    let refund = books
        .record(
            Entry::new("refund", date!("2026-01-12"))
                .debit("Income:Sales", eur("40.00"), EUR)
                .credit("Assets:Cash", eur("40.00"), EUR)
                .memo("refund"),
        )
        .await
        .unwrap();
    let cleared = books
        .clear(
            "clr",
            doubleentry::PostingRef::at(inv.id, 0),
            doubleentry::PostingRef::at(refund.id, 1),
            eur("40.00"),
            date!("2026-01-20"),
        )
        .await
        .unwrap()
        .position;
    books
        .reset_clearing("reset", cleared, date!("2026-01-21"))
        .await
        .unwrap();
    books
        .clear(
            "clr-2",
            doubleentry::PostingRef::at(inv.id, 0),
            doubleentry::PostingRef::at(refund.id, 1),
            eur("10.00"),
            date!("2026-01-22"),
        )
        .await
        .unwrap();
    books
        .record(sale("rent", date!("2026-01-25"), "5.00"))
        .await
        .unwrap();
    // A hold open across the seal, posted after it; one still open.
    let auth = books
        .record(
            Entry::open_hold("auth", date!("2026-01-28"))
                .debit("Assets:Cash", eur("50.00"), EUR)
                .credit("Liabilities:Wallet", eur("50.00"), EUR),
        )
        .await
        .unwrap();
    let reversal = books
        .reversal(id("rent"), "rent-rev", date!("2026-01-26"))
        .await
        .unwrap();
    books.record(reversal).await.unwrap();
    books.seal_period("2026-01").await.unwrap();
    books
        .record(sale("feb", date!("2026-02-10"), "7.00"))
        .await
        .unwrap();
    books
        .record(Entry::post_hold(
            "capture",
            date!("2026-02-11"),
            auth.id,
            PostAmount::Partial(eur("30.00")),
        ))
        .await
        .unwrap();
    books
        .record(
            Entry::open_hold("auth-2", date!("2026-02-12"))
                .expires(date!("2026-03-01"))
                .debit("Assets:Cash", eur("20.00"), EUR)
                .credit("Liabilities:Wallet", eur("20.00"), EUR),
        )
        .await
        .unwrap();
    books.audit().await.unwrap();
    (books, inv.position)
}

/// The leaf of the `populated` ledger's resolution `capture`, re-encoded to
/// resolve another hold (`auth-2`), and its position: a record whose link
/// to its hold was rewritten.
pub async fn retargeted_resolution<S: Store<2>>(store: &S) -> (u64, Vec<u8>) {
    let stored = store
        .entry(EntryRef::Id(id("capture")), None)
        .await
        .unwrap()
        .unwrap();
    let doubleentry::record::Record::Entry(record) = stored.leaf.record::<2>().unwrap() else {
        panic!("capture is an entry");
    };
    let mut parts = record.into_parts();
    parts.hold = Some(doubleentry::record::HoldAction::Post {
        hold: id("auth-2"),
        amount: PostAmount::Partial(eur("30.00")),
    });
    let record = doubleentry::record::Record::Entry(doubleentry::EntryRecord::new(parts).unwrap());
    let bytes = record.to_leaf_bytes();
    assert_ne!(leaf_hash(&bytes), leaf_hash(&stored.leaf.bytes));
    (stored.position, bytes)
}

/// Round 3 (F2, F4): sealing March of a 12-month calendar names the
/// earliest open period (427) on both façades; `seal_through` seals
/// January to March in order, the same receipts on both; reports print
/// amounts at the currency's declared exponent (JPY without decimals).
pub async fn seal_through_and_exponents<S: Store<2>>(store: S) {
    let (mut journal, books) = books(store).await;
    let jpy = doubleentry::currency::JPY;
    let on = date!("2026-01-01");
    journal.declare_currency(jpy, 0, on).unwrap();
    books.declare_currency(jpy, 0, on).await.unwrap();
    let yen = |key: &str| {
        Entry::new(key, date!("2026-02-10"))
            .debit("Assets:Cash", eur("15000"), jpy)
            .credit("Income:Sales", eur("15000"), jpy)
    };
    journal.record(yen("yen")).unwrap();
    books.record(yen("yen")).await.unwrap();
    journal
        .record(sale("eur", date!("2026-02-11"), "12.50"))
        .unwrap();
    books
        .record(sale("eur", date!("2026-02-11"), "12.50"))
        .await
        .unwrap();

    let refused = books.seal_period("2026-03").await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        journal.seal_period("2026-03").unwrap_err().to_string()
    );
    assert_eq!(
        refused.rejection(),
        Some(&Rejection::PredecessorNotSealed {
            predecessor: Label::new("2026-02").unwrap(),
            earliest: Label::new("2026-01").unwrap(),
        })
    );
    assert!(
        refused.to_string().contains("seal from 2026-01 on"),
        "{refused}"
    );

    let sealed = books.seal_through("2026-03").await.unwrap();
    let periods: Vec<String> = sealed.iter().map(|r| r.period.to_string()).collect();
    assert_eq!(periods, ["2026-01", "2026-02", "2026-03"]);
    assert_eq!(journal.seal_through("2026-03").unwrap(), sealed);
    assert_eq!(journal.head(), books.head().await.unwrap());
    assert_eq!(
        books.seal_through("2026-02").await.unwrap(),
        [sealed[1].clone()]
    );
    books.audit().await.unwrap();

    assert_eq!(books.exponent(jpy).await.unwrap(), Some(0));
    assert_eq!(journal.exponent(jpy), Some(0));
    let all = doubleentry::BalanceQuery::all();
    let trial = books.trial_balance(&all).await.unwrap();
    assert_eq!(trial, journal.trial_balance(&all).unwrap());
    let text = trial.to_string();
    assert!(text.contains("15000 "), "{text}");
    assert!(!text.contains("15000.00"), "{text}");
    assert!(text.contains("12.50"), "{text}");
    let statement = books.statement("Assets:Cash", jpy, &all).await.unwrap();
    assert_eq!(statement.exponent, 0);
    assert!(!statement.to_string().contains("15000.00"), "{statement}");
    let rollup = books.rollup(jpy, &all).await.unwrap();
    assert!(rollup.to_string().contains("15000\n"), "{rollup}");
}

/// 028 US2: the calendar as a host reads it — periods of any id, their
/// seals, the watermark, and each as it was at an earlier head — equal on
/// the journal and the store.
pub async fn calendar_reads<S: Store<2>>(store: S) {
    let mut journal = Journal::new(ledger()).as_actor("setup");
    let books = Ledger::new(store, ledger())
        .await
        .unwrap()
        .as_actor("setup");
    assert_eq!(books.watermark().await.unwrap(), None);
    assert!(books.periods().await.unwrap().is_empty());
    for (id, start, end) in [
        ("2026-01", date!("2026-01-01"), date!("2026-01-31")),
        ("2026-Q1-rest", date!("2026-02-01"), date!("2026-03-31")),
        ("FY2026-13", date!("2026-04-01"), date!("2026-04-30")),
    ] {
        journal.define_period(id, start, end).unwrap();
        books.define_period(id, start, end).await.unwrap();
    }
    journal.seal_period("2026-01").unwrap();
    books.seal_period("2026-01").await.unwrap();
    let before = books.head().await.unwrap();
    journal.seal_period("2026-Q1-rest").unwrap();
    books.seal_period("2026-Q1-rest").await.unwrap();

    let periods = books.periods().await.unwrap();
    assert_eq!(periods, journal.periods().unwrap());
    let ids: Vec<String> = periods.iter().map(|p| p.id.to_string()).collect();
    assert_eq!(ids, ["2026-01", "2026-Q1-rest", "FY2026-13"]);
    let sealed: Vec<bool> = periods.iter().map(doubleentry::Period::is_sealed).collect();
    assert_eq!(sealed, [true, true, false]);
    assert_eq!(periods[1].start, date!("2026-02-01"));
    assert_eq!(
        periods[0].to_string(),
        format!(
            "2026-01 2026-01-01..=2026-01-31, sealed at position {}",
            periods[0].seal.as_ref().unwrap().position
        )
    );

    let seals = books.seals().await.unwrap();
    assert_eq!(seals, journal.seals().unwrap());
    assert_eq!(seals.len(), 2);
    assert_eq!(
        (seals[1].start(), seals[1].end()),
        (date!("2026-02-01"), date!("2026-03-31"))
    );
    assert!(seals[0].through() < seals[0].position);
    assert_eq!(books.watermark().await.unwrap(), Some(date!("2026-03-31")));
    assert_eq!(journal.watermark().unwrap(), Some(date!("2026-03-31")));

    // As it was before the second seal.
    let pinned = books.seals_at(before).await.unwrap();
    assert_eq!(pinned, journal.seals_at(before).unwrap());
    assert_eq!(pinned.len(), 1);
    assert_eq!(
        books.watermark_at(before).await.unwrap(),
        Some(date!("2026-01-31"))
    );
    let then = books.periods_at(before).await.unwrap();
    assert_eq!(then.len(), 3);
    assert!(!then[1].is_sealed());
    // A head beyond the log is refused.
    let mut beyond = books.head().await.unwrap();
    beyond.size += 1;
    assert!(matches!(
        books.periods_at(beyond).await.unwrap_err(),
        Error::Query(doubleentry::error::QueryError::PositionBeyondHead { .. })
    ));
}

/// 028 US5: totals per entry kind — `of_kind` (any of), `without_kind`,
/// both (nothing) — equal on the journal and the store, and equal to a
/// direct fold over the recorded drafts, in every read taking a query.
pub async fn kind_queries<S: Store<2>>(store: S) {
    let (mut journal, books) = books(store).await;
    let drafts = [
        (
            "inv-1",
            Some("invoice"),
            "Assets:Cash",
            "Income:Sales",
            "100.00",
        ),
        (
            "inv-2",
            Some("invoice"),
            "Assets:Cash",
            "Income:Sales",
            "40.00",
        ),
        (
            "cn-1",
            Some("credit_note"),
            "Income:Sales",
            "Assets:Cash",
            "15.00",
        ),
        (
            "pay-1",
            Some("payment"),
            "Expenses:Rent",
            "Assets:Cash",
            "60.00",
        ),
        ("misc", None, "Assets:Cash", "Income:Sales", "7.00"),
        (
            "ümlaut",
            Some("Gutschrift-für-Kündigung"),
            "Income:Sales",
            "Assets:Cash",
            "1.00",
        ),
    ];
    for (key, kind, debit, credit, amount) in drafts {
        let mut draft = Entry::new(key, date!("2026-03-02"))
            .debit(debit, eur(amount), EUR)
            .credit(credit, eur(amount), EUR);
        if let Some(kind) = kind {
            draft = draft.kind(kind);
        }
        journal.record(draft.clone()).unwrap();
        books.record(draft).await.unwrap();
    }
    let year = doubleentry::BalanceQuery::between(date!("2026-01-01"), date!("2026-12-31"));
    // The expected cash totals, folded from the drafts by hand.
    let folded = |keep: &dyn Fn(Option<&str>) -> bool| {
        let (mut debit, mut credit) = (Amount::ZERO, Amount::ZERO);
        for (_, kind, d, c, amount) in drafts {
            if keep(kind) {
                if d == "Assets:Cash" {
                    debit = debit.checked_add(eur(amount)).unwrap();
                }
                if c == "Assets:Cash" {
                    credit = credit.checked_add(eur(amount)).unwrap();
                }
            }
        }
        (debit, credit)
    };
    type Keep = Box<dyn Fn(Option<&str>) -> bool>;
    let cases: Vec<(doubleentry::BalanceQuery, Keep)> = vec![
        (
            year.clone().of_kind("invoice"),
            Box::new(|k| k == Some("invoice")),
        ),
        (
            year.clone().of_kind("invoice").of_kind("credit_note"),
            Box::new(|k| matches!(k, Some("invoice" | "credit_note"))),
        ),
        (year.clone().without_kind(), Box::new(|k| k.is_none())),
        (
            year.clone().of_kind("payment").without_kind(),
            Box::new(|_| false),
        ),
        (
            year.clone().of_kind("Gutschrift-für-Kündigung"),
            Box::new(|k| k == Some("Gutschrift-für-Kündigung")),
        ),
    ];
    for (query, keep) in &cases {
        let balance = books.balance("Assets:Cash", EUR, query).await.unwrap();
        assert_eq!(
            balance,
            journal.balance("Assets:Cash", EUR, query).unwrap(),
            "{query:?}"
        );
        assert_eq!(
            (balance.debit(), balance.credit()),
            folded(keep.as_ref()),
            "{query:?}"
        );
        assert_eq!(
            books.trial_balance(query).await.unwrap(),
            journal.trial_balance(query).unwrap()
        );
        assert_eq!(
            books.statement("Assets:Cash", EUR, query).await.unwrap(),
            journal.statement("Assets:Cash", EUR, query).unwrap()
        );
        assert_eq!(
            books.rollup(EUR, query).await.unwrap(),
            journal.rollup(EUR, query).unwrap()
        );
        assert_eq!(
            books.open_items("Assets:Cash", EUR, query).await.unwrap(),
            journal.open_items("Assets:Cash", EUR, query).unwrap()
        );
    }
    let statement = books
        .statement("Assets:Cash", EUR, &year.clone().of_kind("invoice"))
        .await
        .unwrap();
    assert_eq!(statement.lines.len(), 2);
    let invalid = books
        .balance("Assets:Cash", EUR, &year.of_kind(" spaced "))
        .await
        .unwrap_err();
    assert!(
        matches!(
            invalid,
            Error::Query(doubleentry::error::QueryError::InvalidLabel { .. })
        ),
        "{invalid:?}"
    );
}

/// 028 US3: a payment recorded and assigned to two open invoices in one
/// batch — refused whole when one assignment over-applies, recorded whole
/// otherwise, replayed whole on a retry — equal on the journal and the store.
pub async fn payment_run<S: Store<2>>(store: S) {
    use doubleentry::{BatchItem, Clearing, PostingRef};
    let (mut journal, books) = books(store).await;
    let on = date!("2026-01-01");
    journal
        .open("Assets:Receivables", AccountKind::Asset, on)
        .unwrap();
    books
        .open("Assets:Receivables", AccountKind::Asset, on)
        .await
        .unwrap();
    for (key, amount) in [("inv-a", "100.00"), ("inv-b", "50.00")] {
        let invoice = Entry::new(key, date!("2026-03-02"))
            .kind("invoice")
            .debit("Assets:Receivables", eur(amount), EUR)
            .credit("Income:Sales", eur(amount), EUR);
        journal.record(invoice.clone()).unwrap();
        books.record(invoice).await.unwrap();
    }
    let invoice = |key: &str| PostingRef::at(books.entry_id(key).unwrap(), 0);
    let paid = PostingRef::at(books.entry_id("pay-1").unwrap(), 1);
    let assign = |key: &str, invoice: PostingRef, amount: &str| {
        BatchItem::from(Clearing::new(
            key,
            invoice,
            paid,
            eur(amount),
            date!("2026-03-20"),
        ))
    };
    let payment = BatchItem::from(
        Entry::new("pay-1", date!("2026-03-20"))
            .kind("payment")
            .debit("Assets:Cash", eur("150.00"), EUR)
            .credit("Assets:Receivables", eur("150.00"), EUR),
    );

    // The second assignment asks for more than the payment has left.
    let before = books.head().await.unwrap();
    let over = vec![
        payment.clone(),
        assign("pay-1/inv-a", invoice("inv-a"), "100.00"),
        assign("pay-1/inv-b", invoice("inv-b"), "50.01"),
    ];
    let refused = books.batch(over.clone()).await.unwrap_err();
    assert!(
        matches!(
            refused,
            Error::BatchRejected {
                index: 2,
                rejection: Rejection::OverApplied { .. }
            }
        ),
        "{refused:?}"
    );
    assert_eq!(
        journal.batch(over).unwrap_err().to_string(),
        refused.to_string()
    );
    assert_eq!(
        books.head().await.unwrap(),
        before,
        "nothing of it was written"
    );

    let run = vec![
        payment,
        assign("pay-1/inv-a", invoice("inv-a"), "100.00"),
        assign("pay-1/inv-b", invoice("inv-b"), "50.00"),
    ];
    let receipts = books.batch(run.clone()).await.unwrap();
    assert_eq!(receipts, journal.batch(run.clone()).unwrap());
    assert_eq!(receipts.len(), 3);
    assert!(receipts[0].as_recorded().is_some() && receipts[1].as_cleared().is_some());
    assert!(receipts.iter().all(|receipt| !receipt.replayed()));
    assert_eq!(
        receipts
            .iter()
            .map(doubleentry::BatchReceipt::position)
            .collect::<Vec<_>>(),
        [before.size, before.size + 1, before.size + 2]
    );
    let again = books.batch(run).await.unwrap();
    assert!(again.iter().all(doubleentry::BatchReceipt::replayed));
    assert_eq!(
        again
            .iter()
            .map(doubleentry::BatchReceipt::position)
            .collect::<Vec<_>>(),
        receipts
            .iter()
            .map(doubleentry::BatchReceipt::position)
            .collect::<Vec<_>>()
    );
    let open = books
        .open_items("Assets:Receivables", EUR, &doubleentry::BalanceQuery::all())
        .await
        .unwrap();
    assert!(open.is_empty(), "{open:?}");
    // clear_batch: clearings only, the same rule.
    assert!(books.clear_batch(Vec::new()).await.unwrap().is_empty());
    assert_eq!(journal.head(), books.head().await.unwrap());
    books.audit().await.unwrap();
}
