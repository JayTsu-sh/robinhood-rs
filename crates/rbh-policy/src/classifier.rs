//! Classifier engine — Layer 1 of the two-tier classification+action architecture.
//!
//! A classifier maps file attribute conditions to tag key-value pairs written into
//! `entries.sm_status.xattr.*`. Action policies then filter by these tags, keeping
//! all attribute-based logic in one place.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use rbh_predicate::{CmpOp, Field, Predicate, Value};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

use crate::PolicyError;

fn default_true() -> bool {
    true
}

// ── Data model ────────────────────────────────────────────────────────────────

/// A complete classifier definition stored in `classifiers.definition`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierDef {
    pub name: String,
    /// Tag keys this classifier owns. Before each full run, these keys are
    /// cleared from every processed entry, then re-applied per the rules.
    /// During incremental (changelog-driven) classification, they are also
    /// cleared before writing the new tags.
    pub manages: Vec<String>,
    /// First-match ordered rules. The last rule with no `when` is the default.
    pub rules: Vec<ClassifierRule>,
    /// Human-readable schedule string parsed by `trigger_parser::parse_trigger`.
    /// E.g. `"1h"`, `"30m"`, `"cron:0 * * * *"`.
    pub schedule: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// A single classification rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierRule {
    /// Condition expression. `None` = catch-all default (must be last).
    ///
    /// Syntax: `"field op value"` with optional comma-separated AND terms.
    /// Examples: `"atime > -1d"`, `"atime < -90d, size > 1GB"`, `"uid == 1000"`
    pub when: Option<String>,
    /// Tags to write if this rule matches, e.g. `{"tier":"cold","priority":"high"}`.
    pub set: HashMap<String, String>,
}

/// A classifier row as stored in the database.
#[derive(Debug, Clone)]
pub struct ClassifierRow {
    pub id: u64,
    pub name: String,
    pub definition: ClassifierDef,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// MariaDB-backed CRUD store for classifiers.
#[derive(Debug, Clone)]
pub struct ClassifierStore {
    pool: MySqlPool,
}

impl ClassifierStore {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, def: &ClassifierDef) -> Result<u64, PolicyError> {
        validate_classifier(def)?;
        let json = serde_json::to_string(def)?;
        let result = sqlx::query("INSERT INTO classifiers (name, definition, enabled) VALUES (?, ?, ?)")
            .bind(&def.name)
            .bind(&json)
            .bind(def.enabled)
            .execute(&self.pool)
            .await
            .map_err(|e| PolicyError::Store(e.to_string()))?;
        Ok(result.last_insert_id())
    }

    pub async fn get(&self, id: u64) -> Result<ClassifierRow, PolicyError> {
        let row =
            sqlx::query("SELECT id, name, definition, enabled, created_at, updated_at FROM classifiers WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| PolicyError::Store(e.to_string()))?
                .ok_or(PolicyError::NotFound(id))?;
        row_to_classifier(&row)
    }

    pub async fn list(&self) -> Result<Vec<ClassifierRow>, PolicyError> {
        let rows =
            sqlx::query("SELECT id, name, definition, enabled, created_at, updated_at FROM classifiers ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| PolicyError::Store(e.to_string()))?;
        rows.iter().map(row_to_classifier).collect()
    }

    pub async fn update(&self, id: u64, def: &ClassifierDef) -> Result<(), PolicyError> {
        validate_classifier(def)?;
        let json = serde_json::to_string(def)?;
        let result = sqlx::query("UPDATE classifiers SET name = ?, definition = ?, enabled = ? WHERE id = ?")
            .bind(&def.name)
            .bind(&json)
            .bind(def.enabled)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| PolicyError::Store(e.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }

    pub async fn delete(&self, id: u64) -> Result<(), PolicyError> {
        let result = sqlx::query("DELETE FROM classifiers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| PolicyError::Store(e.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }
}

fn row_to_classifier(row: &sqlx::mysql::MySqlRow) -> Result<ClassifierRow, PolicyError> {
    use sqlx::Row;
    let def_bytes: Vec<u8> = row
        .try_get("definition")
        .map_err(|e| PolicyError::Store(e.to_string()))?;
    let definition: ClassifierDef =
        serde_json::from_slice(&def_bytes).map_err(|e| PolicyError::Store(e.to_string()))?;
    Ok(ClassifierRow {
        id: row.try_get("id").map_err(|e| PolicyError::Store(e.to_string()))?,
        name: row.try_get("name").map_err(|e| PolicyError::Store(e.to_string()))?,
        enabled: row.try_get("enabled").map_err(|e| PolicyError::Store(e.to_string()))?,
        created_at: row
            .try_get("created_at")
            .map_err(|e| PolicyError::Store(e.to_string()))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|e| PolicyError::Store(e.to_string()))?,
        definition,
    })
}

fn validate_classifier(def: &ClassifierDef) -> Result<(), PolicyError> {
    if def.name.is_empty() {
        return Err(PolicyError::InvalidTrigger("classifier name must not be empty".into()));
    }
    if def.manages.is_empty() {
        return Err(PolicyError::InvalidTrigger(
            "classifier must declare at least one managed tag key".into(),
        ));
    }
    if def.rules.is_empty() {
        return Err(PolicyError::InvalidTrigger(
            "classifier must have at least one rule".into(),
        ));
    }
    // Validate schedule string
    crate::trigger_parser::parse_trigger(&def.schedule)
        .map_err(|e| PolicyError::InvalidTrigger(format!("invalid schedule: {e}")))?;
    // Validate all `when` expressions
    for rule in &def.rules {
        if let Some(expr) = &rule.when {
            parse_when(expr).map_err(|e| PolicyError::InvalidTrigger(format!("invalid when '{expr}': {e}")))?;
        }
    }
    Ok(())
}

// ── `when` expression parser ──────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum WhenParseError {
    #[error("empty expression")]
    Empty,
    #[error("unknown field: {0}")]
    UnknownField(String),
    #[error("unknown operator: {0}")]
    UnknownOp(String),
    #[error("invalid value in '{0}'")]
    BadValue(String),
}

/// Parse a `when` expression string into a [`Predicate`].
///
/// Grammar (informal):
/// ```text
/// expr    := term ("," term)*       // comma = AND
/// term    := field ws op ws value
/// field   := "atime" | "mtime" | "ctime" | "size" | "uid" | "gid" | "depth" | "nlink"
/// op      := ">" | ">=" | "<" | "<=" | "==" | "!="
/// value   := relative_time | size_with_unit | integer
/// ```
pub fn parse_when(expr: &str) -> Result<Predicate, WhenParseError> {
    let terms: Vec<&str> = expr.split(',').collect();
    if terms.is_empty() || expr.trim().is_empty() {
        return Err(WhenParseError::Empty);
    }
    let predicates: Vec<Predicate> = terms.iter().map(|t| parse_term(t.trim())).collect::<Result<_, _>>()?;
    if predicates.len() == 1 {
        Ok(predicates.into_iter().next().unwrap())
    } else {
        Ok(Predicate::And { children: predicates })
    }
}

fn parse_term(term: &str) -> Result<Predicate, WhenParseError> {
    // Split on two-char operators first, then one-char
    let (field_str, op_str, val_str) = split_term(term)?;

    let (field, is_time_field) = match field_str {
        "atime" => (Field::Atime, true),
        "mtime" => (Field::Mtime, true),
        "ctime" => (Field::Ctime, true),
        "size" => (Field::Size, false),
        "uid" => (Field::Uid, false),
        "gid" => (Field::Gid, false),
        "depth" => (Field::Depth, false),
        "nlink" => (Field::Nlink, false),
        other => return Err(WhenParseError::UnknownField(other.to_string())),
    };

    let cmp = match op_str {
        ">" => CmpOp::Gt,
        ">=" => CmpOp::Ge,
        "<" => CmpOp::Lt,
        "<=" => CmpOp::Le,
        "==" => CmpOp::Eq,
        "!=" => CmpOp::Ne,
        other => return Err(WhenParseError::UnknownOp(other.to_string())),
    };

    let value = if is_time_field {
        Value::Num(parse_time_value(val_str, term)?)
    } else {
        Value::Num(parse_numeric_value(val_str, term)?)
    };

    Ok(Predicate::Cmp { field, cmp, value })
}

fn split_term(term: &str) -> Result<(&str, &str, &str), WhenParseError> {
    // Try two-char operators first
    for op in &[">=", "<=", "==", "!="] {
        if let Some(pos) = term.find(op) {
            let f = term[..pos].trim();
            let v = term[pos + op.len()..].trim();
            return Ok((f, op, v));
        }
    }
    // Single-char operators
    for op in &[">", "<"] {
        if let Some(pos) = term.find(op) {
            let f = term[..pos].trim();
            let v = term[pos + 1..].trim();
            return Ok((f, op, v));
        }
    }
    Err(WhenParseError::BadValue(term.to_string()))
}

/// Parse a time value: relative (`-1d`, `-90d`, `-2h`) or absolute epoch.
fn parse_time_value(s: &str, orig: &str) -> Result<i64, WhenParseError> {
    let s = s.trim();
    if let Some(rel) = s.strip_prefix('-') {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let lower = rel.to_lowercase();
        let (num_str, mult) = if let Some(n) = lower.strip_suffix('d') {
            (n, 86400i64)
        } else if let Some(n) = lower.strip_suffix('h') {
            (n, 3600i64)
        } else if let Some(n) = lower.strip_suffix('m') {
            (n, 60i64)
        } else {
            return Err(WhenParseError::BadValue(orig.to_string()));
        };
        let n: i64 = num_str
            .parse()
            .map_err(|_| WhenParseError::BadValue(orig.to_string()))?;
        Ok(now - n * mult)
    } else {
        // Absolute epoch
        s.parse::<i64>().map_err(|_| WhenParseError::BadValue(orig.to_string()))
    }
}

/// Parse size or plain integer: `1GB`, `100MB`, `1000`.
fn parse_numeric_value(s: &str, orig: &str) -> Result<i64, WhenParseError> {
    let upper = s.trim().to_uppercase();
    let (num_str, mult): (&str, i64) = if let Some(n) = upper.strip_suffix("TB") {
        (n, 1i64 << 40)
    } else if let Some(n) = upper.strip_suffix("GB") {
        (n, 1i64 << 30)
    } else if let Some(n) = upper.strip_suffix("MB") {
        (n, 1i64 << 20)
    } else if let Some(n) = upper.strip_suffix("KB") {
        (n, 1i64 << 10)
    } else {
        (upper.as_str(), 1i64)
    };
    let n: i64 = num_str
        .trim()
        .parse()
        .map_err(|_| WhenParseError::BadValue(orig.to_string()))?;
    Ok(n * mult)
}

// ── In-memory classification ──────────────────────────────────────────────────

/// Evaluate classifier rules against an entry in-memory and return the
/// first matching rule's tags, or `None` if no rule matches.
pub fn evaluate_classifier<'a>(
    def: &'a ClassifierDef, entry: &rbh_entry_store::model::EntryRow,
) -> Option<&'a HashMap<String, String>> {
    for rule in &def.rules {
        let matched = match &rule.when {
            None => true, // default catch-all
            Some(expr) => parse_when(expr)
                .map(|pred| rbh_predicate::matches(&pred, entry))
                .unwrap_or(false),
        };
        if matched {
            return Some(&rule.set);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_when_size_gt() {
        let pred = parse_when("size > 1GB").unwrap();
        match pred {
            Predicate::Cmp {
                field: Field::Size,
                cmp: CmpOp::Gt,
                value: Value::Num(n),
            } => {
                assert_eq!(n, 1i64 << 30);
            }
            _ => panic!("unexpected: {pred:?}"),
        }
    }

    #[test]
    fn parse_when_relative_time() {
        let pred = parse_when("atime > -1d").unwrap();
        match pred {
            Predicate::Cmp {
                field: Field::Atime,
                cmp: CmpOp::Gt,
                value: Value::Num(n),
            } => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                // Should be within a second of now - 86400
                assert!((n - (now - 86400)).abs() <= 1);
            }
            _ => panic!("unexpected: {pred:?}"),
        }
    }

    #[test]
    fn parse_when_and_comma() {
        let pred = parse_when("atime < -90d, size > 1GB").unwrap();
        match pred {
            Predicate::And { children } => assert_eq!(children.len(), 2),
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn parse_when_uid_eq() {
        let pred = parse_when("uid == 1000").unwrap();
        match pred {
            Predicate::Cmp {
                field: Field::Uid,
                cmp: CmpOp::Eq,
                value: Value::Num(1000),
            } => {}
            _ => panic!("unexpected: {pred:?}"),
        }
    }

    #[test]
    fn parse_when_unknown_field_is_error() {
        assert!(matches!(parse_when("foo > 1"), Err(WhenParseError::UnknownField(_))));
    }

    #[test]
    fn classifier_def_serde_roundtrip() {
        let def = ClassifierDef {
            name: "tier".to_string(),
            manages: vec!["tier".to_string()],
            rules: vec![
                ClassifierRule {
                    when: Some("atime > -1d".to_string()),
                    set: [("tier".to_string(), "hot".to_string())].into(),
                },
                ClassifierRule {
                    when: None,
                    set: [("tier".to_string(), "cold".to_string())].into(),
                },
            ],
            schedule: "1h".to_string(),
            enabled: true,
        };
        let json = serde_json::to_string(&def).unwrap();
        let back: ClassifierDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);
    }
}
