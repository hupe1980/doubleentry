-- PostgreSQL: the trigger functions and the column-scoped triggers; {{s}}
-- is the schema qualifier. The per-table append-only, no-delete and
-- no-truncate triggers are generated from the table lists.
CREATE FUNCTION {{s}}refuse_change() RETURNS trigger LANGUAGE plpgsql AS $f$
BEGIN
  RAISE EXCEPTION 'doubleentry: % on %.% refused (append-only)', TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME
    USING ERRCODE = 'P0001';
END $f$;
CREATE FUNCTION {{s}}memo_erase_only() RETURNS trigger LANGUAGE plpgsql AS $f$
BEGIN
  IF TG_OP <> 'UPDATE' OR NEW.position IS DISTINCT FROM OLD.position
     OR NEW.salt IS NOT NULL OR NEW.text IS NOT NULL THEN
    RAISE EXCEPTION 'doubleentry: % on %.memos refused (erase only)', TG_OP, TG_TABLE_SCHEMA
      USING ERRCODE = 'P0001';
  END IF;
  RETURN NEW;
END $f$;
CREATE FUNCTION {{s}}hold_resolve_once() RETURNS trigger LANGUAGE plpgsql AS $f$
BEGIN
  IF TG_OP <> 'UPDATE' OR OLD.resolved_by IS NOT NULL OR NEW.resolved_by IS NULL
     OR (NEW.position, NEW.entry_id, NEW.booking, NEW.expires, NEW.debit_handle,
         NEW.credit_handle, NEW.currency, NEW.amount)
        IS DISTINCT FROM
        (OLD.position, OLD.entry_id, OLD.booking, OLD.expires, OLD.debit_handle,
         OLD.credit_handle, OLD.currency, OLD.amount) THEN
    RAISE EXCEPTION 'doubleentry: % on %.holds refused (resolve once)', TG_OP, TG_TABLE_SCHEMA
      USING ERRCODE = 'P0001';
  END IF;
  RETURN NEW;
END $f$;
CREATE FUNCTION {{s}}fingerprint_erase_only() RETURNS trigger LANGUAGE plpgsql AS $f$
BEGIN
  IF TG_OP <> 'UPDATE' OR OLD.fingerprint IS NULL OR NEW.fingerprint IS NOT NULL
     OR (NEW.position, NEW.ordinal, NEW.entry_id, NEW.idem_key, NEW.reverses)
        IS DISTINCT FROM (OLD.position, OLD.ordinal, OLD.entry_id, OLD.idem_key, OLD.reverses)
     -- Only together with the erasure of the entry's memo (erase_memo
     -- erases the memo first, in the same transaction).
     OR NOT EXISTS (SELECT 1 FROM {{s}}memos m WHERE m.position = OLD.position AND m.text IS NULL) THEN
    RAISE EXCEPTION 'doubleentry: % on %.entries refused (fingerprint erase only)', TG_OP, TG_TABLE_SCHEMA
      USING ERRCODE = 'P0001';
  END IF;
  RETURN NEW;
END $f$;
CREATE TRIGGER entries_fingerprint_erase_only BEFORE UPDATE OR DELETE ON {{s}}entries
  FOR EACH ROW EXECUTE FUNCTION {{s}}fingerprint_erase_only();
CREATE TRIGGER memos_erase_only BEFORE UPDATE OR DELETE ON {{s}}memos
  FOR EACH ROW EXECUTE FUNCTION {{s}}memo_erase_only();
CREATE TRIGGER holds_resolve_once BEFORE UPDATE OR DELETE ON {{s}}holds
  FOR EACH ROW EXECUTE FUNCTION {{s}}hold_resolve_once();
