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
| **Balanced by construction** | An entry reaches a persistable state only through validation, and the validated type has no other constructor. Debits equal credits per currency **and per layer**, so a reservation can never be offset by a settled movement |
| **Exact** | Money is a scaled `i64` with compile-time precision. Every fallible operation returns `Result` — no floats, no panics, no wrapping |
| **Deterministic** | No clock, no RNG, no hash-map iteration order. Identical inputs produce identical bytes |
| **Verifiable** | Entries are leaves in an append-only Merkle log with `O(log n)` inclusion and consistency proofs; closed periods are sealed into a chain that *proves* the log was only appended to, and a sealed balance can be proven **and named** without disclosing the rest of the books |
| **Witnessable** | Every proof is relative to a tree head — so an independent cosigner remembers the last head it vouched for and **refuses a second history at the same size**, in the C2SP format Certificate Transparency and Sigsum already speak |
| **Gross-preserving** | Balances carry debit *and* credit totals, so turnover survives netting |
| **Hierarchical** | Accounts are a tree and only leaves are postable, so a node's balance is unambiguous — `Rollup` folds a trial balance up it, inferring grouping nodes and summarising to any depth |
| **Sliceable** | Postings carry caller-named reporting axes; any balance read narrows by axis, by date range, or by log prefix — in one query, on the booking date or the value date |
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
let cash    = journal.register_path("Assets:Cash",  date!(2026-01-01))?;
let revenue = journal.register_path("Income:Sales", date!(2026-01-01))?;

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
# let cash = journal.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = journal.register_path("Income:Sales", date!(2026-01-01))?;
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
# use doubleentry::{Amount, MoneyError, Rounding};
type Eur = Amount<2>;

// Splitting is exact: the parts always re-sum to the whole.
let parts = Eur::parse("100.00")?.distribute(3)?;
assert_eq!(parts.len(), 3);
assert_eq!(Eur::checked_sum(parts.iter().copied())?, Eur::parse("100.00")?);

// Proportional splits use largest-remainder, with ties broken deterministically.
let split = Eur::parse("10.00")?.allocate(&[1, 4])?;
assert_eq!(split, vec![Eur::parse("2.00")?, Eur::parse("8.00")?]);

// Rates carry an explicit rounding mode — there is no answer that is right
// everywhere, and picking one silently is how a ledger drifts.
let net = Eur::parse("1000.00")?;
assert_eq!(net.checked_mul_ratio(19, 100, Rounding::HalfUp)?, Eur::parse("190.00")?);

let cent = Eur::parse("0.01")?;
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::HalfUp)?,   Eur::parse("0.01")?);
assert_eq!(cent.checked_mul_ratio(1, 2, Rounding::HalfEven)?, Eur::parse("0.00")?);

// Excess precision is refused rather than silently rounded.
assert_eq!(Eur::parse("1.234"), Err(MoneyError::PrecisionLoss { scale: 2 }));

// Arithmetic is total: overflow is a value, not a panic.
assert_eq!(Eur::MAX.checked_add(Eur::from_minor(1)), Err(MoneyError::Overflow));
# Ok::<(), Box<dyn std::error::Error>>(())
```

If your application does proportional division itself, the ledger eventually goes off by a
cent and nobody can say which entry did it. That is why splitting lives here — and why
`checked_mul_ratio` does too: the intermediate product of `net × rate` overflows an `i64`
long before either operand is unreasonable, so the alternative is unchecked hand-rolled
arithmetic in the one place the type exists to protect.

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
# let wallet = journal.register_path("Liabilities:Wallet", date!(2026-01-01))?;
# let cash = journal.register_path("Assets:Cash", date!(2026-01-01))?;
// A customer wallet may not be drawn beyond what was funded.
journal.set_account_limit(wallet, BalanceLimit::NoDebitBalance)?;

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

Checked against the balance the **whole entry** would leave behind, per currency, so the
answer never depends on the order the postings were listed in. Both SQL backends enforce it
*inside the append transaction* — a limit checked before the write reads a pre-image that two
concurrent appends both see, each fitting it and together breaching it.

The settled and pending layers fold into **one** answer, asymmetrically: an outstanding
reservation consumes room, an expected inflow grants none. Neither layer alone works —
checking only the settled one lets a hold step around the limit until the money moves, and
checking each against its own zero forbids a cash box from reserving an outflow at all.

→ [Accounts](https://hupe1980.github.io/doubleentry/docs/accounts/)

---

## 📊 Reporting

Every balance read takes a `BalanceQuery`: one type for narrowing by log prefix,
date range, or reporting axis, and the narrowings compose.

```rust
# use doubleentry::{Amount, BalanceKey, BalanceQuery, Currency, DimensionFilter, Dimensions, Entry, EntryId, IdempotencyKey, Journal, Label, Layer, Posting};
# use doubleentry::period::LedgerId;
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let cash = journal.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = journal.register_path("Income:Sales", date!(2026-01-01))?;
# let segment = Label::new("segment")?;
# let retail_dim = Dimensions::none().with(segment.clone(), Label::new("Retail")?)?;
# journal.record(
#     Entry::new(EntryId::generate(), IdempotencyKey::new(b"r".to_vec())?, date!(2026-03-10))
#         .post(Posting::debit(cash, Eur::parse("100.00")?, Currency::EUR).with_dimensions(retail_dim.clone()))
#         .post(Posting::credit(revenue, Eur::parse("100.00")?, Currency::EUR).with_dimensions(retail_dim)))?;
# journal.record(
#     Entry::new(EntryId::generate(), IdempotencyKey::new(b"u".to_vec())?, date!(2026-03-11))
#         .debit(cash, Eur::parse("400.00")?, Currency::EUR)
#         .credit(revenue, Eur::parse("400.00")?, Currency::EUR))?;
# let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };
// A period's *activity* — what an income statement reports — sliced to one axis.
let retail = DimensionFilter::any().matching(segment.clone(), Label::new("Retail")?);
let march  = BalanceQuery::between(date!(2026-03-01), date!(2026-03-31));

let sliced = journal.trial_balance(march.matching(&retail))?;
assert_eq!(sliced.get_or_zero(&key).debits, Eur::parse("100.00")?);

// A slice of a balanced ledger still balances.
assert!(sliced.totals(Currency::EUR, Layer::Settled)?.is_balanced());

// The clause that keeps a set of slices honest: postings carrying no value for
// the axis land in none of them, and would otherwise vanish from every report.
let unattributed = DimensionFilter::any().missing(segment);
let rest = journal.balance(&key, march.matching(&unattributed))?;
assert_eq!(rest.debits, Eur::parse("400.00")?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`over_prefix(n)` counts entries in **log order** — the number a `TreeHead` carries,
so a balance and the root it belongs with are named the same way. `through(d)` and
`between(a, b)` fold by date, so a backdated entry lands where it economically
belongs. Confusing the two is the classic reconciliation mistake.

And *which* date is the other half of the same question. An entry carries a booking
date and a value date, and a report folds by either:

```rust
# use doubleentry::{Amount, BalanceKey, BalanceQuery, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
# use doubleentry::period::LedgerId;
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let cash = journal.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = journal.register_path("Income:Sales", date!(2026-01-01))?;
# let key = BalanceKey { account: cash, currency: Currency::EUR, layer: Layer::Settled };
# // Booked on 28 March, settling on 2 April: March's books, April's cash.
# journal.record(
#     Entry::new(EntryId::generate(), IdempotencyKey::new(b"straddle".to_vec())?, date!(2026-03-28))
#         .with_value_date(date!(2026-04-02))
#         .debit(cash, Eur::parse("400.00")?, Currency::EUR)
#         .credit(revenue, Eur::parse("400.00")?, Currency::EUR))?;
// One entry, booked 28 March with value 2 April.
let march = BalanceQuery::between(date!(2026-03-01), date!(2026-03-31));

assert_eq!(journal.balance(&key, march)?.debits,                 Eur::parse("400.00")?);
assert_eq!(journal.balance(&key, march.by_value_date())?.debits, Eur::ZERO);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Reconcile a trial balance on booking dates and a bank statement on value dates. The
basis is a *reporting* choice: it never moves an entry between periods and never
changes what a seal committed to, or a settlement instruction could reopen sealed
books. A statement takes it too, opening balance included.

Behind a `LedgerStore` this is one round trip, and an equality clause becomes an
inner join on `posting_dimensions (axis, value)` rather than a scan.

→ [Dimensions](https://hupe1980.github.io/doubleentry/docs/dimensions/) ·
[Statements and historical reads](https://hupe1980.github.io/doubleentry/docs/statements/)

---

## 🌲 Hierarchical reporting

Account paths are a hierarchy, and only leaves are postable — which is exactly what
makes a *node's* balance a single defensible number: everything beneath it. `Rollup`
is that number, for every node at once.

```rust
# use doubleentry::period::LedgerId;
# use doubleentry::{Amount, BalanceQuery, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let bank = journal.register_path("Assets:Bank:Main", date!(2026-01-01))?;
# let cash = journal.register_path("Assets:Cash", date!(2026-01-01))?;
# let sales = journal.register_path("Income:Sales", date!(2026-01-01))?;
# journal.record(
#     Entry::new(EntryId::generate(), IdempotencyKey::new(b"i".to_vec())?, date!(2026-03-15))
#         .debit(bank, Eur::parse("1000.00")?, Currency::EUR)
#         .debit(cash, Eur::parse("190.00")?, Currency::EUR)
#         .credit(sales, Eur::parse("1190.00")?, Currency::EUR))?;
let sheet = journal.rollup(BalanceQuery::all(), Currency::EUR, Layer::Settled)?;

// Nodes come back in depth-first pre-order, so indenting by `depth()` is the report.
assert_eq!(
    sheet.nodes().iter().map(|n| n.path.to_string()).collect::<Vec<_>>(),
    ["Assets", "Assets:Bank", "Assets:Bank:Main", "Assets:Cash", "Income", "Income:Sales"],
);

// `Assets` was never registered, and still heads its own subtree.
let assets = sheet.get(&"Assets".parse()?).expect("inferred from its children");
assert!(!assets.is_registered());
assert!(assets.is_aggregate());                       // nothing posted directly to it
assert_eq!(assets.subtree.debits, Eur::parse("1190.00")?);

// A subtree total already carries everything beneath it, so truncating the tree
// shortens the report rather than changing it.
assert_eq!(sheet.to_depth(1).total()?, sheet.total()?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Grouping nodes are inferred from the paths that exist, so a chart of only leaves still
produces a tree — that is grouping by path prefix, and it invents no account. Every node
carries **both** `own` and `subtree`, because neither reconstructs the other, and both are
gross-preserving. One `(currency, layer)` per report: a parent cannot hold the sum of its
children in two currencies.

It is deliberately **not** a `TrialBalance` — rows holding both a parent and its children
do not sum to zero, though the roots do. And it needs no backend method: the fold is pure
over a trial balance and a registry, both of which a `LedgerStore` already serves.

→ [Hierarchical reporting](https://hupe1980.github.io/doubleentry/docs/reporting/)

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

**Proofs cost `O(log n)` reads, not a replay.** The log stores the *tree* — every node whose
subtree is complete, which in an append-only log can never change — rather than only the
leaves, because producing a proof from leaves alone means rebuilding every interior node on
the way. The SQL backends keep those nodes in an INSERT-only `log_nodes` table and read at
most `1 + ⌈log₂ n⌉` rows: 28 at a hundred million entries, against gigabytes of memory for a
replay. Storage is just under two hashes per entry, and the numbering is Go `sumdb/tlog`'s.

The in-memory log and the SQL backends run the *same* plan-and-assemble functions, so a proof
from a database and one from memory are the same proof. RFC 6962's `MTH`, `PATH` and
`SUBPROOF` are kept as a test-only reference and the fast path is checked against them
exhaustively. Because the tree lives apart from the entries, archiving a prefix to the cold
tier costs no provability: the archive supplies the leaf, the hot store the proof.

A proof from the **empty** tree is refused at both ends — `consistency_proof(0)` errors and a
hand-built one fails `verify`. The statement it makes is true of every history that has ever
existed, so an auditor who archived a head before the first entry would get `true` back from a
check that examined nothing. RFC 9162 §2.1.4.2 and Go's `tlog.ProveTree` refuse it for the same
reason.

→ [Proofs](https://hupe1980.github.io/doubleentry/docs/proofs/)

---

## 👁 Witnessing

Every guarantee above is relative to a **tree head**. So an operator who can serve two heads can
serve two histories — head `H₁` to the tax authority, `H₂` to the bank, a complete and internally
consistent log behind each. Every proof either party checks verifies, because nothing is wrong
*inside* either view. No proof can see this; each party only ever sees one side.

A **witness** is somebody who is not the operator, and who remembers:

```rust
# use doubleentry::witness::{MemoryWitnessStore, Origin, Witness, WitnessError};
# use doubleentry::{Hash, MerkleLog};
# let mut honest = MerkleLog::new();
# for i in 0..8u64 { honest.append(Hash::from_bytes([i as u8; 32])); }
# let mut forked = MerkleLog::new();
# for i in 0..7u64 { forked.append(Hash::from_bytes([i as u8; 32])); }
# forked.append(Hash::from_bytes([0xff; 32]));
let origin = Origin::new("example.com/ledgers/acme-gmbh")?;

// Which log to watch, and from which head, is decided out of band — a witness
// that adopted whatever it was shown first could be introduced to the fork.
let mut witness = Witness::new(MemoryWitnessStore::new());
witness.trust(origin.clone(), honest.head())?;

// A second history at a size already vouched for is refused, permanently.
assert!(matches!(
    witness.offer(&origin, forked.head(), None),
    Err(WitnessError::Fork { size: 8, .. })
));

// Growth has to be proven, never asserted — and the witness has not moved.
let mut grown = honest.clone();
grown.append(Hash::from_bytes([9; 32]));
assert!(matches!(
    witness.offer(&origin, grown.head(), None),
    Err(WitnessError::MissingProof { from: 8, to: 9, .. })
));
let proof = grown.consistency_proof_between(8, 9)?;
assert!(witness.offer(&origin, grown.head(), Some(&proof))?.advanced);
# Ok::<(), Box<dyn std::error::Error>>(())
```

A verifier that requires a cosignature from witnesses **it** chose cannot then be shown a forked
history without those witnesses being compromised too. With the `witness` feature, `Cosigner`
runs the state machine first and signs only if it returns — a refused head produces an error and
no bytes at all.

The wire format is [C2SP `signed-note`](https://c2sp.org/signed-note) /
[`tlog-checkpoint`](https://c2sp.org/tlog-checkpoint) /
[`tlog-cosignature`](https://c2sp.org/tlog-cosignature), because a witness is only worth having
if it is *somebody else's* — a bespoke format could only be cosigned by software this crate
ships. **No HTTP is included**: `AddCheckpoint` is the `tlog-witness` request as bytes, for
whatever client you already have. Shipping an async runtime and a TLS stack to save fifteen lines
is not a trade this crate makes.

A witness never sees an entry, a balance or an account — only an origin, a size and a root. That
is what lets it be your auditor, a notary, or a machine in another administrative domain.

→ [Witnessing](https://hupe1980.github.io/doubleentry/docs/witness/)

---

## 🧷 Period seals

Closing a period commits to **which entries** it contains, **what they add up to**, and
**which accounts those totals are for** — all three as Merkle roots. Seals chain, so removing
or reordering a sealed period breaks every seal after it.

Each seal also carries a **consistency proof** from its predecessor's tree, and that is the
load-bearing link. Hash-chaining alone establishes that the seals are in order and unedited and
says nothing about the entries underneath: two seals claiming tree sizes 100 and 200 with
unrelated roots satisfy every other rule, so a log rebuilt from scratch between two closes would
verify byte for byte. The proof turns *"these commitments are in order"* into *"this history was
only ever appended to"* — checkable offline, by someone holding the seals and no database.

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
# use doubleentry::{Amount, BalanceKey, BalanceQuery, Currency, Entry, EntryId, IdempotencyKey, Journal, Layer};
# use doubleentry::period::{LedgerId, Period, PeriodId, PeriodState};
# use doubleentry::seal::TrialBalanceCommitment;
# use time::macros::date;
# type Eur = Amount<2>;
# let mut journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
# let cash = journal.register_path("Assets:Cash", date!(2026-01-01))?;
# let revenue = journal.register_path("Income:Sales", date!(2026-01-01))?;
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
# let closing = journal.trial_balance(BalanceQuery::through(date!(2026-03-31)))?;
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
let proven = store.prove_sealed_balance(&march, cash_key).await?
    .into_proven().expect("cash has a row in the closing balance");
assert!(proven.verify());
assert_eq!(proven.path().to_string(), "Assets:Cash");
```

The two "nothing to prove" answers — no row, and not registered when the period sealed — come
back as `SealedBalanceOutcome` variants rather than errors, because the books are intact and the
question simply has a negative reply. `is_absent()` covers both at once.

→ [Periods and seals](https://hupe1980.github.io/doubleentry/docs/periods-and-seals/)

---

## 💾 Persistence

The engine keeps no storage of its own. `LedgerStore` defines what a backend must do, and the
**conformance suite** — twenty-five executable checks — is what decides whether an
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
| `witness` | Ed25519 cosigning of tree heads, in the C2SP signed-note format. The witness *state machine* needs no cryptography and is always available; this adds the keys |

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

It also ships no transport. The witness protocol is C2SP's, and `AddCheckpoint` gives you the
request bytes — the HTTP client stays yours, because an async runtime and a TLS stack are a
large thing to inflict on every user who never touches a witness.

Two structural rules *are* enforced, because no downstream layer can repair them:

- **Only leaves are postable.** An account that is both a bucket and a container has two
  defensible balances — its own postings, and its subtree's — and no report can un-mix them.
  Checked when an entry is validated *and* when an account is registered, so a leaf that has
  been posted to cannot later acquire a child.
- **Postings fall inside the account's open window.**

And one you opt into per account, for the same reason — an overdrawn cash account is not
something a report can repair either: a **balance limit**, checked against the balance an
entry would leave behind.

---

## 🩺 Checking the books

Four independent self-checks, and knowing to run all four is itself a thing to get wrong —
so there is one call that does:

```rust
# use doubleentry::{Journal, LedgerId};
# let journal = Journal::<2>::new(LedgerId::new("acme-gmbh")?);
let report = journal.audit();
assert!(report.passed(), "{report}");
# Ok::<(), Box<dyn std::error::Error>>(())
```

It checks that the Merkle log commits to these entries, that the maintained balances match
them, that debits equal credits per currency **and** layer, and that the seal chain holds *and
describes these books* — this log **and** this chart of accounts, because a seal's balances
are keyed on account *handles* and re-registering the same paths in a different order repoints
every one of them while leaving every hash in the chain matching. All four run — they are
independent, and the first failure is not necessarily the most informative — and a failing
report names which and why.

`O(n)`: an audit-time operation. Run it after a restore, after a migration, and before handing
anything to an auditor.

---

## 🧪 Testing

Invariants are covered by property tests over generated inputs, not hand-picked cases. Beyond
that: randomised **simulation** with every invariant re-checked after each step; committed
**golden vectors** for the canonical encoding, the seal preimage and the Merkle log;
**robustness** tests asserting no input can panic a parser; **cost** guards on the shape of the
curve along both axes a ledger grows on — entries *and* accounts; the **conformance** suite;
and **real databases** — SQLite in-process, PostgreSQL in a throwaway container, nothing
mocked.

```console
cargo test                                   # everything but the databases
cargo test --features sqlite                 # adds SQLite; no server needed
cargo test --features postgres               # adds PostgreSQL; needs Docker
cargo test --features iceberg                # adds the cold tier; writes to a temp dir
cargo test --features witness                # adds cosigning
cargo clippy --all-targets --all-features
```

`tests/witness.rs` is worth singling out: it builds the split-view attack against two real
journals, asserts that **every proof in each one passes** — the weakness, stated rather than
glossed — and then asserts that a witness refuses the fork anyway.

The crate forbids `unsafe_code`, and denies `arithmetic_side_effects`, `indexing_slicing`,
`unwrap_used`, `expect_used`, and `panic` in library code. A CI job greps the engine for
clocks, I/O, async and unsafe, so determinism is checked rather than trusted.

→ [Design boundaries and testing](https://hupe1980.github.io/doubleentry/docs/design/)

---

## 📄 License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
