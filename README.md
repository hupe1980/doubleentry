# 📒 doubleentry

[![Crates.io](https://img.shields.io/crates/v/doubleentry.svg)](https://crates.io/crates/doubleentry)
[![Docs.rs](https://img.shields.io/docsrs/doubleentry)](https://docs.rs/doubleentry)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#-license)
[![MSRV](https://img.shields.io/badge/rustc-1.94+-orange.svg)](https://www.rust-lang.org/)

> **An immutable, tamper-evident double-entry bookkeeping engine.**
> Balanced by construction. Exact integer money. Zero I/O. No async. No chart of accounts.

`doubleentry` is a calculation *library*, not a platform. It enforces the invariants of
double-entry bookkeeping inside the engine — and then lets you **prove to a third party**
that it did.

📖 **[Documentation](https://hupe1980.github.io/doubleentry)** ·
🦀 **[API reference](https://docs.rs/doubleentry)** ·
📋 **[Changelog](CHANGELOG.md)**

---

## ✨ Why

Most ledger libraries reduce to CRUD rows and leave the invariants to the application. The
ones that don't usually still can't answer the question an auditor actually asks: *how do I
know this record wasn't changed after the fact?*

| Guarantee | How |
|---|---|
| **Balanced by construction** | An entry reaches a persistable state only through validation, and the validated type has no other constructor |
| **Exact** | Money is a scaled `i64` with compile-time precision. Every fallible operation returns `Result` — no floats, no panics, no wrapping |
| **Deterministic** | No clock, no RNG, no hash-map iteration order. Identical inputs produce identical bytes |
| **Verifiable** | Entries are leaves in an append-only Merkle log with `O(log n)` inclusion and consistency proofs; closed periods are sealed and chained, and a sealed balance can be proven *and named* without disclosing the rest of the books |
| **Gross-preserving** | Balances carry debit *and* credit totals, so turnover survives netting |
| **Bounded** | An account can be forbidden from crossing zero, checked inside the write against the balance the entry would leave — so concurrent draws cannot together overdraw it |

---

## 🚀 Quick start

```rust
use doubleentry::period::LedgerId;
use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal};
use time::macros::date;

type Eur = Amount<2>;

// A journal is one entity's books: its accounts, its calendar, its policy, its
// entries, and the Merkle log that commits to them.
let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);

// Accounts are paths in a hierarchy you define. Only leaves are postable.
let cash    = journal.accounts_mut().register_path("Assets:Cash",  date!(2026-01-01))?;
let revenue = journal.accounts_mut().register_path("Income:Sales", date!(2026-01-01))?;

let recorded = journal.record(
    Entry::new(
        EntryId::generate(),
        IdempotencyKey::new(b"invoice-2026-0001".to_vec())?,
        date!(2026-03-15),
    )
    .debit(cash,     Eur::parse("1190.00")?, Currency::EUR)
    .credit(revenue, Eur::parse("1190.00")?, Currency::EUR),
)?;

// Prove the entry is committed to, without revealing any other entry.
// Verification takes the whole head — size and root — so the position the
// proof names is checked rather than taken on the prover's word.
let head  = journal.head();
let proof = journal.prove_inclusion(recorded.require_index()?)?;
assert!(proof.verify(&recorded.content_hash, &head));
# Ok::<(), Box<dyn std::error::Error>>(())
```

`record` validates the draft against *this* journal's accounts, calendar and policy, then
appends it. There is no separate context to build and no second object to keep in step — the
things validation consults and the thing that stores the result are the same thing.

→ [Getting started](https://hupe1980.github.io/doubleentry/docs/getting-started/)

---

## 🔒 Balanced by construction

`Entry` carries a type-state parameter. A draft proves nothing; sealing it runs every
invariant and yields `Entry<Balanced, P>` — a type with private fields, no public
constructor, and marker types behind a sealed trait.

```rust
# use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal, ValidationError};
# use doubleentry::period::LedgerId;
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let cash = journal.accounts_mut().register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = journal.accounts_mut().register_path("Income:Sales", date!(2026-01-01))?;
let errors = Entry::new(
    EntryId::generate(),
    IdempotencyKey::new(b"k".to_vec())?,
    date!(2026-03-15),
)
.debit(cash,     Eur::parse("100.00")?, Currency::EUR)
.credit(revenue, Eur::parse("99.00")?,  Currency::EUR)
.seal(&journal.context())
.unwrap_err();

// Every violation is reported at once, not one round trip at a time.
assert!(errors.any(|e| matches!(e, ValidationError::Unbalanced { .. })));
# Ok::<(), Box<dyn std::error::Error>>(())
```

**What this claims.** Whether a set of postings balances is a property of runtime values, so
no type system short of dependent types decides it at compile time. What the type state gives
you is that *an unbalanced entry cannot be represented as a validated one* — every API that
persists, exports, or commits to an entry accepts only the balanced form.

→ [Entries and validation](https://hupe1980.github.io/doubleentry/docs/entries/)

---

## 🧾 Money

`Amount<P>` is a scaled `i64` with the precision fixed at compile time. One value has exactly
one representation, which is what makes hashing a monetary amount meaningful.

```rust
# use doubleentry::{Amount, MoneyError};
type Eur = Amount<2>;

// Splitting is exact: the parts always re-sum to the whole.
let parts = Eur::parse("100.00")?.distribute(3)?;
assert_eq!(parts.len(), 3);
assert_eq!(Eur::checked_sum(parts.iter().copied())?, Eur::parse("100.00")?);

// Proportional splits use largest-remainder, with ties broken deterministically.
let split = Eur::parse("10.00")?.allocate(&[1, 4])?;
assert_eq!(split, vec![Eur::parse("2.00")?, Eur::parse("8.00")?]);

// Excess precision is refused rather than silently rounded.
assert_eq!(Eur::parse("1.234"), Err(MoneyError::PrecisionLoss { scale: 2 }));

// Arithmetic is total: overflow is a value, not a panic.
assert_eq!(Eur::MAX.checked_add(Eur::from_minor(1)), Err(MoneyError::Overflow));
# Ok::<(), Box<dyn std::error::Error>>(())
```

If your application does proportional division itself, the ledger eventually goes off by a
cent and nobody can say which entry did it. That is why splitting lives here.

→ [Money](https://hupe1980.github.io/doubleentry/docs/money/) ·
[Debits, credits and gross totals](https://hupe1980.github.io/doubleentry/docs/debits-and-credits/)

---

## 🚧 Balance limits

Accounts are unconstrained by default. Where the books would be *wrong* rather than merely
surprising if a balance crossed zero, say so and the engine enforces it:

```rust
# use doubleentry::account::BalanceLimit;
# use doubleentry::period::LedgerId;
# use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal, JournalError};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let wallet = journal.accounts_mut().register_path("Liabilities:Wallet", date!(2026-01-01))?;
# let cash = journal.accounts_mut().register_path("Assets:Cash", date!(2026-01-01))?;
// A customer wallet may not be drawn beyond what was funded.
journal.accounts_mut().set_limit(wallet, BalanceLimit::NoDebitBalance)?;

let overdraw = Entry::new(
    EntryId::generate(),
    IdempotencyKey::new(b"withdrawal-1".to_vec())?,
    date!(2026-03-15),
)
.debit(wallet, Eur::parse("50.00")?, Currency::EUR)
.credit(cash, Eur::parse("50.00")?, Currency::EUR);

assert!(matches!(
    journal.record(overdraw),
    Err(JournalError::LimitBreached { .. })
));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Checked against the balance the **whole entry** would leave behind, per currency and per
layer, so the answer never depends on the order the postings were listed in. Both SQL
backends enforce it *inside the append transaction* — a limit checked before the write reads
a pre-image that two concurrent appends both see, each fitting it and together breaching it.

→ [Accounts](https://hupe1980.github.io/doubleentry/docs/accounts/)

---

## 🔐 Proofs

The log follows the append-only Merkle tree of RFC 6962 / RFC 9162, with BLAKE3 in place of
SHA-256 and a domain separation tag on every node.

- **Inclusion** — this entry sits at this index under this root, in `O(log n)` hashes,
  revealing nothing else. A chained hash cannot do this.
- **Consistency** — the earlier log is a *prefix* of the later one. Not merely linked:
  provably append-only.

```rust
# use doubleentry::merkle::MerkleLog;
# use doubleentry::Hash;
# fn leaf(i: u64) -> Hash { let mut b = [0u8; 32]; b[..8].copy_from_slice(&i.to_le_bytes()); Hash::from_bytes(b) }
let mut log = MerkleLog::new();
for i in 0..1024 { log.append(leaf(i)); }

let snapshot = log.head();
for i in 1024..2048 { log.append(leaf(i)); }

// The published snapshot is provably a prefix of what the log holds now.
let proof = log.consistency_proof(snapshot.size)?;
assert!(proof.verify(&snapshot, &log.head()));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Leaf hashes depend only on their own entry, so writers never contend on shared hash state —
a chained-hash log serialises every append; this one does not.

→ [Proofs](https://hupe1980.github.io/doubleentry/docs/proofs/)

---

## 🧷 Period seals

Closing a period commits to **which entries** it contains, **what they add up to**, and
**which accounts those totals are for** — all three as Merkle roots. Seals chain, so removing
or reordering a sealed period breaks every seal after it.

A seal carries its `LedgerId` inside the preimage, so it attests to one entity's books or it
does not verify at all. A sealed period is terminal: a correction books into a later open
period carrying the original date, which is the only treatment compatible with a log that has
already been committed to.

The closing balance is *cumulative* through the period's last day, so sealing also moves a
**watermark** that shuts every date below it — including one no period covers — and periods
must be sealed in date order. Otherwise an ordinary booking into an earlier open period, or
into a gap the calendar never defined, would restate a sealed balance while every seal, proof
and chain went on verifying.

```rust
# use doubleentry::{Amount, BalanceKey, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
# use doubleentry::period::{LedgerId, Period, PeriodId, PeriodState};
# use doubleentry::seal::TrialBalanceCommitment;
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let cash = journal.accounts_mut().register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = journal.accounts_mut().register_path("Income:Sales", date!(2026-01-01))?;
let march = PeriodId::new("2026-03")?;
journal.define_period(Period::new(march.clone(), date!(2026-03-01), date!(2026-03-31))?)?;
# journal.record(
#     Entry::new(EntryId::generate(), IdempotencyKey::new(b"e1".to_vec())?, date!(2026-03-15))
#         .debit(cash, Eur::parse("1190.00")?, Currency::EUR)
#         .credit(revenue, Eur::parse("1190.00")?, Currency::EUR))?;

// Stop postings first, so verification runs against a set that cannot grow.
journal.transition_period(&march, PeriodState::Closing)?;
let seal = journal.seal_period(&march)?;

// The books are now shut through March — February included, though no period
// ever covered it. Nothing below the watermark can restate what was sealed.
assert_eq!(journal.calendar().sealed_through(), Some(date!(2026-03-31)));
assert!(!journal.calendar().accepts(date!(2026-02-10)));

// An auditor holding only the seal can be shown one closing balance — and be
// told which account it is — without seeing any other account or entry.
# let closing = journal.trial_balance_through_date(date!(2026-03-31))?;
# let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };
let balance = TrialBalanceCommitment::of(&closing).prove(&key).expect("cash was posted to");
// `_at`, because the registry has moved on since the seal — new accounts, a
// closure, a tightened limit — and the seal names the commitment it had then.
let binding = journal
    .accounts()
    .prove_binding_at(cash, seal.accounts.size)
    .expect("issued by then");

assert!(balance.verify_naming(&binding, &seal));
assert_eq!(binding.path().to_string(), "Assets:Cash");
# Ok::<(), Box<dyn std::error::Error>>(())
```

The trial balance is keyed on account **handles** — dense integers, cheap to compare.
`Seal::accounts` is what says which account each handle is, so renumbering the registry after
the fact cannot leave a seal verifying while its balances quietly mean something else. Both are
stored as Merkle **heads**, size and root together, because a proof is checked against both.

The binding leaf covers the handle and the path — the account's *identity*, and the only part
of it that never changes. Master data (`kind`, the open window, the balance limit) is out, so
closing an account does not retroactively invalidate every proof against every earlier seal.

Behind a `LedgerStore` this is one call, which is worth preferring: assembled by hand the
recipe has five steps and only one of them matters — checking your rebuilt commitment against
the one the seal recorded — and it is the step nothing forces.

```rust,ignore
let proven = store.prove_sealed_balance(&march, cash_key).await?.expect("has a row");
assert!(proven.verify());
assert_eq!(proven.path().to_string(), "Assets:Cash");
```

→ [Periods and seals](https://hupe1980.github.io/doubleentry/docs/periods-and-seals/)

---

## 💾 Persistence

The engine keeps no storage of its own. `LedgerStore` defines what a backend must do, and the
**conformance suite** — twenty executable checks — is what decides whether an
implementation of it is correct.

| Backend | Feature | Verified against |
|---|---|---|
| In-memory | always on | the conformance suite |
| SQLite | `sqlite` | a real database, in-process — no server, no container |
| PostgreSQL | `postgres` | a real database, via testcontainers |

All of them run the same suite — PostgreSQL runs it twice, once per sequencing mode — and a
test asserts they **agree**: same log indices, same content hashes, same tree root, same trial
balance for the same operations. An abstraction that only one implementation satisfies is not
an abstraction.

```rust,ignore
let store = PostgresStore::<2>::connect(&url, LedgerId::new("acme-gmbh")?).await?;
store.migrate().await?;
store.append(&EntryBatch::single(entry)).await?;
```

→ [Persistence](https://hupe1980.github.io/doubleentry/docs/persistence/) ·
[Cold tier](https://hupe1980.github.io/doubleentry/docs/cold-tier/)

---

## 📦 Features

| Feature | Effect |
|---|---|
| `serde` | `Serialize` / `Deserialize` on public types. Transport only — the canonical encoding used for hashing is independent of it |
| `sqlite` | A SQLite-backed `LedgerStore` on `sqlx`, plus the reference schema |
| `postgres` | A PostgreSQL-backed `LedgerStore` on `sqlx`, plus the reference schema |
| `iceberg` | An Apache Iceberg cold tier for sealed periods |

Every validated type round-trips through its **own constructor**, so an invariant that holds
for a constructed value also holds for one read back. Deserialising an entry yields a
`Draft`, never a `Balanced` entry — a witness that can be read off a wire is not a witness.

→ [Features and serialisation](https://hupe1980.github.io/doubleentry/docs/features/)

---

## 🧱 Design boundaries

**Not** an ERP, an ORM, a reporting engine, a chart of accounts, a payments library, a policy
engine, or a distributed system. It does not convert currencies, name your reporting axes, or
read a clock. It produces validated, balanced, provable entries and leaves every domain
decision to you.

Two structural rules *are* enforced, because no downstream layer can repair them:

- **Only leaves are postable.** Posting to both a node and its descendants makes every rollup
  double-count.
- **Postings fall inside the account's open window.**

And one you opt into per account, for the same reason — an overdrawn cash account is not
something a report can repair either: a **balance limit**, checked against the balance an
entry would leave behind.

---

## 🧪 Testing

Invariants are covered by property tests over generated inputs, not hand-picked cases. Beyond
that: randomised **simulation** with every invariant re-checked after each step; committed
**golden vectors** for the canonical encoding, the seal preimage and the Merkle log;
**robustness** tests asserting no input can panic a parser; a **cost** guard on the shape of
the curve; the **conformance** suite; and **real databases** — SQLite in-process, PostgreSQL
in a throwaway container, nothing mocked.

```console
cargo test                                   # everything but the databases
cargo test --features sqlite                 # adds SQLite; no server needed
cargo test --features postgres               # adds PostgreSQL; needs Docker
cargo test --features iceberg                # adds the cold tier; writes to a temp dir
cargo clippy --all-targets --all-features
```

The crate forbids `unsafe_code`, and denies `arithmetic_side_effects`, `indexing_slicing`,
`unwrap_used`, `expect_used`, and `panic` in library code. A CI job greps the engine for
clocks, I/O, async and unsafe, so determinism is checked rather than trusted.

→ [Design boundaries and testing](https://hupe1980.github.io/doubleentry/docs/design/)

---

## 📄 License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
