-- SQLite: the column-scoped triggers. The per-table
-- append-only and no-delete triggers are generated from the table lists;
-- every connection runs with recursive_triggers on, so a REPLACE's implicit
-- delete fires them too. A fingerprint is nulled only together with the
-- erasure of its entry's memo (erase_memo erases the memo first).
CREATE TRIGGER memos_erase_only BEFORE UPDATE ON memos
  WHEN NEW.position IS NOT OLD.position OR NEW.salt IS NOT NULL OR NEW.text IS NOT NULL
  BEGIN SELECT RAISE(ABORT, 'doubleentry: UPDATE on memos refused (erase only)'); END;
CREATE TRIGGER memos_no_delete BEFORE DELETE ON memos
  BEGIN SELECT RAISE(ABORT, 'doubleentry: DELETE on memos refused (erase only)'); END;
CREATE TRIGGER entries_fingerprint_erase_only BEFORE UPDATE ON entries
  WHEN OLD.fingerprint IS NULL OR NEW.fingerprint IS NOT NULL
    OR NEW.position IS NOT OLD.position OR NEW.ordinal IS NOT OLD.ordinal
    OR NEW.entry_id IS NOT OLD.entry_id OR NEW.idem_key IS NOT OLD.idem_key
    OR NEW.reverses IS NOT OLD.reverses
    OR NOT EXISTS (SELECT 1 FROM memos m WHERE m.position = OLD.position AND m.text IS NULL)
  BEGIN SELECT RAISE(ABORT, 'doubleentry: UPDATE on entries refused (fingerprint erase only)'); END;
CREATE TRIGGER entries_no_delete BEFORE DELETE ON entries
  BEGIN SELECT RAISE(ABORT, 'doubleentry: DELETE on entries refused (append-only)'); END;
CREATE TRIGGER holds_resolve_once BEFORE UPDATE ON holds
  WHEN OLD.resolved_by IS NOT NULL OR NEW.resolved_by IS NULL
    OR NEW.position IS NOT OLD.position OR NEW.entry_id IS NOT OLD.entry_id
    OR NEW.booking IS NOT OLD.booking OR NEW.expires IS NOT OLD.expires
    OR NEW.debit_handle IS NOT OLD.debit_handle OR NEW.credit_handle IS NOT OLD.credit_handle
    OR NEW.currency IS NOT OLD.currency OR NEW.amount IS NOT OLD.amount
  BEGIN SELECT RAISE(ABORT, 'doubleentry: UPDATE on holds refused (resolve once)'); END;
CREATE TRIGGER holds_no_delete BEFORE DELETE ON holds
  BEGIN SELECT RAISE(ABORT, 'doubleentry: DELETE on holds refused (resolve once)'); END;
