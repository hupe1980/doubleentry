//! One error to handle, whatever the backend: the same `match`
//! classifies every outcome alike on `Journal`, `Ledger<MemoryStore>` and
//! `Ledger<SqlStore<Sqlite>>`; a failed write is retried with the same draft;
//! tampered data is an integrity failure at its position.
#![cfg(all(feature = "sqlite", feature = "testing"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::prelude::*;
use doubleentry::store::{MemoryStore, SqlStore, Sqlite, Store};
use doubleentry::testing::Flaky;
use doubleentry::{AccountSpec, MemoSalt, Page, PageRead, Posting, error::IntegrityKind};

/// What one call ended in: the variant, the rejection code and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    variant: &'static str,
    code: Option<u16>,
    text: String,
}

fn outcome<T>(result: Result<T>) -> Outcome {
    let error = match result {
        Ok(_) => {
            return Outcome {
                variant: "ok",
                code: None,
                text: String::new(),
            };
        }
        Err(error) => error,
    };
    let variant = match &error {
        Error::Rejected(_) => "rejected",
        Error::Value(_) => "value",
        Error::Amount(_) => "amount",
        Error::Query(_) => "query",
        Error::Integrity(_) => "integrity",
        Error::Backend(_) => "backend",
        _ => "other",
    };
    Outcome {
        variant,
        code: error.rejection().map(Rejection::code),
        text: error.to_string(),
    }
}

fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

/// The drafts of the scenario: a funding, a withdrawal of 80.00 over the
/// 70.00 left, a malformed path and a malformed dimension label.
fn drafts() -> Vec<Entry> {
    vec![
        Entry::new("fund", date!("2026-03-01"))
            .debit("Assets:Bank", eur("70.00"), EUR)
            .credit("Liabilities:Wallets:alice", eur("70.00"), EUR),
        Entry::new("withdraw", date!("2026-03-02"))
            .debit("Liabilities:Wallets:alice", eur("80.00"), EUR)
            .credit("Assets:Bank", eur("80.00"), EUR),
        Entry::new("bad-path", date!("2026-03-03"))
            .debit("Assets::Bank", eur("1.00"), EUR)
            .credit("Liabilities:Wallets:alice", eur("1.00"), EUR),
        Entry::new("bad-label", date!("2026-03-03"))
            .posting(Posting::debit("Assets:Bank", eur("1.00"), EUR).dimension("", "x"))
            .credit("Liabilities:Wallets:alice", eur("1.00"), EUR),
    ]
}

fn wallet() -> AccountSpec {
    AccountSpec::new(
        "Liabilities:Wallets:alice",
        AccountKind::Liability,
        date!("2026-01-01"),
    )
    .limit(BalanceLimit::NoDebitBalance)
}

fn journal_scenario() -> Vec<Outcome> {
    let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap())
        .with_salt_source(|| MemoSalt([1; 16]));
    let on = date!("2026-01-01");
    let mut outcomes = vec![
        outcome(books.declare_currency(EUR, 2, on)),
        outcome(books.open("Assets:Bank", AccountKind::Asset, on)),
        outcome(books.register(wallet())),
        outcome(books.define_months(2026, 1..=12)),
    ];
    for draft in drafts() {
        outcomes.push(outcome(books.record(draft)));
    }
    outcomes.push(outcome(books.seal_period("2027-13")));
    outcomes.push(outcome(
        books.prove_entry_at(EntryId::from_bytes([3; 16]), 1),
    ));
    outcomes.push(outcome(books.balance(
        "Assets:Bank",
        EUR,
        &BalanceQuery::period("2027-13"),
    )));
    outcomes.push(outcome(books.balance(
        "Assets:Csh",
        EUR,
        &BalanceQuery::all(),
    )));
    outcomes
}

async fn ledger_scenario<S: Store<2>>(store: S) -> Vec<Outcome> {
    let books = Ledger::new(store, LedgerId::new("example.com/acme").unwrap())
        .await
        .unwrap()
        .with_salt_source(|| MemoSalt([1; 16]));
    let on = date!("2026-01-01");
    let mut outcomes = vec![
        outcome(books.declare_currency(EUR, 2, on).await),
        outcome(books.open("Assets:Bank", AccountKind::Asset, on).await),
        outcome(books.register(wallet()).await),
        outcome(books.define_months(2026, 1..=12).await),
    ];
    for draft in drafts() {
        outcomes.push(outcome(books.record(draft).await));
    }
    outcomes.push(outcome(books.seal_period("2027-13").await));
    outcomes.push(outcome(
        books.prove_entry_at(EntryId::from_bytes([3; 16]), 1).await,
    ));
    outcomes.push(outcome(
        books
            .balance("Assets:Bank", EUR, &BalanceQuery::period("2027-13"))
            .await,
    ));
    outcomes.push(outcome(
        books.balance("Assets:Csh", EUR, &BalanceQuery::all()).await,
    ));
    outcomes
}

async fn sqlite() -> (tempfile::TempDir, String, SqlStore<Sqlite>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("books.db").display().to_string();
    let store = SqlStore::sqlite(&format!("sqlite://{path}")).await.unwrap();
    (dir, path, store)
}

/// T008 (US1-1, US1-5, US1-6): one classification on every façade and
/// store — the limit breach is `Rejected(LimitExceeded)` with one code and
/// one text, a malformed path or label a stage-1 rejection, an impossible
/// read a `Query`; no ledger operation is ever `Value` or `Amount`.
#[tokio::test]
async fn one_match_classifies_every_outcome_alike() {
    let expected = journal_scenario();
    let (_dir, _, store) = sqlite().await;
    assert_eq!(ledger_scenario(MemoryStore::new()).await, expected);
    assert_eq!(ledger_scenario(store).await, expected);

    assert!(expected[..5].iter().all(|outcome| outcome.variant == "ok"));
    let limit = &expected[5];
    assert_eq!(limit.variant, "rejected");
    assert!(limit.text.starts_with("rejected: [5"), "{}", limit.text);
    assert!(limit.text.contains("Liabilities:Wallets:alice"));
    for malformed in &expected[6..8] {
        assert_eq!(malformed.variant, "rejected", "{malformed:?}");
        assert!(malformed.code.unwrap() < 200, "stage 1: {malformed:?}");
    }
    assert_eq!(expected[8].variant, "rejected", "an invalid period id");
    assert_eq!(expected[9].variant, "query");
    // T008/T021 (post-impl): an unknown period in a query and a misspelt
    // account are `Query` on every façade and store.
    assert_eq!(expected[10].variant, "query", "{:?}", expected[10]);
    assert!(expected[10].text.contains("2027-13"), "{:?}", expected[10]);
    assert_eq!(expected[11].variant, "query", "{:?}", expected[11]);
    assert!(
        expected[11].text.contains("Assets:Csh"),
        "{:?}",
        expected[11]
    );
    assert!(
        expected
            .iter()
            .all(|outcome| !["value", "amount"].contains(&outcome.variant))
    );

    // A standalone value constructor is the only source of `Value`.
    fn ledger_id() -> Result<LedgerId> {
        Ok(LedgerId::new("not a valid id!")?)
    }
    assert!(matches!(ledger_id(), Err(Error::Value(_))));
}

/// T009 (US1-2, US1-3): a write that failed before it applied is retried
/// and recorded once; one that may have applied is retried and comes back
/// as a replay with the original id and position.
#[tokio::test]
async fn a_failed_write_is_retried_with_the_same_draft() {
    retry_scenario(MemoryStore::new()).await;
    // SC-003 (post-impl): the same scenario over SQLite.
    let (_dir, _, store) = sqlite().await;
    retry_scenario(store).await;
}

async fn retry_scenario<S: Store>(store: S) {
    let id = LedgerId::new("example.com/acme").unwrap();
    let books = Ledger::new(store.clone(), id.clone()).await.unwrap();
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).await.unwrap();
    books
        .open("Assets:Bank", AccountKind::Asset, on)
        .await
        .unwrap();
    books.register(wallet()).await.unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    let fund = || drafts().remove(0);

    let before = Ledger::new(Flaky::new(store.clone(), 1).rates(100, 0), id.clone())
        .await
        .unwrap();
    let failed = before.record(fund()).await.unwrap_err();
    assert!(
        failed.is_transient() && !failed.may_have_applied(),
        "{failed}"
    );
    let size = books.head().await.unwrap().size;
    let recorded = books.record(fund()).await.unwrap();
    assert!(!recorded.replayed);
    assert_eq!(recorded.position, size);

    let deposit = || {
        Entry::new("deposit", date!("2026-03-05"))
            .debit("Assets:Bank", eur("5.00"), EUR)
            .credit("Liabilities:Wallets:alice", eur("5.00"), EUR)
    };
    let after = Ledger::new(Flaky::new(store, 1).rates(0, 100), id)
        .await
        .unwrap();
    let unknown = after.record(deposit()).await.unwrap_err();
    assert!(unknown.may_have_applied(), "{unknown}");
    let size = books.head().await.unwrap().size;
    let retried = books.record(deposit()).await.unwrap();
    assert!(retried.replayed);
    assert_eq!(retried.position + 1, size, "it had applied, once");
}

/// A `MemoryStore` whose log answers one leaf with a flipped byte — the
/// tamper of US1-4 on the in-memory store (SC-003, post-impl).
#[derive(Clone)]
struct Tampered {
    inner: MemoryStore,
    position: u64,
}

macro_rules! delegate {
    ($( fn $name:ident(&self $(, $arg:ident: $ty:ty)*) -> $out:ty; )*) => {
        $( async fn $name(&self $(, $arg: $ty)*) -> $out { self.inner.$name($($arg),*).await } )*
    };
}

impl Store for Tampered {
    async fn read_leaves(
        &self,
        read: &doubleentry::store::LeafRead,
    ) -> Result<Page<doubleentry::store::Leaf, u64>, doubleentry::store::StoreError> {
        let mut page = self.inner.read_leaves(read).await?;
        for leaf in &mut page.items {
            if leaf.position == self.position
                && let Some(last) = leaf.bytes.last_mut()
            {
                *last ^= 0xff;
            }
        }
        Ok(page)
    }
    async fn projection(
        &self,
        read: &doubleentry::store::ProjectionRead,
    ) -> Result<doubleentry::store::ProjectionPage<2>, doubleentry::store::StoreError> {
        use doubleentry::store::{Cell, ProjectionPart, ProjectionRow};
        let mut page = self.inner.projection(read).await?;
        if read.part == ProjectionPart::Leaves {
            for row in &mut page.items {
                if let [Cell::Int(position), kind, Cell::Bytes(bytes)] = row.cells()
                    && u64::try_from(*position) == Ok(self.position)
                {
                    let mut bytes = bytes.clone();
                    if let Some(last) = bytes.last_mut() {
                        *last ^= 0xff;
                    }
                    *row = ProjectionRow::new(vec![
                        Cell::Int(*position),
                        kind.clone(),
                        Cell::Bytes(bytes),
                    ]);
                }
            }
        }
        Ok(page)
    }
    delegate! {
        fn create(&self, genesis: &doubleentry::record::Genesis) -> Result<doubleentry::store::LedgerInfo, doubleentry::store::StoreError>;
        fn open(&self) -> Result<doubleentry::store::LedgerInfo, doubleentry::store::StoreError>;
        fn write(&self, op: &doubleentry::store::Op<2>) -> Result<Result<doubleentry::store::Done<2>, Rejection>, doubleentry::store::StoreError>;
        fn size(&self) -> Result<u64, doubleentry::store::StoreError>;
        fn read_nodes(&self, ids: &[u64]) -> Result<Vec<doubleentry::log::Hash>, doubleentry::store::StoreError>;
        fn seals(&self, through: Option<u64>) -> Result<Vec<doubleentry::store::SealRow>, doubleentry::store::StoreError>;
        fn accounts(&self, refs: &[doubleentry::AccountRef]) -> Result<Vec<Option<doubleentry::store::AccountRow>>, doubleentry::store::StoreError>;
        fn account_records(&self, handle: AccountHandle, page: PageRead<u64>) -> Result<Page<doubleentry::store::Leaf, u64>, doubleentry::store::StoreError>;
        fn entry(&self, by: doubleentry::store::EntryRef<'_>, through: Option<u64>) -> Result<Option<doubleentry::store::StoredEntry>, doubleentry::store::StoreError>;
        fn holds(&self, read: &doubleentry::store::HoldRead) -> Result<Page<doubleentry::store::HoldRow<2>, doubleentry::HoldCursor>, doubleentry::store::StoreError>;
        fn clearings(&self, key: doubleentry::store::BalanceKey, page: PageRead<u64>) -> Result<Page<doubleentry::store::Leaf, u64>, doubleentry::store::StoreError>;
        fn seal_nodes(&self, period: &doubleentry::PeriodId, currency: Currency, layer: Layer, ids: &[(u32, u32)]) -> Result<Option<Vec<Option<doubleentry::log::Hash>>>, doubleentry::store::StoreError>;
        fn balances(&self, read: &doubleentry::store::BalanceRead) -> Result<Page<(doubleentry::store::BalanceKey, Balance), doubleentry::store::BalanceKey>, doubleentry::store::StoreError>;
        fn postings(&self, read: &doubleentry::store::PostingRead) -> Result<Page<doubleentry::store::PostingLine<2>, doubleentry::store::PostingCursor>, doubleentry::store::StoreError>;
        fn memo(&self, id: EntryId) -> Result<Option<doubleentry::store::Memo>, doubleentry::store::StoreError>;
        fn erase_memo(&self, id: EntryId) -> Result<doubleentry::store::Erasure, doubleentry::store::StoreError>;
    }
}

/// US1-4 on `Journal` and `Ledger<MemoryStore>` (SC-003, post-impl): a
/// leaf altered behind the ledger's back is an integrity failure at its
/// position — never transient — on every target.
#[tokio::test]
async fn tampered_leaves_are_integrity_failures_on_every_target() {
    let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    books.open("Assets:Bank", AccountKind::Asset, on).unwrap();
    books.register(wallet()).unwrap();
    books.define_months(2026, 1..=12).unwrap();
    let fund = books.record(drafts().remove(0)).unwrap();
    let mut leaves: Vec<(u64, Vec<u8>)> = books
        .leaves()
        .map(|leaf| (leaf.position, leaf.bytes.to_vec()))
        .collect();
    if let Some(last) = leaves[fund.position as usize].1.last_mut() {
        *last ^= 0xff;
    }
    let restored = Journal::<2>::from_log(
        books.id(),
        leaves
            .iter()
            .map(|(position, bytes)| doubleentry::log::ReplayLeaf {
                position: *position,
                bytes,
                memo: None,
                fingerprint: None,
            }),
    );
    let Err(Error::Integrity(integrity)) = restored else {
        panic!("a tampered journal log is an integrity failure");
    };
    assert_eq!(integrity.position(), Some(fund.position));

    let store = MemoryStore::new();
    let ledger = Ledger::new(store.clone(), books.id().clone())
        .await
        .unwrap();
    ledger.declare_currency(EUR, 2, on).await.unwrap();
    ledger
        .open("Assets:Bank", AccountKind::Asset, on)
        .await
        .unwrap();
    ledger.register(wallet()).await.unwrap();
    ledger.define_months(2026, 1..=12).await.unwrap();
    let fund = ledger.record(drafts().remove(0)).await.unwrap();
    ledger.audit().await.unwrap();
    let tampered = Ledger::new(
        Tampered {
            inner: store,
            position: fund.position,
        },
        books.id().clone(),
    )
    .await
    .unwrap();
    let error = tampered.audit().await.unwrap_err();
    let Error::Integrity(integrity) = &error else {
        panic!("{error}");
    };
    assert_eq!(integrity.position(), Some(fund.position), "{error}");
    assert!(!error.is_transient() && !error.may_have_applied());
}

/// T010 (US1-4): a stored record edited behind the ledger's back makes
/// `audit` an integrity failure at that record's position, never transient.
#[tokio::test]
async fn tampered_data_is_an_integrity_failure_at_its_position() {
    use sqlx::{Connection, Executor, Row, SqliteConnection};

    let (_dir, path, store) = sqlite().await;
    let books = Ledger::new(store, LedgerId::new("example.com/acme").unwrap())
        .await
        .unwrap();
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).await.unwrap();
    books
        .open("Assets:Bank", AccountKind::Asset, on)
        .await
        .unwrap();
    books.register(wallet()).await.unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    let fund = books.record(drafts().remove(0)).await.unwrap();
    books.audit().await.unwrap();

    let mut db = SqliteConnection::connect(&format!("sqlite://{path}"))
        .await
        .unwrap();
    let triggers = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'trigger'")
        .fetch_all(&mut db)
        .await
        .unwrap();
    for trigger in triggers {
        let name: String = trigger.get(0);
        db.execute(sqlx::AssertSqlSafe(format!("DROP TRIGGER \"{name}\"")))
            .await
            .unwrap();
    }
    db.execute(sqlx::AssertSqlSafe(format!(
        "UPDATE leaves SET bytes = x'06ff' WHERE position = {}",
        fund.position
    )))
    .await
    .unwrap();

    let error = books.audit().await.unwrap_err();
    let Error::Integrity(integrity) = &error else {
        panic!("{error}");
    };
    assert_eq!(integrity.position(), Some(fund.position), "{error}");
    assert_eq!(integrity.kind(), IntegrityKind::Undecodable);
    assert!(!error.is_transient() && !error.may_have_applied());
}
