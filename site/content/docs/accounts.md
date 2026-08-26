+++
title = "Accounts"
description = "The account tree, the two structural rules the engine enforces, handles as durable state, and limiting which side a balance may fall on."
weight = 2
+++

## Paths, not a chart of accounts

An account is a path in a hierarchy you define — `Assets:Current:Bank:Main`. The
engine does not know what `Assets` means, does not require that name, and
validates against no national or corporate scheme.

```rust
let cash    = journal.register_path("Assets:Cash",  date!(2026-01-01))?;
let revenue = journal.register_path("Income:Sales", date!(2026-01-01))?;
```

Ancestors are not created implicitly. A path may be registered without its
parents existing, and registering a parent later reclassifies it as a
non-postable node — provided nothing has been booked to it yet, which is the
[leaf rule](#the-two-rules-it-does-enforce) below.

Not registering them costs nothing at reporting time:
[a rollup](@/docs/reporting.md) infers `Assets` from `Assets:Cash` either way.
Register a parent when you want it *classified* or named, not to make the tree
appear.

Registration goes through the **journal**, not through `AccountRegistry`
directly, because that rule needs the postings to decide and a registry holds
none. `close_account`, `reopen_account` and `set_account_limit` sit alongside it
for the same reason: master data changes what may be booked next, so it belongs
where the books are.

## The two rules it does enforce

Both are structural, and neither can be repaired by a layer above:

- **Only leaves are postable.** An account with registered descendants is an
  aggregation path. An account that is both a bucket and a container has two
  defensible balances — its own postings, and its subtree's — and every report
  has to pick one silently. Nothing downstream can un-mix them, because both
  readings are of the same rows.

  The rule is checked in both directions. An entry is refused if it targets a
  node, and a **registration is refused if it would put a child under an account
  that has already been posted to**. Without the second half the first is a
  trap: registering the child turns the parent into a node, every subsequent
  entry on it is refused — *including the reversal that would correct it* — and
  the balance already sitting there could never be moved again, which an
  append-only log has no way back from.

  `AccountRegistry` cannot make that check, because it holds no postings.
  `Journal` and every `LedgerStore` can, and do.

  What the rule buys is [hierarchical reporting](@/docs/reporting.md): a node
  can never hold postings of its own, so its balance is unambiguously its
  subtree's.

  Note the contrast with the open window below, which is *not* checked
  retroactively: closing an account before dates it has already been posted on
  is allowed. The difference is that a window is only a gate on future writes,
  while the leaf rule decides what a balance *means* — and leaves an account
  with no way to correct itself.
- **Postings fall inside the account's open window.** An account has an opening
  date and an optional closing date, both inclusive.

Everything else about an account is metadata. `AccountKind` records the side an
account normally carries a balance on, and is used only by
[closing entries](@/docs/closing.md) and by your reporting layer — never to
constrain a posting, because an asset account legitimately goes into credit all
the time.

## Handles are ledger state, not a runtime detail

Registration issues an `AccountId`: a dense `u32` position, cheap to compare and
to index by. That handle is written into every posting row and into every
trial-balance leaf a [seal](@/docs/periods-and-seals.md) commits to — so it is
part of the ledger's persistent state.

A registry rebuilt in a different order would repoint history. Rebuild from the
stored bindings, never by re-registering paths:

```rust
// Right: each account lands at the handle it was issued.
let registry = AccountRegistry::from_records(store.accounts().await?)?;

// Wrong: reissues handles in whatever order this code happens to use.
// for path in my_chart { registry.register_path(path, opened)?; }
```

`AccountRegistry::commitment` is a Merkle head over every handle-to-path
binding, in handle order. Two registries agree on it only if they agree on every
path *and* on the handle each was issued — which is what lets a seal pin the
handle space it committed to, and what lets one balance be
[named to an auditor](@/docs/periods-and-seals.md) without disclosing the
rest of the chart.

The leaf covers the handle and the path and **nothing else**. That is the
account's identity, and precisely the part that never changes. Master data is
out of it, deliberately — see below.

## Master data changes; postings do not

Closing an account, reclassifying it, or limiting it all change **what may be
booked next**, and never what was booked already. That is why the registry is
mutable while the journal is not:

```rust
journal.close_account(old_bank, date!(2026-06-30))?;
```

Pushing that change to a store is the same call that registered the account.
`register_account` is an upsert on the mutable fields, and the **path at a handle
is immutable** — rebinding one is refused outright, because every posting row
naming it would silently repoint:

```rust
for record in journal.account_records() {
    store.register_account(&record).await?;    // records new and changed alike
}
```

Master data is *outside* the entry log, and it is also outside the registry
commitment. That is load-bearing rather than incidental: `kind`, the open window
and the balance limit are mutable by design, so hashing them into the binding
leaf would mean that closing an account on a Tuesday retroactively invalidated
every binding proof against every seal ever issued — a routine operation reading
as evidence of tampering.

So closing an account does not move the commitment, and a proof taken before the
change still verifies after it. What *does* move it is a change to identity:
re-registering the same paths in a different order renumbers every handle, the
head changes, and the seal that named the old numbering stops matching. Which is
the alteration the commitment exists to expose — and which
`SealChain::verify_against_accounts` is what actually
[checks](@/docs/periods-and-seals.md#verifying-against-the-books), rather than
leaving to be noticed.

### What that costs

The ledger therefore records master data's **current** state, not its history.
No entry says an account was closed on the 30th, or that a limit was imposed the
week before, or who did either — and none of it is tamper-evident. That is the
price of the paragraph above, and it is a real boundary: **record master-data
changes where you record your other administrative events.**

What the ledger does guarantee is narrower: a handle's *path* is immutable,
rebinding one is refused, and renumbering the handle space is caught against
every seal ever issued.

Because the registry still **grows**, a proof against a seal must be built at the
size the seal recorded:

```rust
let binding = journal
    .accounts()
    .prove_binding_at(cash, seal.accounts.size)
    .expect("issued by then");
```

## Balance limits

By default an account's balance may fall on either side. `BalanceLimit`
constrains it where the books would be **wrong** rather than merely surprising if
it crossed zero:

| Limit | Meaning | Typical use |
|---|---|---|
| `Unlimited` | Either side. The default | Most accounts |
| `NoCreditBalance` | Credits may never exceed debits | A cash box or a bank account with no overdraft |
| `NoDebitBalance` | Debits may never exceed credits | A customer wallet or prepayment that cannot be drawn beyond what was funded |

```rust
journal.set_account_limit(wallet, BalanceLimit::NoDebitBalance)?;

let err = journal.record(overdraw)?;   // JournalError::LimitBreached { .. }
```

### What it is checked against

The **balance the entry would leave behind**, per currency, and on the entry as a
whole rather than posting by posting. An entry that dips an account past its
limit and back within one booking is one movement, and judging it posting by
posting would make the answer depend on the order you happened to list them in.

Gross totals are irrelevant: only the net crosses zero, so an account with heavy
offsetting turnover is unaffected as long as the net stays on the permitted side.
Zero is on the permitted side of both limits.

### Reservations count, expected inflows do not

Both layers are folded into one answer, and **asymmetrically**. A pending net
working *against* the limit consumes room; one working *for* it grants none. An
outflow you have committed to is gone; an inflow you have merely reserved has not
arrived.

`BalanceLimit::headroom_minor` is that answer, in minor units, negative when the
limit is breached:

```rust
// A wallet funded with 100, limited so it cannot be drawn past what it holds.
let limit = BalanceLimit::NoDebitBalance;
assert_eq!(limit.headroom_minor(&funded_100, &Balance::ZERO), Some(100));

// A reservation of 30 is money already promised: it takes 30 of the room.
assert_eq!(limit.headroom_minor(&funded_100, &held_30), Some(70));

// A reservation the other way is an inflow that has not arrived. It grants none.
assert_eq!(limit.headroom_minor(&funded_100, &expected_500), Some(100));
```

Discharging a reservation nets its pending side back to zero, so the room it was
holding returns as the movement lands in the settled layer.

Neither layer alone is the answer. Checking only the settled one lets a hold step
around the limit until the money moves — too late. Checking each layer against
its own zero forbids a cash box from reserving an outflow at all, which is the
one thing the pending layer is for.

### Where it is enforced

Not in `Entry::seal`. Sealing asks whether an entry is well formed against master
data — a question about the entry alone. A limit is a question about the *books*:
the same entry is legal or not depending on what has already been recorded. So it
is a `record`-time rule, like the correction rules.

Both SQL backends enforce it **inside the append transaction**, after the
postings are written, so the aggregate sees exactly the balance the entry would
leave. A check before the write would read a pre-image that two concurrent
appends both see — each fitting the limit, together breaching it. PostgreSQL
takes a row lock on the constrained account for the same reason; the conformance
suite and a concurrency test against a real database both cover it.

### Three consequences worth knowing

**A limit can refuse a reversal.** Reversing a funding entry withdraws money the
account may since have committed. A limit constrains the resulting balance, so it
cannot make an exception for a correction — and one that did would be a limit
that does not hold. Reverse whatever consumed the funding first, or lift the
limit deliberately.

**Tightening a limit never invalidates history.** An account already past a newly
imposed limit keeps its balance, and everything recorded stays recorded.

**But it is then frozen against partial repair.** The rule is that an accepted
entry leaves a balance satisfying the limit, and a half-repaired balance does
not. One entry that brings the account back over the line is accepted; two that
each get halfway are not. Lift the limit if that is what you need, so the
exception is a deliberate act on the record rather than a rule that quietly
bends.
