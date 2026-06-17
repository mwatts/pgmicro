use std::collections::HashMap;

use crate::schema::{Schema, Table};
use crate::sync::Arc;
use crate::{Connection, LimboError, Result, Value};
use turso_parser_pg::translator::{PgCommentStmt, PgCommentTarget};

/// PostgreSQL catalog class OIDs for `pg_description.classoid`.
pub const PG_CLASS_CLASSOID: i64 = 1259;
pub const PG_TYPE_CLASSOID: i64 = 1247;

const USER_TABLE_OID_START: i64 = 16384;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PgDescriptionKey {
    pub objoid: i64,
    pub classoid: i64,
    pub objsubid: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PgCommentRegistry {
    comments: HashMap<PgDescriptionKey, String>,
}

impl PgCommentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_comment(&mut self, key: PgDescriptionKey, comment: String) {
        if comment.is_empty() {
            self.comments.remove(&key);
        } else {
            self.comments.insert(key, comment);
        }
    }

    pub fn rows(&self) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = self
            .comments
            .iter()
            .map(|(key, text)| {
                vec![
                    Value::from_i64(key.objoid),
                    Value::from_i64(key.classoid),
                    Value::from_i64(key.objsubid),
                    Value::from_text(text.clone()),
                ]
            })
            .collect();
        rows.sort_by(|a, b| {
            let a0 = a[0].as_int().unwrap_or(0);
            let b0 = b[0].as_int().unwrap_or(0);
            a0.cmp(&b0)
                .then_with(|| a[1].as_int().unwrap_or(0).cmp(&b[1].as_int().unwrap_or(0)))
                .then_with(|| a[2].as_int().unwrap_or(0).cmp(&b[2].as_int().unwrap_or(0)))
        });
        rows
    }
}

fn user_tables_sorted(schema: &Schema) -> Vec<(&String, &Arc<Table>)> {
    let mut tables: Vec<_> = schema
        .tables
        .iter()
        .filter(|(name, table)| {
            if name.starts_with("sqlite_")
                || name.starts_with("pg_")
                || name.starts_with("pragma_")
                || name.starts_with("json_")
            {
                return false;
            }
            matches!(table.as_ref(), Table::BTree(_))
        })
        .collect();
    tables.sort_by(|a, b| a.0.cmp(b.0));
    tables
}

fn table_oid_map(schema: &Schema) -> HashMap<String, i64> {
    user_tables_sorted(schema)
        .into_iter()
        .enumerate()
        .map(|(i, (name, _))| (name.to_lowercase(), USER_TABLE_OID_START + i as i64))
        .collect()
}

fn resolve_table_oid(conn: &Connection, table_name: &str) -> Result<i64> {
    let schema = conn.schema.read();
    let map = table_oid_map(&schema);
    map.get(&table_name.to_lowercase())
        .copied()
        .ok_or_else(|| LimboError::ParseError(format!("relation \"{table_name}\" does not exist")))
}

fn resolve_column_subid(conn: &Connection, table_name: &str, column_name: &str) -> Result<i64> {
    let schema = conn.schema.read();
    let tables = user_tables_sorted(&schema);
    let table = tables
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(table_name))
        .map(|(_, t)| *t)
        .ok_or_else(|| {
            LimboError::ParseError(format!("relation \"{table_name}\" does not exist"))
        })?;
    let Table::BTree(btree) = table.as_ref() else {
        return Err(LimboError::ParseError(format!(
            "relation \"{table_name}\" does not exist"
        )));
    };
    for (i, col) in btree.columns().iter().enumerate() {
        if col
            .name
            .as_ref()
            .is_some_and(|n| n.eq_ignore_ascii_case(column_name))
        {
            return Ok((i + 1) as i64);
        }
    }
    Err(LimboError::ParseError(format!(
        "column \"{column_name}\" of relation \"{table_name}\" does not exist"
    )))
}

fn resolve_type_oid(conn: &Connection, type_name: &str) -> Result<i64> {
    let schema = conn.schema.read();
    for (_, table) in user_tables_sorted(&schema) {
        let Table::BTree(btree) = table.as_ref() else {
            continue;
        };
        for col in btree.columns() {
            if col.ty_str.eq_ignore_ascii_case(type_name) {
                return Ok(crate::pg_catalog::sqlite_type_to_pg_oid(&col.ty_str));
            }
        }
    }
    Ok(crate::pg_catalog::sqlite_type_to_pg_oid(type_name))
}

pub fn apply_comment_stmt(conn: &Connection, stmt: &PgCommentStmt) -> Result<()> {
    let key = match &stmt.target {
        PgCommentTarget::Table { name } => PgDescriptionKey {
            objoid: resolve_table_oid(conn, name)?,
            classoid: PG_CLASS_CLASSOID,
            objsubid: 0,
        },
        PgCommentTarget::Column { table, column } => PgDescriptionKey {
            objoid: resolve_table_oid(conn, table)?,
            classoid: PG_CLASS_CLASSOID,
            objsubid: resolve_column_subid(conn, table, column)?,
        },
        PgCommentTarget::Type { name } => PgDescriptionKey {
            objoid: resolve_type_oid(conn, name)?,
            classoid: PG_TYPE_CLASSOID,
            objsubid: 0,
        },
    };

    conn.pg_comments
        .write()
        .set_comment(key, stmt.comment.clone());
    Ok(())
}

pub fn pg_description_rows(conn: &Arc<Connection>) -> Vec<Vec<Value>> {
    conn.pg_comments.read().rows()
}
