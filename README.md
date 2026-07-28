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
| **Verifiable** | Entries are leaves in an append-only Merkle log with `O(log n)` inclusion and consistency proofs; closed periods are sealed and chained |
| **Gross-preserving** | Balances carry debit *and* credit totals, so turnover survives netting |

---

## 🚀 Quick start

```rust
use doubleentry::account::AccountRegistry;
use doubleentry::entry::{LedgerPolicy, SealContext};
use doubleentry::period::{LedgerId, PeriodCalendar};
use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal};
use time::macros::date;

type Eur = Amount<2>;

// Accounts are paths in a hierarchy you define. Only leaves are postable.
let mut accounts = AccountRegistry::new();
let cash    = accounts.register_path("Assets:Cash",  date!(2026-01-01))?;
let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;

let calendar = PeriodCalendar::new();
let policy   = LedgerPolicy::default();
let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };

let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);

let recorded = journal.seal_and_record(
    Entry::new(
        EntryId::generate(),
        IdempotencyKey::new(b"invoice-2026-0001".to_vec())?,
        date!(2026-03-15),
    )
    .debit(cash,     Eur::parse("1190.00")?, Currency::EUR)
    .credit(revenue, Eur::parse("1190.00")?, Currency::EUR),
    &ctx,
)?;

// Prove the entry is committed to, without revealing any other entry.
let head  = journal.head();
let proof = journal.prove_inclusion(recorded.require_index()?)?;
assert!(proof.verify(&recorded.content_hash, &head.root));
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

## 🔒 Balanced by construction

`Entry` carries a type-state parameter. A draft proves nothing; sealing it runs every
invariant and yields `Entry<Balanced, P>` — a type with private fields, no public
constructor, and marker types behind a sealed trait.

```rust
# use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, ValidationError};
# use doubleentry::account::AccountRegistry;
# use doubleentry::entry::{LedgerPolicy, SealContext};
# use doubleentry::period::{LedgerId, PeriodCalendar};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let cash = accounts.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;
# let calendar = PeriodCalendar::new();
# let policy = LedgerPolicy::default();
# let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };
let errors = Entry::new(
    EntryId::generate(),
    IdempotencyKey::new(b"k".to_vec())?,
    date!(2026-03-15),
)
.debit(cash,     Eur::parse("100.00")?, Currency::EUR)
.credit(revenue, Eur::parse("99.00")?,  Currency::EUR)
.seal(&ctx)
.unwrap_err();

// Every violation is reported at once, not one round trip at a time.
assert!(errors.any(|e| matches!(e, ValidationError::Unbalanced { .. })));
# Ok::<(), Box<dyn std::error::Error>>(())
```

**What this does and does not claim.** Whether a set of postings balances is a property of
runtime values, so no type system short of dependent types decides it at compile time. What
the type state gives you is that *an unbalanced entry cannot be represented as a validated
one* — every API that persists, exports, or commits to an entry accepts only the balanced
form.

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

---

## ⚖️ Debits, credits, and gross totals

Direction is explicit rather than encoded in a sign, because a signed net destroys turnover:
an account showing zero could have seen no activity, or a million in each direction.

```rust
# use doubleentry::{Amount, Balance, Direction};
# type Eur = Amount<2>;
let mut b = Balance::<2>::ZERO;
b.add(Direction::Debit,  Eur::parse("1000.00")?)?;
b.add(Direction::Credit, Eur::parse("1000.00")?)?;

assert_eq!(b.signed_net()?, Eur::ZERO);        // nets out …
assert_eq!(b.debits,  Eur::parse("1000.00")?); // … but the turnover survives
assert_eq!(b.credits, Eur::parse("1000.00")?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

## 🔐 Proofs

The log follows the append-only Merkle tree of RFC 6962 / RFC 9162, with BLAKE3 in place of
SHA-256 and a domain separation tag on every node.

- **Inclusion proof** — this entry sits at this index under this root, in `O(log n)` hashes,
  revealing nothing else. A chained hash cannot do this.
- **Consistency proof** — the earlier log is a *prefix* of the later one. Not merely linked:
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
assert!(proof.verify(&snapshot.root, &log.root()));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Leaf hashes depend only on their own entry, so writers never contend on shared hash state —
a chained-hash log serialises every append; this one does not.

---

## 🔁 Idempotency

Recording is keyed by `IdempotencyKey`. The contract is explicit, because "won't duplicate"
is not a specification:

| Situation | Result |
|---|---|
| Key unseen | Appended |
| Key seen, content identical | **No-op**, returns the original outcome — a safe retry |
| Key seen, content differs | **Conflict**, refused — never a silent overwrite, never a second entry |

---

## ↩️ Corrections

Postings are immutable. A correction is a new entry that reverses an earlier one, and the
rules are enforced rather than documented:

- An entry may be reversed **at most once**.
- A reversal **may not itself be reversed** — reversal chains are how ledgers become
  unreadable.
- A reversal **must actually invert** the entry it names. Claiming a reversal while posting
  something else would mark the original corrected when the amounts never netted.
- A correction to a **sealed period** books into a later open period, carrying the original
  booking date as metadata. Sealed periods never change.

```rust
# use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal};
# use doubleentry::account::AccountRegistry;
# use doubleentry::entry::{LedgerPolicy, SealContext};
# use doubleentry::period::{LedgerId, PeriodCalendar};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let cash = accounts.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;
# let calendar = PeriodCalendar::new();
# let policy = LedgerPolicy::default();
# let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let original = Entry::new(EntryId::generate(), IdempotencyKey::new(b"o".to_vec())?, date!(2026-03-15))
#     .debit(cash, Eur::parse("50.00")?, Currency::EUR)
#     .credit(revenue, Eur::parse("50.00")?, Currency::EUR)
#     .seal(&ctx)?;
# journal.record(original.clone())?;
let reversal = original.reverse(
    EntryId::generate(),
    IdempotencyKey::new(b"reversal-of-o".to_vec())?,
    date!(2026-04-01),
);
journal.record(reversal.seal(&ctx)?)?;

// A second reversal of the same entry is refused.
assert!(journal.reversal_of(original.id()).is_some());
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

## 🧮 Open items

A receivable is raised by one posting and settled by another. Knowing *which* settled *which*
is what turns a running balance into a list of open items — and it has to agree with the
postings exactly, so it belongs in the ledger.

Nothing is mutated. Clearing records which postings offset which and by how much; a posting's
**residual** is its amount less everything applied to it.

```rust
# use doubleentry::{Amount, BalanceKey, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
# use doubleentry::account::AccountRegistry;
# use doubleentry::clearing::{ClearedItem, Clearing, ClearingId, PostingRef};
# use doubleentry::entry::{LedgerPolicy, SealContext};
# use doubleentry::period::{LedgerId, PeriodCalendar};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let receivable = accounts.register_path("Assets:Receivable", date!(2026-01-01))?;
# let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;
# let calendar = PeriodCalendar::new();
# let policy = LedgerPolicy::default();
# let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let invoice = Entry::new(EntryId::generate(), IdempotencyKey::new(b"inv".to_vec())?, date!(2026-03-01))
#     .debit(receivable, Eur::parse("1000.00")?, Currency::EUR)
#     .credit(revenue, Eur::parse("1000.00")?, Currency::EUR)
#     .seal(&ctx)?;
# let invoice_id = invoice.id();
# journal.record(invoice)?;
# let payment = Entry::new(EntryId::generate(), IdempotencyKey::new(b"pay".to_vec())?, date!(2026-03-20))
#     .credit(receivable, Eur::parse("400.00")?, Currency::EUR)
#     .debit(revenue, Eur::parse("400.00")?, Currency::EUR)
#     .seal(&ctx)?;
# let payment_id = payment.id();
# journal.record(payment)?;
// A payment of 400 against an invoice of 1000.
journal.clear(Clearing {
    id: ClearingId::generate(),
    account: receivable,
    currency: Currency::EUR,
    cleared_on: date!(2026-03-20),
    items: vec![
        ClearedItem { posting: PostingRef::new(invoice_id, 0), applied: Eur::parse("400.00")? },
        ClearedItem { posting: PostingRef::new(payment_id, 0), applied: Eur::parse("400.00")? },
    ],
})?;

let key = BalanceKey { account: receivable, currency: Currency::EUR, layer: Layer::Settled };
let open = journal.open_items(&key)?;

// The payment is fully applied; the invoice keeps its remainder open.
assert_eq!(open.len(), 1);
assert_eq!(open[0].residual, Eur::parse("600.00")?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Applying less than the full amount leaves both sides open for the remainder — what a partial
payment needs. Booking the shortfall as a fresh item instead is an ordinary entry your
application posts; the engine does not choose between the two policies.

A clearing that matched the wrong items can be **reset**, which releases the applied amounts.
The original record stays: an assignment made and withdrawn is itself part of the trail.

---

## 📋 Statements

A trial balance says where an account ended up and nothing about how it got there.

```rust
# use doubleentry::{Amount, BalanceKey, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
# use doubleentry::account::AccountRegistry;
# use doubleentry::entry::{LedgerPolicy, SealContext};
# use doubleentry::period::{LedgerId, PeriodCalendar};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let cash = accounts.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;
# let calendar = PeriodCalendar::new();
# let policy = LedgerPolicy::default();
# let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# for (i, amt) in [("100.00", b"a"), ("250.00", b"b")].iter().enumerate() {
#     let _ = i;
#     journal.seal_and_record(
#         Entry::new(EntryId::generate(), IdempotencyKey::new(amt.1.to_vec())?, date!(2026-03-15))
#             .debit(cash, Eur::parse(amt.0)?, Currency::EUR)
#             .credit(revenue, Eur::parse(amt.0)?, Currency::EUR),
#         &ctx,
#     )?;
# }
let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };
let lines = journal.statement(&key)?;

assert_eq!(lines.len(), 2);
assert_eq!(lines[1].running.debits, Eur::parse("350.00")?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

## 📆 Closing entries

Closing zeroes the accounts whose balances belong to one period only and moves the net to
equity. Which equity account receives it is a chart-of-accounts decision, so you name it.

```rust
# use doubleentry::account::{Account, AccountKind, AccountPath, AccountRegistry};
# use doubleentry::{Amount, BalanceKey, Currency, Direction, Layer, Posting, TrialBalance, closing_postings};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let mut reg = |path: &str, kind: AccountKind| accounts.register(
#     Account::new(AccountPath::parse(path).unwrap(), date!(2026-01-01)).with_kind(kind)).unwrap();
# let income = reg("Income:Sales", AccountKind::Income);
# let cost = reg("Expense:Rent", AccountKind::Expense);
# let equity = reg("Equity:Retained", AccountKind::Equity);
# let mut tb = TrialBalance::<2>::new();
# tb.apply(&Posting::credit(income, Eur::parse("1000.00")?, Currency::EUR))?;
# tb.apply(&Posting::debit(cost, Eur::parse("300.00")?, Currency::EUR))?;
let postings = closing_postings(
    &tb,
    &accounts,
    &[AccountKind::Income, AccountKind::Expense],
    equity,
    Layer::Settled,
)?;

// The postings balance by construction; applying them flattens both accounts
// and leaves the period's profit in equity.
# let mut after = tb;
# for p in &postings { after.apply(p)?; }
let key = BalanceKey { account: equity, currency: Currency::EUR, layer: Layer::Settled };
assert_eq!(after.get_or_zero(&key).net()?, (Direction::Credit, Eur::parse("700.00")?));
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

## 🧷 Period seals

Closing a period commits to **which entries** it contains and **what they add up
to** — both as Merkle roots. Seals chain, so removing or reordering a sealed
period breaks every seal after it.

```rust
# use doubleentry::{Amount, Currency, Entry, EntryId, IdempotencyKey, Journal};
# use doubleentry::account::AccountRegistry;
# use doubleentry::entry::{LedgerPolicy, SealContext};
# use doubleentry::period::{LedgerId, Period, PeriodCalendar, PeriodId, PeriodState};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let cash = accounts.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;
let mut calendar = PeriodCalendar::new();
let march = PeriodId::new("2026-03")?;
calendar.define(Period::new(march.clone(), date!(2026-03-01), date!(2026-03-31))?)?;

# let policy = LedgerPolicy::default();
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# {
#     let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };
#     journal.seal_and_record(
#         Entry::new(EntryId::generate(), IdempotencyKey::new(b"e1".to_vec())?, date!(2026-03-15))
#             .debit(cash, Eur::parse("10.00")?, Currency::EUR)
#             .credit(revenue, Eur::parse("10.00")?, Currency::EUR),
#         &ctx,
#     )?;
# }
// Stop postings first, so verification runs against a set that cannot grow.
calendar.transition(&march, PeriodState::Closing)?;
let seal = journal.seal_period(&march, &mut calendar)?;

assert!(seal.is_self_consistent());
assert!(journal.verify_seals().is_ok());

// A seal names the books it covers, inside its own hash.
assert_eq!(seal.ledger.as_str(), "acme-gmbh");
# Ok::<(), Box<dyn std::error::Error>>(())
```

A seal carries its `LedgerId` **inside the preimage**, not beside it. Two ledgers
can record the same amounts on the same accounts on the same day — the entry hash
covers what an entry says, not which book it landed in — so their tree heads and
trial balance roots can coincide exactly. Without the ledger in the hash their
seals would be byte-identical, and a seal handed to an auditor would attest to no
one in particular. With it, relabelling a seal invalidates it rather than
transferring it, and a `SealChain` rejects a seal from another ledger outright.

A sealed period is terminal — there is no reopening. A correction books into a
later open period carrying the original date, which is the only treatment
compatible with a log that has already been committed to.

---

## 📌 Checkpoints and assertions

Both are claims about balances that the journal can check, for opposite reasons.

A **checkpoint** is an optimisation. Balances are defined as a fold over the
journal; a checkpoint records the fold up to a point. Because that trades a
definition for a cache, it pins the log position *and* the tree head it was taken
against, so it can always be re-derived and can never be quietly reused against a
history that changed.

A **balance assertion** is the opposite: a claim from outside — a bank statement,
a counterparty confirmation — checked against the fold. It is the cheapest
mechanism there is for catching silent divergence.

```rust
# use doubleentry::{Amount, BalanceAssertion, BalanceKey, Currency, Entry, EntryId};
# use doubleentry::{IdempotencyKey, Journal, Layer};
# use doubleentry::account::AccountRegistry;
# use doubleentry::entry::{LedgerPolicy, SealContext};
# use doubleentry::period::{LedgerId, PeriodCalendar};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut accounts = AccountRegistry::new();
# let cash = accounts.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = accounts.register_path("Income:Sales", date!(2026-01-01))?;
# let calendar = PeriodCalendar::new();
# let policy = LedgerPolicy::default();
# let ctx = SealContext { accounts: &accounts, calendar: &calendar, policy: &policy };
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# journal.seal_and_record(
#     Entry::new(EntryId::generate(), IdempotencyKey::new(b"e1".to_vec())?, date!(2026-03-15))
#         .debit(cash, Eur::parse("350.00")?, Currency::EUR)
#         .credit(revenue, Eur::parse("350.00")?, Currency::EUR),
#     &ctx,
# )?;
let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };

let checkpoint = journal.checkpoint(&key)?;
assert!(journal.verify_checkpoint(&checkpoint).is_ok());

// What the statement says the balance is.
let outcome = journal.check_assertion(&BalanceAssertion::net(key, Eur::parse("350.00")?))?;
assert!(outcome.held());

// A mismatch reports the amount unaccounted for, not just a boolean.
let wrong = journal.check_assertion(&BalanceAssertion::net(key, Eur::parse("300.00")?))?;
assert_eq!(wrong.to_string(), "failed: expected 300.00, found 350.00, off by 50.00");
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

## 📎 Source documents

An entry can cite the document behind it, and bind that document's content hash so the link is
tamper-evident — a document swapped after the fact no longer matches the booking that cites it.

```rust
# use doubleentry::{DocumentRef, Hash};
let invoice = DocumentRef::new("INV-2026-0001", Hash::from_bytes([0x33; 32]))?;
assert!(invoice.is_verifiable());

// When the identifier is all you have — a reference off a message bus, a payment
// reference from a bank statement — say so rather than inventing a hash.
let cited = DocumentRef::unverified("INV-2026-0001")?;
assert!(!cited.is_verifiable());
# Ok::<(), Box<dyn std::error::Error>>(())
```

The hash is optional because the alternative is worse: making it mandatory pushes callers into
fabricating one, which produces a commitment that looks cryptographic and verifies nothing.
Absence states what is true — the entry names a document without vouching for its contents.
Presence is part of the canonical encoding, so an unhashed reference cannot later be passed off
as a hashed one.

---

## 🧭 Dimensions

Postings carry typed tags orthogonal to the account path, so a trial balance can group by any
axis without restructuring the tree. Encoding every axis into the path multiplies your account
count and freezes your reporting dimensions at design time.

```rust
# use doubleentry::{ActivityId, Dimensions, SegmentId};
let dims = Dimensions::none()
    .with_activity(ActivityId::new("Retail")?)
    .with_segment(SegmentId::new("Hardware")?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Values are opaque to the engine: it stores them, folds them into the entry hash so they are
covered by tamper evidence, and groups by them on request. It ships no vocabulary.

A ledger can require an activity on every posting, which is what you want where accounts must
be kept separately per line of business — an unattributed posting is then rejected rather than
landing outside every activity's accounts.

---

## 💾 Persistence

The engine keeps no storage of its own. `LedgerStore` says what a backend must do,
`MemoryStore` is one, and — more usefully — the **conformance suite** decides whether any
other one is correct.

```rust
use doubleentry::LedgerId;
use doubleentry::storage::{MemoryStore, conformance};

let store = MemoryStore::<2>::new(LedgerId::new("my-ledger")?);
let report = conformance::block_on(conformance::check_all(&store));
report.assert_passed();
# Ok::<(), Box<dyn std::error::Error>>(())
```

A ledger is only as trustworthy as its weakest backend, and the ways a backend can be subtly
wrong — a read-then-write idempotency check that races, a batch that half-lands, an index
sequence with a gap — produce no error and no symptom until an audit. The suite makes the
contract executable: a backend either passes it or is not a backend.

| Guarantee | Checked by |
|---|---|
| Append-only; recorded entries never change | `check_reads_are_stable` |
| Batches are atomic — all or nothing | `check_batch_is_atomic` |
| Identical replay is a no-op | `check_idempotent_replay` |
| Same key, different content is refused | `check_idempotency_conflict` |
| Indices are dense, ordered, gap-free | `check_append_assigns_dense_indices` |
| Balances agree with a fold over the log | `check_balances_match_the_log` |
| Every record is provable; growth is append-only | `check_proofs_verify` |
| Reversal: at most once, never of a reversal, must actually invert | `check_reversal_rules` |
| Clearing: over-application, imbalance, duplicates, double reset | `check_clearing_rules` |
| Residuals reflect exactly what was applied | `check_open_items_track_residuals` |

This is not decoration. Extending the suite from nine checks to twelve immediately failed the
PostgreSQL backend on two counts — a reversal could be reversed, and a clearing could be reset
twice — because the schema's constraints cannot express relational rules like "the target is
not itself a reversal" or "this `UPDATE` matched nothing." Both are fixed; the point is that a
backend which looked correct was not, and only an executable contract said so.

Reads are **cursor-paged rather than streamed**: a cursor maps onto
`WHERE index > ? ORDER BY index LIMIT ?` in any SQL backend, survives a dropped connection,
and needs no async-iteration machinery — so this crate stays free of a futures dependency and
your backend stays free of an executor choice.

`LedgerStore` uses `async fn` in trait: static dispatch, no per-call allocation, not
`dyn`-compatible. When the backend comes from configuration, `DynLedgerStore` boxes the
futures and restores object safety. A blanket impl covers every store, so you get both without
writing anything.

### One ledger per database

A `LedgerStore` handle serves exactly one **ledger**, named by a `LedgerId` and bound at
construction. A ledger owns its log, its dense index space, its Merkle tree, its accounts and
its seal chain — nothing crosses between two of them.

```rust,ignore
let store = PostgresStore::<2>::new(pool, LedgerId::new("acme-gmbh")?);
store.migrate().await?;   // claims the database for this ledger, or refuses it
```

That separation is **physical**, not a filter column, and the reason is the seal. A seal commits
to one entity's history; sharing a log between tenants would have each tenant's seal committing
to the others' entries, and an inclusion proof shown to one auditor would reveal how many
entries the others hold. A `WHERE ledger_id = ?` predicate cannot fix that — and it fails open:
one query missing its predicate is a silent cross-tenant read. With one database per ledger
there is no predicate to forget.

`migrate` records the ledger in the database and refuses to open one that belongs to a
different ledger, so two stores cannot quietly merge two logs into one.

### Sharing a database with your application

One ledger per database does not mean a database with nothing else in it. On PostgreSQL the
ledger's tables live in their own schema — `doubleentry` — leaving `public` to you:

```rust,ignore
let store = PostgresStore::<2>::connect(&url, LedgerId::new("acme-gmbh")?).await?;
store.migrate().await?;
```

`connect` sets `search_path` so unqualified names resolve to the ledger's schema. This matters
because `accounts` is a name many applications have already spent on something else, and a
ledger that squats on it in `public` cannot be adopted without a rename. Your `public.accounts`
and the ledger's `doubleentry.accounts` coexist, in one database, in one transaction if you
want them there.

The schema is a **default, not a policy** — pass your own, including `public` when the database
belongs to the ledger alone:

```rust,ignore
let store = PostgresStore::<2>::connect_with(&url, ledger, "public").await?;
```

Whatever the choice, isolation is only real if it is in effect, so `migrate` **verifies** it
rather than assuming it: if unqualified names would resolve somewhere else it returns
`WrongSearchPath` instead of quietly creating a second set of tables. If you build the pool
yourself, set `options=-c search_path=<schema>` on it and declare it with `.in_schema(...)`.

SQLite needs none of this — one ledger per file already is the isolation.

### Account handles outlive the process

An `AccountId` is a **position in registration order**, and that position is written into every
posting row and into the trial balance leaves a seal commits to. It is persistent ledger state,
not a runtime detail. Re-registering paths on start-up would reissue positions in whatever order
your code happened to use and silently repoint history.

So load the bindings; do not rebuild them:

```rust,ignore
// Start-up: restore each account at the handle it was issued.
let registry = AccountRegistry::from_records(store.accounts().await?)?;

// Later, when a new account appears:
let id = registry.register_path("Assets:Receivables:Customer:100241", today)?;
for record in registry.records() {
    store.register_account(&record).await?;   // no-op for handles already stored
}
```

`from_records` refuses bindings that are not a dense `0..n` — a gap or a duplicate means the
stored set is not the one the handles were issued against. `AccountRegistry::commitment()` is a
Merkle root over every handle-to-account binding, so a registry you built locally can be checked
against the stored one rather than trusted:

```rust,ignore
assert_eq!(local.commitment(), AccountRegistry::from_records(store.accounts().await?)?.commitment());
```

The conformance suite tests this round trip, including that `kind` and `closed_on` survive it —
validation reads both, so a backend that drops them changes what may be posted.

### Backends

| Backend | Feature | Verified against |
|---|---|---|
| In-memory | always on | the conformance suite |
| SQLite | `sqlite` | a real database, in-process — no server, no container |
| PostgreSQL | `postgres` | a real database, via testcontainers |

All of them run the same conformance suite — PostgreSQL runs it twice, once per sequencing mode —
and a test asserts they **agree**: same log indices,
same content hashes, same tree root, same trial balance for the same operations. An abstraction
that only one implementation satisfies is not an abstraction.

Writing the second backend paid for itself immediately — it exposed a bug in the first. The
PostgreSQL store had been ordering the seal chain by `sealed_at, period_id`, which is neither
chain order nor reliable: two seals in the same clock tick order arbitrarily, and the fallback
is lexical. SQLite has no sub-second timestamp ordering at all, which forced the question and
produced the right answer for both — an explicit `chain_position`.

Choosing between them: SQLite suits embedded and single-process deployments and needs no
server. PostgreSQL additionally enforces the balance invariant *in the database* via a deferred
constraint trigger, enforces period non-overlap with an `EXCLUDE` constraint, and supports
revoking `UPDATE` and `DELETE` — so where the ledger must be defended against processes other
than this one, it is the stronger choice. Both schemas say which guarantees they can and
cannot carry.

### PostgreSQL

```rust,ignore
use doubleentry::storage::postgres::PostgresStore;
use doubleentry::storage::{EntryBatch, LedgerStore};

let store = PostgresStore::<2>::new(pool, ledger.clone());
store.migrate().await?;              // idempotent; applies schema/postgres.sql
store.append(&EntryBatch::single(entry)).await?;
```

The SQLite store is the same shape:

```rust,ignore
use doubleentry::storage::sqlite::SqliteStore;

let store = SqliteStore::<2>::new(pool, ledger.clone());
store.migrate().await?;              // idempotent; applies schema/sqlite.sql
store.append(&EntryBatch::single(entry)).await?;
```

Two details in [`schema/postgres.sql`](schema/postgres.sql) are worth knowing about, because
both are easy to get wrong and neither fails loudly:

**Head queries are `O(1)`.** Each entry records the Merkle root it produced, and the
perfect-subtree accumulator is persisted in a small `O(log n)` table updated inside the append.
A head query is one row read rather than a rebuild from every content hash ever stored — and
historical heads are equally cheap, which is what lets a checkpoint pin itself to one history.
The accumulator is derived state, so a test asserts it matches a full rebuild at every size.

**Sequencing is a choice.** `Sequencing::Inline` (the default) assigns a log position during the
append, under an advisory lock: an entry is provable the moment it is durable, and appends to one
ledger serialise. `Sequencing::Deferred` lets writers insert concurrently and assigns positions
afterwards via `store.sequence()` — no contention, at the cost of a window in which an entry is
recorded but not yet placed. That window is why `Recorded::index` is an `Option`: the difference
between *recorded* and *committed to* is real, and hiding it would be dishonest.

The sequencer advances on a **commit-order watermark**
(`insert_xid < pg_snapshot_xmin(pg_current_snapshot())`), never a high-water mark — it only picks
up rows whose inserting transaction has definitely finished. A row still in flight is left for the
next pass rather than skipped, because a skipped row would appear *behind* the reader once it
committed and would never be collected.

One consequence worth knowing before choosing this mode: **that watermark is cluster-wide.** A
transaction left open anywhere in the instance, including in another database, holds it back, and
everything recorded after that transaction began waits for it to end. Safe, never lossy — but
sequencing latency is bounded by the longest open transaction in the cluster.

**`log_index` is deliberately not a `SEQUENCE`.** `nextval` is consumed *before* commit, so a
transaction holding index 5 can commit after one holding 6 — and a reader tracking a
high-water mark then steps over 5 permanently. The index is assigned inside the append, under
an advisory lock held for the transaction. That serialises appends; it is the right trade for
correctness, and the schema's operational notes describe the shape to move to under load.

**Reads verify integrity.** Rows are rehydrated through `Entry::adopt_verified`, which
recomputes the content hash and compares it with the one stored alongside. Re-running
validation would be *wrong* here — validation is against the ledger's current accounts and
periods, so a historical entry would start failing the day its account closed. The hash proves
the bytes are exactly what passed validation originally, which re-validation does not: that
would accept a *different* entry that also happens to balance. A row altered underneath the
engine surfaces as an error on the next read rather than as a wrong number in a report.

The database also enforces the balance invariant itself, via a deferred constraint trigger.
The engine cannot produce an unbalanced entry — but application-level and database-level
enforcement fail independently, and this is the one worth paying for twice.

---

## 🧊 Cold tier

A sealed period is finished: its entries will not change, and keeping them in the operational
database forever costs money and query time for data nobody books against.
`--features iceberg` moves them into Apache Iceberg — columnar, on object storage, queryable
from DataFusion, DuckDB, Trino or Spark — without weakening what the seal promises.

```rust,ignore
use doubleentry::storage::iceberg::{ColdTier, iceberg_schema};

// Create the table once, with the schema the cold tier writes.
let creation = TableCreation::builder()
    .name("journal".to_owned())
    .schema(iceberg_schema()?)
    .build();
catalog.create_table(&namespace, creation).await?;

// Then archive each period as it seals.
let cold = ColdTier::new(table_ident);
let result = cold.compact(&store, &seal, &catalog).await?;
assert_eq!(result.verified_root, seal.tree_head.root);
```

Each compaction commits one Iceberg snapshot whose summary carries the seal hash, tree root,
trial-balance root, period, and the log position archived through. An auditor handed a table
name and a seal hash can verify the archive with off-the-shelf tooling and no access to this
crate — which is why the commitment goes in the table metadata rather than a sidecar only we can
read. Every write is an `append` operation, so a snapshot recording anything else is itself
evidence.

Two failure modes are refused rather than papered over: a seal committing to a shorter log than
the archive already holds (`SealBehindArchive`), and a table whose latest snapshot carries no
archive state (`MalformedState`) — it holds data this crate did not write, and treating it as
empty would duplicate all of it.

**Copy, verify, then tombstone.** Moving data out of hot storage *is* a deletion from hot
storage, which sits in tension with an append-only ledger. The tension is made explicit rather
than papered over: the missing entries are collected, pushed onto the Merkle accumulator the
last compaction left behind, and the resulting root compared against the seal. A mismatch aborts
and writes nothing — so the operational rows stay exactly where they are. The precise claim is
that *content* is immutable and *location* is not.

**Incremental, and verified as such.** Successive compactions do not overlap: each writes only
what the archive is missing. Because each snapshot carries the accumulator for the prefix it
archived, the next one verifies the **whole archive** against the seal while reading only the
delta — an accumulator is a commitment to everything behind it.

**The seal travels with the data.** Each compaction commits one Iceberg snapshot whose summary
carries the seal hash, the tree root, the trial-balance root, and the period. An auditor handed
a table name and a seal hash can verify the archive with off-the-shelf tooling and no access to
this crate — which is why the commitment goes in the table metadata rather than a sidecar only
we can read. Iceberg's guarantees compose with the ledger's: snapshots are immutable, the
snapshot log is append-only, and every write is an `append` operation, so a snapshot recording
anything else is itself evidence.

Rows are one per **posting**, with the owning entry's fields repeated and the `period` that
archived them — a natural partition key. Two column types differ from the engine's, because
Iceberg has neither unsigned nor 16-bit integers: `log_index` is `Int64` and `posting_index` is
`Int32`.

---

## 🧱 Design boundaries

**Not** an ERP, an ORM, a reporting engine, a chart of accounts, a payments library, a policy
engine, or a distributed system. It produces validated, balanced, provable entries and leaves
every domain decision to you.

Two structural rules *are* enforced, because no downstream layer can repair them:

- **Only leaves are postable.** Posting to both a node and its descendants makes every rollup
  double-count.
- **Postings fall inside the account's open window.**

---

## 🧪 Testing

Invariants are covered by property tests over generated inputs, not hand-picked cases:
splitting conserves money for any total and weights; inclusion and consistency proofs verify
for every log size and every prefix; tampering with any committed leaf changes the root;
seals detect any edit to their own fields; checkpoints always re-derive; clearing never
over-applies and never moves money; closing always balances and always flattens; canonical
encoding is a pure function of the value; and a journal of balanced entries always folds to a
balanced trial balance.

Beyond that:

- **Simulation.** Long, randomly generated sequences of appends, replays, reversals, clearings,
  resets and seals, with *every* invariant re-checked after each step. Sequences are generated
  from a seed, so a failure reproduces exactly — which is what the deterministic kernel is for.
- **Golden vectors.** Committed hashes for the canonical encoding and the Merkle log. Nothing in
  the compiler stops a field being reordered or a domain tag being edited, and every such change
  silently invalidates every hash ever written while leaving the suite green.
- **Robustness.** No input, however malformed, may panic a parser or a deserialiser. A panic on
  hostile input is a denial of service in something meant to be a system of record.
- **Conformance.** The storage contract, executable — run against both the in-memory and the
  PostgreSQL backend.
- **Real databases.** SQLite runs in-process; the PostgreSQL tests start a throwaway container.
  Nothing is mocked: the constraints, the locking, the deferred triggers, and the behaviour
  under concurrent appends are the real ones, because those are exactly the parts that cannot
  be checked any other way.

```console
cargo test                                   # everything but the databases
cargo test --features sqlite                 # adds SQLite; no server needed
cargo test --features postgres               # adds PostgreSQL; needs Docker
cargo test --features iceberg                # adds the cold tier; writes to a temp dir
cargo clippy --all-targets --all-features
```

The crate denies `unsafe_code`, and denies `arithmetic_side_effects`, `indexing_slicing`,
`unwrap_used`, `expect_used`, and `panic` in library code.

---

## 📦 Features

| Feature | Effect |
|---|---|
| `serde` | `Serialize` / `Deserialize` on public types. Transport only — the canonical encoding used for hashing is independent of it |
| `postgres` | A PostgreSQL-backed `LedgerStore` on `sqlx`, plus the reference schema |
| `sqlite` | A SQLite-backed `LedgerStore` on `sqlx`, plus the reference schema |
| `iceberg` | An Apache Iceberg cold tier for sealed periods |

### Serialisation rules

Deriving `Deserialize` on a newtype whose constructor validates its input
silently defeats that validation, and does so precisely for values that came from
outside the program. Every validated type here therefore round-trips through its
own constructor:

```rust
# use doubleentry::{ActivityId, Amount, Currency};
# type Eur = Amount<2>;
# #[cfg(feature = "serde")] {
// Values no constructor would accept do not survive a round trip.
assert!(serde_json::from_str::<Currency>("\"eur\"").is_err());
assert!(serde_json::from_str::<ActivityId>("\"\"").is_err());

// Money is a decimal string, never a raw scaled integer — the integer is
// meaningless without knowing the scale.
assert_eq!(serde_json::to_string(&Eur::parse("12.34")?)?, "\"12.34\"");
assert!(serde_json::from_str::<Eur>("1234").is_err());
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

Deserialising an entry yields a **`Draft`**, never a `Balanced` entry. A witness
that can be read off a wire is not a witness: a peer could assert it for postings
that do not balance, against accounts that do not exist, in a period that is
closed. The receiver re-runs `seal` against its own accounts, calendar, and
policy — and a valid entry re-seals to the same content hash.

---

## 📄 License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
