+++
title = "Open items and clearing"
description = "Matching receivables against payments, partial application, residual tracking, and resetting a clearing that matched the wrong items."
weight = 9
+++

## The problem

A receivable is raised by one posting and settled by another. Knowing *which*
settled *which* is what turns a running balance into a list of open items.

That is a ledger concern rather than an application one, because it has to agree
with the postings exactly. An application-side matching table drifts from the
books the first time an entry is reversed.

## Clearing is assignment, not movement

Nothing is mutated and no balance changes. A clearing records which postings
offset which, and by how much. A posting's **residual** is its amount less
everything applied to it, and it is *open* while the residual is positive.

```rust
use doubleentry::clearing::{Clearing, ClearingId, PostingRef};

journal.clear(
    Clearing::new(ClearingId::generate(), receivable_key, date!(2026-03-20))
        .apply(PostingRef::new(invoice.id, 0), Eur::parse("400.00")?)
        .apply(PostingRef::new(payment.id, 0), Eur::parse("400.00")?),
)?;

let open = journal.open_items(&receivable_key)?;
assert_eq!(open.len(), 1);                                  // payment fully applied
assert_eq!(open[0].residual, Eur::parse("600.00")?);        // invoice partly settled
```

Because it is assignment rather than movement, the trial balance before and after
a clearing is identical.

## Open items come back oldest first

FIFO — apply this payment to the oldest open invoice — is what open items are
*for*, so the list is in **log order**: the order the entries were recorded.

That is the same answer this crate gives to ordering everywhere else, and it is
deliberately not the obvious alternative of sorting by `PostingRef`, which orders
by entry **identifier**. Identity is supplied by the caller — the engine never
generates one on the deterministic path — so identifier order is chronological
only when the caller happens to use `EntryId::generate()`, whose UUIDv7 values
are time-ordered. Bring your own identifiers and that ordering silently becomes
arbitrary, which is the worst kind: it looks right in testing.

```rust
let open = journal.open_items(&receivable_key)?;
// open[0] is the oldest still-outstanding posting on this account.
```

`ClearingRegister::open_items` therefore imposes no order at all — it returns
candidates in the order it was given them, because it knows nothing about the
log and cannot honestly claim age. Supplying them in log order is the job of
whatever *does* know: the journal and every backend do, and the conformance
suite checks it.

## Over a store, they are paged

An account invoiced against for a decade is not a response body, so a durable
store pages open items exactly as it pages a statement — same log order, same
`PostingCursor`:

```rust
let mut cursor = Some(PostingCursor::start().with_limit(100));
while let Some(c) = cursor {
    let page = store.open_items(receivable_key, c).await?;
    for item in &page.items { /* … */ }
    cursor = page.next;
}
```

They are the filtered and unfiltered views of the same postings, so they read
alike. Each `OpenItem` carries its `position` — a `PostingPosition`, the entry's
log index paired with the posting's index within it — which is both what the
list is ordered by and what the next page resumes after.

That pairing is why the cursor is not a plain log index: one entry may leave
*several* postings open on one account, so a page boundary can fall inside an
entry, and an entry-addressed cursor would skip whatever remained of it.
`Journal::open_items` returns the whole list unpaged, as `Journal::statement`
does — an in-memory journal already holds everything.

## The rules

A clearing is refused unless all of these hold:

| Rule | Why |
|---|---|
| At least two items | A clearing relates postings to each other |
| No posting appears twice | Otherwise the applied total is ambiguous |
| Every posting resolves | A reference to an entry the ledger does not hold means nothing |
| Same account, currency **and layer** | See below |
| Every applied amount is positive | Zero carries no information; negative would mean the opposite side |
| Nothing over-applied | The applied amount cannot exceed what the posting still has open |
| Debits and credits match | The clearing itself balances |

The **layer** constraint deserves its own note. A reservation and a settled
payment are different claims on the same account. Netting one against the other
would report an open item as closed while the money had not moved. Both layers
clear the same way; they just do not clear against each other.

## Partial application

Applying less than the full amount leaves both sides open for the remainder,
which is the behaviour a partial payment needs: the invoice stays visible as
partly settled rather than vanishing or being rewritten.

Booking the shortfall as a fresh item instead — clearing the original in full and
raising a new one for the difference — is an ordinary entry your application
posts. The engine does not choose between the two policies.

## Resetting

A clearing can be reset when it turns out to have matched the wrong items:

```rust
journal.reset_clearing(clearing_id, date!(2026-04-02))?;
```

The reset is a **new record** that releases the applied amounts. The original
clearing stays in the register, because an assignment that was made and withdrawn
is itself part of the audit trail.

Applied amounts are derived by replaying the register's events, so a reset
genuinely releases what it released and nothing is edited in place. Resetting a
clearing twice is refused.

## In SQL

Open items are a **view**, not a materialised table:

```sql
SELECT p.entry_id, p.posting_index, p.amount_minor - COALESCE(applied.total, 0) AS residual_minor
FROM postings p
LEFT JOIN (
    SELECT ci.entry_id, ci.posting_index, SUM(ci.applied_minor) AS total
    FROM clearing_items ci
    JOIN clearings c ON c.clearing_id = ci.clearing_id
    WHERE c.reset_on IS NULL          -- a released clearing applies nothing
    GROUP BY ci.entry_id, ci.posting_index
) applied ON applied.entry_id = p.entry_id AND applied.posting_index = p.posting_index
WHERE p.amount_minor - COALESCE(applied.total, 0) > 0;
```

A backend may materialise it for performance; the definition is what matters, and
it is the definition the conformance suite checks.
