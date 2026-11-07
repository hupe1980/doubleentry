+++
title = "Balances and trial balances"
description = "BalanceQuery, balance and trial_balance on either façade, and how reports render."
weight = 1
+++

## What to call

- [`BalanceQuery`](https://docs.rs/doubleentry/latest/doubleentry/struct.BalanceQuery.html): the question every read answers.
- [`balance`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.balance)`(account, currency, &query)` →
  [`Balance`](https://docs.rs/doubleentry/latest/doubleentry/struct.Balance.html);
  [`trial_balance`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.trial_balance)`(&query)` →
  [`TrialBalance`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.TrialBalance.html).
  The same on [`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.balance), `async`.
- A bad question: [`Error::Query`](https://docs.rs/doubleentry/latest/doubleentry/enum.Error.html#variant.Query)
  with a [`QueryError`](https://docs.rs/doubleentry/latest/doubleentry/error/enum.QueryError.html).

## One question: which postings count

Every read (balance, trial balance, statement, rollup, open items, assertion)
takes a `BalanceQuery`: one **window** plus optional **modifiers**.

| Window | Selects the postings |
|---|---|
| `all()` | all |
| `through_position(p)` | of the records at positions `0..=p` |
| `through(date)` | dated on or before `date` |
| `between(start, end)` | dated from `start` to `end`, inclusive |
| `period(id)` | dated within the defined period `id` |

| Modifier | Effect |
|---|---|
| `.by_value_date()` | date windows read the value date |
| `.matching(axis, value)` / `.missing(axis)` | [dimension](@/docs/reporting/dimensions.md) filters, conjunctive |
| `.of_kind(kind)` / `.without_kind()` | the entry's kind (`Entry::kind`): `of_kind` repeated is any of the kinds; `without_kind` keeps entries recorded without one; both match nothing |
| `.pending()` | the Pending layer (open [holds](@/docs/concepts/holds.md)) instead of Settled |
| `.pinned(head)` | evaluate against the log prefix of `head` |

The builder never fails; names are checked when the query runs. Date windows
select by date, not by log position: an entry recorded late with an earlier date
counts on its date.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=4)?;
let march = books.record(
    Entry::new("sale-1", date!("2026-03-31"))
        .value_date(date!("2026-04-02")) // the bank credits it in April
        .debit("Assets:Bank", amount!("100.00"), EUR)
        .credit("Income:Sales", amount!("100.00"), EUR),
)?;
books.record(
    Entry::new("sale-2", date!("2026-04-10"))
        .debit("Assets:Bank", amount!("50.00"), EUR)
        .credit("Income:Sales", amount!("50.00"), EUR),
)?;
let bank = |q: &BalanceQuery| books.balance("Assets:Bank", EUR, q).map(|b| b.net());

assert_eq!(bank(&BalanceQuery::all())?, amount!("150.00"));
assert_eq!(bank(&BalanceQuery::through_position(march.position))?, amount!("100.00"));
assert_eq!(bank(&BalanceQuery::through(date!("2026-03-31")))?, amount!("100.00"));
assert_eq!(bank(&BalanceQuery::through(date!("2026-03-31")).by_value_date())?, amount!("0.00"));
assert_eq!(bank(&BalanceQuery::period("2026-04"))?, amount!("50.00"));

let sales = books.balance("Income:Sales", EUR, &BalanceQuery::all())?;
assert_eq!((sales.debit(), sales.credit(), sales.postings()), (amount!("0.00"), amount!("150.00"), 2));
assert_eq!(sales.to_string(), "dr 0.00 cr 150.00 net -150.00 (2 postings)");
```

An entry's kind (`invoice`, `payment`, `credit_note`, …) is a property of the
whole entry, so per-kind totals need no dimension mirrored onto a posting:

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 1..=12)?;
books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
let book = |key: &str, kind: &str, eur| {
    Entry::new(key, date!("2026-05-04")).kind(kind).debit("Assets:Receivables", eur, EUR).credit("Income:Sales", eur, EUR)
};
books.record(book("INV-1", "invoice", amount!("300.00")))?;
books.record(book("INV-2", "invoice", amount!("200.00")))?;
books.record(book("FEE-1", "dunning_fee", amount!("5.00")))?;

let year = BalanceQuery::between(date!("2026-01-01"), date!("2026-12-31"));
let sales = |q: BalanceQuery| books.balance("Income:Sales", EUR, &q).map(|b| b.credit());
assert_eq!(sales(year.clone().of_kind("invoice"))?, amount!("500.00"));
assert_eq!(sales(year.clone().of_kind("invoice").of_kind("dunning_fee"))?, amount!("505.00"));
assert_eq!(sales(year.without_kind())?, amount!("0.00"));
```

A `Balance` is **gross**: total debits, total credits, posting count. `net()`
is debit minus credit, so a credit balance is negative. A reversal adds to both
sides; gross figures only grow.

## The trial balance

One row per (account, currency) with a selected posting, in (path, currency)
order, each with the account's handle, path, kind, class, currency and gross
`balance`. `rows()`, `get(&path, currency)`, `totals(currency)`,
`is_balanced()`, `is_filtered()`, `layer()`, `basis()`, `currencies()` and
`exponent(currency)` read it. Without a dimension filter it always balances; a
filtered one is a slice and may not.

`Display` prints an aligned table at **each currency's declared exponent**:

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("setup");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.declare_currency(JPY, 0, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets:Bank", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;
books.record(
    Entry::new("sale-2", date!("2026-03-16"))
        .debit("Assets:Bank", amount!("15000"), JPY)
        .credit("Income:Sales", amount!("15000"), JPY),
)?;
let trial = books.trial_balance(&BalanceQuery::period("2026-03"))?;
assert!(trial.is_balanced() && !trial.is_filtered());
assert_eq!(trial.totals(JPY)?.debit(), amount!("15000"));
assert_eq!(
    trial.to_string(),
    concat!(
        "trial balance, settled, by booking date\n",
        "account       currency    debit   credit\n",
        "Assets:Bank   EUR       1190.00     0.00\n",
        "Assets:Bank   JPY         15000        0\n",
        "Income:Sales  EUR          0.00  1190.00\n",
        "Income:Sales  JPY             0    15000\n",
        "total         EUR       1190.00  1190.00\n",
        "total         JPY         15000    15000\n",
    ),
);
```

Report types align their own columns. One-line types (`Amount`, `Balance`,
`Recorded`, `StatementLine`, assertion outcomes) honour width, fill and
alignment, so `{:>12}` lines them up.

## Pinned reads

Every read evaluates **one log prefix**: the head when the read starts, or the
one you pin. Pinning to a signed checkpoint's head reproduces what the books
said at that size. The store's root at that size must equal the head's root, or
the read fails with `Error::Integrity`.

```rust
use doubleentry::prelude::*;
use doubleentry::store::MemoryStore;

let ledger = Ledger::new(MemoryStore::new(), LedgerId::new("example.com/acme")?).await?.as_actor("setup");
ledger.declare_currency(EUR, 2, date!("2026-01-01")).await?;
ledger.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01")).await?;
ledger.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
ledger.define_months(2026, 3..=3).await?;
let sale = |key: &str| {
    Entry::new(key, date!("2026-03-15"))
        .debit("Assets:Bank", amount!("10.00"), EUR)
        .credit("Income:Sales", amount!("10.00"), EUR)
};
ledger.record(sale("sale-1")).await?;
let published = ledger.head().await?; // e.g. the head of a signed checkpoint
ledger.record(sale("sale-2")).await?;

let then = ledger.trial_balance(&BalanceQuery::all().pinned(published)).await?;
assert_eq!(then.totals(EUR)?.debit(), amount!("10.00"));
// Every store answers exactly as the in-memory Journal.
let now = ledger.trial_balance(&BalanceQuery::all()).await?;
assert_eq!(now, ledger.to_journal().await?.trial_balance(&BalanceQuery::all())?);
```

A store answers from maintained balances and indexed postings, never from an
SQL sum of money; the conformance suite checks every query form against the
`Journal`.

## Refused questions

| `QueryError` | When |
|---|---|
| `PositionBeyondHead` | `through_position` or a pinned head beyond the log |
| `InvalidWindow` | `between` with a start after the end |
| `UnknownPeriod` | `period` with an id no period has |
| `InvalidLabel` | a period id or dimension name that is not a valid label |
| `UnknownAccount` | an account not registered in the prefix read |

## Limits

- A trial balance is not a balance sheet or a P&L: no sign per account kind, no
  grouping. See [Rollup](@/docs/reporting/rollup.md) for subtotals.
- No currency conversion.
- A wide date window over a large store costs time proportional to the postings
  it reads; an unfiltered `all()` balance comes from the maintained balance.
