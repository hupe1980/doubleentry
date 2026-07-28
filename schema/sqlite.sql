-- Reference SQLite schema for a `doubleentry` backend.
--
-- The same model as `schema/postgres.sql`, expressed in what SQLite provides.
-- Idempotent: re-applying it is a no-op.
--
-- Four things PostgreSQL enforces cannot be expressed here, and the backend
-- enforces each in code instead. They are called out at the point they would
-- otherwise appear, because a reader comparing the two files should not have to
-- guess which omissions are deliberate.

-- Foreign keys are off by default in SQLite, which would silently disable every
-- REFERENCES clause below.
PRAGMA foreign_keys = ON;

-- ── ledger identity ─────────────────────────────────────────────────────────
--
-- One database holds exactly one ledger. That is the isolation boundary: its own
-- log, its own dense index space, its own Merkle tree, its own seal chain.
--
-- Sharing a database between ledgers would mean sharing all of those. A seal
-- commits to one entity's history, so a shared log would have each tenant's seal
-- committing to the others' entries, and an inclusion proof shown to one auditor
-- would reveal how many entries the others hold. Filtering by a column cannot
-- fix that, and it fails open: one query missing its predicate is a silent leak.
-- Separation here is physical, so there is no predicate to forget.
--
-- This row records which ledger the database holds. `migrate` writes it on first
-- use and refuses to open a database belonging to a different one.

CREATE TABLE IF NOT EXISTS ledger_meta (
    -- Exactly one row, enforced by the check.
    only_row        INTEGER     PRIMARY KEY,
    ledger_id       TEXT        NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),

    CONSTRAINT ledger_meta_single_row CHECK (only_row = 1)
);

-- ── accounts ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS accounts (
    account_index   INTEGER NOT NULL,
    path            TEXT    NOT NULL,
    kind            TEXT,
    opened_on       TEXT    NOT NULL,
    closed_on       TEXT,

    CONSTRAINT accounts_window CHECK (closed_on IS NULL OR closed_on >= opened_on),
    CONSTRAINT accounts_kind CHECK (
        kind IS NULL
        OR kind IN ('asset', 'liability', 'equity', 'income', 'expense')
    ),
    PRIMARY KEY (account_index),
    UNIQUE (path)
);

-- ── periods ─────────────────────────────────────────────────────────────────
--
-- Difference from PostgreSQL: no `EXCLUDE USING gist`, so non-overlap is
-- enforced by `PeriodCalendar` rather than by the database.

CREATE TABLE IF NOT EXISTS periods (
    period_id       TEXT NOT NULL,
    starts_on       TEXT NOT NULL,
    ends_on         TEXT NOT NULL,
    state           TEXT NOT NULL,

    CONSTRAINT periods_range CHECK (ends_on >= starts_on),
    CONSTRAINT periods_state CHECK (state IN ('open', 'closing', 'sealed')),
    PRIMARY KEY (period_id)
);

-- ── entries ─────────────────────────────────────────────────────────────────
--
-- INSERT-only. SQLite has no per-table privileges, so immutability here rests on
-- the application never issuing UPDATE or DELETE — weaker than a GRANT, and
-- worth knowing when choosing between the two backends.

CREATE TABLE IF NOT EXISTS entries (
    -- Dense and gap-free from zero, in append order. Not AUTOINCREMENT: the
    -- index is assigned inside the append so it cannot be consumed by a
    -- transaction that later rolls back.
    log_index               INTEGER,

    entry_id                BLOB    NOT NULL,

    -- The idempotency gate, claimed by the INSERT itself.
    idempotency_key         BLOB    NOT NULL,

    content_hash            BLOB    NOT NULL,

    booking_date            TEXT    NOT NULL,
    value_date              TEXT    NOT NULL,
    description             TEXT    NOT NULL DEFAULT '',

    provenance_actor        TEXT,
    provenance_source       TEXT,
    provenance_correlation  TEXT,

    document_id             TEXT,
    document_content_hash   BLOB,

    reverses                BLOB,
    original_booking_date   TEXT,

    -- The Merkle root after this entry, so a head query is one row read.
    tree_root               BLOB,

    recorded_at             TEXT    NOT NULL DEFAULT (datetime('now')),

    CONSTRAINT entries_log_index_non_negative CHECK (log_index IS NULL OR log_index >= 0),
    CONSTRAINT entries_content_hash_width CHECK (length(content_hash) = 32),
    CONSTRAINT entries_tree_root_width CHECK (tree_root IS NULL OR length(tree_root) = 32),
    CONSTRAINT entries_sequenced_pair CHECK ((log_index IS NULL) = (tree_root IS NULL)),
    -- A hash without an identifier references nothing. The converse is
    -- allowed: an entry may name a document it holds no hash for.
    CONSTRAINT entries_document_pair CHECK (
        document_id IS NOT NULL OR document_content_hash IS NULL
    ),
    CONSTRAINT entries_reversal_pair CHECK (
        (reverses IS NULL) = (original_booking_date IS NULL)
    ),
    PRIMARY KEY (entry_id),
    UNIQUE (log_index),
    UNIQUE (idempotency_key)
);

-- An entry may be reversed at most once.
CREATE UNIQUE INDEX IF NOT EXISTS entries_one_reversal_each
    ON entries (reverses) WHERE reverses IS NOT NULL;

CREATE INDEX IF NOT EXISTS entries_booking_date ON entries (booking_date);
CREATE INDEX IF NOT EXISTS entries_log_order ON entries (log_index)
    WHERE log_index IS NOT NULL;

-- ── merkle accumulator ──────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS log_subtrees (
    height          INTEGER NOT NULL,
    node            BLOB    NOT NULL,
    position        INTEGER NOT NULL,

    CONSTRAINT log_subtrees_width CHECK (length(node) = 32),
    PRIMARY KEY (height)
);

-- ── postings ────────────────────────────────────────────────────────────────
--
-- Difference from PostgreSQL: no DEFERRABLE constraint triggers, so the balance
-- invariant is not enforced a second time at the database layer. The engine
-- still refuses to produce an unbalanced entry; what is lost is defence in depth
-- against anything writing to this table that is not the engine.

CREATE TABLE IF NOT EXISTS postings (
    -- Keyed on the entry, not its log position: the position may not exist yet.
    entry_id        BLOB    NOT NULL,
    posting_index   INTEGER NOT NULL,

    account_index   INTEGER NOT NULL,
    direction       TEXT    NOT NULL,
    amount_minor    INTEGER NOT NULL,
    currency        TEXT    NOT NULL,
    layer           TEXT    NOT NULL,

    dim_activity    TEXT,
    dim_segment     TEXT,
    dim_cost_object TEXT,
    dim_party       TEXT,

    PRIMARY KEY (entry_id, posting_index),
    FOREIGN KEY (entry_id) REFERENCES entries (entry_id),
    FOREIGN KEY (account_index) REFERENCES accounts (account_index),
    CONSTRAINT postings_direction CHECK (direction IN ('D', 'C')),
    CONSTRAINT postings_layer CHECK (layer IN ('settled', 'pending')),
    CONSTRAINT postings_amount_positive CHECK (amount_minor > 0)
);

CREATE INDEX IF NOT EXISTS postings_account
    ON postings (account_index, currency, layer, entry_id);
CREATE INDEX IF NOT EXISTS postings_activity
    ON postings (dim_activity) WHERE dim_activity IS NOT NULL;

-- ── seals ───────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS seals (
    period_id           TEXT NOT NULL,
    first_index         INTEGER,
    last_index          INTEGER,
    entry_count         INTEGER NOT NULL,

    tree_size           INTEGER NOT NULL,
    tree_root           BLOB    NOT NULL,
    trial_balance_root  BLOB    NOT NULL,

    prev_seal           BLOB,
    seal_hash           BLOB    NOT NULL,

    -- Monotonic ordering of the chain. SQLite has no stable timestamp ordering
    -- at sub-second resolution, so the chain position is explicit.
    chain_position      INTEGER NOT NULL,

    sealed_at           TEXT    NOT NULL DEFAULT (datetime('now')),

    CONSTRAINT seals_hash_widths CHECK (
        length(tree_root) = 32
        AND length(trial_balance_root) = 32
        AND length(seal_hash) = 32
        AND (prev_seal IS NULL OR length(prev_seal) = 32)
    ),
    CONSTRAINT seals_span CHECK (
        (first_index IS NULL) = (last_index IS NULL)
        AND (last_index IS NULL OR last_index >= first_index)
    ),
    PRIMARY KEY (period_id),
    FOREIGN KEY (period_id) REFERENCES periods (period_id),
    UNIQUE (seal_hash),
    UNIQUE (chain_position)
);

-- Exactly one seal may lack a predecessor.
-- Exactly one seal may lack a predecessor.
CREATE UNIQUE INDEX IF NOT EXISTS seals_single_genesis ON seals ((prev_seal IS NULL))
    WHERE prev_seal IS NULL;

-- ── clearing ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS clearings (
    clearing_id     BLOB    NOT NULL,
    account_index   INTEGER NOT NULL,
    currency        TEXT    NOT NULL,
    cleared_on      TEXT    NOT NULL,
    reset_on        TEXT,

    PRIMARY KEY (clearing_id),
    FOREIGN KEY (account_index) REFERENCES accounts (account_index)
);

CREATE TABLE IF NOT EXISTS clearing_items (
    clearing_id     BLOB    NOT NULL,
    entry_id        BLOB    NOT NULL,
    posting_index   INTEGER NOT NULL,
    applied_minor   INTEGER NOT NULL,

    PRIMARY KEY (clearing_id, entry_id, posting_index),
    FOREIGN KEY (clearing_id) REFERENCES clearings (clearing_id),
    FOREIGN KEY (entry_id, posting_index) REFERENCES postings (entry_id, posting_index),
    CONSTRAINT clearing_items_positive CHECK (applied_minor > 0)
);

CREATE INDEX IF NOT EXISTS clearing_items_posting
    ON clearing_items (entry_id, posting_index);

DROP VIEW IF EXISTS open_items;
CREATE VIEW open_items AS
SELECT
    p.entry_id,
    p.posting_index,
    p.account_index,
    p.currency,
    p.layer,
    p.direction,
    p.amount_minor                                      AS original_minor,
    COALESCE(applied.total, 0)                          AS applied_minor,
    p.amount_minor - COALESCE(applied.total, 0)         AS residual_minor
FROM postings p
LEFT JOIN (
    SELECT ci.entry_id, ci.posting_index,
           SUM(ci.applied_minor) AS total
    FROM clearing_items ci
    JOIN clearings c ON c.clearing_id = ci.clearing_id
    WHERE c.reset_on IS NULL          -- a released clearing applies nothing
    GROUP BY ci.entry_id, ci.posting_index
) applied
    ON applied.entry_id = p.entry_id
   AND applied.posting_index = p.posting_index
WHERE p.amount_minor - COALESCE(applied.total, 0) > 0;

-- ── checkpoints ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS checkpoints (
    account_index   INTEGER NOT NULL,
    currency        TEXT    NOT NULL,
    layer           TEXT    NOT NULL,

    through_index   INTEGER,
    debits_minor    INTEGER NOT NULL,
    credits_minor   INTEGER NOT NULL,

    tree_size       INTEGER NOT NULL,
    tree_root       BLOB    NOT NULL,

    taken_at        TEXT    NOT NULL DEFAULT (datetime('now')),

    PRIMARY KEY (account_index, currency, layer),
    CONSTRAINT checkpoints_layer CHECK (layer IN ('settled', 'pending')),
    CONSTRAINT checkpoints_root_width CHECK (length(tree_root) = 32)
);

-- ── operational notes ───────────────────────────────────────────────────────
--
-- Write serialisation. SQLite admits one writer at a time, and the backend opens
-- its append with BEGIN IMMEDIATE so the write lock is taken before the index is
-- read rather than after. A deferred transaction would read the next index, then
-- fail to upgrade under contention — or worse, succeed against a stale read.
--
-- Recommended connection settings: `journal_mode = WAL` so readers do not block
-- the writer, `foreign_keys = ON`, and a `busy_timeout` long enough to absorb a
-- concurrent append. `SqliteStore::migrate` sets the first two.
--
-- Choosing between the backends. SQLite suits embedded and single-process
-- deployments and needs no server. PostgreSQL additionally enforces the balance
-- invariant and period non-overlap in the database, and supports revoking UPDATE
-- and DELETE — so where the ledger must be defended against processes other than
-- this one, it is the stronger choice.
