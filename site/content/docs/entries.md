+++
title = "Entries and validation"
description = "The type-state entry, what balanced-by-construction claims, and the ledger-wide policy that decides which bookings are legal."
weight = 3
+++

## Balanced by construction

`Entry` carries a type-state parameter. A draft is freely editable and proves
nothing; sealing it runs every invariant and yields `Entry<Balanced, P>` — a type
with private fields, no public constructor, and marker types behind a sealed
trait. Nothing outside the crate can produce one by any route other than
validation.

```rust
let errors = Entry::new(
    EntryId::generate(),
    IdempotencyKey::new(b"k".to_vec())?,
    date!(2026-03-15),
)
.debit(cash,     Eur::parse("100.00")?, Currency::EUR)
.credit(revenue, Eur::parse("99.00")?,  Currency::EUR)
.seal(&journal.context())
.unwrap_err();

// Every violation is reported at once, not one round trip at a time.
assert!(errors.any(|e| matches!(e, ValidationError::Unbalanced { .. })));
```

### What this claims, and what it does not

Worth stating precisely, because it is easy to overclaim. Whether a set of
postings balances is a property of runtime values, so no type system short of
dependent types decides it at compile time.

What the type state gives you is that **an unbalanced entry cannot be represented
as a validated one**. Every API that persists, exports, or commits to an entry
accepts only the balanced form, so unbalanced data cannot reach storage without a
bug in this crate.

## Every violation at once

Validation reports all violations rather than the first. A caller repairing a
batch import should not discover its problems one round trip at a time.

The checks a `seal` runs:

| Check | Why it exists |
|---|---|
| At least two postings | Fewer is not double-entry |
| At most `MAX_POSTINGS` | A posting is addressed by a `u16` and a SQL `SMALLINT`; beyond that the position would truncate and point at the wrong movement |
| No zero amounts | A zero posting carries no information |
| No negative amounts | The side is carried by the direction, not the sign |
| Account is registered | A handle from another registry names nothing here |
| Account is a leaf | Posting to a node and its descendants double-counts every rollup |
| Booking date inside the account's open window | |
| Debits equal credits, **per currency** | A cross-currency entry balances each currency independently |
| Booking date's period accepts postings | A sealed period has been committed to |
| Policy: currency, required dimensions, value-date drift | Ledger-wide rules you opted into |

Two further rules are checked when the entry is **recorded** rather than sealed,
because both are questions about the books rather than about the entry: the
[correction rules](@/docs/corrections.md), and any
[balance limit](@/docs/accounts.md#balance-limits) on an account it touches. The
same entry is legal or not depending on what has already been recorded, so no
amount of inspecting it in isolation can answer them.

## Per-currency balance

An entry may move more than one currency, and each balances independently:

```rust
// EUR balances, USD balances: a legitimate cross-currency booking.
let entry = Entry::new(id, key, date!(2026-03-15))
    .debit(cash,    Eur::parse("10.00")?, Currency::EUR)
    .credit(revenue, Eur::parse("10.00")?, Currency::EUR)
    .debit(fx,      Eur::parse("5.00")?,  Currency::USD)
    .credit(revenue, Eur::parse("5.00")?,  Currency::USD)
    .seal(&journal.context())?;
```

The engine never converts between currencies. A rate is a business decision with
a source, a timestamp and a policy behind it, and a library that guessed one
would be wrong in a way no downstream report could detect.

## Ledger policy

Everything in `LedgerPolicy` is off by default: a policy you did not ask for is a
rule you discover by having a valid booking rejected.

```rust
use doubleentry::{Currency, Label, LedgerPolicy};

let policy = LedgerPolicy::permissive()
    .in_currency(Currency::EUR)                     // single-currency ledger
    .requiring(Label::new("activity")?)             // every posting must be attributed
    .with_max_value_date_drift(30);                 // value date within 30 days of booking

let journal = Journal::<2>::new(ledger).with_policy(policy);
```

A policy applies to what is recorded **next**. It does not re-validate what is
already recorded, and it must not: an entry that was legal when written stays
readable forever.

For `requiring`, the engine checks *presence*, never the value. Which values are
legal is a question about your business, and one this crate has no way to answer.
See [Dimensions](@/docs/dimensions.md).

## Provenance and documents

Every entry records who booked it and, optionally, what it was booked against.
An audit trail that cannot say who made a booking is not an audit trail.

```rust
use doubleentry::{DocumentRef, Hash, Provenance};

let entry = Entry::new(id, key, date!(2026-03-15))
    .with_provenance(
        Provenance::none()
            .with_actor("clerk-17")?
            .with_source("billing-service")?
            .with_correlation("run-2026-03-15")?,
    )
    .with_document(DocumentRef::new(
        "INV-2026-0001",
        Hash::digest(b"acme/invoice/v1", pdf_bytes),
    )?)
    // … postings …
    .seal(&journal.context())?;
```

Both are folded into the entry's content hash, so neither can be edited after the
fact without invalidating it.

The document hash is **optional**, and that is deliberate. Systems routinely book
against a document they hold only an identifier for — an invoice number arriving
on a message bus, a payment reference from a bank statement. A mandatory hash
pushes those callers into inventing one, which produces a commitment that looks
cryptographic and verifies nothing. `DocumentRef::unverified` says exactly what
is true: the entry names a document without vouching for its contents. The
presence of the hash is itself part of the canonical encoding, so an unhashed
reference cannot later be passed off as a hashed one.

## Entry kind

`with_kind` tags the whole entry — an invoice, a payment, an advance, a
correction. It is entry-level rather than per-posting because what an entry *is*
belongs to the whole entry, and putting it on the postings would repeat it and
permit two postings of one entry to disagree.

It is opaque to the engine: stored, hashed, indexed and grouped by, never
interpreted. No vocabulary ships with this crate.

## Pending and settled

A posting sits in one of two layers. `Layer::Settled` means the amount has moved;
`Layer::Pending` means it is reserved but has not.

What the engine guarantees is **separation**: balances, statements, clearings and
[balance limits](@/docs/accounts.md#balance-limits) are all keyed on the layer,
so a reservation and a settled movement are totalled apart and never net against
each other. Reporting a reservation as though the money had moved is exactly the
error the layer exists to prevent.

What the engine does *not* do is model a reservation's lifecycle. It ships no
`post`/`void` operation, no expiry, and no link from a settling entry back to the
hold it discharges. Resolving a reservation is an ordinary append — reverse the
pending entry to release it, and book the settled entry when the money moves —
and the pieces you need to keep that honest are already here:

- **At-most-once release.** An entry can be reversed at most once, and a reversal
  cannot itself be reversed, so a hold cannot be released twice.
- **Residuals.** [Clearing](@/docs/open-items.md) works within the pending layer,
  so a partially consumed hold reports its remainder like any other open item.
- **Reservation totals.** `trial_balance` and `balance` both key on the layer, so
  "how much is currently held" is a read, not a computation.

That boundary is deliberate. Expiry needs a clock, and this engine reads none;
and a `post`/`void` vocabulary would have to choose whether a partial settlement
releases the remainder — a policy question with different right answers in
payments, in trading and in inventory. Both belong to the layer above, and both
are straightforward to build on the guarantees above.
