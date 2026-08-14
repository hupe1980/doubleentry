+++
title = "Debits, credits and gross totals"
description = "Why the side is explicit rather than a sign, and why a balance carries both gross totals instead of only the net."
weight = 4
+++

## The side is not a sign

A `Posting` carries a `Direction` — `Debit` or `Credit` — and a **magnitude**
that must not be negative. The side is explicit rather than encoded in the sign
of the amount, and a negative amount is rejected at validation rather than
silently reinterpreted as the opposite direction.

```rust
let d = Posting::debit(cash,  Eur::parse("100.00")?, Currency::EUR);
let c = Posting::credit(cash, Eur::parse("100.00")?, Currency::EUR);

assert_eq!(d.signed()?, Eur::parse("100.00")?);
assert_eq!(c.signed()?, Eur::parse("-100.00")?);
```

`signed()` is available when you want to net, but it is a projection, not the
representation. The reason is the next section.

## A net cannot reproduce turnover

A `Balance` carries the gross debit total **and** the gross credit total, not
just the difference:

```rust
let mut b = Balance::<2>::ZERO;
b.add(Direction::Debit,  Eur::parse("1000.00")?)?;
b.add(Direction::Credit, Eur::parse("1000.00")?)?;

assert_eq!(b.signed_net()?, Eur::ZERO);          // nets to nothing …
assert_eq!(b.debits,  Eur::parse("1000.00")?);   // … but a thousand moved
assert_eq!(b.credits, Eur::parse("1000.00")?);   //     in each direction
assert!(b.is_balanced());
assert!(!b.is_empty());
```

An account showing a net of zero could have seen no activity at all, or heavy
offsetting turnover. Those are different facts, and the gross totals cannot be
reconstructed from the net afterwards — so a ledger that stores only the net has
thrown the distinction away permanently.

`is_empty()` distinguishes them: it is true only when nothing moved in either
direction.

This is also why a reversal is visible rather than invisible. Reversing an entry
leaves the net at zero while both gross totals survive, so the books show that
something was booked and then corrected — not that nothing happened.

## Normal sides

`AccountKind` records the side an account normally carries a balance on:

| Kind | Normal side |
|---|---|
| Asset, Expense | Debit |
| Liability, Equity, Income | Credit |

This is **metadata only**. The engine records it, exposes it, and never uses it
to constrain a posting — a chart of accounts is your concern, and an asset
account legitimately goes into credit all the time. It is used by
[closing entries](@/docs/closing.md), which need to know which accounts belong to
one period, and it is available to your reporting layer.

## Trial balances

A `TrialBalance` maps `(account, currency, layer)` to a `Balance`. Iteration
order is deterministic, so any report or hash derived from one is reproducible.

```rust
let tb = journal.trial_balance(None)?;

// The classic check: debits equal credits across every account.
let totals = tb.totals(Currency::EUR, Layer::Settled)?;
assert!(totals.is_balanced());
```

Note the key includes the **layer**. Settled movements and pending reservations
are totalled separately and never net against each other — reporting a
reservation as though the money had moved is exactly the error the layer exists
to prevent.

`journal.verify_balanced()` runs this check across every currency and layer at
once; `journal.verify_balances()` goes further and recomputes the maintained
balances from the entries, proving the incremental state has not drifted from
what it claims to summarise.
