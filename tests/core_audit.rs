//! `audit()` as a replay: every tamper fixture fails with its kind and
//! position, untampered journals pass, and `from_log` reproduces a journal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{Claims, audit_leaves, books, eur, leaf_bytes, sale};
use doubleentry::store::{
    Cell, MemoPlain, ProjectionPart, ProjectionRow, Replayer, StoreProjection,
};
use doubleentry::{
    AccountHandle, AccountKind, AccountRef, BalanceLimit, Dimensions, Entry, EntryRecord,
    IdempotencyKey, Journal, Label, Layer, LedgerId, PostingRef, Rejection, Side,
    currency::EUR,
    date,
    error::AuditFailure,
    record::{
        AccountAction, AccountEvent, AccountRegistration, EntryParts, PostingRecord, Record,
        SealParts,
    },
};
use doubleentry::{error::AuditFailureKind, log::ReplayLeaf};

/// A journal with an entry, a memo entry, a reversal, a clearing and a seal.
struct Sample {
    books: Journal,
    memo_at: u64,
    seal_at: u64,
}

fn sample() -> Sample {
    let mut books = books();
    let invoice = books
        .record(sale("inv", date!("2026-01-10"), "10.00"))
        .unwrap();
    let memo_at = books
        .record(sale("memo", date!("2026-01-11"), "20.00").memo("Rechnung 4711"))
        .unwrap()
        .position;
    let payment = books
        .record(
            Entry::new("pay", date!("2026-01-12"))
                .debit("Income:Sales", eur("10.00"), EUR)
                .credit("Assets:Cash", eur("10.00"), EUR),
        )
        .unwrap();
    books
        .clear(
            "clr",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(payment.id, 1),
            eur("10.00"),
            date!("2026-01-13"),
        )
        .unwrap();
    let reversal = books
        .reversal(invoice.id, "rev", date!("2026-01-14"))
        .unwrap();
    books.record(reversal).unwrap();
    let seal_at = books.seal_period("2026-01").unwrap().position;
    books
        .record(sale("feb", date!("2026-02-10"), "5.00"))
        .unwrap();
    books.audit().unwrap();
    Sample {
        books,
        memo_at,
        seal_at,
    }
}

fn forged_entry(key: &str, on: doubleentry::Date, legs: &[(u32, Side, &str)]) -> Vec<u8> {
    let postings = legs
        .iter()
        .map(|&(handle, side, amount)| PostingRecord {
            handle: AccountHandle::new(handle).unwrap(),
            side,
            amount: eur(amount),
            currency: EUR,
            layer: Layer::Settled,
            dimensions: Dimensions::new(),
        })
        .collect();
    Record::Entry(
        EntryRecord::new(EntryParts {
            key: IdempotencyKey::new(key).unwrap(),
            booking_date: on,
            value_date: on,
            captured_at: None,
            kind: None,
            memo: None,
            documents: Vec::new(),
            postings,
            reverses: None,
            hold: None,
            rates: Vec::new(),
        })
        .unwrap(),
    )
    .to_leaf_bytes()
}

/// Audits tampered leaves against honest claims.
fn tampered(sample: &Sample, tamper: impl FnOnce(&mut Vec<Vec<u8>>)) -> Failed {
    let mut leaves = leaf_bytes(&sample.books);
    tamper(&mut leaves);
    failed(audit_leaves(&sample.books, &leaves, &Claims::of(&sample.books)).unwrap_err())
}

/// Audits honest leaves against tampered claims.
fn claimed(sample: &Sample, tamper: impl FnOnce(&mut StoreProjection)) -> Failed {
    let mut claims = Claims::of(&sample.books);
    tamper(&mut claims);
    failed(audit_leaves(&sample.books, &leaf_bytes(&sample.books), &claims).unwrap_err())
}

/// An audit failure as its comparable fields.
type Failed = (Option<u64>, AuditFailureKind);

fn failed(failure: AuditFailure) -> Failed {
    (failure.position, failure.kind)
}

fn at(position: u64, kind: AuditFailureKind) -> Failed {
    (Some(position), kind)
}

/// The part and rows of a projection mismatch.
fn mismatch(failure: Failed) -> (ProjectionPart, Option<ProjectionRow>, Option<ProjectionRow>) {
    match failure {
        (
            None,
            AuditFailureKind::ProjectionMismatch {
                part,
                expected,
                found,
            },
        ) => (part, expected.map(|row| *row), found.map(|row| *row)),
        other => panic!("not a projection mismatch: {other:?}"),
    }
}

// ── The tamper fixtures (FR-031) ──────────────────────────────────────────────

#[test]
fn fixture_01_a_flipped_leaf_byte() {
    let sample = sample();
    let position = sample.memo_at;
    let failure = tampered(&sample, |leaves| {
        let leaf = &mut leaves[position as usize];
        let amount = 2000i128.to_le_bytes();
        let at = leaf.windows(16).position(|w| w == amount).unwrap();
        leaf[at] ^= 1;
    });
    assert_eq!(failure.0, Some(position));
    assert!(matches!(
        failure.1,
        AuditFailureKind::WouldReject(Rejection::Unbalanced { .. })
    ));
}

#[test]
fn fixture_02_an_undecodable_leaf() {
    let sample = sample();
    let failure = tampered(&sample, |leaves| leaves[3] = vec![0x99]);
    assert_eq!(failure.0, Some(3));
    assert!(matches!(failure.1, AuditFailureKind::Undecodable(_)));
}

#[test]
fn fixture_03_position_zero_is_not_a_genesis() {
    let sample = sample();
    let failure = tampered(&sample, |leaves| leaves[0] = leaves[1].clone());
    assert_eq!(failure, at(0, AuditFailureKind::GenesisMissing));
}

#[test]
fn fixture_04_a_genesis_for_another_ledger() {
    let sample = sample();
    let other = Journal::new(LedgerId::new("example.com/other").unwrap());
    let genesis = leaf_bytes(&other)[0].clone();
    let failure = tampered(&sample, |leaves| leaves[0] = genesis);
    assert_eq!(failure, at(0, AuditFailureKind::GenesisMismatch));
    let scaled = Journal::<4>::with_precision(common::ledger());
    let genesis = scaled.leaves().next().unwrap().bytes.to_vec();
    let failure = tampered(&sample, |leaves| leaves[0] = genesis);
    assert_eq!(failure, at(0, AuditFailureKind::GenesisMismatch));
}

#[test]
fn fixture_05_two_cancelling_one_legged_entries() {
    let sample = sample();
    let size = sample.books.head().size;
    let failure = tampered(&sample, |leaves| {
        leaves.push(forged_entry(
            "a",
            date!("2026-03-01"),
            &[(0, Side::Debit, "1.00")],
        ));
        leaves.push(forged_entry(
            "b",
            date!("2026-03-01"),
            &[(1, Side::Credit, "1.00")],
        ));
    });
    assert_eq!(
        failure,
        at(
            size,
            AuditFailureKind::WouldReject(Rejection::TooFewPostings { count: 1 })
        )
    );
}

#[test]
fn fixture_06_an_entry_inside_a_sealed_period() {
    let sample = sample();
    let size = sample.books.head().size;
    let failure = tampered(&sample, |leaves| {
        leaves.push(forged_entry(
            "late",
            date!("2026-01-20"),
            &[(0, Side::Debit, "1.00"), (1, Side::Credit, "1.00")],
        ));
    });
    assert_eq!(
        failure,
        at(
            size,
            AuditFailureKind::WouldReject(Rejection::PeriodSealed {
                period: Label::new("2026-01").unwrap(),
                date: date!("2026-01-20")
            })
        )
    );
}

#[test]
fn fixture_07_an_entry_on_an_unregistered_handle() {
    let sample = sample();
    let size = sample.books.head().size;
    let failure = tampered(&sample, |leaves| {
        leaves.push(forged_entry(
            "ghost",
            date!("2026-02-20"),
            &[(42, Side::Debit, "1.00"), (1, Side::Credit, "1.00")],
        ));
    });
    assert_eq!(
        failure,
        at(
            size,
            AuditFailureKind::WouldReject(Rejection::UnknownAccount {
                account: AccountRef::Handle(AccountHandle::new(42).unwrap())
            })
        )
    );
}

#[test]
fn fixture_08_a_duplicate_entry_key() {
    let sample = sample();
    let size = sample.books.head().size;
    let failure = tampered(&sample, |leaves| {
        let last = leaves.last().unwrap().clone();
        leaves.push(last);
    });
    assert_eq!(failure, at(size, AuditFailureKind::NotProduced));
}

#[test]
fn fixture_09_a_recorded_master_data_no_op() {
    let sample = sample();
    let size = sample.books.head().size;
    // Position 1 declares EUR; the same declaration again changes nothing.
    let failure = tampered(&sample, |leaves| leaves.push(leaves[1].clone()));
    assert_eq!(failure, at(size, AuditFailureKind::NotProduced));
}

fn reseal(sample: &Sample, change: impl FnOnce(&mut SealParts)) -> Failed {
    let position = sample.seal_at as usize;
    tampered(sample, |leaves| {
        let Record::Seal(seal) = Record::<2>::decode(&leaves[position]).unwrap() else {
            panic!("not a seal");
        };
        let mut parts = seal.parts().clone();
        change(&mut parts);
        leaves[position] =
            Record::Seal(doubleentry::record::Seal::new(parts).unwrap()).to_leaf_bytes();
    })
}

#[test]
fn fixture_10_a_seal_with_a_wrong_tree_head() {
    let sample = sample();
    let failure = reseal(&sample, |parts| {
        parts.trees[0].root = doubleentry::log::Hash::from_bytes([0; 32]);
    });
    assert_eq!(
        failure,
        at(
            sample.seal_at,
            AuditFailureKind::SealMismatch {
                period: Label::new("2026-01").unwrap()
            }
        )
    );
}

#[test]
fn fixture_11_a_seal_with_wrong_totals() {
    let sample = sample();
    let failure = reseal(&sample, |parts| {
        let totals = parts.trees[0].totals;
        parts.trees[0].totals = totals.post(Side::Debit, eur("0.01")).unwrap();
    });
    assert_eq!(
        failure,
        at(
            sample.seal_at,
            AuditFailureKind::SealMismatch {
                period: Label::new("2026-01").unwrap()
            }
        )
    );
}

#[test]
fn fixture_12_an_altered_balance_row() {
    let sample = sample();
    let mut before = None;
    let failure = claimed(&sample, |claims| {
        before = Some(common::tamper_cell(
            claims,
            ProjectionPart::Balances,
            0,
            3,
            Cell::Bytes(vec![0; 16]),
        ));
    });
    let (part, expected, found) = mismatch(failure);
    assert_eq!(part, ProjectionPart::Balances);
    assert_eq!(expected, before);
    assert_ne!(found, before);
}

#[test]
fn fixture_13_an_extra_balance_row() {
    let sample = sample();
    let failure = claimed(&sample, |claims| {
        claims
            .rows_mut(ProjectionPart::Balances)
            .push(ProjectionRow::new(vec![
                Cell::Int(99),
                Cell::Text("EUR".into()),
                Cell::Int(1),
                Cell::Bytes(vec![0; 16]),
                Cell::Bytes(vec![0; 16]),
                Cell::Int(1),
            ]));
    });
    let (part, expected, found) = mismatch(failure);
    assert_eq!(part, ProjectionPart::Balances);
    assert_eq!(expected, None);
    assert_eq!(found.unwrap().cells()[0], Cell::Int(99));
}

#[test]
fn fixture_14_a_period_claimed_sealed_without_a_seal() {
    let sample = sample();
    let failure = claimed(&sample, |claims| {
        claims
            .rows_mut(ProjectionPart::Seals)
            .push(ProjectionRow::new(vec![
                Cell::Text("2026-02".into()),
                Cell::Int(99),
            ]));
    });
    assert_eq!(mismatch(failure).0, ProjectionPart::Seals);
}

#[test]
fn fixture_15_an_altered_applied_clearing_amount() {
    let sample = sample();
    let failure = claimed(&sample, |claims| {
        common::tamper_cell(
            claims,
            ProjectionPart::Cleared,
            0,
            2,
            Cell::Bytes(vec![9; 16]),
        );
    });
    assert_eq!(mismatch(failure).0, ProjectionPart::Cleared);
}

#[test]
fn fixture_16_a_swapped_handle_and_path() {
    let sample = sample();
    let failure = claimed(&sample, |claims| {
        let rows = claims.rows_mut(ProjectionPart::Accounts);
        let first = rows[0].cells()[1].clone();
        let second = rows[1].cells()[1].clone();
        common::tamper_cell(claims, ProjectionPart::Accounts, 0, 1, second);
        common::tamper_cell(claims, ProjectionPart::Accounts, 1, 1, first);
    });
    let (part, expected, _) = mismatch(failure);
    assert_eq!(part, ProjectionPart::Accounts);
    assert_eq!(expected.unwrap().cells()[0], Cell::Int(0));
}

#[test]
fn fixture_17_an_altered_memo_text() {
    let sample = sample();
    let mut replay = Replayer::<2>::new(sample.books.id());
    let mut failure = None;
    for leaf in sample.books.leaves() {
        let altered = leaf.memo.map(|memo| MemoPlain {
            salt: memo.salt,
            text: format!("{}!", memo.text),
        });
        let fed = replay.feed(ReplayLeaf {
            memo: altered.as_ref(),
            ..leaf
        });
        if let Err(error) = fed {
            failure = Some(error);
            break;
        }
    }
    assert_eq!(
        failed(failure.unwrap()),
        at(sample.memo_at, AuditFailureKind::MemoMismatch)
    );
}

#[test]
fn fixture_18_an_altered_reversal_link() {
    let sample = sample();
    let failure = claimed(&sample, |claims| {
        let row = claims
            .rows(ProjectionPart::Entries)
            .iter()
            .position(|row| row.cells()[4] != Cell::Null)
            .unwrap();
        common::tamper_cell(
            claims,
            ProjectionPart::Entries,
            row,
            4,
            Cell::Bytes(vec![0xee; 16]),
        );
    });
    assert_eq!(mismatch(failure).0, ProjectionPart::Entries);
}

#[test]
fn fixture_register_effective_other_than_open_from_is_not_produced() {
    // FR-042: the record the core would write is compared byte for byte.
    let sample = sample();
    let size = sample.books.head().size;
    let registration = AccountRegistration {
        handle: AccountHandle::new(5).unwrap(),
        path: "Assets:New".parse().unwrap(),
        kind: AccountKind::Asset,
        class: None,
        open_from: date!("2026-03-01"),
        open_to: None,
        limit: BalanceLimit::Unlimited,
    };
    let event = |effective| {
        Record::<2>::Account(AccountEvent {
            action: AccountAction::Register(registration.clone()),
            effective,
            actor: Label::new("setup").unwrap(),
        })
        .to_leaf_bytes()
    };
    let honest = event(date!("2026-03-01"));
    let mut leaves = leaf_bytes(&sample.books);
    leaves.push(honest);
    let mut replay = Replayer::<2>::new(sample.books.id());
    for (position, bytes) in leaves.iter().enumerate() {
        replay
            .feed(ReplayLeaf {
                position: position as u64,
                bytes,
                memo: None,
                fingerprint: None,
            })
            .unwrap();
    }
    let failure = tampered(&sample, |leaves| leaves.push(event(date!("2026-03-02"))));
    assert_eq!(failure, at(size, AuditFailureKind::NotProduced));
}

#[test]
fn a_leaf_fed_out_of_order_is_a_position_mismatch() {
    let sample = sample();
    let mut replay = Replayer::<2>::new(sample.books.id());
    let leaf = sample.books.leaves().nth(1).unwrap();
    assert_eq!(
        failed(replay.feed(leaf).unwrap_err()),
        at(0, AuditFailureKind::PositionMismatch { expected: 0 })
    );
}

// ── Untampered books pass (SC-003) ────────────────────────────────────────────

/// A small deterministic generator (no randomness in the tests).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

fn random_journal(seed: u64, operations: usize) -> Journal {
    let accounts = [
        "Assets:Cash",
        "Income:Sales",
        "Liabilities:Wallet",
        "Expenses:Rent",
        "Equity:Retained",
    ];
    let mut rng = Lcg(seed);
    let mut books = books();
    let mut sealed = 0u8;
    let mut recorded: Vec<(Entry, doubleentry::Recorded)> = Vec::new();
    for n in 0..operations {
        match rng.next(20) {
            0 if sealed < 11 && n > operations / 12 * usize::from(sealed + 1) => {
                sealed += 1;
                books.seal_period(format!("2026-{sealed:02}")).unwrap();
            }
            1 | 2 if !recorded.is_empty() => {
                let (draft, original) = &recorded[rng.next(recorded.len() as u64) as usize];
                let again = books.record(draft.clone()).unwrap();
                assert_eq!(again.position, original.position);
            }
            3 if !recorded.is_empty() => {
                let (_, original) = &recorded[rng.next(recorded.len() as u64) as usize];
                let on = common::month_start(sealed + 1);
                if let Ok(reversal) = books.reversal(original.id, &format!("rev-{n}"), on) {
                    let _ = books.record(reversal);
                }
            }
            _ => {
                let month = sealed + 1 + rng.next(u64::from(12 - sealed)) as u8;
                let day = common::month_start(month)
                    .replace_day(1 + rng.next(28) as u8)
                    .unwrap();
                let debit = accounts[rng.next(5) as usize];
                let credit = accounts[rng.next(5) as usize];
                let amount =
                    doubleentry::Amount::from_minor(1 + i128::from(rng.next(100_000))).unwrap();
                let mut draft = Entry::new(format!("e-{n}"), day)
                    .debit(debit, amount, EUR)
                    .credit(credit, amount, EUR);
                if rng.next(4) == 0 {
                    draft = draft.memo(format!("memo {n}"));
                }
                if let Ok(done) = books.record(draft.clone()) {
                    recorded.push((draft, done));
                }
            }
        }
    }
    books
}

#[test]
fn untampered_random_journals_pass() {
    for seed in 0..20 {
        let books = random_journal(seed, 1000);
        let report = books.audit().unwrap();
        assert_eq!(report.head, books.head());
        assert_eq!(
            audit_leaves(&books, &leaf_bytes(&books), &Claims::of(&books)).unwrap(),
            report,
            "seed {seed}"
        );
    }
}

#[test]
fn from_log_reproduces_head_state_and_next_positions() {
    let mut books = random_journal(7, 300);
    let mut restored: Journal = Journal::from_log(books.id(), books.leaves()).unwrap();
    assert_eq!(restored.head(), books.head());
    assert_eq!(restored.audit().unwrap(), books.audit().unwrap());
    let next = sale("after-restore", date!("2026-12-15"), "1.00");
    let original = books.record(next.clone()).unwrap();
    let copy = restored.record(next).unwrap();
    assert_eq!(original, copy);
    assert_eq!(restored.head(), books.head());
    // An empty log has no genesis.
    assert!(matches!(
        Journal::<2>::from_log(books.id(), std::iter::empty()),
        Err(doubleentry::Error::Integrity(_))
    ));
}
