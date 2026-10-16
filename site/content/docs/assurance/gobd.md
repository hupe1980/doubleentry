+++
title = "GoBD"
description = "A compliance matrix for German tax auditors, and notes for the Verfahrensdokumentation."
weight = 6
+++

## What to call

- [`Entry`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html) — the journal fields: [`document`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.document) ([`Document`](https://docs.rs/doubleentry/latest/doubleentry/struct.Document.html)), [`rate`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.rate) ([`Rate`](https://docs.rs/doubleentry/latest/doubleentry/struct.Rate.html)), [`captured_at`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.captured_at) ([`CaptureTime`](https://docs.rs/doubleentry/latest/doubleentry/struct.CaptureTime.html)), [`kind`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.kind), [`memo`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.memo), [`value_date`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.value_date), [`reverses`](https://docs.rs/doubleentry/latest/doubleentry/struct.Entry.html#method.reverses).
- [`Journal::as_actor`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.as_actor), [`Journal::open`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.open), [`Journal::set_class`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.set_class), [`Journal::set_policy`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.set_policy) — master data as dated records.
- [`Journal::seal_period`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.seal_period) — Festschreibung of a period.
- [`Journal::export`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.export) and [`export::verify`](https://docs.rs/doubleentry/latest/doubleentry/export/fn.verify.html) — data access (Z3).
- [`Journal::erase_memo`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.erase_memo) — erasure of personal data after the retention period.
- [`Journal::audit`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.audit) and [`Journal::checkpoint`](https://docs.rs/doubleentry/latest/doubleentry/struct.Journal.html#method.checkpoint) — the controls behind Unveränderbarkeit.

`Ledger<S>` has the same methods, `async`.

This matrix maps the GoBD to what the crate (the **engine**) guarantees, with
the test that demonstrates it, and to what remains the **integrator's** duty.
It is a technical mapping, **not legal advice**: ordnungsmäßigkeit is decided
for the whole system as operated. The engine has no users, screens, clock or
network; everything that needs one is the integrator's. `just docs-stale` fails
if a cited test no longer exists.

**Text cited**: BMF-Schreiben of 28 November 2019 (IV A 4 - S 0316/19/10003,
BStBl I 2019 S. 1269), as amended on 11 March 2024
(IV D 2 - S 0316/21/10001 :002) and 14 July 2025 (IV D 2 - S 0316/00128/005/088).
"Rz." is a Randziffer of that text.

## Journal fields (Rz. 94, with Rz. 77–79, 85)

| Norm | Requirement | Engine guarantee and evidence | Integrator duty | Scope |
|---|---|---|---|---|
| Rz. 94, 77 | Eindeutige Belegnummer | Each entry carries up to 16 documents, each with a type label, a reference (≤ 256 bytes), an optional date and an optional SHA-256; all are committed in the entry's leaf and exported as document rows. `tests/golden.rs::every_record_kind_matches_the_reference_encoder`, `tests/core_rejections.rs::row_109_too_many_documents`, `tests/export.rs::us6_every_entry_and_the_root_rebuild_from_the_rows` | Put the Belegnummer in `Document::reference`; keep references unique in your numbering scheme (the engine does not check uniqueness of references across entries). | engine + integrator |
| Rz. 94, 77 | Buchungsbetrag | Posting amounts are exact integers (`i128` scaled by 10^P), strictly positive; excess precision is refused, never rounded; balances never wrap. `tests/core_rejections.rs::row_104_non_positive_amount`, `tests/core_rejections.rs::row_306_excess_precision_is_refused_never_rounded`, `tests/core_rejections.rs::row_500_balance_overflow_never_wraps` | — | engine |
| Rz. 94 | Soll- und Haben-Betrag | Every posting has a side (`Debit`/`Credit`), never a sign; an entry must balance per (currency, layer). `tests/core_rejections.rs::row_106_unbalanced_reports_the_first_pair` | — | engine |
| Rz. 94, 77 | Währungsangabe | Every posting names its currency; a currency must be declared with an exponent before use, and the exponent can never be lowered. `tests/core_rejections.rs::row_305_undeclared_currency`, `tests/core_master.rs::us1_s4_exponent_is_never_lowered_may_be_raised_and_is_bounded_by_p` | — | engine |
| Rz. 94, 77 | Wechselkurs bei Fremdwährung | The entry's `rates` hold (base, quote, mantissa, exponent), up to 16, sorted, committed in the leaf and exported as rate rows. Data only: the engine converts nothing. `tests/golden.rs::every_record_kind_matches_the_reference_encoder`, `tests/model.rs::us3_rates_are_sorted_and_replaced_per_pair`, `tests/core_rejections.rs::row_110_too_many_rates` | Supply the rate for every foreign-currency booking; choose the rate source; do any conversion and revaluation (§ 256a HGB) yourself, as postings. | engine + integrator |
| Rz. 94, 77 | Hinreichende Erläuterung | The entry's `kind` label and its memo (the Buchungstext). The memo is committed as a salted SHA-256, so the text can later be erased without changing any hash; its text is exported while it exists. `tests/golden.rs::identity_memo_and_fingerprint_match`, `tests/model.rs::us6_memo_commitment_vector_and_erasure`, `tests/core_rejections.rs::row_108_invalid_memo` | Write a sufficient Buchungstext (the engine checks form, not content); decide when a memo may be erased (see retention below). | engine + integrator |
| Rz. 94, 77, 85 | Belegdatum | `Document::dated` — an optional date per document, committed and exported. `tests/golden.rs::every_record_kind_matches_the_reference_encoder`, `tests/export.rs::us6_every_entry_and_the_root_rebuild_from_the_rows` | Set the date on every document (it is optional in the engine). | engine + integrator |
| Rz. 94 | Buchungsdatum | Every entry has a booking date (and a value date); it must lie in a defined period. `tests/core_rejections.rs::row_300_no_period`, `tests/core_rejections.rs::row_308_value_date_drift` | — | engine |
| Rz. 94, 85; § 146(1) AO | Erfassungsdatum (soweit abweichend vom Buchungsdatum) | `Entry::captured_at` — Unix seconds and UTC offset, committed in the leaf and in the draft fingerprint (a retry with another capture time is a key conflict). The engine has **no clock**: the time is whatever the caller supplies. `tests/core_rejections.rs::retry_with_another_capture_time_is_a_key_conflict`, `tests/golden.rs::every_record_kind_matches_the_reference_encoder` | Supply `captured_at` on every entry, from a clock you control and document; check timeliness (zeitgerechte Erfassung, Rz. 45–50, 87) in your process — the engine does not. | engine records, integrator supplies |
| Rz. 94 | Autorisierung (soweit vorhanden) | Master-data records (accounts, currencies, policies) carry the `actor` set with `as_actor`. **Entries carry no approver**: no approval is committed. `tests/core_master.rs::us1_s1_setup_appends_currency_account_and_period_records`, `tests/core_master.rs::an_invalid_actor_is_refused_by_the_next_master_data_write` | Run approvals (Freigaben, Vier-Augen-Prinzip) before `record`, and keep their evidence in your system; set `as_actor` to a role or pseudonymous user id you can resolve (labels must not carry personal data). | integrator |
| Rz. 94 | Buchungsperiode / Voranmeldungszeitraum | Each booking date lies in exactly one `Period` record; the calendar is contiguous (no gaps); export rows carry the period. `tests/core_calendar.rs::us3_period_gap_is_refused`, `tests/core_rejections.rs::row_300_no_period` | Define periods that match your fiscal year; map them to the Umsatzsteuer-Voranmeldungszeitraum (monthly or quarterly) in your reporting. | engine + integrator |
| Rz. 94, 78 | Umsatzsteuersatz, Steuerschlüssel, Umsatzsteuerbetrag, Umsatzsteuerkonto, USt-IdNr., Steuernummer | No tax fields in the engine. The VAT amount is a posting to a VAT account; rate, tax key and identifiers are posting dimensions by convention; a ledger policy can make dimensions **required**, and the engine then refuses an entry without them. `tests/core_rejections.rs::row_307_missing_dimension`, `tests/export.rs::us6_a_sales_invoice_pairs_against_revenue_and_vat` | Define the dimension axes and values (e.g. `tax_key`, `vat_rate`), the VAT accounts, and the policy that requires them; keep identifiers that name a natural person out of dimensions (keep them in your master data). | integrator convention, engine enforces presence |
| Rz. 94 | Konto und Gegenkonto | n-leg entries are decomposed into deterministic Konto/Gegenkonto pairs per (currency, layer) that re-sum exactly to the entry; exported as pair rows. `tests/export.rs::us6_a_sales_invoice_pairs_against_revenue_and_vat`, `tests/export.rs::us6_exact_matches_then_the_greedy_merge`, `tests/properties.rs::export_pairs` | Map account paths to account numbers (SKR03/SKR04) in your export adapter. | engine |
| Rz. 94 | Buchungsschlüssel (soweit vorhanden) | The entry's `kind` label is validated and committed; its vocabulary is the caller's. `tests/core_rejections.rs::row_107_invalid_kind` | Define the Buchungsschlüssel vocabulary (`kind` values, or a dimension such as a BU-Schlüssel) and document it. | integrator |
| Rz. 94 (since 11.03.2024) | Kontoart (Aktiva, Passiva, Kapital, Aufwand, Ertrag, sonstige) | `AccountKind` (Asset, Liability, Equity, Income, Expense) is fixed at registration: no change record exists for it, and reopening a path with another kind is refused. Posting rows carry it. `tests/core_master.rs::account_change_has_no_variant_for_path_or_kind`, `tests/core_master.rs::us1_s3_reopening_a_path_with_another_kind_is_account_exists` | Map "sonstige" (statistical or off-balance accounts) to one of the five kinds, or keep such accounts in a separate ledger, and document the mapping. | engine |
| Rz. 94 (since 11.03.2024) | Kontotyp (Bilanz, GuV, Debitor, Kreditor, steuerlicher Gewinn / außerbilanzielle Zu- und Abrechnungen, sonstige) | The account `class` label: a dated master-data record, frozen once the account has a posting; posting rows carry the class in force on the booking date. `tests/core_master.rs::us1_s5_class_is_frozen_after_the_first_posting`, `tests/export.rs::us6_postings_carry_the_master_data_in_force_on_their_booking_date` | Choose and document the class values (e.g. `bilanz`, `guv`, `debitor`, `kreditor`). | engine stores, integrator chooses values |
| Rz. 94 | Eindeutige Identifikationsnummer des Geschäftsvorfalls (Splitbuchungen) | A split booking is **one** entry with up to 1 024 postings. Its `EntryId` is derived from the caller's idempotency key and the ledger id — unique per ledger, not only per fiscal year — and committed through the key. `tests/model.rs::us3_entry_ids_are_derived`, `tests/golden.rs::identity_memo_and_fingerprint_match` | Use the source transaction id as the idempotency key. | engine |

## Master data, Unveränderbarkeit, Festschreibung

| Norm | Requirement | Engine guarantee and evidence | Integrator duty | Scope |
|---|---|---|---|---|
| Rz. 111 (with 59, 60, 89) | Master-data changes keep the meaning of movement data ("Kontoeigenschaft"); master data historised with validity dates; the change history itself unalterable; changes to settings and parameters recorded | Accounts, currencies and the ledger policy are **log records** with `effective` and `actor`; an entry is validated against the version in force on its booking date; account kind is immutable, class is frozen after the first posting, an exponent is never lowered; a change effective at or below the sealed watermark is refused, and a policy cannot take effect on or before a booking date already recorded. The history is part of the log, so it is as unalterable as the entries. `tests/core_master.rs::us1_s6_change_at_or_below_the_watermark_is_refused`, `tests/core_master.rs::the_window_in_force_on_the_booking_date_decides`, `tests/regressions_round3.rs::a3_c7_a_policy_cannot_reach_back_over_recorded_entries`, `tests/sqlite.rs::a3_c1_c2_master_data_and_posting_dates_are_audited` | Record your own settings and parameters outside the ledger (account-number mapping, tax-key tables, adapter configuration) with a history, and keep them for the retention period. | engine + integrator |
| § 146(4) AO, § 239(3) HGB; Rz. 58–60, 107–109, 111–112 | No change that hides the original content; corrections leave original and correction recognisable | The log is append-only; nothing is updated in place. A correction is a reversal entry that must mirror its target exactly (sides swapped), or a new entry. Every leaf is in an RFC 6962 tree; `audit()` replays the log and compares every table; tampered records are refused on read. `tests/core_rejections.rs::row_403_not_a_mirror`, `tests/core_audit.rs::fixture_01_a_flipped_leaf_byte`, `tests/sqlite.rs::a3_s1_the_audit_compares_every_table`, `tests/sqlite.rs::a3_c9_swapped_entry_ids_are_refused` | Correct only by reversal or new entry; never edit the database. | engine |
| Rz. 110; PostgreSQL | Unveränderbarkeit by software and organisation | Triggers refuse `UPDATE`, `DELETE` and `TRUNCATE` on every record table; memo erasure is the one column-scoped exception, hold resolution happens once. With least-privilege roles the application role can only insert and select. `tests/postgres.rs::immutability_is_enforced_by_the_database`, `tests/postgres.rs::a3_s1_an_app_role_insert_fails_the_audit` | Run the application with the least-privilege role; keep the owner role for `create()` only; restrict and log superuser access (see [PostgreSQL](@/docs/operating/postgresql.md)). | engine + integrator |
| Rz. 110; SQLite | "Ablage in einem Dateisystem erfüllt die Anforderungen regelmäßig nicht, soweit nicht zusätzliche Maßnahmen ergriffen werden" | SQLite is a file. The triggers refuse `UPDATE` and `DELETE` (also through `REPLACE`), but anyone who can write the file can drop them. What the engine offers is **detection, not prevention**: the log, the seals, `audit()` replay and witnessed checkpoints make every edit detectable. `tests/sqlite.rs::immutability_is_enforced_by_the_database`, `tests/sqlite.rs::a3_s2_replace_and_upsert_are_refused_on_every_record_table`, `tests/sqlite.rs::tampering_fails_the_audit`, `tests/transparency.rs::us3_verifier_moves_only_forward_through_consistency` | The operator's duty: protect the file (file-system permissions, a write-once or versioned backup target), run `audit()` periodically and after every restore, and publish witnessed checkpoints to an independent party. Document these measures. | engine + integrator |
| Rz. 85, 92, 109 | Festschreibung — mandatory unless the system makes records unalterable automatically on capture; no open pre-entries or batches that can be changed unprotocolled | Every entry is unalterable **on append** — there is no draft, pre-entry or batch state in the ledger. In addition, a seal record closes a period: no entry, clearing or master-data change dated on or before the sealed watermark is accepted, and only a seal record seals. `tests/core_rejections.rs::row_301_period_sealed`, `tests/core_calendar.rs::us3_nothing_on_or_before_the_watermark`, `tests/core_calendar.rs::us3_only_a_seal_seals`, `tests/regressions_engine.rs::ef2_sealed_period_refused_and_audited` | Seal periods on a documented schedule (e.g. after the Umsatzsteuer-Voranmeldung); if your system keeps drafts before `record`, they are your pre-entries and need your own protocol. | engine + integrator |
| Rz. 100–102 | Internes Kontrollsystem: access controls, segregation of duties, input and processing controls | Input controls the engine provides: staged validation with one rejection per input (structure, idempotency, context, references, limits), size limits, balance limits, required dimensions. `tests/core_rejections.rs::row_106_unbalanced_reports_the_first_pair`, `tests/core_rejections.rs::row_304_account_not_open` | Access control, segregation of duties, approvals, reconciliation controls and their protocol; describe the IKS in the Verfahrensdokumentation. | engine + integrator |
| Rz. 103–106 | Datensicherheit: protection against loss and unauthorised change; backup procedure documented | `audit()` verifies a restored ledger completely. `tests/sqlite.rs::tampering_fails_the_audit` | Backups, their retention and restore tests; run `audit()` after every restore (see [backup and restore](@/docs/operating/backup-and-restore.md)). | integrator |

## Data access (§ 147(6) AO, Rz. 158–178)

| Norm | Requirement | Engine guarantee and evidence | Integrator duty | Scope |
|---|---|---|---|---|
| § 147(6) Satz 1 AO; Rz. 165 | Z1 — unmittelbarer Datenzugriff: read-only access with the system's own evaluation functions | Reporting functions to build on: trial balance, balances by date, position or dimension, statements, rollups, open items. `tests/reports.rs::us1_a_pinned_head_reads_that_prefix_only` | Provide the read-only user, the screens and the evaluations. | integrator (engine provides the queries) |
| § 147(6) Satz 2 AO; Rz. 166 (as of 14.07.2025) | Z2 — mittelbarer Datenzugriff: evaluations by the taxpayer on the authority's instructions, delivered machine-readable | The same queries, plus the export rows as a machine-readable base. `tests/export.rs::us6_every_store_and_page_size_exports_the_same_rows` | Run the evaluation and deliver it in a machine-readable format. | integrator |
| § 147(6) Satz 3 AO; Rz. 167–178 | Z3 — Datenüberlassung: Stamm-, Bewegungs- und Metadaten in a machine-readable format | `export` emits deterministic flat rows: one leaf row per record (so the recipient rebuilds every hash and the root and compares it with a checkpoint), entries, postings with the master data in force on the booking date, Konto/Gegenkonto pairs, documents, rates, account and currency master-data history, seals and their totals; amounts as integer minor units with their exponent. `export::verify` checks a whole export against a tree head. `tests/export.rs::us6_every_entry_and_the_root_rebuild_from_the_rows`, `tests/export.rs::us6_postings_carry_the_master_data_in_force_on_their_booking_date`, `tests/export.rs::us6_amounts_are_minor_units_of_the_currency_exponent` | Render the rows in a file format (CSV per table); add the ledger policy (required dimensions, value-date drift), which the export carries as leaf rows only, from your documentation. | engine + integrator |
| Anlage to the GoBD (Beschreibungsstandard) | GDPdU-style `index.xml` with CSV tables | No file format is in the crate. The row model maps to the field classes of DATEV EXTF, the GDPdU Beschreibungsstandard and SAF-T, shown by adapter sketches in the tests. `tests/export.rs::adapter_sketch_gdpdu_builds_a_table_row`, `tests/export.rs::adapter_sketch_datev_builds_a_buchungsstapel_line` | Write or use an adapter that renders `index.xml` and the CSV tables. | integrator (adapter) |
| § 147b AO | The BMF may prescribe uniform export interfaces by ordinance | No ordinance under § 147b AO is in force. An interface it prescribes is an adapter over the same rows. | Follow any ordinance in force. | integrator (adapter) |

## Belege (Rz. 77–82, 119, 131)

| Norm | Requirement | Engine guarantee and evidence | Integrator duty | Scope |
|---|---|---|---|---|
| Rz. 77–82 | Every booking is linked to a Beleg (Belegfunktion), and the link is protected | Up to 16 documents per entry, each with type, reference, date and the SHA-256 of the document; all committed in the leaf, so the link cannot be changed without detection, and the hash binds the exact bytes of the Beleg. `tests/golden.rs::every_record_kind_matches_the_reference_encoder`, `tests/export.rs::us6_every_entry_and_the_root_rebuild_from_the_rows` | Archive the Belege themselves (the crate stores no document content) in a system that meets the GoBD, and record their SHA-256 on the entry. | engine + integrator |
| Rz. 119, 125, 131 (as of 14.07.2025) | E-Rechnung: the structured part (XML) is the original to keep; incoming documents in the format received | `Document::with_sha256` binds the hash of whichever bytes you give it. | Hash and archive the **XML** of an e-invoice (and the PDF part only if it carries tax-relevant extra information, e.g. Buchungsvermerke); keep incoming documents in the received format. | integrator |

## Retention and erasure

| Norm | Requirement | Engine guarantee and evidence | Integrator duty | Scope |
|---|---|---|---|---|
| § 147(3), (4) AO; § 257(4), (5) HGB | Books (Bücher, Journal, Jahresabschluss) 10 years; Buchungsbelege 8 years (since 1 January 2025, Viertes Bürokratieentlastungsgesetz); credit institutions, insurers and investment firms keep Buchungsbelege 10 years (§ 257(4) HGB); periods start at the end of the calendar year and do not expire while the documents matter for an open assessment period | The crate **deletes nothing**: no record, no row, no leaf. The ledger is a book, so it is kept for 10 years from the end of the year of its last entry. `tests/sqlite.rs::immutability_is_enforced_by_the_database` | Compute retention periods (including the Ablaufhemmung); keep the database, its backups and the Verfahrensdokumentation that long; decide what happens afterwards. Deleting a ledger at the end of its retention is your operation, outside the crate. | integrator |
| GDPR Art. 5(1)(e), 17(3)(b) | Personal data is erased once its retention basis has lapsed; legal retention duties override erasure until then | The memo — the only free-text field — is committed as `SHA-256(tag ‖ salt ‖ text)`: `erase_memo` removes text and salt without changing any hash, proof or export verification; it also removes the draft fingerprint, so a guessed memo cannot be confirmed. Every other field is structured and must not carry personal data. `tests/sqlite.rs::erase_leaves_no_text_salt_or_fingerprint`, `tests/export.rs::us6_an_erased_memo_still_rebuilds`, `tests/core_rejections.rs::erasing_a_memo_turns_its_retry_into_a_key_conflict` | Decide **when** to erase: the memo is the Buchungstext and part of the books, so erase only personal data whose retention basis has lapsed (normally after the 10-year period of the books). Keep personal data out of labels, paths, dimensions, document references and keys. See [privacy](@/docs/operating/privacy.md). | engine + integrator |

## Out of scope

| Norm | Requirement | Owner |
|---|---|---|
| § 146a AO, KassenSichV; DSFinV-K | Cash registers with a certified technical security device (TSE) and the DSFinV-K export | the cash-register system; a ledger may receive its totals |
| DLS (Digitale LohnSchnittstelle) | Payroll data export | the payroll system |
| § 5b EStG (E-Bilanz) | Taxonomy-based XBRL transmission of the financial statements | tax or reporting software over the export rows |
| § 147(3) AO, Art. 17 GDPR | Deleting records after their retention period | the integrator's operations |
| § 147(6) AO, § 197(3) AO | Transmission of data to the tax authority (upload platform, media) | the integrator |
| Rz. 179–181; IDW PS 880 | Software certification | the integrator's product, examined as a whole |

## Verfahrensdokumentation (Rz. 151–155)

The integrator writes the Verfahrensdokumentation (Rz. 151); the engine
contributes to each part (Rz. 153):

| Part (Rz. 153) | What the engine contributes | What you add |
|---|---|---|
| Allgemeine Beschreibung | The model: one append-only log per ledger; every rule decided by one rule core; sealed periods; the rows of this matrix. | Your business process, organisation and the role of the ledger in it. |
| Anwenderdokumentation | The meaning of every field (this matrix, the [entries](@/docs/concepts/entries.md) page), every rejection code and what to do about it ([errors](@/docs/start/errors.md)). | Your screens, input conventions, Buchungsschlüssel and dimension vocabulary, approval workflow. |
| Technische Systemdokumentation | The facts below, the [log](@/docs/assurance/the-log.md), [proofs](@/docs/assurance/proofs.md) and [stores](@/docs/operating/stores.md) pages. | Your architecture, interfaces, adapters, configuration, and how you install and version the crate. |
| Betriebsdokumentation | What to run and when: `audit()` after restores and periodically, sealing, checkpoints and witnesses, memo erasure ([operating](@/docs/operating/_index.md)). | Your backup, restore, monitoring, access and incident procedures, and their protocols. |

Facts to copy into the technical system documentation:

| Fact | Value |
|---|---|
| Engine | `doubleentry`, the version resolved in your `Cargo.lock` |
| Log format | format tag `doubleentry/1`, committed in every ledger's genesis record (position 0) |
| Database schema | schema identifier `doubleentry/1` in the `meta` table; a database of another schema is refused, never migrated |
| Hash and tree | SHA-256; RFC 6962 Merkle tree (leaf `0x00`, node `0x01` prefixes); canonical, length-prefixed encoding, never serde; leaves ≤ 65 535 bytes |
| Transparency formats | C2SP `signed-note`, `tlog-checkpoint`, `tlog-cosignature` (Ed25519), `tlog-witness`, `tlog-policy`, `tlog-proof`, `tlog-tiles` |
| Stores | in memory (no persistence); SQLite (one file, one writer at a time, triggers; detection of tampering, not prevention); PostgreSQL (one writer per ledger by advisory lock, triggers, least-privilege roles) |
| Controls | staged validation with stable rejection codes; size and balance limits; required dimensions; period seals and the sealed watermark; `audit()` replay of every table; verification of stored records on read; signed and witnessed checkpoints |

**Programmidentität (Rz. 154).** The program in use is identified by the crate
version and the **integrator's `Cargo.lock`**: keep it under version control
with each release, for the retention period. The genesis record commits the
format tag. Seals record no engine version, because a seal is a deterministic
function of the log.

## No certification

doubleentry holds **no IDW PS 880 certificate** or other software certification;
third-party certificates do not bind the tax authority (Rz. 179–181). The engine
detects tampering on SQLite and against a database superuser, it does not
prevent it; and it commits the capture time, rate or Belegdatum it is given
without knowing whether it is true.
