//! The schema: one DDL template rendered per dialect, and the table lists
//! the triggers are generated from.

use super::dialect::{Tokens, Triggers};

/// The DDL template.
const TEMPLATE: &str = include_str!("schema.sql");

/// Tables that refuse every `UPDATE` and `DELETE`: the log and its indexes.
pub(crate) const APPEND_ONLY: [&str; 16] = [
    "meta",
    "leaves",
    "nodes",
    "accounts",
    "account_versions",
    "currency_versions",
    "policy_versions",
    "periods",
    "postings",
    "posting_dims",
    "seals",
    "seal_leaves",
    "seal_nodes",
    "clearings",
    "clearing_resets",
    "clearing_refs",
];

/// Maintained values: updated by the append transaction, never deleted.
pub(crate) const MAINTAINED: [&str; 4] = ["balances", "cleared", "outstanding", "slices"];

/// Tables with one column-scoped exception each: memos (erasure), holds
/// (resolve once), entries (fingerprint erasure); their triggers are
/// written out per dialect.
pub(crate) const SCOPED: [&str; 3] = ["memos", "holds", "entries"];

/// The columns the write path updates in each column-scoped table — the
/// one change its trigger lets through: a memo's erasure, a hold's
/// resolution, a fingerprint's erasure. A runtime role is granted `UPDATE`
/// on exactly these.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) const SCOPED_UPDATES: [(&str, &[&str]); 3] = [
    ("memos", &["salt", "text"]),
    ("holds", &["resolved_by", "how"]),
    ("entries", &["fingerprint"]),
];

/// The statements that give `role` exactly what a store needs after
/// `create`, in schema `schema` (both already validated identifiers): first
/// every privilege revoked, so applying the list again leaves exactly these;
/// then `USAGE` on the schema, `SELECT, INSERT` on every table, `UPDATE` on
/// the maintained tables and on the scoped columns. No `DELETE`, `TRUNCATE`,
/// `REFERENCES`, `TRIGGER` or DDL. Generated from the table lists the DDL
/// and the triggers come from, so a new table is never missed.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) fn runtime_grants(schema: &str, role: &str) -> Vec<String> {
    let (s, r) = (format!("\"{schema}\""), format!("\"{role}\""));
    let mut grants = vec![
        format!("REVOKE ALL ON SCHEMA {s} FROM {r}"),
        format!("REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM {r}"),
        format!("GRANT USAGE ON SCHEMA {s} TO {r}"),
    ];
    for table in all_tables() {
        grants.push(format!("GRANT SELECT, INSERT ON {s}.{table} TO {r}"));
    }
    for table in MAINTAINED {
        grants.push(format!("GRANT UPDATE ON {s}.{table} TO {r}"));
    }
    for (table, columns) in SCOPED_UPDATES {
        grants.push(format!(
            "GRANT UPDATE ({}) ON {s}.{table} TO {r}",
            columns.join(", ")
        ));
    }
    grants
}

/// The keys (primary and unique) of every append-only and column-scoped
/// table: an insert repeating one is refused (SQLite `REPLACE`). Not
/// `clearing_refs`, whose key is all its columns — a `REPLACE` there can only
/// put a row back as it was — and which the write path inserts with
/// `ON CONFLICT DO NOTHING`.
pub(crate) const KEYS: [(&str, &[&[&str]]); 18] = [
    ("meta", &[&["singleton"]]),
    ("leaves", &[&["position"]]),
    ("nodes", &[&["id"]]),
    ("accounts", &[&["handle"], &["path"], &["position"]]),
    ("account_versions", &[&["handle", "field", "position"]]),
    ("currency_versions", &[&["code", "position"], &["position"]]),
    ("policy_versions", &[&["position"]]),
    (
        "periods",
        &[&["period_id"], &["first_day"], &["last_day"], &["position"]],
    ),
    ("postings", &[&["position", "idx"]]),
    ("posting_dims", &[&["position", "idx", "axis"]]),
    ("seals", &[&["period_id"], &["position"]]),
    (
        "seal_leaves",
        &[&["period_id", "currency", "layer", "handle"]],
    ),
    (
        "seal_nodes",
        &[&["period_id", "currency", "layer", "level", "idx"]],
    ),
    ("clearings", &[&["position"], &["idem_key"]]),
    ("clearing_resets", &[&["clearing"], &["position"]]),
    ("memos", &[&["position"]]),
    ("holds", &[&["position"], &["entry_id"], &["resolved_by"]]),
    (
        "entries",
        &[
            &["position"],
            &["ordinal"],
            &["entry_id"],
            &["idem_key"],
            &["reverses"],
        ],
    ),
];

/// Every table.
pub(crate) fn all_tables() -> impl Iterator<Item = &'static str> {
    APPEND_ONLY.into_iter().chain(MAINTAINED).chain(SCOPED)
}

/// The template with `{{s}}` replaced by `qualifier` and the type tokens by
/// the dialect's.
pub(crate) fn render(template: &str, qualifier: &str, tokens: Tokens) -> String {
    template
        .replace("{{s}}", qualifier)
        .replace("{{int}}", tokens.int)
        .replace("{{bytes}}", tokens.bytes)
        .replace("{{text}}", tokens.text)
        .replace("{{len}}", tokens.len)
        .replace("{{strict}}", tokens.strict)
}

/// The triggers, rendered from the dialect's templates and the table lists.
pub(crate) fn triggers(qualifier: &str, triggers: Triggers) -> String {
    let lists = [
        (triggers.append_only, &APPEND_ONLY[..]),
        (triggers.maintained, &MAINTAINED[..]),
    ];
    let mut ddl = triggers.scoped.replace("{{s}}", qualifier);
    for (template, tables) in lists {
        for table in tables {
            ddl.push_str(&template.replace("{{t}}", table).replace("{{s}}", qualifier));
        }
    }
    for table in all_tables() {
        ddl.push_str(
            &triggers
                .every_table
                .replace("{{t}}", table)
                .replace("{{s}}", qualifier),
        );
    }
    if !triggers.no_replace.is_empty() {
        for (table, keys) in KEYS {
            let condition = keys
                .iter()
                .map(|columns| {
                    let equal: Vec<String> = columns
                        .iter()
                        .map(|column| format!("o.{column} = NEW.{column}"))
                        .collect();
                    format!("({})", equal.join(" AND "))
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            ddl.push_str(
                &triggers
                    .no_replace
                    .replace("{{t}}", table)
                    .replace("{{cond}}", &condition)
                    .replace("{{s}}", qualifier),
            );
        }
    }
    ddl
}

/// The tables of the schema, rendered.
pub(crate) fn tables(qualifier: &str, tokens: Tokens) -> String {
    render(TEMPLATE, qualifier, tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_cover_every_table_of_the_template_once() {
        let mut created: Vec<&str> = TEMPLATE
            .lines()
            .filter_map(|line| line.strip_prefix("CREATE TABLE {{s}}"))
            .filter_map(|rest| rest.split_whitespace().next())
            .collect();
        let mut listed: Vec<&str> = all_tables().collect();
        created.sort_unstable();
        listed.sort_unstable();
        assert_eq!(created.len(), 23);
        assert_eq!(created, listed);
        // Every append-only and column-scoped table has its keys listed.
        let mut keyed: Vec<&str> = KEYS.iter().map(|(table, _)| *table).collect();
        let mut kept: Vec<&str> = APPEND_ONLY
            .into_iter()
            .chain(SCOPED)
            .filter(|table| *table != "clearing_refs")
            .collect();
        keyed.sort_unstable();
        kept.sort_unstable();
        assert_eq!(keyed, kept);
    }

    /// The columns of `table` in the template.
    fn columns(table: &str) -> Vec<String> {
        let start = format!("CREATE TABLE {{{{s}}}}{table} (");
        let body = TEMPLATE.split(&start).nth(1).unwrap_or_default();
        let body = body.split("){{strict}};").next().unwrap_or_default();
        body.lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|word| word.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn every_scoped_table_names_its_updatable_columns() {
        let mut named: Vec<&str> = SCOPED_UPDATES.iter().map(|(table, _)| *table).collect();
        let mut scoped = SCOPED.to_vec();
        named.sort_unstable();
        scoped.sort_unstable();
        assert_eq!(named, scoped);
        for (table, updated) in SCOPED_UPDATES {
            let columns = columns(table);
            for column in updated {
                assert!(columns.iter().any(|c| c == column), "{table}.{column}");
            }
        }
    }

    #[test]
    fn the_runtime_grants_cover_every_table_and_nothing_more() {
        let grants = runtime_grants("ledger", "app");
        assert_eq!(
            grants.first().map(String::as_str),
            Some("REVOKE ALL ON SCHEMA \"ledger\" FROM \"app\"")
        );
        for table in all_tables() {
            assert!(
                grants.contains(&format!(
                    "GRANT SELECT, INSERT ON \"ledger\".{table} TO \"app\""
                )),
                "{table}"
            );
        }
        let updates = grants
            .iter()
            .filter(|g| g.starts_with("GRANT UPDATE"))
            .count();
        assert_eq!(updates, MAINTAINED.len() + SCOPED_UPDATES.len());
        assert!(
            grants
                .iter()
                .all(|g| !g.contains("DELETE") && !g.contains("TRUNCATE"))
        );
        assert_eq!(grants.len(), 3 + all_tables().count() + updates);
    }
}
