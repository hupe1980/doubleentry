//! Golden vectors: every record kind's leaf bytes and hash, the log root, the
//! entry id, the memo commitment, the draft fingerprint, the balance-tree
//! constants and the RFC 6962 vectors — against `tests/vectors/golden.txt`,
//! the output of the independent reference encoder `tests/vectors/ref_encoder.py`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use doubleentry::store::Tree;
use doubleentry::{
    AccountHandle, AccountKind, Amount, Balance, BalanceLimit, CaptureTime, Currency, Dimensions,
    Document, Entry, EntryId, EntryRecord, IdempotencyKey, Label, Layer, LedgerId, LedgerPolicy,
    MemoSalt, PostAmount, PostingRef, Rate, Side,
    currency::{EUR, USD},
    log::{BalanceTree, Hash, empty_root, leaf_hash, node_hash, root_of, zero_leaf_hash},
    record::{
        AccountAction, AccountChange, AccountEvent, AccountRegistration, ClearingAction,
        ClearingEvent, CurrencyDeclaration, EntryParts, Genesis, HoldAction, MemoCommitment,
        PeriodDefinition, PolicyEvent, PostingRecord, Record, Seal, SealParts,
    },
};
use time::Date;
use time::macros::date;

const LEDGER: &str = "example.com/acme";

fn golden() -> BTreeMap<String, String> {
    include_str!("vectors/golden.txt")
        .lines()
        .filter_map(|line| line.split_once(" = "))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn expect(vectors: &BTreeMap<String, String>, name: &str, actual: &str) {
    let expected = vectors
        .get(name)
        .unwrap_or_else(|| panic!("no vector {name}"));
    assert_eq!(actual, expected, "vector {name}");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ledger() -> LedgerId {
    LedgerId::new(LEDGER).unwrap()
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).unwrap()
}

fn label(text: &str) -> Label {
    Label::new(text).unwrap()
}

fn id(name: &str) -> EntryId {
    EntryId::derive(&ledger(), &key(name))
}

fn handle(index: u32) -> AccountHandle {
    AccountHandle::new(index).unwrap()
}

fn minor(value: i128) -> Amount {
    Amount::from_minor(value).unwrap()
}

fn salt() -> MemoSalt {
    MemoSalt(core::array::from_fn(|i| u8::try_from(i).unwrap()))
}

fn documents() -> Vec<Document> {
    vec![
        Document::new("invoice", "RE-2026-0001")
            .unwrap()
            .dated(date!(2026 - 03 - 15))
            .with_sha256([0x11; 32]),
        Document::new("receipt", "BON-77").unwrap(),
    ]
}

fn rates() -> Vec<Rate> {
    vec![
        Rate::parse(EUR, USD, "1.0873").unwrap(),
        Rate::parse(USD, Currency::new("CHF").unwrap(), "0.9").unwrap(),
    ]
}

fn posting(
    index: u32,
    side: Side,
    amount: i128,
    currency: Currency,
    layer: Layer,
    dims: &[(&str, &str)],
) -> PostingRecord {
    let dimensions = dims.iter().fold(Dimensions::new(), |d, (axis, value)| {
        d.with(label(axis), label(value)).unwrap()
    });
    PostingRecord {
        handle: handle(index),
        side,
        amount: minor(amount),
        currency,
        layer,
        dimensions,
    }
}

fn entry(
    name: &str,
    booking: Date,
    hold: Option<HoldAction>,
    postings: Vec<PostingRecord>,
) -> Record {
    Record::Entry(
        EntryRecord::new(EntryParts {
            key: key(name),
            booking_date: booking,
            value_date: booking,
            captured_at: None,
            kind: None,
            memo: None,
            documents: vec![],
            postings,
            reverses: None,
            hold,
            rates: vec![],
        })
        .unwrap(),
    )
}

fn account_change(change: AccountChange) -> Record {
    Record::Account(AccountEvent {
        action: AccountAction::Change {
            handle: handle(0),
            change,
        },
        effective: date!(2026 - 02 - 01),
        actor: label("setup"),
    })
}

fn balance(debit: i128, credit: i128, postings: u64) -> Balance {
    Balance::from_parts(minor(debit), minor(credit), postings).unwrap()
}

fn tree(currency: Currency, layer: Layer, leaves: &[(u32, Balance)]) -> BalanceTree {
    let mut tree = BalanceTree::new(currency, layer, 3);
    for (index, balance) in leaves {
        tree.set(handle(*index), *balance).unwrap();
    }
    tree
}

fn eur_settled() -> BalanceTree {
    tree(
        EUR,
        Layer::Settled,
        &[(0, balance(119_000, 0, 1)), (2, balance(0, 119_000, 1))],
    )
}

fn usd_pending() -> BalanceTree {
    tree(USD, Layer::Pending, &[(2, balance(5000, 0, 1))])
}

/// The reference instances, in log order (the names of `golden.txt`).
fn reference_log() -> Vec<(&'static str, Record)> {
    let full = EntryRecord::new(EntryParts {
        key: key("invoice-2026-0001"),
        booking_date: date!(2026 - 03 - 15),
        value_date: date!(2026 - 03 - 17),
        captured_at: Some(CaptureTime::new(1_773_568_800, 60).unwrap()),
        kind: Some(label("invoice")),
        memo: Some(MemoCommitment::compute(&salt(), "Rechnung 4711")),
        documents: documents(),
        postings: vec![
            posting(
                0,
                Side::Debit,
                119_000,
                EUR,
                Layer::Settled,
                &[("project", "alpha"), ("cost-center", "100")],
            ),
            posting(1, Side::Credit, 119_000, EUR, Layer::Settled, &[]),
            posting(
                2,
                Side::Debit,
                5000,
                USD,
                Layer::Pending,
                &[("project", "beta")],
            ),
        ],
        reverses: Some(id("invoice-2026-0000")),
        hold: Some(HoldAction::Post {
            hold: id("hold-2026-0001"),
            amount: PostAmount::Partial(minor(2500)),
        }),
        rates: rates(),
    })
    .unwrap();
    let seal = Seal::new(SealParts {
        period: label("2026-03"),
        start: date!(2026 - 03 - 01),
        end: date!(2026 - 03 - 31),
        through: 15,
        accounts: 3,
        trees: vec![eur_settled().head().unwrap(), usd_pending().head().unwrap()],
    })
    .unwrap();
    vec![
        ("genesis", Record::Genesis(Genesis { ledger: ledger() })),
        (
            "account_register",
            Record::Account(AccountEvent {
                action: AccountAction::Register(AccountRegistration {
                    handle: handle(0),
                    path: "Assets:Cash".parse().unwrap(),
                    kind: AccountKind::Asset,
                    class: None,
                    open_from: date!(2026 - 01 - 01),
                    open_to: None,
                    limit: BalanceLimit::Unlimited,
                }),
                effective: date!(2026 - 01 - 01),
                actor: label("setup"),
            }),
        ),
        (
            "account_change_open_to",
            account_change(AccountChange::OpenTo(Some(date!(2026 - 12 - 31)))),
        ),
        (
            "account_change_limit",
            account_change(AccountChange::Limit(BalanceLimit::NoCreditBalance)),
        ),
        (
            "account_change_class",
            account_change(AccountChange::Class(Some(label("current")))),
        ),
        (
            "currency",
            Record::Currency(CurrencyDeclaration {
                code: EUR,
                exponent: 2,
                effective: date!(2026 - 01 - 01),
                actor: label("setup"),
            }),
        ),
        (
            "policy",
            Record::Policy(PolicyEvent {
                policy: LedgerPolicy::new([label("project"), label("cost-center")], Some(30))
                    .unwrap(),
                effective: date!(2026 - 01 - 01),
                actor: label("setup"),
            }),
        ),
        (
            "period",
            Record::Period(
                PeriodDefinition::new(
                    label("2026-03"),
                    date!(2026 - 03 - 01),
                    date!(2026 - 03 - 31),
                )
                .unwrap(),
            ),
        ),
        ("entry_full", Record::Entry(full)),
        (
            "entry_minimal",
            entry(
                "minimal-1",
                date!(2026 - 03 - 16),
                None,
                vec![
                    posting(0, Side::Debit, 100, EUR, Layer::Settled, &[]),
                    posting(1, Side::Credit, 100, EUR, Layer::Settled, &[]),
                ],
            ),
        ),
        (
            "hold_open",
            entry(
                "hold-2026-0001",
                date!(2026 - 03 - 10),
                Some(HoldAction::Open {
                    expires: Some(date!(2026 - 03 - 31)),
                }),
                vec![
                    posting(0, Side::Debit, 5000, EUR, Layer::Pending, &[]),
                    posting(1, Side::Credit, 5000, EUR, Layer::Pending, &[]),
                ],
            ),
        ),
        (
            "hold_post",
            entry(
                "hold-2026-0001-post",
                date!(2026 - 03 - 12),
                Some(HoldAction::Post {
                    hold: id("hold-2026-0001"),
                    amount: PostAmount::Full,
                }),
                vec![],
            ),
        ),
        (
            "hold_void",
            entry(
                "hold-2026-0002-void",
                date!(2026 - 03 - 12),
                Some(HoldAction::Void {
                    hold: id("hold-2026-0002"),
                }),
                vec![],
            ),
        ),
        (
            "hold_expire",
            entry(
                "hold-2026-0003-expire",
                date!(2026 - 04 - 01),
                Some(HoldAction::Expire {
                    hold: id("hold-2026-0003"),
                }),
                vec![],
            ),
        ),
        (
            "clearing_apply",
            Record::Clearing(ClearingEvent {
                action: ClearingAction::Apply {
                    key: key("clear-1"),
                    debit: PostingRef::at(id("invoice-2026-0001"), 0),
                    credit: PostingRef::at(id("minimal-1"), 1),
                    amount: minor(100),
                },
                date: date!(2026 - 03 - 20),
            }),
        ),
        (
            "clearing_reset",
            Record::Clearing(ClearingEvent {
                action: ClearingAction::Reset {
                    key: key("clear-1-reset"),
                    clearing: 14,
                },
                date: date!(2026 - 03 - 21),
            }),
        ),
        ("seal", Record::Seal(seal)),
    ]
}

#[test]
fn every_record_kind_matches_the_reference_encoder() {
    let vectors = golden();
    for (name, record) in reference_log() {
        let bytes = record.to_leaf_bytes();
        expect(&vectors, &format!("{name}.leaf"), &hex(&bytes));
        expect(
            &vectors,
            &format!("{name}.hash"),
            &record.leaf_hash().to_string(),
        );
        assert_eq!(
            Record::decode(&bytes).as_ref(),
            Ok(&record),
            "{name} decodes back"
        );
    }
}

#[test]
fn the_log_root_matches() {
    let vectors = golden();
    let log = reference_log();
    let mut tree = Tree::new();
    for (_, record) in &log {
        tree.push(record.leaf_hash()).unwrap();
    }
    expect(&vectors, "log.root", &tree.head().root.to_string());
    let genesis_and_currency = [log[0].1.leaf_hash(), log[5].1.leaf_hash()];
    expect(
        &vectors,
        "log.root_genesis_currency",
        &root_of(&genesis_and_currency).to_string(),
    );
    // SC-001's known answers, independent of the file.
    assert_eq!(
        root_of(&genesis_and_currency).to_string(),
        "d434000ff6c3a9c155f121e2eec3488c3b148f003b6ac46859aa39e2ca657715"
    );
    assert_eq!(
        hex(&log[0].1.to_leaf_bytes()),
        "0110000000000000006578616d706c652e636f6d2f61636d65020d00000000000000646f75626c65656e7472792f31"
    );
    assert_eq!(
        log[0].1.leaf_hash().to_string(),
        "cd615a0dac3171f8c156c2f65bc613bf9d1a5381874de8ca201b508d203e83d5"
    );
    assert_eq!(
        log[5].1.leaf_hash().to_string(),
        "10db827e838327bdf4db3b275cbda802f34e4aca0284bea419ce98b7714a1bbc"
    );
    assert_eq!(
        log[1].1.leaf_hash().to_string(),
        "958cbcc6880ff4aab363af382587e3534ed9b6a87bbf5c9518b7ead4e5cd3c5c"
    );
}

#[test]
fn identity_memo_and_fingerprint_match() {
    let vectors = golden();
    expect(&vectors, "entry_id", &id("invoice-2026-0001").to_string());
    assert_eq!(
        id("invoice-2026-0001").to_string(),
        "bab3f09bfedcc7d793e973e78dbe4bd4"
    );
    let memo = MemoCommitment::compute(&salt(), "Rechnung 4711");
    expect(&vectors, "memo_commitment", &memo.to_string());
    assert_eq!(
        memo.to_string(),
        "d1f4b79422abbf28c679a18d6487c3faafdd151b1f1fc0fc941d2dabd3e47dbc"
    );
    let mut draft: Entry = Entry::new("invoice-2026-0001", date!(2026 - 03 - 15))
        .value_date(date!(2026 - 03 - 17))
        .captured_at(CaptureTime::new(1_773_568_800, 60).unwrap())
        .kind("invoice")
        .memo("Rechnung 4711");
    for document in documents() {
        draft = draft.document(document);
    }
    draft = draft
        .posting(
            doubleentry::Posting::debit("Assets:Cash", minor(119_000), EUR)
                .dimension("project", "alpha")
                .dimension("cost-center", "100"),
        )
        .credit(handle(1), minor(119_000), EUR)
        .reverses(id("invoice-2026-0000"));
    // Rates are given in reverse order: the draft keeps them sorted.
    for rate in rates().into_iter().rev() {
        draft = draft.rate(rate);
    }
    expect(
        &vectors,
        "fingerprint_full_draft",
        &hex(draft.fingerprint().unwrap().as_bytes()),
    );
}

#[test]
fn balance_tree_constants_match() {
    let vectors = golden();
    let z0 = zero_leaf_hash();
    let z1 = node_hash(&z0, &z0);
    let z2 = node_hash(&z1, &z1);
    expect(&vectors, "balance.zero_leaf", &z0.to_string());
    expect(&vectors, "balance.z1", &z1.to_string());
    expect(&vectors, "balance.z2", &z2.to_string());
    expect(
        &vectors,
        "balance.root_3",
        &eur_settled().root().to_string(),
    );
    expect(
        &vectors,
        "balance.root_usd_pending",
        &usd_pending().root().to_string(),
    );
    assert_eq!(
        z0.to_string(),
        "e2ba45c903dc4ca03b6cf2b47661325a9318d7322d3303903b86629d9a33ef8d"
    );
    assert_eq!(
        z1.to_string(),
        "4727b671550593680af002492c831ac7582eacd0618cc16509a2bbc7a60545ff"
    );
    assert_eq!(
        z2.to_string(),
        "04ee1353c899241c75fef221cd3420c29a4fdde9a4bd4a38e236180822f85368"
    );
    assert_eq!(
        eur_settled().root().to_string(),
        "61a2dffcc9633717adfac15db9f40c2f9390bfab45181a7e2dad078cf25451e8"
    );
}

fn rfc6962_leaves() -> Vec<Hash> {
    [
        "",
        "00",
        "10",
        "2021",
        "3031",
        "40414243",
        "5051525354555657",
        "606162636465666768696a6b6c6d6e6f",
    ]
    .iter()
    .map(|text| {
        let bytes: Vec<u8> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect();
        leaf_hash(&bytes)
    })
    .collect()
}

fn path(hashes: &[Hash]) -> String {
    hashes
        .iter()
        .map(Hash::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn rfc6962_vectors_match() {
    let vectors = golden();
    let leaves = rfc6962_leaves();
    let mut tree = Tree::new();
    expect(&vectors, "rfc6962.root.0", &empty_root().to_string());
    for (n, leaf) in leaves.iter().enumerate() {
        tree.push(*leaf).unwrap();
        expect(
            &vectors,
            &format!("rfc6962.root.{}", n + 1),
            &tree.head().root.to_string(),
        );
    }
    for (m, n) in [(0u64, 8u64), (5, 8), (2, 3), (1, 5)] {
        let proof = tree.inclusion_proof(m, n).unwrap();
        expect(
            &vectors,
            &format!("rfc6962.inclusion.{m}.{n}"),
            &path(&proof.path),
        );
    }
    for (m, n) in [(1u64, 8u64), (6, 8), (2, 5)] {
        let proof = tree.consistency_proof(m, n).unwrap();
        expect(
            &vectors,
            &format!("rfc6962.consistency.{m}.{n}"),
            &path(&proof.path),
        );
    }
}
