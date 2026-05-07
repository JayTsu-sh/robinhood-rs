//! Policy data model — types stored as JSON in `rbh_entries.policies.definition`.
//!
//! Two-tier architecture:
//!   Layer 1 — Classifiers (`/api/classifiers`): write tags to sm_status.xattr
//!   Layer 2 — Action Policies (`/api/policies`): filter by tags, execute actions

use chrono::{DateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

/// A complete action policy definition. Stored as JSON in `policies.definition`.
///
/// Action policies filter entries by tag (written by classifiers) and execute
/// a single action kind. All attribute-based filtering lives in classifiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDef {
    pub name: String,
    pub kind: PolicyKind,
    /// Tag key-value filter — ALL tags must match (AND semantics).
    /// Expands to `AND(Xattr(k1=v1), Xattr(k2=v2), …)` at query time.
    /// An empty map matches all entries (equivalent to `scope: true`).
    #[serde(default)]
    pub match_tags: HashMap<String, String>,
    /// Human-readable trigger expression. Parsed server-side to `TriggerSpec`.
    ///
    /// Examples: `"1h"`, `"30m"`, `"cron:0 2 * * *"`,
    ///           `"fs>85%"`, `"count>10000"`, `"volume>100GB"`, `"ost>80%"`
    pub trigger: String,
    /// Kind-specific execution options.
    #[serde(default)]
    pub action: ActionOpts,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Flattened action options. Only the sub-struct relevant to `kind` should be set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ActionOpts {
    /// Maximum entries to process per run.
    #[serde(default)]
    pub max_count: Option<u64>,
    /// Maximum total bytes to process per run.
    #[serde(default)]
    pub max_volume: Option<u64>,
    /// Concurrency: parallel action workers. Defaults to 1.
    #[serde(default)]
    pub nb_threads: Option<u32>,
    /// Per-entry action timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// LRU sort: order candidates oldest-first by this attribute.
    #[serde(default)]
    pub lru_sort: Option<LruSortAttr>,
    /// Rate limit on dispatched actions.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// Retry policy per action failure.
    #[serde(default)]
    pub retry: Option<RetryParams>,
    /// HSM parameters (archive_id, hints) — for HsmArchive/Release/Restore/Remove.
    #[serde(default)]
    pub hsm: Option<HsmParams>,
    /// External backup tool config — for Backup kind.
    #[serde(default)]
    pub backup: Option<rbh_backup::BackupCommandConfig>,
    /// Arbitrary-command config — for Migration kind.
    #[serde(default)]
    pub cmd: Option<CmdParams>,
    /// Webhook/log config — for Alert kind.
    #[serde(default)]
    pub alert: Option<AlertParams>,
    /// Skip entries with nlink > 1 (hardlink safety). Default: false.
    #[serde(default)]
    pub skip_hardlinked: bool,
}

/// Policy kind — determines which action executor handles matched entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    Purge,
    Migration,
    HsmArchive,
    HsmRelease,
    HsmRestore,
    HsmRemove,
    Alert,
    Backup,
}

impl PolicyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Purge => "purge",
            Self::Migration => "migration",
            Self::HsmArchive => "hsm_archive",
            Self::HsmRelease => "hsm_release",
            Self::HsmRestore => "hsm_restore",
            Self::HsmRemove => "hsm_remove",
            Self::Alert => "alert",
            Self::Backup => "backup",
        }
    }
}

/// LRU sort attribute for candidate ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LruSortAttr {
    #[default]
    None,
    Atime,
    Mtime,
    Ctime,
    LastSeen,
}

impl LruSortAttr {
    pub fn column(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Atime => Some("atime"),
            Self::Mtime => Some("mtime"),
            Self::Ctime => Some("ctime"),
            Self::LastSeen => Some("last_seen"),
        }
    }
}

/// Arbitrary-command action parameters for Migration kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdParams {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub cmd_vars: HashMap<String, String>,
}

/// Alert sink configuration for Alert kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertParams {
    #[serde(default)]
    pub webhook: Option<String>,
    #[serde(default = "default_true")]
    pub log: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// Retry policy per action failure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetryParams {
    pub max_attempts: u32,
    #[serde(default = "default_backoff")]
    pub backoff_secs: u64,
}

fn default_backoff() -> u64 {
    1
}

/// HSM backend parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HsmParams {
    pub archive_id: Option<u32>,
    #[serde(default)]
    pub hints: Option<String>,
}

/// Action rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_per_sec: Option<u32>,
    pub max_bytes_per_sec: Option<u64>,
}

// ── Trigger specification ────────────────────────────────────────────────────
//
// The `trigger` field in PolicyDef is a human-readable string parsed by
// `rbh_policy::trigger_parser::parse_trigger()` into one of these variants.
// They are also used directly by the threshold checker in rbh-daemon.

/// Trigger specification — internal representation after parsing `PolicyDef.trigger`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerSpec {
    Interval {
        secs: u64,
    },
    Cron {
        expression: String,
    },
    Once {
        at: DateTime<Utc>,
    },
    Immediate,
    Window {
        start: NaiveTime,
        end: NaiveTime,
        #[serde(default)]
        mode: WindowModeSpec,
    },
    ThresholdCount {
        check_interval_secs: u64,
        high_count: u64,
        #[serde(default)]
        low_count: u64,
        #[serde(default)]
        post_trigger_wait_secs: u64,
        #[serde(default)]
        target: ThresholdTarget,
    },
    ThresholdVolume {
        check_interval_secs: u64,
        high_bytes: u64,
        #[serde(default)]
        low_bytes: u64,
        #[serde(default)]
        post_trigger_wait_secs: u64,
        #[serde(default)]
        target: ThresholdTarget,
    },
    ThresholdOstPct {
        check_interval_secs: u64,
        high_pct: u32,
        #[serde(default)]
        low_pct: u32,
        #[serde(default)]
        post_trigger_wait_secs: u64,
        #[serde(default)]
        target: ThresholdTarget,
    },
    ThresholdFsPct {
        check_interval_secs: u64,
        high_pct: u32,
        #[serde(default)]
        low_pct: u32,
        #[serde(default)]
        post_trigger_wait_secs: u64,
    },
}

/// Per-threshold target scope.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThresholdTarget {
    #[default]
    Fs,
    Ost {
        osts: Vec<u32>,
    },
    Pool {
        name: String,
    },
    User {
        uid: u32,
    },
    Group {
        gid: u32,
    },
}

/// Window trigger mode.
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
    pub fn kind(&self) -> PolicyKind {
        self.definition.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_def_serde_roundtrip() {
        let def = PolicyDef {
            name: "archive_cold".to_string(),
            kind: PolicyKind::HsmArchive,
            match_tags: [("tier".to_string(), "cold".to_string())].into(),
            trigger: "fs > 85%".to_string(),
            action: ActionOpts {
                hsm: Some(HsmParams {
                    archive_id: Some(1),
                    hints: None,
                }),
                ..Default::default()
            },
            enabled: true,
        };
        let json = serde_json::to_string(&def).unwrap();
        let back: PolicyDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn policy_def_empty_match_tags_is_valid() {
        let json = r#"{
            "name":"purge_all","kind":"purge",
            "trigger":"1h"
        }"#;
        let def: PolicyDef = serde_json::from_str(json).unwrap();
        assert!(def.match_tags.is_empty());
        assert!(def.enabled);
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
