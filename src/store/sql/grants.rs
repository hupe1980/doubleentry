//! The least-privilege grants of a PostgreSQL runtime role, generated from
//! the schema's table catalogue.

use super::dialect::Mode;
use super::{Postgres, SqlStore};
use crate::store::StoreError;

/// Whether `name` is an identifier this crate puts in SQL:
/// `[A-Za-z_][A-Za-z0-9_]{0,62}` — nothing a statement template gives a
/// meaning (`$n`, `{{…}}`) and nothing that needs escaping; always quoted,
/// so its case is kept.
pub(super) fn identifier(name: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let leading_digit = name.starts_with(|c: char| c.is_ascii_digit());
    !name.is_empty() && !leading_digit && name.chars().all(word) && name.len() <= 63
}

impl Postgres {
    /// The statements that give `role` exactly the privileges a store
    /// needs after `create` in `schema`, in order: every privilege of the
    /// role on the schema and its tables revoked (so applying the list
    /// again leaves exactly these), `USAGE` on the schema, `SELECT, INSERT`
    /// on every table, `UPDATE` on the maintained tables (balances, cleared
    /// amounts, outstanding holds, closing slices) and on the columns the
    /// write path changes (memo erasure, hold resolution, fingerprint
    /// erasure). Pure: nothing connects — for a migration tool or a DBA.
    ///
    /// # Errors
    ///
    /// `InvalidSchemaName` or `InvalidRoleName` for a name that is not
    /// `[A-Za-z_][A-Za-z0-9_]{0,62}`.
    pub fn runtime_grants(schema: &str, role: &str) -> Result<Vec<String>, StoreError> {
        if !identifier(schema) {
            return Err(StoreError::invalid_schema_name(schema));
        }
        if !identifier(role) {
            return Err(StoreError::invalid_role_name(role));
        }
        Ok(super::schema::runtime_grants(schema, role))
    }
}

impl<const P: u8> SqlStore<Postgres, P> {
    /// [`Postgres::runtime_grants`] for this store's schema.
    ///
    /// # Errors
    ///
    /// `InvalidRoleName` for a role that is not `[A-Za-z_][A-Za-z0-9_]{0,62}`.
    pub fn runtime_grants(&self, role: &str) -> Result<Vec<String>, StoreError> {
        Postgres::runtime_grants(&self.inner.schema, role)
    }

    /// Applies [`runtime_grants`](Self::runtime_grants) for `role` in one
    /// transaction. Run it on a store connected as the schema's owner,
    /// after `create` (`Ledger::new`); the role must exist.
    ///
    /// # Errors
    ///
    /// `InvalidRoleName` before anything runs; `Backend` when a statement
    /// fails (an unknown role, a connection without the owner's rights) —
    /// then none of them applied.
    pub async fn grant_runtime(&self, role: &str) -> Result<(), StoreError> {
        let grants = self.runtime_grants(role)?;
        let mut session = self.session(Mode::Write, "grant").await?;
        let mut script = grants.join(";\n");
        script.push_str(";\n");
        session.script(script).await?;
        session.commit().await
    }
}
