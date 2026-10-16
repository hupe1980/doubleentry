+++
title = "Entries"
description = "Drafts by path, postings, layers, dimensions, documents, memo commitments, rates, value dates."
weight = 3
+++

## What to call

[`Entry`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html) — `Entry::new`, `debit`,
`credit`, `posting`, `kind`, `value_date`, `captured_at`, `document`, `rate`, `memo`;
[`Posting`](https://docs.rs/doubleentry/latest/doubleentry/struct.Posting.html) with `dimension`;
[`Document`](https://docs.rs/doubleentry/latest/doubleentry/struct.Document.html),
[`Rate`](https://docs.rs/doubleentry/latest/doubleentry/struct.Rate.html),
[`CaptureTime`](https://docs.rs/doubleentry/latest/doubleentry/struct.CaptureTime.html).
Record with `record` or `record_batch`; read back with `entry` and `memo`;
check a form with [`validate`](https://docs.rs/doubleentry/latest/doubleentry/fn.validate.html).

## A draft

`Entry::new(key, booking_date)` starts a draft under an
[idempotency key](@/docs/concepts/idempotency.md). `debit` and `credit` add a
posting: an account by path or handle, a positive amount, a currency. An entry
has 2 to 1 024 postings and balances per (currency, layer).

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
let cash = books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
books.open("Liabilities:VAT", AccountKind::Liability, date!("2026-01-01"))?;

let recorded = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .kind("invoice")
        .debit(cash, amount!("1190.00"), EUR) // by handle
        .credit("Income:Sales", amount!("1000.00"), EUR) // by path
        .credit("Liabilities:VAT", amount!("190.00"), EUR),
)?;
let stored = books.entry(recorded.id).ok_or("recorded")?;
assert_eq!(stored.parts().postings.len(), 3);
assert_eq!(stored.parts().value_date, date!("2026-03-15")); // defaults to the booking date
```

## Builders never fail

No builder returns a `Result`. An invalid key, path, label or dimension stays in
the draft and `record` reports it as a coded `Rejection`. `validate(&draft)`
lists every **structural** violation in precedence order without reading the
ledger — for a form; periods, accounts and limits are only checked by `record`.

```rust
use doubleentry::prelude::*;
use doubleentry::validate;

let draft: Entry = Entry::new("", date!("2026-03-15")) // empty key: 100
    .debit("Assets::Cash", amount!("10.00"), EUR) // empty segment: 103
    .credit("Income:Sales", amount!("9.00"), EUR); // unbalanced: 106
let codes: Vec<u16> = validate(&draft).iter().map(Rejection::code).collect();
assert_eq!(codes, [100, 103, 106]);
```

| Limit | Value | Code |
|---|---|---|
| postings per entry | 2 – 1 024 | 101 / 102 |
| dimension axes per posting | 8 | 105 |
| memo | 1 – 4 096 bytes | 108 |
| documents / rates per entry | 16 / 16 | 109 / 110 |
| encoded entry (the log leaf) | 65 535 bytes | 111 |

The leaf limit is the C2SP proof-line limit; `Entry::encoded_len` computes the
size before submitting.

## Postings, layers, dimensions

`Posting::debit` / `Posting::credit` build a posting with up to eight
**dimensions**, added with `.posting(…)`:
`.posting(Posting::debit("Expenses:Travel", amount!("80.00"), EUR).dimension("project", "alpha"))`. Dimensions slice [reports](@/docs/reporting/dimensions.md); a
ledger policy can require axes (307).

Every posting a caller writes is on the **Settled** layer. The **Pending** layer
is written only by [holds](@/docs/concepts/holds.md), so an account's Pending
balance is exactly what its open holds reserve.

## Documents, rates, dates

- **Document** — `Document::new(type, reference)`, reference up to 256 bytes,
  optionally `dated(…)` and `with_sha256(…)` of the original's bytes.
- **Rate** — `Rate::parse(base, quote, "1.0873")`: `quote` per unit of `base`,
  recorded as evidence; nothing is converted.
- **Value date** — when money moves; defaults to the booking date. A policy
  can bound the drift (308).
- **Capture time** — `CaptureTime::new(unix_seconds, utc_offset_minutes)`: the
  caller's statement; the crate reads no clock.

```rust
use doubleentry::prelude::*;
use doubleentry::CaptureTime;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.declare_currency(USD, 2, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.open("Assets:Bank:USD", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;

let digest = [0x5a; 32]; // SHA-256 of the invoice XML
let recorded = books.record(
    Entry::new("invoice-2026-0042", date!("2026-03-15"))
        .value_date(date!("2026-03-17"))
        .captured_at(CaptureTime::new(1_773_568_800, 60)?)
        .document(Document::new("invoice", "RE-2026-0042")?.dated(date!("2026-03-15")).with_sha256(digest))
        .rate(Rate::parse(EUR, USD, "1.0873")?)
        .debit("Assets:Bank:USD", amount!("108.73"), USD)
        .credit("Income:Sales", amount!("108.73"), USD),
)?;
let stored = books.entry(recorded.id).ok_or("recorded")?;
assert_eq!(stored.parts().documents[0].sha256(), Some(&digest));
assert_eq!(stored.parts().rates[0].to_string(), "1.0873");
```

## The memo is a commitment

The memo (1–4 096 bytes; tabs and line breaks allowed) is the one free-text
field, so the one that may hold personal data. The leaf holds only a salted
commitment, `SHA-256(tag ‖ salt ‖ text)` with a 16-byte salt; text and salt are
kept beside it. `erase_memo(id)` removes both and changes no hash
([Privacy](@/docs/operating/privacy.md)). The façade draws the salt from the
operating system unless the draft carries one (`memo_salted`) or the façade has
a salt source (`with_salt_source`, for tests). A retry still replays: the text
is compared, not the salt.

```rust
use doubleentry::prelude::*;
use doubleentry::MemoSalt;

let mut books = Journal::new(LedgerId::new("example.com/acme")?).with_salt_source(|| MemoSalt([7; 16]));
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 3..=3)?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;

let sale = books.record(
    Entry::new("sale-1", date!("2026-03-15"))
        .memo("Kunde: Erika Mustermann")
        .debit("Assets:Cash", amount!("10.00"), EUR)
        .credit("Income:Sales", amount!("10.00"), EUR),
)?;
assert_eq!(books.memo(sale.id).as_deref(), Some("Kunde: Erika Mustermann"));
let head = books.head();
books.erase_memo(sale.id)?;
assert_eq!((books.memo(sale.id), books.head()), (None, head)); // no hash changed
books.audit()?;
```

## Limits

- `validate` checks the draft alone; an empty list does not mean `record` will
  accept it.
- Dates, capture times and document hashes are the caller's statements; the
  crate fetches and stores no document.
- Labels and references are not screened for personal data; keep it out of
  them.
