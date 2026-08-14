+++
title = "Closing entries"
description = "Flattening income and expense into equity at year end, as ordinary postings that still go through validation."
weight = 13
+++

## What closing does

Closing zeroes the accounts whose balances belong to one period only — typically
income and expense — by posting their opposite and moving the net to equity.

It is bookkeeping mechanics rather than reporting, so it belongs in the engine.
*Which* equity account receives the result is a chart-of-accounts decision and
stays with you.

```rust
use doubleentry::{closing_postings, AccountKind, Layer};

let closing = journal.trial_balance_through_date(date!(2026-12-31))?;

let postings = closing_postings(
    &closing,
    journal.accounts(),
    &[AccountKind::Income, AccountKind::Expense],
    retained_earnings,
    Layer::Settled,
)?;
```

## The result is postings, not an entry

`closing_postings` returns a `Vec<Posting<P>>`. You assemble them into a draft and
seal it like anything else, which is where the period, the accounts and the
balance invariant get checked:

```rust
let mut draft = Entry::new(EntryId::generate(), key, date!(2026-12-31))
    .with_kind(Label::new("year-end-close")?);

for posting in postings {
    draft = draft.post(posting);
}

journal.record(draft)?;
```

Returning postings rather than a finished entry keeps the identity, the
idempotency key, the kind and the provenance where they belong: with you. A
closing entry is still an entry someone booked, and the audit trail should say
who.

## What it produces

One posting per account with a non-zero net, on the side that flattens it,
followed by one balancing posting on the equity account **per currency**.

The postings balance per currency by construction, so sealing succeeds unless
something else about the draft is wrong.

Accounts that already net to zero are skipped — a posting of zero carries no
information and validation would reject it anyway. So running a second close over
an already-closed period returns an empty vector rather than failing.

## Rules it enforces

Only two, and both prevent a result that would be silently wrong:

- **The equity account must be registered.** Otherwise the balancing leg names
  nothing.
- **The equity account must not itself be in scope for closing.** If it were,
  the result would depend on the order accounts happened to be visited — the
  equity leg would be closed by a later iteration of the same pass.

Accounts with no `AccountKind` are skipped, since there is nothing to select them
by. Closing is opt-in on the classification you supply.

## Layers

Closing operates on one layer at a time. Pending reservations are not income or
expense that has been earned or incurred — they are amounts still reserved — so
flattening them into equity would recognise a result that has not happened.
