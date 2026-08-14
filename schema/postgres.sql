-- Reference PostgreSQL schema for a `doubleentry` backend.
--
-- This is the canonical mapping from the engine's model onto SQL. It is not
-- executed by the crate; it is the shape a backend is expected to implement, and
-- the conformance suite in `doubleentry::storage::conformance` is what decides
-- whether an implementation of it is correct.
--
-- Every constraint below exists because dropping it breaks a stated guarantee.
-- Where that is not obvious, it is spelled out.
--
-- Idempotent: re-applying it is a no-op, because a backend is restarted far more
-- often than it is created.

BEGIN;

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
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT ledger_meta_single_row CHECK (only_row = 1)
);

-- ── accounts ────────────────────────────────────────────────────────────────
--
-- Master data, not journal data: closing an account changes what may be booked
-- next and never what was booked already, so this table is mutable while
-- everything below it is not.

CREATE TABLE IF NOT EXISTS accounts (
    account_index   INTEGER     NOT NULL,
    path            TEXT        NOT NULL,
    -- 'asset' | 'liability' | 'equity' | 'income' | 'expense', or NULL.
    -- Reporting metadata only; the engine never constrains a posting by it.
    kind            TEXT,
    opened_on       DATE        NOT NULL,
    closed_on       DATE,
    -- 'unlimited' | 'no_credit' | 'no_debit'. A rule about what may be booked
    -- next, like the open window: 'no_credit' forbids the net going credit (an
    -- asset that cannot be overdrawn), 'no_debit' forbids it going debit (a
    -- liability that cannot be drawn beyond what was funded). Enforced inside
    -- the append, because the check is against the balance the entry would
    -- leave behind.
    balance_limit   TEXT        NOT NULL DEFAULT 'unlimited',

    CONSTRAINT accounts_window CHECK (closed_on IS NULL OR closed_on >= opened_on),
    CONSTRAINT accounts_balance_limit CHECK (
        balance_limit IN ('unlimited', 'no_credit', 'no_debit')
    ),
    CONSTRAINT accounts_kind CHECK (
        kind IS NULL
        OR kind IN ('asset', 'liability', 'equity', 'income', 'expense')
    ),
    PRIMARY KEY (account_index),
    UNIQUE (path)
);

-- ── periods ─────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS periods (
    period_id       TEXT        NOT NULL,
    starts_on       DATE        NOT NULL,
    ends_on         DATE        NOT NULL,
    -- 'open' | 'closing' | 'sealed'. Sealed is terminal.
    state           TEXT        NOT NULL,

    CONSTRAINT periods_range CHECK (ends_on >= starts_on),
    CONSTRAINT periods_state CHECK (state IN ('open', 'closing', 'sealed')),
    PRIMARY KEY (period_id),
    -- Periods may not overlap: a booking date must resolve to one period.
    EXCLUDE USING gist (daterange(starts_on, ends_on, '[]') WITH &&)
);

-- ── entries ─────────────────────────────────────────────────────────────────
--
-- INSERT-only. Grant no UPDATE or DELETE on this table or on `postings`; the
-- engine's immutability guarantee is only as strong as the privileges behind it.

CREATE TABLE IF NOT EXISTS entries (
    -- Identity. Stable from the moment the row is written, unlike the log
    -- position, which may be assigned later.
    entry_id            UUID        NOT NULL,

    -- Position in the log: dense and gap-free from zero, in commit order.
    --
    -- Deliberately NOT a sequence. Sequence values are consumed before commit,
    -- so two concurrent transactions can commit out of order and a reader
    -- tracking a high-water mark steps over the lower index permanently.
    --
    -- NULL while an entry is recorded but not yet sequenced. In inline mode the
    -- append assigns it under a lock; in deferred mode writers insert
    -- concurrently and the sequencer assigns positions afterwards.
    log_index           BIGINT,

    -- The transaction that inserted this row, as a 64-bit id that never wraps.
    --
    -- This is what makes deferred sequencing safe. The sequencer only picks up
    -- rows whose inserting transaction is *definitely* finished — those with
    -- insert_xid below pg_snapshot_xmin(pg_current_snapshot()) — so a row that
    -- is still in flight cannot be skipped and then appear behind the reader.
    insert_xid          xid8        NOT NULL DEFAULT pg_current_xact_id(),

    -- The idempotency gate. This unique index — claimed by the INSERT itself,
    -- never by a preceding SELECT — is what makes a retry safe under
    -- concurrency. A read-then-write races and duplicates the entry.
    idempotency_key     BYTEA       NOT NULL,

    -- BLAKE3 over the canonical encoding. Distinguishes a safe replay (same
    -- hash) from a conflict (same key, different hash).
    content_hash        BYTEA       NOT NULL,

    booking_date        DATE        NOT NULL,
    value_date          DATE        NOT NULL,
    description         TEXT        NOT NULL DEFAULT '',

    -- Caller-defined entry kind (a Label, e.g. an invoice/payment type). Opaque to
    -- the engine, part of the content hash, and carried on statement lines.
    kind                TEXT,

    provenance_actor        TEXT,
    provenance_source       TEXT,
    provenance_correlation  TEXT,

    document_id             TEXT,
    document_content_hash   BYTEA,

    -- Set when this entry reverses another, together with the original's
    -- booking date so a correction booked into a later period can still be
    -- attributed to the period it economically belongs to.
    reverses                UUID,
    original_booking_date   DATE,

    -- The Merkle tree root *after* this entry, so a head query is a single row
    -- read rather than a rebuild from every content hash ever stored. Historical
    -- heads are equally cheap, which is what pins a checkpoint to one history.
    --
    -- NULL until the entry is sequenced: a root only means something once the
    -- entry has a position.
    tree_root           BYTEA,

    -- When the row was written. Audit metadata only: ordering is by log_index,
    -- because a wall clock is neither monotonic nor agreed between writers.
    recorded_at         TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT entries_log_index_non_negative CHECK (log_index IS NULL OR log_index >= 0),
    CONSTRAINT entries_content_hash_width CHECK (octet_length(content_hash) = 32),
    CONSTRAINT entries_tree_root_width CHECK (
        tree_root IS NULL OR octet_length(tree_root) = 32
    ),
    -- A position and a root are assigned together or not at all.
    CONSTRAINT entries_sequenced_pair CHECK ((log_index IS NULL) = (tree_root IS NULL)),
    PRIMARY KEY (entry_id),
    UNIQUE (log_index),
    UNIQUE (idempotency_key),
    -- A hash without an identifier references nothing. The converse is
    -- allowed: an entry may name a document it holds no hash for.
    CONSTRAINT entries_document_pair CHECK (
        document_id IS NOT NULL OR document_content_hash IS NULL
    ),
    CONSTRAINT entries_reversal_pair CHECK (
        (reverses IS NULL) = (original_booking_date IS NULL)
    )
);

-- An entry may be reversed at most once.
CREATE UNIQUE INDEX IF NOT EXISTS entries_one_reversal_each
    ON entries (reverses) WHERE reverses IS NOT NULL;

CREATE INDEX IF NOT EXISTS entries_booking_date ON entries (booking_date);
CREATE INDEX IF NOT EXISTS entries_log_order ON entries (log_index)
    WHERE log_index IS NOT NULL;
-- The sequencer's working set: everything not yet placed, oldest transaction
-- first, so positions follow commit order.
CREATE INDEX IF NOT EXISTS entries_unsequenced ON entries (insert_xid, entry_id)
    WHERE log_index IS NULL;

-- ── merkle accumulator ──────────────────────────────────────────────────────
--
-- The perfect-subtree roots covering the log: one row per set bit in the size,
-- so `O(log n)` rows. Updated inside the append, which is already serialised.
--
-- This is derived state — it can always be rebuilt from `entries.content_hash`
-- in log order — but keeping it means an append costs `O(log n)` instead of
-- rebuilding the whole tree.

CREATE TABLE IF NOT EXISTS log_subtrees (
    -- Height of the perfect subtree, 0 for a single leaf.
    height          SMALLINT    NOT NULL,
    node            BYTEA       NOT NULL,
    -- Ordering position, largest subtree first.
    position        SMALLINT    NOT NULL,

    CONSTRAINT log_subtrees_width CHECK (octet_length(node) = 32),
    PRIMARY KEY (height)
);

-- ── postings ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS postings (
    -- Keyed on the entry, not its log position: the position may not exist yet.
    entry_id        UUID        NOT NULL,
    posting_index   SMALLINT    NOT NULL,

    account_index   INTEGER     NOT NULL,
    -- 'D' | 'C'. The side is explicit rather than encoded in the sign: a signed
    -- net cannot reproduce gross turnover, which a trial balance must report.
    direction       CHAR(1)     NOT NULL,
    -- A magnitude in minor units at the ledger's scale. Never negative.
    amount_minor    BIGINT      NOT NULL,
    currency        CHAR(3)     NOT NULL,
    -- 'settled' | 'pending'. Pending reserves without moving.
    layer           TEXT        NOT NULL,

    PRIMARY KEY (entry_id, posting_index),
    FOREIGN KEY (entry_id) REFERENCES entries (entry_id),
    FOREIGN KEY (account_index) REFERENCES accounts (account_index),
    CONSTRAINT postings_direction CHECK (direction IN ('D', 'C')),
    CONSTRAINT postings_layer CHECK (layer IN ('settled', 'pending')),
    -- Zero carries no information and a negative magnitude would silently mean
    -- the opposite direction.
    CONSTRAINT postings_amount_positive CHECK (amount_minor > 0)
);

CREATE INDEX IF NOT EXISTS postings_account
    ON postings (account_index, currency, layer, entry_id);

-- ── posting dimensions ──────────────────────────────────────────────────────
--
-- One row per axis, rather than a column per axis. The engine ships no axis
-- names, so a column set would have to be either this crate's guess at yours or
-- a schema change every time a new one is needed. A child table is also the only
-- shape that indexes: `WHERE axis = 'activity' AND value = 'Network'` is an
-- index scan here and a full scan over a JSON blob.

CREATE TABLE IF NOT EXISTS posting_dimensions (
    entry_id        UUID        NOT NULL,
    posting_index   SMALLINT    NOT NULL,
    axis            TEXT        NOT NULL,
    value           TEXT        NOT NULL,

    PRIMARY KEY (entry_id, posting_index, axis),
    FOREIGN KEY (entry_id, posting_index) REFERENCES postings (entry_id, posting_index)
);

CREATE INDEX IF NOT EXISTS posting_dimensions_lookup
    ON posting_dimensions (axis, value);

-- Defence in depth. The engine will not produce an unbalanced entry, but
-- application-level and database-level enforcement fail independently, and this
-- is the invariant worth paying for twice. DEFERRED so an entry's postings may
-- be inserted in any order within one transaction.
CREATE OR REPLACE FUNCTION postings_balance_per_currency() RETURNS trigger AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM postings
        WHERE entry_id = NEW.entry_id
        GROUP BY currency
        HAVING SUM(CASE WHEN direction = 'D' THEN amount_minor ELSE -amount_minor END) <> 0
    ) THEN
        RAISE EXCEPTION 'entry % is unbalanced in at least one currency', NEW.entry_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS postings_balanced ON postings;
CREATE CONSTRAINT TRIGGER postings_balanced
    AFTER INSERT ON postings
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION postings_balance_per_currency();

-- An entry needs at least two postings to be double-entry.
CREATE OR REPLACE FUNCTION postings_at_least_two() RETURNS trigger AS $$
BEGIN
    IF (SELECT count(*) FROM postings
        WHERE entry_id = NEW.entry_id) < 2 THEN
        RAISE EXCEPTION 'entry % has fewer than two postings', NEW.entry_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS postings_minimum ON postings;
CREATE CONSTRAINT TRIGGER postings_minimum
    AFTER INSERT ON postings
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION postings_at_least_two();

-- ── seals ───────────────────────────────────────────────────────────────────
--
-- One row per sealed period, chained. Removing or reordering a row breaks every
-- seal after it, which is the point.

CREATE TABLE IF NOT EXISTS seals (
    period_id           TEXT        NOT NULL,
    -- Smallest and largest log index belonging to the period. Entries append in
    -- recording order, not booking-date order, so the span may enclose entries
    -- from other periods — hence the separate count.
    first_index         BIGINT,
    last_index          BIGINT,
    entry_count         BIGINT      NOT NULL,

    tree_size           BIGINT      NOT NULL,
    tree_root           BYTEA       NOT NULL,
    trial_balance_root  BYTEA       NOT NULL,

    -- Merkle root over the handle-to-account bindings in force at sealing.
    --
    -- The trial balance root above is keyed on `account_index`, a dense integer
    -- that means nothing on its own. Without this column those handles float:
    -- renumbering `accounts` afterwards would leave every seal and every balance
    -- proof verifying unchanged while every balance referred to a different
    -- account — the exact alteration a seal exists to expose.
    accounts_root       BYTEA       NOT NULL,

    prev_seal           BYTEA,
    seal_hash           BYTEA       NOT NULL,

    -- Position in the chain. Explicit, because the chain has a definite order
    -- and a timestamp does not reproduce it: two seals in the same clock tick
    -- would order arbitrarily, and the fallback of ordering by period_id is
    -- lexical rather than chronological.
    chain_position      BIGINT      NOT NULL,

    sealed_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT seals_hash_widths CHECK (
        octet_length(tree_root) = 32
        AND octet_length(trial_balance_root) = 32
        AND octet_length(accounts_root) = 32
        AND octet_length(seal_hash) = 32
        AND (prev_seal IS NULL OR octet_length(prev_seal) = 32)
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
CREATE UNIQUE INDEX IF NOT EXISTS seals_single_genesis ON seals ((prev_seal IS NULL))
    WHERE prev_seal IS NULL;

-- ── clearing ────────────────────────────────────────────────────────────────
--
-- Assignment, not movement: clearing never changes a balance. A posting's
-- residual is its amount less everything applied to it.

CREATE TABLE IF NOT EXISTS clearings (
    clearing_id     UUID        NOT NULL,
    account_index   INTEGER     NOT NULL,
    currency        CHAR(3)     NOT NULL,
    -- A clearing relates postings within one layer. A reservation and a settled
    -- movement are different claims on the same account, and netting one
    -- against the other would report an open item closed while the money had
    -- not moved.
    layer           TEXT        NOT NULL,
    cleared_on      DATE        NOT NULL,
    -- Set when released. The withdrawn assignment stays on file: an assignment
    -- made and taken back is itself part of the audit trail.
    reset_on        DATE,

    PRIMARY KEY (clearing_id),
    FOREIGN KEY (account_index) REFERENCES accounts (account_index),
    CONSTRAINT clearings_layer CHECK (layer IN ('settled', 'pending'))
);

CREATE TABLE IF NOT EXISTS clearing_items (
    clearing_id     UUID        NOT NULL,
    entry_id        UUID        NOT NULL,
    posting_index   SMALLINT    NOT NULL,
    applied_minor   BIGINT      NOT NULL,

    PRIMARY KEY (clearing_id, entry_id, posting_index),
    FOREIGN KEY (clearing_id) REFERENCES clearings (clearing_id),
    FOREIGN KEY (entry_id, posting_index) REFERENCES postings (entry_id, posting_index),
    CONSTRAINT clearing_items_positive CHECK (applied_minor > 0)
);

CREATE INDEX IF NOT EXISTS clearing_items_posting
    ON clearing_items (entry_id, posting_index);

-- Open items: everything with a positive residual. A backend may materialise
-- this; the definition is what matters.
CREATE OR REPLACE VIEW open_items AS
SELECT
    p.entry_id,
    p.posting_index,
    p.account_index,
    p.currency,
    p.layer,
    p.direction,
    p.amount_minor                                              AS original_minor,
    -- SUM() over BIGINT yields NUMERIC in PostgreSQL; cast back so the column
    -- type matches the minor-unit representation everywhere else.
    COALESCE(applied.total, 0)::BIGINT                          AS applied_minor,
    (p.amount_minor - COALESCE(applied.total, 0))::BIGINT       AS residual_minor
FROM postings p
LEFT JOIN (
    SELECT ci.entry_id, ci.posting_index,
           SUM(ci.applied_minor)::BIGINT AS total
    FROM clearing_items ci
    JOIN clearings c ON c.clearing_id = ci.clearing_id
    WHERE c.reset_on IS NULL          -- a released clearing applies nothing
    GROUP BY ci.entry_id, ci.posting_index
) applied
    ON applied.entry_id = p.entry_id
   AND applied.posting_index = p.posting_index
WHERE p.amount_minor - COALESCE(applied.total, 0) > 0;

-- ── checkpoints ─────────────────────────────────────────────────────────────
--
-- A recorded balance over a known prefix of the log, so a read does not fold the
-- whole journal. Derived state, and safe only because it can be re-derived.
--
-- The prefix is `tree_size`: the head both names how much of the log the balance
-- covers and pins which history it covers, so a checkpoint cannot be silently
-- reused against a log that changed. There is deliberately no second position
-- column — two columns that must agree are two columns that can disagree.

CREATE TABLE IF NOT EXISTS checkpoints (
    account_index   INTEGER     NOT NULL,
    currency        CHAR(3)     NOT NULL,
    layer           TEXT        NOT NULL,

    debits_minor    BIGINT      NOT NULL,
    credits_minor   BIGINT      NOT NULL,

    tree_size       BIGINT      NOT NULL,
    tree_root       BYTEA       NOT NULL,

    taken_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (account_index, currency, layer),
    CONSTRAINT checkpoints_layer CHECK (layer IN ('settled', 'pending')),
    CONSTRAINT checkpoints_root_width CHECK (octet_length(tree_root) = 32)
);

COMMIT;

-- ── operational notes ───────────────────────────────────────────────────────
--
-- Privileges. The application role needs INSERT and SELECT on `entries` and
-- `postings`, and no UPDATE or DELETE. Immutability enforced only in application
-- code is a convention; enforced by GRANT it is a property.
--
-- Sequencing. Two modes, both supported by this schema.
--
-- Inline: the append takes an advisory lock, reads the next index, and assigns
-- it before committing. Simple, and an entry is provable the moment it is
-- durable. Appends to one ledger serialise.
--
-- Deferred: writers INSERT concurrently with `log_index` NULL, and a single
-- sequencer assigns positions afterwards. Appends do not block each other; the
-- cost is a window in which an entry is durable but not yet provable.
--
-- The sequencer must advance on a commit-order watermark, never on a high-water
-- mark over an unordered column:
--
--   SELECT ... FROM entries
--   WHERE log_index IS NULL
--     AND insert_xid < pg_snapshot_xmin(pg_current_snapshot())
--   ORDER BY insert_xid, entry_id
--
-- The predicate admits only rows whose inserting transaction has finished. A
-- row still in flight is left for the next pass rather than skipped — which is
-- the whole point, since a skipped row would appear *behind* the reader once it
-- committed and would never be picked up again.
--
-- Note that this watermark is cluster-wide. A transaction left open anywhere in
-- the instance — including in another database — holds it back, and everything
-- recorded after that transaction began waits for it to end. The behaviour is
-- safe rather than lossy: the sequencer simply declines to place what it cannot
-- yet prove is settled. But sequencing latency is bounded by the longest open
-- transaction in the cluster, which is worth monitoring.
