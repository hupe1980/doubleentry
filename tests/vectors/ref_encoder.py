#!/usr/bin/env python3
"""Independent reference encoder for the doubleentry/1 record format.

Standard library only; run with `python3 -I tests/vectors/ref_encoder.py`.
It shares no code with the Rust crate: it re-implements the byte layouts of
the canonical encoding (fixed-width little-endian integers, u64 length and
count prefixes, 0x00/0x01 options), the tagged hashes, RFC 6962 hashing and
the seal balance-tree leaf from their written definitions, and prints one
`name = hex` line per known-answer vector. `tests/vectors/golden.txt` is this
script's output; `tests/golden.rs` checks the crate against that file.
"""

import datetime
import hashlib
import struct

# ── primitives ────────────────────────────────────────────────────────────────


def u8(v):
    return struct.pack("<B", v)


def u16(v):
    return struct.pack("<H", v)


def u32(v):
    return struct.pack("<I", v)


def u64(v):
    return struct.pack("<Q", v)


def i8(v):
    return struct.pack("<b", v)


def i16(v):
    return struct.pack("<h", v)


def i32(v):
    return struct.pack("<i", v)


def i64(v):
    return struct.pack("<q", v)


def i128(v):
    return (v % (1 << 128)).to_bytes(16, "little")


def text(s):
    b = s.encode("utf-8")
    return u64(len(b)) + b


def opt(v, enc):
    return b"\x00" if v is None else b"\x01" + enc(v)


def seq(items, enc):
    return u64(len(items)) + b"".join(enc(x) for x in items)


def date(iso):
    d = datetime.date.fromisoformat(iso)
    return i32(d.toordinal() + 1721425)  # proleptic Gregorian ordinal -> Julian day number


def sha256(b):
    return hashlib.sha256(b).digest()


def tagged(tag, payload):
    t = tag.encode("ascii")
    return sha256(u64(len(t)) + t + payload)


# ── RFC 6962 ──────────────────────────────────────────────────────────────────


def leaf_hash(b):
    return sha256(b"\x00" + b)


def node_hash(left, right):
    return sha256(b"\x01" + left + right)


def split(n):
    k = 1
    while k * 2 < n:
        k *= 2
    return k


def mth(leaves):
    """Merkle Tree Hash over leaf hashes (RFC 6962 section 2.1)."""
    if not leaves:
        return sha256(b"")
    if len(leaves) == 1:
        return leaves[0]
    k = split(len(leaves))
    return node_hash(mth(leaves[:k]), mth(leaves[k:]))


def path(m, leaves):
    if len(leaves) <= 1:
        return []
    k = split(len(leaves))
    if m < k:
        return path(m, leaves[:k]) + [mth(leaves[k:])]
    return path(m - k, leaves[k:]) + [mth(leaves[:k])]


def subproof(m, leaves, complete):
    n = len(leaves)
    if m == n:
        return [] if complete else [mth(leaves)]
    k = split(n)
    if m <= k:
        return subproof(m, leaves[:k], complete) + [mth(leaves[k:])]
    return subproof(m - k, leaves[k:], False) + [mth(leaves[:k])]


# ── values ────────────────────────────────────────────────────────────────────

SIDE = {"debit": 0, "credit": 1}
LAYER = {"settled": 0, "pending": 1}
KIND = {"asset": 0, "liability": 1, "equity": 2, "income": 3, "expense": 4}
LIMIT = {"unlimited": 0, "no-credit-balance": 1, "no-debit-balance": 2}
FORMAT = "doubleentry/1"
LEDGER = "example.com/acme"


def entry_id(ledger, key):
    return tagged("doubleentry/1/entry-id", text(ledger) + text(key))[:16]


def memo_commitment(salt, memo):
    return tagged("doubleentry/1/memo", salt + text(memo))


def dims(d):
    return seq(sorted(d.items()), lambda kv: text(kv[0]) + text(kv[1]))


def document(d):
    return (
        text(d["type"])
        + text(d["reference"])
        + opt(d.get("date"), date)
        + opt(d.get("sha256"), lambda b: b)
    )


def rate(r):
    return text(r[0]) + text(r[1]) + i128(r[2]) + i8(r[3])


def hold(h):
    tag = h[0]
    if tag == "open":
        return u8(0) + opt(h[1], date)
    if tag == "post":
        amount = b"\x00" if h[2] is None else b"\x01" + i128(h[2])
        return u8(1) + h[1] + amount
    if tag == "void":
        return u8(2) + h[1]
    return u8(3) + h[1]


def balance(b):
    return i128(b[0]) + i128(b[1]) + u64(b[2])


def posting_ref(r):
    return r[0] + u16(r[1])


# ── records ───────────────────────────────────────────────────────────────────


def genesis(ledger, precision):
    return u8(1) + text(ledger) + u8(precision) + text(FORMAT)


def register(handle, account, kind, cls, open_from, open_to, limit, effective, actor):
    body = (
        u32(handle)
        + text(account)
        + u8(KIND[kind])
        + opt(cls, text)
        + date(open_from)
        + opt(open_to, date)
        + u8(LIMIT[limit])
    )
    return u8(2) + u8(0) + body + date(effective) + text(actor)


def change(handle, field, value, effective, actor):
    if field == "open_to":
        v = u8(0) + opt(value, date)
    elif field == "limit":
        v = u8(1) + u8(LIMIT[value])
    else:
        v = u8(2) + opt(value, text)
    return u8(2) + u8(1) + u32(handle) + v + date(effective) + text(actor)


def currency(code, exponent, effective, actor):
    return u8(3) + text(code) + u8(exponent) + date(effective) + text(actor)


def policy(required, drift, effective, actor):
    return u8(4) + seq(sorted(required), text) + opt(drift, u32) + date(effective) + text(actor)


def period(pid, start, end):
    return u8(5) + text(pid) + date(start) + date(end)


def entry_body(e, account, memo):
    return (
        text(e["key"])
        + date(e["booking"])
        + date(e.get("value", e["booking"]))
        + opt(e.get("captured"), lambda c: i64(c[0]) + i16(c[1]))
        + opt(e.get("kind"), text)
        + opt(e.get("memo"), memo)
        + seq(e.get("documents", []), document)
        + seq(
            e["postings"],
            lambda p: account(p["account"])
            + u8(SIDE[p["side"]])
            + i128(p["amount"])
            + text(p["currency"])
            + u8(LAYER[p.get("layer", "settled")])
            + dims(p.get("dims", {})),
        )
        + opt(e.get("reverses"), lambda b: b)
        + opt(e.get("hold"), hold)
        + seq(sorted(e.get("rates", []), key=lambda r: (r[0], r[1])), rate)
    )


def entry(e):
    return u8(6) + entry_body(e, u32, lambda m: m)


def fingerprint(draft):
    def account(a):
        return u8(1) + u32(a) if isinstance(a, int) else u8(0) + text(a)

    return tagged("doubleentry/1/fingerprint", entry_body(draft, account, text))


def clearing_apply(key, debit, credit, amount, when):
    return u8(7) + u8(0) + text(key) + posting_ref(debit) + posting_ref(credit) + i128(amount) + date(when)


def clearing_reset(key, position, when):
    return u8(7) + u8(1) + text(key) + u64(position) + date(when)


def seal(pid, start, end, through, accounts, trees):
    def head(t):
        return text(t[0]) + u8(LAYER[t[1]]) + t[2] + balance(t[3])

    return (
        u8(8)
        + text(pid)
        + date(start)
        + date(end)
        + u64(through)
        + u32(accounts)
        + seq(sorted(trees, key=lambda t: (t[0], LAYER[t[1]])), head)
    )


# ── balance trees ─────────────────────────────────────────────────────────────


def balance_leaf(b):
    return u8(0x81) + balance(b)


def balance_tree_root(accounts, nonzero):
    leaves = [leaf_hash(balance_leaf(nonzero.get(h, (0, 0, 0)))) for h in range(accounts)]
    return mth(leaves)


# ── reference instances ───────────────────────────────────────────────────────

SALT = bytes(range(16))
MEMO = "Rechnung 4711"
HOLD_1 = entry_id(LEDGER, "hold-2026-0001")
REVERSED = entry_id(LEDGER, "invoice-2026-0000")
DOCUMENTS = [
    {"type": "invoice", "reference": "RE-2026-0001", "date": "2026-03-15", "sha256": bytes([0x11]) * 32},
    {"type": "receipt", "reference": "BON-77"},
]
RATES = [("USD", "CHF", 9, -1), ("EUR", "USD", 10873, -4)]
FULL = {
    "key": "invoice-2026-0001",
    "booking": "2026-03-15",
    "value": "2026-03-17",
    "captured": (1773568800, 60),
    "kind": "invoice",
    "memo": memo_commitment(SALT, MEMO),
    "documents": DOCUMENTS,
    "postings": [
        {"account": 0, "side": "debit", "amount": 119000, "currency": "EUR",
         "dims": {"project": "alpha", "cost-center": "100"}},
        {"account": 1, "side": "credit", "amount": 119000, "currency": "EUR"},
        {"account": 2, "side": "debit", "amount": 5000, "currency": "USD", "layer": "pending",
         "dims": {"project": "beta"}},
    ],
    "reverses": REVERSED,
    "hold": ("post", HOLD_1, 2500),
    "rates": RATES,
}
MINIMAL = {
    "key": "minimal-1",
    "booking": "2026-03-16",
    "postings": [
        {"account": 0, "side": "debit", "amount": 100, "currency": "EUR"},
        {"account": 1, "side": "credit", "amount": 100, "currency": "EUR"},
    ],
}
FULL_DRAFT = {
    "key": "invoice-2026-0001",
    "booking": "2026-03-15",
    "value": "2026-03-17",
    "captured": (1773568800, 60),
    "kind": "invoice",
    "memo": MEMO,
    "documents": DOCUMENTS,
    "postings": [
        {"account": "Assets:Cash", "side": "debit", "amount": 119000, "currency": "EUR",
         "dims": {"project": "alpha", "cost-center": "100"}},
        {"account": 1, "side": "credit", "amount": 119000, "currency": "EUR"},
    ],
    "reverses": REVERSED,
    "rates": RATES,
}
EUR_SETTLED = {0: (119000, 0, 1), 2: (0, 119000, 1)}
USD_PENDING = {2: (5000, 0, 1)}


def hold_entry(key, booking, action, postings=()):
    return {"key": key, "booking": booking, "hold": action, "postings": list(postings)}


LOG = [
    ("genesis", genesis(LEDGER, 2)),
    ("account_register", register(0, "Assets:Cash", "asset", None, "2026-01-01", None, "unlimited",
                                  "2026-01-01", "setup")),
    ("account_change_open_to", change(0, "open_to", "2026-12-31", "2026-02-01", "setup")),
    ("account_change_limit", change(0, "limit", "no-credit-balance", "2026-02-01", "setup")),
    ("account_change_class", change(0, "class", "current", "2026-02-01", "setup")),
    ("currency", currency("EUR", 2, "2026-01-01", "setup")),
    ("policy", policy(["project", "cost-center"], 30, "2026-01-01", "setup")),
    ("period", period("2026-03", "2026-03-01", "2026-03-31")),
    ("entry_full", entry(FULL)),
    ("entry_minimal", entry(MINIMAL)),
    ("hold_open", entry(hold_entry("hold-2026-0001", "2026-03-10", ("open", "2026-03-31"), [
        {"account": 0, "side": "debit", "amount": 5000, "currency": "EUR", "layer": "pending"},
        {"account": 1, "side": "credit", "amount": 5000, "currency": "EUR", "layer": "pending"},
    ]))),
    ("hold_post", entry(hold_entry("hold-2026-0001-post", "2026-03-12", ("post", HOLD_1, None)))),
    ("hold_void", entry(hold_entry("hold-2026-0002-void", "2026-03-12",
                                   ("void", entry_id(LEDGER, "hold-2026-0002"))))),
    ("hold_expire", entry(hold_entry("hold-2026-0003-expire", "2026-04-01",
                                     ("expire", entry_id(LEDGER, "hold-2026-0003"))))),
    ("clearing_apply", clearing_apply("clear-1", (entry_id(LEDGER, "invoice-2026-0001"), 0),
                                      (entry_id(LEDGER, "minimal-1"), 1), 100, "2026-03-20")),
    ("clearing_reset", clearing_reset("clear-1-reset", 14, "2026-03-21")),
    ("seal", seal("2026-03", "2026-03-01", "2026-03-31", 15, 3, [
        ("USD", "pending", balance_tree_root(3, USD_PENDING), (5000, 0, 1)),
        ("EUR", "settled", balance_tree_root(3, EUR_SETTLED), (119000, 119000, 2)),
    ])),
]

RFC6962_LEAVES = [bytes.fromhex(h) for h in [
    "", "00", "10", "2021", "3031", "40414243", "5051525354555657", "606162636465666768696a6b6c6d6e6f",
]]


def main():
    out = []
    for name, leaf in LOG:
        out.append(f"{name}.leaf = {leaf.hex()}")
        out.append(f"{name}.hash = {leaf_hash(leaf).hex()}")
    hashes = [leaf_hash(leaf) for _, leaf in LOG]
    out.append(f"log.root = {mth(hashes).hex()}")
    out.append(f"log.root_genesis_currency = {mth([hashes[0], hashes[5]]).hex()}")
    out.append(f"entry_id = {entry_id(LEDGER, 'invoice-2026-0001').hex()}")
    out.append(f"memo_commitment = {memo_commitment(SALT, MEMO).hex()}")
    out.append(f"fingerprint_full_draft = {fingerprint(FULL_DRAFT).hex()}")
    zero = leaf_hash(balance_leaf((0, 0, 0)))
    z1 = node_hash(zero, zero)
    out.append(f"balance.zero_leaf = {zero.hex()}")
    out.append(f"balance.z1 = {z1.hex()}")
    out.append(f"balance.z2 = {node_hash(z1, z1).hex()}")
    out.append(f"balance.root_3 = {balance_tree_root(3, EUR_SETTLED).hex()}")
    out.append(f"balance.root_usd_pending = {balance_tree_root(3, USD_PENDING).hex()}")
    rfc = [leaf_hash(b) for b in RFC6962_LEAVES]
    for n in range(9):
        out.append(f"rfc6962.root.{n} = {mth(rfc[:n]).hex()}")
    for m, n in [(0, 8), (5, 8), (2, 3), (1, 5)]:
        out.append(f"rfc6962.inclusion.{m}.{n} = {','.join(h.hex() for h in path(m, rfc[:n]))}")
    for m, n in [(1, 8), (6, 8), (2, 5)]:
        out.append(f"rfc6962.consistency.{m}.{n} = {','.join(h.hex() for h in subproof(m, rfc[:n], True))}")
    print("\n".join(out))


if __name__ == "__main__":
    main()
