#!/usr/bin/env python3
"""The docs guard behind `just docs-stale` (026 FR-016, FR-023).

1. **Stale names.** No 0.7-only API name in README.md, site/content/ or the
   doc comments (`//!`, `///`) under src/. Matching is case-sensitive and
   whole-word, so prose such as "balanced entries" never matches. CHANGELOG.md
   is exempt (its 0.7 → 0.8 map names old items by design), and so is a line
   carrying the marker `stale-ok` (`<!-- stale-ok -->` in Markdown,
   `// stale-ok` in code) — the Assurance → Design page uses it where it names
   what was removed and why.
2. **docs.rs links.** Every `docs.rs/doubleentry/latest/doubleentry/…` link in
   README.md and site/content/ names an item of tests/public-api.txt (the
   reviewed public surface, all features), including its method, variant or
   field anchor.
3. **GoBD evidence.** Every test the GoBD matrix cites as
   `` `tests/<file>.rs::<fn>` `` or `` `src/<path>.rs::<fn>` `` exists.

Stdlib only; run with `python3 -I`.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Names that existed in 0.7 (or in an earlier 0.8 draft) and are gone. The
# list grows when a release removes more.
STALE = [
    "SealContext",
    "Entry<Balanced",
    "<Balanced",
    "adopt_verified",
    "record_validated",
    "EntryBatch",
    "LedgerStore",
    "DynLedgerStore",
    "verify_store",
    "migrate(",
    "migrate()",
    "ColdTier",
    "iceberg",
    "Iceberg",
    "prune_through",
    "BLAKE3",
    "blake3",
    "PeriodState::Sealed",
    "calendar_mut",
    "transition_period",
    "SealedBalanceOutcome",
    "Witness::offer",
    "WitnessStore",
    "EntryId::generate",
    "PostgresStore",
    "SqliteStore",
    "TrialBalanceCommitment",
    "AccountBindingProof",
    "VerifiedBalance",
]

WORD = r"[A-Za-z0-9_]"


def pattern(name):
    left = f"(?<!{WORD})" if re.match(WORD, name[0]) else ""
    right = f"(?!{WORD})" if re.match(WORD, name[-1]) else ""
    return re.compile(left + re.escape(name) + right)


PATTERNS = [(name, pattern(name)) for name in STALE]
DOC_LINE = re.compile(r"^\s*(//!|///)")


def markdown_files():
    files = [ROOT / "README.md"]
    files += sorted((ROOT / "site" / "content").rglob("*.md"))
    return [f for f in files if f.exists()]


def check_stale():
    hits = []
    sources = [(f, None) for f in markdown_files()]
    sources += [(f, DOC_LINE) for f in sorted((ROOT / "src").rglob("*.rs"))]
    schema = ROOT / "schema"
    if schema.is_dir():
        sources += [(f, None) for f in sorted(schema.rglob("*")) if f.is_file()]
    for path, only in sources:
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if only is not None and not only.match(line):
                continue
            if "stale-ok" in line:
                continue
            for name, rx in PATTERNS:
                if rx.search(line):
                    hits.append(f"{path.relative_to(ROOT)}:{number}: `{name}`: {line.strip()}")
    return hits


DOCS_RS = re.compile(r"https://docs\.rs/doubleentry/latest/doubleentry/([A-Za-z0-9_./#-]*)")


def api_listing():
    return (ROOT / "tests" / "public-api.txt").read_text(encoding="utf-8")


def resolve(link, api):
    """None when the docs.rs link names an item of the public API, else why not."""
    target, _, anchor = link.partition("#")
    parts = [p for p in target.split("/") if p]
    if not parts or parts[-1] in ("", "index.html", "all.html"):
        module = "::".join(["doubleentry", *parts[:-1]]) if parts else "doubleentry"
        if module == "doubleentry" or re.search(rf"^pub mod {re.escape(module)}$", api, re.M):
            return None
        return f"no module {module}"
    page = parts[-1]
    m = re.fullmatch(r"(struct|enum|trait|macro|fn|type|constant|mod)\.([A-Za-z0-9_]+)\.html", page)
    if not m:
        return f"unrecognised page {page}"
    kind, name = m.groups()
    path = "::".join(["doubleentry", *parts[:-1], name])
    word = {"constant": "const", "mod": "mod"}.get(kind, kind)
    if kind == "macro":
        item = re.compile(rf"^pub macro {re.escape(path)}!", re.M)
    elif kind == "fn":
        item = re.compile(rf"^pub (const |async )?fn {re.escape(path)}(<|\()", re.M)
    else:
        item = re.compile(rf"^(#\[[^\]]*\] )?pub {word} {re.escape(path)}(<|:|$| |\()", re.M)
    if not item.search(api):
        return f"no {kind} {path}"
    if not anchor:
        return None
    a = re.fullmatch(r"(method|tymethod|variant|structfield|associatedconstant|associatedtype|impl-[^.]*)\.?([A-Za-z0-9_]*)", anchor)
    if not a:
        return f"unrecognised anchor #{anchor}"
    what, member = a.groups()
    if what.startswith("impl-"):
        return None
    generic = rf"{re.escape(path)}(<[^>]*>)?::{re.escape(member)}"
    if what in ("method", "tymethod"):
        rx = re.compile(rf"fn {generic}(<|\()")
    elif what == "variant":
        rx = re.compile(rf"{generic}($|\(| )", re.M)
    elif what == "structfield":
        rx = re.compile(rf"^pub {generic}:", re.M)
    else:
        rx = re.compile(rf"{generic}")
    if not rx.search(api):
        return f"no {what} {path}::{member}"
    return None


def check_links():
    api = api_listing()
    bad = []
    for path in markdown_files():
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for m in DOCS_RS.finditer(line):
                why = resolve(m.group(1), api)
                if why:
                    bad.append(f"{path.relative_to(ROOT)}:{number}: {m.group(0)}: {why}")
    return bad


CITED = re.compile(r"`((?:tests|src)/[A-Za-z0-9_/]+\.rs)::([A-Za-z0-9_]+)`")


def check_gobd():
    bad = []
    pages = sorted((ROOT / "site" / "content").rglob("gobd.md"))
    for page in pages:
        text = page.read_text(encoding="utf-8")
        cited = CITED.findall(text)
        if not cited:
            bad.append(f"{page.relative_to(ROOT)}: cites no test as `tests/<file>.rs::<fn>`")
        for file, fn in cited:
            source = ROOT / file
            if not source.exists():
                bad.append(f"{page.relative_to(ROOT)}: `{file}::{fn}`: no such file")
            elif not re.search(rf"\bfn {re.escape(fn)}\b", source.read_text(encoding="utf-8")):
                bad.append(f"{page.relative_to(ROOT)}: `{file}::{fn}`: no such test")
    return bad, sum(len(CITED.findall(p.read_text(encoding="utf-8"))) for p in pages)


def main():
    failed = False
    stale = check_stale()
    if stale:
        failed = True
        print("❌ 0.7-only names in the docs (mark a deliberate mention `stale-ok`):", file=sys.stderr)
        print("\n".join(f"  {hit}" for hit in stale), file=sys.stderr)
    links = check_links()
    if links:
        failed = True
        print("❌ docs.rs links that name no public item (tests/public-api.txt):", file=sys.stderr)
        print("\n".join(f"  {hit}" for hit in links), file=sys.stderr)
    gobd, cited = check_gobd()
    if gobd:
        failed = True
        print("❌ GoBD matrix evidence that does not exist:", file=sys.stderr)
        print("\n".join(f"  {hit}" for hit in gobd), file=sys.stderr)
    if failed:
        sys.exit(1)
    print(f"🧹 docs: no stale names ({len(STALE)} checked), docs.rs links resolve, {cited} GoBD citations exist")


if __name__ == "__main__":
    main()
