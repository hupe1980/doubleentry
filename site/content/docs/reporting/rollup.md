+++
title = "Rollup"
description = "A trial balance folded up the account-path tree: subtotals per path prefix, inferred grouping nodes, cut to any depth."
weight = 3
+++

## What to call

- [`rollup`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.rollup)`(currency, &query)` →
  [`Rollup`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Rollup.html) of
  [`RollupNode`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.RollupNode.html)s; on
  [`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.rollup), `async`.
- [`Rollup::of`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Rollup.html#method.of)`(&trial, currency)` folds a trial balance you already have.
- [`to_depth`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Rollup.html#method.to_depth),
  [`roots`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Rollup.html#method.roots),
  [`get`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Rollup.html#method.get),
  [`total`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Rollup.html#method.total).

## Subtotals up the path tree

A rollup adds every trial-balance row of one currency into each prefix of its
path. Each node has `own` (posted to the path itself) and `subtree` (the path and
everything beneath). Nodes come in pre-order, siblings by path. A prefix that
is not a registered account is an **inferred grouping node** (`account` and
`kind` are `None`, `is_registered()` is false): no need to open `Assets` to get an
`Assets` subtotal.

```rust
use doubleentry::prelude::*;
use doubleentry::report::Rollup;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
for path in ["Assets:Bank:Main", "Assets:Bank:Savings", "Assets:Cash"] {
    books.open(path, AccountKind::Asset, date!("2026-01-01"))?;
}
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets:Bank:Main", amount!("1000.00"), EUR)
        .debit("Assets:Bank:Savings", amount!("500.00"), EUR)
        .debit("Assets:Cash", amount!("190.00"), EUR)
        .credit("Income:Sales", amount!("1690.00"), EUR),
)?;

let query = BalanceQuery::period("2026-03");
let sheet = books.rollup(EUR, &query)?;
let bank = sheet.get(&"Assets:Bank".parse()?).ok_or("no node")?;
assert!(!bank.is_registered() && bank.own.is_zero());
assert_eq!(bank.subtree.net(), amount!("1500.00"));
assert_eq!(sheet.roots().map(|n| n.path.to_string()).collect::<Vec<_>>(), ["Assets", "Income"]);
assert_eq!(
    sheet.to_string(),
    concat!(
        "Assets        1690.00\n",
        "  Bank        1500.00\n",
        "    Main      1000.00\n",
        "    Savings    500.00\n",
        "  Cash         190.00\n",
        "Income       -1690.00\n",
        "  Sales      -1690.00\n",
    ),
);

// Cut to depth 1: shorter, not different.
let top = sheet.to_depth(1);
assert_eq!(top.to_string(), "Assets   1690.00\nIncome  -1690.00\n");
assert_eq!(top.total()?, sheet.total()?);

// A pure fold of the trial balance: same totals, same result from every store.
let trial = books.trial_balance(&query)?;
assert_eq!(Rollup::of(&trial, EUR)?, sheet);
assert_eq!(sheet.total()?, trial.totals(EUR)?);
```

`Display` indents the last path segment two spaces per level and right-aligns
the subtree's **net** (debit positive) at the currency's exponent. `to_depth(n)`
keeps depths `1..=n` (a root has depth 1) with full subtree totals; `to_depth(0)`
is empty. `rollup(currency, &query)` is `trial_balance(&query)` then
`Rollup::of`, so every query modifier applies. A subtotal that does not fit an
amount is `QueryError::Overflow`, naming the path.

[`examples/reporting.rs`](https://github.com/hupe1980/doubleentry/blob/main/examples/reporting.rs)
combines a dimension-filtered balance, a rollup and a statement.

## Limits

- One currency per rollup; no conversion.
- Only paths with selected postings, and their ancestors, appear.
- Nets are debit-positive for every kind; presenting income and liabilities
  with the usual sign is yours (`RollupNode::kind` tells the kind).
- Not a balance sheet or P&L layout.
