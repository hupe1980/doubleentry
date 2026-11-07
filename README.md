# 📒 doubleentry

[![Crates.io](https://img.shields.io/crates/v/doubleentry.svg)](https://crates.io/crates/doubleentry)
[![Docs.rs](https://img.shields.io/docsrs/doubleentry)](https://docs.rs/doubleentry)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#-license)
[![MSRV](https://img.shields.io/badge/rustc-1.94+-orange.svg)](https://www.rust-lang.org/)

> **An immutable, tamper-evident double-entry bookkeeping engine.**

A ledger is one entity's books, kept as one append-only RFC 6962 SHA-256 Merkle log of typed
records: genesis, currencies, accounts, policies, periods, entries, clearings and seals. One
pure function, the *rule core*, decides every write: it returns the records to append or one
`Rejection` with a stable code. Two façades call it with the same methods:
[`Journal`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html) (sync, in
memory) and [`Ledger<S>`](https://docs.rs/doubleentry/latest/doubleentry/struct.Ledger.html)
(async, over SQLite, PostgreSQL or your own `Store`).

Why: a ledger should not leave its invariants to the application, and it should be able to
answer an auditor's question — *how do I know this record was not changed afterwards?* —
with a proof instead of a promise.

📖 [Documentation](https://hupe1980.github.io/doubleentry/docs/) ·
🦀 [API reference](https://docs.rs/doubleentry) ·
📋 [Changelog](CHANGELOG.md)

## 🚀 Quick start

```rust
use doubleentry::prelude::*;

let mut books = Journal::new(LedgerId::new("example.com/acme")?);
books.declare_currency(EUR, 2, date!("2026-01-01"))?;
books.define_months(2026, 1..=12)?;
books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;

let rec = books.record(
    Entry::new("invoice-2026-0001", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("1190.00"), EUR)
        .credit("Income:Sales", amount!("1190.00"), EUR),
)?;
let inclusion = books.prove_entry(rec.id)?; // an RFC 6962 inclusion proof
assert_eq!((inclusion.index, inclusion.size), (rec.position, books.head().size));

// Seal March (and the months before it), then prove one closing balance.
let archived = books.head();
books.seal_through("2026-03")?;
let proof = books.prove_balance("Assets:Cash", EUR, Layer::Settled, "2026-03")?;
let proven = proof.verify(&books.head())?;
assert_eq!(proven.balance.net(), amount!("1190.00"));
books.prove_consistency(archived.size, books.head().size)?.verify(&archived, &books.head())?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The idempotency key (`"invoice-2026-0001"`) is the entry's identity: recording the same draft
again writes nothing and returns the original receipt with `replayed == true`.

`batch` records entries, clearings and resets in one transaction, all or none — a payment
and its assignment to open invoices commit together. `entry_id` gives the payment's id before
it is recorded, so a clearing in the same batch can name its posting:

```rust
# use doubleentry::prelude::*;
# let mut books = Journal::new(LedgerId::new("example.com/acme")?);
# books.declare_currency(EUR, 2, date!("2026-01-01"))?;
# books.define_months(2026, 1..=12)?;
# books.open("Assets:Receivables", AccountKind::Asset, date!("2026-01-01"))?;
# books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
# books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
# let invoice = books.record(Entry::new("INV-1", date!("2026-03-01")).kind("invoice")
#     .debit("Assets:Receivables", amount!("100.00"), EUR).credit("Income:Sales", amount!("100.00"), EUR))?;
let (paid, on) = (amount!("100.00"), date!("2026-03-10"));
let payment = PostingRef::at(books.entry_id("PAY-1")?, 1);
let receipts = books.batch([
    Entry::new("PAY-1", on)
        .kind("payment")
        .debit("Assets:Bank", paid, EUR)
        .credit("Assets:Receivables", paid, EUR)
        .into(),
    Clearing::new("PAY-1/INV-1", PostingRef::at(invoice.id, 0), payment, paid, on).into(),
])?; // a refused item is `Error::BatchRejected { index, .. }`, and nothing is written
assert_eq!(receipts.len(), 2);

books.seal_through("2026-03")?;
assert_eq!(books.watermark()?, Some(date!("2026-03-31"))); // nothing books on or before it
let year = BalanceQuery::between(date!("2026-01-01"), date!("2026-12-31"));
let invoiced = books.balance("Assets:Receivables", EUR, &year.of_kind("invoice"))?;
assert_eq!(invoiced.debit(), amount!("100.00"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Over SQLite (feature `sqlite`) the same calls are `async`:

```rust
# #[cfg(feature = "sqlite")]
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
use doubleentry::prelude::*;
use doubleentry::store::SqlStore;

let store = SqlStore::sqlite("sqlite::memory:").await?; // or "sqlite://books.db?mode=rwc"
let ledger = Ledger::new(store, LedgerId::new("example.com/acme")?).await?; // creates or opens
ledger.declare_currency(EUR, 2, date!("2026-01-01")).await?;
ledger.define_months(2026, 1..=12).await?;
ledger.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")).await?;
ledger.open("Income:Sales", AccountKind::Income, date!("2026-01-01")).await?;
ledger
    .record(
        Entry::new("invoice-2026-0001", date!("2026-03-15"))
            .debit("Assets:Cash", amount!("1190.00"), EUR)
            .credit("Income:Sales", amount!("1190.00"), EUR),
    )
    .await?;
ledger.audit().await?; // replays the log and compares every stored table
# Ok::<(), Box<dyn std::error::Error>>(())
# })
# }
# #[cfg(not(feature = "sqlite"))]
# fn main() {}
```

`SqlStore::postgres(url)` is the same for PostgreSQL. A ledger needs a schema (or SQLite file)
of its own; one holding other tables is refused with `ConfigError::ForeignSchema`. The
service can run as a least-privilege role: `grant_runtime(role)` applies the grants generated
from the schema (`runtime_grants(role)` lists them). More in
[Getting started](https://hupe1980.github.io/doubleentry/docs/start/getting-started/) and the
programs in [`examples/`](https://github.com/hupe1980/doubleentry/tree/main/examples).

## 🔒 Guarantees

| Guarantee | How |
|---|---|
| **Every entry balances** per (currency, layer) | The rule core refuses an unbalanced draft inside `record`, in the store's write transaction; there is no other way to write an entry. |
| **Exact money** | `Amount<P>` is an `i128` scaled by 10^P. Checked arithmetic returning `Result`, no floats, explicit `Rounding`; excess decimals are refused. |
| **Deterministic identity** | `EntryId` is SHA-256 over the ledger id and the idempotency key, so a retry replays instead of booking twice. The rule core reads no clock and no randomness. |
| **Tamper evidence** | Every record is a leaf of one RFC 6962 tree. Inclusion and consistency proofs verify against a `TreeHead`; records are re-hashed on read (`Error::Integrity`). |
| **Provable balances** | A seal commits, per (currency, layer), to a tree of account balances; `prove_balance` proves one, named, against a head alone. |
| **Witnessable** | `checkpoint` signs a C2SP `tlog-checkpoint`; `WitnessSession` speaks `tlog-witness` to stock witnesses; `Policy` and `Verifier` check cosignatures. Tested against the Go reference implementations. |
| **Full audit** | `audit()` replays every record through the rule core and compares every table the store keeps, row by row. |
| **Atomic batches** | `batch` writes entries, clearings and resets in one transaction, each decided against its predecessors; a refusal names the item and writes nothing; a retry replays every receipt. |
| **One behaviour** | `Journal`, `MemoryStore`, SQLite and PostgreSQL return the same `Rejection` for the same input, checked by a public conformance suite. |

It does not prevent a database superuser from editing tables — it detects it: on read, in
`audit()`, and, for a history rewritten from scratch, against a head someone else kept (an
auditor's archive or a witness cosignature). There is no FX conversion, no chart of
accounts, no HTTP client, no clock and no migrations. See
[Design](https://hupe1980.github.io/doubleentry/docs/assurance/design/).

## 📦 Features

All off by default.

| Feature | Enables |
|---|---|
| `serde` | `Serialize` / `Deserialize` on public types (transport only; hashing never uses serde) |
| `sqlite` | `SqlStore<Sqlite>` (SQLite bundled) |
| `postgres` | `SqlStore<Postgres>` (no TLS stack) |
| `postgres-tls` | `postgres` + TLS (rustls on aws-lc-rs, Mozilla roots) |
| `witness` | Ed25519 signing and verification for `transparency` |
| `testing` | The store conformance suite and, with `witness`, `TestWitness` |
| `tracing` | `tracing` spans; never an amount, memo or account path |

The driver (sqlx) is an implementation detail: it is not in the API and not in your manifest,
and its upgrades ship without an API change. TLS is `postgres-tls` plus `sslmode=verify-full`
in the URL.

## 🦀 MSRV

Rust **1.94**, declared as `rust-version` and checked in CI. It is never raised in a patch
release, and only to a stable release at least six months old.

## 📄 License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
