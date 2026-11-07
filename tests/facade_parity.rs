//! `Journal` and `Ledger<S>` are one API: the same method names and
//! parameters — `async` and on `&self` for the ledger — and the same answers
//! for the same calls.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use doubleentry::PageRead;
use doubleentry::store::{MemoryStore, Store};
use doubleentry::{
    AccountKind, AccountSpec, BalanceLimit, BalanceQuery, Entry, Error, Journal, Layer, Ledger,
    LedgerId, MemoSalt, Rejection,
    currency::EUR,
    date,
    error::QueryError,
    record::{AccountEvent, Record},
};

/// Every `pub fn` of a façade's source: name → parameters without the
/// receiver, whitespace-normalised.
fn methods(source: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let mut rest = source;
    while let Some(start) = ["pub fn ", "pub async fn ", "pub const fn "]
        .iter()
        .filter_map(|marker| rest.find(marker).map(|at| (at, marker.len())))
        .min()
    {
        let after = &rest[start.0 + start.1..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let open = after.find('(').unwrap();
        let mut depth = 0;
        let mut close = open;
        for (index, c) in after[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + index;
                        break;
                    }
                }
                _ => {}
            }
        }
        let parameters: Vec<String> = after[open + 1..close]
            .split(',')
            .map(|parameter| parameter.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|parameter| !parameter.is_empty() && !parameter.ends_with("self"))
            .collect();
        found.insert(name, parameters.join(", "));
        rest = &after[close..];
    }
    found
}

#[test]
fn the_facades_have_the_same_methods_and_parameters() {
    let journal = methods(include_str!("../src/journal.rs"));
    let ledger = methods(include_str!("../src/ledger.rs"));
    let only_journal = ["new", "with_precision", "from_log", "leaves"];
    let only_ledger = ["new", "with_precision", "to_journal"];
    for (name, parameters) in &journal {
        if !only_journal.contains(&name.as_str()) {
            assert_eq!(ledger.get(name), Some(parameters), "Ledger::{name}");
        }
    }
    for name in ledger.keys() {
        assert!(
            journal.contains_key(name) || only_ledger.contains(&name.as_str()),
            "Journal::{name}"
        );
    }
    // The methods 020 and 022 own (023, 024 and 025 add theirs here).
    for name in [
        "id",
        "actor",
        "as_actor",
        "with_salt_source",
        "declare_currency",
        "open",
        "register",
        "set_limit",
        "set_class",
        "close_account",
        "reopen_account",
        "set_policy",
        "define_period",
        "define_months",
        "seal_period",
        "close_year",
        "record",
        // H21: atomic batches.
        "record_batch",
        "reversal",
        "clear",
        "reset_clearing",
        "erase_memo",
        "audit",
        "head",
        "head_at",
        "prove_entry",
        "prove_entry_at",
        "prove_consistency",
        "entry",
        "entry_by_key",
        "account",
        "memo",
        // 023: holds.
        "hold",
        "holds_due",
        // 025: reports and the export view.
        "balance",
        "trial_balance",
        "statement",
        "rollup",
        "open_items",
        "check_assertion",
        "check_assertions",
        "export",
        // 024: checkpoints, sealed balances, bundles and tiles.
        "checkpoint",
        "prove_balance",
        "bundle_record",
        "bundle_entry",
        "bundle_balance",
        "tile",
        "tile_entries",
        // 028: the host surface.
        "entry_id",
        "batch",
        "clear_batch",
        "periods",
        "periods_at",
        "seals",
        "seals_at",
        "watermark",
        "watermark_at",
    ] {
        assert!(
            journal.contains_key(name) && ledger.contains_key(name),
            "{name}"
        );
    }
    assert!(ledger.contains_key("to_journal"));
}

fn assert_send<T: Send>(_: T) {}

/// T015: every future of a `Ledger` is `Send`, so a service can spawn it.
#[test]
fn ledger_futures_are_send() {
    let books = futures_ledger();
    let on = date!("2026-01-01");
    let id = doubleentry::EntryId::from_bytes([1; 16]);
    assert_send(books.declare_currency(EUR, 2, on));
    assert_send(books.open("Assets:Cash", AccountKind::Asset, on));
    assert_send(books.register(AccountSpec::new("Assets:Cash", AccountKind::Asset, on)));
    assert_send(books.set_limit("Assets:Cash", BalanceLimit::Unlimited, on));
    assert_send(books.set_class("Assets:Cash", None, on));
    assert_send(books.close_account("Assets:Cash", on));
    assert_send(books.reopen_account("Assets:Cash", on));
    assert_send(books.set_policy(doubleentry::LedgerPolicy::default(), on));
    assert_send(books.define_period("2026-01", on, on));
    assert_send(books.define_months(2026, 1..=12));
    assert_send(books.seal_period("2026-01"));
    assert_send(books.close_year("close", on, "Equity:Retained"));
    assert_send(books.record(Entry::new("k", on)));
    assert_send(books.record_batch(vec![Entry::new("k", on)]));
    assert_send(books.reversal(id, "rev", on));
    assert_send(books.clear(
        "clr",
        doubleentry::PostingRef::at(id, 0),
        doubleentry::PostingRef::at(id, 1),
        doubleentry::Amount::ZERO,
        on,
    ));
    assert_send(books.reset_clearing("reset", 1, on));
    assert_send(books.erase_memo(id));
    assert_send(books.audit());
    assert_send(books.head());
    assert_send(books.head_at(1));
    assert_send(books.prove_entry(id));
    assert_send(books.prove_entry_at(id, 1));
    assert_send(books.prove_consistency(1, 1));
    assert_send(books.entry(id));
    assert_send(books.entry_by_key("k"));
    assert_send(books.account("Assets:Cash"));
    assert_send(books.memo(id));
    assert_send(books.hold(id));
    assert_send(books.prove_balance("Assets:Cash", EUR, doubleentry::Layer::Settled, "2026-01"));
    assert_send(books.tile(doubleentry::transparency::TileId::hashes(0, 0, 1).unwrap()));
    assert_send(books.tile_entries(doubleentry::transparency::TileId::entries(0, 1).unwrap()));
    #[cfg(feature = "witness")]
    {
        let key = doubleentry::transparency::LogKey::from_seed(books.id(), [7; 32]);
        let signed = Journal::new(books.id().clone()).checkpoint(&key).unwrap();
        assert_send(books.checkpoint(&key));
        assert_send(books.bundle_record(0, &signed));
        assert_send(books.bundle_entry(id, &signed));
        assert_send(books.bundle_balance(
            "Assets:Cash",
            EUR,
            doubleentry::Layer::Settled,
            "2026-01",
            &signed,
        ));
    }
    assert_send(books.holds_due(on, doubleentry::PageRead::first(10)));
    assert_send(books.to_journal());
    assert_send(Ledger::new(
        MemoryStore::new(),
        LedgerId::new("example.com/acme").unwrap(),
    ));
}

/// A ledger value for the `Send` check (never awaited).
fn futures_ledger() -> Ledger<MemoryStore> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(Ledger::new(
            MemoryStore::new(),
            LedgerId::new("example.com/acme").unwrap(),
        ))
        .unwrap()
}

/// T014 (US2-4): `Ledger::new` creates, then opens; another ledger or scale
/// in the store is a configuration error.
#[tokio::test]
async fn new_creates_then_opens_and_refuses_another_ledger() {
    let store = MemoryStore::new();
    let id = LedgerId::new("example.com/acme").unwrap();
    let first = Ledger::new(store.clone(), id.clone()).await.unwrap();
    first
        .declare_currency(EUR, 2, date!("2026-01-01"))
        .await
        .unwrap();
    let again = Ledger::new(store.clone(), id.clone()).await.unwrap();
    assert_eq!(again.head().await.unwrap().size, 2);
    let other = Ledger::new(store, LedgerId::new("example.com/other").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(&other, Error::Config(_)), "{other}");
    // F8 (round 3): a configuration mistake, named as such — not Integrity.
    assert_eq!(
        other.to_string(),
        "configuration: the store holds the ledger example.com/acme (P = 2), not example.com/other (P = 2)"
    );
    let wide = MemoryStore::<4>::new();
    Ledger::<_, 4>::with_precision(wide.clone(), id.clone())
        .await
        .unwrap();
    // The same store opened at another scale is another ledger.
    let sql_like = MemoryStore::<2>::new();
    sql_like
        .create(&doubleentry::record::Genesis {
            ledger: LedgerId::new("example.com/b").unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        Ledger::new(sql_like, id).await,
        Err(Error::Config(_))
    ));
}

/// T016: the actor belongs to the clone and is written into master data.
#[tokio::test]
async fn the_actor_belongs_to_the_clone() {
    let store = MemoryStore::new();
    let books = Ledger::new(store.clone(), LedgerId::new("example.com/acme").unwrap())
        .await
        .unwrap();
    let alice = books.clone().as_actor("alice");
    let handle = alice
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .await
        .unwrap();
    assert_eq!((alice.actor(), books.actor()), ("alice", "system"));
    let records = store
        .account_records(handle, PageRead::first(10))
        .await
        .unwrap();
    match records.items[0].record::<2>().unwrap() {
        Record::Account(AccountEvent { actor, .. }) => assert_eq!(actor.as_str(), "alice"),
        other => panic!("{other:?}"),
    }
}

/// T013 (part 1) and T017: one scenario on both façades, compared call by
/// call — master data, months, entries (one refused, one replayed), a seal,
/// a reversal recorded twice, year-end closing, erasure and proofs.
#[tokio::test]
async fn one_scenario_one_answer() {
    let id = LedgerId::new("example.com/acme").unwrap();
    let mut journal = Journal::new(id.clone())
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([5; 16]));
    let books = Ledger::new(MemoryStore::new(), id)
        .await
        .unwrap()
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([5; 16]));
    let on = date!("2026-01-01");
    journal.declare_currency(EUR, 2, on).unwrap();
    books.declare_currency(EUR, 2, on).await.unwrap();
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Income:Sales", AccountKind::Income),
        ("Equity:Retained", AccountKind::Equity),
    ] {
        assert_eq!(
            journal.open(path, kind, on).unwrap(),
            books.open(path, kind, on).await.unwrap()
        );
    }
    journal.define_months(2026, 1..=12).unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    books.define_months(2026, 1..=12).await.unwrap(); // already defined: no-op
    assert_eq!(journal.head(), books.head().await.unwrap());
    let invalid = books.define_months(2026, 12..=13).await.unwrap_err();
    assert_eq!(
        invalid.rejection().map(Rejection::code),
        journal
            .define_months(2026, 12..=13)
            .unwrap_err()
            .rejection()
            .map(Rejection::code)
    );
    let sale = |key: &str, amount: &str| {
        Entry::new(key, date!("2026-01-15"))
            .debit("Assets:Cash", amount.parse().unwrap(), EUR)
            .credit("Income:Sales", amount.parse().unwrap(), EUR)
            .memo(format!("memo {key}"))
    };
    let mut recorded = Vec::new();
    for (key, amount) in [("a", "1.00"), ("b", "2.00"), ("c", "3.00")] {
        let expected = journal.record(sale(key, amount)).unwrap();
        assert_eq!(books.record(sale(key, amount)).await.unwrap(), expected);
        recorded.push(expected);
    }
    assert_eq!(
        journal.record(sale("a", "9.00")).unwrap_err().rejection(),
        books
            .record(sale("a", "9.00"))
            .await
            .unwrap_err()
            .rejection()
    );
    assert!(books.record(sale("b", "2.00")).await.unwrap().replayed);
    assert!(journal.record(sale("b", "2.00")).unwrap().replayed);
    // H21: an atomic batch gives the same receipts and the same head on both
    // façades; a refused one names the same draft and writes nothing; an
    // identical retry replays.
    let batch = || vec![sale("d", "4.00"), sale("b", "2.00"), sale("e", "5.00")];
    let expected = journal.record_batch(batch()).unwrap();
    assert_eq!(books.record_batch(batch()).await.unwrap(), expected);
    assert_eq!(journal.head(), books.head().await.unwrap());
    assert!(expected[1].replayed && !expected[0].replayed);
    let again = books.record_batch(batch()).await.unwrap();
    assert!(again.iter().all(|r| r.replayed));
    assert_eq!(again, journal.record_batch(batch()).unwrap());
    let refused = || vec![sale("f", "6.00"), sale("a", "7.00")];
    let error = books.record_batch(refused()).await.unwrap_err();
    assert!(matches!(
        error,
        doubleentry::Error::BatchRejected {
            index: 1,
            rejection: Rejection::KeyConflict { .. }
        }
    ));
    assert_eq!(
        error.to_string(),
        journal.record_batch(refused()).unwrap_err().to_string()
    );
    assert_eq!(journal.head(), books.head().await.unwrap());
    assert_eq!(books.entry_by_key("f").await.unwrap(), None);
    // 025 T008: the balance step of 022 T013's scenario.
    let query = BalanceQuery::all();
    assert_eq!(
        books.balance("Assets:Cash", EUR, &query).await.unwrap(),
        journal.balance("Assets:Cash", EUR, &query).unwrap()
    );
    assert_eq!(
        journal.seal_period("2026-01").unwrap(),
        books.seal_period("2026-01").await.unwrap()
    );
    // 024 T035: the prove_balance step of 022 T013's scenario.
    let proof = journal
        .prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-01")
        .unwrap();
    assert_eq!(
        books
            .prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-01")
            .await
            .unwrap(),
        proof
    );
    let sealed = proof.verify(&journal.head()).unwrap();
    assert_eq!(
        sealed.balance,
        journal
            .balance(
                "Assets:Cash",
                EUR,
                &BalanceQuery::through(date!("2026-01-31"))
            )
            .unwrap()
    );
    let reversal = journal
        .reversal(recorded[0].id, "rev-a", date!("2026-02-01"))
        .unwrap();
    assert_eq!(
        books
            .reversal(recorded[0].id, "rev-a", date!("2026-02-01"))
            .await
            .unwrap(),
        reversal
    );
    assert_eq!(
        journal.record(reversal.clone()).unwrap(),
        books.record(reversal).await.unwrap()
    );
    let twice = journal
        .reversal(recorded[0].id, "rev-b", date!("2026-02-02"))
        .unwrap();
    let expected = journal.record(twice.clone()).unwrap_err();
    assert!(matches!(
        expected.rejection(),
        Some(Rejection::AlreadyReversed { .. })
    ));
    assert_eq!(
        books.record(twice).await.unwrap_err().rejection(),
        expected.rejection()
    );
    let unknown = doubleentry::EntryId::from_bytes([9; 16]);
    assert!(matches!(
        books.reversal(unknown, "x", on).await,
        Err(Error::Query(QueryError::UnknownEntry { .. }))
    ));
    let closing = journal
        .close_year("close", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    assert_eq!(
        books
            .close_year("close", date!("2026-12-31"), "Equity:Retained")
            .await
            .unwrap(),
        closing
    );
    for draft in closing {
        assert_eq!(
            journal.record(draft.clone()).unwrap(),
            books.record(draft).await.unwrap()
        );
    }
    journal.erase_memo(recorded[1].id).unwrap();
    books.erase_memo(recorded[1].id).await.unwrap();
    assert!(matches!(
        books.erase_memo(unknown).await,
        Err(Error::Query(_))
    ));
    let head = journal.head();
    assert_eq!(books.head().await.unwrap(), head);
    assert_eq!(
        journal.prove_entry(recorded[2].id).unwrap(),
        books.prove_entry(recorded[2].id).await.unwrap()
    );
    assert_eq!(journal.audit().unwrap(), books.audit().await.unwrap());
    // T019: lookups and proofs at earlier sizes answer alike.
    for entry in &recorded {
        assert_eq!(
            journal.entry(entry.id),
            books.entry(entry.id).await.unwrap()
        );
        assert_eq!(journal.memo(entry.id), books.memo(entry.id).await.unwrap());
    }
    assert_eq!(journal.memo(recorded[0].id).as_deref(), Some("memo a"));
    assert_eq!(journal.memo(recorded[1].id), None, "erased");
    assert_eq!(
        journal.entry_by_key("c"),
        books.entry_by_key("c").await.unwrap()
    );
    assert!(journal.entry_by_key("c").is_some());
    assert_eq!(books.entry_by_key("").await.unwrap(), None);
    assert_eq!(journal.entry(unknown), None);
    for account in ["Assets:Cash", "Income:Sales", "Nowhere", "not a path!"] {
        assert_eq!(
            journal.account(account),
            books.account(account).await.unwrap()
        );
    }
    let at = recorded[2].position + 1;
    assert_eq!(
        journal.head_at(at).unwrap(),
        books.head_at(at).await.unwrap()
    );
    assert_eq!(
        journal.prove_entry_at(recorded[2].id, at).unwrap(),
        books.prove_entry_at(recorded[2].id, at).await.unwrap()
    );
    journal
        .prove_entry_at(recorded[2].id, at)
        .unwrap()
        .verify_record::<2>(
            &journal.head_at(at).unwrap(),
            &leaf(&journal, recorded[2].position),
        )
        .unwrap();
    let consistency = journal.prove_consistency(at, head.size).unwrap();
    assert_eq!(
        books.prove_consistency(at, head.size).await.unwrap(),
        consistency
    );
    consistency
        .verify(&journal.head_at(at).unwrap(), &head)
        .unwrap();
    for (old, new) in [(0, at), (at + 1, at), (1, head.size + 1)] {
        let expected = journal.prove_consistency(old, new).unwrap_err();
        assert!(matches!(expected, Error::Query(_)), "{expected}");
        assert_eq!(
            books
                .prove_consistency(old, new)
                .await
                .unwrap_err()
                .to_string(),
            expected.to_string()
        );
    }
    assert!(matches!(
        books
            .prove_entry_at(recorded[2].id, recorded[2].position)
            .await,
        Err(Error::Query(QueryError::UnknownEntry { .. }))
    ));
    assert!(matches!(
        journal.head_at(head.size + 1),
        Err(Error::Query(QueryError::PositionBeyondHead { .. }))
    ));
    // T019: a journal rebuilt from the store is the same ledger.
    let replica = books.to_journal().await.unwrap();
    assert_eq!(replica.head(), head);
    assert_eq!(replica.audit().unwrap(), journal.audit().unwrap());
    assert_eq!(replica.actor(), "setup");
}

/// The leaf bytes at `position` of a journal.
fn leaf(journal: &Journal, position: u64) -> Vec<u8> {
    journal
        .leaves()
        .nth(usize::try_from(position).unwrap())
        .unwrap()
        .bytes
        .to_vec()
}

/// A journal and a ledger over a fresh `MemoryStore`, both writing as
/// `setup` with one salt source.
async fn pair() -> (Journal, Ledger<MemoryStore>) {
    let id = LedgerId::new("example.com/acme").unwrap();
    let journal = Journal::new(id.clone())
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([5; 16]));
    let books = Ledger::new(MemoryStore::new(), id)
        .await
        .unwrap()
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([5; 16]));
    (journal, books)
}

/// T018 (FR-006): repeating master data writes nothing and answers as the
/// first call did; different content goes through the rule core.
#[tokio::test]
async fn master_data_is_idempotent() {
    let (mut journal, books) = pair().await;
    let on = date!("2026-01-01");
    for _ in 0..2 {
        journal.declare_currency(EUR, 2, on).unwrap();
        books.declare_currency(EUR, 2, on).await.unwrap();
        journal.define_months(2026, 1..=3).unwrap();
        books.define_months(2026, 1..=3).await.unwrap();
    }
    let first = journal.open("Assets:Cash", AccountKind::Asset, on).unwrap();
    let size = journal.head().size;
    assert_eq!(
        journal.open("Assets:Cash", AccountKind::Asset, on).unwrap(),
        first
    );
    assert_eq!(journal.head().size, size);
    for _ in 0..2 {
        assert_eq!(
            books
                .open("Assets:Cash", AccountKind::Asset, on)
                .await
                .unwrap(),
            first
        );
    }
    journal
        .set_limit("Assets:Cash", BalanceLimit::NoCreditBalance, on)
        .unwrap();
    journal
        .set_limit("Assets:Cash", BalanceLimit::NoCreditBalance, on)
        .unwrap();
    for _ in 0..2 {
        books
            .set_limit("Assets:Cash", BalanceLimit::NoCreditBalance, on)
            .await
            .unwrap();
    }
    assert_eq!(journal.head(), books.head().await.unwrap());
    assert_eq!(journal.head().size, size + 1);
    // Another exponent is not a repeat: refused by the core (above P = 2).
    let refused = journal.declare_currency(EUR, 3, on).unwrap_err();
    assert_eq!(
        books
            .declare_currency(EUR, 3, on)
            .await
            .unwrap_err()
            .rejection(),
        refused.rejection()
    );
    assert!(refused.rejection().is_some());
}

/// T021 (US2-2): a leg names its account by path or by handle alike; an
/// unknown or malformed path is the same rejection on both façades.
#[tokio::test]
async fn accounts_are_addressed_by_path_or_handle() {
    let (mut journal, books) = pair().await;
    let on = date!("2026-01-01");
    journal.declare_currency(EUR, 2, on).unwrap();
    books.declare_currency(EUR, 2, on).await.unwrap();
    journal.define_months(2026, 1..=1).unwrap();
    books.define_months(2026, 1..=1).await.unwrap();
    let cash = journal.open("Assets:Cash", AccountKind::Asset, on).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, on)
        .await
        .unwrap();
    journal
        .open("Income:Sales", AccountKind::Income, on)
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, on)
        .await
        .unwrap();
    let amount: doubleentry::Amount = "5.00".parse().unwrap();
    let by_path = Entry::new("by-path", date!("2026-01-10"))
        .debit("Assets:Cash", amount, EUR)
        .credit("Income:Sales", amount, EUR);
    let by_handle = Entry::new("by-handle", date!("2026-01-10"))
        .debit(cash, amount, EUR)
        .credit("Income:Sales", amount, EUR);
    for draft in [by_path, by_handle] {
        assert_eq!(
            journal.record(draft.clone()).unwrap(),
            books.record(draft).await.unwrap()
        );
    }
    assert_eq!(journal.account(cash), journal.account("Assets:Cash"));
    assert_eq!(
        books.account(cash).await.unwrap(),
        books.account("Assets:Cash").await.unwrap()
    );
    for path in ["Assets:Csh", "not a path!", ""] {
        let draft = Entry::new(format!("bad-{path}"), date!("2026-01-10"))
            .debit(path, amount, EUR)
            .credit("Income:Sales", amount, EUR);
        let expected = journal.record(draft.clone()).unwrap_err();
        assert!(expected.rejection().is_some(), "{expected}");
        assert_eq!(
            books.record(draft).await.unwrap_err().rejection(),
            expected.rejection(),
            "{path}"
        );
    }
    assert_eq!(journal.head(), books.head().await.unwrap());
}

/// T022 (FR-032): a memo without a salt gets one from the façade's salt
/// source, and retries stay replays; a caller's salt is kept.
#[tokio::test]
async fn memo_salts_come_from_the_facade() {
    let (mut journal, books) = pair().await;
    let on = date!("2026-01-01");
    journal.declare_currency(EUR, 2, on).unwrap();
    books.declare_currency(EUR, 2, on).await.unwrap();
    journal.define_months(2026, 1..=1).unwrap();
    books.define_months(2026, 1..=1).await.unwrap();
    for (path, kind) in [
        ("Assets:Cash", AccountKind::Asset),
        ("Income:Sales", AccountKind::Income),
    ] {
        journal.open(path, kind, on).unwrap();
        books.open(path, kind, on).await.unwrap();
    }
    let amount: doubleentry::Amount = "5.00".parse().unwrap();
    let draft = |key: &str| {
        Entry::new(key, date!("2026-01-10"))
            .debit("Assets:Cash", amount, EUR)
            .credit("Income:Sales", amount, EUR)
    };
    let memo = draft("memo").memo("for the customer");
    let first = books.record(memo.clone()).await.unwrap();
    let size = books.head().await.unwrap().size;
    let again = books.record(memo.clone()).await.unwrap();
    assert!(again.replayed && !first.replayed);
    assert_eq!((again.id, again.position), (first.id, first.position));
    assert_eq!(books.head().await.unwrap().size, size, "one leaf");
    assert_eq!(journal.record(memo).unwrap(), first);
    // A caller's salt is the salt committed, not the source's.
    let salted = draft("salted").memo_salted("kept", MemoSalt([42; 16]));
    let recorded = journal.record(salted.clone()).unwrap();
    let stored = journal.entry(recorded.id).unwrap();
    let expected = doubleentry::record::MemoCommitment::compute(&MemoSalt([42; 16]), "kept");
    assert_eq!(stored.parts().memo, Some(expected));
    assert_eq!(books.record(salted).await.unwrap(), recorded);
    assert_eq!(journal.head(), books.head().await.unwrap());
}
