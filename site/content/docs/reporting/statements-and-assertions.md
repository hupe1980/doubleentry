+++
title = "Statements and assertions"
description = "Account statements with an opening balance and lines in date order, and balance assertions that report expected, actual and the difference."
weight = 2
+++

## What to call

- [`statement`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.statement)`(account, currency, &query)` →
  [`Statement`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.Statement.html) of
  [`StatementLine`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.StatementLine.html)s.
- [`check_assertion`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.check_assertion) /
  [`check_assertions`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.check_assertions) of a
  [`BalanceAssertion`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.BalanceAssertion.html) at an
  [`AssertAt`](https://docs.rs/doubleentry/latest/doubleentry/report/enum.AssertAt.html) →
  [`AssertionOutcome`](https://docs.rs/doubleentry/latest/doubleentry/report/enum.AssertionOutcome.html).
- The same on [`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.statement), `async`.

## Statements

- `opening`: the selected postings dated **before** the window (zero for a
  window without a start);
- `lines`: those dated **within** it, ordered by (date, position, posting
  index), each with its running balance;
- `closing`: `opening` plus every line.

The account can be named by path or by handle.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 2..=4)?;
let sale = |key: &str, on, amount| {
    Entry::new(key, on).debit("Assets:Bank", amount, EUR).credit("Income:Sales", amount, EUR)
};
books.record(sale("feb", date!("2026-02-10"), amount!("1000.00")))?;
books.record(sale("mar-late", date!("2026-03-20"), amount!("40.00")))?;
books.record(sale("mar-early", date!("2026-03-05"), amount!("100.00")))?; // recorded later, dated earlier
books.record(sale("mar-end", date!("2026-03-31"), amount!("1.00")).value_date(date!("2026-04-02")))?;

let march = books.statement("Assets:Bank", EUR, &BalanceQuery::period("2026-03"))?;
assert_eq!(march.opening.net(), amount!("1000.00"));
let days: Vec<_> = march.lines.iter().map(|line| line.booking_date).collect();
assert_eq!(days, [date!("2026-03-05"), date!("2026-03-20"), date!("2026-03-31")]); // by date, not position
assert_eq!(march.lines[0].running.net(), amount!("1100.00"));
assert_eq!(march.closing.net(), amount!("1141.00"));
assert_eq!(
    march.to_string(),
    concat!(
        "Assets:Bank EUR, settled, by booking date, 2026-03-01 to 2026-03-31\n",
        "opening                          1000.00\n",
        "2026-03-05   9/0  debit  100.00  1100.00\n",
        "2026-03-20   8/0  debit   40.00  1140.00\n",
        "2026-03-31  10/0  debit    1.00  1141.00\n",
        "closing                          1141.00\n",
    ),
);

// By value date, the March 31 entry moves into April, as on a bank statement.
let april = books.statement("Assets:Bank", EUR, &BalanceQuery::period("2026-04").by_value_date())?;
assert_eq!(april.lines.len(), 1);
```

`Display` prints a heading, the opening, one row per line (date,
position/posting index, side, amount, running balance) and the closing, at the
currency's declared exponent.

## Balance assertions

An assertion is a claim from outside: "the bank says this account held 1000.00
at the end of 31 March". It names an account, a currency, a point and the
expected **net** (debit positive), about the Settled layer (`.pending()` for
Pending):

| `AssertAt` | The balance |
|---|---|
| `EndOfDay { date, basis }` | of every posting dated on or before `date`, by booking or value date |
| `Position(p)` | after the record at log position `p` |

The result is `Held { actual }` or `Failed(failure)` with expected, actual and a
`Difference`: a direction (`Excess` or `Shortfall`) and a `u128` magnitude in
units of 10<sup>−P</sup>, which cannot overflow. A failed claim is an outcome,
not an error.

```rust
use doubleentry::prelude::*;
use doubleentry::report::{AssertAt, AssertionOutcome, BalanceAssertion, DateBasis, Deviation};

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.record(
    Entry::new("sale-1", date!("2026-03-31"))
        .debit("Assets:Bank", amount!("990.00"), EUR)
        .credit("Income:Sales", amount!("990.00"), EUR),
)?;

let end_of_march = AssertAt::EndOfDay { date: date!("2026-03-31"), basis: DateBasis::Value };
let outcomes = books.check_assertions(&[
    BalanceAssertion::new("Assets:Bank", EUR, end_of_march, amount!("990.00")),
    BalanceAssertion::new("Assets:Bank", EUR, end_of_march, amount!("1000.00")),
])?;
assert!(outcomes[0].is_held());
let AssertionOutcome::Failed(failure) = &outcomes[1] else { panic!("the bank says 1000.00") };
assert_eq!((failure.difference.direction, failure.difference.units), (Deviation::Shortfall, 1000));
assert_eq!(
    failure.to_string(),
    "Assets:Bank EUR settled at the end of 2026-03-31 by value date: expected 1000.00, actual 990.00, shortfall 10.00",
);
```

An assertion fails with `Error::Query` where a balance read would: an
unregistered account (`UnknownAccount`), or `AssertAt::Position` beyond the head
(`PositionBeyondHead`).

## Limits

- A statement covers one account, one currency and one layer; for subtree totals
  use a [rollup](@/docs/reporting/rollup.md).
- An assertion is not stored in the ledger and does not block later entries.
