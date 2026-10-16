+++
title = "Debits and credits"
description = "Sides, gross balances that only grow, and the derived net."
weight = 4
+++

## What to call

[`Side`](https://docs.rs/doubleentry/latest/doubleentry/enum.Side.html) (`Debit`, `Credit`);
[`Balance`](https://docs.rs/doubleentry/latest/doubleentry/struct.Balance.html) with
`debit()`, `credit()`, `postings()`, `net()`;
[`AccountKind::normal_side`](https://docs.rs/doubleentry/latest/doubleentry/enum.AccountKind.html#method.normal_side).

## A side, never a sign

Every posting has a side and a **positive** amount; zero or negative is refused
(104). An entry balances **per (currency, layer)**: for each currency and each
layer (Settled, Pending), debits equal credits, and a refusal names the pair
that does not (106).

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.declare_currency(USD, 2, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Equity:Capital", AccountKind::Equity, date!("2026-01-01"))?;

books.record(
    Entry::new("capital", date!("2026-03-01"))
        .debit("Assets:Bank", amount!("1000.00"), EUR)
        .credit("Equity:Capital", amount!("1000.00"), EUR)
        .debit("Assets:Bank", amount!("500.00"), USD)
        .credit("Equity:Capital", amount!("500.00"), USD),
)?; // two currencies, each balanced on its own

let mixed = books.record(
    Entry::new("mixed", date!("2026-03-02"))
        .debit("Assets:Bank", amount!("10.00"), EUR)
        .credit("Equity:Capital", amount!("10.00"), USD),
);
assert_eq!(
    mixed.err().map(|e| e.to_string()).as_deref(),
    Some("rejected: [106] EUR Settled does not balance: debits 10.00, credits 0.00"),
);
```

## Gross balances that only grow

A `Balance` is the T-account of one (account, currency, layer): gross debits,
gross credits and the posting count. A posting only ever adds, so a balance
never shrinks; `net()` = debit − credit is derived. Two balances are equal only
if all three parts are; `nets_equal` compares nets alone. A reversal shows why:
the net returns to zero, both totals grow, and the count records two movements.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;

let sale = books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("50.00"), EUR)
        .credit("Income:Sales", amount!("50.00"), EUR),
)?;
books.record(books.reversal(sale.id, "sale-1-reversal", date!("2026-03-16"))?)?;

let cash = books.balance("Assets:Cash", EUR, &BalanceQuery::all())?;
assert!(cash.net().is_zero());
assert_eq!(cash.to_string(), "dr 50.00 cr 50.00 net 0.00 (2 postings)");
```

## The sign of `net()`

`net()` is debit − credit for **every** kind, so a credit-normal account
(liability, equity, income) normally shows a negative net — `Income:Sales`
above would read `-50.00` before the reversal. A report that prints those as
positive negates the net when `kind.normal_side()` is `Side::Credit`.

## Why

- **A side cannot be misread.** "−50" depends on whose view the sign was taken
  from; "debit 50" does not.
- **Gross keeps what net loses**: turnover, the number of movements, that a
  reversal happened. Gross totals are monotone, so a verifier can check that a
  seal's totals never decrease from one seal to the next.
- **One formula** keeps a trial balance summing to zero per currency.

## Limits

- `net()` is a view; seals and balance proofs commit to the gross triple.
- An entry that would overflow a gross total is refused (500); sums across
  accounts are checked and reported as an error, never wrapped.
