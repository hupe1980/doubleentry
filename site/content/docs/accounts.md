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
let cash    = journal.accounts_mut().register_path("Assets:Cash",  date!(2026-01-01))?;
let revenue = journal.accounts_mut().register_path("Income:Sales", date!(2026-01-01))?;
```

Ancestors are not created implicitly. A path may be registered without its
parents existing, and registering a parent later reclassifies it as a
non-postable node.

## The two rules it does enforce

Both are structural, and neither can be repaired by a layer above:

- **Only leaves are postable.** An account with registered descendants is an
  aggregation path. Posting to both a node and its descendants makes every
  rollup double-count, and no reporting layer can undo that afterwards.
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
journal.accounts_mut().close(old_bank, date!(2026-06-30))?;
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
the alteration the commitment exists to expose.

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
journal.accounts_mut().set_limit(wallet, BalanceLimit::NoDebitBalance)?;

let err = journal.record(overdraw)?;   // JournalError::LimitBreached { .. }
```

### What it is checked against

The **balance the entry would leave behind**, per currency and per layer
independently, and on the entry as a whole rather than posting by posting. An
entry that dips an account past its limit and back within one booking is one
movement, and judging it posting by posting would make the answer depend on the
order you happened to list them in.

Gross totals are irrelevant: only the net crosses zero, so an account with heavy
offsetting turnover is unaffected as long as the net stays on the permitted side.
Zero is on the permitted side of both limits.

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
