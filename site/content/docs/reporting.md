+++
title = "Hierarchical reporting"
description = "Folding a trial balance up the account tree — the report the account hierarchy exists for."
weight = 14
+++

## Why the tree is a tree

An account path is a hierarchy, and [only leaves are
postable](@/docs/accounts.md). That second rule is what makes a *node's* balance
a single defensible number: an account that were both a bucket and a container
would have two — its own postings, and its subtree's — and every report would
have to pick one silently.

A `Rollup` is that number, for every node at once.

```rust
use doubleentry::{BalanceQuery, Currency, Layer};

let sheet = journal.rollup(BalanceQuery::through(date!(2026-03-31)), Currency::EUR, Layer::Settled)?;

for node in sheet.nodes() {
    println!("{:indent$}{:<28} {:>12}",
        "", node.path, node.subtree.signed_net()?,
        indent = (node.depth() - 1) * 2);
}
```

```text
Assets                            1190.00
  Assets:Bank                     1000.00
    Assets:Bank:Main              1000.00
  Assets:Cash                      190.00
Income                           -1190.00
  Income:Sales                   -1190.00
```

Nodes come back in **path order**, which for segment-wise paths is depth-first
pre-order: a parent always precedes its children and siblings are adjacent. So
printing them in order, indented by `depth()`, *is* the report — there is no tree
to walk and no recursion to write.

## Own and subtree

Every node carries both figures, because neither reconstructs the other:

| Field | Means |
|---|---|
| `own` | what was posted **directly** to this path. Zero for an aggregation node |
| `subtree` | this node **and everything beneath it**. The number a balance sheet prints |

A subtree total cannot be split back into the postings that landed on the node
itself, and a set of own-balances cannot be summed by a reader who does not know
the tree.

Both are [gross-preserving](@/docs/debits-and-credits.md) — debits and credits
summed separately, never netted on the way up — so an account with heavy
offsetting turnover still reports it at every level above.

## Grouping nodes are inferred, not invented

`Assets` appears in the report above whether or not it was ever registered.

That is grouping by path prefix, and it asserts nothing about `Assets` being an
account: the crate still does not know what the word means. A node the registry
knows carries its handle and its `AccountKind`; one it does not carries neither,
and `is_registered()` says which.

```rust
let assets = sheet.get(&"Assets".parse()?).expect("inferred from its children");
assert!(!assets.is_registered());
assert!(assets.is_aggregate());          // nothing was posted directly to it
```

Without it a chart that registers only leaves — the default, since
[ancestors are not created implicitly](@/docs/accounts.md) — would produce a
report with no hierarchy in it. Register the parents when you want them
classified or named; it is not a precondition for seeing them.

## One currency, one layer

A report is scoped to a single `(currency, layer)`.

A parent cannot hold the sum of its children in two currencies. The same goes for
the [layer](@/docs/entries.md#pending-and-settled): a reservation and a settled
movement are different claims, and a subtree total mixing them would report money
as moved that has not.

A multi-currency report is several reports, because that is what it is:

```rust
for currency in journal.currencies() {
    let sheet = journal.rollup(query, currency, Layer::Settled)?;
    // …
}
```

## Summarising to a depth

```rust
let summary = sheet.to_depth(2);
```

Nothing is recomputed: a node's `subtree` already carries everything beneath it,
so truncating makes the report **shorter rather than wrong**. The surviving
deepest nodes are where the pruned figures show, and `total()` is unchanged.
`own` keeps its meaning throughout.

## It is a view, not a trial balance

`Rollup` is deliberately a distinct type from `TrialBalance`.

A trial balance satisfies the trial-balance identity: its rows sum to zero. Rows
holding both a parent *and* its children do not — that is the same money counted
at every level. What does hold is that the **roots** sum to the trial balance:

```rust
assert_eq!(sheet.total()?, trial_balance.totals(Currency::EUR, Layer::Settled)?);
```

Every own-balance sits under exactly one root, so a report over balanced books is
balanced. A property test over generated charts with overlapping prefixes checks
it: double counting a node under two ancestors, or dropping one whose parent was
never registered, breaks that sum and nothing else notices.

## Behind a store

No backend method exists for this, and none is needed. The fold is a pure
function of a trial balance and a registry, and a
[`LedgerStore`](@/docs/persistence.md) already serves both:

```rust
let registry = AccountRegistry::from_records(store.accounts().await?)?;
let balances = store.trial_balance(BalanceQuery::through(as_at)).await?;
let sheet    = Rollup::of(&balances, &registry, Currency::EUR, Layer::Settled)?;
```

Because the fold is pure, a report built this way and one from `Journal::rollup`
are the same report — so there is nothing here for a backend to get wrong, and
nothing for the conformance suite to check. A test asserts the two agree anyway.

## Getting back to the postings

A report line is a starting point, not an ending. `key_of` hands back the
`BalanceKey` a registered node's own balance came from, which is what
[statements](@/docs/statements.md), [open items](@/docs/open-items.md) and
[balance proofs](@/docs/periods-and-seals.md) are all keyed on:

```rust
if let Some(key) = sheet.key_of(node) {
    let lines = journal.statement(&key, query)?;
}
```

An inferred grouping node has no key, because it has no balance of its own to
read back.
