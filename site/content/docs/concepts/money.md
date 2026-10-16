+++
title = "Money"
description = "Amount<P>: exact i128 money, declared currency exponents, explicit rounding, exact splits."
weight = 1
+++

## What to call

[`Amount<P>`](https://docs.rs/doubleentry/latest/doubleentry/struct.Amount.html) with the
[`amount!`](https://docs.rs/doubleentry/latest/doubleentry/macro.amount.html) literal or `Amount::parse`;
`checked_*`, `rescale` with a [`Rounding`](https://docs.rs/doubleentry/latest/doubleentry/enum.Rounding.html),
`allocate`, `distribute`. Currencies:
[`Currency`](https://docs.rs/doubleentry/latest/doubleentry/struct.Currency.html),
[`currency!`](https://docs.rs/doubleentry/latest/doubleentry/macro.currency.html),
the constants in [`doubleentry::currency`](https://docs.rs/doubleentry/latest/doubleentry/currency/index.html),
`declare_currency`, `exponent`, `Amount::display_at`.

## Integers at a fixed scale

`Amount<P>` is an `i128` count of 10<sup>−P</sup> units; `Amount` is
`Amount<2>`. `P` is fixed at compile time, at most 18. The range is symmetric,
so `negate()` and `abs()` cannot overflow. `amount!` is checked by the
compiler; `Amount::parse` is strict — more decimals than `P` are refused, even
trailing zeros, never rounded.

```rust
use doubleentry::{Amount, amount};
use doubleentry::error::AmountError;

let price: Amount = amount!("1190.00");
assert_eq!(price.to_minor(), 119_000);
assert_eq!(Amount::<2>::parse("1190.5")?, amount!("1190.50"));
assert_eq!(Amount::<2>::parse("1190.000"), Err(AmountError::PrecisionLoss { scale: 2 }));
assert_eq!(Amount::<2>::parse("1,190.00"), Err(AmountError::InvalidLiteral));
assert_eq!(Amount::<2>::MIN, Amount::<2>::MAX.negate());
```

## Checked arithmetic, explicit rounding

`Amount` has no `+`, `-` or `*`. Whatever can overflow returns a `Result`;
whatever can produce a fraction of a minor unit takes a `Rounding` — `HalfUp`,
`HalfEven`, `HalfDown`, `TowardZero`, `AwayFromZero`, `Floor`, `Ceiling` —
and there is no default. `checked_mul_ratio(n, d, rounding)` multiplies exactly
and divides once. Summing an iterator collects into a `Result`.

```rust
use doubleentry::{Amount, Rounding, amount};
use doubleentry::error::AmountError;

let net: Amount = amount!("1000.00");
let vat = net.checked_mul_ratio(19, 100, Rounding::HalfUp)?;
assert_eq!(net.checked_add(vat)?, amount!("1190.00"));

let cent: Amount = amount!("0.01");
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::HalfUp)?, amount!("0.01"));
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::HalfEven)?, amount!("0.00"));

let unit_price: Amount<4> = amount!("2.3450");
assert_eq!(unit_price.rescale::<2>(Rounding::HalfEven)?, amount!("2.34")); // down rounds, up is exact

assert_eq!(Amount::<2>::MAX.checked_add(cent), Err(AmountError::Overflow));
let total: Result<Amount, AmountError> = [amount!("1.10"), amount!("2.20")].into_iter().sum();
assert_eq!(total?, amount!("3.30"));
```

## Splits that re-sum exactly

`allocate(&weights)` splits by the largest-remainder method: the parts add up
to the original, leftover minor units go to the largest remainders, ties to the
lowest index. `distribute(n)` is the equal-weights case. Both refuse more than
65 536 parts; empty or all-zero weights and zero parts are refused.

```rust
use doubleentry::{Amount, amount};

let rent: Amount = amount!("100.00");
assert_eq!(rent.allocate(&[1, 1, 1])?, [amount!("33.34"), amount!("33.33"), amount!("33.33")]);
assert_eq!(rent.allocate(&[2, 0, 1])?, [amount!("66.67"), amount!("0.00"), amount!("33.33")]);
let refund: Amount = amount!("-0.05");
assert_eq!(refund.distribute(3)?, [amount!("-0.02"), amount!("-0.02"), amount!("-0.01")]);
assert!(rent.allocate(&[0, 0]).is_err());
```

## Currencies are declared

A `Currency` is a code of 1–16 characters `A–Z`, `0–9`, with no registry and no
case folding. `doubleentry::currency` has 25 ISO 4217 constants (the prelude
brings `EUR`, `USD`, `GBP`, `CHF`, `JPY`); any other code is `Currency::new` or
`currency!("USDC")`. The code carries no decimals: the ledger **declares** each
currency with its exponent and an effective date, as a record of the log.

| Code | Refused because |
|---|---|
| 123 | the exponent exceeds the ledger's `P` |
| 305 | a posting's currency is not declared |
| 306 | a posting has more decimals than its currency's exponent |
| 424 | a declaration lowers an exponent (raising is allowed) |

```rust
use doubleentry::prelude::*;
use doubleentry::{currency, currency::BHD};

let mut books = Journal::<3>::with_precision(LedgerId::new("example.com/gulf")?);
books.declare_currency(BHD, 3, date!("2026-01-01"))?;
books.declare_currency(JPY, 0, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Equity:Capital", AccountKind::Equity, date!("2026-01-01"))?;

let fund = |key: &str, amount, currency| {
    Entry::new(key, date!("2026-03-02"))
        .debit("Assets:Bank", amount, currency)
        .credit("Equity:Capital", amount, currency)
};
books.record(fund("fund-bhd", amount!("1500.125"), BHD))?;
assert_eq!(books.record(fund("fund-jpy", amount!("1.500"), JPY)).err().and_then(|e| e.code()), Some(306));
assert_eq!(books.record(fund("fund-usd", amount!("1.000"), USD)).err().and_then(|e| e.code()), Some(305));
assert_eq!(books.declare_currency(BHD, 2, date!("2026-03-01")).err().and_then(|e| e.code()), Some(424));
assert_eq!(books.declare_currency(currency!("BTC"), 8, date!("2026-03-01")).err().and_then(|e| e.code()), Some(123));

let yen: Amount<3> = amount!("15000.000");
let exponent = books.exponent(JPY).ok_or("JPY is declared")?;
assert_eq!(yen.display_at(exponent).to_string(), "15000"); // reports print at the declared exponent
```

`Journal::<P>::with_precision(id)` (and `Ledger::<S, P>::with_precision`) picks
the scale; it is recorded in the genesis record and fixed for the ledger.

## Why

- **Integers, one scale.** A hashed record needs exactly one byte form per
  value: no floats, no run-time decimal scale.
- **`i128`.** Gross totals only grow; 128 bits leave room at every scale up to
  18, and every total is still checked.
- **Rounding is a decision.** VAT, interest and FX each have a written rule; a
  silent default is how books drift by a cent.

## Limits

- The crate converts nothing: a `Rate` on an entry is recorded evidence
  ([Entries](@/docs/concepts/entries.md)).
- `checked_mul_ratio` can overflow on the intermediate product even when the
  quotient would fit.
