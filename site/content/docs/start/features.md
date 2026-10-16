+++
title = "Features and MSRV"
description = "Cargo features, what each adds, TLS for PostgreSQL, and the MSRV policy."
weight = 4
+++

## The features

All features are **off by default**. Without any, the crate is the rule core,
both façades, `MemoryStore`, proofs, reports, the export view and every C2SP
text format — no async runtime, no database driver, no signature library. The
same list is rendered under "Feature flags" on the
[crate root](https://docs.rs/doubleentry/latest/doubleentry/index.html).

| Feature | Adds | Brings |
|---|---|---|
| `serde` | `Serialize` / `Deserialize` on all public types; the hashed encoding never goes through serde | `serde` |
| `sqlite` | `SqlStore<Sqlite>`, SQLite bundled | `sqlx` (tokio runtime) |
| `postgres` | `SqlStore<Postgres>`, no TLS stack | `sqlx` (tokio runtime) |
| `witness` | Ed25519: `LogKey` signs checkpoints; cosignatures and policies verify; `WitnessSession`, `Verifier` | `ed25519-dalek` |
| `testing` | `doubleentry::testing`: the conformance suite for stores (with `witness`, also `TestWitness`) | `tokio` |
| `tracing` | a span per façade operation and per SQL store write — never an amount, memo or account path | `tracing` |

Always on: `sha2`, `time`, `unicode-normalization` (NFC for names),
`getrandom` (memo salts), `thiserror` and `document-features`.

Without `witness`, notes, checkpoints, vkeys, policies, requests, bundles and
tiles still parse and render, so a monitor that only reads checkpoints needs no
Ed25519. The crate has no HTTP client and no witness: it builds
`add-checkpoint` requests and reads replies as bytes.

## TLS for PostgreSQL

`sqlx` is compiled without a TLS stack; the application picks one with one line,
and Cargo's feature unification switches it on for the crate's connections:

```toml
[dependencies]
doubleentry = { version = "0.8", features = ["postgres"] }
sqlx = { version = "0.9", features = ["tls-rustls-aws-lc-rs"] }   # or tls-rustls-ring-webpki, tls-native-tls
```

The connection URL then asks for it, for example `?sslmode=require`. SQLite
never needs TLS.

## Runtime

`Ledger`'s methods are plain `async fn`s. The SQL stores run on sqlx's tokio
runtime, so `SqlStore` is used inside a tokio runtime — `#[tokio::main]` or one
the program builds
([With a database](@/docs/start/with-a-database.md#without-an-async-runtime-of-your-own)).
`Journal` needs no runtime.

## MSRV

Rust **1.94**, edition 2024, set by `rust-version` in `Cargo.toml` and required
by sqlx 0.9. CI builds every feature and target with exactly that toolchain and
`--locked`. The MSRV is a stable release at least six months old when adopted,
and rises only in a non-patch release, with a CHANGELOG line naming the reason.

## Limits

- `tracing` only emits spans; the subscriber is the application's.
- `testing::TestWitness` is a test double; the crate ships no production witness.
- The MSRV covers the locked dependencies; newer versions an application's own
  lock file picks may need a newer compiler.
