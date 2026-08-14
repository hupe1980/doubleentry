+++
title = "Corrections"
description = "Reversals as the only correction mechanism, the rules the engine enforces on them, and how a sealed period is corrected."
weight = 8
+++

## Postings are immutable

There is no edit and no delete. A correction is a **new entry** that reverses an
earlier one, and the rules are enforced rather than documented:

- An entry can be reversed **at most once**.
- A reversal **cannot itself be reversed** — otherwise a chain of corrections
  becomes ambiguous about what the current state is.
- An entry claiming to reverse another **must actually invert it**: same
  accounts, amounts, currencies, layers and dimensions, in the same order, with
  every side flipped.
- A correction to a **sealed period** books into a later open period, carrying
  the original booking date as metadata.

```rust
let reversal = original.reverse(
    EntryId::generate(),
    IdempotencyKey::new(b"reversal-1".to_vec())?,
    date!(2026-04-01),
);
let recorded = journal.record(reversal)?;

assert_eq!(journal.reversal_of(original_id), Some(recorded.id));
```

`Entry::reverse` flips every posting's side and preserves its magnitude, so no
arithmetic is performed and no overflow is possible.

## Why inversion is checked

An entry that names an original but does not invert it would leave the original
marked as reversed while the amounts failed to net. The ledger would then assert
a correction it did not make — worse than no reversal tracking at all.

The check requires postings to correspond one-to-one **in order**, which is what
`Entry::reverse` produces. Building a reversal by hand and reordering its
postings is refused rather than matched heuristically: a rule that guesses is a
rule an auditor cannot check.

## Provenance is not inherited

`reverse` deliberately does **not** copy provenance or entry kind. A correction is
a new act by whoever made it, and it is not an invoice merely because the entry it
reverses was one. Set both on the returned draft:

```rust
let reversal = original
    .reverse(EntryId::generate(), key, date!(2026-04-01))
    .with_provenance(Provenance::none().with_actor("controller")?)
    .with_kind(Label::new("correction")?);
```

The description *is* carried over, since it describes the transaction being
undone.

## Correcting a sealed period

A sealed period is terminal — there is no reopening. Reopening would mean
rewriting history that has already been committed to, which is the one thing an
append-only log cannot do.

So the reversal books into the current open period and carries
`original_booking_date`, which retains the date the correction economically
belongs to:

```rust
// Back into a sealed March: refused.
assert!(journal.record(original.reverse(id1, key1, date!(2026-03-20))).is_err());

// Into an open April: accepted, and it still says which period it belongs to.
let recorded = journal.record(original.reverse(id2, key2, date!(2026-04-01)))?;
let stored   = journal.get(recorded.id).expect("recorded");
assert_eq!(stored.original_booking_date(), Some(date!(2026-03-15)));
```

Your reporting layer can therefore present the correction against March while the
log and the seals show, truthfully, that it was booked in April.

## The net and the gross

A reversal nets the original to zero and leaves **both gross totals standing**:

```rust
let after = journal.balance(&cash_key, None)?;
assert_eq!(after.signed_net()?, Eur::ZERO);
assert_eq!(after.debits,  Eur::parse("42.00")?);
assert_eq!(after.credits, Eur::parse("42.00")?);
```

That is the point of [carrying gross totals](@/docs/debits-and-credits.md): a
corrected booking remains visible as something that happened and was undone,
rather than vanishing as though it never had.
