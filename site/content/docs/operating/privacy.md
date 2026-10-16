+++
title = "Privacy"
description = "Memo erasure for GDPR Art. 17, and why every other field must not carry personal data."
weight = 5
+++

## What to call

- Write a memo: [`Entry::memo`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.memo)
  (salt from the operating system) or [`Entry::memo_salted`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.memo_salted)
  with a [`MemoSalt`](https://docs.rs/doubleentry/latest/doubleentry/struct.MemoSalt.html).
- Read it: [`memo`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.memo)`(id)`.
  Erase it: [`erase_memo`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.erase_memo)`(id)`
  (also on [`Journal`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.erase_memo)).
- What stays in the leaf: [`MemoCommitment`](https://docs.rs/doubleentry/latest/doubleentry/record/struct.MemoCommitment.html).

## The memo is the one erasable field

The memo (the GoBD *Buchungstext*) is free text, so it may hold personal data.
It is kept **outside** the hash:

- the leaf holds only `SHA-256(tag ‖ salt ‖ text)` with a 16-byte random salt,
  so a short memo cannot be guessed back from it;
- text and salt are stored beside the leaf;
- `erase_memo(id)` deletes text, salt and the entry's draft fingerprint without
  changing any leaf, hash, proof, seal or checkpoint.

A memo is 1–4 096 bytes; tabs and line breaks are allowed, other control
characters are refused (108).

```rust
use doubleentry::export::{ExportRow, ExportScope, verify};
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let refund = Entry::new("pay-7f3a", date!("2026-03-15"))
    .debit("Assets:Bank", amount!("40.00"), EUR)
    .credit("Income:Sales", amount!("40.00"), EUR)
    .memo("goodwill refund, Ms. Example, broken parcel");
let rec = books.record(refund.clone())?;
let head = books.head();
let proof = books.prove_entry(rec.id)?;

books.erase_memo(rec.id)?;
assert_eq!(books.memo(rec.id), None);
assert_eq!(books.head(), head); // nothing hashed changed
let leaf = books.leaves().nth(usize::try_from(rec.position)?).ok_or("no leaf")?;
proof.verify_record::<2>(&head, leaf.bytes)?; // the old proof still verifies
books.audit()?;

// The export still verifies; the entry row has its commitment, not its text.
let rows: Vec<ExportRow> = books.export(ExportScope::All, None)?.collect::<Result<_>>()?;
assert!(rows.iter().any(|row| matches!(row, ExportRow::Entry(e) if e.memo_commitment.is_some() && e.memo_text.is_none())));
verify(rows, &head)?;

// A retry can no longer be recognised: the key conflicts.
assert_eq!(books.record(refund).err().and_then(|e| e.code()), Some(200)); // KeyConflict
```

Erasing again, or erasing an entry without a memo, changes nothing; an unknown
id is `Error::Query` (`UnknownEntry`). On the SQL stores a trigger allows exactly
this change and nothing else (see [PostgreSQL](@/docs/operating/postgresql.md)).

The fingerprint goes too because idempotency compares a retry with the first
draft, memo included: kept, it would confirm a guessed memo through a replay.
After erasure a submission under that key is a `KeyConflict`, so erase only
long after the last possible retry.

## Every other field is permanent

Everything else is committed in clear: in the leaf, in every proof, tile and
bundle, and in the export. Keep personal data out of:

- the **idempotency key** (and so the entry id): use an opaque source id,
  never an e-mail address or a name;
- **account paths**: `Liabilities:Wallets:7f3a`, not `…:jane.doe`;
- **labels**: dimension axes and values, entry kinds, document types, actors,
  period ids;
- **document references**: a number or a hash, not a file path with a name.

Keep the mapping from these ids to a person in **your** system, where it can be
deleted.

## When to erase

GDPR Art. 17(3)(b) lets legal retention duties override erasure. § 147 AO keeps
books and journals for 10 years and *Buchungsbelege* for 8; the memo is part of
the booking record. The usual course is to erase memos when retention ends. The
crate has no clock and no retention schedule: your application calls
`erase_memo` per entry. See [GoBD](@/docs/assurance/gobd.md).

## Logs

With the `tracing` feature, spans carry the ledger id, at most 64 bytes of a
key, outcomes, codes and positions; never an amount, a memo, an account path or a
document reference. Error messages never contain memo text.

## Limits

- Erasing a memo does not anonymise an entry whose keys, paths or labels
  identify a person.
- Backups, exports and replicas you already made keep the text until you erase
  there too or they expire.
- No legal advice.
