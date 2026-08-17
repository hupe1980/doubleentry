+++
title = "Periods and seals"
description = "Closing a period, chaining its seal, and proving one closing balance to an auditor without disclosing the rest of the books."
weight = 11
+++

## The period lifecycle

A period is a bounded range of booking dates with a state. Periods may not
overlap, because a booking date has to resolve to exactly one of them.

```
Open ──▶ Closing ──▶ Sealed
  ▲         │
  └─────────┘
```

- **Open** accepts postings.
- **Closing** stops them, so verification runs against a set that can no longer
  grow underneath it.
- **Sealed** is terminal. There is no reopening.

`Closing → Open` exists to abandon a close that failed verification. Nothing
returns from `Sealed`.

An empty calendar imposes no restriction, and a date no period covers is
unrestricted too — until something is sealed.

```rust
let march = PeriodId::new("2026-03")?;
journal.define_period(Period::new(march.clone(), date!(2026-03-01), date!(2026-03-31))?)?;

// Stop postings first — a separate, earlier decision than sealing.
journal.transition_period(&march, PeriodState::Closing)?;
let seal = journal.seal_period(&march)?;

assert!(seal.is_self_consistent());
assert!(journal.verify_seals().is_ok());
```

## The sealed watermark

Sealing commits to a **cumulative** closing balance: every entry booked on or
before the period's last day. That is only a stable claim if nothing can be
booked at or before that day afterwards — so sealing moves a watermark, and the
whole range below it closes:

```rust
// March sealed above, so the books are closed through its last day.
assert_eq!(journal.calendar().sealed_through(), Some(date!(2026-03-31)));

// February was never even defined as a period. It is still shut.
assert!(!journal.calendar().accepts(date!(2026-02-10)));
assert!(journal.calendar().accepts(date!(2026-04-01)));
```

Without it the guarantee has two holes, and neither needs tampering to reach —
both are ordinary writes the engine would otherwise accept:

1. **A gap in the calendar.** February is not a defined period, so it reports
   `Open`. One legal February booking restates March's sealed closing balance.
2. **Sealing out of order.** February is defined and still open when March
   seals. Same outcome, by a different route.

The watermark closes the first. `check_sealable` closes the second: a period is
sealable only once every **earlier** defined period is sealed, and only if it
ends *after* the watermark.

```rust
// February is defined and open, so March is not sealable yet.
assert!(matches!(
    journal.seal_period(&march),
    Err(JournalError::Period(PeriodError::UnsealedPredecessor { .. })),
));
```

In both cases the seal, its proofs and the whole chain would have gone on
verifying byte for byte while the balance they attest to no longer described the
books. That is precisely the alteration a seal exists to expose, so it is refused
at the point of the write rather than left for an auditor to notice.

One rule, one place: `PeriodCalendar::check_sealable` is what the in-memory
journal and every durable backend call, and the [conformance
suite](@/docs/persistence.md) fails a backend that does not enforce it.

## What a seal commits to

Three Merkle heads, and each answers a different question:

| Head | Claim |
|---|---|
| `tree_head` | Which entries the log held when the period closed |
| `trial_balance` | What they add up to, per account, currency and layer |
| `accounts` | Which account each of those handles actually is |

Each is a **head** — a size and a root — not a bare root. A proof is checked
against both halves, because a proof's own index and size fields steer its walk
rather than being checked by it. See [Proofs](@/docs/proofs.md#verify-against-a-head-never-a-bare-root).

Plus `prev_seal`, which chains it to the seal before. Removing or reordering a
sealed period breaks every seal after it.

The whole thing is hashed into `seal_hash`, so editing any field invalidates the
seal rather than restating it.

### "Belongs to the period" means two different things

The **tree head** is the whole log at the moment of sealing, not the period's
entries alone. Entries are appended in recording order rather than booking-date
order, so a period's entries need not be contiguous. An inclusion proof therefore
establishes *this entry was in the log the seal closed over*; `entry_count` and
`index_span` describe how much of that log the period accounts for.

The **closing balances** are the stronger statement, and they are exact: they
fold every entry booked on or before the period's last day and nothing else.
Sealing March in April is the normal case, and April must not leak into March's
closing balance. The [watermark](#the-sealed-watermark) is what keeps that exact
in the other direction — nothing can be added *below* the period's last day
afterwards either.

### The ledger is inside the hash

A seal carries its `LedgerId` in the preimage, not beside it. Two ledgers can
record the same amounts on the same accounts on the same day — the entry hash
covers what an entry says, not which book it landed in — so their tree heads and
trial-balance roots can coincide exactly.

Without the ledger in the hash their seals would be byte-identical, and a seal
handed to an auditor would attest to no one in particular. With it, relabelling a
seal invalidates it rather than transferring it, and a `SealChain` rejects a seal
from another ledger outright.

## Proving one balance

A seal commits to the closing trial balance as a Merkle **root**, not a flat
digest, and that choice is what makes selective disclosure possible. A digest can
only be checked by whoever holds every balance — which is precisely the party an
auditor is trying not to have to trust. A root lets one row be proven in
`O(log n)`, revealing nothing about the others.

```rust
use doubleentry::seal::TrialBalanceCommitment;

// Rebuild the commitment from the same closing balances the seal was built over.
let closing    = journal.trial_balance_through_date(date!(2026-03-31))?;
let commitment = TrialBalanceCommitment::of(&closing);
assert_eq!(commitment.head(), seal.trial_balance);

let proof = commitment.prove(&cash_key).expect("cash was posted to");

// The auditor holds the seal and this one proof. Nothing else.
assert!(proof.verify_against(&seal));
```

Both gross totals are in the leaf, not the net: two accounts that net to zero —
one quiet, one with heavy offsetting turnover — must not produce the same
commitment. An account with no postings has no row, so `prove` returns `None`
rather than manufacturing a proof that it held zero; absence and a zero balance
are different claims.

## Naming the account

A trial-balance leaf names its account by **handle** — a dense integer, chosen so
comparisons and lookups are cheap. On its own that makes the proof above a
statement about an integer: the auditor learns handle `#0` held €1190.00 and has
to take your word for what `#0` is.

Worse, nothing would stop you changing the answer later. Re-registering the same
paths in a different order renumbers every handle, and every seal, every balance
proof and the whole chain would go on verifying byte for byte while each balance
quietly referred to a different account — exactly the alteration a seal exists to
expose.

So a seal commits to the account registry too:

```rust
// `_at`, not the bare form: the seal recorded the commitment the registry had
// when the period closed, and the registry has grown since.
let binding = journal
    .accounts()
    .prove_binding_at(cash, seal.accounts.size)
    .expect("issued by then");

// The complete claim: this account, this balance, this period — from the seal
// and two O(log n) paths, disclosing nothing about any other account.
assert!(proof.verify_naming(&binding, &seal));
assert_eq!(binding.path().to_string(), "Assets:Cash");
```

`verify_naming` refuses a binding for a different handle than the balance is for,
so a genuine balance cannot be presented under another account's name.

### The leaf covers identity, not master data

The binding leaf hashes the **handle and the path** and nothing else. That is the
account's identity, and it is exactly the part that never changes — `restore`
refuses to move a path onto a handle that already holds a different one, because
a rebound handle repoints every posting row that names it.

The classification, open window and balance limit are deliberately out of it.
Those are master data: `close`, `reopen` and `set_limit` exist to change them,
and like an account's open window they govern what may be booked next rather than
what was booked already. Hashing them in meant that closing an account on a
Tuesday invalidated every binding proof against every seal ever issued — a
routine operation reading as evidence of tampering.

What the commitment does still pin is the thing that matters: re-registering the
same paths in a different order renumbers every handle, and the `accounts` head
moves, so the seal that named the old numbering stops matching.

### Proving it through a store

Assembled by hand this is five steps of which exactly one matters — comparing
your rebuilt commitment against the one the seal recorded — and it is the step
nothing forces. Skip it and you hold a proof against a commitment you computed
yourself: internally consistent, and evidence of nothing.

`prove_sealed_balance` does the whole thing, including that comparison, and
errors rather than handing back a proof if the rebuild does not match. It is on
`Journal` and on `LedgerStore` alike, and both route through the same
`SealedBalance::assemble`, so a backend cannot drift from the engine:

```rust
let proven = journal.prove_sealed_balance(&march, cash_key)?
    .expect("cash has a row in the closing trial balance");

assert!(proven.verify());
assert_eq!(proven.path().to_string(), "Assets:Cash");
```

A `SealedBalance` serialises, which is the point of bundling it: the artifact
exists to leave the process that built it. The recipient holds the seal, the
balance proof and the binding proof and checks all three with one `verify()`,
having none of the books.

Three answers are worth telling apart, and it does:

| Outcome | Meaning |
|---|---|
| `Ok(None)` | The account is registered but has no row — *not* a balance of zero |
| `NotYetRegistered` | The account was onboarded after the seal, so it cannot name it |
| `Restated` | The books no longer reproduce the seal's closing balance |

Note the fold it rebuilds with: a closing balance is cumulative by **booking
date** (`trial_balance_through_date`), not a prefix of the log by position
(`trial_balance`). Sealing March in April is the normal case, so at the moment
the seal is taken the log already holds April entries and the two answers
differ.

## The chain

`SealChain` enforces every rule that relates a seal to its predecessor:

- Each seal hashes to its own contents (`Tampered` otherwise).
- Each references the previous seal's hash (`BrokenChain`).
- Exactly the first seal may omit a predecessor (`MisplacedGenesis`).
- Tree heads never shrink (`NonMonotonic`).
- The account registry never shrinks (`ShrunkenRegistry`) — handles are dense
  positions and are never reissued, so a seal committing to *fewer* bindings than
  its predecessor is a registry rebuilt from a truncated set, which renumbers the
  handles every earlier balance is keyed on.
- No period is sealed twice (`DuplicatePeriod`).
- Every seal names the same ledger (`ForeignLedger`).

Appending and re-verifying share one implementation, so a chain cannot accept a
link it would later reject. Verification is linear in the number of seals, which
matters for a ledger on daily periods: the periods seen so far are carried along
rather than rescanned at every position.

```rust
journal.verify_seals()?;    // every seal, every link
```

## What a seal does and does not do

A seal detects **alteration**, not **access**. Preventing writes is the storage
layer's job — revoke `UPDATE` and `DELETE`, which the
[PostgreSQL backend](@/docs/persistence.md) supports. Making a write recognisable
afterwards is the seal's job.

Publishing a seal or a tree head — to an auditor, a timestamping service, or any
append-only location — is what turns detectable tampering into tampering
detectable *by someone else*. A seal you hold and never publish only protects you
against your own future mistakes.
