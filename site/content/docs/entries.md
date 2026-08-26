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
| Account is a leaf | An account that is both a bucket and a container has two defensible balances, and no report can un-mix them |
| Booking date inside the account's open window | |
| Debits equal credits, **per currency and layer** | Each currency balances independently, and so does each layer — see [below](#pending-and-settled) |
| Booking date's period accepts postings | A sealed period has been committed to |
| Policy: currency, required dimensions, value-date drift | Ledger-wide rules you opted into |

Two further rules are checked when the entry is **recorded** rather than sealed,
because both are questions about the books rather than about the entry: the
[correction rules](@/docs/corrections.md), and any
[balance limit](@/docs/accounts.md#balance-limits) on an account it touches. The
same entry is legal or not depending on what has already been recorded, so no
amount of inspecting it in isolation can answer them.

## Balancing, per currency and per layer

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
would be wrong in a way no downstream report could detect. What it does give you
is the arithmetic in the middle: `Amount::checked_mul_ratio` applies a rate with
an explicit rounding mode, exactly, and `rescale` crosses between the precision
a rate was published in and the one your books are kept in — see
[Money](@/docs/money.md#rates-tax-and-conversion).

The [layer](#pending-and-settled) balances independently too, for the same
reason and with more teeth: every balance the engine reports is keyed on it.

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

### Each layer balances on its own

Because every balance the engine reports is keyed on the layer, **an entry has to
balance within each layer separately** — not merely per currency across both.

```rust
// Refused. It nets to zero in EUR and leaves both layers out of balance.
Entry::new(id, key, date!(2026-03-15))
    .post(Posting::debit(cash, Eur::parse("50.00")?, Currency::EUR).in_layer(Layer::Pending))
    .post(Posting::credit(revenue, Eur::parse("50.00")?, Currency::EUR))
```

Accepting it would falsify `journal.verify_balanced()` permanently: that check
totals every currency and layer independently, both totals would be wrong, and an
append-only log has no way to take the entry back. The error names both layers.

### Resolving a reservation

The rule is what makes a hold and its discharge expressible as **one atomic
entry**: release the reservation in the pending layer, book the movement in the
settled one, and each side balances.

```rust
Entry::new(id, key, date!(2026-03-20))
    // Release the hold …
    .post(Posting::credit(cash, Eur::parse("50.00")?, Currency::EUR).in_layer(Layer::Pending))
    .post(Posting::debit(holds, Eur::parse("50.00")?, Currency::EUR).in_layer(Layer::Pending))
    // … and move the money.
    .debit(cash, Eur::parse("50.00")?, Currency::EUR)
    .credit(revenue, Eur::parse("50.00")?, Currency::EUR)
```

Releasing a hold without settling it is the pending inverse on its own, which is
what `Entry::reverse` builds. Settling *part* of it is the same entry with a
smaller settled leg and a partial pending inverse; the remainder stays reserved.

None of that needs an operation the crate does not already have. Two questions
are worth answering plainly, though.

**What stops a hold being released twice?** A
[balance limit](@/docs/accounts.md#balance-limits), and only that. The engine
cannot tell a release from a fresh hold the other way — they are the same
posting. It *can* tell that a cash box may not go credit, and a limit folds both
layers, so a doubled release drives the fold below zero and is refused inside the
write:

```rust
journal.set_account_limit(cash, BalanceLimit::NoCreditBalance)?;
// … hold 100, release 100, and a second release is `LimitBreached`.
```

On an unlimited account a doubled release is accepted, like any other credit to
cash you do not have. **Limit the accounts you hold reservations on.**

**Which release discharged which hold?** [Clearing](@/docs/open-items.md) is
scoped to a layer, so open-item matching over the pending layer answers it: each
reservation is an open item, a release is applied against it, over-application is
refused, and `open_items` lists what is outstanding — the same machinery invoices
and payments use.

### No timeouts, and no post/void

A reservation does not expire on its own. Expiry needs a clock, and a hold that
vanished without an entry would be a balance change nothing in the log accounts
for. Book the release from your scheduler instead.

A `post`/`void` vocabulary is absent for a different reason: it would have to
decide whether a partial settlement releases the remainder, and that has
different right answers in payments, in trading and in inventory.

## Booking date and value date

Every entry carries two dates. The **booking date** is when it went into the
books: it decides which [period](@/docs/periods-and-seals.md) the entry belongs
to, whether that period still accepts postings, and what a
[seal](@/docs/periods-and-seals.md) commits to. The **value date** is when the
money is treated as having moved, and it defaults to the booking date.

```rust
let entry = Entry::new(id, key, date!(2026-03-28))
    .with_value_date(date!(2026-04-02))     // booked in March, settles in April
    // … postings …
    .seal(&journal.context())?;
```

Any balance, trial balance or statement can be folded by the value date instead —
see [Two dates](@/docs/statements.md#two-dates-and-which-one-a-report-means).
What it never does is decide period membership; if it could, a settlement
instruction could reopen sealed books.

`LedgerPolicy::with_max_value_date_drift` bounds how far the two may sit apart,
for ledgers where a large gap means a data-entry error rather than a genuine
forward value.
