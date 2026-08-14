# doubleentry — task runner (https://just.systems)
#
# `just` with no arguments lists every recipe.

set shell := ["bash", "-uc"]

# MSRV — keep in sync with `rust-version` in Cargo.toml, rust-toolchain.toml,
# the `msrv` job in .github/workflows/ci.yml, and the badge in README.md.
msrv := "1.94"

# Every feature except `postgres`, which needs a running Docker daemon.
portable := "serde,sqlite,iceberg"

# 📋 List all recipes
default:
    @just --list

# ✅ Everything CI runs, in CI order
ci: fmt-check lint purity test doc package
    @echo "✅ all checks passed"

# 🧊 Enforce "no clock, no I/O, no async, no unsafe" over the engine
purity:
    #!/usr/bin/env bash
    # Storage backends are exempt by construction — talking to a database is
    # their whole job — so this covers src/ minus src/storage.
    #
    # `now_v7` is in the pattern because it reads the clock as surely as
    # `SystemTime::now` does, and it is easy to reach for. The two identifier
    # generators that legitimately call it opt out with an explicit marker, so a
    # third one cannot appear by accident.
    set -uo pipefail
    hits="$(grep -rn --include='*.rs' -E \
        'now_utc|now_v7|SystemTime::now|Instant::now|std::(fs|env|net|process)|\basync\b|\bunsafe\b' \
        src/ \
        | grep -v '^src/storage' \
        | grep -vE ':[[:space:]]*(///|//!|//)' \
        | grep -v 'purity-exempt' || true)"
    if [ -n "$hits" ]; then
        echo "❌ ambient state, async or unsafe reached the engine:" >&2
        echo "$hits" >&2
        echo "" >&2
        echo "The engine promises equal inputs give equal bytes. Take the date" >&2
        echo "as a parameter, and keep I/O behind a LedgerStore. If a clock read" >&2
        echo "is genuinely off the deterministic path, say so with a" >&2
        echo "'purity-exempt' marker on the line and document why." >&2
        exit 1
    fi
    echo "🧊 pure: no clock, no I/O, no async, no unsafe"

# 🎨 Format the workspace
fmt:
    cargo fmt --all

# 🎨 Fail if anything is unformatted
fmt-check:
    cargo fmt --all -- --check

# 📎 Clippy with warnings denied (default + all features)
lint:
    cargo clippy --all-targets -- -D warnings
    cargo clippy --all-targets --all-features -- -D warnings

# 🔎 Fast type-check, all features
check:
    cargo check --all-targets --all-features

# 🧪 Full test suite, Docker-backed PostgreSQL included
test:
    cargo test --all-features

# 🧪 Everything that runs without a Docker daemon
test-portable:
    cargo test --features {{ portable }} --all-targets
    cargo test --features {{ portable }} --doc

# 🐘 The PostgreSQL backend only (needs Docker)
test-postgres:
    cargo test --features postgres --test postgres

# 📐 The conformance suite against every backend
conformance:
    cargo test --all-features conforms -- --nocapture

# 🔒 Golden vectors: canonical encoding, entry hash, seal hash, Merkle roots
golden:
    cargo test --all-features --test golden

# 🔒 Print current vector values, for deliberately re-pinning them
golden-emit:
    cargo test --all-features --test golden -- --ignored --nocapture

# 🧪 Run tests matching a filter, e.g. `just test-one seal`
test-one filter:
    cargo test --all-features {{ filter }} -- --nocapture

# 🎛️ Build every feature in isolation, then all together
features:
    cargo build --no-default-features
    cargo build --no-default-features --features serde
    cargo build --no-default-features --features sqlite
    cargo build --no-default-features --features postgres
    cargo build --no-default-features --features iceberg
    cargo build --all-features

# 🦀 Compile on the pinned MSRV
msrv:
    RUSTUP_TOOLCHAIN={{ msrv }} cargo check --all-features --all-targets

# 📚 Build the docs with warnings denied
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

# 📚 Build and open the docs
doc-open:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --open

# 🌐 Build the site (needs `zola`); internal links are checked, not assumed
site:
    cd site && zola build && zola check

# 🌐 Serve the site locally with live reload
site-serve:
    cd site && zola serve

# 📦 Dry-run the crates.io package (catches bad metadata before tagging)
package:
    # `--allow-dirty` is local convenience; CI runs this on a clean checkout.
    cargo publish --dry-run --all-features --allow-dirty

# 🛡️ Audit dependencies for advisories (needs `cargo install cargo-audit`)
audit:
    cargo audit

# ⬆️ Show outdated dependencies (needs `cargo install cargo-outdated`)
outdated:
    cargo outdated --root-deps-only

# 🐳 Reclaim space from testcontainers leftovers
docker-prune:
    docker container prune -f
    docker volume prune -f

# 🏷️ Tag the current Cargo.toml version and push it — triggers release.yml
tag:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(cargo metadata --no-deps --format-version 1 \
        | grep -o '"version":"[^"]*"' | head -1 | cut -d'"' -f4)"
    if [ -n "$(git status --porcelain)" ]; then
        echo "❌ working tree is dirty — commit first" >&2
        exit 1
    fi
    just ci
    git tag -a "v${version}" -m "v${version}"
    echo "🏷️  tagged v${version} — push with: git push origin v${version}"

# 🧹 Remove build artifacts
clean:
    cargo clean
