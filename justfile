# doubleentry — task runner (https://just.systems)
#
# CI jobs (.github/workflows/ci.yml) run the recipe of the same name. Recipes
# use stable unless they say otherwise; cargo calls pass `--locked`. The one
# `#` line above a recipe is what `just --list` shows.

set shell := ["bash", "-uc"]

# The MSRV, read from `rust-version` in Cargo.toml — its single source.
msrv := `sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml`

# Every feature except `postgres` (needs Docker). Same as PORTABLE_FEATURES in ci.yml.
portable := "serde,sqlite,witness,testing,tracing"

# 📋 List all recipes
default:
    @just --list

# ✅ The main CI checks (no nightly, Go or MSRV toolchain needed; Docker for PostgreSQL)
ci: fmt-check lint purity features test examples doc package deny site-verify docs-stale
    @echo "✅ all checks passed"

# 🧊 Enforce "no clock, no I/O, no async, no unsafe, no hash order" over the pure core
purity:
    #!/usr/bin/env bash
    # Scope: the pure modules (core, model, log, report, transparency, export).
    # The façades, stores and test support talk to the OS and are out of scope.
    # `HashMap`/`HashSet` iterate in random order. A line off the deterministic
    # path opts out with a `purity-exempt` marker and a reason.
    set -uo pipefail
    scope=()
    for dir in src/core src/model src/log src/report src/transparency src/export; do
        [ -d "$dir" ] && scope+=("$dir")
    done
    hits="$(grep -rn --include='*.rs' -E \
        'now_utc|now_v7|SystemTime::now|Instant::now|std::(fs|env|net|process)|\basync\b|\bunsafe\b|\bHash(Map|Set)\b|\brand::' \
        "${scope[@]}" \
        | grep -vE ':[[:space:]]*(///|//!|//)' \
        | grep -v 'purity-exempt' || true)"
    if [ -n "$hits" ]; then
        echo "❌ ambient state, async, unsafe or hash order reached the pure core:" >&2
        echo "$hits" >&2
        echo "" >&2
        echo "The core promises equal inputs give equal bytes. Take the date or" >&2
        echo "the salt as a parameter, use ordered maps, and keep I/O in a store" >&2
        echo "or a façade. If a line is genuinely off the deterministic path, say" >&2
        echo "so with a 'purity-exempt' marker on it and document why." >&2
        exit 1
    fi
    echo "🧊 pure: ${scope[*]}"

# 🎨 Format the workspace
fmt:
    cargo fmt --all

# 🎨 Fail if anything is unformatted
fmt-check:
    cargo fmt --all -- --check

# 📎 Clippy with warnings denied (default + all features)
lint:
    cargo clippy --locked --all-targets -- -D warnings
    cargo clippy --locked --all-targets --all-features -- -D warnings

# 🔎 Fast type-check, all features
check:
    cargo check --locked --all-targets --all-features

# 🧪 Full test suite, Docker-backed PostgreSQL included
test:
    cargo test --locked --all-features

# 🧪 Everything that runs without a Docker daemon
test-portable:
    cargo test --locked --all-targets
    cargo test --locked --features {{ portable }} --all-targets
    cargo test --locked --features {{ portable }} --doc

# 🐘 The PostgreSQL store only (needs Docker)
test-postgres:
    cargo test --locked --features testing,postgres --test postgres

# 📐 The conformance suite against every store (PostgreSQL needs Docker)
conformance:
    cargo test --locked --features testing,sqlite --test memory --test sqlite conforms -- --nocapture
    cargo test --locked --features testing,postgres --test postgres conforms -- --nocapture

# ▶️ Build and run every example under examples/ (they assert their outcomes)
examples:
    #!/usr/bin/env bash
    # The portable features cover every example's `required-features`.
    set -euo pipefail
    shopt -s nullglob
    ran=0
    for file in examples/*.rs; do
        name="$(basename "$file" .rs)"
        echo "▶️  $name"
        cargo run --locked --quiet --features {{ portable }} --example "$name"
        ran=$((ran + 1))
    done
    echo "▶️  $ran examples ran"

# 🔭 The public API (nightly cargo-public-api) against tests/public-api.txt
public-api:
    #!/usr/bin/env bash
    # The listing is the reviewed surface; update it with `just public-api-bless`.
    # No dependency path but `time::Date`, and no item under two paths except
    # through `prelude`.
    set -euo pipefail
    listing="$(cargo +nightly public-api --all-features --simplified)"
    if ! diff -u tests/public-api.txt <(echo "$listing"); then
        echo "❌ the public API changed: review the diff, then 'just public-api-bless'" >&2
        exit 1
    fi
    # A driver's types render under their defining crate (`sqlx_core::…`).
    forbidden="$(grep -E '\b(sqlx[a-z_]*|tokio[a-z_]*|ed25519_dalek|curve25519_dalek|sha2|digest|tracing[a-z_]*|getrandom)::' <<< "$listing" || true)"
    if [ -n "$forbidden" ]; then
        echo "❌ a dependency's path is in the public API:" >&2
        echo "$forbidden" >&2
        exit 1
    fi
    # Of the `time` crate only `Date` may appear (`core::time` is std).
    timed="$(grep -oE '(^|[^:a-z_])time::[A-Za-z_:]+' <<< "$listing" | grep -v 'time::date::Date' || true)"
    if [ -n "$timed" ]; then
        echo "❌ a time type other than Date is in the public API:" >&2
        echo "$timed" | sort -u >&2
        exit 1
    fi
    # Distinct items that share a name: export rows vs. store rows, and
    # transparency::Error vs. doubleentry::Error.
    distinct='^(struct (AccountRow|PostingRow|SealRow)|enum Error):'
    duplicates="$(grep -E '^(#\[[^]]*\] )?pub (struct|enum|trait|type|const|static|macro|union) ' <<< "$listing" \
        | grep -v '^pub const fn ' \
        | sed -E 's/^(#\[[^]]*\] )?pub (struct|enum|trait|type|const|static|macro|union) ([A-Za-z0-9_:!]+).*/\2 \3/; s/:$//' \
        | grep -E ' doubleentry(::[a-z_][a-z0-9_]*)*::[A-Za-z_][A-Za-z0-9_]*!?$' \
        | grep -v ' doubleentry::prelude::' \
        | awk '{n = split($2, part, "::"); print $1, part[n], $2}' | sort -u \
        | awk '{key = $1 " " $2; count[key]++; paths[key] = paths[key] " " $3}
               END {for (key in count) if (count[key] > 1) print key ":" paths[key]}' \
        | grep -vE "$distinct" || true)"
    if [ -n "$duplicates" ]; then
        echo "❌ items reachable under two paths (only prelude may add one):" >&2
        echo "$duplicates" >&2
        exit 1
    fi
    echo "🔭 public API: $(wc -l < tests/public-api.txt | tr -d ' ') lines, as reviewed"

# 🔭 Re-snapshot tests/public-api.txt after a reviewed surface change
public-api-bless:
    cargo +nightly public-api --all-features --simplified > tests/public-api.txt

# 🔒 Golden vectors (record encoding and hashes, roots, proofs) and the committed C2SP vectors
golden:
    cargo test --locked --all-features --test golden --test transparency_interop

# 🤝 C2SP interop with the Go reference implementations, both ways (needs Go)
interop:
    #!/usr/bin/env bash
    # 1. Go writes tests/vectors/c2sp/ from fixed seeds (byte-identical every
    #    run; a diff is a format change to review and commit).
    # 2. The crate reads them (`just golden` does this without Go).
    # 3. The crate writes its own artefacts to target/interop/; Go verifies
    #    them and runs the stock transparency-dev witness on its requests.
    # 4. The crate verifies the cosignatures the stock witness returned.
    set -euo pipefail
    (cd interop/c2sp && go run ./gen)
    cargo test --locked --all-features --test transparency_interop
    cargo test --locked --all-features --test transparency_interop -- --ignored --exact emit
    (cd interop/c2sp && go run ./check)
    cargo test --locked --all-features --test transparency_interop -- --ignored --exact witness_roundtrip
    echo "🤝 interop: Go and the crate agree both ways"

# 🔒 Print current vector values, for deliberately re-pinning them
golden-emit:
    cargo test --locked --all-features --test golden -- --ignored --nocapture

# 🧪 Run tests matching a filter, e.g. `just test-one seal`
test-one filter:
    cargo test --locked --all-features {{ filter }} -- --nocapture

# 🎛️ Check no features, each feature alone, then all together
features:
    # `--each-feature` includes "no features"; library only. The all-features
    # pass covers every target.
    cargo hack check --locked --each-feature
    cargo check --locked --all-features --all-targets

# 🦀 Check on the MSRV from Cargo.toml; README badge must agree
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! grep -q "badge/rustc-{{ msrv }}+-" README.md; then
        echo "❌ README.md MSRV badge does not name rust-version {{ msrv }} from Cargo.toml" >&2
        exit 1
    fi
    cargo +{{ msrv }} check --locked --all-features --all-targets
    echo "🦀 MSRV {{ msrv }}: ok"

# 🪶 Every direct dependency at its declared floor, on the MSRV
minimal-versions:
    #!/usr/bin/env bash
    # Resolved on nightly in a temporary copy (Cargo.lock untouched), then
    # tested on the MSRV with the portable features and checked with all.
    set -euo pipefail
    root="$(pwd)"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    rsync -a --exclude target --exclude .git --exclude concepts --exclude specs \
        --exclude site --exclude fuzz ./ "$work/"
    cd "$work"
    export CARGO_TARGET_DIR="$root/target/minimal-versions"
    cargo +nightly update -Z direct-minimal-versions
    cargo +{{ msrv }} test --locked --features {{ portable }} --all-targets
    cargo +{{ msrv }} test --locked --features {{ portable }} --doc
    cargo +{{ msrv }} check --locked --all-features --all-targets
    echo "🪶 minimal versions: ok on {{ msrv }}"

# 📚 Build the docs with warnings denied
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features

# 📚 Build the docs with warnings denied, and open them
doc-open:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features --open

# 📚 Build the docs exactly as docs.rs does (nightly, --cfg docsrs)
docsrs:
    RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo +nightly doc --locked --no-deps --all-features

# 🌐 Compile and run every Rust block on the site
site-verify:
    # `rust,no_run` blocks are compiled only; `rust,ignore` fails.
    python3 -I scripts/site-verify.py

# 🧹 Fail on 0.7-only API names in README, site and rustdoc; check docs.rs links and GoBD citations
docs-stale:
    # CHANGELOG.md and lines marked `stale-ok` are exempt.
    python3 -I scripts/docs-stale.py

# 🧪 Check every local proof-of-concept test has a regression test or a design decision (local only)
poc-audit:
    bash scripts/poc-audit.sh

# 🌐 Build and link-check the site (needs `zola`)
site:
    cd site && zola build && zola check

# 🌐 Serve the site locally with live reload
site-serve:
    cd site && zola serve

# 📦 Dry-run the crates.io package
package:
    # `--allow-dirty` for local use; CI runs on a clean checkout.
    cargo publish --dry-run --locked --all-features --allow-dirty

# 📦 Fail if the package would ship a non-crate path
package-list:
    #!/usr/bin/env bash
    set -euo pipefail
    list="$(cargo package --list --locked --all-features --allow-dirty)"
    bad="$(grep -E '^(fuzz|scripts|site|concepts|specs|\.specify|\.claude|\.github|\.cargo)/|^deny\.toml$' <<< "$list" || true)"
    if [ -n "$bad" ]; then
        echo "❌ the package would ship non-crate paths:" >&2
        echo "$bad" >&2
        exit 1
    fi
    echo "📦 package list: $(wc -l <<< "$list" | tr -d ' ') files, crate paths only"

# 🛡️ Advisories, licenses, bans and sources (cargo-deny)
deny:
    cargo deny --all-features --locked check

# 🧭 Semver check against the latest crates.io release
semver:
    cargo semver-checks check-release --all-features

# 🐛 Compile every fuzz target (nightly, cargo-fuzz)
fuzz-build:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! cargo +nightly fuzz --version >/dev/null 2>&1; then
        echo "❌ needs a nightly toolchain and cargo-fuzz (cargo install cargo-fuzz --locked)" >&2
        exit 1
    fi
    # The host triple, explicitly: a prebuilt cargo-fuzz may default to its own.
    cargo +nightly fuzz build --target "$(rustc +nightly -vV | sed -n 's/^host: //p')"

# 🐛 Run one fuzz target; new inputs go to target/, seeds stay in fuzz/corpus/
fuzz TARGET SECS="60":
    #!/usr/bin/env bash
    set -euo pipefail
    # Targets: record note witness_response policy bundle serde values.
    mkdir -p "target/fuzz-corpus/{{ TARGET }}" "fuzz/corpus/{{ TARGET }}"
    cargo +nightly fuzz run --target "$(rustc +nightly -vV | sed -n 's/^host: //p')" {{ TARGET }} \
        "{{ justfile_directory() }}/target/fuzz-corpus/{{ TARGET }}" \
        "{{ justfile_directory() }}/fuzz/corpus/{{ TARGET }}" \
        -- -max_total_time={{ SECS }} -timeout=10 -rss_limit_mb=4096

# ⏱️ Run the benchmarks (filter: memory, sqlite, postgres, proofs; journal_memory with --bench)
bench *ARGS:
    # PostgreSQL needs Docker.
    cargo bench --locked --all-features {{ ARGS }}

# ⏱️ Compile every benchmark without running it
bench-build:
    cargo bench --locked --all-features --no-run

# 🗺️ Check the local roadmap against specs/, and that nothing published links into either (local only)
concepts-check:
    bash scripts/concepts-check.sh

# 🐳 Reclaim space from testcontainers leftovers
docker-prune:
    docker container prune -f
    docker volume prune -f

# 🏷️ Tag the Cargo.toml version after `just ci` (push triggers release.yml)
tag:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(cargo metadata --locked --no-deps --format-version 1 \
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
