//! Regressions for audit findings in the rule core, the model and the
//! façades. Each proof of concept asserted the defect; each test here asserts
//! the corrected behaviour and names the proof of concept it inverts
//! (`// PoC: <id>`). The store findings are in `tests/sqlite.rs` and
//! `tests/postgres.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::store::{
    AccountRow, BalanceRead, BalanceScope, Done, EntryRef, Erasure, HoldRead, HoldRow, Leaf,
    LeafRead, LedgerInfo, Memo, MemoryStore, Op, Position, PostingCursor, PostingLine, PostingRead,
    ProjectionPage, ProjectionRead, Replayer, SealRow, Store, StoreError, StoredEntry,
};
use doubleentry::{
    AccountHandle, AccountKind, AccountPath, AccountRef, Amount, Balance, BalanceLimit, Entry,
    EntryId, Error, HoldCursor, Journal, Label, Layer, Ledger, LedgerId, LedgerPolicy, MemoSalt,
    Page, PageRead, Posting, PostingRef, Rejection,
    currency::EUR,
    date,
    error::{AuditFailureKind, ValueError},
    log::Hash,
    record::{Fingerprint, Genesis, MAX_LEAF_BYTES},
    store::BalanceKey,
};

fn ledger() -> LedgerId {
    LedgerId::new("example.com/acme").unwrap()
}

fn journal() -> Journal {
    Journal::new(ledger())
        .with_salt_source(|| MemoSalt([7; 16]))
        .as_actor("setup")
}

fn one() -> Amount {
    "1.00".parse().unwrap()
}

/// Cash, sales and the months of 2026 in EUR.
fn books() -> Journal {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books.define_months(2026, 1..=12).unwrap();
    books
}

/// A3-C3: every closing draft is measured with its own key, so a draft
/// numbered 100 (two key bytes longer than draft 1) still fits the leaf and
/// the year closes with the drafts `close_year` hands out.
// PoC: A3-C3 a3_c3_close_year_draft_hundred_exceeds_leaf_limit
#[test]
fn a3_c3_every_closing_draft_fits_and_records() {
    const PER_DRAFT: usize = 28;
    const SPECIAL: usize = 99 * PER_DRAFT;
    let mut books = books();
    books
        .open("Equity:Retained", AccountKind::Equity, date!("2026-01-01"))
        .unwrap();
    let axis = |i: usize| format!("{i}{}", "a".repeat(62));
    let mut drafts = Vec::new();
    for n in 0..(SPECIAL + 40) {
        let mut credit = Posting::credit("Income:Sales", one(), EUR);
        for i in 0..8 {
            let len = match (i, n) {
                (0, SPECIAL) => 43 + 6,
                (0, _) => 43,
                _ => 59,
            };
            credit = credit.dimension(axis(i), format!("{n:05}{}", "v".repeat(len)));
        }
        drafts.push(
            Entry::new(format!("s{n}"), date!("2026-06-01"))
                .debit("Assets:Cash", one(), EUR)
                .posting(credit),
        );
    }
    books.record_batch(drafts).unwrap();
    let closing = books
        .close_year("close-2026", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    assert!(closing.len() >= 100, "{} drafts", closing.len());
    for draft in &closing {
        assert!(draft.encoded_len() <= MAX_LEAF_BYTES);
    }
    for draft in closing {
        books.record(draft).unwrap();
    }
    books.audit().unwrap();
}

/// A3-C6: an exposure whose figure no longer fits an amount does not refuse
/// an entry that lowers it: the repayment is recorded.
// PoC: A3-C6 a3_c6_overflowing_exposure_blocks_repayment
#[test]
fn a3_c6_a_repayment_is_never_refused_by_an_overflowing_exposure() {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:A", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Assets:B", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books.define_months(2026, 1..=12).unwrap();
    let max = Amount::<2>::MAX;
    books
        .record(
            Entry::new("big", date!("2026-02-01"))
                .debit("Assets:A", max, EUR)
                .credit("Assets:B", max, EUR),
        )
        .unwrap();
    books
        .record(
            Entry::open_hold("hold", date!("2026-02-02"))
                .debit("Assets:A", one(), EUR)
                .credit("Assets:B", one(), EUR),
        )
        .unwrap();
    books
        .set_limit(
            "Assets:A",
            BalanceLimit::NoDebitBalance,
            date!("2026-02-03"),
        )
        .unwrap();
    // The repayment lowers A's exposure: recorded.
    books
        .record(
            Entry::new("repay", date!("2026-02-04"))
                .debit("Assets:B", one(), EUR)
                .credit("Assets:A", one(), EUR),
        )
        .unwrap();
    // A further draw would take A's gross debits past the maximum: refused.
    let draw = books.record(
        Entry::new("draw", date!("2026-02-05"))
            .debit("Assets:A", one(), EUR)
            .credit("Assets:B", one(), EUR),
    );
    assert!(
        matches!(
            draw,
            Err(Error::Rejected(Rejection::BalanceOverflow { .. }))
        ),
        "{draw:?}"
    );
    books.audit().unwrap();
}

/// A3-C7: a policy may not take effect on or before the latest booking date
/// already recorded (431), so it never contradicts an entry in the log; one
/// effective after it is accepted.
// PoC: A3-C7 a3_c7_policy_backdated_over_existing_entries
#[test]
fn a3_c7_a_policy_cannot_reach_back_over_recorded_entries() {
    let mut books = books();
    books
        .record(
            Entry::new("sale", date!("2026-03-15"))
                .debit("Assets:Cash", one(), EUR)
                .credit("Income:Sales", one(), EUR),
        )
        .unwrap();
    let policy = LedgerPolicy::new([Label::new("costcenter").unwrap()], None).unwrap();
    for effective in [date!("2026-03-01"), date!("2026-03-15")] {
        assert_eq!(
            books
                .set_policy(policy.clone(), effective)
                .unwrap_err()
                .rejection(),
            Some(&Rejection::PolicyBeforeBookings {
                effective,
                last_booking: date!("2026-03-15"),
            })
        );
    }
    books.set_policy(policy, date!("2026-03-16")).unwrap();
    let missing = books.record(
        Entry::new("sale-2", date!("2026-03-16"))
            .debit("Assets:Cash", one(), EUR)
            .credit("Income:Sales", one(), EUR),
    );
    assert!(matches!(
        missing,
        Err(Error::Rejected(Rejection::MissingDimension { .. }))
    ));
    books.audit().unwrap();
}

/// A3-C8: a hold's Pending postings are a reservation, not open items:
/// clearing them is refused (419).
// PoC: A3-C8 a3_c8_clearing_accepts_pending_hold_legs
#[test]
fn a3_c8_pending_postings_are_never_cleared() {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open(
            "Liabilities:Wallet",
            AccountKind::Liability,
            date!("2026-01-01"),
        )
        .unwrap();
    books.define_months(2026, 1..=12).unwrap();
    let hold = books
        .record(
            Entry::open_hold("h", date!("2026-03-01"))
                .debit("Liabilities:Wallet", one(), EUR)
                .credit("Liabilities:Wallet", one(), EUR),
        )
        .unwrap();
    let cleared = books.clear(
        "c",
        PostingRef::at(hold.id, 0),
        PostingRef::at(hold.id, 1),
        one(),
        date!("2026-03-02"),
    );
    assert_eq!(
        cleared.unwrap_err().rejection(),
        Some(&Rejection::ClearingNotSettled {
            posting: PostingRef::at(hold.id, 0)
        })
    );
    books.audit().unwrap();
}

/// A3-M-1: labels and path segments refuse invisible characters — Unicode
/// default-ignorable code points and line or paragraph separators — at an
/// edge or inside, so two accounts never print alike.
// PoC: A3-M-1 a3_model_label_accepts_invisible_format_chars
#[test]
fn a3_m1_invisible_characters_are_refused_in_labels_and_paths() {
    for (spoof, found) in [
        ("Assets:\u{200B}Cash", '\u{200B}'),
        ("Assets:Cash\u{FEFF}", '\u{FEFF}'),
        ("Assets:Ca\u{200D}sh", '\u{200D}'),
        ("Assets:\u{202E}hsaC", '\u{202E}'),
        ("Assets:Ca\u{2028}sh", '\u{2028}'),
        ("Assets:Ca\u{00AD}sh", '\u{00AD}'),
    ] {
        assert_eq!(
            AccountPath::parse(spoof).unwrap_err(),
            ValueError::InvisibleCharacter { found },
            "{spoof:?}"
        );
        assert!(Label::new(spoof.trim_start_matches("Assets:")).is_err());
    }
    let mut books = books();
    let refused = books.open(
        "Assets:Ca\u{200B}sh",
        AccountKind::Asset,
        date!("2026-01-01"),
    );
    assert!(matches!(
        refused,
        Err(Error::Rejected(Rejection::InvalidPath { .. }))
    ));
    // A visible character is still a label character.
    assert!(Label::new("Café").is_ok());
}

/// A3-S5: a memo holding NUL (or another control character but tab and
/// line breaks) is refused structurally (108) — by every store alike, so a
/// ledger moves between dialects unchanged.
// PoC: A3-S5 a3_s5_nul_in_memo_splits_the_dialects
#[test]
fn a3_s5_control_characters_in_a_memo_are_refused_structurally() {
    let mut books = books();
    let draft = |key: &str, memo: &str| {
        Entry::new(key, date!("2026-03-15"))
            .debit("Assets:Cash", one(), EUR)
            .credit("Income:Sales", one(), EUR)
            .memo(memo)
    };
    for memo in ["a\0b", "bell\u{7}", "esc\u{1b}[0m"] {
        assert_eq!(
            books.record(draft("m", memo)).unwrap_err().rejection(),
            Some(&Rejection::InvalidMemo {
                reason: ValueError::ControlCharacter
            }),
            "{memo:?}"
        );
    }
    books
        .record(draft("ok", "line one\nline two\ttabbed\r\n"))
        .unwrap();
}

/// A3-S3: the replay recomputes every entry's draft fingerprint from its
/// record and memo text; a stored fingerprint that differs fails the audit,
/// and so does one kept after its memo was erased.
// PoC: A3-S3 a3_s3_fingerprint_forgery_is_invisible_to_audit
#[test]
fn a3_s3_fingerprints_are_recomputed_by_the_replay() {
    let mut books = books();
    books
        .record(
            Entry::new("memo", date!("2026-03-15"))
                .debit("Assets:Cash", one(), EUR)
                .credit("Income:Sales", one(), EUR)
                .memo("Max Mustermann"),
        )
        .unwrap();
    let forged = Fingerprint::from_bytes([0; 32]);
    let mut replay = Replayer::<2>::new(books.id());
    let mut failure = None;
    for leaf in books.leaves() {
        let fed = replay.feed(doubleentry::log::ReplayLeaf {
            fingerprint: leaf.fingerprint.map(|_| forged),
            ..leaf
        });
        if let Err(error) = fed {
            failure = Some(error);
            break;
        }
    }
    assert_eq!(failure.unwrap().kind, AuditFailureKind::FingerprintMismatch);
    // A fingerprint kept beside an erased memo is refused too.
    let mut replay = Replayer::<2>::new(books.id());
    let mut failure = None;
    for leaf in books.leaves() {
        let fed = replay.feed(doubleentry::log::ReplayLeaf { memo: None, ..leaf });
        if let Err(error) = fed {
            failure = Some(error);
            break;
        }
    }
    assert_eq!(failure.unwrap().kind, AuditFailureKind::FingerprintMismatch);
}

/// A store that counts the sealed balances it reads, and serves seal nodes
/// only when `serve`.
#[derive(Clone, Default)]
struct Counted {
    inner: MemoryStore,
    serve: bool,
    sealed_rows: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Counted {
    fn sealed_rows(&self) -> usize {
        self.sealed_rows.load(std::sync::atomic::Ordering::Relaxed)
    }
}

macro_rules! forward {
    ($($method:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {
        $(
            async fn $method(&self, $($arg: $ty),*) -> Result<$out, StoreError> {
                self.inner.$method($($arg),*).await
            }
        )*
    };
}

impl Store<2> for Counted {
    forward! {
        create(genesis: &Genesis) -> LedgerInfo;
        open() -> LedgerInfo;
        write(op: &Op<2>) -> Result<Done<2>, Rejection>;
        size() -> u64;
        read_leaves(read: &LeafRead) -> Page<Leaf, Position>;
        read_nodes(ids: &[u64]) -> Vec<Hash>;
        seals(through: Option<u64>) -> Vec<SealRow>;
        accounts(refs: &[AccountRef]) -> Vec<Option<AccountRow>>;
        account_records(handle: AccountHandle, page: PageRead<Position>) -> Page<Leaf, Position>;
        entry(by: EntryRef<'_>, through: Option<u64>) -> Option<StoredEntry>;
        holds(read: &HoldRead) -> Page<HoldRow<2>, HoldCursor>;
        clearings(key: BalanceKey, page: PageRead<Position>) -> Page<Leaf, Position>;
        projection(read: &ProjectionRead) -> ProjectionPage<2>;
        postings(read: &PostingRead) -> Page<PostingLine<2>, PostingCursor>;
        memo(id: EntryId) -> Option<Memo>;
        erase_memo(id: EntryId) -> Erasure;
    }

    async fn balances(
        &self,
        read: &BalanceRead,
    ) -> Result<Page<(BalanceKey, Balance), BalanceKey>, StoreError> {
        let page = self.inner.balances(read).await?;
        if matches!(read.scope, BalanceScope::Sealed(_)) {
            self.sealed_rows
                .fetch_add(page.items.len(), std::sync::atomic::Ordering::Relaxed);
        }
        Ok(page)
    }

    async fn seal_nodes(
        &self,
        period: &doubleentry::PeriodId,
        currency: doubleentry::Currency,
        layer: Layer,
        ids: &[(u32, u32)],
    ) -> Result<Option<Vec<Option<Hash>>>, StoreError> {
        if self.serve {
            self.inner.seal_nodes(period, currency, layer, ids).await
        } else {
            Ok(None)
        }
    }
}

/// L-1: a sealed-balance proof reads the account's leaf and the stored nodes
/// of its path, not every leaf of the period; a store serving no seal nodes
/// gets the same proof by rebuilding the tree. Both verify.
// PoC: L-1 a3_log_prove_balance_is_linear_in_accounts
#[tokio::test]
async fn l1_a_sealed_balance_proof_reads_one_leaf_and_its_path() {
    let without = Counted::default();
    let with = Counted {
        serve: true,
        ..Counted::default()
    };
    let fallback = Ledger::new(without.clone(), ledger()).await.unwrap();
    let served = Ledger::new(with.clone(), ledger()).await.unwrap();
    for books in [&fallback, &served] {
        books
            .declare_currency(EUR, 2, date!("2026-01-01"))
            .await
            .unwrap();
        books
            .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
            .await
            .unwrap();
        books.define_months(2026, 1..=1).await.unwrap();
        for n in 0..150 {
            let path = format!("Assets:Customers:c{n:03}");
            books
                .open(path.as_str(), AccountKind::Asset, date!("2026-01-01"))
                .await
                .unwrap();
            books
                .record(
                    Entry::new(format!("s{n}"), date!("2026-01-10"))
                        .debit(path.as_str(), one(), EUR)
                        .credit("Income:Sales", one(), EUR),
                )
                .await
                .unwrap();
        }
        books.seal_period("2026-01").await.unwrap();
    }
    let accounts = [
        "Assets:Customers:c000",
        "Assets:Customers:c077",
        "Income:Sales",
    ];
    for account in accounts {
        let head = served.head().await.unwrap();
        let proof = served
            .prove_balance(account, EUR, Layer::Settled, "2026-01")
            .await
            .unwrap();
        proof.verify(&head).unwrap();
        let rebuilt = fallback
            .prove_balance(account, EUR, Layer::Settled, "2026-01")
            .await
            .unwrap();
        assert_eq!(proof, rebuilt, "{account}");
    }
    // Served: one leaf per proof. Not served: every leaf of the period.
    assert_eq!(with.sealed_rows(), accounts.len());
    assert!(without.sealed_rows() >= accounts.len() * 151);
    served.audit().await.unwrap();
    fallback.audit().await.unwrap();
}
