+++
title = "Accounts"
description = "Account paths, kinds and classes, open windows, limits, and master data as dated log records."
weight = 2
+++

## What to call

[`AccountPath`](https://docs.rs/doubleentry/latest/doubleentry/struct.AccountPath.html),
[`AccountKind`](https://docs.rs/doubleentry/latest/doubleentry/enum.AccountKind.html),
[`BalanceLimit`](https://docs.rs/doubleentry/latest/doubleentry/enum.BalanceLimit.html).
Open with `open`, or `register` an
[`AccountSpec`](https://docs.rs/doubleentry/latest/doubleentry/struct.AccountSpec.html)
(class, last open day, limit). Change with `set_limit`, `set_class`,
`close_account`, `reopen_account`. Read with `account` →
[`Account`](https://docs.rs/doubleentry/latest/doubleentry/struct.Account.html).

## Paths

An account is named by its place in a tree: `Assets:Bank:Checking`, 1 to 16
segments. Each segment is a
[`Label`](https://docs.rs/doubleentry/latest/doubleentry/struct.Label.html):
1–64 characters in NFC, no whitespace at either edge (refused, not trimmed), no
control or invisible character, no `:`. Two names that print alike are equal.

```rust
use doubleentry::AccountPath;
use doubleentry::error::ValueError;

let checking: AccountPath = "Assets:Bank:Checking".parse()?;
assert_eq!(checking.depth(), 3);
assert!("Assets:Bank".parse::<AccountPath>()?.is_ancestor_of(&checking));
assert_eq!("Caf\u{65}\u{301}:Till".parse::<AccountPath>()?, "Caf\u{e9}:Till".parse()?); // NFC
assert_eq!("Assets::Cash".parse::<AccountPath>(), Err(ValueError::Empty));
assert_eq!("Assets: Cash".parse::<AccountPath>(), Err(ValueError::EdgeWhitespace));
assert!(matches!("Assets:Ca\u{200b}sh".parse::<AccountPath>(), Err(ValueError::InvisibleCharacter { .. })));
```

The façades take paths as text; a malformed one is refused by the call with
code 103.

## Kind, class, handle

The **kind** is fixed at registration: `Asset` and `Expense` are debit-normal,
`Liability`, `Equity` and `Income` credit-normal. It drives the year-end close
and is reported with every account; it does not change the arithmetic
([Debits and credits](@/docs/concepts/debits-and-credits.md)).

The **class** is an optional label of your own (`customer`, `HGB:B.II.4`),
dated like all master data and carried into the export. It is frozen once the
account has a posting (422).

The **handle** is the count of registrations before it, starting at 0,
committed in the register record. Drafts and calls take a path or a handle.

## Rules

| Code | Refused because |
|---|---|
| 420 | the path is registered with other master data (so the kind stays fixed) |
| 303 | a posting goes to a grouping node: only **leaves** take postings |
| 421 | a sub-account is opened below an account with postings |
| 304 | a posting is dated outside the account's open window |
| 423 | `close_account` would end the window before existing postings |
| 501 | an entry leaves a limited account beyond its limit **and** worse off than before |

Opening the same account again with the same master data returns its handle and
writes nothing. Grouping nodes need not be registered at all — reports
[roll up](@/docs/reporting/rollup.md) by path.

A **balance limit** forbids a net balance on one side: `NoDebitBalance` (a
customer wallet must not be overdrawn) or `NoCreditBalance`. The latest limit
applies whatever the booking date, and outstanding
[holds](@/docs/concepts/holds.md) count against it. Because only a worsening is
refused, a repayment into an account over a tightened limit still passes.

```rust
use doubleentry::prelude::*;
use doubleentry::AccountSpec;

let start = date!("2026-01-01");
let mut books = Journal::new(LedgerId::new("example.com/wallets")?).as_actor("onboarding");
books.declare_currency(EUR, 2, start)?;
books.define_months(2026, 1..=12)?;
let bank = books.open("Assets:Bank", AccountKind::Asset, start)?;
let alice = books.register(
    AccountSpec::new("Liabilities:Wallets:alice", AccountKind::Liability, start)
        .class("customer")
        .limit(BalanceLimit::NoDebitBalance),
)?;
assert_eq!((bank.get(), alice.get()), (0, 1));
assert_eq!(books.open("Assets:Bank", AccountKind::Asset, start)?, bank); // a repeat writes nothing
let other_kind = books.open("Assets:Bank", AccountKind::Liability, start);
assert_eq!(other_kind.err().and_then(|e| e.code()), Some(420));

let pay = |key: &str, eur| {
    Entry::new(key, date!("2026-03-02"))
        .debit(alice, eur, EUR) // by handle
        .credit("Assets:Bank", eur, EUR) // by path
};
books.record(
    Entry::new("top-up", date!("2026-03-01"))
        .debit("Assets:Bank", amount!("100.00"), EUR)
        .credit(alice, amount!("100.00"), EUR),
)?;
books.record(pay("pay-1", amount!("60.00")))?;
assert_eq!(books.record(pay("pay-2", amount!("60.00"))).err().and_then(|e| e.code()), Some(501));

let below = books.open("Assets:Bank:Sub", AccountKind::Asset, start);
assert_eq!(below.err().and_then(|e| e.code()), Some(421)); // Assets:Bank has postings
let reclass = books.set_class(alice, Some("vip"), date!("2026-03-10"));
assert_eq!(reclass.err().and_then(|e| e.code()), Some(422));
```

## Master data is dated records

Every account change is a record in the log with an **effective date** and the
**actor** from `as_actor`; currency declarations and the policy are records of
the same kind. An entry is checked against the master data in force on its
booking date. History is never rewritten: an account field's effective dates
never go backwards (310), and nothing may take effect on or before the
**watermark**, the last day of the latest sealed period (309).

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).as_actor("controller");
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 1..=12)?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Expenses:Travel", AccountKind::Expense, date!("2026-01-01"))?;
books.seal_through("2026-02")?;

let backdated = books.set_limit("Assets:Cash", BalanceLimit::NoCreditBalance, date!("2026-02-15"));
assert_eq!(backdated.err().and_then(|e| e.code()), Some(309));

books.close_account("Expenses:Travel", date!("2026-03-31"))?;
let april = books.record(
    Entry::new("trip-9", date!("2026-04-02"))
        .debit("Expenses:Travel", amount!("80.00"), EUR)
        .credit("Assets:Cash", amount!("80.00"), EUR),
);
assert_eq!(april.err().and_then(|e| e.code()), Some(304));
```

## Limits

- No chart of accounts ships with the crate; the class is opaque, no taxonomy
  is checked.
- The kind does not restrict sides: a liability may carry a debit balance
  unless a limit forbids it.
- A limit is checked on entries, not on the master-data change that tightens
  it.
