+++
title = "Statements, checkpoints and assertions"
description = "Reading how an account got to where it is, caching that fold safely, and reconciling the ledger against an outside source."
weight = 9
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
let page = store.statement(cash_key, Cursor::start().with_limit(100)).await?;
```

## Checkpoints

A balance is *defined* as a fold over the journal — correct, and linear in
history. A checkpoint records that fold up to a point so later reads start from
there.

Because that trades a definition for a cache, a checkpoint is only safe if it can
be re-derived. So it carries the log position **and** the tree head it was taken
against:

```rust
let cp = journal.checkpoint(&cash_key)?;
journal.verify_checkpoint(&cp)?;    // re-derives and compares
```

`verify_checkpoint` checks three things independently: that the position exists,
that the tree head matches the log *at that size*, and that the balance matches a
fold. The head check is what makes a stale checkpoint detectable — a checkpoint
that matches numerically but was taken against a different history is stale by
construction, not by convention, and silently trusting it would carry a wrong
balance forward.

A checkpoint over a prefix stays valid as the log grows, because it describes a
real prefix and its pinned head lets it be re-derived exactly.

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

Assertions can target an earlier log position, which is how you reconcile against
a statement that arrived late:

```rust
let historical = BalanceAssertion::net(cash_key, expected).at_index(1_204);
```

## Historical reads

Both statements and balances accept a log position rather than a date:

```rust
let then = journal.balance(&cash_key, Some(LogIndex::new(1_204)))?;
```

"The balance as the journal stood after 1 204 entries" is a well-defined
question; "the balance on a date" is not, because entries are appended in
recording order rather than booking-date order and a late correction changes what
a date means. For the date-shaped question, use
`trial_balance_through_date`, which folds every entry *booked* on or before a day
— that is what a period's closing balance means, and it is what
[seals](@/docs/periods-and-seals.md) commit to.

Reading a current balance is a lookup. Reading a historical one replays that
key's postings, which is linear in how much has moved through the account rather
than in the size of the journal.
