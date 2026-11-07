+++
title = "Idempotency"
description = "The idempotency key is the entry's identity: derived ids, replays, key conflicts, and the draft fingerprint."
weight = 6
+++

## What to call

The key is the first argument of `Entry::new`. `record` returns a
[`Recorded`](https://docs.rs/doubleentry/latest/doubleentry/struct.Recorded.html) whose `replayed`
says whether it was a retry; a used key with other content is
[`KeyConflict`](https://docs.rs/doubleentry/latest/doubleentry/enum.Rejection.html#variant.KeyConflict)
(200). [`EntryId::derive`](https://docs.rs/doubleentry/latest/doubleentry/struct.EntryId.html#method.derive)
and `Entry::id` compute an id up front; `entry_by_key` looks one up.

## The key is the identity

The key is 1–128 bytes of text you choose — typically the source transaction's
id. The entry id is **derived** from it, `SHA-256(tag ‖ ledger id ‖ key)`
truncated to 16 bytes: no clock, no randomness. You know the id before you
record, a retry carries the original's id, and since the key is in the committed
record, so is the id.

```rust
use doubleentry::prelude::*;
use doubleentry::IdempotencyKey;

let ledger = LedgerId::new("example.com/acme")?;
let mut books = Journal::new(ledger.clone());
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let sale = |amount| {
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount, EUR)
        .credit("Income:Sales", amount, EUR)
};

let known_before = sale(amount!("100.00")).id(&ledger);
let first = books.record(sale(amount!("100.00")))?;
assert_eq!(Some(first.id), known_before);
assert_eq!(first.id, EntryId::derive(&ledger, &IdempotencyKey::new("invoice-2026-0001")?));

let size = books.head().size;
let retry = books.record(sale(amount!("100.00")))?; // same content: a replay
assert!(retry.replayed && (retry.id, retry.position) == (first.id, first.position));
assert_eq!(books.head().size, size); // nothing appended

match books.record(sale(amount!("100.01"))) {
    Err(Error::Rejected(Rejection::KeyConflict { position })) => assert_eq!(position, first.position),
    other => panic!("expected a key conflict, got {other:?}"),
}
```

A replay works even after the entry's period was sealed or its account closed,
because idempotency runs before every state rule
([Validation](@/docs/concepts/validation.md)). A client that timed out resends
**the same draft** and gets either a fresh receipt or the original one.

## What "the same content" means

The comparison uses a fingerprint of the draft as submitted, kept beside the
entry, covering every field including the memo **text**, capture time,
documents and rates. Two details:

- **The memo salt is not part of it**, so a retry with a fresh salt replays.
- **Accounts are compared by handle**, so naming an account by path in one
  attempt and by handle in the other is still the same draft.

Do not regenerate fields such as a capture time on retry: another capture time
is another draft, and a conflict.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
let cash = books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let draft = |memo: &str, by_handle: bool| {
    let entry = Entry::new("sale-1", date!("2026-03-15")).memo(memo);
    let entry = if by_handle {
        entry.debit(cash, amount!("10.00"), EUR)
    } else {
        entry.debit("Assets:Cash", amount!("10.00"), EUR)
    };
    entry.credit("Income:Sales", amount!("10.00"), EUR)
};

books.record(draft("Rechnung 4711", false))?;
assert!(books.record(draft("Rechnung 4711", true))?.replayed); // another salt and spelling
assert_eq!(books.record(draft("Rechnung 4712", true)).unwrap_err().code(), Some(200));
```

`record_batch` applies the same rule to each draft in order: an identical batch
sent again returns the original receipts, each `replayed`, and writes nothing.

## Erasing a memo ends replays

`erase_memo` removes the memo text and salt and also the entry's stored
fingerprint, so the ledger cannot confirm a guessed text by replaying it. After
erasure any submission under that key is a `KeyConflict`. The fingerprint is
not in the log: `Journal::from_log` and `audit()` recompute it from each entry
record and its memo text, so a restored copy replays old keys exactly as the
original does.

```rust
use doubleentry::prelude::*;
use doubleentry::log::ReplayLeaf;

let id = LedgerId::new("example.com/acme")?;
let mut books = Journal::new(id.clone());
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let draft = Entry::new("refund-17", date!("2026-03-15"))
    .memo("Refund for Max Mustermann")
    .debit("Income:Sales", amount!("25.00"), EUR)
    .credit("Assets:Cash", amount!("25.00"), EUR);
let recorded = books.record(draft.clone())?;

// A copy rebuilt from the leaves, without stored fingerprints, still replays.
let leaves = books.leaves().map(|leaf| ReplayLeaf { fingerprint: None, ..leaf });
let mut copy: Journal = Journal::from_log(&id, leaves)?;
assert!(copy.record(draft.clone())?.replayed);

let head = books.head();
books.erase_memo(recorded.id)?;
assert_eq!(books.head(), head); // no hash changed
assert_eq!(books.record(draft).unwrap_err().code(), Some(200));
```

## Together with your own database: the outbox

The store joins none of your transactions. To change your own rows and the
ledger exactly once, use an outbox:

1. In one transaction of your database, write the business change and an
   outbox row holding the key and the whole draft.
2. A worker records each pending row. `Ok` — replayed or not — marks it done;
   a crash before marking replays next time.
3. `is_transient()` or `may_have_applied()`: retry with the same key.
   `Error::Rejected(r)`: park the row with `r.code()` for a person.

Never record before the business commit: the log is append-only, so an entry
for a change that rolled back needs a reversal. Keep the draft's content in the
row; one recomputed from changed state is `KeyConflict` (200). Concurrent
workers are safe: the key serialises them.
[`examples/outbox.rs`](https://github.com/hupe1980/doubleentry/blob/main/examples/outbox.rs)
runs every case.

## Limits

- A key is unique per ledger: the same key in two ledgers gives two ids.
- A replay does not say whether the first attempt was yours or another
  client's.
- Clearings have keys of their own, and setup calls (`open`,
  `declare_currency`, `define_period`, …) write nothing when repeated
  identically. Two different keys are two entries.
- Keys do not expire.
