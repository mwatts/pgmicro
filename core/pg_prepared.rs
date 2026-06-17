use std::collections::HashMap;

use crate::{LimboError, Result};

#[derive(Debug, Clone, Default)]
pub struct PgPreparedStatementRegistry {
    statements: HashMap<String, String>,
}

impl PgPreparedStatementRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prepare(&mut self, name: &str, sql: &str) -> Result<()> {
        let name = normalize_name(name)?;
        self.statements.insert(name, sql.to_string());
        Ok(())
    }

    pub fn deallocate(&mut self, name: &str) -> Result<()> {
        let name = normalize_name(name)?;
        if self.statements.remove(&name).is_none() {
            return Err(LimboError::ParseError(format!(
                "prepared statement \"{name}\" does not exist"
            )));
        }
        Ok(())
    }

    pub fn deallocate_all(&mut self) {
        self.statements.clear();
    }

    pub fn get(&self, name: &str) -> Result<String> {
        let name = normalize_name(name)?;
        self.statements.get(&name).cloned().ok_or_else(|| {
            LimboError::ParseError(format!("prepared statement \"{name}\" does not exist"))
        })
    }
}

fn normalize_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(LimboError::ParseError(
            "prepared statement name cannot be empty".to_string(),
        ));
    }
    Ok(trimmed.to_lowercase())
}
