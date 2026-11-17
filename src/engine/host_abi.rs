use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::HostAbiError;

/// ABI version tag matching `nosrc:runtime/host@0.1.0`
pub const HOST_ABI_VERSION: &str = "0.1.0";

/// Error code constants returned across the Wasm Host ABI boundary
pub const ABI_OK: i32 = 0;
pub const ERR_GENERIC: i32 = -1;
pub const ERR_CAPABILITY_DISABLED: i32 = -2;
pub const ERR_MEMORY_ACCESS: i32 = -3;
pub const ERR_MALFORMED_JSON: i32 = -4;
pub const ERR_SSRF_BLOCKED: i32 = -5;
pub const ERR_EGRESS_FORBIDDEN: i32 = -6;
pub const ERR_DB_SQL: i32 = -7;
pub const ERR_KV_STORE: i32 = -8;
pub const ERR_NOT_FOUND: i32 = -9;

/// Table definition for embedded KV store
pub const KV_TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("nosrc_kv");

/// State mode toggle: embedded (SQLite + redb) vs external (Postgres/Redis/disabled)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateMode {
    Embedded,
    External,
}

/// Dynamic capabilities granted to a Wasm guest execution instance.
#[derive(Clone)]
pub struct HostCapabilities {
    pub state_mode: StateMode,
    pub allowlisted_domains: HashSet<String>,
    pub embedded_db: Option<Arc<AsyncMutex<rusqlite::Connection>>>,
    pub embedded_kv: Option<Arc<redb::Database>>,
    pub http_client: reqwest::Client,
}

impl HostCapabilities {
    pub fn new_embedded(
        db: rusqlite::Connection,
        kv: Option<Arc<redb::Database>>,
        allowlisted_domains: HashSet<String>,
    ) -> Self {
        Self {
            state_mode: StateMode::Embedded,
            allowlisted_domains,
            embedded_db: Some(Arc::new(AsyncMutex::new(db))),
            embedded_kv: kv,
            http_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn new_external(allowlisted_domains: HashSet<String>) -> Self {
        Self {
            state_mode: StateMode::External,
            allowlisted_domains,
            embedded_db: None,
            embedded_kv: None,
            http_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_default(),
        }
    }

    // ------------------------------------------------------------------------
    // Key-Value Store Operations (redb)
    // ------------------------------------------------------------------------

    pub fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, HostAbiError> {
        if self.state_mode == StateMode::External {
            return Err(HostAbiError::CapabilityDisabled(
                "Embedded KV store disabled in external mode".into(),
            ));
        }

        let kv = self
            .embedded_kv
            .as_ref()
            .ok_or_else(|| HostAbiError::KvError("Embedded KV database not initialized".into()))?;

        let read_txn = kv
            .begin_read()
            .map_err(|e| HostAbiError::KvError(e.to_string()))?;

        let table = match read_txn.open_table(KV_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(HostAbiError::KvError(e.to_string())),
        };

        let result = table
            .get(key)
            .map_err(|e| HostAbiError::KvError(e.to_string()))?;

        Ok(result.map(|access| access.value().to_vec()))
    }

    pub fn kv_set(&self, key: &str, value: &[u8]) -> Result<(), HostAbiError> {
        if self.state_mode == StateMode::External {
            return Err(HostAbiError::CapabilityDisabled(
                "Embedded KV store disabled in external mode".into(),
            ));
        }

        let kv = self
            .embedded_kv
            .as_ref()
            .ok_or_else(|| HostAbiError::KvError("Embedded KV database not initialized".into()))?;

        let write_txn = kv
            .begin_write()
            .map_err(|e| HostAbiError::KvError(e.to_string()))?;

        {
            let mut table = write_txn
                .open_table(KV_TABLE)
                .map_err(|e| HostAbiError::KvError(e.to_string()))?;

            table
                .insert(key, value)
                .map_err(|e| HostAbiError::KvError(e.to_string()))?;
        }

        write_txn
            .commit()
            .map_err(|e| HostAbiError::KvError(e.to_string()))?;

        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<bool, HostAbiError> {
        if self.state_mode == StateMode::External {
            return Err(HostAbiError::CapabilityDisabled(
                "Embedded KV store disabled in external mode".into(),
            ));
        }

        let kv = self
            .embedded_kv
            .as_ref()
            .ok_or_else(|| HostAbiError::KvError("Embedded KV database not initialized".into()))?;

        let write_txn = kv
            .begin_write()
            .map_err(|e| HostAbiError::KvError(e.to_string()))?;

        let existed = {
            let mut table = match write_txn.open_table(KV_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
                Err(e) => return Err(HostAbiError::KvError(e.to_string())),
            };

            let prev = table
                .remove(key)
                .map_err(|e| HostAbiError::KvError(e.to_string()))?;

            prev.is_some()
        };

        write_txn
            .commit()
            .map_err(|e| HostAbiError::KvError(e.to_string()))?;

        Ok(existed)
    }

    // ------------------------------------------------------------------------
    // Relational Database Operations (rusqlite)
    // ------------------------------------------------------------------------

    /// Executes a parameterized query that returns rows as a JSON array of objects.
    /// Strictly rejects raw multi-statement strings to prevent SQL injection.
    pub async fn db_query(
        &self,
        sql: &str,
        params_json: &serde_json::Value,
    ) -> Result<serde_json::Value, HostAbiError> {
        if self.state_mode == StateMode::External {
            return Err(HostAbiError::CapabilityDisabled(
                "Embedded SQL database disabled in external mode".into(),
            ));
        }

        validate_single_sql_statement(sql)?;

        let db_arc = self.embedded_db.as_ref().ok_or_else(|| {
            HostAbiError::DatabaseError("Embedded database not initialized".into())
        })?;

        let db_lock = db_arc.lock().await;

        let mut stmt = db_lock
            .prepare(sql)
            .map_err(|e| HostAbiError::DatabaseError(e.to_string()))?;

        let column_names: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let params_vec = json_to_rusqlite_params(params_json)?;
        let rusqlite_params: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let mut rows = stmt
            .query(rusqlite_params.as_slice())
            .map_err(|e| HostAbiError::DatabaseError(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| HostAbiError::DatabaseError(e.to_string()))?
        {
            let mut obj = serde_json::Map::new();
            for (idx, name) in column_names.iter().enumerate() {
                let val: rusqlite::types::Value = row
                    .get(idx)
                    .map_err(|e| HostAbiError::DatabaseError(e.to_string()))?;
                let json_val = match val {
                    rusqlite::types::Value::Null => serde_json::Value::Null,
                    rusqlite::types::Value::Integer(i) => serde_json::Value::from(i),
                    rusqlite::types::Value::Real(f) => serde_json::Value::from(f),
                    rusqlite::types::Value::Text(t) => serde_json::Value::from(t),
                    rusqlite::types::Value::Blob(b) => serde_json::Value::from(b),
                };
                obj.insert(name.clone(), json_val);
            }
            results.push(serde_json::Value::Object(obj));
        }

        Ok(serde_json::Value::Array(results))
    }

    /// Executes a parameterized statement (INSERT/UPDATE/DELETE/DDL) and returns affected row count.
    pub async fn db_execute(
        &self,
        sql: &str,
        params_json: &serde_json::Value,
    ) -> Result<usize, HostAbiError> {
        if self.state_mode == StateMode::External {
            return Err(HostAbiError::CapabilityDisabled(
                "Embedded SQL database disabled in external mode".into(),
            ));
        }

        validate_single_sql_statement(sql)?;

        let db_arc = self.embedded_db.as_ref().ok_or_else(|| {
            HostAbiError::DatabaseError("Embedded database not initialized".into())
        })?;

        let db_lock = db_arc.lock().await;

        let params_vec = json_to_rusqlite_params(params_json)?;
        let rusqlite_params: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let affected = db_lock
            .execute(sql, rusqlite_params.as_slice())
            .map_err(|e| HostAbiError::DatabaseError(e.to_string()))?;

        Ok(affected)
    }

    // ------------------------------------------------------------------------
    // Outbound HTTP Egress Validation
    // ------------------------------------------------------------------------

    pub fn validate_egress_url(&self, url_str: &str) -> Result<reqwest::Url, HostAbiError> {
        let parsed = reqwest::Url::parse(url_str)
            .map_err(|e| HostAbiError::NetworkError(format!("Invalid URL: {e}")))?;

        let host_str = parsed
            .host_str()
            .ok_or_else(|| HostAbiError::NetworkError("Missing host in URL".into()))?;

        if let Ok(ip) = host_str.parse::<IpAddr>() {
            if is_private_or_restricted_ip(&ip) {
                return Err(HostAbiError::SsrfBlocked(ip.to_string()));
            }
        }

        if host_str == "localhost" || host_str.ends_with(".localhost") {
            return Err(HostAbiError::SsrfBlocked(host_str.to_string()));
        }

        if !self.allowlisted_domains.contains(host_str) {
            return Err(HostAbiError::EgressForbidden {
                domain: host_str.to_string(),
            });
        }

        Ok(parsed)
    }
}

/// Enforces that guest SQL is a single statement (rejecting multi-statement strings)
fn validate_single_sql_statement(sql: &str) -> Result<(), HostAbiError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(HostAbiError::DatabaseError("Empty SQL query".into()));
    }

    // Disallow semicolons that aren't at the very end of the string
    let without_trailing_semicolon = trimmed.trim_end_matches(';').trim();
    if without_trailing_semicolon.contains(';') {
        return Err(HostAbiError::DatabaseError(
            "Multi-statement SQL queries are forbidden for security. Use single parameterized statements."
                .into(),
        ));
    }

    Ok(())
}

/// Convert JSON value array into rusqlite ToSql trait objects
fn json_to_rusqlite_params(
    json: &serde_json::Value,
) -> Result<Vec<Box<dyn rusqlite::ToSql>>, HostAbiError> {
    match json {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(arr) => {
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    serde_json::Value::Null => {
                        params.push(Box::new(rusqlite::types::Null));
                    }
                    serde_json::Value::Bool(b) => {
                        params.push(Box::new(*b));
                    }
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            params.push(Box::new(i));
                        } else if let Some(f) = n.as_f64() {
                            params.push(Box::new(f));
                        } else {
                            params.push(Box::new(n.to_string()));
                        }
                    }
                    serde_json::Value::String(s) => {
                        params.push(Box::new(s.clone()));
                    }
                    _ => {
                        // Complex objects/arrays passed as JSON strings
                        params.push(Box::new(item.to_string()));
                    }
                }
            }
            Ok(params)
        }
        _ => Err(HostAbiError::MalformedJson(
            "SQL parameters must be passed as a JSON array".into(),
        )),
    }
}

/// Helper to verify IP is not loopback, private, link-local, or broadcast
pub fn is_private_or_restricted_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_broadcast()
                || ipv4.is_documentation()
                || ipv4.is_unspecified()
        }
        IpAddr::V6(ipv6) => ipv6.is_loopback() || ipv6.is_unspecified(),
    }
}

/// Generate UTC timestamp (ISO 8601) deterministically via host clock
pub fn host_clock_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Generate UUIDv7 deterministically via host
pub fn host_uuid_v7() -> String {
    uuid::Uuid::now_v7().to_string()
}
