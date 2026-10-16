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
}
