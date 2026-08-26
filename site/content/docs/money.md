+++
title = "Money"
description = "Exact scaled-integer arithmetic: choosing a precision, applying rates and rounding modes, allocating without leaking minor units, and why there are no arithmetic operators."
weight = 4
+++

## Scaled integers, not floats

`Amount<P>` is an `i64` count of minor units with the precision `P` fixed at
compile time. `Amount<2>` counts hundredths; `Amount<5>` counts
hundred-thousandths.

There is no binary floating point anywhere, and no decimal type whose scale can
vary at run time. One value has exactly **one** representation — which is what
makes hashing a monetary amount meaningful in the first place.

```rust
type Eur = Amount<2>;

let a = Eur::parse("1234.56")?;
assert_eq!(a.to_minor(), 123_456);
assert_eq!(a.to_string(), "1234.56");
```

Parsing rejects more precision than the scale can carry, rather than rounding
silently. At the point where an amount enters a ledger, a value that does not fit
the booking scale is a defect upstream:

```rust
assert!(Eur::parse("1.234").is_err());     // more precision than scale 2
assert_eq!(Eur::parse("1.2300")?, Eur::parse("1.23")?);  // trailing zeros are not precision
```

## Choosing a precision

The `i64` bounds the *minor units*, so raising the scale spends range on
precision rather than on magnitude:

| `P` | Largest major-unit value |
|---|---|
| 0 | ~9.2 × 10¹⁸ |
| 2 | ~9.2 × 10¹⁶ |
| 4 | ~9.2 × 10¹⁴ |
| 8 | ~9.2 × 10¹⁰ |
| 18 | ~9.2 |

So `Amount<2>` covers roughly 92 quadrillion currency units — ample for both
individual postings and cumulative balances — while `Amount<18>` compiles but
cannot represent ten of anything. `MAX_PRECISION` is the point past which one
major unit stops being representable at all, not a recommendation.

Pick the scale your currency is *booked* in. `Currency::minor_units` returns it
for the codes it knows, and `None` for the rest rather than guessing:

```rust
assert_eq!(Currency::EUR.minor_units(), Some(2));
assert_eq!(Currency::JPY.minor_units(), Some(0));   // yen has no minor unit
assert_eq!(Currency::new("XYZ")?.minor_units(), None);
```

Note that `Amount<P>` and `Currency` are independent: the type does not stop you
pairing `Amount<2>` with `JPY`. Enforcing that pairing would mean a distinct type
per currency, which makes a multi-currency ledger unwritable. The ledger's scale
is a deployment decision; `LedgerPolicy::in_currency` is how you constrain the
currency side of it.

## No arithmetic operators

`Add`, `Sub` and friends are deliberately **not** implemented. In a ledger an
overflow is a condition to report, not a process to abort:

```rust
assert_eq!(Eur::MAX.checked_add(Eur::from_minor(1)), Err(MoneyError::Overflow));
assert_eq!(Eur::MIN.checked_neg(),                   Err(MoneyError::Overflow));
```

Every operation that can overflow returns a `Result`. The crate denies
`clippy::arithmetic_side_effects` in library code, so this is enforced rather
than remembered.

## Rates, tax and conversion

`checked_mul_ratio` applies a ratio and resolves the remainder by a rounding mode
you name — `net × 19 / 100` for VAT, `principal × days / 365` for interest,
`eur × rate / 10⁶` for a conversion. It is exact in `i128` and only the result has
to fit, which matters: the intermediate product overflows an `i64` long before
either operand is unreasonable.

```rust
let net = Eur::parse("1000.00")?;
assert_eq!(net.checked_mul_ratio(19, 100, Rounding::HalfUp)?, Eur::parse("190.00")?);

// A quantity times a unit price lands on a minor unit, so there is nothing to
// round and no mode to pick.
assert_eq!(Eur::parse("1.99")?.checked_mul_int(3)?, Eur::parse("5.97")?);
```

The mode is **required**: there is no answer that is right everywhere, and
choosing one silently is how a ledger drifts.

| Mode | Ties go | Typically |
|---|---|---|
| `HalfUp` | away from zero | VAT and most tax rules |
| `HalfEven` | to the nearer even unit | accrual and valuation (unbiased over many roundings) |
| `HalfDown` | toward zero | |
| `TowardZero` / `AwayFromZero` | — | truncation, and its opposite |
| `Floor` / `Ceiling` | — | a fee you may not over- or undercharge |

The names follow `java.math.RoundingMode` and Python's `decimal`, so a rule
written against a specification in either transcribes rather than translates.

```rust
let cent = Eur::parse("0.01")?;
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::HalfUp)?,   Eur::parse("0.01")?);
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::HalfEven)?, Eur::parse("0.00")?);
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::Floor)?,    Eur::parse("0.00")?);
```

### Crossing scales

`rescale` restates an amount at a different precision. Widening is exact;
narrowing takes a rounding mode, because it is the one conversion in the crate
that can lose information.

Paired with `checked_mul_ratio` it makes a conversion at a published rate
expressible: parse the rate at the precision it was published in, apply it, and
restate the result in the scale the receiving books are kept in.

```rust
let eur  = Amount::<2>::parse("1000.00")?;
let rate = Amount::<6>::parse("1.087350")?;

let usd: Amount<2> = eur
    .rescale::<6>(Rounding::HalfEven)?
    .checked_mul_ratio(rate.to_minor(), 1_000_000, Rounding::HalfEven)?
    .rescale::<2>(Rounding::HalfEven)?;

assert_eq!(usd, Amount::<2>::parse("1087.35")?);
```

The engine still does not convert currencies. An entry balances each currency
independently, so a conversion is two legs and a rate you chose, booked through
an account you named — this only makes the arithmetic in the middle exact.

## Allocation without leaks

Splitting money is where value quietly disappears. Both operations here are exact:
the parts always re-sum to the original.

```rust
// Equal split: parts differ by at most one minor unit.
let parts = Eur::parse("100.00")?.distribute(3)?;
assert_eq!(parts, vec![
    Eur::parse("33.34")?,
    Eur::parse("33.33")?,
    Eur::parse("33.33")?,
]);
assert_eq!(Eur::checked_sum(parts.iter().copied())?, Eur::parse("100.00")?);

// Proportional split, by weight.
let parts = Eur::parse("10.00")?.allocate(&[1, 4])?;
assert_eq!(parts, vec![Eur::parse("2.00")?, Eur::parse("8.00")?]);
```

`allocate` uses the **largest-remainder** method: leftover minor units go to the
parts with the largest fractional remainder, and ties break toward the lowest
index. The result is therefore a deterministic function of the inputs — the same
split always produces the same parts, which is what lets an allocation be
replayed or reproduced in a test.

Negative totals work the same way. The magnitude is split and the sign restored,
so truncation always rounds toward zero regardless of sign and the parts still
re-sum exactly.

## On the wire

With the `serde` feature, money serialises as a **decimal string**, never as a
float and never as the raw scaled integer:

```rust
assert_eq!(serde_json::to_string(&Eur::parse("12.34")?)?, "\"12.34\"");
assert!(serde_json::from_str::<Eur>("1234").is_err());
```

The raw integer is meaningless without knowing `P`, so a consumer reading it at
the wrong scale would silently misread every amount by a factor of ten.
