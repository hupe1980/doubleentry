+++
title = "Statements, checkpoints and assertions"
description = "Reading how an account got to where it is, caching that fold safely, and reconciling the ledger against an outside source."
weight = 10
+++

## Statements

A trial balance says where an account ended up and nothing about how it got
there. A statement is the other half: every posting touching one key, in log
order, with the running balance after each.

```rust
let lines = journal.statement(&cash_key)?;

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
let page = store.statement(cash_key, PostingCursor::start().with_limit(100)).await?;
for line in &page.lines { /* … */ }
let next = page.next;   // `None` at the end
```

### The cursor addresses a posting, not an entry

`PostingCursor` is deliberately not the `Cursor` that pages the log. A log page
is a list of entries; a statement is a list of **postings**, and one entry may
put several on the same account — a split receipt booked as three lines against
one credit is an ordinary entry.

So a page boundary can fall *inside* an entry, and a cursor that could only name
an entry had no way to say where. Resuming from one asked for `log_index > after`
and skipped every remaining posting of the entry the page ended in: lines
vanished, silently and permanently, and the running balance stayed internally
consistent across the gap so nothing looked wrong.

`PostingPosition` is the pair — the entry's log index and the posting's index
within it — and `StatementLine::position()` hands you the one to resume after.
[Open items](@/docs/open-items.md) page the same way, behind the same cursor:
they are the filtered view of the same postings.
The running balance is folded from the start of the account either way, so a
page opens where the previous one closed rather than at the account's own closing
figure.

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
position field — two fields that must agree are two fields that can disagree, and
this pair used to: a checkpoint taken over an empty ledger recorded "no position"
where the balance reader understood "the current balance", so it silently started
failing the moment anything was recorded.

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
```

Use the **dated** form to reconcile against anything external. A bank statement
says "as at 31 March", not "after 4 812 entries", and folding by booking date is
what puts a late-arriving backdated entry in the period it economically belongs
to. `journal.balance_on_date` answers the same question directly.

Use the **positional** form when you need a claim that is exact against one
history — pairing it with a tree head, or pinning a regression test.

## Historical reads

Balances and trial balances take a **prefix size** — a count of entries, not an
index:

```rust
let then    = journal.balance(&cash_key, Some(1_204))?;   // after 1 204 entries
let nothing = journal.balance(&cash_key, Some(0))?;       // the empty ledger
let now     = journal.balance(&cash_key, None)?;          // everything so far
```

A count rather than an index because it is the same number a `TreeHead` carries,
so a balance and the root it belongs with are always named the same way — and
because `Some(0)` then means something, where a "last index included" of zero
cannot express an empty prefix at all.

Both forms are well defined and neither replaces the other. "As the journal stood
after 1 204 entries" is exact against one history. "On a date" is the question
every external source asks, and `journal.balance_on_date` and
`trial_balance_through_date` answer it by folding every entry *booked* on or
before a day — which is also what a period's closing balance means, and what
[seals](@/docs/periods-and-seals.md) commit to.

Reading a current balance is a lookup. Reading a historical one replays that
key's postings, which is linear in how much has moved through the account rather
than in the size of the journal.
