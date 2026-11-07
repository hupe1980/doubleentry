-- The doubleentry/1 schema: one template for both dialects.
-- {{s}} is the schema qualifier, the other tokens the dialect's types.
-- Created by `create` only; `open` refuses any other schema identifier.

CREATE TABLE {{s}}meta (
    singleton    {{int}}   PRIMARY KEY CHECK (singleton = 1),
    schema_id    {{text}}  NOT NULL,
    ledger_id    {{text}}  NOT NULL,
    amount_scale {{int}}   NOT NULL CHECK (amount_scale BETWEEN 0 AND 18),
    genesis      {{bytes}} NOT NULL CHECK ({{len}}(genesis) = 32)
){{strict}};

CREATE TABLE {{s}}leaves (
    position {{int}}   PRIMARY KEY CHECK (position >= 0),
    kind     {{int}}   NOT NULL CHECK (kind BETWEEN 1 AND 8),
    bytes    {{bytes}} NOT NULL CHECK ({{len}}(bytes) BETWEEN 1 AND 65535)
){{strict}};
CREATE INDEX leaves_by_kind ON {{s}}leaves (kind, position);

CREATE TABLE {{s}}nodes (
    id   {{int}}   PRIMARY KEY CHECK (id >= 0),
    hash {{bytes}} NOT NULL CHECK ({{len}}(hash) = 32)
){{strict}};

CREATE TABLE {{s}}accounts (
    handle   {{int}}  PRIMARY KEY CHECK (handle BETWEEN 0 AND 4294967295),
    path     {{text}} NOT NULL UNIQUE,
    kind     {{int}}  NOT NULL CHECK (kind BETWEEN 0 AND 4),
    position {{int}}  NOT NULL UNIQUE REFERENCES {{s}}leaves (position)
){{strict}};

CREATE TABLE {{s}}account_versions (
    handle     {{int}}  NOT NULL REFERENCES {{s}}accounts (handle),
    field      {{int}}  NOT NULL CHECK (field IN (0, 1, 2)),
    effective  {{int}}  NOT NULL,
    position   {{int}}  NOT NULL REFERENCES {{s}}leaves (position),
    open_from  {{int}},
    open_to    {{int}},
    limit_code {{int}}  CHECK (limit_code IN (0, 1, 2)),
    class      {{text}},
    CHECK (field <> 0 OR open_from IS NOT NULL),
    CHECK (field <> 1 OR limit_code IS NOT NULL),
    PRIMARY KEY (handle, field, position)
){{strict}};
CREATE INDEX account_versions_in_force ON {{s}}account_versions (handle, field, effective, position);

CREATE TABLE {{s}}currency_versions (
    code      {{text}} NOT NULL,
    exponent  {{int}}  NOT NULL CHECK (exponent BETWEEN 0 AND 18),
    effective {{int}}  NOT NULL,
    position  {{int}}  NOT NULL UNIQUE REFERENCES {{s}}leaves (position),
    PRIMARY KEY (code, position)
){{strict}};
CREATE INDEX currency_versions_in_force ON {{s}}currency_versions (code, effective, position);

CREATE TABLE {{s}}policy_versions (
    position  {{int}} PRIMARY KEY REFERENCES {{s}}leaves (position),
    effective {{int}} NOT NULL
){{strict}};
CREATE INDEX policy_versions_in_force ON {{s}}policy_versions (effective, position);

CREATE TABLE {{s}}periods (
    period_id {{text}} PRIMARY KEY,
    first_day {{int}}  NOT NULL UNIQUE,
    last_day  {{int}}  NOT NULL UNIQUE CHECK (last_day >= first_day),
    position  {{int}}  NOT NULL UNIQUE REFERENCES {{s}}leaves (position)
){{strict}};

CREATE TABLE {{s}}entries (
    position    {{int}}   PRIMARY KEY REFERENCES {{s}}leaves (position),
    ordinal     {{int}}   NOT NULL UNIQUE CHECK (ordinal >= 0),
    entry_id    {{bytes}} NOT NULL UNIQUE CHECK ({{len}}(entry_id) = 16),
    idem_key    {{bytes}} NOT NULL UNIQUE CHECK ({{len}}(idem_key) BETWEEN 1 AND 128),
    fingerprint {{bytes}} CHECK (fingerprint IS NULL OR {{len}}(fingerprint) = 32),
    reverses    {{bytes}} UNIQUE REFERENCES {{s}}entries (entry_id),
    kind        {{text}}  CHECK (kind IS NULL OR {{len}}(kind) BETWEEN 1 AND 256)
){{strict}};

CREATE TABLE {{s}}postings (
    position     {{int}}   NOT NULL REFERENCES {{s}}entries (position),
    idx          {{int}}   NOT NULL CHECK (idx BETWEEN 0 AND 1023),
    handle       {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    currency     {{text}}  NOT NULL,
    layer        {{int}}   NOT NULL CHECK (layer IN (0, 1)),
    side         {{int}}   NOT NULL CHECK (side IN (0, 1)),
    amount       {{bytes}} NOT NULL CHECK ({{len}}(amount) = 16),
    booking      {{int}}   NOT NULL,
    value_date   {{int}}   NOT NULL,
    dimensions   {{text}}  NOT NULL,
    debit_after  {{bytes}} NOT NULL CHECK ({{len}}(debit_after) = 16),
    credit_after {{bytes}} NOT NULL CHECK ({{len}}(credit_after) = 16),
    count_after  {{int}}   NOT NULL CHECK (count_after >= 1),
    PRIMARY KEY (position, idx)
){{strict}};
CREATE INDEX postings_by_key     ON {{s}}postings (handle, currency, layer, position, idx);
CREATE INDEX postings_by_key_day ON {{s}}postings (handle, currency, layer, booking, position, idx);
CREATE INDEX postings_by_day     ON {{s}}postings (booking, position, idx);

CREATE TABLE {{s}}posting_dims (
    position {{int}}  NOT NULL,
    idx      {{int}}  NOT NULL,
    axis     {{text}} NOT NULL,
    value    {{text}} NOT NULL,
    PRIMARY KEY (position, idx, axis),
    FOREIGN KEY (position, idx) REFERENCES {{s}}postings (position, idx)
){{strict}};
CREATE INDEX posting_dims_by_value ON {{s}}posting_dims (axis, value, position, idx);

CREATE TABLE {{s}}memos (
    position {{int}}   PRIMARY KEY REFERENCES {{s}}entries (position),
    salt     {{bytes}} CHECK (salt IS NULL OR {{len}}(salt) = 16),
    text     TEXT,
    CHECK ((salt IS NULL) = (text IS NULL))
){{strict}};

CREATE TABLE {{s}}holds (
    position      {{int}}   PRIMARY KEY REFERENCES {{s}}entries (position),
    entry_id      {{bytes}} NOT NULL UNIQUE REFERENCES {{s}}entries (entry_id),
    booking       {{int}}   NOT NULL,
    expires       {{int}},
    debit_handle  {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    credit_handle {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    currency      {{text}}  NOT NULL,
    amount        {{bytes}} NOT NULL CHECK ({{len}}(amount) = 16),
    resolved_by   {{bytes}} UNIQUE REFERENCES {{s}}entries (entry_id),
    how           {{int}}   CHECK (how IN (0, 1, 2)),
    CHECK ((resolved_by IS NULL) = (how IS NULL))
){{strict}};
CREATE INDEX holds_open_due    ON {{s}}holds (expires, position)       WHERE resolved_by IS NULL AND expires IS NOT NULL;
CREATE INDEX holds_open_debit  ON {{s}}holds (debit_handle, position)  WHERE resolved_by IS NULL;
CREATE INDEX holds_open_credit ON {{s}}holds (credit_handle, position) WHERE resolved_by IS NULL;

CREATE TABLE {{s}}seals (
    period_id {{text}} PRIMARY KEY REFERENCES {{s}}periods (period_id),
    position  {{int}}  NOT NULL UNIQUE REFERENCES {{s}}leaves (position)
){{strict}};

CREATE TABLE {{s}}seal_leaves (
    period_id {{text}}  NOT NULL REFERENCES {{s}}seals (period_id),
    currency  {{text}}  NOT NULL,
    layer     {{int}}   NOT NULL CHECK (layer IN (0, 1)),
    handle    {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    debit     {{bytes}} NOT NULL CHECK ({{len}}(debit) = 16),
    credit    {{bytes}} NOT NULL CHECK ({{len}}(credit) = 16),
    postings  {{int}}   NOT NULL CHECK (postings >= 1),
    PRIMARY KEY (period_id, currency, layer, handle)
){{strict}};

CREATE TABLE {{s}}seal_nodes (
    period_id {{text}}  NOT NULL REFERENCES {{s}}seals (period_id),
    currency  {{text}}  NOT NULL,
    layer     {{int}}   NOT NULL CHECK (layer IN (0, 1)),
    level     {{int}}   NOT NULL CHECK (level BETWEEN 0 AND 32),
    idx       {{int}}   NOT NULL CHECK (idx >= 0),
    hash      {{bytes}} NOT NULL CHECK ({{len}}(hash) = 32),
    PRIMARY KEY (period_id, currency, layer, level, idx)
){{strict}};

CREATE TABLE {{s}}clearings (
    position {{int}}   PRIMARY KEY REFERENCES {{s}}leaves (position),
    idem_key {{bytes}} NOT NULL UNIQUE CHECK ({{len}}(idem_key) BETWEEN 1 AND 128)
){{strict}};

CREATE TABLE {{s}}clearing_resets (
    clearing {{int}} PRIMARY KEY REFERENCES {{s}}clearings (position),
    position {{int}} NOT NULL UNIQUE REFERENCES {{s}}clearings (position)
){{strict}};

CREATE TABLE {{s}}clearing_refs (
    position {{int}}  NOT NULL REFERENCES {{s}}clearings (position),
    handle   {{int}}  NOT NULL REFERENCES {{s}}accounts (handle),
    currency {{text}} NOT NULL,
    layer    {{int}}  NOT NULL CHECK (layer IN (0, 1)),
    PRIMARY KEY (position, handle, currency, layer)
){{strict}};
CREATE INDEX clearing_refs_by_key ON {{s}}clearing_refs (handle, currency, layer, position);

CREATE TABLE {{s}}balances (
    handle   {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    currency {{text}}  NOT NULL,
    layer    {{int}}   NOT NULL CHECK (layer IN (0, 1)),
    debit    {{bytes}} NOT NULL CHECK ({{len}}(debit) = 16),
    credit   {{bytes}} NOT NULL CHECK ({{len}}(credit) = 16),
    postings {{int}}   NOT NULL CHECK (postings >= 1),
    PRIMARY KEY (handle, currency, layer)
){{strict}};

CREATE TABLE {{s}}cleared (
    position {{int}}   NOT NULL,
    idx      {{int}}   NOT NULL,
    applied  {{bytes}} NOT NULL CHECK ({{len}}(applied) = 16),
    PRIMARY KEY (position, idx),
    FOREIGN KEY (position, idx) REFERENCES {{s}}postings (position, idx)
){{strict}};

CREATE TABLE {{s}}outstanding (
    handle   {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    currency {{text}}  NOT NULL,
    debit    {{bytes}} NOT NULL CHECK ({{len}}(debit) = 16),
    credit   {{bytes}} NOT NULL CHECK ({{len}}(credit) = 16),
    PRIMARY KEY (handle, currency)
){{strict}};

CREATE TABLE {{s}}slices (
    handle     {{int}}   NOT NULL REFERENCES {{s}}accounts (handle),
    currency   {{text}}  NOT NULL,
    dimensions {{text}}  NOT NULL,
    debit      {{bytes}} NOT NULL CHECK ({{len}}(debit) = 16),
    credit     {{bytes}} NOT NULL CHECK ({{len}}(credit) = 16),
    postings   {{int}}   NOT NULL CHECK (postings >= 1),
    PRIMARY KEY (handle, currency, dimensions)
){{strict}};
