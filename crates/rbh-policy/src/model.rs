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
    /// Fileclasses to skip entirely (fast-deny, composed into the scope
    /// query as `AND NOT (p1 OR p2 OR …)`).
    #[serde(default)]
    pub ignore_fileclass: Vec<FileClassDef>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// An inline fileclass definition. `name` is informational (used in logs);
/// `predicate` is the filter composed into the candidate query.
///
/// Matches robinhood-C's `fileclass { definition { ... } }` conceptually but
/// without the DSL — users POST these as part of the policy JSON body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileClassDef {
    pub name: String,
    pub predicate: Predicate,
}

/// Compose `scope AND NOT (ic1.predicate OR ic2.predicate OR …)`. Returns
/// `scope` unchanged when `ignore` is empty.
pub fn compose_scope_with_ignores(scope: &Predicate, ignore: &[FileClassDef]) -> Predicate {
    if ignore.is_empty() {
        return scope.clone();
    }
    let or_of_ignores = Predicate::Or {
        children: ignore.iter().map(|f| f.predicate.clone()).collect(),
    };
    Predicate::And {
        children: vec![
            scope.clone(),
            Predicate::Not {
                inner: Box::new(or_of_ignores),
            },
        ],
    }
}

/// Policy kind — determines which action executor handles matched entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    Purge,
    Migration,
    HsmArchive,
    HsmRelease,
    /// Restore a released file from the HSM backend back to Lustre.
    HsmRestore,
    /// Remove the HSM backend copy (file stays on Lustre).
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

/// A first-match rule within a policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// In-memory condition evaluated on scope-filtered entries.
    pub condition: Predicate,
    /// Action parameters (merged over policy defaults).
    pub action: ActionParams,
}

/// LRU sort attribute. Controls which timestamp column the candidate query
/// orders by (oldest first). Matches robinhood-C's `lru_sort_attr`.
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
    /// SQL column name, or `None` when no LRU sort is requested.
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
    /// LRU sort attribute — when set, candidates are ordered oldest-first.
    #[serde(default)]
    pub lru_sort: Option<LruSortAttr>,
    /// Rate limit applied to dispatched actions. See [`RateLimit`].
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// Retry policy applied per action failure.
    #[serde(default)]
    pub retry: Option<RetryParams>,
    /// HSM-specific parameters (archive_id, hints). Only consumed by
    /// HSM action executors; ignored by other kinds.
    #[serde(default)]
    pub hsm: Option<HsmParams>,
    /// External backup tool configuration — drives
    /// [`PolicyKind::Backup`] via rbh-backup's CommandBackupAdapter.
    #[serde(default)]
    pub backup: Option<rbh_backup::BackupCommandConfig>,
    /// Arbitrary-command configuration consumed by
    /// [`PolicyKind::Migration`]. Mirrors robinhood-C's `action = cmd(...)`
    /// and `common_actions.c`.
    #[serde(default)]
    pub cmd: Option<CmdParams>,
    /// Webhook / logging configuration for [`PolicyKind::Alert`].
    #[serde(default)]
    pub alert: Option<AlertParams>,
    /// When true, entries with `nlink > 1` are skipped at dispatch
    /// time. Mirrors robinhood-C's `check_hardlinks_before_rm` safety
    /// check — prevents a policy that targets `path A` from
    /// implicitly mutating the file visible at hardlinked `path B`.
    /// Default: false (preserves original behavior).
    #[serde(default)]
    pub skip_hardlinked: bool,
}

/// Arbitrary-command action parameters. `command` is resolved on PATH;
/// each item in `args` is rendered with built-in placeholders and
/// any operator-defined key=value pairs in `cmd_vars`:
///
/// Built-in: `{fid}`, `{path}` (.lustre/fid virtual path), `{fullpath}`
/// (real POSIX path via fid_to_path), `{size}`, `{uid}`, `{gid}`,
/// `{mtime}`, `{ctime}`, `{atime}`.
///
/// Custom: any key in `cmd_vars` is available as `{key}` in `args`.
/// Mirrors robinhood-C's `action_params { key = value; }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdParams {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-invocation timeout in seconds (overrides action `timeout_secs`).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Custom template variables — each key becomes `{key}` in args.
    /// Mirrors robinhood-C's `action_params { target_pool = hddpool; }`.
    #[serde(default)]
    pub cmd_vars: std::collections::HashMap<String, String>,
}

/// Alert sink configuration. Exactly one of `webhook` / `log_only`
/// should be effective; when both are set, both fire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertParams {
    /// POST the payload to this URL as JSON.
    #[serde(default)]
    pub webhook: Option<String>,
    /// If true, emit a `tracing::warn!` record per matched entry.
    #[serde(default = "default_true")]
    pub log: bool,
    /// Optional free-form message field included in the JSON payload.
    #[serde(default)]
    pub message: Option<String>,
}

/// Retry policy for a single entry. On `Failed`/error outcomes the
/// dispatcher sleeps `backoff_secs` and calls the executor again, up
/// to `max_attempts` total attempts (so `max_attempts=3` = 1 initial +
/// 2 retries).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetryParams {
    pub max_attempts: u32,
    #[serde(default = "default_backoff")]
    pub backoff_secs: u64,
}

fn default_backoff() -> u64 {
    1
}

/// HSM-specific per-policy knobs. `archive_id` routes the request to a
/// specific HSM backend; `hints` is opaque payload passed to the HSM
/// agent (documented per-agent; commonly a tier name for multi-tier).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HsmParams {
    /// HSM archive backend id. When `None`, defaults to 1.
    pub archive_id: Option<u32>,
    /// Agent-specific hints (e.g. tier selector). Base64 when round-
    /// tripping through JSON if the payload isn't UTF-8.
    #[serde(default)]
    pub hints: Option<String>,
}

/// Action rate limit. Both fields are optional; `None` means unlimited.
///
/// * `max_per_sec` — hard ceiling on actions dispatched per second.
/// * `max_bytes_per_sec` — approximate ceiling on bytes processed per
///   second (summed from `EntryRow::size`; does not reflect real I/O).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_per_sec: Option<u32>,
    pub max_bytes_per_sec: Option<u64>,
}

impl ActionParams {
    /// Merge `self` over `base`: fields in `self` override `base`.
    pub fn merge_over(&self, base: &ActionParams) -> ActionParams {
        ActionParams {
            max_count: self.max_count.or(base.max_count),
            max_volume: self.max_volume.or(base.max_volume),
            timeout_secs: self.timeout_secs.or(base.timeout_secs),
            nb_threads: self.nb_threads.or(base.nb_threads),
            lru_sort: self.lru_sort.or(base.lru_sort),
            rate_limit: match (self.rate_limit, base.rate_limit) {
                (Some(a), Some(b)) => Some(RateLimit {
                    max_per_sec: a.max_per_sec.or(b.max_per_sec),
                    max_bytes_per_sec: a.max_bytes_per_sec.or(b.max_bytes_per_sec),
                }),
                (Some(a), None) => Some(a),
                (None, b) => b,
            },
            retry: self.retry.or(base.retry),
            hsm: match (self.hsm.clone(), base.hsm.clone()) {
                (Some(a), Some(b)) => Some(HsmParams {
                    archive_id: a.archive_id.or(b.archive_id),
                    hints: a.hints.or(b.hints),
                }),
                (Some(a), None) => Some(a),
                (None, b) => b,
            },
            backup: self.backup.clone().or_else(|| base.backup.clone()),
            cmd: self.cmd.clone().or_else(|| base.cmd.clone()),
            alert: self.alert.clone().or_else(|| base.alert.clone()),
            // skip_hardlinked: "opted in by either side" — safety field.
            skip_hardlinked: self.skip_hardlinked || base.skip_hardlinked,
        }
    }
}

/// Trigger specification. Time-based variants (Interval / Cron / Once /
/// Immediate / Window) map 1:1 to scheduler-rs schedules; threshold
/// variants are checked inside the daemon by a dedicated polling task and
/// produce immediate policy runs when they fire.
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

    /// Threshold: re-check every `check_interval_secs`. Fires when the count
    /// of entries matching `scope AND scope_extra` reaches or exceeds
    /// `high_count`, stops re-firing until the count drops below `low_count`
    /// (soft cooldown enforced by `post_trigger_wait_secs`).
    ThresholdCount {
        check_interval_secs: u64,
        high_count: u64,
        #[serde(default)]
        low_count: u64,
        #[serde(default)]
        post_trigger_wait_secs: u64,
        /// Optional narrowing — resolved to a `TargetFilter` for the run.
        #[serde(default)]
        target: ThresholdTarget,
    },

    /// Threshold on total size (bytes) of entries matching the scope.
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

    /// Threshold on *live* OST disk usage (from `llapi_obd_statfs`),
    /// not the DB-derived `SUM(size)`. Fires when any targeted OST's
    /// used ratio reaches `high_pct`; the firing run is narrowed to
    /// that OST so purging reclaims space on the hot OST first.
    ///
    /// `target` accepts `Fs` (any OST), `Pool { name }` (any OST in
    /// the pool, matched by name prefix after stripping the `OSTxxxx`
    /// suffix), or `Ost { osts }` (one or more explicit OST indices).
    ThresholdOstPct {
        check_interval_secs: u64,
        /// Fire when `used/total >= high_pct`. Percent as 0..100.
        high_pct: u32,
        /// Low-watermark for the in-run stopper. 0 disables.
        #[serde(default)]
        low_pct: u32,
        #[serde(default)]
        post_trigger_wait_secs: u64,
        #[serde(default)]
        target: ThresholdTarget,
    },
    /// Trigger when the **aggregate** filesystem usage percentage
    /// (sum-of-used / sum-of-total across all OSTs, via `llapi_obd_statfs`)
    /// reaches `high_pct`. Unlike `ThresholdOstPct` which fires on any
    /// individual hot OST, this fires on the whole-filesystem fill level
    /// (equivalent to `trigger_on = global_usage` in robinhood-C).
    ThresholdFsPct {
        check_interval_secs: u64,
        /// Fire when aggregate `used/total >= high_pct/100`. Range 0..=100.
        high_pct: u32,
        /// Stop in-run when aggregate drops to or below `low_pct/100`. 0 disables.
        #[serde(default)]
        low_pct: u32,
        #[serde(default)]
        post_trigger_wait_secs: u64,
    },
}

/// Per-threshold target. Kept separate from `TargetFilter` in `task.rs` so
/// the model layer stays independent; the daemon converts at fire time.
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
            lru_sort: Some(LruSortAttr::Atime),
            rate_limit: Some(RateLimit {
                max_per_sec: Some(100),
                max_bytes_per_sec: Some(10_000_000),
            }),
            retry: Some(RetryParams {
                max_attempts: 3,
                backoff_secs: 2,
            }),
            hsm: Some(HsmParams {
                archive_id: Some(1),
                hints: Some("tier=cold".into()),
            }),
            backup: Some(rbh_backup::BackupCommandConfig {
                command: "/usr/local/bin/rbhext_tool".into(),
                args: None,
                timeout_secs: Some(600),
                dest_template: None,
            }),
            cmd: Some(CmdParams {
                command: "/usr/bin/logger".into(),
                args: vec!["base".into()],
                timeout_secs: Some(5),
                cmd_vars: std::collections::HashMap::new(),
            }),
            alert: Some(AlertParams {
                webhook: Some("http://alerts.invalid/hook".into()),
                log: true,
                message: Some("base".into()),
            }),
            skip_hardlinked: true,
        };
        let over = ActionParams {
            max_count: Some(50),
            max_volume: None,
            timeout_secs: None,
            nb_threads: Some(8),
            lru_sort: None,
            rate_limit: Some(RateLimit {
                max_per_sec: Some(30),
                max_bytes_per_sec: None,
            }),
            retry: None,
            hsm: Some(HsmParams {
                archive_id: Some(2),
                hints: None,
            }),
            backup: None,
            cmd: None,
            alert: None,
            skip_hardlinked: false,
        };
        let merged = over.merge_over(&base);
        assert_eq!(merged.max_count, Some(50));
        assert_eq!(merged.max_volume, Some(1_000_000));
        assert_eq!(merged.timeout_secs, Some(60));
        assert_eq!(merged.nb_threads, Some(8));
        assert_eq!(merged.lru_sort, Some(LruSortAttr::Atime));
        // rate_limit: over.max_per_sec wins, max_bytes_per_sec falls back to base.
        let rl = merged.rate_limit.unwrap();
        assert_eq!(rl.max_per_sec, Some(30));
        assert_eq!(rl.max_bytes_per_sec, Some(10_000_000));
        // retry: over.retry=None, base.retry=Some → base wins.
        let r = merged.retry.unwrap();
        assert_eq!(r.max_attempts, 3);
        assert_eq!(r.backoff_secs, 2);
        // hsm: archive_id=over; hints=base (over's hints=None).
        let h = merged.hsm.unwrap();
        assert_eq!(h.archive_id, Some(2));
        assert_eq!(h.hints.as_deref(), Some("tier=cold"));
        // backup: over is None → base wins (both variants covered).
        let b = merged.backup.unwrap();
        assert_eq!(b.command, "/usr/local/bin/rbhext_tool");
        assert_eq!(b.timeout_secs, Some(600));
        // cmd: over is None → base wins.
        let c = merged.cmd.unwrap();
        assert_eq!(c.command, "/usr/bin/logger");
        // alert: over is None → base wins.
        let a = merged.alert.unwrap();
        assert_eq!(a.webhook.as_deref(), Some("http://alerts.invalid/hook"));
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

    #[test]
    fn compose_with_no_ignores_returns_scope() {
        let scope = Predicate::Cmp {
            field: rbh_predicate::Field::Size,
            cmp: rbh_predicate::CmpOp::Gt,
            value: rbh_predicate::Value::Num(100),
        };
        assert_eq!(compose_scope_with_ignores(&scope, &[]), scope);
    }

    #[test]
    fn compose_adds_and_not_or() {
        let scope = Predicate::True;
        let ignores = vec![
            FileClassDef {
                name: "hot".into(),
                predicate: Predicate::Cmp {
                    field: rbh_predicate::Field::Mtime,
                    cmp: rbh_predicate::CmpOp::Gt,
                    value: rbh_predicate::Value::Num(1),
                },
            },
            FileClassDef {
                name: "project_a".into(),
                predicate: Predicate::NameLike {
                    pattern: "projA_%".into(),
                },
            },
        ];
        let composed = compose_scope_with_ignores(&scope, &ignores);
        // Round-trip through SQL generator to prove the tree is well-formed.
        let (sql, _params) = rbh_predicate::to_sql(&composed);
        assert!(sql.contains("NOT"));
        assert!(sql.contains("mtime > ?"));
        assert!(sql.contains("name LIKE ?"));
    }

    #[test]
    fn ignore_fileclass_deserializes_inline() {
        let json = r#"{
            "name":"p","kind":"purge",
            "scope":{"op":"true"},
            "rules":[],
            "default_action":{},
            "triggers":[],
            "ignore_fileclass":[
                {"name":"hot","predicate":{"op":"cmp","field":"mtime","cmp":"gt","value":100}}
            ]
        }"#;
        let def: PolicyDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.ignore_fileclass.len(), 1);
        assert_eq!(def.ignore_fileclass[0].name, "hot");
    }
}
