//! Policy data model — types stored as JSON in `rbh_entries.policies.definition`.

use chrono::{DateTime, NaiveTime, Utc};
use rbh_predicate::Predicate;
use serde::{Deserialize, Serialize};

/// A complete policy definition. Stored as JSON in the `definition` column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDef {
    pub name: String,
    pub kind: PolicyKind,
    /// SQL-pushable WHERE clause for candidate selection.
    pub scope: Predicate,
    /// First-match ordered rules applied to scope-filtered entries.
    pub rules: Vec<Rule>,
    /// Default action parameters (overridden by rule-level params).
    pub default_action: ActionParams,
    /// Each trigger becomes a scheduler-rs schedule.
    pub triggers: Vec<TriggerSpec>,
    /// Fileclasses to skip entirely (fast-deny).
    #[serde(default)]
    pub ignore_fileclass: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Policy kind — determines which action executor handles matched entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    Purge,
    Migration,
    HsmArchive,
    HsmRelease,
    Alert,
}

impl PolicyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Purge => "purge",
            Self::Migration => "migration",
            Self::HsmArchive => "hsm_archive",
            Self::HsmRelease => "hsm_release",
            Self::Alert => "alert",
        }
    }
}

/// A first-match rule within a policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// In-memory condition evaluated on scope-filtered entries.
    pub condition: Predicate,
    /// Action parameters (merged over policy defaults).
    pub action: ActionParams,
}

/// Action execution parameters. Each field is optional — `None` means
/// "inherit from the policy default" at merge time.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ActionParams {
    /// Maximum number of entries to process per run.
    pub max_count: Option<u64>,
    /// Maximum total bytes to process per run.
    pub max_volume: Option<u64>,
    /// Per-entry action timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Concurrency limit for this action.
    pub nb_threads: Option<u32>,
}

impl ActionParams {
    /// Merge `self` over `base`: fields in `self` override `base`.
    pub fn merge_over(&self, base: &ActionParams) -> ActionParams {
        ActionParams {
            max_count: self.max_count.or(base.max_count),
            max_volume: self.max_volume.or(base.max_volume),
            timeout_secs: self.timeout_secs.or(base.timeout_secs),
            nb_threads: self.nb_threads.or(base.nb_threads),
        }
    }
}

/// Trigger specification — maps 1:1 to a scheduler-rs concrete `Trigger`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerSpec {
    /// Fire every N seconds.
    Interval { secs: u64 },
    /// Fire on a cron schedule (6-field: sec min hour dom mon dow).
    Cron { expression: String },
    /// Fire once at the given time.
    Once { at: DateTime<Utc> },
    /// Fire immediately (one-shot).
    Immediate,
    /// Fire within a daily time window.
    Window {
        start: NaiveTime,
        end: NaiveTime,
        #[serde(default)]
        mode: WindowModeSpec,
    },
}

/// Window mode — once per window or repeat at interval.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowModeSpec {
    #[default]
    Once,
    Repeat {
        interval_secs: u64,
    },
}

/// A policy row as stored in the database.
#[derive(Debug, Clone)]
pub struct PolicyRow {
    pub id: u64,
    pub name: String,
    pub definition: PolicyDef,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl PolicyRow {
    /// Policy kind — derived from the definition, not stored separately.
    pub fn kind(&self) -> PolicyKind {
        self.definition.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_params_merge_override() {
        let base = ActionParams {
            max_count: Some(100),
            max_volume: Some(1_000_000),
            timeout_secs: Some(60),
            nb_threads: Some(4),
        };
        let over = ActionParams {
            max_count: Some(50),
            max_volume: None,
            timeout_secs: None,
            nb_threads: Some(8),
        };
        let merged = over.merge_over(&base);
        assert_eq!(merged.max_count, Some(50));
        assert_eq!(merged.max_volume, Some(1_000_000));
        assert_eq!(merged.timeout_secs, Some(60));
        assert_eq!(merged.nb_threads, Some(8));
    }

    #[test]
    fn policy_def_serde_roundtrip() {
        let def = PolicyDef {
            name: "purge_old".to_string(),
            kind: PolicyKind::Purge,
            scope: Predicate::Cmp {
                field: rbh_predicate::Field::Mtime,
                cmp: rbh_predicate::CmpOp::Lt,
                value: rbh_predicate::Value::Num(1_700_000_000),
            },
            rules: vec![Rule {
                condition: Predicate::True,
                action: ActionParams {
                    max_count: Some(1000),
                    ..Default::default()
                },
            }],
            default_action: ActionParams::default(),
            triggers: vec![TriggerSpec::Interval { secs: 300 }],
            ignore_fileclass: vec![],
            enabled: true,
        };
        let json = serde_json::to_string(&def).unwrap();
        let back: PolicyDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn trigger_spec_serde_variants() {
        let specs = vec![
            (r#"{"type":"interval","secs":300}"#, "interval"),
            (r#"{"type":"cron","expression":"0 0 2 * * *"}"#, "cron"),
            (r#"{"type":"immediate"}"#, "immediate"),
        ];
        for (json, expected_type) in specs {
            let spec: TriggerSpec = serde_json::from_str(json).unwrap();
            let roundtrip = serde_json::to_string(&spec).unwrap();
            assert!(roundtrip.contains(expected_type));
        }
    }

    #[test]
    fn window_mode_defaults_to_once() {
        let json = r#"{"type":"window","start":"09:00:00","end":"17:00:00"}"#;
        let spec: TriggerSpec = serde_json::from_str(json).unwrap();
        match spec {
            TriggerSpec::Window { mode, .. } => assert_eq!(mode, WindowModeSpec::Once),
            _ => panic!("expected Window"),
        }
    }
}
