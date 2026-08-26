+++
title = "Dimensions"
description = "Reporting axes carried on every posting, orthogonal to the account path — and why the crate ships none of them."
weight = 13
+++

## Axes, not deeper paths

Dimensions are the axes reporting slices by. They are carried on every posting
and are orthogonal to the account path: the path carries general-ledger
structure, dimensions carry everything else you need to group by.

```rust
use doubleentry::{Dimensions, Label};

let dims = Dimensions::none()
    .with(Label::new("activity")?, Label::new("Network")?)?
    .with(Label::new("segment")?,  Label::new("Electricity")?)?;

let posting = Posting::debit(cash, Eur::parse("100.00")?, Currency::EUR)
    .with_dimensions(dims);
```

The alternative is encoding the axis into the path —
`Grid:Electricity:HV:Revenue`. That multiplies the account count by the product
of the axes and freezes the reporting dimensions at design time. Keeping them
separate lets a trial balance group by any axis without restructuring the tree.

## The axes are yours

The engine ships **no** axis names, because there is no set that is right for
everyone: an energy utility separates by regulated activity, a fund
administrator by mandate, a marketplace by counterparty. Naming four of them in
the library would be a chart of accounts by another route — and would then have
to be worked around by everyone whose fifth axis matters.

What the engine does:

- **Bounds them** — at most 8 axes per posting, 64 characters per label.
  Dimensions are hashed into every entry and written on every posting row, so an
  unbounded map would make the size of a ledger a function of what a caller
  happened to attach.
- **Orders them** deterministically, so the canonical encoding is a function of
  the *set*, not of the order they were built in.
- **Folds them into the entry hash**, so they are covered by tamper evidence.
- **Lets you require them**, through `LedgerPolicy::requiring`.
- **Reports on them** — [below](#slicing-a-report).

It never interprets a value.

## Slicing a report

A `DimensionFilter` narrows any balance read: `balance`, `trial_balance`,
`balances`. It is a conjunction, and it takes two kinds of clause.

```rust
let retail = DimensionFilter::any()
    .matching(Label::new("segment")?, Label::new("Retail")?);

// One account, one slice.
let b = journal.balance(&key, BalanceQuery::all().matching(&retail))?;

// Every account, one slice — and every report that works on a trial balance
// works on this one, because it is a trial balance.
let tb = journal.trial_balance(BalanceQuery::all().matching(&retail))?;
assert!(tb.totals(Currency::EUR, Layer::Settled)?.is_balanced());
```

`dimension_values` tells you which values an axis actually takes, since the
values are yours and nothing else knows:

```rust
for value in journal.dimension_values("segment") {
    let filter = DimensionFilter::any().matching(Label::new("segment")?, value);
    // … one slice per value.
}
```

### The clause that keeps a report honest

`missing(axis)` matches postings that do **not** carry the axis at all. Slice by
the values an axis takes and the totals will not reach the trial balance, because
unattributed postings land in none of the slices — silently.

```rust
let unattributed = DimensionFilter::any().missing(Label::new("segment")?);
let rest = journal.balance(&key, BalanceQuery::all().matching(&unattributed))?;
// slices + rest == the whole
```

Requiring the axis with `LedgerPolicy::requiring` is how you stop that happening
going forward. `missing` is how you find what slipped through before you did.

### Slicing and dating compose

`BalanceQuery` carries the date range and the filter together, so a segment P&L
for one month is one query — and one round trip to a `LedgerStore`:

```rust
let march = BalanceQuery::between(date!(2026-03-01), date!(2026-03-31));
let retail_in_march = store.trial_balance(march.matching(&retail)).await?;
```

Note `between`, not `through`: a date **range** is the period's *activity*, what
an income statement reports; `through` is the cumulative position a balance sheet
reports. See [Historical reads](@/docs/statements.md#historical-reads).

## Requiring an axis

Set this where the books cannot be kept without an attribution — a regulated
activity, a mandate, a fund — so that an unattributed posting is rejected at the
door rather than silently landing outside every grouping a report knows about.
Discovering it later means restating.

```rust
let policy = LedgerPolicy::permissive()
    .requiring(Label::new("activity")?);
```

The engine checks **presence**, never the value. Which values are legal is a
question about your business, and one this crate has no way to answer.

A posting carrying a *different* axis does not satisfy the requirement — the
check is per axis, not "has some dimension".

## Labels

`Label` is the crate's validated short-string type, used for axis names,
dimension values, entry kinds, period identifiers and provenance fields. The same
type is used for all of them because they have the same rules, and inventing a
newtype per position would give type safety over values that are, by design,
interchangeable opaque text.

It rejects the empty string, anything over 64 characters, and any control
character. Control characters are refused because a label ends up in log lines,
CSV exports and error messages, and an embedded newline or terminal escape turns
one field into two.

## In SQL

The axes are a child table keyed on the posting, one row per axis, indexed on
`(axis, value)`:

```sql
CREATE TABLE posting_dimensions (
    entry_id        UUID        NOT NULL,
    posting_index   SMALLINT    NOT NULL,
    axis            TEXT        NOT NULL,
    value           TEXT        NOT NULL,
    PRIMARY KEY (entry_id, posting_index, axis),
    FOREIGN KEY (entry_id, posting_index) REFERENCES postings (entry_id, posting_index)
);
```

A column per axis would be either this crate's guess at yours or a schema change
every time a new one is needed. A JSON blob indexes nothing:
`WHERE axis = 'activity' AND value = 'Network'` is an index scan here and a full
scan there.

The index is not decoration. An equality clause becomes an **inner join** on
`posting_dimensions`, so the planner drives from `(axis, value)` — the selective
side — rather than testing every posting in turn. It cannot duplicate a posting,
because `(entry_id, posting_index, axis)` is the primary key and matches at most
one row. An *absence* clause has no row to join to and stays a `NOT EXISTS`.

Because dimensions are part of the content hash, a backend that drops an axis
does not merely under-report — it makes the entry **unreadable**, since
rehydration recomputes the hash and refuses a mismatch. The conformance suite
checks this.
