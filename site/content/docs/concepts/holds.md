+++
title = "Holds"
description = "Two-phase transfers: open a hold, then post, void or expire it exactly once; outstanding holds count against balance limits."
weight = 8
+++

## What to call

Drafts: [`Entry::open_hold`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.open_hold)
(with `expires`), `Entry::post_hold` with a
[`PostAmount`](https://docs.rs/doubleentry/latest/doubleentry/enum.PostAmount.html),
`Entry::void_hold`, `Entry::expire_hold` — each recorded with `record`. Reads:
`hold` → [`HoldState`](https://docs.rs/doubleentry/latest/doubleentry/struct.HoldState.html),
`holds_due`, and `BalanceQuery::pending`.

## A reservation, ended once

A hold — a card authorisation, a wallet reservation — reserves an amount from
one account to another on the **Pending** layer. Exactly one resolution ends it:

| Resolution | Effect |
|---|---|
| `post_hold(key, on, hold, PostAmount::Full)` or `Partial(amount)` | releases the whole reservation and settles the posted amount between the hold's two accounts; the rest of a partial post is released |
| `void_hold(key, on, hold)` | releases the whole reservation |
| `expire_hold(key, on, hold)` | like a void, booked on or after the expiry date |

A hold has one debit and one credit of the same amount and currency. A
resolution carries **no postings**: the rule core derives them and writes them
into its record, so readers see ordinary postings. The hold's id is its entry's
derived id, so a scheduler that knows the key knows the id.

```rust
use doubleentry::prelude::*;
use doubleentry::{HoldStatus, PostAmount};

let mut books = Journal::new(LedgerId::new("example.com/payments")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
books.define_months(2026, 11..=11)?;
let wallet = "Liabilities:Wallets:alice";

let hold = books.record(
    Entry::open_hold("auth-4711", date!("2026-11-20"))
        .expires(date!("2026-11-30"))
        .debit(wallet, amount!("100.00"), EUR)
        .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
)?;
assert_eq!(books.balance(wallet, EUR, &BalanceQuery::all().pending())?.net(), amount!("100.00"));

let capture = books.record(Entry::post_hold(
    "capture-4711",
    date!("2026-11-22"),
    hold.id,
    PostAmount::Partial(amount!("60.00")),
))?;
let state = books.hold(hold.id).ok_or("no hold")?;
assert_eq!(state.status, HoldStatus::Posted { by: capture.id, amount: amount!("60.00") });
assert!(books.balance(wallet, EUR, &BalanceQuery::all().pending())?.net().is_zero());
assert_eq!(books.balance(wallet, EUR, &BalanceQuery::all())?.net(), amount!("60.00"));

let void = books.record(Entry::void_hold("void-4711", date!("2026-11-23"), hold.id));
assert_eq!(void.unwrap_err().code(), Some(442)); // resolved once
```

## The rules

| Code | Rule |
|---|---|
| 140 | a hold has exactly one debit and one credit |
| 141 | its expiry date lies after its booking date |
| 142 | a resolution carries no postings |
| 143 | a Pending posting outside a hold is refused |
| 145 | a partial post is positive |
| 440 / 441 | the named entry exists and is a hold |
| 442 | it is not resolved yet; the rejection names the resolving entry |
| 443 | the resolution is not dated before the hold |
| 444 | a Post is booked **before** the expiry date |
| 445 / 446 | an Expire needs an expiry date and is booked on or after it |
| 447 / 448 | a partial post fits the currency's decimals (never rounded) and the held amount |
| 449 | a Post needs both accounts open; Void and Expire do not |

A Void may be booked any day from the hold's booking date on. Every resolution
needs an open period on its own date. Nothing expires by itself: the crate reads
no clock. A scheduler asks `holds_due(as_of, page)` for the open holds whose
expiry is on or before `as_of`, ordered by (expiry, position), and records an
expiry for each.

```rust
use doubleentry::prelude::*;
use doubleentry::PageRead;

let mut books = Journal::new(LedgerId::new("example.com/payments")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
books.define_months(2026, 11..=12)?;
let hold = books.record(
    Entry::open_hold("auth-4711", date!("2026-11-20"))
        .expires(date!("2026-11-30"))
        .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
        .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
)?;

let early = books.record(Entry::expire_hold("expire-4711", date!("2026-11-29"), hold.id));
assert_eq!(early.unwrap_err().code(), Some(446));
assert!(books.holds_due(date!("2026-11-29"), PageRead::first(100)).items.is_empty());

for state in books.holds_due(date!("2026-11-30"), PageRead::first(100)).items {
    books.record(Entry::expire_hold(format!("expire-{}", state.id), date!("2026-11-30"), state.id))?;
}
assert!(books.holds_due(date!("2026-12-31"), PageRead::first(100)).items.is_empty());
```

A page holds at most 1 000 items; when `page.next` is `Some(cursor)`, ask again
with `PageRead::first(limit).after(cursor)`.

## Holds and balance limits

Outstanding holds count against [balance limits](@/docs/concepts/accounts.md).
On a `NoDebitBalance` account the exposure is settled debits **plus outstanding
hold debits** minus settled credits (`NoCreditBalance` mirrors it). A hold that
would breach the limit is refused when opened (501); releases are never refused
by a limit.

```rust
use doubleentry::prelude::*;
use doubleentry::PostAmount;

let mut books = Journal::new(LedgerId::new("example.com/payments")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Liabilities:Wallets:alice", AccountKind::Liability, date!("2026-01-01"))?;
books.open("Liabilities:Card:clearing", AccountKind::Liability, date!("2026-01-01"))?;
books.set_limit("Liabilities:Wallets:alice", BalanceLimit::NoDebitBalance, date!("2026-01-01"))?;
books.define_months(2026, 11..=11)?;
books.record(
    Entry::new("top-up", date!("2026-11-02"))
        .debit("Assets:Bank", amount!("100.00"), EUR)
        .credit("Liabilities:Wallets:alice", amount!("100.00"), EUR),
)?;
let hold = |key: &str, amount| {
    Entry::open_hold(key, date!("2026-11-03"))
        .debit("Liabilities:Wallets:alice", amount, EUR)
        .credit("Liabilities:Card:clearing", amount, EUR)
};

let first = books.record(hold("auth-1", amount!("70.00")))?;
assert_eq!(books.record(hold("auth-2", amount!("40.00"))).unwrap_err().code(), Some(501));
// Capturing 50.00 releases the other 20.00 of headroom.
books.record(Entry::post_hold("capture-1", date!("2026-11-04"), first.id, PostAmount::Partial(amount!("50.00"))))?;
books.record(hold("auth-2", amount!("40.00")))?;
```

A hold open at a period's end stays open across the seal, which commits to it
in its Pending balance tree; the year-end close touches only the Settled layer.
`HoldState::derive` computes a hold's state from its hold and resolution records
alone. The full program is
[`examples/holds.rs`](https://github.com/hupe1980/doubleentry/blob/main/examples/holds.rs).

## Why

One resolution per hold (TigerBeetle's two-phase transfers) makes
over-release impossible by construction, and derived resolution postings cannot
be got wrong by the client. Counting holds against limits stops two
authorisations that each fit, but not together, from both being accepted.

## Limits

- No background expiry: a hold expires when you record `expire_hold`.
- One currency and one pair of accounts per hold; a Post settles between those
  two. Capture elsewhere with an ordinary entry after the post.
- A posted hold cannot be un-posted; a refund is a new entry.
