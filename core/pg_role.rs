use crate::sync::Arc;
use crate::{Connection, LimboError, Result, Value};

/// Well-known OIDs for bootstrap roles (PostgreSQL conventions).
pub const ROLE_TURSO_OID: i64 = 10;
pub const ROLE_PG_DATABASE_OWNER_OID: i64 = 6171;

/// A PostgreSQL role row exposed through pg_roles / pg_authid / pg_user.
#[derive(Debug, Clone)]
pub struct PgRole {
    pub oid: i64,
    pub name: String,
    pub rolsuper: bool,
    pub rolinherit: bool,
    pub rolcreaterole: bool,
    pub rolcreatedb: bool,
    pub rolcanlogin: bool,
    pub rolreplication: bool,
    pub rolconnlimit: i64,
    pub rolbypassrls: bool,
}

impl PgRole {
    pub fn turso() -> Self {
        Self {
            oid: ROLE_TURSO_OID,
            name: "turso".to_string(),
            rolsuper: true,
            rolinherit: true,
            rolcreaterole: true,
            rolcreatedb: true,
            rolcanlogin: true,
            rolreplication: true,
            rolconnlimit: -1,
            rolbypassrls: true,
        }
    }

    pub fn pg_database_owner() -> Self {
        Self {
            oid: ROLE_PG_DATABASE_OWNER_OID,
            name: "pg_database_owner".to_string(),
            rolsuper: false,
            rolinherit: true,
            rolcreaterole: false,
            rolcreatedb: false,
            rolcanlogin: false,
            rolreplication: false,
            rolconnlimit: -1,
            rolbypassrls: false,
        }
    }

    fn to_pg_roles_row(&self) -> Vec<Value> {
        vec![
            Value::from_i64(self.oid),
            Value::from_text(self.name.clone()),
            Value::from_i64(i64::from(self.rolsuper)),
            Value::from_i64(i64::from(self.rolinherit)),
            Value::from_i64(i64::from(self.rolcreaterole)),
            Value::from_i64(i64::from(self.rolcreatedb)),
            Value::from_i64(i64::from(self.rolcanlogin)),
            Value::from_i64(i64::from(self.rolreplication)),
            Value::from_i64(self.rolconnlimit),
            Value::Null,
            Value::Null,
            Value::from_i64(i64::from(self.rolbypassrls)),
            Value::Null,
        ]
    }

    pub fn to_authid_row(&self) -> Vec<Value> {
        let base = self.to_pg_roles_row();
        vec![
            base[0].clone(),
            base[1].clone(),
            base[2].clone(),
            base[3].clone(),
            base[4].clone(),
            base[5].clone(),
            base[6].clone(),
            base[7].clone(),
            base[8].clone(),
            base[9].clone(),
            base[10].clone(),
            base[11].clone(),
            base[12].clone(),
        ]
    }

    pub fn to_user_row(&self) -> Vec<Value> {
        vec![
            Value::from_text(self.name.clone()),
            Value::from_i64(self.oid),
            Value::from_i64(i64::from(self.rolcreatedb)),
            Value::from_i64(i64::from(self.rolsuper)),
            Value::from_i64(i64::from(self.rolreplication)),
            Value::from_i64(i64::from(self.rolbypassrls)),
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    }
}

/// In-memory role registry for a connection (CREATE ROLE / DROP ROLE).
#[derive(Debug, Clone)]
pub struct PgRoleRegistry {
    roles: Vec<PgRole>,
    next_oid: i64,
    current_role_oid: i64,
}

impl PgRoleRegistry {
    pub fn bootstrap() -> Self {
        Self {
            roles: vec![PgRole::turso(), PgRole::pg_database_owner()],
            next_oid: 16_384,
            current_role_oid: ROLE_TURSO_OID,
        }
    }

    pub fn current_role_oid(&self) -> i64 {
        self.current_role_oid
    }

    pub fn role_by_oid(&self, oid: i64) -> Option<&PgRole> {
        self.roles.iter().find(|r| r.oid == oid)
    }

    pub fn role_by_name(&self, name: &str) -> Option<&PgRole> {
        let lower = name.to_lowercase();
        self.roles
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(&lower))
    }

    pub fn create_role(&mut self, name: &str) -> Result<()> {
        let name = normalize_role_name(name)?;
        if self.role_by_name(&name).is_some() {
            return Err(LimboError::ParseError(format!(
                "role \"{name}\" already exists"
            )));
        }
        let oid = self.next_oid;
        self.next_oid += 1;
        self.roles.push(PgRole {
            oid,
            name,
            rolsuper: false,
            rolinherit: true,
            rolcreaterole: false,
            rolcreatedb: false,
            rolcanlogin: true,
            rolreplication: false,
            rolconnlimit: -1,
            rolbypassrls: false,
        });
        Ok(())
    }

    pub fn drop_role(&mut self, name: &str, missing_ok: bool) -> Result<()> {
        let name = normalize_role_name(name)?;
        if name == "turso" {
            return Err(LimboError::ParseError(
                "role \"turso\" cannot be dropped".to_string(),
            ));
        }
        let Some(pos) = self
            .roles
            .iter()
            .position(|r| r.name.eq_ignore_ascii_case(&name))
        else {
            if missing_ok {
                return Ok(());
            }
            return Err(LimboError::ParseError(format!(
                "role \"{name}\" does not exist"
            )));
        };
        let dropped_oid = self.roles[pos].oid;
        self.roles.remove(pos);
        if self.current_role_oid == dropped_oid {
            self.current_role_oid = ROLE_TURSO_OID;
        }
        Ok(())
    }

    pub fn pg_roles_rows(&self) -> Vec<Vec<Value>> {
        self.roles.iter().map(PgRole::to_pg_roles_row).collect()
    }

    pub fn pg_authid_rows(&self) -> Vec<Vec<Value>> {
        self.roles.iter().map(PgRole::to_authid_row).collect()
    }

    pub fn pg_user_rows(&self) -> Vec<Vec<Value>> {
        self.roles
            .iter()
            .filter(|r| r.rolcanlogin)
            .map(PgRole::to_user_row)
            .collect()
    }
}

fn normalize_role_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    let is_quoted = (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''));
    let unquoted = if is_quoted {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    if unquoted.is_empty() {
        return Err(LimboError::ParseError(
            "role name cannot be empty".to_string(),
        ));
    }
    if !unquoted
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(LimboError::ParseError(format!(
            "invalid role name \"{unquoted}\""
        )));
    }
    // `name` always arrives already identifier-folded by libpg_query (which strips
    // surrounding quotes itself before we ever see the string): unquoted role names
    // are already lowercased, quoted role names already keep their original case.
    // The quote-stripping above is defensive only; it is not reachable via the
    // CREATE ROLE / DROP ROLE call paths today. Re-lowercasing here regardless of
    // `is_quoted` used to clobber the case of quoted role names (e.g. `CREATE ROLE
    // "Alice"` was silently stored as `alice`), so we must not fold again.
    Ok(unquoted.to_string())
}

pub fn exec_pg_get_user_by_id(conn: &Connection, oid: i64) -> Value {
    let registry = conn.pg_roles.read();
    registry
        .role_by_oid(oid)
        .map(|r| Value::from_text(r.name.clone()))
        .unwrap_or(Value::Null)
}

pub fn pg_role_registry_rows(conn: &Arc<Connection>, kind: PgRoleCatalogKind) -> Vec<Vec<Value>> {
    let registry = conn.pg_roles.read();
    match kind {
        PgRoleCatalogKind::Roles => registry.pg_roles_rows(),
        PgRoleCatalogKind::Authid => registry.pg_authid_rows(),
        PgRoleCatalogKind::User => registry.pg_user_rows(),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum PgRoleCatalogKind {
    Roles,
    Authid,
    User,
}
