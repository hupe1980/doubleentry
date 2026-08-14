//! Golden vectors for the canonical encoding and the Merkle log.
//!
//! Hashes are only stable if the bytes behind them are. Nothing in the compiler
//! stops a field being reordered, a length prefix being dropped, or a domain tag
//! being edited — and every one of those silently changes every hash in every
//! ledger ever written by this crate, while leaving the test suite green.
//!
//! These vectors are the tripwire. A change here is either a mistake, or a
//! deliberate format revision.
//!
//! Before the first release, a deliberate revision means regenerating these
//! values — `just golden-emit` prints them — because there are no ledgers in the
//! world to be compatible with. After it, it means bumping the encoding version
//! in the domain tag (`doubleentry/entry/v1`) as well, so old bytes are never
//! silently reinterpreted under a new format, and writing down how existing
//! seals migrate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use doubleentry::account::AccountRegistry;
use doubleentry::balance::TrialBalance;
use doubleentry::canonical::Canonical;
use doubleentry::entry::{Draft, LedgerPolicy, SealContext};
use doubleentry::merkle::TreeHead;
use doubleentry::merkle::{MerkleLog, empty_root, leaf_hash};
use doubleentry::period::{LedgerId, PeriodCalendar, PeriodId};
use doubleentry::seal::{PeriodCoverage, Seal};
use doubleentry::{
    Amount, Balanced, Currency, Description, Dimensions, DocumentRef, Entry, EntryId, Hash,
    IdempotencyKey, Label, Posting, Provenance,
};
use time::macros::date;
use uuid::Uuid;

type Eur = Amount<2>;

fn label(s: &str) -> Label {
    Label::new(s).expect("valid label")
}

/// A fixed leaf payload, derived without a clock or a random source.
fn leaf(i: u64) -> Hash {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&i.to_le_bytes());
    Hash::from_bytes(bytes)
}

/// The reference entry.
///
/// Exercises every field that enters the canonical encoding: both directions,
/// dimensions, provenance, a document reference, a reversal link, and a value
/// date that differs from the booking date.
fn reference_entry() -> Entry<Balanced, 2> {
    let mut accounts = AccountRegistry::new();
    let cash = accounts
        .register_path("Assets:Cash", date!(2020 - 01 - 01))
        .expect("registers");
    let revenue = accounts
        .register_path("Income:Sales", date!(2020 - 01 - 01))
        .expect("registers");

    let calendar = PeriodCalendar::new();
    let policy = LedgerPolicy::default();
    let ctx = SealContext {
        accounts: &accounts,
        calendar: &calendar,
        policy: &policy,
    };

    let dimensions = Dimensions::none()
        .with(label("activity"), label("Network"))
        .expect("fits")
        .with(label("segment"), label("Electricity"))
        .expect("fits");

    // A fixed identifier: excluded from the encoding, but pinned so the vector
    // is reproducible end to end.
    let id = EntryId::from_uuid(Uuid::from_bytes([0x11; 16]));
    let original = EntryId::from_uuid(Uuid::from_bytes([0x22; 16]));

    Entry::<Draft, 2>::new(
        id,
        IdempotencyKey::new(b"golden-vector-key".to_vec()).expect("valid"),
        date!(2026 - 03 - 15),
    )
    .with_value_date(date!(2026 - 03 - 17))
    .with_description(Description::new("reference entry").expect("valid"))
    .with_provenance(
        Provenance::none()
            .with_actor("auditor")
            .expect("valid")
            .with_source("golden")
            .expect("valid"),
    )
    .with_document(DocumentRef::new("INV-2026-0001", Hash::from_bytes([0x33; 32])).expect("valid"))
    .reversing(original, date!(2026 - 02 - 01))
    .post(Posting::debit(cash, Eur::from_minor(119_000), Currency::EUR).with_dimensions(dimensions))
    .post(Posting::credit(
        revenue,
        Eur::from_minor(119_000),
        Currency::EUR,
    ))
    .seal(&ctx)
    .expect("balances")
}

#[test]
fn canonical_encoding_of_the_reference_entry_is_unchanged() {
    let encoded = reference_entry().to_canonical_bytes();
    let hex: String = encoded.iter().map(|b| format!("{b:02x}")).collect();

    let expected = concat!(
        "0211000000676f6c64656e2d766563746f722d6b6579ea0700030fea070003110f0000",
        "007265666572656e636520656e747279020000000000000000d8d00100000000004555",
        "520002000000080000006163746976697479070000004e6574776f726b070000007365",
        "676d656e740b000000456c6563747269636974790100000001d8d00100000000004555",
        "52000000000000010700000061756469746f720106000000676f6c64656e00010d0000",
        "00494e562d323032362d30303031013333333333333333333333333333333333333333",
        "333333333333333333333333012222222222222222222222222222222201ea07000201",
    );
    assert_eq!(
        hex, expected,
        "the canonical encoding of an entry changed; see this file's module docs"
    );
}

#[test]
fn the_reference_entry_content_hash_is_unchanged() {
    assert_eq!(
        reference_entry().content_hash().to_hex(),
        "0237cc80981925555b91e7553e5af80319dfb1bfb872472b7f1ec3caedaf34a3",
        "the entry content hash changed; see this file's module docs"
    );
}

/// The reference seal.
///
/// Built from fixed inputs rather than from a journal, so the vector pins the
/// seal preimage itself and not the arithmetic that happened to produce it.
fn reference_seal() -> Seal {
    let mut tb = TrialBalance::<2>::new();
    let mut accounts = AccountRegistry::new();
    let cash = accounts
        .register_path("Assets:Cash", date!(2020 - 01 - 01))
        .expect("registers");
    let revenue = accounts
        .register_path("Income:Sales", date!(2020 - 01 - 01))
        .expect("registers");
    tb.apply(&Posting::debit(
        cash,
        Eur::from_minor(119_000),
        Currency::EUR,
    ))
    .expect("fits");
    tb.apply(&Posting::credit(
        revenue,
        Eur::from_minor(119_000),
        Currency::EUR,
    ))
    .expect("fits");

    Seal::build::<2>(
        LedgerId::new("golden-ledger").expect("valid"),
        PeriodId::new("2026-03").expect("valid"),
        PeriodCoverage::spanning(0, 3, 4),
        TreeHead {
            size: 4,
            root: Hash::from_bytes([0x44; 32]),
        },
        &tb,
        Some(Hash::from_bytes([0x55; 32])),
    )
}

#[test]
fn the_reference_seal_hash_is_unchanged() {
    // A seal hash is what an auditor archives and what later verification is
    // checked against. Changing its preimage invalidates every seal ever
    // issued, so it is pinned separately from the entry encoding.
    assert_eq!(
        reference_seal().seal_hash.to_hex(),
        "98cde30eeb7387be890a575fa8e946e67f8ae3b7c0c3be9cb323d5bc9eac5714",
        "the seal hash changed; see this file's module docs"
    );
}

#[test]
fn a_document_reference_without_a_hash_encodes_differently() {
    // The presence of the hash is part of the preimage, so an entry citing a
    // document it cannot vouch for is not interchangeable with one that can.
    let hashed = DocumentRef::new("INV-1", Hash::from_bytes([0x33; 32])).expect("valid");
    let unverified = DocumentRef::unverified("INV-1").expect("valid");
    assert!(hashed.is_verifiable());
    assert!(!unverified.is_verifiable());
    assert_ne!(
        hashed.to_canonical_bytes(),
        unverified.to_canonical_bytes(),
        "an unhashed document reference must not encode as a hashed one"
    );
}

#[test]
fn merkle_constants_are_unchanged() {
    assert_eq!(
        empty_root().to_hex(),
        "854ee3641f62b1063d0eee1b9a4f6f872b38625e3f4a090dd5a9e5c58d04ebf9"
    );
    assert_eq!(
        leaf_hash(&leaf(0)).to_hex(),
        "db5c7a5ad0a6c6654868caba0dafda6a794f4278cb5c4573728dc645d31dd632"
    );
}

#[test]
fn merkle_roots_for_known_sizes_are_unchanged() {
    let expected = [
        "db5c7a5ad0a6c6654868caba0dafda6a794f4278cb5c4573728dc645d31dd632",
        "b8581ec29a5787add8ade15bdbb4f3e2412222c6600b1195f659dd82d43d25d4",
        "4846c0dda0fe1aa609475f8287ac34a09d3f5e8d3a2812d275359f6977989d12",
        "c8fdd4a2a60ada5bad1bd77a1a1407370e53db996cd109c3b578f21ee0040ffc",
        "6bc34b2a6f4e608393c18d44b21493be394757cde46cb361caae9544fc95804b",
        "c01bef11185172ec477ffab55f45d97ef5a789bcfbb6091792556fb5de13add0",
    ];
    let sizes = [1u64, 2, 3, 4, 5, 8];
    for (size, want) in sizes.iter().zip(expected.iter()) {
        let log = MerkleLog::from_leaves((0..*size).map(leaf).collect());
        assert_eq!(log.root().to_hex(), *want, "root changed for size {size}");
    }
}

#[test]
fn money_formatting_is_unchanged() {
    // The decimal string is what gets serialised and what a scale-2 amount
    // hashes as; a change here changes wire compatibility.
    assert_eq!(Eur::from_minor(119_000).to_string(), "1190.00");
    assert_eq!(Eur::from_minor(-5).to_string(), "-0.05");
    assert_eq!(Eur::from_minor(0).to_string(), "0.00");
    assert_eq!(Amount::<0>::from_minor(7).to_string(), "7");
    assert_eq!(Amount::<5>::from_minor(123).to_string(), "0.00123");
}

#[test]
#[ignore = "prints current values for regenerating the vectors above"]
fn emit_vectors() {
    let e = reference_entry();
    let hex: String = e
        .to_canonical_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!("ENCODING={hex}");
    println!("ENTRY_HASH={}", e.content_hash().to_hex());
    println!("SEAL_HASH={}", reference_seal().seal_hash.to_hex());
    println!("EMPTY_ROOT={}", empty_root().to_hex());
    println!("LEAF_ZERO={}", leaf_hash(&leaf(0)).to_hex());
    for size in [1u64, 2, 3, 4, 5, 8] {
        let log = MerkleLog::from_leaves((0..size).map(leaf).collect());
        println!("ROOT_{size}={}", log.root().to_hex());
    }
}
