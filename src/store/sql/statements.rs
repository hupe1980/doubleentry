//! The statement catalogue: every statement written once with
//! `$n` placeholders and rendered once per store with its schema qualifier.
//! Only bulk inserts and list lookups are rendered by the dialect.

use std::sync::{Arc, OnceLock};

use super::dialect::{Batch, Kind, Statement};
use crate::store::projection::{Column, Ty};
use crate::store::{PostingOrder, PostingRead, ProjectionPart};

impl Statement {
    /// `template` with `{{s}}` replaced by `qualifier`.
    pub(crate) fn render(template: &str, qualifier: &str, columns: &'static [Kind]) -> Self {
        Self {
            text: Arc::from(template.replace("{{s}}", qualifier)),
            columns,
        }
    }
}

/// Declares the shared statements: name, returned column types, SQL.
macro_rules! catalogue {
    ($( $(#[doc = $doc:literal])* $name:ident: [$($kind:ident),*] = $sql:literal; )*) => {
        /// The shared statements, rendered for one store.
        #[derive(Debug)]
        pub(crate) struct Shared {
            $( $(#[doc = $doc])* pub(crate) $name: Statement, )*
        }

        impl Shared {
            fn render(qualifier: &str) -> Self {
                Self { $( $name: Statement::render($sql, qualifier, &[$(Kind::$kind),*]), )* }
            }
        }
    };
}

catalogue! {
    // ── Lifecycle and header ─────────────────────────────────────────────────
    /// The meta row.
    meta: [Text, Text, Int, Bytes] =
        "SELECT schema_id, ledger_id, amount_scale, genesis FROM {{s}}meta WHERE singleton = 1";
    /// Inserts the meta row of a new ledger.
    insert_meta: [] =
        "INSERT INTO {{s}}meta (singleton, schema_id, ledger_id, amount_scale, genesis) VALUES (1, $1, $2, $3, $4)";
    /// The write header in one round trip: size, account count, entry count,
    /// the last day of the last sealed period, the latest booking date
    /// (`postings_by_day`), the last period with its seal, and the meta row's
    /// schema id, ledger id and scale (re-checked on every write).
    header: [Int, Int, Int, Int, Int, Text, Int, Int, Int, Int, Int, Bytes, Text, Text, Int] =
        "SELECT (SELECT COALESCE(MAX(position) + 1, 0) FROM {{s}}leaves), \
         (SELECT COALESCE(MAX(handle) + 1, 0) FROM {{s}}accounts), \
         (SELECT COALESCE(MAX(ordinal) + 1, 0) FROM {{s}}entries), \
         (SELECT p.last_day FROM {{s}}seals s JOIN {{s}}periods p ON p.period_id = s.period_id \
         ORDER BY s.position DESC LIMIT 1), \
         (SELECT MAX(booking) FROM {{s}}postings), \
         lp.period_id, lp.first_day, lp.last_day, lp.position, lp.sealed_at, lp.kind, lp.bytes, \
         (SELECT schema_id FROM {{s}}meta WHERE singleton = 1), \
         (SELECT ledger_id FROM {{s}}meta WHERE singleton = 1), \
         (SELECT amount_scale FROM {{s}}meta WHERE singleton = 1) \
         FROM (SELECT 1 AS one) o LEFT JOIN (SELECT p.period_id, p.first_day, p.last_day, \
         p.position, s.position AS sealed_at, l.kind, l.bytes FROM {{s}}periods p \
         LEFT JOIN {{s}}seals s ON s.period_id = p.period_id \
         LEFT JOIN {{s}}leaves l ON l.position = s.position \
         ORDER BY p.first_day DESC LIMIT 1) lp ON 1 = 1";
    /// The bytes of one leaf.
    leaf_bytes: [Bytes] = "SELECT bytes FROM {{s}}leaves WHERE position = $1";
    /// The size.
    size: [Int] = "SELECT COALESCE(MAX(position) + 1, 0) FROM {{s}}leaves";

    // ── Facts that take no key ───────────────────────────────────────────────
    /// `PolicyLatest`.
    policy_latest: [Int, Int, Bytes] =
        "SELECT v.position, l.kind, l.bytes FROM {{s}}policy_versions v JOIN {{s}}leaves l \
         ON l.position = v.position ORDER BY v.position DESC LIMIT 1";
    /// `FirstUnsealed`: the earliest period without a seal.
    first_unsealed: [Text, Int, Int, Int, Int, Int, Bytes] =
        "SELECT p.period_id, p.first_day, p.last_day, p.position, s.position, l.kind, l.bytes \
         FROM {{s}}periods p LEFT JOIN {{s}}seals s ON s.period_id = p.period_id \
         LEFT JOIN {{s}}leaves l ON l.position = s.position \
         WHERE s.position IS NULL ORDER BY p.first_day LIMIT 1";
    /// `AllBalances` (sealing).
    all_balances: [Int, Text, Int, Bytes, Bytes, Int] =
        "SELECT handle, currency, layer, debit, credit, postings FROM {{s}}balances \
         ORDER BY handle, currency, layer";
    /// `PostingsAfter` (sealing): `postings_by_day`.
    postings_after: [Int, Text, Int, Int, Bytes] =
        "SELECT handle, currency, layer, side, amount FROM {{s}}postings WHERE booking > $1 \
         ORDER BY booking, position, idx";
    /// `PnlSlices`, first half: the maintained slices of income and expense
    /// accounts.
    pnl_slices: [Int, Text, Text, Bytes, Bytes, Int] =
        "SELECT s.handle, s.currency, s.dimensions, s.debit, s.credit, s.postings \
         FROM {{s}}slices s JOIN {{s}}accounts a ON a.handle = s.handle WHERE a.kind IN (3, 4)";
    /// `PnlSlices`, second half: their Settled postings booked after a date.
    pnl_after: [Int, Text, Text, Int, Bytes] =
        "SELECT p.handle, p.currency, p.dimensions, p.side, p.amount FROM {{s}}postings p \
         JOIN {{s}}accounts a ON a.handle = p.handle \
         WHERE p.booking > $1 AND p.layer = 0 AND a.kind IN (3, 4)";

    // ── Persist ──────────────────────────────────────────────────────────────
    /// The leaf.
    insert_leaf: [] = "INSERT INTO {{s}}leaves (position, kind, bytes) VALUES ($1, $2, $3)";
    /// `IndexRow::EntryKey` (+ `Reversal`).
    insert_entry: [] =
        "INSERT INTO {{s}}entries (position, ordinal, entry_id, idem_key, fingerprint, reverses) \
         VALUES ($1, $2, $3, $4, $5, $6)";
    /// `IndexRow::Account`.
    insert_account: [] =
        "INSERT INTO {{s}}accounts (handle, path, kind, position) VALUES ($1, $2, $3, $4)";
    /// `IndexRow::CurrencyVersion`.
    insert_currency: [] =
        "INSERT INTO {{s}}currency_versions (code, exponent, effective, position) VALUES ($1, $2, $3, $4)";
    /// `IndexRow::PolicyVersion`.
    insert_policy: [] =
        "INSERT INTO {{s}}policy_versions (position, effective) VALUES ($1, $2)";
    /// `IndexRow::Period`.
    insert_period: [] =
        "INSERT INTO {{s}}periods (period_id, first_day, last_day, position) VALUES ($1, $2, $3, $4)";
    /// `IndexRow::Sealed`.
    insert_seal: [] = "INSERT INTO {{s}}seals (period_id, position) VALUES ($1, $2)";
    /// `IndexRow::ClearingKey`.
    insert_clearing: [] = "INSERT INTO {{s}}clearings (position, idem_key) VALUES ($1, $2)";
    /// `IndexRow::ClearingReset`.
    insert_reset: [] = "INSERT INTO {{s}}clearing_resets (clearing, position) VALUES ($1, $2)";
    /// `IndexRow::Applied`: the keys a clearing touches.
    insert_clearing_ref: [] =
        "INSERT INTO {{s}}clearing_refs (position, handle, currency, layer) \
         SELECT $1, p.handle, p.currency, p.layer FROM {{s}}postings p \
         JOIN {{s}}entries e ON e.position = p.position WHERE e.entry_id = $2 AND p.idx = $3 \
         ON CONFLICT DO NOTHING";
    /// The memo.
    insert_memo: [] = "INSERT INTO {{s}}memos (position, salt, text) VALUES ($1, $2, $3)";
    /// A hold opened.
    insert_hold: [] =
        "INSERT INTO {{s}}holds (position, entry_id, booking, expires, debit_handle, \
         credit_handle, currency, amount) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)";
    /// A hold resolved: the only update of a hold row (resolve once).
    resolve_hold: [] =
        "UPDATE {{s}}holds SET resolved_by = $2, how = $3 WHERE entry_id = $1 AND resolved_by IS NULL";

    // ── Reads ────────────────────────────────────────────────────────────────
    /// `read_leaves`.
    leaves_from: [Int, Int, Bytes] =
        "SELECT position, kind, bytes FROM {{s}}leaves WHERE position >= $1 AND position < $2 \
         ORDER BY position LIMIT $3";
    /// `read_leaves` of one kind (`leaves_by_kind`).
    leaves_of_kind: [Int, Int, Bytes] =
        "SELECT position, kind, bytes FROM {{s}}leaves WHERE kind = $1 AND position >= $2 \
         AND position < $3 ORDER BY position LIMIT $4";
    /// `seals`.
    seals: [Text, Int, Int, Bytes] =
        "SELECT s.period_id, s.position, l.kind, l.bytes FROM {{s}}seals s \
         JOIN {{s}}leaves l ON l.position = s.position WHERE s.position < $1 ORDER BY s.position";
    /// `account_records`.
    account_records: [Int, Int, Bytes] =
        "SELECT DISTINCT v.position, l.kind, l.bytes FROM {{s}}account_versions v \
         JOIN {{s}}leaves l ON l.position = v.position \
         WHERE v.handle = $1 AND v.position > $2 AND v.position < $3 ORDER BY v.position LIMIT $4";
    /// `entry` by id.
    stored_entry_by_id: [Int, Bytes, Bytes, Bytes, Int, Bytes] =
        "SELECT e.position, e.entry_id, e.idem_key, e.fingerprint, l.kind, l.bytes \
         FROM {{s}}entries e JOIN {{s}}leaves l ON l.position = e.position \
         WHERE e.entry_id = $1 AND e.position < $2";
    /// `entry` by key.
    stored_entry_by_key: [Int, Bytes, Bytes, Bytes, Int, Bytes] =
        "SELECT e.position, e.entry_id, e.idem_key, e.fingerprint, l.kind, l.bytes \
         FROM {{s}}entries e JOIN {{s}}leaves l ON l.position = e.position \
         WHERE e.idem_key = $1 AND e.position < $2";
    /// `holds` by id.
    hold_by_id: [Int, Bytes, Int, Int, Int, Int, Text, Bytes, Bytes, Int] =
        "SELECT position, entry_id, booking, expires, debit_handle, credit_handle, currency, \
         amount, resolved_by, how FROM {{s}}holds WHERE entry_id = $1 AND position < $2";
    /// `holds` open by account (`holds_open_debit` ∪ `holds_open_credit`):
    /// each side paged on its partial index before the two pages are merged,
    /// so a page costs its size, not the account's open holds.
    holds_open_by_account: [Int, Bytes, Int, Int, Int, Int, Text, Bytes, Bytes, Int] =
        "SELECT * FROM (SELECT position, entry_id, booking, expires, debit_handle, \
         credit_handle, currency, amount, resolved_by, how FROM {{s}}holds \
         WHERE debit_handle = $1 AND resolved_by IS NULL AND position > $2 AND position < $3 \
         ORDER BY position LIMIT $4) d \
         UNION SELECT * FROM (SELECT position, entry_id, booking, expires, debit_handle, \
         credit_handle, currency, amount, resolved_by, how FROM {{s}}holds \
         WHERE credit_handle = $1 AND resolved_by IS NULL AND position > $2 AND position < $3 \
         ORDER BY position LIMIT $4) c ORDER BY position LIMIT $4";
    /// `holds` due by a date (`holds_open_due`).
    holds_due: [Int, Bytes, Int, Int, Int, Int, Text, Bytes, Bytes, Int] =
        "SELECT position, entry_id, booking, expires, debit_handle, credit_handle, currency, \
         amount, resolved_by, how FROM {{s}}holds WHERE resolved_by IS NULL AND expires IS NOT NULL \
         AND expires <= $1 AND (expires, position) > ($2, $3) AND position < $4 \
         ORDER BY expires, position LIMIT $5";
    /// `clearings`.
    clearings_of_key: [Int, Int, Bytes] =
        "SELECT r.position, l.kind, l.bytes FROM {{s}}clearing_refs r \
         JOIN {{s}}leaves l ON l.position = r.position WHERE r.handle = $1 AND r.currency = $2 \
         AND r.layer = $3 AND r.position > $4 AND r.position < $5 ORDER BY r.position LIMIT $6";
    /// `balances` `Current`, all keys.
    balances_after_key: [Int, Text, Int, Bytes, Bytes, Int] =
        "SELECT handle, currency, layer, debit, credit, postings FROM {{s}}balances \
         WHERE (handle, currency, layer) > ($1, $2, $3) ORDER BY handle, currency, layer LIMIT $4";
    /// `balances` `At`, every key, one page in one statement: each balance key
    /// after the cursor with its last `balance_after` below a position (a
    /// key without postings there drops out of the join).
    balances_at_after_key: [Int, Text, Int, Bytes, Bytes, Int] =
        "SELECT b.handle, b.currency, b.layer, p.debit_after, p.credit_after, p.count_after \
         FROM {{s}}balances b JOIN {{s}}postings p ON p.handle = b.handle \
         AND p.currency = b.currency AND p.layer = b.layer AND p.position = \
         (SELECT MAX(x.position) FROM {{s}}postings x WHERE x.handle = b.handle \
         AND x.currency = b.currency AND x.layer = b.layer AND x.position < $4) \
         AND p.idx = (SELECT MAX(y.idx) FROM {{s}}postings y WHERE y.position = p.position \
         AND y.handle = b.handle AND y.currency = b.currency AND y.layer = b.layer) \
         WHERE (b.handle, b.currency, b.layer) > ($1, $2, $3) \
         ORDER BY b.handle, b.currency, b.layer LIMIT $5";
    /// `balances` `Sealed`, all keys.
    sealed_balances: [Text, Int, Int, Bytes, Bytes, Int] =
        "SELECT currency, layer, handle, debit, credit, postings FROM {{s}}seal_leaves \
         WHERE period_id = $1 AND (currency, layer, handle) > ($2, $3, $4) \
         ORDER BY currency, layer, handle LIMIT $5";
    /// `memo`.
    memo: [Int, Bytes, Text] =
        "SELECT CAST(CASE WHEN m.position IS NULL THEN 0 ELSE 1 END AS BIGINT), m.salt, m.text \
         FROM {{s}}entries e LEFT JOIN {{s}}memos m ON m.position = e.position WHERE e.entry_id = $1";
    /// Erases the memo.
    erase_memo: [] =
        "UPDATE {{s}}memos SET salt = NULL, text = NULL \
         WHERE position = (SELECT position FROM {{s}}entries WHERE entry_id = $1) AND text IS NOT NULL";
    /// Erases the draft's fingerprint (after the memo).
    erase_fingerprint: [] =
        "UPDATE {{s}}entries SET fingerprint = NULL WHERE entry_id = $1 AND fingerprint IS NOT NULL";
}

/// Declares the batched lookups: name, key column types, returned column
/// types, SQL. Each binds its keys as `k (i, …)` — `i` the key's index,
/// prepended to both lists — and selects `k.i` first.
macro_rules! keyed {
    ($( $(#[doc = $doc:literal])* $name:ident: ($($key:ident),*) -> [$($kind:ident),*] = $sql:expr; )*) => {
        /// The batched lookups, rendered for one store.
        #[derive(Debug)]
        pub(crate) struct Keyed {
            $( $(#[doc = $doc])* pub(crate) $name: Batch, )*
        }

        impl Keyed {
            fn render(qualifier: &str) -> Self {
                Self { $( $name: Batch {
                    template: Arc::from($sql.replace("{{s}}", qualifier)),
                    kinds: vec![Kind::Int, $(Kind::$key),*],
                    columns: &[Kind::Int, $(Kind::$kind),*],
                }, )* }
            }
        }
    };
}

/// An account with the latest version of each of its fields (window,
/// limit, class): registration leaf, has-descendants, last booking date,
/// then (effective, position, value…) per field.
macro_rules! account_select {
    () => {
        "SELECT k.i, a.handle, l.kind, l.bytes, \
         CAST(CASE WHEN EXISTS (SELECT 1 FROM {{s}}accounts d WHERE d.path >= (a.path || ':') \
         AND d.path < (a.path || ';')) THEN 1 ELSE 0 END AS BIGINT), \
         (SELECT MAX(booked.last) FROM (SELECT (SELECT MAX(p.booking) FROM {{s}}postings p \
         WHERE p.handle = b.handle AND p.currency = b.currency AND p.layer = b.layer) AS last \
         FROM {{s}}balances b WHERE b.handle = a.handle) booked), \
         w.effective, w.position, w.open_from, w.open_to, \
         m.effective, m.position, m.limit_code, \
         c.effective, c.position, c.class FROM k "
    };
}

/// The joins of [`account_select`] after the account and its leaf.
macro_rules! account_versions {
    () => {
        " JOIN {{s}}leaves l ON l.position = a.position \
         LEFT JOIN {{s}}account_versions w ON w.handle = a.handle AND w.field = 0 AND w.position = \
         (SELECT MAX(x.position) FROM {{s}}account_versions x WHERE x.handle = a.handle AND x.field = 0) \
         LEFT JOIN {{s}}account_versions m ON m.handle = a.handle AND m.field = 1 AND m.position = \
         (SELECT MAX(x.position) FROM {{s}}account_versions x WHERE x.handle = a.handle AND x.field = 1) \
         LEFT JOIN {{s}}account_versions c ON c.handle = a.handle AND c.field = 2 AND c.position = \
         (SELECT MAX(x.position) FROM {{s}}account_versions x WHERE x.handle = a.handle AND x.field = 2)"
    };
}

/// A period with its seal, joined to `k` on `$on`.
macro_rules! period_select {
    ($on:literal) => {
        concat!(
            "SELECT k.i, p.period_id, p.first_day, p.last_day, p.position, s.position, l.kind, \
             l.bytes FROM k JOIN {{s}}periods p ON ",
            $on,
            " LEFT JOIN {{s}}seals s ON s.period_id = p.period_id \
             LEFT JOIN {{s}}leaves l ON l.position = s.position"
        )
    };
}

keyed! {
    // ── Facts: one round trip per kind and plan round ────────────────────────
    /// `EntryByKey`.
    entry_by_key: (Bytes) -> [Int, Bytes, Bytes] =
        "WITH k (i, idem_key) AS ({{rows}}) SELECT k.i, e.position, e.entry_id, e.fingerprint \
         FROM k JOIN {{s}}entries e ON e.idem_key = k.idem_key";
    /// `AccountByPath`.
    account_by_path: (Text) -> [Int, Int, Bytes, Int, Int, Int, Int, Int, Int, Int, Int, Int, Int, Int, Text] =
        concat!("WITH k (i, path) AS ({{rows}}) ", account_select!(),
            "JOIN {{s}}accounts a ON a.path = k.path", account_versions!());
    /// `Account`.
    account_by_handle: (Int) -> [Int, Int, Bytes, Int, Int, Int, Int, Int, Int, Int, Int, Int, Int, Int, Text] =
        concat!("WITH k (i, handle) AS ({{rows}}) ", account_select!(),
            "JOIN {{s}}accounts a ON a.handle = k.handle", account_versions!());
    /// `AccountTerms`: the version of each field in force on a date.
    account_terms: (Int, Int) -> [Int, Int, Int, Int, Text] =
        "WITH k (i, handle, d) AS ({{rows}}) \
         SELECT k.i, v.field, v.open_from, v.open_to, v.limit_code, v.class FROM k \
         JOIN {{s}}account_versions v ON v.handle = k.handle AND v.position = \
         (SELECT w.position FROM {{s}}account_versions w WHERE w.handle = k.handle \
         AND w.field = v.field AND w.effective <= k.d ORDER BY w.effective DESC, w.position DESC LIMIT 1)";
    /// `Currency`: the exponent in force on a date.
    currency_at: (Text, Int) -> [Int] =
        "WITH k (i, code, d) AS ({{rows}}) SELECT k.i, c.exponent FROM k \
         JOIN {{s}}currency_versions c ON c.code = k.code AND c.position = \
         (SELECT x.position FROM {{s}}currency_versions x WHERE x.code = k.code AND x.effective <= k.d \
         ORDER BY x.effective DESC, x.position DESC LIMIT 1)";
    /// `CurrencyLatest`.
    currency_latest: (Text) -> [Int, Int, Int] =
        "WITH k (i, code) AS ({{rows}}) SELECT k.i, c.exponent, c.effective, c.position FROM k \
         JOIN {{s}}currency_versions c ON c.code = k.code AND c.position = \
         (SELECT MAX(x.position) FROM {{s}}currency_versions x WHERE x.code = k.code)";
    /// `Policy`: the policy record in force on a date.
    policy_at: (Int) -> [Int, Int, Bytes] =
        "WITH k (i, d) AS ({{rows}}) SELECT k.i, v.position, l.kind, l.bytes FROM k \
         JOIN {{s}}policy_versions v ON v.position = (SELECT x.position FROM {{s}}policy_versions x \
         WHERE x.effective <= k.d ORDER BY x.effective DESC, x.position DESC LIMIT 1) \
         JOIN {{s}}leaves l ON l.position = v.position";
    /// `PeriodAt`: the last period starting on or before a date, with its seal.
    period_at: (Int) -> [Text, Int, Int, Int, Int, Int, Bytes] =
        concat!("WITH k (i, d) AS ({{rows}}) ", period_select!(
            "p.first_day = (SELECT MAX(x.first_day) FROM {{s}}periods x WHERE x.first_day <= k.d)"));
    /// `Period`: a period by id, with its seal.
    period_by_id: (Text) -> [Text, Int, Int, Int, Int, Int, Bytes] =
        concat!("WITH k (i, id) AS ({{rows}}) ", period_select!("p.period_id = k.id"));
    /// `Entry`.
    entry_by_id: (Bytes) -> [Int, Int, Bytes] =
        "WITH k (i, id) AS ({{rows}}) SELECT k.i, e.position, l.kind, l.bytes FROM k \
         JOIN {{s}}entries e ON e.entry_id = k.id JOIN {{s}}leaves l ON l.position = e.position";
    /// `ReversedBy`.
    reversed_by: (Bytes) -> [Bytes] =
        "WITH k (i, id) AS ({{rows}}) SELECT k.i, e.entry_id FROM k \
         JOIN {{s}}entries e ON e.reverses = k.id";
    /// `OutstandingHoldDebits` and `OutstandingHoldCredits`.
    outstanding: (Int, Text) -> [Bytes, Bytes] =
        "WITH k (i, handle, currency) AS ({{rows}}) SELECT k.i, o.debit, o.credit FROM k \
         JOIN {{s}}outstanding o ON o.handle = k.handle AND o.currency = k.currency";
    /// `Hold`: the hold index row with the hold's leaf.
    hold: (Bytes) -> [Int, Int, Bytes, Bytes, Int] =
        "WITH k (i, id) AS ({{rows}}) SELECT k.i, h.position, l.kind, l.bytes, h.resolved_by, \
         h.how FROM k JOIN {{s}}holds h ON h.entry_id = k.id \
         JOIN {{s}}leaves l ON l.position = h.position";
    /// `Balance`.
    balance: (Int, Text, Int) -> [Bytes, Bytes, Int] =
        "WITH k (i, handle, currency, layer) AS ({{rows}}) \
         SELECT k.i, b.debit, b.credit, b.postings FROM k JOIN {{s}}balances b \
         ON b.handle = k.handle AND b.currency = k.currency AND b.layer = k.layer";
    /// `ClearingByKey`.
    clearing_by_key: (Bytes) -> [Int, Int, Bytes, Int] =
        "WITH k (i, idem_key) AS ({{rows}}) SELECT k.i, c.position, l.kind, l.bytes, r.position \
         FROM k JOIN {{s}}clearings c ON c.idem_key = k.idem_key \
         JOIN {{s}}leaves l ON l.position = c.position \
         LEFT JOIN {{s}}clearing_resets r ON r.clearing = c.position";
    /// `ClearingAt`.
    clearing_at: (Int) -> [Int, Int, Bytes, Int] =
        "WITH k (i, position) AS ({{rows}}) SELECT k.i, c.position, l.kind, l.bytes, r.position \
         FROM k JOIN {{s}}clearings c ON c.position = k.position \
         JOIN {{s}}leaves l ON l.position = c.position \
         LEFT JOIN {{s}}clearing_resets r ON r.clearing = c.position";
    /// `Applied`.
    applied: (Bytes, Int) -> [Bytes] =
        "WITH k (i, entry_id, idx) AS ({{rows}}) SELECT k.i, c.applied FROM k \
         JOIN {{s}}entries e ON e.entry_id = k.entry_id \
         JOIN {{s}}cleared c ON c.position = e.position AND c.idx = k.idx";

    // ── Persist and reads ────────────────────────────────────────────────────
    /// Stored nodes by id (`read_nodes` and the append).
    nodes: (Int) -> [Bytes] =
        "WITH k (i, id) AS ({{rows}}) SELECT k.i, n.hash FROM k JOIN {{s}}nodes n ON n.id = k.id";
    /// A cleared posting's position and the amount cleared against it.
    cleared: (Bytes, Int) -> [Int, Bytes] =
        "WITH k (i, entry_id, idx) AS ({{rows}}) SELECT k.i, e.position, c.applied FROM k \
         JOIN {{s}}entries e ON e.entry_id = k.entry_id \
         LEFT JOIN {{s}}cleared c ON c.position = e.position AND c.idx = k.idx";
    /// Maintained slices.
    slices: (Int, Text, Text) -> [Bytes, Bytes, Int] =
        "WITH k (i, handle, currency, dimensions) AS ({{rows}}) \
         SELECT k.i, s.debit, s.credit, s.postings FROM k JOIN {{s}}slices s \
         ON s.handle = k.handle AND s.currency = k.currency AND s.dimensions = k.dimensions";
    /// `accounts` by path, batched.
    account_rows_by_path: (Text) -> [Int, Text, Int, Int] =
        "WITH k (i, path) AS ({{rows}}) SELECT k.i, a.handle, a.path, a.kind, a.position \
         FROM k JOIN {{s}}accounts a ON a.path = k.path";
    /// `accounts` by handle, batched.
    account_rows_by_handle: (Int) -> [Int, Text, Int, Int] =
        "WITH k (i, handle) AS ({{rows}}) SELECT k.i, a.handle, a.path, a.kind, a.position \
         FROM k JOIN {{s}}accounts a ON a.handle = k.handle";
    /// `balances` `At`, explicit keys: each key's last `balance_after` below a
    /// position.
    balance_at: (Int, Text, Int, Int) -> [Bytes, Bytes, Int] =
        "WITH k (i, handle, currency, layer, below) AS ({{rows}}) \
         SELECT k.i, p.debit_after, p.credit_after, p.count_after FROM k JOIN {{s}}postings p \
         ON p.handle = k.handle AND p.currency = k.currency AND p.layer = k.layer \
         AND p.position = (SELECT MAX(x.position) FROM {{s}}postings x WHERE x.handle = k.handle \
         AND x.currency = k.currency AND x.layer = k.layer AND x.position < k.below) \
         AND p.idx = (SELECT MAX(y.idx) FROM {{s}}postings y WHERE y.position = p.position \
         AND y.handle = k.handle AND y.currency = k.currency AND y.layer = k.layer)";
    /// `balances` `Sealed`, explicit keys.
    sealed_balance: (Text, Text, Int, Int) -> [Bytes, Bytes, Int] =
        "WITH k (i, period_id, currency, layer, handle) AS ({{rows}}) \
         SELECT k.i, s.debit, s.credit, s.postings FROM k JOIN {{s}}seal_leaves s \
         ON s.period_id = k.period_id AND s.currency = k.currency AND s.layer = k.layer \
         AND s.handle = k.handle";
    /// `seal_nodes`: stored nodes of a seal's balance tree.
    seal_nodes: (Text, Text, Int, Int, Int) -> [Bytes] =
        "WITH k (i, period_id, currency, layer, level, idx) AS ({{rows}}) \
         SELECT k.i, n.hash FROM k JOIN {{s}}seal_nodes n ON n.period_id = k.period_id \
         AND n.currency = k.currency AND n.layer = k.layer AND n.level = k.level AND n.idx = k.idx";
}

/// The postings page's select list and joins.
const POSTINGS_PAGE: &str = "SELECT p.position, p.idx, e.entry_id, p.side, p.amount, p.booking, \
     p.value_date, p.dimensions, p.debit_after, p.credit_after, p.count_after, c.applied \
     FROM {{s}}postings p JOIN {{s}}entries e ON e.position = p.position \
     LEFT JOIN {{s}}cleared c ON c.position = p.position AND c.idx = p.idx \
     WHERE p.handle = $1 AND p.currency = $2 AND p.layer = $3 AND p.position < $4";

/// The columns of a postings page.
pub(crate) const POSTINGS_COLUMNS: &[Kind] = &[
    Kind::Int,
    Kind::Int,
    Kind::Bytes,
    Kind::Int,
    Kind::Bytes,
    Kind::Int,
    Kind::Int,
    Kind::Text,
    Kind::Bytes,
    Kind::Bytes,
    Kind::Int,
    Kind::Bytes,
];

/// The bulk inserts (`INSERT … {{rows}}`).
#[derive(Debug)]
pub(crate) struct Inserts {
    /// Tree nodes.
    pub(crate) nodes: Batch,
    /// `IndexRow::AccountVersion`.
    pub(crate) account_versions: Batch,
    /// `IndexRow::SealTree`.
    pub(crate) seal_leaves: Batch,
    /// `IndexRow::SealTree`: the tree's stored nodes.
    pub(crate) seal_nodes: Batch,
    /// Postings.
    pub(crate) postings: Batch,
    /// Posting dimensions.
    pub(crate) posting_dims: Batch,
    /// Balances (upsert).
    pub(crate) balances: Batch,
    /// Cleared amounts (upsert).
    pub(crate) cleared: Batch,
    /// Slices (upsert).
    pub(crate) slices: Batch,
    /// The amounts open holds reserve (upsert).
    pub(crate) outstanding: Batch,
}

/// The columns of a bulk insert and their types.
type Columns = &'static [(&'static str, Kind)];

const NODES: Columns = &[("id", Kind::Int), ("hash", Kind::Bytes)];
const ACCOUNT_VERSIONS: Columns = &[
    ("handle", Kind::Int),
    ("field", Kind::Int),
    ("effective", Kind::Int),
    ("position", Kind::Int),
    ("open_from", Kind::Int),
    ("open_to", Kind::Int),
    ("limit_code", Kind::Int),
    ("class", Kind::Text),
];
const SEAL_LEAVES: Columns = &[
    ("period_id", Kind::Text),
    ("currency", Kind::Text),
    ("layer", Kind::Int),
    ("handle", Kind::Int),
    ("debit", Kind::Bytes),
    ("credit", Kind::Bytes),
    ("postings", Kind::Int),
];
const SEAL_NODES: Columns = &[
    ("period_id", Kind::Text),
    ("currency", Kind::Text),
    ("layer", Kind::Int),
    ("level", Kind::Int),
    ("idx", Kind::Int),
    ("hash", Kind::Bytes),
];
const POSTINGS: Columns = &[
    ("position", Kind::Int),
    ("idx", Kind::Int),
    ("handle", Kind::Int),
    ("currency", Kind::Text),
    ("layer", Kind::Int),
    ("side", Kind::Int),
    ("amount", Kind::Bytes),
    ("booking", Kind::Int),
    ("value_date", Kind::Int),
    ("dimensions", Kind::Text),
    ("debit_after", Kind::Bytes),
    ("credit_after", Kind::Bytes),
    ("count_after", Kind::Int),
];
const POSTING_DIMS: Columns = &[
    ("position", Kind::Int),
    ("idx", Kind::Int),
    ("axis", Kind::Text),
    ("value", Kind::Text),
];
const BALANCES: Columns = &[
    ("handle", Kind::Int),
    ("currency", Kind::Text),
    ("layer", Kind::Int),
    ("debit", Kind::Bytes),
    ("credit", Kind::Bytes),
    ("postings", Kind::Int),
];
const CLEARED: Columns = &[
    ("position", Kind::Int),
    ("idx", Kind::Int),
    ("applied", Kind::Bytes),
];
const OUTSTANDING: Columns = &[
    ("handle", Kind::Int),
    ("currency", Kind::Text),
    ("debit", Kind::Bytes),
    ("credit", Kind::Bytes),
];
const SLICES: Columns = &[
    ("handle", Kind::Int),
    ("currency", Kind::Text),
    ("dimensions", Kind::Text),
    ("debit", Kind::Bytes),
    ("credit", Kind::Bytes),
    ("postings", Kind::Int),
];

/// The statements reading one projection part: its first page and the
/// pages after a key.
#[derive(Debug)]
pub(crate) struct PartPages {
    /// From the first row: `$1` the limit.
    pub(crate) first: Statement,
    /// After a key: `$1 … $k` the key, `$k+1` the limit.
    pub(crate) after: Statement,
    /// The memos part only: the `memos` rows (position, salt, text), first
    /// and after a position, merged with the entries' rows.
    pub(crate) memos: Option<(Statement, Statement)>,
}

/// The column kinds of a part's layout, computed once per process.
fn kinds(part: ProjectionPart) -> &'static [Kind] {
    static KINDS: OnceLock<Vec<(ProjectionPart, Vec<Kind>)>> = OnceLock::new();
    let all = KINDS.get_or_init(|| {
        ProjectionPart::ALL
            .into_iter()
            .map(|part| (part, part.layout().iter().map(kind).collect()))
            .collect()
    });
    all.iter()
        .find(|(candidate, _)| *candidate == part)
        .map_or(&[], |(_, kinds)| kinds.as_slice())
}

/// The statement kind of a projected column.
const fn kind(column: &Column) -> Kind {
    match column.ty {
        Ty::Int => Kind::Int,
        Ty::Bytes => Kind::Bytes,
        Ty::Text => Kind::Text,
    }
}

/// The statements of one part, rendered for `qualifier`.
fn part_pages(part: ProjectionPart, qualifier: &str) -> PartPages {
    let layout = part.layout();
    let columns = kinds(part);
    let key = part.key_len();
    if part == ProjectionPart::Memos {
        let select = "SELECT e.position, e.fingerprint, m.position, m.salt, m.text \
            FROM {{s}}entries e LEFT JOIN {{s}}memos m ON m.position = e.position";
        let memos = "SELECT position, salt, text FROM {{s}}memos";
        let render = |sql: String, columns| Statement::render(&sql, qualifier, columns);
        return PartPages {
            first: render(format!("{select} ORDER BY e.position LIMIT $1"), columns),
            after: render(
                format!("{select} WHERE e.position > $1 ORDER BY e.position LIMIT $2"),
                columns,
            ),
            memos: Some((
                render(
                    format!("{memos} ORDER BY position LIMIT $1"),
                    &[Kind::Int, Kind::Bytes, Kind::Text],
                ),
                render(
                    format!("{memos} WHERE position > $1 ORDER BY position LIMIT $2"),
                    &[Kind::Int, Kind::Bytes, Kind::Text],
                ),
            )),
        };
    }
    let names: Vec<&str> = layout.iter().map(|column| column.name).collect();
    let keys = names.get(..key).unwrap_or_default().join(", ");
    let select = format!("SELECT {} FROM {{{{s}}}}{}", names.join(", "), part.table());
    let params: Vec<String> = (1..=key).map(|n| format!("${n}")).collect();
    PartPages {
        first: Statement::render(
            &format!("{select} ORDER BY {keys} LIMIT $1"),
            qualifier,
            columns,
        ),
        after: Statement::render(
            &format!(
                "{select} WHERE ({keys}) > ({}) ORDER BY {keys} LIMIT ${}",
                params.join(", "),
                key.saturating_add(1)
            ),
            qualifier,
            columns,
        ),
        memos: None,
    }
}

/// Every statement of one store.
#[derive(Debug)]
pub(crate) struct Catalogue {
    /// The schema qualifier.
    pub(crate) qualifier: String,
    /// The shared statements.
    pub(crate) shared: Shared,
    /// The batched lookups.
    pub(crate) keyed: Keyed,
    /// The bulk inserts.
    pub(crate) inserts: Inserts,
    /// The projection pages, in [`ProjectionPart::ALL`] order.
    pub(crate) parts: Vec<(ProjectionPart, PartPages)>,
}

impl Catalogue {
    /// The statements of projection part `part`.
    pub(crate) fn part(&self, part: ProjectionPart) -> Option<&PartPages> {
        self.parts
            .iter()
            .find(|(candidate, _)| *candidate == part)
            .map(|(_, pages)| pages)
    }
}

impl Catalogue {
    /// Renders every statement for schema `qualifier`; the row sources of
    /// batches are the dialect's, rendered per execution.
    pub(crate) fn new(qualifier: String) -> Self {
        let insert = |table: &str, columns: Columns, conflict: &str| {
            let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
            Batch {
                template: Arc::from(format!(
                    "INSERT INTO {qualifier}{table} ({}) {{{{rows}}}}{conflict}",
                    names.join(", ")
                )),
                kinds: columns.iter().map(|(_, kind)| *kind).collect(),
                columns: &[],
            }
        };
        let inserts = Inserts {
            nodes: insert("nodes", NODES, ""),
            account_versions: insert("account_versions", ACCOUNT_VERSIONS, ""),
            seal_leaves: insert("seal_leaves", SEAL_LEAVES, ""),
            seal_nodes: insert("seal_nodes", SEAL_NODES, ""),
            postings: insert("postings", POSTINGS, ""),
            posting_dims: insert("posting_dims", POSTING_DIMS, ""),
            balances: insert(
                "balances",
                BALANCES,
                " ON CONFLICT (handle, currency, layer) DO UPDATE SET debit = excluded.debit, \
                 credit = excluded.credit, postings = excluded.postings",
            ),
            cleared: insert(
                "cleared",
                CLEARED,
                " ON CONFLICT (position, idx) DO UPDATE SET applied = excluded.applied",
            ),
            slices: insert(
                "slices",
                SLICES,
                " ON CONFLICT (handle, currency, dimensions) DO UPDATE SET debit = excluded.debit, \
                 credit = excluded.credit, postings = excluded.postings",
            ),
            outstanding: insert(
                "outstanding",
                OUTSTANDING,
                " ON CONFLICT (handle, currency) DO UPDATE SET debit = excluded.debit, \
                 credit = excluded.credit",
            ),
        };
        Self {
            shared: Shared::render(&qualifier),
            keyed: Keyed::render(&qualifier),
            inserts,
            parts: ProjectionPart::ALL
                .into_iter()
                .map(|part| (part, part_pages(part, &qualifier)))
                .collect(),
            qualifier,
        }
    }

    /// The statement of one postings page: the variant for the read's order
    /// and filters (never caller text), with the positions of its parameters
    /// in the order `reads::postings` binds them.
    pub(crate) fn postings_page(&self, read: &PostingRead) -> Statement {
        let mut sql = String::from(POSTINGS_PAGE);
        let mut next = 5u32;
        let mut param = || {
            let current = next;
            next = next.saturating_add(1);
            format!("${current}")
        };
        let order = match read.order {
            PostingOrder::Position => {
                sql.push_str(&format!(
                    " AND (p.position, p.idx) > ({}, {})",
                    param(),
                    param()
                ));
                " ORDER BY p.position, p.idx"
            }
            PostingOrder::Booking => {
                let (a, b, c) = (param(), param(), param());
                sql.push_str(&format!(
                    " AND (p.booking, p.position, p.idx) > ({a}, {b}, {c})"
                ));
                " ORDER BY p.booking, p.position, p.idx"
            }
        };
        if read.booking.is_some() {
            sql.push_str(&format!(
                " AND p.booking BETWEEN {} AND {}",
                param(),
                param()
            ));
        }
        if read.value.is_some() {
            sql.push_str(&format!(
                " AND p.value_date BETWEEN {} AND {}",
                param(),
                param()
            ));
        }
        for _ in &read.dimensions {
            let (axis, value) = (param(), param());
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM {{{{s}}}}posting_dims d WHERE d.position = p.position \
                 AND d.idx = p.idx AND d.axis = {axis} AND d.value = {value})"
            ));
        }
        sql.push_str(order);
        sql.push_str(&format!(" LIMIT {}", param()));
        Statement::render(&sql, &self.qualifier, POSTINGS_COLUMNS)
    }
}
