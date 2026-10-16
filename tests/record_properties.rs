//! One byte form per record: strict decoding, round trips and the size
//! cap.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use doubleentry::{
    AccountHandle, AccountKind, Amount, Balance, BalanceLimit, CaptureTime, Currency, Dimensions,
    Document, Entry, EntryId, EntryRecord, IdempotencyKey, Label, Layer, LedgerId, LedgerPolicy,
    MemoSalt, PostAmount, PostingRef, Rate, Side,
    currency::EUR,
    error::ValueError,
    log::leaf_hash,
    record::{
        AccountAction, AccountChange, AccountEvent, AccountRegistration, ClearingAction,
        ClearingEvent, CurrencyDeclaration, DecodeError, EntryParts, Genesis, HoldAction,
        MAX_LEAF_BYTES, MemoCommitment, PeriodDefinition, PolicyEvent, PostingRecord, Record, Seal,
        SealParts, TreeHeadEntry,
    },
};
use proptest::prelude::*;
use time::Date;
use time::macros::date;

// ── the golden leaves and targeted refusals ───────────────────────────────────

fn golden_leaves() -> BTreeMap<String, Vec<u8>> {
    include_str!("vectors/golden.txt")
        .lines()
        .filter_map(|line| line.split_once(".leaf = "))
        .map(|(name, hex)| {
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            (name.to_owned(), bytes)
        })
        .collect()
}

fn leaf(name: &str) -> Vec<u8> {
    golden_leaves().remove(name).unwrap()
}

fn text(s: &str) -> Vec<u8> {
    let mut out = (s.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(s.as_bytes());
    out
}

/// `bytes` with the first occurrence of `from` replaced by `to`.
fn replace(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let at = bytes
        .windows(from.len())
        .position(|window| window == from)
        .expect("pattern present");
    [&bytes[..at], to, &bytes[at + from.len()..]].concat()
}

fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
    Record::<2>::decode(bytes)
}

#[test]
fn every_golden_leaf_refuses_structural_damage() {
    for (name, bytes) in golden_leaves() {
        for kind in [0x00, 0x09, 0x80, 0x81, 0xff] {
            let mut flipped = bytes.clone();
            flipped[0] = kind;
            assert_eq!(
                decode(&flipped),
                Err(DecodeError::UnknownKind(kind)),
                "{name}"
            );
        }
        let mut appended = bytes.clone();
        appended.push(0);
        assert_eq!(
            decode(&appended),
            Err(DecodeError::TrailingBytes { count: 1 }),
            "{name}"
        );
        let truncated = &bytes[..bytes.len() - 1];
        assert!(
            matches!(decode(truncated), Err(DecodeError::Truncated { .. })),
            "{name}"
        );
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_err(), "{name} cut at {cut}");
        }
    }
}

#[test]
fn unknown_tags_are_refused() {
    let tag = |field, tag| Err(DecodeError::InvalidTag { field, tag });
    let mut account = leaf("account_register");
    account[1] = 2;
    assert_eq!(decode(&account), tag("account action", 2));
    let mut field = leaf("account_change_limit");
    field[1 + 1 + 4] = 3;
    assert_eq!(decode(&field), tag("account field", 3));
    let mut limit = leaf("account_change_limit");
    limit[1 + 1 + 4 + 1] = 3;
    assert_eq!(decode(&limit), tag("balance limit", 3));
    let mut clearing = leaf("clearing_reset");
    clearing[1] = 2;
    assert_eq!(decode(&clearing), tag("clearing action", 2));

    // The hold tag of hold_void sits after key, dates and six empty fields.
    let void = leaf("hold_void");
    let hold_at = 1 + text("hold-2026-0002-void").len() + 4 + 4 + 3 + 8 + 8 + 1 + 1;
    assert_eq!(void[hold_at], 2, "the Void tag");
    let mut bad_hold = void.clone();
    bad_hold[hold_at] = 4;
    assert_eq!(decode(&bad_hold), tag("hold", 4));
    let mut bad_option = void;
    bad_option[hold_at - 1] = 2;
    assert_eq!(decode(&bad_option), tag("hold", 2));

    // hold_post ends with the post-amount tag (Full = 0) and an empty rates seq.
    let mut bad_amount = leaf("hold_post");
    let amount_at = bad_amount.len() - 8 - 1;
    assert_eq!(bad_amount[amount_at], 0, "the Full tag");
    bad_amount[amount_at] = 2;
    assert_eq!(decode(&bad_amount), tag("post amount", 2));

    let minimal = leaf("entry_minimal");
    // The first posting: handle 0, side Debit (0), amount 100.
    let posting = |side: u8| [&0u32.to_le_bytes()[..], &[side], &100i128.to_le_bytes()].concat();
    let bad_side = replace(&minimal, &posting(0), &posting(7));
    assert_eq!(decode(&bad_side), tag("side", 7));
}

#[test]
fn non_canonical_values_are_refused() {
    let full = leaf("entry_full");
    // Dimensions out of order.
    let sorted = [
        text("cost-center"),
        text("100"),
        text("project"),
        text("alpha"),
    ]
    .concat();
    let unsorted = [
        text("project"),
        text("alpha"),
        text("cost-center"),
        text("100"),
    ]
    .concat();
    assert_eq!(
        decode(&replace(&full, &sorted, &unsorted)),
        Err(DecodeError::NonCanonical {
            field: "dimensions"
        })
    );
    // A duplicated rate pair.
    let second = [text("USD"), text("CHF")].concat();
    let duplicate = [text("EUR"), text("USD")].concat();
    assert_eq!(
        decode(&replace(&full, &second, &duplicate)),
        Err(DecodeError::NonCanonical { field: "rates" })
    );
    // A non-normalised rate: 108730 × 10⁻⁵ instead of 10873 × 10⁻⁴.
    let normal = [&10873i128.to_le_bytes()[..], &(-4i8).to_le_bytes()].concat();
    let padded = [&108_730i128.to_le_bytes()[..], &(-5i8).to_le_bytes()].concat();
    assert_eq!(
        decode(&replace(&full, &normal, &padded)),
        Err(DecodeError::NonCanonical { field: "rate" })
    );
    // An NFD label.
    let nfd_period = [
        &[5u8][..],
        &text("Cafe\u{301}"),
        &date!(2026 - 03 - 01).to_julian_day().to_le_bytes(),
        &date!(2026 - 03 - 31).to_julian_day().to_le_bytes(),
    ]
    .concat();
    assert_eq!(
        decode(&nfd_period),
        Err(DecodeError::NotNfc { field: "period id" })
    );
    // A balance with no postings but a debit.
    let seal = leaf("seal");
    let usd_totals = [
        &5000i128.to_le_bytes()[..],
        &0i128.to_le_bytes(),
        &1u64.to_le_bytes(),
    ]
    .concat();
    let broken = [
        &5000i128.to_le_bytes()[..],
        &0i128.to_le_bytes(),
        &0u64.to_le_bytes(),
    ]
    .concat();
    assert_eq!(
        decode(&replace(&seal, &usd_totals, &broken)),
        Err(DecodeError::Value {
            field: "tree totals",
            error: ValueError::OutOfRange
        })
    );
    // Trees out of order: swap the two tree heads.
    let parsed = match decode(&seal).unwrap() {
        Record::Seal(seal) => seal,
        _ => unreachable!(),
    };
    let heads: Vec<Vec<u8>> = parsed.parts().trees.iter().map(tree_head_bytes).collect();
    let swapped = replace(
        &seal,
        &heads.concat(),
        &[heads[1].clone(), heads[0].clone()].concat(),
    );
    assert_eq!(
        decode(&swapped),
        Err(DecodeError::NonCanonical { field: "trees" })
    );
}

fn tree_head_bytes(tree: &TreeHeadEntry) -> Vec<u8> {
    [
        text(tree.currency.as_str()),
        vec![u8::from(tree.layer == Layer::Pending)],
        tree.root.as_bytes().to_vec(),
        tree.totals.debit().to_minor().to_le_bytes().to_vec(),
        tree.totals.credit().to_minor().to_le_bytes().to_vec(),
        tree.totals.postings().to_le_bytes().to_vec(),
    ]
    .concat()
}

#[test]
fn scale_format_and_count_violations_are_refused() {
    assert_eq!(
        Record::<6>::decode(&leaf("genesis")),
        Err(DecodeError::PrecisionMismatch {
            expected: 6,
            found: 2
        })
    );
    let other_format = replace(
        &leaf("genesis"),
        &text("doubleentry/1"),
        &text("doubleentry/2"),
    );
    assert_eq!(decode(&other_format), Err(DecodeError::UnknownFormat));
    let mut currency = leaf("currency");
    currency[1 + text("EUR").len()] = 3;
    assert_eq!(
        decode(&currency),
        Err(DecodeError::Value {
            field: "exponent",
            error: ValueError::ExponentAboveScale {
                exponent: 3,
                scale: 2
            }
        })
    );
    // The same declaration is valid in a ledger of scale 3.
    assert!(matches!(
        Record::<3>::decode(&currency),
        Ok(Record::Currency(_))
    ));

    // The documents count of the minimal entry set to u64::MAX.
    let minimal = leaf("entry_minimal");
    let documents_at = 1 + text("minimal-1").len() + 4 + 4 + 1 + 1 + 1;
    let mut huge = minimal.clone();
    huge[documents_at..documents_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        decode(&huge),
        Err(DecodeError::TooMany {
            field: "documents",
            max: 16,
            found: u64::MAX
        })
    );
    let oversized = vec![6u8; MAX_LEAF_BYTES + 1];
    assert_eq!(
        decode(&oversized),
        Err(DecodeError::TooLarge {
            max: MAX_LEAF_BYTES,
            found: MAX_LEAF_BYTES + 1
        })
    );
}

// ── generated records round-trip ──────────────────────────────────────────────

fn label() -> impl Strategy<Value = Label> {
    "[a-zA-Z0-9][a-zA-Z0-9 é-]{0,6}[a-z0-9]".prop_map(|s| Label::new(s).unwrap())
}

fn path() -> impl Strategy<Value = doubleentry::AccountPath> {
    prop::collection::vec("[A-Z][a-z]{0,6}", 1..4)
        .prop_map(|segments| doubleentry::AccountPath::from_segments(segments).unwrap())
}

fn day() -> impl Strategy<Value = Date> {
    (2_451_545i32..2_488_070).prop_map(|jd| Date::from_julian_day(jd).unwrap())
}

fn currency() -> impl Strategy<Value = Currency> {
    "[A-Z0-9]{1,5}".prop_map(|s| Currency::new(&s).unwrap())
}

fn amount() -> impl Strategy<Value = Amount> {
    (-i128::MAX..=i128::MAX).prop_map(|minor| Amount::from_minor(minor).unwrap())
}

fn key() -> impl Strategy<Value = IdempotencyKey> {
    "[ -~]{1,20}".prop_map(|s| IdempotencyKey::new(s).unwrap())
}

fn entry_id() -> impl Strategy<Value = EntryId> {
    any::<[u8; 16]>().prop_map(EntryId::from_bytes)
}

fn handle() -> impl Strategy<Value = AccountHandle> {
    (0..=AccountHandle::MAX).prop_map(|h| AccountHandle::new(h).unwrap())
}

fn balance() -> impl Strategy<Value = Balance> {
    prop_oneof![
        Just(Balance::ZERO),
        (0..=i128::MAX, 0..=i128::MAX, 1..=u64::MAX).prop_map(|(d, c, n)| {
            Balance::from_parts(
                Amount::from_minor(d).unwrap(),
                Amount::from_minor(c).unwrap(),
                n,
            )
            .unwrap_or(Balance::ZERO)
        }),
    ]
}

fn dimensions() -> impl Strategy<Value = Dimensions> {
    prop::collection::btree_map(label(), label(), 0..=3).prop_map(|map| {
        map.into_iter().fold(Dimensions::new(), |d, (axis, value)| {
            d.with(axis, value).unwrap()
        })
    })
}

fn rate() -> impl Strategy<Value = Rate> {
    (currency(), currency(), 1i128..1_000_000_000, -20i8..20)
        .prop_filter_map("same currency", |(b, q, m, e)| Rate::new(b, q, m, e).ok())
}

fn rates() -> impl Strategy<Value = Vec<Rate>> {
    prop::collection::vec(rate(), 0..4).prop_map(|rates| {
        let by_pair: BTreeMap<_, _> = rates
            .into_iter()
            .map(|r| ((r.base(), r.quote()), r))
            .collect();
        by_pair.into_values().collect()
    })
}

fn document() -> impl Strategy<Value = Document> {
    (
        label(),
        "[ -~]{1,30}",
        prop::option::of(day()),
        prop::option::of(any::<[u8; 32]>()),
    )
        .prop_map(|(doc_type, reference, date, digest)| {
            let mut document = Document::new(doc_type, reference).unwrap();
            if let Some(date) = date {
                document = document.dated(date);
            }
            if let Some(digest) = digest {
                document = document.with_sha256(digest);
            }
            document
        })
}

fn hold() -> impl Strategy<Value = HoldAction> {
    prop_oneof![
        prop::option::of(day()).prop_map(|expires| HoldAction::Open { expires }),
        (entry_id(), prop::option::of(amount())).prop_map(|(hold, partial)| HoldAction::Post {
            hold,
            amount: partial.map_or(PostAmount::Full, PostAmount::Partial),
        }),
        entry_id().prop_map(|hold| HoldAction::Void { hold }),
        entry_id().prop_map(|hold| HoldAction::Expire { hold }),
    ]
}

fn layer() -> impl Strategy<Value = Layer> {
    prop_oneof![Just(Layer::Settled), Just(Layer::Pending)]
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Debit), Just(Side::Credit)]
}

fn posting() -> impl Strategy<Value = PostingRecord> {
    (
        handle(),
        side(),
        amount(),
        currency(),
        layer(),
        dimensions(),
    )
        .prop_map(
            |(handle, side, amount, currency, layer, dimensions)| PostingRecord {
                handle,
                side,
                amount,
                currency,
                layer,
                dimensions,
            },
        )
}

fn capture() -> impl Strategy<Value = CaptureTime> {
    (-62_135_596_800i64..=253_402_300_799, -1080i16..=1080)
        .prop_map(|(s, o)| CaptureTime::new(s, o).unwrap())
}

fn entry_record() -> impl Strategy<Value = Record> {
    (
        (
            key(),
            day(),
            day(),
            prop::option::of(capture()),
            prop::option::of(label()),
        ),
        prop::option::of(any::<[u8; 32]>().prop_map(MemoCommitment::from_bytes)),
        prop::collection::vec(document(), 0..3),
        prop::collection::vec(posting(), 0..5),
        (
            prop::option::of(entry_id()),
            prop::option::of(hold()),
            rates(),
        ),
    )
        .prop_map(
            |(
                (key, booking_date, value_date, captured_at, kind),
                memo,
                documents,
                postings,
                (reverses, hold, rates),
            )| {
                Record::Entry(
                    EntryRecord::new(EntryParts {
                        key,
                        booking_date,
                        value_date,
                        captured_at,
                        kind,
                        memo,
                        documents,
                        postings,
                        reverses,
                        hold,
                        rates,
                    })
                    .unwrap(),
                )
            },
        )
}

fn limit() -> impl Strategy<Value = BalanceLimit> {
    prop_oneof![
        Just(BalanceLimit::Unlimited),
        Just(BalanceLimit::NoCreditBalance),
        Just(BalanceLimit::NoDebitBalance)
    ]
}

fn account_kind() -> impl Strategy<Value = AccountKind> {
    prop_oneof![
        Just(AccountKind::Asset),
        Just(AccountKind::Liability),
        Just(AccountKind::Equity),
        Just(AccountKind::Income),
        Just(AccountKind::Expense)
    ]
}

fn account_record() -> impl Strategy<Value = Record> {
    let register = (
        handle(),
        path(),
        account_kind(),
        prop::option::of(label()),
        day(),
        prop::option::of(day()),
        limit(),
    )
        .prop_map(|(handle, path, kind, class, open_from, open_to, limit)| {
            AccountAction::Register(AccountRegistration {
                handle,
                path,
                kind,
                class,
                open_from,
                open_to,
                limit,
            })
        });
    let change = (
        handle(),
        prop_oneof![
            prop::option::of(day()).prop_map(AccountChange::OpenTo),
            limit().prop_map(AccountChange::Limit),
            prop::option::of(label()).prop_map(AccountChange::Class),
        ],
    )
        .prop_map(|(handle, change)| AccountAction::Change { handle, change });
    (prop_oneof![register, change], day(), label()).prop_map(|(action, effective, actor)| {
        Record::Account(AccountEvent {
            action,
            effective,
            actor,
        })
    })
}

fn seal_record() -> impl Strategy<Value = Record> {
    (
        label(),
        day(),
        0u32..400,
        any::<u64>(),
        any::<u32>(),
        prop::collection::btree_map((currency(), layer()), (any::<[u8; 32]>(), balance()), 0..4),
    )
        .prop_map(|(period, start, days, through, accounts, trees)| {
            Record::Seal(
                Seal::new(SealParts {
                    period,
                    start,
                    end: start
                        .checked_add(time::Duration::days(i64::from(days)))
                        .unwrap(),
                    through,
                    accounts,
                    trees: trees
                        .into_iter()
                        .map(|((currency, layer), (root, totals))| TreeHeadEntry {
                            currency,
                            layer,
                            root: doubleentry::log::Hash::from_bytes(root),
                            totals,
                        })
                        .collect(),
                })
                .unwrap(),
            )
        })
}

fn record() -> impl Strategy<Value = Record> {
    prop_oneof![
        "[a-z.]{1,20}".prop_map(|s| Record::Genesis(Genesis {
            ledger: LedgerId::new(s).unwrap()
        })),
        account_record(),
        (currency(), 0u8..=2, day(), label()).prop_map(|(code, exponent, effective, actor)| {
            Record::Currency(CurrencyDeclaration {
                code,
                exponent,
                effective,
                actor,
            })
        }),
        (
            prop::collection::btree_set(label(), 0..4),
            prop::option::of(any::<u32>()),
            day(),
            label()
        )
            .prop_map(
                |(required, drift, effective, actor)| Record::Policy(PolicyEvent {
                    policy: LedgerPolicy::new(required, drift).unwrap(),
                    effective,
                    actor,
                })
            ),
        (label(), day(), 0u32..400).prop_map(|(id, start, days)| {
            let end = start
                .checked_add(time::Duration::days(i64::from(days)))
                .unwrap();
            Record::Period(PeriodDefinition::new(id, start, end).unwrap())
        }),
        entry_record(),
        (
            key(),
            entry_id(),
            any::<u16>(),
            entry_id(),
            any::<u16>(),
            amount(),
            any::<u64>(),
            any::<bool>(),
            day()
        )
            .prop_map(|(key, de, di, ce, ci, amount, clearing, reset, date)| {
                let action = if reset {
                    ClearingAction::Reset { key, clearing }
                } else {
                    ClearingAction::Apply {
                        key,
                        debit: PostingRef::at(de, di),
                        credit: PostingRef::at(ce, ci),
                        amount,
                    }
                };
                Record::Clearing(ClearingEvent { action, date })
            }),
        seal_record(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn records_round_trip(record in record()) {
        let bytes = record.to_leaf_bytes();
        prop_assert!(bytes.len() <= MAX_LEAF_BYTES);
        prop_assert_eq!(decode(&bytes), Ok(record.clone()));
        prop_assert_eq!(record.leaf_hash(), leaf_hash(&bytes));
    }

    #[test]
    fn accepted_mutations_re_encode_to_the_same_bytes(
        name_index in 0usize..17,
        position in any::<prop::sample::Index>(),
        value in any::<u8>(),
        mode in 0u8..3,
    ) {
        let leaves: Vec<Vec<u8>> = golden_leaves().into_values().collect();
        let mut bytes = leaves[name_index % leaves.len()].clone();
        let at = position.index(bytes.len());
        match mode {
            0 => bytes[at] = value,
            1 => bytes.insert(at, value),
            _ => { bytes.remove(at); }
        }
        if let Ok(record) = decode(&bytes) {
            prop_assert_eq!(record.to_leaf_bytes(), bytes);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100_000))]

    #[test]
    fn random_bytes_never_panic(kind in 0u8..=9, body in prop::collection::vec(any::<u8>(), 0..512)) {
        let bytes = [&[kind][..], &body].concat();
        if let Ok(record) = decode(&bytes) {
            prop_assert_eq!(record.to_leaf_bytes(), bytes);
        }
    }
}

// ── the size cap (SC-010) ─────────────────────────────────────────────────────

fn plain_posting(index: u32) -> PostingRecord {
    PostingRecord {
        handle: AccountHandle::new(index).unwrap(),
        side: if index.is_multiple_of(2) {
            Side::Debit
        } else {
            Side::Credit
        },
        amount: Amount::from_minor(100).unwrap(),
        currency: EUR,
        layer: Layer::Settled,
        dimensions: Dimensions::new(),
    }
}

fn entry_parts(postings: Vec<PostingRecord>) -> EntryParts {
    EntryParts {
        key: IdempotencyKey::new("big").unwrap(),
        booking_date: date!(2026 - 03 - 15),
        value_date: date!(2026 - 03 - 15),
        captured_at: None,
        kind: None,
        memo: None,
        documents: vec![],
        postings,
        reverses: None,
        hold: None,
        rates: vec![],
    }
}

/// 1 024 postings, padded with one dimension each (18–81 bytes) to exactly
/// `target` leaf bytes.
fn padded_entry(target: usize) -> EntryParts {
    let mut parts = entry_parts((0..1024).map(plain_posting).collect());
    let base = Record::Entry(EntryRecord::new(parts.clone()).unwrap())
        .to_leaf_bytes()
        .len();
    let mut extra = target - base;
    for posting in &mut parts.postings {
        if extra == 0 {
            break;
        }
        // One dimension adds 16 + len(axis) + len(value) bytes; keep the rest 0 or ≥ 18.
        let add = if extra == 81 || extra >= 81 + 18 {
            81
        } else if extra > 81 {
            extra - 18
        } else {
            extra
        };
        let value = Label::new("v".repeat(add - 17)).unwrap();
        posting.dimensions = Dimensions::new()
            .with(Label::new("a").unwrap(), value)
            .unwrap();
        extra -= add;
    }
    assert_eq!(extra, 0);
    parts
}

#[test]
fn a_full_entry_fits_and_one_byte_more_is_refused() {
    let plain = Record::<2>::Entry(
        EntryRecord::new(entry_parts((0..1024).map(plain_posting).collect())).unwrap(),
    );
    let bytes = plain.to_leaf_bytes();
    assert_eq!(
        bytes.len(),
        1 + 8 + 3 + 4 + 4 + 3 + 8 + 8 + 1024 * 41 + 2 + 8
    );
    assert_eq!(decode(&bytes), Ok(plain));

    let at_cap = Record::<2>::Entry(EntryRecord::new(padded_entry(MAX_LEAF_BYTES)).unwrap());
    let bytes = at_cap.to_leaf_bytes();
    assert_eq!(bytes.len(), MAX_LEAF_BYTES);
    assert_eq!(decode(&bytes), Ok(at_cap));
    assert_eq!(
        EntryRecord::new(padded_entry(MAX_LEAF_BYTES + 1)),
        Err(ValueError::TooLarge {
            max: MAX_LEAF_BYTES,
            found: MAX_LEAF_BYTES + 1
        })
    );
    let mut too_many = entry_parts((0..1025).map(|i| plain_posting(i % 7)).collect());
    assert_eq!(
        EntryRecord::new(too_many.clone()),
        Err(ValueError::TooLong {
            max: 1024,
            found: 1025
        })
    );
    too_many.postings.truncate(1024);
    too_many.rates = vec![
        Rate::parse(EUR, doubleentry::currency::USD, "1.1").unwrap(),
        Rate::parse(EUR, doubleentry::currency::USD, "1.2").unwrap(),
    ];
    assert_eq!(EntryRecord::new(too_many), Err(ValueError::Duplicate));
}

/// A seal padded to exactly `target` leaf bytes with tree heads whose
/// currency codes are shortened as needed.
fn padded_seal(target: usize) -> SealParts {
    let tree = |code: String| TreeHeadEntry {
        currency: Currency::new(&code).unwrap(),
        layer: Layer::Settled,
        root: doubleentry::log::Hash::from_bytes([1; 32]),
        totals: Balance::ZERO,
    };
    let base = 1 + text("p").len() + 4 + 4 + 8 + 4 + 8;
    // Each tree head is 81 + len(code) bytes; start one too many at 16 chars.
    let count = (target - base) / 97 + 1;
    let mut excess = base + count * 97 - target;
    let mut codes: Vec<String> = Vec::new();
    for index in 0..count {
        let shrink = excess.min(12);
        excess -= shrink;
        codes.push(format!("C{index:0width$}", width = 15 - shrink));
    }
    assert_eq!(excess, 0);
    codes.sort();
    SealParts {
        period: Label::new("p").unwrap(),
        start: date!(2026 - 03 - 01),
        end: date!(2026 - 03 - 31),
        through: 1,
        accounts: 1,
        trees: codes.into_iter().map(tree).collect(),
    }
}

#[test]
fn a_full_seal_fits_and_one_byte_more_is_refused() {
    let at_cap = Record::<2>::Seal(Seal::new(padded_seal(MAX_LEAF_BYTES)).unwrap());
    let bytes = at_cap.to_leaf_bytes();
    assert_eq!(bytes.len(), MAX_LEAF_BYTES);
    assert_eq!(decode(&bytes), Ok(at_cap));
    assert_eq!(
        Seal::new(padded_seal(MAX_LEAF_BYTES + 1)),
        Err(ValueError::TooLarge {
            max: MAX_LEAF_BYTES,
            found: MAX_LEAF_BYTES + 1
        })
    );
}

// ── Entry::encoded_len equals the recorded leaf (SC-010) ─────────────────────

fn draft() -> impl Strategy<Value = (Entry, Vec<doubleentry::AccountPath>)> {
    (
        (
            key(),
            day(),
            prop::option::of(day()),
            prop::option::of(capture()),
            prop::option::of(label()),
        ),
        prop::option::of(("[ -~]{1,40}", any::<[u8; 16]>())),
        prop::collection::vec(document(), 0..3),
        prop::collection::vec(
            (
                prop_oneof![path().prop_map(Ok), handle().prop_map(Err)],
                side(),
                amount(),
                currency(),
                dimensions(),
            ),
            0..6,
        ),
        (prop::option::of(entry_id()), rates()),
    )
        .prop_map(
            |(
                (key, booking, value, captured, kind),
                memo,
                documents,
                postings,
                (reverses, rates),
            )| {
                let mut entry: Entry = Entry::new(key, booking);
                if let Some(value) = value {
                    entry = entry.value_date(value);
                }
                if let Some(captured) = captured {
                    entry = entry.captured_at(captured);
                }
                if let Some(kind) = kind {
                    entry = entry.kind(kind);
                }
                if let Some((text, salt)) = memo {
                    entry = entry.memo_salted(text, MemoSalt(salt));
                }
                for document in documents {
                    entry = entry.document(document);
                }
                let mut paths = Vec::new();
                for (account, side, amount, currency, dimensions) in postings {
                    let posting = match account {
                        Ok(path) => {
                            paths.push(path.clone());
                            match side {
                                Side::Debit => doubleentry::Posting::debit(path, amount, currency),
                                Side::Credit => {
                                    doubleentry::Posting::credit(path, amount, currency)
                                }
                            }
                        }
                        Err(handle) => match side {
                            Side::Debit => doubleentry::Posting::debit(handle, amount, currency),
                            Side::Credit => doubleentry::Posting::credit(handle, amount, currency),
                        },
                    };
                    entry = entry.posting(posting.dimensions(dimensions));
                }
                if let Some(reverses) = reverses {
                    entry = entry.reverses(reverses);
                }
                for rate in rates {
                    entry = entry.rate(rate);
                }
                (entry, paths)
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn encoded_len_is_the_recorded_leaf_length((draft, _paths) in draft()) {
        prop_assert!(draft.build_errors().is_empty());
        // Resolve every account to a handle, as the rule core will.
        let parts = EntryParts {
            key: draft.key().unwrap().clone(),
            booking_date: draft.booking_date(),
            value_date: draft.get_value_date(),
            captured_at: draft.get_captured_at(),
            kind: draft.get_kind().cloned(),
            memo: draft
                .memo_text()
                .zip(draft.memo_salt())
                .map(|(text, salt)| MemoCommitment::compute(salt, text)),
            documents: draft.documents().to_vec(),
            postings: draft
                .postings()
                .iter()
                .enumerate()
                .map(|(index, posting)| PostingRecord {
                    handle: AccountHandle::new(index as u32).unwrap(),
                    side: posting.side(),
                    amount: posting.amount(),
                    currency: posting.currency(),
                    layer: posting.get_layer(),
                    dimensions: posting.get_dimensions().clone(),
                })
                .collect(),
            reverses: draft.get_reverses(),
            hold: draft.get_hold().copied(),
            rates: draft.rates().to_vec(),
        };
        let record: Record = Record::Entry(EntryRecord::new(parts).unwrap());
        prop_assert_eq!(draft.encoded_len(), record.to_leaf_bytes().len());
    }
}
