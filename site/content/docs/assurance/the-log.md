+++
title = "The log"
description = "One RFC 6962 SHA-256 Merkle log of typed records, its canonical encoding, and what a position means."
weight = 1
+++

## What to call

- [`Journal::head`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.head) and [`Ledger::head`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.head) — the log's [`TreeHead`](https://docs.rs/doubleentry/latest/doubleentry/struct.TreeHead.html): its size and root.
- [`Journal::leaves`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.leaves) — every leaf, in position order, with its side data.
- [`record::Record`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.Record.html) — the eight record kinds; [`Record::to_leaf_bytes`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.Record.html#method.to_leaf_bytes), [`Record::decode`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.Record.html#method.decode), [`Record::leaf_hash`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.Record.html#method.leaf_hash).
- [`record::RecordKind`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.RecordKind.html), [`record::MAX_LEAF_BYTES`](https://docs.rs/doubleentry/latest/doubleentry/record/constant.MAX_LEAF_BYTES.html), [`record::FORMAT`](https://docs.rs/doubleentry/latest/doubleentry/record/constant.FORMAT.html).
- [`log::Hash`](https://docs.rs/doubleentry/latest/doubleentry/log/struct.Hash.html), [`log::leaf_hash`](https://docs.rs/doubleentry/latest/doubleentry/log/fn.leaf_hash.html), [`log::node_hash`](https://docs.rs/doubleentry/latest/doubleentry/log/fn.node_hash.html), [`log::root_of`](https://docs.rs/doubleentry/latest/doubleentry/log/fn.root_of.html), [`log::empty_root`](https://docs.rs/doubleentry/latest/doubleentry/log/fn.empty_root.html).

## One log of typed records

A ledger is one append-only Merkle tree; everything that changes the books is a
leaf of it. No side table holds authority.

| Kind byte | Record | Written by |
|---|---|---|
| `0x01` | `Genesis` — the ledger id (the C2SP origin), the scale `P`, the format tag `doubleentry/1` | creating the ledger; always position 0 |
| `0x02` | `Account` — a registration (handle, path, kind, class, open window, limit) or a change of open window, limit or class; each with `effective` and `actor` | `open`, `register`, `close_account`, `reopen_account`, `set_limit`, `set_class` |
| `0x03` | `Currency` — a code and its exponent, with `effective` and `actor` | `declare_currency` |
| `0x04` | `Policy` — a full policy snapshot (required dimensions, value-date drift), with `effective` and `actor` | `set_policy` |
| `0x05` | `Period` — id, first and last day, contiguous with the period before | `define_period`, `define_months` |
| `0x06` | `Entry` — key, booking and value date, capture time, kind, memo commitment, documents, postings, reversal link, hold action, rates | `record`, `record_batch`, `reversal`, the hold constructors, `close_year` |
| `0x07` | `Clearing` — an apply or a reset of an open-item match | `clear`, `reset_clearing` |
| `0x08` | `Seal` — period, first and last day, `through`, account count, and per (currency, layer) a balance-tree root and totals | `seal_period`, `seal_through` |

A record's **position** is its leaf index: `Recorded::position`, a seal's
`through`, an export row's `position` and an inclusion proof's `index` are the
same number.

```rust
use doubleentry::log::{leaf_hash, root_of};
use doubleentry::prelude::*;
use doubleentry::record::{FORMAT, MAX_LEAF_BYTES, Record, RecordKind};

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))?;
let sale = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;

let mut kinds = Vec::new();
let mut hashes = Vec::new();
for leaf in books.leaves() {
    assert!(leaf.bytes.len() <= MAX_LEAF_BYTES);
    let record: Record = Record::decode(leaf.bytes)?;
    assert_eq!(record.to_leaf_bytes(), leaf.bytes); // one byte form per record
    kinds.push(record.kind());
    hashes.push(leaf_hash(leaf.bytes));
}
assert_eq!(
    kinds,
    [
        RecordKind::Genesis,
        RecordKind::Currency,
        RecordKind::Account,
        RecordKind::Account,
        RecordKind::Period,
        RecordKind::Entry,
    ]
);
assert_eq!(sale.position, 5);
let head = books.head();
assert_eq!(head.size, 6);
assert_eq!(head.root, root_of(&hashes));

let genesis = books.leaves().next().ok_or("a ledger starts with its genesis")?;
assert!(genesis.bytes.ends_with(FORMAT.as_bytes())); // "doubleentry/1"
```

## The tree

The tree is [RFC 6962](https://www.rfc-editor.org/rfc/rfc6962) exactly, with
SHA-256 and nothing else:

- `leaf_hash = SHA-256(0x00 ‖ leaf_bytes)`
- `node_hash = SHA-256(0x01 ‖ left ‖ right)`
- the empty tree's root is `SHA-256("")`

Stock witnesses, monitors and `transparency-dev/merkle` verify its proofs
without knowing anything about bookkeeping. A
[`TreeHead`](https://docs.rs/doubleentry/latest/doubleentry/struct.TreeHead.html)
is a size and a root. Stored nodes follow Go `tlog`'s numbering: a node is
stored once its subtree is complete (`2n − popcount(n)` nodes for `n` leaves)
and never changes, so every [proof](@/docs/assurance/proofs.md) can be produced
at any earlier size.

## Leaf bytes and the canonical encoding

`leaf_bytes = kind (u8) ‖ canonical(record)`, at most **65 535 bytes**
([`MAX_LEAF_BYTES`](https://docs.rs/doubleentry/latest/doubleentry/record/constant.MAX_LEAF_BYTES.html)) —
the `tlog-tiles` entry-bundle and Go proof-line limit. A draft that would encode
to more is refused before it reaches the log (rejection code 111), and
`Record::decode` refuses longer input before reading it.

The canonical encoding gives every value exactly one byte form:

- integers are fixed-width little-endian; no varints;
- text and byte strings carry a `u64` byte-length prefix; text is UTF-8 in NFC;
- dates are the `i32` Julian day number;
- an `Option` is `0x00`, or `0x01` followed by the value;
- a sequence is a `u64` element count followed by its elements, in the one order
  the model keeps (postings by index, dimensions and rates sorted).

It is written by hand, never by serde, so no library version can change a hash.
`Record::decode` is its strict inverse: truncation, trailing bytes, unknown tags,
non-NFC text, out-of-range dates, over-long counts and values a constructor would
refuse are each a typed
[`DecodeError`](https://docs.rs/doubleentry/latest/doubleentry/record/enum.DecodeError.html);
it never panics.

The genesis record of `example.com/acme` at `P = 2`, byte by byte:

```text
01                                  kind: Genesis
10 00 00 00 00 00 00 00             u64 length 16
65 78 61 6d 70 6c 65 2e 63 6f 6d    "example.com/acme"
2f 61 63 6d 65
02                                  P = 2
0d 00 00 00 00 00 00 00             u64 length 13
64 6f 75 62 6c 65 65 6e 74 72 79    "doubleentry/1"
2f 31
```

```rust
use doubleentry::LedgerId;
use doubleentry::record::{Genesis, Record};

let genesis: Record = Record::Genesis(Genesis { ledger: LedgerId::new("example.com/acme")? });
assert_eq!(
    genesis.leaf_hash().to_string(),
    "cd615a0dac3171f8c156c2f65bc613bf9d1a5381874de8ca201b508d203e83d5"
);
assert_eq!(Record::decode(&genesis.to_leaf_bytes()), Ok(genesis));
```

Hashes that are not tree hashes — the derived `EntryId`, the memo commitment and
the draft fingerprint — use length-prefixed domain tags (`doubleentry/1/entry-id`,
`doubleentry/1/memo`, `doubleentry/1/fingerprint`), so a value hashed for one
purpose is never a valid hash for another. A seal's balance-tree leaf starts with
its own kind byte `0x81`, which no log record uses.

## The format tag

Every genesis record commits to [`FORMAT`](https://docs.rs/doubleentry/latest/doubleentry/record/constant.FORMAT.html)
= `doubleentry/1`. A decoder refuses a genesis with another tag, or with a scale
other than the type it is decoded as. The crate version is not committed
anywhere; seals carry no engine version.

Every record kind's leaf bytes and hash, a reference log root and the
non-tree hashes are pinned in `tests/vectors/golden.txt`, produced by an
independent reference encoder; see [Testing](@/docs/assurance/testing.md).

## Limits

- The log proves **order and content**, not time: a capture time is whatever the
  caller supplied.
- A change is **detectable**, not impossible: [audit](@/docs/assurance/audit.md)
  catches an inconsistent rewrite, only a
  [witnessed checkpoint](@/docs/assurance/transparency.md) a consistent one.
- A ledger in another format is recorded anew, never converted (see
  [Stores](@/docs/operating/stores.md#schema-identifier-no-migrations)).
