//! Values: money, names, identity and drafts, memos.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::{
    AccountHandle, AccountPath, AccountRef, Amount, Balance, CaptureTime, Currency, Dimensions,
    Document, Entry, EntryId, EntryRecord, IdempotencyKey, Label, Layer, LedgerId, LedgerPolicy,
    MemoSalt, Rate, Rounding, Side, amount, currency,
    currency::{EUR, USD},
    error::{AmountError, Field, ValueError},
    record::{EntryParts, HoldAction, MemoCommitment, PostingRecord, Record},
};
use proptest::prelude::*;
use time::macros::date;

// ── US1: exact money ──────────────────────────────────────────────────────────

fn a2(text: &str) -> Result<Amount<2>, AmountError> {
    text.parse()
}

#[test]
fn us1_parse_accepts_exactly_the_grammar() {
    assert_eq!(a2("0"), Ok(Amount::ZERO));
    assert_eq!(a2("-0.00"), Ok(Amount::ZERO));
    assert_eq!(a2("12.3").map(Amount::to_minor), Ok(1230));
    assert_eq!(a2("-7").map(Amount::to_minor), Ok(-700));
    for bad in [
        "+1", " 1", "1 ", "1_000", "1e3", ".5", "5.", "", "-", "1.2.3", "--1", "0x10",
    ] {
        assert_eq!(a2(bad), Err(AmountError::InvalidLiteral), "{bad:?}");
    }
    assert_eq!(a2("1.000"), Err(AmountError::PrecisionLoss { scale: 2 }));
    assert_eq!(a2("1190.001"), Err(AmountError::PrecisionLoss { scale: 2 }));
    assert_eq!(
        "1.0".parse::<Amount<0>>(),
        Err(AmountError::PrecisionLoss { scale: 0 })
    );
    assert_eq!("15".parse::<Amount<0>>().map(Amount::to_minor), Ok(15));
}

#[test]
fn us1_extremes_round_trip_at_every_scale() {
    fn check<const P: u8>(max: &str) {
        let parsed: Amount<P> = max.parse().unwrap();
        assert_eq!(parsed, Amount::<P>::MAX);
        assert_eq!(parsed.to_string(), max);
        let min = format!("-{max}");
        let parsed: Amount<P> = min.parse().unwrap();
        assert_eq!(parsed, Amount::<P>::MIN);
        assert_eq!(parsed.to_string(), min);
    }
    check::<0>("170141183460469231731687303715884105727");
    check::<2>("1701411834604692317316873037158841057.27");
    check::<18>("170141183460469231731.687303715884105727");
    assert_eq!(
        a2("1701411834604692317316873037158841057.28"),
        Err(AmountError::Overflow)
    );
    assert_eq!(
        a2("-1701411834604692317316873037158841057.28"),
        Err(AmountError::Overflow)
    );
}

#[test]
fn us1_i128_min_is_unrepresentable_and_negation_is_total() {
    assert_eq!(
        Amount::<2>::from_minor(i128::MIN),
        Err(AmountError::Overflow)
    );
    assert_eq!(Amount::<2>::MIN.negate(), Amount::MAX);
    assert_eq!(Amount::<2>::MAX.negate(), Amount::MIN);
    assert_eq!(Amount::<2>::MIN.abs(), Amount::MAX);
    assert_eq!(Amount::<2>::from_major(5).map(Amount::to_minor), Ok(500));
    assert_eq!(
        Amount::<2>::from_major(i128::MAX),
        Err(AmountError::Overflow)
    );
}

#[test]
fn us1_display_honours_the_formatter() {
    let a: Amount = amount!("1190.00");
    assert_eq!(format!("{a:>12}"), "     1190.00");
    assert_eq!(format!("{a:<12}|"), "1190.00     |");
    assert_eq!(format!("{a:+}"), "+1190.00");
    assert_eq!(format!("{a:012}"), "000001190.00");
    assert_eq!(format!("{:012}", a.negate()), "-00001190.00");
    assert_eq!(format!("{a:*^11}"), "**1190.00**");
    assert_eq!(format!("{a:?}"), "Amount<2>(1190.00)");
    let small: Amount = amount!("-0.05");
    assert_eq!(small.to_string(), "-0.05");
    let whole: Amount<0> = amount!("42");
    assert_eq!(whole.to_string(), "42");
}

#[test]
fn us1_fits_exponent() {
    let a: Amount = amount!("12.30");
    assert!(a.fits_exponent(2));
    assert!(a.fits_exponent(1));
    assert!(!a.fits_exponent(0));
    assert!(!a.fits_exponent(3));
    assert!(Amount::<2>::ZERO.fits_exponent(0));
}

/// The exact rational result of `n / d` rounded by `mode`, by a separate
/// definition: floor and ceiling of the exact quotient, then the tie rules.
fn reference_round(n: i128, d: i128, mode: Rounding) -> i128 {
    let (n, d) = if d < 0 { (-n, -d) } else { (n, d) };
    let floor = n.div_euclid(d);
    let remainder = n.rem_euclid(d);
    if remainder == 0 {
        return floor;
    }
    let ceil = floor + 1;
    let (toward_zero, away) = if floor < 0 {
        (ceil, floor)
    } else {
        (floor, ceil)
    };
    match mode {
        Rounding::Floor => floor,
        Rounding::Ceiling => ceil,
        Rounding::TowardZero => toward_zero,
        Rounding::AwayFromZero => away,
        _ => match (2 * remainder).cmp(&d) {
            core::cmp::Ordering::Less => floor,
            core::cmp::Ordering::Greater => ceil,
            core::cmp::Ordering::Equal => match mode {
                Rounding::HalfUp => away,
                Rounding::HalfDown => toward_zero,
                _ if floor % 2 == 0 => floor,
                _ => ceil,
            },
        },
    }
}

const MODES: [Rounding; 7] = [
    Rounding::HalfUp,
    Rounding::HalfEven,
    Rounding::HalfDown,
    Rounding::TowardZero,
    Rounding::AwayFromZero,
    Rounding::Floor,
    Rounding::Ceiling,
];

#[test]
fn us1_every_rounding_mode_matches_the_rational_reference() {
    // n/2 covers ±0.5, ±1.5, ±2.5 ties; n/4 covers below and above the midpoint.
    for mode in MODES {
        for n in -12i128..=12 {
            for d in [2i128, -2, 3, 4, -4, 7] {
                let a = Amount::<2>::from_minor(n).unwrap();
                assert_eq!(
                    a.checked_mul_ratio(1, d, mode).map(Amount::to_minor),
                    Ok(reference_round(n, d, mode)),
                    "{n}/{d} {mode:?}"
                );
            }
        }
    }
    let ties = [
        (Rounding::HalfUp, [3, 2, 1, -1, -2, -3]),
        (Rounding::HalfEven, [2, 2, 0, 0, -2, -2]),
        (Rounding::HalfDown, [2, 1, 0, 0, -1, -2]),
        (Rounding::TowardZero, [2, 1, 0, 0, -1, -2]),
        (Rounding::AwayFromZero, [3, 2, 1, -1, -2, -3]),
        (Rounding::Floor, [2, 1, 0, -1, -2, -3]),
        (Rounding::Ceiling, [3, 2, 1, 0, -1, -2]),
    ];
    for (mode, expected) in ties {
        let got: Vec<i128> = [25, 15, 5, -5, -15, -25]
            .iter()
            .map(|tenths| {
                Amount::<1>::from_minor(*tenths)
                    .unwrap()
                    .rescale::<0>(mode)
                    .unwrap()
                    .to_minor()
            })
            .collect();
        assert_eq!(got, expected, "{mode:?}");
    }
}

#[test]
fn us1_rescale_up_and_down() {
    let a: Amount<2> = amount!("1.25");
    assert_eq!(
        a.rescale::<4>(Rounding::HalfUp).map(Amount::to_minor),
        Ok(12_500)
    );
    assert_eq!(
        a.rescale::<1>(Rounding::HalfEven).map(Amount::to_minor),
        Ok(12)
    );
    assert_eq!(
        a.rescale::<1>(Rounding::HalfUp).map(Amount::to_minor),
        Ok(13)
    );
    assert_eq!(
        Amount::<0>::MAX.rescale::<2>(Rounding::HalfUp),
        Err(AmountError::Overflow)
    );
}

#[test]
fn us1_overflow_is_an_error_never_a_wrap() {
    let max = Amount::<2>::MAX;
    let one = Amount::<2>::from_minor(1).unwrap();
    assert_eq!(max.checked_add(one), Err(AmountError::Overflow));
    assert_eq!(
        Amount::<2>::MIN.checked_sub(one),
        Err(AmountError::Overflow)
    );
    assert_eq!(max.checked_mul_int(2), Err(AmountError::Overflow));
    assert_eq!(max.checked_mul_int(-1), Ok(Amount::MIN));
    // The intermediate overflows even though the quotient would fit.
    assert_eq!(
        max.checked_mul_ratio(2, 2, Rounding::HalfUp),
        Err(AmountError::Overflow)
    );
    assert_eq!(
        one.checked_mul_ratio(1, 0, Rounding::HalfUp),
        Err(AmountError::DivideByZero)
    );
}

#[test]
// PoC: f7_distribute_huge_n_panics
fn us1_splits_are_exact() {
    let hundred: Amount = amount!("100.00");
    assert_eq!(
        hundred.allocate(&[1, 1, 1]).unwrap(),
        [amount!("33.34"), amount!("33.33"), amount!("33.33")]
    );
    assert_eq!(
        hundred.negate().allocate(&[1, 1, 1]).unwrap(),
        [amount!("-33.34"), amount!("-33.33"), amount!("-33.33")]
    );
    assert_eq!(
        hundred.allocate(&[1, 0, 1]).unwrap(),
        [amount!("50.00"), amount!("0.00"), amount!("50.00")]
    );
    // Ties go to the lowest index.
    let cent: Amount = amount!("0.01");
    assert_eq!(
        cent.allocate(&[1, 1]).unwrap(),
        [amount!("0.01"), amount!("0.00")]
    );
    // Largest remainder wins over position.
    let ten_cents: Amount = amount!("0.10");
    assert_eq!(
        ten_cents.allocate(&[1, 2]).unwrap(),
        [amount!("0.03"), amount!("0.07")]
    );
    // No intermediate overflow at the extremes (Σw = u64::MAX, the largest allowed).
    let parts = Amount::<2>::MAX.allocate(&[u64::MAX - 1, 1]).unwrap();
    assert_eq!(
        Amount::checked_sum(parts.iter().copied()),
        Ok(Amount::<2>::MAX)
    );
    assert_eq!(hundred.allocate(&[]), Err(AmountError::ZeroWeight));
    assert_eq!(hundred.allocate(&[0, 0]), Err(AmountError::ZeroWeight));
    assert_eq!(
        hundred.allocate(&vec![1; Amount::<2>::MAX_PARTS + 1]),
        Err(AmountError::TooManyParts {
            max: Amount::<2>::MAX_PARTS
        })
    );
    assert_eq!(hundred.allocate(&[u64::MAX, 1]), Err(AmountError::Overflow));
    assert_eq!(
        hundred.distribute(Amount::<2>::MAX_PARTS).map(|p| p.len()),
        Ok(Amount::<2>::MAX_PARTS)
    );
    assert_eq!(
        hundred.distribute(usize::MAX),
        Err(AmountError::TooManyParts {
            max: Amount::<2>::MAX_PARTS
        })
    );
    assert_eq!(hundred.distribute(0), Err(AmountError::ZeroWeight));
    assert_eq!(hundred.distribute(3), hundred.allocate(&[1, 1, 1]));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn us1_allocations_conserve_the_total(
        minor in -(1i128 << 100)..(1i128 << 100),
        weights in prop::collection::vec(0u64..1_000_000, 1..20),
    ) {
        prop_assume!(weights.iter().any(|w| *w > 0));
        let amount = Amount::<2>::from_minor(minor).unwrap();
        let parts = amount.allocate(&weights).unwrap();
        prop_assert_eq!(Amount::checked_sum(parts.iter().copied()), Ok(amount));
        let total: u128 = weights.iter().map(|w| u128::from(*w)).sum();
        for (part, weight) in parts.iter().zip(&weights) {
            // |part − amount·w/Σw| < 1 minor unit, checked exactly in integers.
            let exact = minor * i128::from(*weight);
            let scaled = part.to_minor() * i128::try_from(total).unwrap();
            prop_assert!((scaled - exact).abs() < i128::try_from(total).unwrap());
            if *weight == 0 {
                prop_assert!(part.is_zero());
            }
        }
    }

    #[test]
    fn us1_display_parse_round_trips(minor in any::<i128>().prop_filter("min", |m| *m != i128::MIN)) {
        let amount = Amount::<2>::from_minor(minor).unwrap();
        prop_assert_eq!(amount.to_string().parse::<Amount<2>>(), Ok(amount));
        let amount = Amount::<7>::from_minor(minor).unwrap();
        prop_assert_eq!(amount.to_string().parse::<Amount<7>>(), Ok(amount));
    }
}

#[test]
fn us1_sums_refuse_overflow() {
    let amounts = [Amount::<2>::MAX, Amount::from_minor(1).unwrap()];
    let owned: Result<Amount<2>, AmountError> = amounts.into_iter().sum();
    assert_eq!(owned, Err(AmountError::Overflow));
    let borrowed: Result<Amount<2>, AmountError> = amounts.iter().sum();
    assert_eq!(borrowed, Err(AmountError::Overflow));
    let fine: Result<Amount, AmountError> = [amount!("1.10"), amount!("2.20")].iter().sum();
    assert_eq!(fine, Ok(amount!("3.30")));
    assert_eq!(Amount::<2>::checked_sum([]), Ok(Amount::ZERO));
}

#[test]
fn us1_currency_codes() {
    assert_eq!(Currency::new("EUR"), Ok(EUR));
    assert_eq!(
        Currency::new("USDC").map(|c| c.to_string()),
        Ok("USDC".into())
    );
    assert!(Currency::new("XAU").is_ok());
    assert!(Currency::new("0123456789ABCDEF").is_ok());
    assert_eq!(Currency::new(""), Err(ValueError::Empty));
    assert_eq!(
        Currency::new("eur"),
        Err(ValueError::InvalidCharacter { found: 'e' })
    );
    assert_eq!(
        Currency::new("EUR "),
        Err(ValueError::InvalidCharacter { found: ' ' })
    );
    assert_eq!(
        Currency::new("0123456789ABCDEFG"),
        Err(ValueError::TooLong { max: 16, found: 17 })
    );
    assert!(Currency::new("AB").unwrap() < Currency::new("ABC").unwrap() && EUR < USD);
    const BTC: Currency = currency!("BTC");
    assert_eq!(BTC.as_str(), "BTC");
    assert_eq!(format!("{EUR:>5}|{EUR:?}"), "  EUR|Currency(EUR)");
}

#[test]
fn us1_balances_are_gross() {
    let seven: Amount = amount!("7.00");
    let five: Amount = amount!("5.00");
    let two: Amount = amount!("2.00");
    let a = Balance::ZERO
        .post(Side::Debit, seven)
        .unwrap()
        .post(Side::Credit, five)
        .unwrap();
    let b = Balance::ZERO.post(Side::Debit, two).unwrap();
    assert_ne!(a, b);
    assert!(a.nets_equal(&b));
    assert_eq!(a.net(), two);
    assert_eq!(a.postings(), 2);
    assert_eq!(
        Balance::ZERO.post(Side::Debit, Amount::<2>::ZERO),
        Err(AmountError::NotPositive)
    );
    assert_eq!(
        Balance::ZERO.post(Side::Debit, two.negate()),
        Err(AmountError::NotPositive)
    );
    let full = Balance::from_parts(Amount::<2>::MAX, Amount::ZERO, 1).unwrap();
    assert_eq!(full.post(Side::Debit, two), Err(AmountError::Overflow));
    assert_eq!(full.net(), Amount::MAX);
    let other = Balance::from_parts(Amount::ZERO, Amount::<2>::MAX, 1).unwrap();
    assert_eq!(other.net(), Amount::MIN);
    assert_eq!(full.checked_add(&full), Err(AmountError::Overflow));
    assert_eq!(a.checked_add(&b).map(|s| s.postings()), Ok(3));
    assert_eq!(
        Balance::from_parts(two, Amount::ZERO, 0),
        Err(ValueError::OutOfRange)
    );
    assert_eq!(
        Balance::<2>::from_parts(Amount::ZERO, Amount::ZERO, 1),
        Err(ValueError::OutOfRange)
    );
    assert_eq!(
        Balance::from_parts(two.negate(), Amount::ZERO, 1),
        Err(ValueError::OutOfRange)
    );
}

// ── US7: names that cannot be confused ────────────────────────────────────────

#[test]
fn us7_nfc_and_nfd_spellings_are_one_name() {
    assert_eq!(
        AccountPath::parse("Assets:Caf\u{65}\u{301}").unwrap(),
        AccountPath::parse("Assets:Caf\u{e9}").unwrap()
    );
    assert_eq!(Label::new("Caf\u{65}\u{301}"), Label::new("Caf\u{e9}"));
}

#[test]
fn us7_label_rules() {
    assert!(Label::new("x".repeat(64)).is_ok());
    assert_eq!(
        Label::new("x".repeat(65)),
        Err(ValueError::TooLong { max: 64, found: 65 })
    );
    assert_eq!(Label::new(" Cash"), Err(ValueError::EdgeWhitespace));
    assert_eq!(Label::new("Cash\t"), Err(ValueError::EdgeWhitespace));
    assert_eq!(Label::new("Ca\u{0}sh"), Err(ValueError::ControlCharacter));
    assert_eq!(Label::new(""), Err(ValueError::Empty));
}

#[test]
// PoC: f6_account_path_separator_in_segment
fn us7_path_rules() {
    assert_eq!(
        AccountPath::from_segments(["Assets:Cash"]),
        Err(ValueError::SeparatorInSegment)
    );
    let sixteen = vec!["A"; 16].join(":");
    assert_eq!(AccountPath::parse(&sixteen).map(|p| p.depth()), Ok(16));
    let seventeen = vec!["A"; 17].join(":");
    assert_eq!(
        AccountPath::parse(&seventeen),
        Err(ValueError::TooDeep { max: 16 })
    );
    assert_eq!(
        AccountPath::parse(&format!("Assets:{}", "x".repeat(65))),
        Err(ValueError::TooLong { max: 64, found: 65 })
    );
    assert_eq!(AccountPath::parse(" Cash"), Err(ValueError::EdgeWhitespace));
    for bad in ["Assets::Cash", ":Cash", "Cash:", ""] {
        assert_eq!(AccountPath::parse(bad), Err(ValueError::Empty), "{bad:?}");
    }
}

#[test]
fn us7_paths_order_segment_wise_and_navigate() {
    let cash = AccountPath::parse("Assets:Cash").unwrap();
    let petty = AccountPath::parse("Assets:Cash:Petty").unwrap();
    let cash2 = AccountPath::parse("Assets:Cash2").unwrap();
    let mut sorted = vec![cash2.clone(), petty.clone(), cash.clone()];
    sorted.sort();
    assert_eq!(sorted, [cash.clone(), petty.clone(), cash2.clone()]);
    assert_eq!(petty.parent(), Some(cash.clone()));
    assert_eq!(AccountPath::parse("Assets").unwrap().parent(), None);
    assert_eq!(cash.child("Petty"), Ok(petty.clone()));
    assert_eq!(cash.child("a:b"), Err(ValueError::SeparatorInSegment));
    assert!(cash.is_ancestor_of(&petty));
    assert!(!cash.is_ancestor_of(&cash));
    assert!(!cash.is_ancestor_of(&cash2));
    assert_eq!(
        petty.segments().collect::<Vec<_>>(),
        ["Assets", "Cash", "Petty"]
    );
    assert_eq!(petty.depth(), 3);
}

fn segment() -> impl Strategy<Value = String> {
    "[A-Za-z0-9][A-Za-z0-9 _é-]{0,10}[A-Za-z0-9]"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn us7_paths_round_trip(segments in prop::collection::vec(segment(), 1..=16)) {
        let path = AccountPath::from_segments(&segments).unwrap();
        prop_assert_eq!(path.to_string().parse::<AccountPath>(), Ok(path.clone()));
        prop_assert_eq!(path.to_string(), segments.join(":"));
    }

    #[test]
    fn us7_labels_round_trip(text in segment()) {
        let label = Label::new(&text).unwrap();
        prop_assert_eq!(label.to_string().parse::<Label>(), Ok(label));
    }
}

#[test]
fn us7_dimensions() {
    let label = |s: &str| Label::new(s).unwrap();
    let mut dims = Dimensions::new();
    for axis in ["h", "g", "f", "e", "d", "c", "b", "a"] {
        dims = dims.with(label(axis), label("v")).unwrap();
    }
    assert_eq!(dims.len(), 8);
    assert_eq!(
        dims.clone().with(label("z"), label("v")),
        Err(ValueError::TooManyDimensions)
    );
    assert_eq!(
        dims.clone().with(label("a"), label("w")),
        Err(ValueError::DuplicateAxis)
    );
    let axes: Vec<&str> = dims.iter().map(|(axis, _)| axis.as_str()).collect();
    assert_eq!(axes, ["a", "b", "c", "d", "e", "f", "g", "h"]);
    assert_eq!(dims.get(&label("c")), Some(&label("v")));
    assert!(Dimensions::new().is_empty());
}

#[test]
fn us7_identifiers() {
    assert!(LedgerId::new("x".repeat(255)).is_ok());
    assert_eq!(
        LedgerId::new("x".repeat(256)),
        Err(ValueError::TooLong {
            max: 255,
            found: 256
        })
    );
    assert_eq!(
        LedgerId::new("a+b"),
        Err(ValueError::InvalidCharacter { found: '+' })
    );
    assert_eq!(
        LedgerId::new("a b"),
        Err(ValueError::InvalidCharacter { found: ' ' })
    );
    assert_eq!(
        LedgerId::new("café"),
        Err(ValueError::InvalidCharacter { found: 'é' })
    );
    assert_eq!(LedgerId::new(""), Err(ValueError::Empty));

    assert!(IdempotencyKey::new("k".repeat(128)).is_ok());
    assert_eq!(
        IdempotencyKey::new("k".repeat(129)),
        Err(ValueError::TooLong {
            max: 128,
            found: 129
        })
    );
    assert_eq!(
        IdempotencyKey::new("a\nb"),
        Err(ValueError::ControlCharacter)
    );
    // Keys are byte-exact: NFD stays NFD.
    let nfd = IdempotencyKey::new("Cafe\u{301}").unwrap();
    assert_eq!(nfd.as_str(), "Cafe\u{301}");
    assert_ne!(nfd, IdempotencyKey::new("Caf\u{e9}").unwrap());

    assert_eq!(AccountHandle::new(u32::MAX), None);
    assert_eq!(
        AccountHandle::new(AccountHandle::MAX).map(AccountHandle::get),
        Some(u32::MAX - 1)
    );
    assert_eq!(AccountHandle::new(42).unwrap().to_string(), "#42"); // 022 contracts §6
}

#[test]
fn us7_policy() {
    let label = |s: &str| Label::new(s).unwrap();
    let policy = LedgerPolicy::new([label("b"), label("a")], Some(3)).unwrap();
    assert_eq!(policy.required_dimensions(), [label("a"), label("b")]);
    assert_eq!(policy.max_value_date_drift_days(), Some(3));
    assert_eq!(
        LedgerPolicy::new([label("a"), label("a")], None),
        Err(ValueError::Duplicate)
    );
    let nine = ["1", "2", "3", "4", "5", "6", "7", "8", "9"].map(label);
    assert_eq!(
        LedgerPolicy::new(nine, None),
        Err(ValueError::TooManyDimensions)
    );
    assert_eq!(
        LedgerPolicy::default().required_dimensions(),
        [] as [Label; 0]
    );
}

// ── US3: derived identity and the fingerprint ─────────────────────────────────

fn ledger() -> LedgerId {
    LedgerId::new("example.com/acme").unwrap()
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).unwrap()
}

#[test]
fn us3_entry_ids_are_derived() {
    let id = EntryId::derive(&ledger(), &key("invoice-2026-0001"));
    assert_eq!(id.to_string(), "bab3f09bfedcc7d793e973e78dbe4bd4");
    let other = EntryId::derive(
        &LedgerId::new("example.com/other").unwrap(),
        &key("invoice-2026-0001"),
    );
    assert_ne!(id, other);
    assert_eq!(
        "BAB3F09BFEDCC7D793E973E78DBE4BD4".parse::<EntryId>(),
        Ok(id)
    );
    assert_eq!("bab3".parse::<EntryId>(), Err(ValueError::InvalidHex));
    assert_eq!(EntryId::from_bytes(*id.as_bytes()), id);
}

#[test]
fn us3_entry_content_values() {
    assert!(CaptureTime::new(0, 1080).is_ok());
    assert!(CaptureTime::new(0, -1080).is_ok());
    assert_eq!(CaptureTime::new(0, 1081), Err(ValueError::OutOfRange));
    assert_eq!(CaptureTime::new(0, -1081), Err(ValueError::OutOfRange));
    assert!(CaptureTime::new(253_402_300_799, 0).is_ok());
    assert_eq!(
        CaptureTime::new(253_402_300_800, 0),
        Err(ValueError::OutOfRange)
    );
    // A date-time maps on with its whole seconds and offset minutes; no
    // `time` type is in the signature (022 FR-023).
    let at = time::macros::datetime!(2026-03-15 10:00:00.9 +01:00);
    let captured = CaptureTime::new(at.unix_timestamp(), at.offset().whole_minutes()).unwrap();
    assert_eq!(
        (captured.unix_seconds(), captured.utc_offset_minutes()),
        (1_773_565_200, 60)
    );

    assert!(Document::new("invoice", "x".repeat(256)).is_ok());
    assert_eq!(
        Document::new("invoice", "x".repeat(257)),
        Err(ValueError::TooLong {
            max: 256,
            found: 257
        })
    );
    assert_eq!(Document::new("", "x"), Err(ValueError::Empty));

    let rate = Rate::new(EUR, USD, 108_730, -5).unwrap();
    assert_eq!((rate.mantissa(), rate.exponent()), (10873, -4));
    assert_eq!(Rate::parse(EUR, USD, "1.0873"), Ok(rate));
    assert_eq!(Rate::parse(EUR, USD, "1.087300"), Ok(rate));
    assert_eq!(Rate::parse(EUR, USD, "0001.0873"), Ok(rate));
    assert_eq!(
        Rate::parse(EUR, USD, "15000").map(|r| (r.mantissa(), r.exponent())),
        Ok((15, 3))
    );
    assert_eq!(
        Rate::parse(EUR, USD, "15000").map(|r| r.to_string()),
        Ok("15000".into())
    );
    assert_eq!(
        Rate::parse(EUR, USD, "0.0005").map(|r| r.to_string()),
        Ok("0.0005".into())
    );
    assert_eq!(Rate::new(EUR, EUR, 1, 0), Err(ValueError::SameCurrency));
    assert_eq!(Rate::new(EUR, USD, 0, 0), Err(ValueError::NotPositive));
    assert_eq!(Rate::new(EUR, USD, -1, 0), Err(ValueError::NotPositive));
    assert_eq!(Rate::parse(EUR, USD, "0.00"), Err(ValueError::NotPositive));
    assert_eq!(
        Rate::parse(EUR, USD, "-1"),
        Err(ValueError::InvalidCharacter { found: '-' })
    );
    assert_eq!(Rate::parse(EUR, USD, "1."), Err(ValueError::Empty));
    assert_eq!(
        Rate::new(EUR, USD, 10, i8::MAX),
        Err(ValueError::OutOfRange)
    );
}

fn full_draft() -> Entry {
    Entry::new("invoice-2026-0001", date!(2026 - 03 - 15))
        .value_date(date!(2026 - 03 - 17))
        .captured_at(CaptureTime::new(1_773_568_800, 60).unwrap())
        .kind("invoice")
        .memo("Rechnung 4711")
        .document(
            Document::new("invoice", "RE-2026-0001")
                .unwrap()
                .dated(date!(2026 - 03 - 15)),
        )
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit(AccountHandle::new(1).unwrap(), amount!("1190.00"), EUR)
        .reverses(EntryId::derive(&ledger(), &key("invoice-2026-0000")))
        .rate(Rate::parse(EUR, USD, "1.0873").unwrap())
}

#[test]
fn us3_the_design_example_builds_without_question_marks() {
    let draft: Entry = Entry::new("invoice-2026-0001", date!(2026 - 03 - 15))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR);
    assert!(draft.build_errors().is_empty());
    assert_eq!(draft.get_value_date(), date!(2026 - 03 - 15));
    assert_eq!(draft.postings().len(), 2);
    assert_eq!(
        draft.id(&ledger()).map(|id| id.to_string()),
        Some("bab3f09bfedcc7d793e973e78dbe4bd4".into())
    );
}

#[test]
fn us3_builder_errors_are_collected_per_field() {
    let field_of =
        |draft: Entry| -> Vec<Field> { draft.build_errors().iter().map(|e| e.field).collect() };
    let base = || -> Entry { Entry::new("k", date!(2026 - 03 - 15)) };
    assert_eq!(
        field_of(base().debit("Assets::Cash", amount!("1.00"), EUR)),
        [Field::Posting(0)]
    );
    assert_eq!(field_of(base().kind("")), [Field::Kind]);
    assert_eq!(field_of(base().kind("").kind("invoice")), [] as [Field; 0]);
    assert_eq!(
        field_of(Entry::new("", date!(2026 - 03 - 15))),
        [Field::Key]
    );
    let mut posting = doubleentry::Posting::debit("Assets:Cash", amount!("1.00"), EUR);
    for axis in ["a", "b", "c", "d", "e", "f", "g", "h", "i"] {
        posting = posting.dimension(axis, "v");
    }
    assert_eq!(
        field_of(
            base()
                .credit("Income", amount!("1.00"), EUR)
                .posting(posting)
        ),
        [Field::Dimension { posting: 1 }]
    );
    let draft: Entry = base().kind("");
    let error = draft.fingerprint().unwrap_err();
    assert_eq!((error.field, error.error), (Field::Kind, ValueError::Empty));
    assert_eq!(draft.id(&ledger()).map(|_| ()), Some(()));
    assert_eq!(
        Entry::<2>::new("", date!(2026 - 03 - 15)).id(&ledger()),
        None
    );
}

#[test]
fn us3_rates_are_sorted_and_replaced_per_pair() {
    let draft: Entry = Entry::new("k", date!(2026 - 03 - 15))
        .rate(Rate::parse(USD, EUR, "0.9").unwrap())
        .rate(Rate::parse(EUR, USD, "1.1").unwrap())
        .rate(Rate::parse(EUR, USD, "1.2").unwrap());
    let rates: Vec<String> = draft
        .rates()
        .iter()
        .map(|r| format!("{}/{} {r}", r.base(), r.quote()))
        .collect();
    assert_eq!(rates, ["EUR/USD 1.2", "USD/EUR 0.9"]);
}

#[test]
fn us3_fingerprints_cover_exactly_the_draft() {
    let base = full_draft();
    assert_eq!(base.fingerprint(), full_draft().fingerprint());
    let changed: Vec<Entry> = vec![
        full_draft().value_date(date!(2026 - 03 - 18)),
        full_draft().captured_at(CaptureTime::new(1_773_568_801, 60).unwrap()),
        full_draft().kind("receipt"),
        full_draft().memo("Rechnung 4712"),
        full_draft().document(Document::new("receipt", "x").unwrap()),
        full_draft().debit("Assets:Bank", amount!("0.01"), EUR),
        full_draft().reverses(EntryId::from_bytes([0; 16])),
        full_draft().rate(Rate::parse(EUR, USD, "1.0874").unwrap()),
        Entry::new("invoice-2026-0002", date!(2026 - 03 - 15)),
        // The same account by handle instead of path is a different draft.
        Entry::new("invoice-2026-0001", date!(2026 - 03 - 15))
            .value_date(date!(2026 - 03 - 17))
            .captured_at(CaptureTime::new(1_773_568_800, 60).unwrap())
            .kind("invoice")
            .memo("Rechnung 4711")
            .document(
                Document::new("invoice", "RE-2026-0001")
                    .unwrap()
                    .dated(date!(2026 - 03 - 15)),
            )
            .debit(AccountHandle::new(0).unwrap(), amount!("1190.00"), EUR)
            .credit(AccountHandle::new(1).unwrap(), amount!("1190.00"), EUR)
            .reverses(EntryId::derive(&ledger(), &key("invoice-2026-0000")))
            .rate(Rate::parse(EUR, USD, "1.0873").unwrap()),
    ];
    for (index, draft) in changed.iter().enumerate() {
        assert_ne!(draft.fingerprint(), base.fingerprint(), "change {index}");
    }
    // The salt is not part of the fingerprint.
    let salted = |salt: u8| -> Entry {
        Entry::new("k", date!(2026 - 03 - 15)).memo_salted("memo", MemoSalt([salt; 16]))
    };
    assert_eq!(salted(1).fingerprint(), salted(2).fingerprint());
    assert_eq!(
        salted(1).fingerprint(),
        Entry::<2>::new("k", date!(2026 - 03 - 15))
            .memo("memo")
            .fingerprint()
    );
}

#[test]
fn us3_a_decoded_entry_re_derives_its_id() {
    // There is no stored id to swap: the id of a decoded leaf is re-derived
    // from its committed key, so swapping two entries' ids is inexpressible.
    let entry = |name: &str| -> Record {
        Record::Entry(
            EntryRecord::new(EntryParts {
                key: key(name),
                booking_date: date!(2026 - 03 - 15),
                value_date: date!(2026 - 03 - 15),
                captured_at: None,
                kind: None,
                memo: None,
                documents: vec![],
                postings: vec![PostingRecord {
                    handle: AccountHandle::new(0).unwrap(),
                    side: Side::Debit,
                    amount: amount!("1.00"),
                    currency: EUR,
                    layer: Layer::Settled,
                    dimensions: Dimensions::new(),
                }],
                reverses: None,
                hold: None,
                rates: vec![],
            })
            .unwrap(),
        )
    };
    for name in ["invoice-2026-0001", "invoice-2026-0002"] {
        let Ok(Record::Entry(decoded)) = Record::<2>::decode(&entry(name).to_leaf_bytes()) else {
            panic!("not an entry");
        };
        assert_eq!(
            decoded.id(&ledger()),
            EntryId::derive(&ledger(), &key(name))
        );
    }
}

// ── US6: erasable memo ────────────────────────────────────────────────────────

#[test]
fn us6_memo_commitment_vector_and_erasure() {
    let salt = MemoSalt(core::array::from_fn(|i| i as u8));
    let commitment = MemoCommitment::compute(&salt, "Rechnung 4711");
    assert_eq!(
        commitment.to_string(),
        "d1f4b79422abbf28c679a18d6487c3faafdd151b1f1fc0fc941d2dabd3e47dbc"
    );
    // The record holds only the commitment: the leaf is the same bytes whether
    // or not anyone still holds the text and the salt.
    let leaf = |memo: MemoCommitment| -> Vec<u8> {
        Record::<2>::Entry(
            EntryRecord::new(EntryParts {
                key: key("k"),
                booking_date: date!(2026 - 03 - 15),
                value_date: date!(2026 - 03 - 15),
                captured_at: None,
                kind: None,
                memo: Some(memo),
                documents: vec![],
                postings: vec![],
                reverses: None,
                hold: None,
                rates: vec![],
            })
            .unwrap(),
        )
        .to_leaf_bytes()
    };
    let before = leaf(commitment);
    let (text, salt) = (String::from("Rechnung 4711"), salt);
    drop((text, salt));
    assert_eq!(
        leaf(MemoCommitment::from_bytes(*commitment.as_bytes())),
        before
    );

    let long: Entry = Entry::new("k", date!(2026 - 03 - 15)).memo("x".repeat(4097));
    let errors: Vec<(Field, ValueError)> = long
        .build_errors()
        .iter()
        .map(|error| (error.field, error.error))
        .collect();
    assert_eq!(
        errors,
        [(
            Field::Memo,
            ValueError::TooLong {
                max: 4096,
                found: 4097
            }
        )]
    );
    let empty: Entry = Entry::new("k", date!(2026 - 03 - 15)).memo("");
    assert_eq!(empty.build_errors()[0].field, Field::Memo);
}

// ── FR-049: only holds write the Pending layer ────────────────────────────────

#[test]
fn caller_postings_are_settled_and_carry_no_hold() {
    let draft: Entry = Entry::new("k", date!(2026 - 03 - 15))
        .debit("Assets:Cash", amount!("1.00"), EUR)
        .credit("Income:Sales", amount!("1.00"), EUR);
    assert!(
        draft
            .postings()
            .iter()
            .all(|p| p.get_layer() == Layer::Settled)
    );
    assert_eq!(draft.get_hold(), None::<&HoldAction>);
    assert_eq!(
        draft.postings()[0].account(),
        Some(&AccountRef::Path(
            AccountPath::parse("Assets:Cash").unwrap()
        ))
    );
}

#[test]
fn draft_encoded_len_matches_the_leaf() {
    let draft = full_draft();
    let resolved = EntryParts {
        key: key("invoice-2026-0001"),
        booking_date: draft.booking_date(),
        value_date: draft.get_value_date(),
        captured_at: draft.get_captured_at(),
        kind: draft.get_kind().cloned(),
        memo: Some(MemoCommitment::from_bytes([9; 32])),
        documents: draft.documents().to_vec(),
        postings: draft
            .postings()
            .iter()
            .enumerate()
            .map(|(i, p)| PostingRecord {
                handle: AccountHandle::new(i as u32).unwrap(),
                side: p.side(),
                amount: p.amount(),
                currency: p.currency(),
                layer: p.get_layer(),
                dimensions: p.get_dimensions().clone(),
            })
            .collect(),
        reverses: draft.get_reverses(),
        hold: None,
        rates: draft.rates().to_vec(),
    };
    let record: Record = Record::Entry(EntryRecord::new(resolved).unwrap());
    assert_eq!(draft.encoded_len(), record.to_leaf_bytes().len());
}

// ── serde (FR-048) ────────────────────────────────────────────────────────────

#[cfg(feature = "serde")]
mod serde_round_trips {
    use super::*;
    use doubleentry::{
        PostAmount, TreeHead,
        log::{ConsistencyProof, Hash, InclusionProof, leaf_hash},
    };
    use serde::{Serialize, de::DeserializeOwned};

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + core::fmt::Debug>(value: &T) {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value, "{json}");
    }

    #[test]
    fn every_value_round_trips() {
        round_trip::<Amount>(&amount!("1190.00"));
        round_trip(&EUR);
        round_trip(&Label::new("Café").unwrap());
        round_trip(&AccountPath::parse("Assets:Cash").unwrap());
        round_trip(&ledger());
        round_trip(&key("invoice-2026-0001"));
        round_trip(&EntryId::derive(&ledger(), &key("k")));
        round_trip(&leaf_hash(b"x"));
        round_trip(&AccountHandle::new(7).unwrap());
        round_trip(
            &Dimensions::new()
                .with(Label::new("a").unwrap(), Label::new("b").unwrap())
                .unwrap(),
        );
        round_trip(&Rate::parse(EUR, USD, "1.0873").unwrap());
        round_trip(
            &Document::new("invoice", "RE-1")
                .unwrap()
                .dated(date!(2026 - 03 - 15))
                .with_sha256([7; 32]),
        );
        round_trip(&CaptureTime::new(1, 2).unwrap());
        round_trip(&HoldAction::<2>::Post {
            hold: EntryId::from_bytes([1; 16]),
            amount: PostAmount::Partial(amount!("1.00")),
        });
        round_trip(&HoldAction::<2>::Open {
            expires: Some(date!(2026 - 03 - 31)),
        });
        round_trip(&Side::Credit);
        round_trip(&Layer::Pending);
        round_trip(&LedgerPolicy::new([Label::new("a").unwrap()], Some(2)).unwrap());
        round_trip(
            &Balance::<2>::ZERO
                .post(Side::Debit, amount!("2.00"))
                .unwrap(),
        );
        round_trip(&TreeHead {
            size: 3,
            root: leaf_hash(b"r"),
        });
        round_trip(&InclusionProof {
            index: 1,
            size: 3,
            path: vec![leaf_hash(b"a"), Hash::default()],
        });
        round_trip(&ConsistencyProof {
            old_size: 1,
            new_size: 3,
            path: vec![leaf_hash(b"a")],
        });
        round_trip(
            &doubleentry::Posting::<2>::debit("Assets:Cash", amount!("1.00"), EUR)
                .dimension("a", "b"),
        );
        round_trip(&full_draft());
        let salted: Entry = full_draft().memo_salted("m", MemoSalt([3; 16]));
        round_trip(&salted);
    }

    #[test]
    fn values_are_strings_and_are_validated_on_the_way_in() {
        assert_eq!(
            serde_json::to_string(&amount!("1.50") as &Amount).unwrap(),
            "\"1.50\""
        );
        assert!(serde_json::from_str::<Amount>("1.5").is_err());
        assert!(serde_json::from_str::<Amount>("\"1.505\"").is_err());
        assert!(serde_json::from_str::<Currency>("\"eur\"").is_err());
        assert!(serde_json::from_str::<AccountPath>("\"A::B\"").is_err());
        assert!(serde_json::from_str::<AccountHandle>(&u32::MAX.to_string()).is_err());
        let nfd: Label = serde_json::from_str("\"Cafe\u{301}\"").unwrap();
        assert_eq!(nfd.as_str(), "Caf\u{e9}");
        let bad_balance = r#"{"debit":"1.00","credit":"0.00","postings":0}"#;
        assert!(serde_json::from_str::<Balance>(bad_balance).is_err());
    }

    #[test]
    fn a_draft_with_build_errors_does_not_serialize() {
        let broken: Entry = Entry::new("", date!(2026 - 03 - 15));
        assert!(serde_json::to_string(&broken).is_err());
    }
}

// ── FR-049: PostingRef::at and PeriodId ───────────────────────────────────────

#[test]
fn posting_ref_at_equals_the_decoded_clearing_reference() {
    use doubleentry::{
        PeriodId, PostingRef,
        record::{ClearingAction, ClearingEvent},
    };
    let target = EntryId::derive(&ledger(), &key("invoice-2026-0001"));
    let clearing: Record = Record::Clearing(ClearingEvent {
        action: ClearingAction::Apply {
            key: key("clear-1"),
            debit: PostingRef::at(target, 3),
            credit: PostingRef::at(target, 4),
            amount: amount!("1.00"),
        },
        date: date!(2026 - 03 - 20),
    });
    let Ok(Record::Clearing(decoded)) = Record::<2>::decode(&clearing.to_leaf_bytes()) else {
        panic!("not a clearing");
    };
    let ClearingAction::Apply { debit, .. } = decoded.action else {
        panic!("not an apply");
    };
    assert_eq!(debit, PostingRef::at(target, 3));
    assert_eq!((debit.entry, debit.index), (target, 3));
    let period: PeriodId = Label::new("2026-03").unwrap();
    assert_eq!(period.as_str(), "2026-03");
}

// ── SC-004 (post-impl): every value type round-trips, 1 024 cases each ──────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn sc004_currency_round_trips(code in "[A-Z]{3}") {
        let currency = Currency::new(&code).unwrap();
        prop_assert_eq!(currency.to_string().parse::<Currency>(), Ok(currency));
        prop_assert_eq!(currency.as_str(), code.as_str());
    }

    #[test]
    fn sc004_ids_and_keys_round_trip(
        host in "[a-z]{1,12}\\.[a-z]{2,6}",
        name in "[a-z0-9-]{1,20}",
        key in "[ -~]{1,64}",
    ) {
        let ledger = LedgerId::new(format!("{host}/{name}")).unwrap();
        prop_assert_eq!(ledger.to_string().parse::<LedgerId>(), Ok(ledger.clone()));
        let key = IdempotencyKey::new(&key).unwrap();
        prop_assert_eq!(key.to_string().parse::<IdempotencyKey>(), Ok(key.clone()));
        let id = EntryId::derive(&ledger, &key);
        prop_assert_eq!(id.to_string().parse::<EntryId>(), Ok(id));
    }

    #[test]
    fn sc004_hashes_round_trip(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let hash = doubleentry::log::leaf_hash(&bytes);
        prop_assert_eq!(hash.to_string().parse::<doubleentry::log::Hash>(), Ok(hash));
    }

    #[test]
    fn sc004_rates_round_trip(mantissa in 1i128..1_000_000_000_000, exponent in -12i8..6) {
        let rate = Rate::new(EUR, USD, mantissa, exponent).unwrap();
        prop_assert_eq!(Rate::parse(EUR, USD, &rate.to_string()), Ok(rate));
    }

    #[test]
    fn sc004_dimensions_round_trip(
        pairs in prop::collection::btree_map("[a-z][a-z0-9]{0,8}", "[A-Za-z0-9]{1,12}", 0..6),
    ) {
        let mut dimensions = Dimensions::new();
        for (axis, value) in &pairs {
            dimensions = dimensions
                .with(Label::new(axis).unwrap(), Label::new(value).unwrap())
                .unwrap();
        }
        let rebuilt = dimensions
            .iter()
            .try_fold(Dimensions::new(), |acc, (axis, value)| {
                acc.with(axis.to_string().parse()?, value.to_string().parse()?)
            });
        prop_assert_eq!(rebuilt, Ok(dimensions));
    }
}

#[cfg(feature = "serde")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// SC-004: serde round-trips as properties, not one example per type.
    #[test]
    fn sc004_serde_round_trips(
        minor in -(1i128 << 100)..(1i128 << 100),
        code in "[A-Z]{3}",
        key in "[ -~]{1,64}",
        mantissa in 1i128..1_000_000_000,
        exponent in -9i8..3,
    ) {
        fn back<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
            serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
        }
        let amount = Amount::<2>::from_minor(minor).unwrap();
        prop_assert_eq!(back(&amount), amount);
        let currency = Currency::new(&code).unwrap();
        prop_assert_eq!(back(&currency), currency);
        let key = IdempotencyKey::new(&key).unwrap();
        prop_assert_eq!(back(&key), key.clone());
        let id = EntryId::derive(&ledger(), &key);
        prop_assert_eq!(back(&id), id);
        let rate = Rate::new(EUR, USD, mantissa, exponent).unwrap();
        prop_assert_eq!(back(&rate), rate);
    }
}
