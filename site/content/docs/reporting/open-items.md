+++
title = "Open items"
description = "Clearings as log records: apply and reset, and the open items an account still carries."
weight = 5
+++

## What to call

- Match a debit with a credit: [`clear`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.clear)`(key, debit, credit, amount, on)`,
  each posting named by a [`PostingRef`](https://docs.rs/doubleentry/latest/doubleentry/struct.PostingRef.html)
  ([`PostingRef::at`](https://docs.rs/doubleentry/latest/doubleentry/struct.PostingRef.html#method.at)`(entry, index)`).
- Undo it: [`reset_clearing`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.reset_clearing)`(key, clearing, on)`.
- Read what is open: [`open_items`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.open_items)`(account, currency, &query)` →
  [`OpenItem`](https://docs.rs/doubleentry/latest/doubleentry/report/struct.OpenItem.html)s.
- The same on [`Ledger`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html#method.clear), `async`.

## Clearings are records

An account's balance says how much is owed; **open items** say which invoice is
still unpaid, and by how much. The link between an invoice's debit and a
payment's credit is a **clearing**: a record of the log (kind 7) with its own
idempotency key and date, covered by the tree head, proofs and `audit()`. What is
applied to a posting is a fold of the clearing records, the applies minus those
reset; nothing is updated in place.

```rust
use doubleentry::prelude::*;

const AR: &str = "Assets:Receivables:Customer-42";
let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 3..=4)?;
books.open(AR, AccountKind::Asset, date!("2026-01-01"))?;
books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;

let invoice = books.record(
    Entry::new("INV-1", date!("2026-03-01"))
        .debit(AR, amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;
let payment = books.record(
    Entry::new("PAY-1", date!("2026-03-10"))
        .debit("Assets:Bank", amount!("500.00"), EUR)
        .credit(AR, amount!("500.00"), EUR),
)?;
let (debit, credit) = (PostingRef::at(invoice.id, 0), PostingRef::at(payment.id, 1));

let early = books.clear("CLR-1", debit, credit, amount!("500.00"), date!("2026-03-05"));
assert_eq!(early.err().and_then(|e| e.code()), Some(415)); // before the payment was booked
let too_much = books.clear("CLR-1", debit, credit, amount!("600.00"), date!("2026-03-10"));
assert_eq!(too_much.err().and_then(|e| e.code()), Some(414)); // the payment has only 500.00

let clearing = books.clear("CLR-1", debit, credit, amount!("500.00"), date!("2026-03-10"))?;
assert_eq!(books.clear("CLR-1", debit, credit, amount!("500.00"), date!("2026-03-10"))?, clearing); // a replay

let open = books.open_items(AR, EUR, &BalanceQuery::all())?;
assert_eq!(open.len(), 1); // the payment is fully applied, the invoice is not
assert_eq!((open[0].original, open[0].applied, open[0].residual), (amount!("1190.00"), amount!("500.00"), amount!("690.00")));
assert_eq!(open[0].posting_ref(), debit);

books.reset_clearing("RST-1", clearing, date!("2026-04-02"))?; // the payment bounced
let march = books.open_items(AR, EUR, &BalanceQuery::through(date!("2026-03-31")))?;
assert_eq!(march.iter().map(|item| item.residual).collect::<Vec<_>>(), [amount!("690.00")]);
let now = books.open_items(AR, EUR, &BalanceQuery::all())?;
assert_eq!(now.iter().map(|item| item.residual).collect::<Vec<_>>(), [amount!("1190.00"), amount!("500.00")]);
```

`clear` returns the clearing's position. A retry with the same key and
arguments returns it again and writes nothing; the same key with other arguments
is `KeyConflict` (200). A full program:
[examples/invoice_clearing.rs](https://github.com/hupe1980/doubleentry/blob/main/examples/invoice_clearing.rs).

## Refusals

| Code | Rejection | When |
|---|---|---|
| 120 | `NonPositiveClearingAmount` | the amount is zero or negative |
| 300 / 301 | `NoPeriod` / `PeriodSealed` | the date is in no period, or a sealed one |
| 410 | `ClearingTargetUnknown` | a `PostingRef` names no posting |
| 411 | `ClearingSideMismatch` | the debit reference is not a debit, or the credit not a credit |
| 412 | `ClearingKeyMismatch` | the postings differ in account, currency or layer |
| 413 | `ClearingPrecision` | more decimals than the currency's exponent |
| 414 | `OverApplied` | more than is still open on either posting |
| 415 | `ClearingBeforePosting` | dated before either posting's booking date |
| 416 | `ClearingUnknown` | a reset names no apply |
| 417 | `ClearingAlreadyReset` | the apply was already reset |
| 418 | `ResetBeforeClearing` | a reset dated before its apply |
| 419 | `ClearingNotSettled` | the postings are Pending (a hold's), not Settled |

A reset releases one apply whole; the amounts can then be cleared again under a
new key.

## Reading open items

`open_items` returns every selected posting not fully offset, by (position,
posting index). Each `OpenItem` has the entry id and position, posting index,
booking and value date, side, `original`, `applied` and `residual` (always
positive). The query sets "as of when":

- `through(date)`, `between`, `period`: the postings in the window and only the
  clearings and resets **dated** on or before its end;
- `through_position(n)`, `pinned(head)`: only what was in the log then;
- dimension clauses narrow the postings.

The default layer is Settled. `.pending()` lists a hold's Pending legs, which are
never cleared; read holds with `hold` and `holds_due`.

## Limits

- No automatic matching: your code decides which postings belong together.
- No payment terms, dunning or ageing buckets; `OpenItem` gives the dates and
  amounts to build them.
- The [export](@/docs/reporting/export.md) carries clearings as leaf hashes
  only, not their content.
