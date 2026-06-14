//! PostgreSQL schema name validation for file-backed schema databases.

use crate::{LimboError, Result};

/// Validate a PostgreSQL schema name before it is used to build a filesystem path
/// for the backing schema database file.
///
/// Schema names flow into `turso-postgres-schema-<name>.db` paths, so an
/// unvalidated name (e.g. `../../etc/passwd`) would allow path traversal and
/// deletion or creation of arbitrary process-writable files.
///
/// PostgreSQL allows a much wider set of identifiers (quoted names, Unicode,
/// dots, etc.), but for the ATTACH-based schema mapping we restrict names to a
/// conservative `[A-Za-z0-9_]` set that cannot escape the target directory.
pub fn validate_schema_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(LimboError::ParseError(
            "schema name must not be empty".to_string(),
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(LimboError::ParseError(format!(
            "invalid schema name \"{name}\": only ASCII letters, digits, and underscores are allowed"
        )));
    }
    Ok(())
}
