+++
title = "Proofs"
description = "Inclusion, consistency and sealed-balance proofs against a tree head, and what they read."
weight = 2
+++

## What to call

- [`Journal::prove_entry`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.prove_entry) → [`InclusionProof`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.InclusionProof.html); [`InclusionProof::verify`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.InclusionProof.html#method.verify)`(leaf_hash, head)` and [`InclusionProof::verify_record`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.InclusionProof.html#method.verify_record)`(head, leaf_bytes)`.
- [`Journal::prove_consistency`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.prove_consistency) → [`ConsistencyProof`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.ConsistencyProof.html); [`ConsistencyProof::verify`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.ConsistencyProof.html#method.verify)`(old, new)`.
- [`Journal::prove_balance`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.prove_balance) → [`SealedBalanceProof`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.SealedBalanceProof.html); [`SealedBalanceProof::verify`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.SealedBalanceProof.html#method.verify)`(head)` → [`ProvenBalance`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.ProvenBalance.html).
- [`Journal::head`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.head), [`Journal::head_at`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.head_at), [`Journal::prove_entry_at`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.prove_entry_at) — proofs at an earlier size.
- [`ProofError`](https://docs.rs/doubleentry/latest/doubleentry/log/enum.ProofError.html) — why a proof failed.

[`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.prove_entry)
has the same methods, `async`, and returns the same proofs.

## Three questions a proof answers

| Proof | Answers | Verifies against |
|---|---|---|
| inclusion | this record is at this position of the log | one tree head |
| consistency | the log at this size is a prefix of the log at that size — nothing was rewritten or removed | two tree heads |
| sealed balance | this account had this gross balance in this (currency, layer) at the end of this sealed period | one tree head |

Every proof verifies against a [`TreeHead`](https://docs.rs/doubleentry/latest/doubleentry/struct.TreeHead.html),
size **and** root, never a bare root: a size taken from the proof itself would
be a number the prover chose.

## Inclusion

```rust
use doubleentry::prelude::*;
use doubleentry::record::Record;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=4)?;
let sale = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;

let head = books.head();
let proof = books.prove_entry(sale.id)?;
assert_eq!((proof.index, proof.size), (sale.position, head.size));

// The verifier hashes the record itself; nothing is taken on trust.
let entry = books.entry(sale.id).ok_or("recorded")?;
let record: Record = Record::Entry(entry);
proof.verify(&record.leaf_hash(), &head)?;
let proven: Record = proof.verify_record(&head, &record.to_leaf_bytes())?;
assert_eq!(proven, record);

// Another record, or another head, does not verify.
let genesis = books.leaves().next().ok_or("genesis")?;
assert!(proof.verify(&doubleentry::log::leaf_hash(genesis.bytes), &head).is_err());
```

`verify_record` hashes and decodes the leaf bytes, so the verifier holds the
record the proof is about; an entry's id derives from the key the leaf commits
to.

## Consistency, and proofs at earlier sizes

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let sale = books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("10.00"), EUR)
        .credit("Income:Sales", amount!("10.00"), EUR),
)?;
let then = books.head(); // what an auditor was shown in March

books.record(
    Entry::new("sale-2", date!("2026-03-20"))
        .debit("Assets:Cash", amount!("5.00"), EUR)
        .credit("Income:Sales", amount!("5.00"), EUR),
)?;
let now = books.head();

// The March head is a prefix of today's.
books.prove_consistency(then.size, now.size)?.verify(&then, &now)?;

// A proof against the March head, produced today.
assert_eq!(books.head_at(then.size)?, then);
let old = books.prove_entry_at(sale.id, then.size)?;
assert_eq!(old.size, then.size);

// Vacuous proofs are refused: nothing is proven from the empty tree.
assert!(matches!(books.prove_consistency(0, now.size), Err(Error::Query(_))));
```

Stored nodes never change once their subtree is complete, so every store
proves against **any earlier size**: an auditor who archived a head in March
gets proofs against exactly that head in December.

Vacuous proofs are refused: an inclusion proof into the empty tree
([`ProofError::EmptyTree`](https://docs.rs/doubleentry/latest/doubleentry/log/enum.ProofError.html#variant.EmptyTree))
and a consistency proof from it
([`ProofError::EmptyOldTree`](https://docs.rs/doubleentry/latest/doubleentry/log/enum.ProofError.html#variant.EmptyOldTree)),
as RFC 9162 defines consistency only for `0 < m ≤ n`. A ledger's log is never
empty (position 0 is its genesis).

## Sealed balance

A seal commits, per (currency, layer), to a tree whose leaf `i` is account
handle `i`'s cumulative gross balance `(debit, credit, postings)` through the
period's last day; accounts without postings get the canonical zero leaf. A
sealed-balance proof is three ordinary RFC 6962 paths:

1. the balance leaf at the account's handle in the seal's tree — the verifier
   checks `index == handle` and `size == the seal's account count`, so there is
   exactly one row per account and "no row" is a sound answer;
2. the seal record's inclusion in the log;
3. the inclusion of the account's `Account` registration record — which carries
   the handle and the path, so the proof **names** the account. The registration
   must lie at or before the seal's `through`.

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;

// Before the seal there is nothing to prove.
assert!(matches!(
    books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-03"),
    Err(Error::Query(_))
));

books.seal_period("2026-03")?;
let proof = books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-03")?;
let proven = proof.verify(&books.head())?;
assert_eq!(proven.account.to_string(), "Assets:Cash");
assert_eq!(proven.kind, AccountKind::Asset);
assert_eq!(proven.balance.net(), amount!("1190.00"));
assert_eq!(proven.period.to_string(), "2026-03");
assert_eq!(proven.head, books.head());
```

The recipient learns one account's balance and the seal's totals per
(currency, layer), nothing about other accounts. As one signed, offline file:
a [proof bundle](@/docs/assurance/transparency.md#proof-bundles).

A zero balance
in a (currency, layer) the seal has no tree for is proven by that absence (no
balance-tree path); it is no evidence that the currency was ever used.

## Cost

- Appending is amortised `O(1)` hashes.
- An inclusion or consistency proof at size `n` reads `O(log n)` stored nodes,
  planned in advance and fetched in **one** batched
  [`Store::read_nodes`](https://docs.rs/doubleentry/latest/doubleentry/store/trait.Store.html#tymethod.read_nodes),
  at the current or any earlier size.
- A sealed-balance proof reads the account's registration leaf, the seal leaf
  and the stored nodes of one balance-tree path, never the period's postings.
- A path has at most 63 hashes
  ([`MAX_PROOF_HASHES`](https://docs.rs/doubleentry/latest/doubleentry/log/constant.MAX_PROOF_HASHES.html));
  sizes above 2⁶² are refused.

A store verifies a proof before handing it out: tampered stored nodes give
`Error::Integrity`, not a proof that fails later.

## Limits

- A proof shows what **one** tree head contains. An operator can show two
  consistent heads to two parties; only a
  [witness](@/docs/assurance/transparency.md) closes that gap.
- A sealed-balance proof shows the gross balance as sealed, not that the
  bookkeeping behind it was right (that is the [audit](@/docs/assurance/audit.md)'s
  and your auditor's question).
- A head carries no timestamp.
