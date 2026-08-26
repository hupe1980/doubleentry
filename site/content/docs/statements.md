+++
title = "Statements, checkpoints and assertions"
description = "Reading how an account got to where it is, caching that fold safely, and reconciling the ledger against an outside source."
weight = 11
+++

## Statements

A trial balance says where an account ended up and nothing about how it got
there. A statement is the other half: every posting touching one key, in log
order, with the running balance after each.

```rust
let lines = journal.statement(&cash_key, BalanceQuery::all())?;

for line in &lines {
    println!("{} {} {} → {}",
        line.booking_date, line.direction, line.amount, line.running.signed_net()?);
}
```

Each line carries the log index, a `PostingRef` back to the exact posting, the
booking date, the direction and amount, the running balance, and the owning
entry's `kind` — so a statement can be grouped or filtered by document type
without a second lookup per line.

Over a durable store, statements are **paged**. An account statement over ten
years is not a response body:

```rust
let page = store
    .statement(cash_key, BalanceQuery::all(), PostingCursor::start().with_limit(100))
    .await?;
for line in &page.lines { /* … */ }
let next = page.next;   // `None` at the end
```

### Scoping to a period

A statement takes the same [`BalanceQuery`](#historical-reads) every balance read
takes, so "this account, March, this reporting axis" is one query:

```rust
let march = BalanceQuery::between(date!(2026-03-01), date!(2026-03-31));
let page  = store.statement(cash_key, march, PostingCursor::start()).await?;

assert_eq!(page.opening, february_closing);
```

`page.opening` is the balance the account **carried into** the page. It is two
disjoint folds added together:

1. **What came before the window** — everything the query narrows to that was
   booked *before* its start date. That is what makes it an opening balance
   rather than zero. Every other narrowing still applies: an opening balance for
   one reporting axis is over that axis.
2. **What the earlier pages showed** — the statement's own lines up to the cursor
   this page resumed from.

So `opening` plus the page's movements is the last line's `running`, and a window
with no activity reports the carried balance as both its opening and its closing.

### The first fold is by date, not by log position

Entries are appended in **recording** order, which is not booking-date order — a
backdated invoice arrives after the entries it belongs before. Bound the carried
balance by log position and two things break at once: an entry booked in April
but recorded before the last March movement lands in the middle of a March
statement, and the February entry that arrived after it is left out of the
opening. Both are ordinary bookings.

`BalanceQuery::opening()` is what the carry set is folded over, and it is defined
by date — so the answer does not depend on the order entries arrived in. The
conformance suite books its fixture out of date order for this reason.

### Lines are in ledger order

A statement lists postings in **log order** — the order they were recorded. That
is the order a `PostingCursor` pages by and the only total order that stays
stable as the journal grows, which is what paging needs. It is an audit trail
rather than a presentation: sort a page by `booking_date` or `value_date` if that
is what a reader wants to see.

### The cursor addresses a posting, not an entry

`PostingCursor` is deliberately not the `Cursor` that pages the log. A log page
is a list of entries; a statement is a list of **postings**, and one entry may
put several on the same account — a split receipt booked as three lines against
one credit is an ordinary entry.

So a page boundary can fall *inside* an entry, and a cursor naming only an entry
has no way to say where: resuming from one asks for `log_index > after` and skips
whatever remained of the entry the page ended in.

`PostingPosition` is the pair — the entry's log index and the posting's index
within it — and `StatementLine::position()` hands you the one to resume after.
[Open items](@/docs/open-items.md) page the same way, behind the same cursor:
they are the filtered view of the same postings.

Each page opens where the previous one closed, because the second half of
`opening` is exactly the lines the earlier pages showed. Paging a statement one
line at a time reproduces the running balances the whole statement gave, figure
for figure.

## Two dates, and which one a report means

An entry carries a **booking date** — when it went into the books — and a **value
date** — when the money is treated as having moved. A date-bounded query folds by
the booking date unless you say otherwise:

```rust
let march = BalanceQuery::between(date!(2026-03-01), date!(2026-03-31));

let in_the_books = journal.balance(&cash_key, march)?;
let in_the_cash  = journal.balance(&cash_key, march.by_value_date())?;
```

The difference is the entries booked in one month and settling in the next, which
at a period boundary is most of them. An invoice booked on 28 March with value
2 April belongs to March's books and April's cash; both answers are right about
different questions.

| You are reconciling against | Fold by |
|---|---|
| a trial balance, a period close, a prior-year comparative | booking date — the books are kept on it |
| a bank statement, a custodian position, a counterparty confirmation | value date — nobody outside your ledger speaks booking dates |

The basis is a **reporting** choice. It never moves an entry between periods,
never affects which period may still be posted to, and never changes what a
[seal](@/docs/periods-and-seals.md) commits to — otherwise a settlement
instruction could reopen sealed books.

A statement takes the basis too, opening balance included, and every line reports
both dates, so a value-dated statement still says which period each movement was
booked in.

## Checkpoints

A balance is *defined* as a fold over the journal — correct, and linear in
history. A checkpoint records that fold up to a point so later reads start from
there.

Because that trades a definition for a cache, a checkpoint is only safe if it can
be re-derived. So it carries the tree head it was taken against:

```rust
let cp = journal.checkpoint(&cash_key)?;
assert_eq!(cp.size(), journal.len() as u64);
journal.verify_checkpoint(&cp)?;    // re-derives and compares
```

The head does double duty: it **names the prefix** the balance covers, and it
**pins the history** that prefix belongs to. There is deliberately no separate
position field — two fields that must agree are two fields that can disagree.

`verify_checkpoint` checks three things independently: that the prefix is within
the log, that the tree head matches the log *at that size*, and that a fold over
exactly that prefix reproduces the balance. The head check is what makes a stale
checkpoint detectable — one that matches numerically but was taken against a
different history is stale by construction, not by convention, and silently
trusting it would carry a wrong balance forward.

A checkpoint stays valid as the log grows, because it describes a real prefix and
its pinned head lets it be re-derived exactly. That includes the empty one: "this
account had not moved after zero entries" is a true statement no later append can
falsify.

## Balance assertions

A checkpoint is an optimisation. An assertion is the opposite: a claim from
*outside* the ledger — a bank statement, a counterparty confirmation, an ERP
export — checked against the fold. It catches the divergence that reconciliation
exists to find, and it is the cheapest such mechanism there is.

```rust
use doubleentry::BalanceAssertion;

let claim   = BalanceAssertion::net(cash_key, Eur::parse("3500.00")?);
let outcome = journal.check_assertion(&claim)?;

if !outcome.held() {
    // "failed: expected 3500.00, found 3450.00, off by -50.00"
    eprintln!("{outcome}");
}
```

The expected value is a **signed net**, debit positive, because that is the form
an external source reports: a statement says what the balance is, not how much
moved in each direction. `BalanceAssertion::on_side` converts a magnitude on a
named side into that form.

A failed assertion reports the difference, not just the fact of failure — the
difference is the number you go looking for.

### Where to evaluate it

An assertion names the state of the books it is about, and the two forms answer
genuinely different questions:

```rust
// After the first 1 204 entries. Exact, reproducible, and meaningful only
// inside this system.
let positional = BalanceAssertion::net(cash_key, expected).over_prefix(1_204);

// Everything booked on or before 31 March, whenever it was recorded.
let dated = BalanceAssertion::net(cash_key, expected).on_date(date!(2026-03-31));

// Everything *valued* on or before 31 March — what a bank statement says.
let cash = BalanceAssertion::net(cash_key, expected).on_value_date(date!(2026-03-31));
```

Use a **dated** form to reconcile against anything external. A bank statement
says "as at 31 March", not "after 4 812 entries", and folding by date is what
puts a late-arriving backdated entry in the period it economically belongs to.
`journal.balance(&key, BalanceQuery::through(date))` answers the same question
directly.

Then pick the right date. `on_date` is booking-dated — the form to check against
a figure taken from the books; `on_value_date` is the form to check against one
taken from outside. Checking a bank statement against a booking-dated fold is the
classic reconciliation mistake: it produces a month-end difference nobody can
find, because nothing is wrong.

Use the **positional** form when you need a claim that is exact against one
history — pairing it with a tree head, or pinning a regression test.

## Historical reads

Every balance read takes a `BalanceQuery`, which is one type for every way of
narrowing a report — because they are the same fold with different predicates,
and keeping them apart produced a method per combination.

```rust
let now      = journal.balance(&cash_key, BalanceQuery::all())?;
let then     = journal.balance(&cash_key, BalanceQuery::over_prefix(1_204))?;
let nothing  = journal.balance(&cash_key, BalanceQuery::over_prefix(0))?;
let as_at    = journal.balance(&cash_key, BalanceQuery::through(date!(2026-03-31)))?;
let in_march = journal.balance(
    &cash_key,
    BalanceQuery::between(date!(2026-03-01), date!(2026-03-31)),
)?;
```

`over_prefix` takes a **count of entries**, not an index — the same number a
`TreeHead` carries, so a balance and the root it belongs with are always named
the same way, and `over_prefix(0)` then means something where a "last index
included" of zero cannot express an empty prefix at all.

The two orderings are both well defined and neither replaces the other:

| Query | Folds | Use it for |
|---|---|---|
| `over_prefix(n)` | the first `n` entries in **log order** | a claim exact against one history; pairing with a tree head |
| `through(d)` | everything dated on or before `d` | a period's closing balance, which is what [seals](@/docs/periods-and-seals.md) commit to |
| `between(a, b)` | everything dated in `[a, b]` | a period's **activity** — an income statement, not a balance sheet |

Both dated forms fold by **booking date** unless `by_value_date()` says
otherwise — see [above](#two-dates-and-which-one-a-report-means).

`matching(&filter)` narrows any of them to a
[reporting axis](@/docs/dimensions.md#slicing-a-report), and the narrowings
compose: a segment P&L for one month is one query, and one round trip to a store.

Reading a current balance is a lookup — `BalanceQuery::all()` serves the
maintained totals. Anything narrower replays that key's postings, which is linear
in how much has moved through the account rather than in the size of the journal.
