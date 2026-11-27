//! Compile the supported Rev.8 policy subset.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

pub mod execution;
pub mod heat;
pub mod resources;
mod tiering;
use resources::{ResourceBinding, ResourceKind};
pub use tiering::PoolMigration;

use chrono::{DateTime, Datelike, TimeDelta, Utc};
use rbh_entry_store::store::{EntryStore, QueryParam};
use rbh_entry_store::{BackendKind, EntryKind, FileSystemId};
use rbh_predicate::{CmpOp, Field, Predicate, SqlParam, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{MySql, QueryBuilder};

use crate::PolicyError;

pub(crate) const API_VERSION: &str = "hsm.storage.io/v1alpha3";
pub(crate) const BUILTIN_PREVIEW: &str = "builtin-preview";
pub(crate) const DB_EPOCH_MICROS: &str = "TIMESTAMPDIFF(MICROSECOND, '1970-01-01', UTC_TIMESTAMP(6))";

pub struct CompiledPolicy {
    pub name: String,
    pub filesystem: FileSystemId,
    pub(crate) selectors: BTreeSet<String>,
    pub(crate) uses_read_heat: bool,
    pub(crate) where_clause: String,
    pub(crate) params: Vec<QueryParam>,
    pub(crate) actions: Vec<Invocation>,
    pub(crate) decision: String,
    pub(crate) decision_params: Vec<QueryParam>,
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
    pub(crate) one_shot: bool,
    pub(crate) window: Option<JsonValue>,
    pub(crate) budget: BudgetLimits,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewResult {
    pub revision: u64,
    pub candidate_count: u64,
    pub estimated_bytes: u64,
    pub resource_bindings: Vec<ResourceBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_heat: Option<heat::PreviewHeat>,
}

/// Reject legacy and unsupported document versions at the REST boundary.
pub fn validate_policy_version(document: &JsonValue) -> Result<(), PolicyError> {
    if document.get("apiVersion").and_then(JsonValue::as_str) != Some(API_VERSION) {
        return Err(invalid(format!(
            "apiVersion must be {API_VERSION}. Submit a Rev.8 Policy document with kind, metadata and spec."
        )));
    }
    Ok(())
}

pub(crate) fn parse(document: &JsonValue) -> Result<Policy, PolicyError> {
    validate_policy_version(document)?;
    let policy: Policy = serde_path_to_error::deserialize(document).map_err(|error| invalid(format!("{error}")))?;
    if !valid_name(&policy.metadata.name) {
        return Err(invalid(
            "metadata.name must be a lowercase resource name of 1–253 characters",
        ));
    }
    policy.schedule_after(Utc::now())?;
    policy.spec.execution.budget.run.validate()?;
    if policy.enforced_and_enabled() && policy.spec.execution.budget.run.max_duration.is_none() {
        return Err(invalid(
            "spec.execution.budget.run.maxDuration is required for enforced activation",
        ));
    }
    if let Some(window) = &policy.spec.execution.admission_window {
        execution::Window::parse(window)?;
    }
    Ok(policy)
}

impl Policy {
    pub(crate) fn schedule_after(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, PolicyError> {
        let Trigger::Periodic { interval } = &self.spec.trigger else {
            return Ok(None);
        };
        let delay = duration_millis(interval)
            .and_then(|value| i64::try_from(value).ok())
            .and_then(TimeDelta::try_milliseconds)
            .ok_or_else(|| invalid("spec.trigger.interval must be a positive Rev.8 duration in the scheduler range"))?;
        now.checked_add_signed(delay)
            .filter(|date| date.year() <= 9999)
            .map(Some)
            .ok_or_else(|| invalid("spec.trigger.interval exceeds the scheduler range"))
    }

    pub(crate) fn next_run(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, PolicyError> {
        if matches!(self.spec.mode, Some(RunMode::OneShot)) {
            return Ok(None);
        }
        self.schedule_after(now)
    }

    pub(crate) fn enforced_and_enabled(&self) -> bool {
        self.spec.enabled != Some(false)
            && self
                .spec
                .execution
                .activation
                .as_ref()
                .is_some_and(|activation| activation.mode == ActivationMode::Enforced)
    }

    pub(crate) fn filesystem(&self) -> &FileSystemId {
        &self.spec.target.filesystem
    }

    pub(crate) fn references(&self) -> [(ResourceKind, &str, &str); 3] {
        [
            (
                ResourceKind::ExecutionProfile,
                &self.spec.execution.profile_ref,
                "spec.execution.profileRef",
            ),
            (
                ResourceKind::AuthorizationPolicy,
                &self.spec.governance.authorization_ref,
                "spec.governance.authorizationRef",
            ),
            (
                ResourceKind::ApprovalPolicy,
                &self.spec.governance.approval_policy_ref,
                "spec.governance.approvalPolicyRef",
            ),
        ]
    }

    pub(crate) fn compile(self, selectors: &BTreeMap<String, JsonValue>) -> Result<CompiledPolicy, PolicyError> {
        let spec = self.spec;
        let mut conditions = ConditionCompiler::new(selectors);
        let target_condition = conditions.compile(&spec.target.condition, "spec.target.where")?;
        let files = Predicate::Cmp {
            field: Field::Kind,
            cmp: CmpOp::Eq,
            value: Value::Num(EntryKind::File as i64),
        };
        let CompiledRules {
            excludes,
            decision,
            params: decision_params,
            actions,
            uses_read_heat,
            execute_requires_heat,
        } = spec.rules.compile(&mut conditions)?;
        let scope = Condition::join("AND", vec![Condition::indexed(files), target_condition, excludes]);
        let uses_read_heat = uses_read_heat || scope.uses_read_heat;
        let scope_requires_heat = !scope.can_match_without_heat;
        let mut where_clause = scope.sql;
        if uses_read_heat && !scope.uses_read_heat {
            where_clause = format!("({where_clause}) IS TRUE");
        }
        let enforced = spec
            .execution
            .activation
            .is_some_and(|activation| activation.mode == ActivationMode::Enforced);
        let mut params = query_params(scope.params);
        if let Some(migration) = actions.iter().find_map(|invocation| invocation.migration.as_ref()) {
            if !matches!(spec.trigger, Trigger::Periodic { .. }) {
                return Err(invalid("lustre.pool_migrate requires a periodic trigger"));
            }
            if actions
                .iter()
                .any(|invocation| invocation.migration.as_ref() != Some(migration))
            {
                return Err(invalid(
                    "a tiering policy requires one source pool, destination pool and set of migration parameters",
                ));
            }
            where_clause.push_str(" AND (CAST(pool_name AS BINARY) = CAST(? AS BINARY)) IS TRUE");
            params.push(QueryParam::Str(migration.source_pool.clone()));
            if uses_read_heat && enforced && migration.min_residence_micros.is_none() {
                return Err(invalid("heat-driven migration requires action.params.minResidence"));
            }
            if uses_read_heat && enforced && !scope_requires_heat && !execute_requires_heat {
                return Err(invalid(
                    "each execute decision in a heat-driven migration policy must require a read heat condition",
                ));
            }
        } else if uses_read_heat && enforced {
            return Err(invalid("enforced read heat conditions require lustre.pool_migrate"));
        }
        Ok(CompiledPolicy {
            name: self.metadata.name,
            filesystem: spec.target.filesystem,
            selectors: conditions.referenced,
            where_clause,
            actions,
            decision,
            decision_params: query_params(decision_params),
            uses_read_heat,
            enabled: spec.enabled.unwrap_or(true),
            enforced,
            one_shot: matches!(spec.mode, Some(RunMode::OneShot)),
            window: spec.execution.admission_window,
            budget: spec.execution.budget.run.limits(),
            params,
        })
    }
}

fn query_params(params: Vec<SqlParam>) -> Vec<QueryParam> {
    params
        .into_iter()
        .map(|param| match param {
            SqlParam::Num(value) => QueryParam::Int(value),
            SqlParam::Str(value) => QueryParam::Str(value),
        })
        .collect()
}

// Expressions are compiled by the server. Client values remain bound.
pub(crate) fn push_expression(query: &mut QueryBuilder<MySql>, sql: &str, params: &[QueryParam]) {
    let mut parts = sql.split('?');
    query.push(parts.next().expect("SQL has an initial fragment"));
    for param in params {
        match param {
            QueryParam::Int(value) => query.push_bind(*value),
            QueryParam::Str(value) => query.push_bind(value.clone()),
        };
        query.push(parts.next().expect("compiled SQL bind placeholder"));
    }
    debug_assert!(parts.next().is_none());
}

#[tracing::instrument(skip(entries))]
pub(crate) async fn validate_filesystem(
    entries: &EntryStore, filesystem: &FileSystemId,
) -> Result<rbh_entry_store::FileSystemConfig, PolicyError> {
    let config = entries
        .get_filesystem(filesystem)
        .await
        .map_err(|error| PolicyError::Store(error.to_string()))?
        .ok_or_else(|| invalid(format!("spec.target.filesystem is not registered: {}", filesystem)))?;
    if config.backend != BackendKind::Lustre || !config.capabilities.hsm {
        return Err(invalid(format!(
            "spec.target.filesystem does not support Lustre HSM: {}",
            filesystem
        )));
    }
    Ok(config)
}

impl CompiledPolicy {
    pub(crate) fn pool_migration(&self) -> Option<&PoolMigration> {
        self.actions.iter().find_map(|invocation| invocation.migration.as_ref())
    }

    fn scope(&self, database_time_micros: Option<i64>) -> (Cow<'_, str>, Cow<'_, [QueryParam]>) {
        let Some(duration) = self
            .pool_migration()
            .and_then(|migration| migration.min_residence_micros)
        else {
            return (Cow::Borrowed(&self.where_clause), Cow::Borrowed(&self.params));
        };
        let (cutoff, param) = match database_time_micros {
            Some(now) => ("?".into(), now.saturating_sub(duration)),
            None => (format!("{DB_EPOCH_MICROS} - ?"), duration),
        };
        let mut params = self.params.clone();
        params.push(QueryParam::Int(param));
        (
            Cow::Owned(format!(
                "{} AND (pool_entered_at <= {cutoff}) IS TRUE",
                self.where_clause
            )),
            Cow::Owned(params),
        )
    }

    pub(crate) fn push_scope(&self, query: &mut QueryBuilder<MySql>, database_time_micros: Option<i64>) {
        let (sql, params) = self.scope(database_time_micros);
        push_expression(query, &sql, &params);
    }

    pub(crate) fn validate_capabilities(&self, config: &rbh_entry_store::FileSystemConfig) -> Result<(), PolicyError> {
        if self.pool_migration().is_some() && !(config.capabilities.stripe && config.capabilities.ost) {
            return Err(invalid(
                "lustre.pool_migrate requires filesystem stripe and OST capabilities",
            ));
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(filesystem = %self.filesystem, revision))]
    pub async fn preview(
        &self, entries: &EntryStore, revision: u64, resource_bindings: Vec<ResourceBinding>,
    ) -> Result<PreviewResult, PolicyError> {
        let config = validate_filesystem(entries, &self.filesystem).await?;
        self.validate_capabilities(&config)?;
        let (scope, params) = self.scope(None);
        let where_clause = format!("{scope} AND ({}) > 0", self.decision);
        let params: Vec<_> = params.iter().chain(&self.decision_params).cloned().collect();
        let (candidate_count, estimated_bytes) = entries
            .scoped_totals_where(&self.filesystem, &where_clause, &params)
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?;
        Ok(PreviewResult {
            revision,
            candidate_count,
            estimated_bytes,
            resource_bindings,
            read_heat: None,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    #[serde(rename = "apiVersion")]
    _api_version: String,
    #[serde(rename = "kind")]
    _kind: PolicyKind,
    metadata: Metadata,
    spec: Spec,
}

#[derive(Deserialize)]
enum PolicyKind {
    Policy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    name: String,
    #[serde(rename = "description")]
    _description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    enabled: Option<bool>,
    #[serde(rename = "priority")]
    _priority: Option<u64>,
    mode: Option<RunMode>,
    target: Target,
    trigger: Trigger,
    #[serde(rename = "objective")]
    _objective: Objective,
    rules: Rules,
    execution: Execution,
    governance: Governance,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunMode {
    Continuous,
    OneShot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Target {
    filesystem: FileSystemId,
    #[serde(rename = "objectType")]
    _object_type: ObjectType,
    #[serde(rename = "consistency")]
    _consistency: Option<Consistency>,
    #[serde(rename = "where", default = "match_all")]
    condition: JsonValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ObjectType {
    File,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Consistency {
    Indexed,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Trigger {
    Manual,
    Periodic { interval: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Objective {
    Exhaust,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Execution {
    profile_ref: String,
    activation: Option<Activation>,
    admission_window: Option<JsonValue>,
    budget: Budget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    mode: ActivationMode,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ActivationMode {
    Preview,
    Enforced,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    run: RunBudget,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunBudget {
    max_operations: NonZeroU64,
    max_submit_attempts: NonZeroU64,
    max_object_logical_bytes: JsonValue,
    max_duration: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct BudgetLimits {
    pub operations: u64,
    pub attempts: u64,
    pub bytes: u64,
    pub duration_ms: Option<u64>,
}

impl RunBudget {
    fn limits(&self) -> BudgetLimits {
        BudgetLimits {
            operations: self.max_operations.get(),
            attempts: self.max_submit_attempts.get(),
            bytes: size_bytes(&self.max_object_logical_bytes).expect("validated budget"),
            duration_ms: self.max_duration.as_deref().and_then(duration_millis),
        }
    }

    fn validate(&self) -> Result<(), PolicyError> {
        size_bytes(&self.max_object_logical_bytes)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| invalid("spec.execution.budget.run.maxObjectLogicalBytes must be a positive size"))?;
        if let Some(duration) = &self.max_duration {
            duration_millis(duration)
                .ok_or_else(|| invalid("spec.execution.budget.run.maxDuration must be a positive Rev.8 duration"))?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Governance {
    authorization_ref: String,
    approval_policy_ref: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Rules {
    #[serde(rename = "strategy")]
    _strategy: Option<Strategy>,
    #[serde(rename = "parameterMerge")]
    _parameter_merge: Option<ParameterMerge>,
    #[serde(rename = "onNoMatch")]
    _on_no_match: Option<OnNoMatch>,
    #[serde(default)]
    exclude: Vec<JsonValue>,
    defaults: Option<RuleDefaults>,
    entries: Vec<Rule>,
}

struct CompiledRules {
    excludes: Condition,
    decision: String,
    params: Vec<SqlParam>,
    uses_read_heat: bool,
    execute_requires_heat: bool,
    actions: Vec<Invocation>,
}

impl Rules {
    fn compile(self, conditions: &mut ConditionCompiler<'_>) -> Result<CompiledRules, PolicyError> {
        if self.entries.is_empty() {
            return Err(invalid("spec.rules.entries must contain at least one rule"));
        }
        let default_action = self.defaults.and_then(|defaults| defaults.action);
        if let Some(action) = &default_action {
            let kind = action
                .kind
                .ok_or_else(|| invalid("spec.rules.defaults.action.type is required"))?;
            action.validate_defaults(kind)?;
        }
        let mut rules: Vec<_> = self.entries.into_iter().enumerate().collect();
        rules.sort_by_key(|(_, rule)| std::cmp::Reverse(rule.priority));
        let excludes = self
            .exclude
            .iter()
            .enumerate()
            .map(|(i, condition)| conditions.compile(condition, &format!("spec.rules.exclude[{i}]")))
            .collect::<Result<Vec<_>, _>>()?;
        let excludes = Condition::join("OR", excludes).negate();
        let mut uses_read_heat = false;
        let mut execute_requires_heat = true;
        let mut skip_requires_heat_to_pass = false;
        let mut clauses = vec!["CASE".to_owned()];
        let mut actions = Vec::new();
        let mut params = Vec::new();
        for (index, rule) in rules {
            let path = format!("spec.rules.entries[{index}]");
            let branch = match rule.decision {
                Decision::Execute | Decision::RequireApproval => {
                    let kind = rule
                        .action
                        .as_ref()
                        .and_then(|action| action.kind)
                        .or_else(|| default_action.as_ref().and_then(|action| action.kind))
                        .ok_or_else(|| {
                            invalid(format!("{path}.action.type is required when no default action is set"))
                        })?;
                    let mut params = default_action
                        .as_ref()
                        .filter(|action| action.kind == Some(kind))
                        .and_then(|action| action.params.clone())
                        .unwrap_or_default();
                    if let Some(rule_params) = rule.action.and_then(|action| action.params) {
                        params.extend(rule_params);
                    }
                    actions.push(Invocation::parse(
                        kind,
                        params,
                        matches!(rule.decision, Decision::RequireApproval),
                        &path,
                    )?);
                    actions.len()
                }
                Decision::Skip if rule.action.is_some() => {
                    return Err(invalid(format!("{path}.action cannot be set on a skip rule")));
                }
                Decision::Skip => 0,
            };
            let condition = conditions.compile(&rule.condition, &format!("{path}.match"))?;
            if branch > 0 {
                execute_requires_heat &= skip_requires_heat_to_pass || !condition.can_match_without_heat;
            } else if condition.uses_read_heat {
                // A prior skip that cannot be ruled out without heat already
                // guards all lower-priority execute decisions.
                skip_requires_heat_to_pass |= !condition.can_fail_without_heat;
            }
            if condition.uses_read_heat {
                // An earlier unknown rule could override the remainder of
                // first_match, so it must defer instead of falling through.
                uses_read_heat = true;
                clauses.push(format!(
                    "WHEN ({0}) IS NOT FALSE THEN CASE WHEN {0} THEN {branch} END",
                    condition.sql
                ));
                params.extend(condition.params.iter().cloned());
            } else {
                clauses.push(format!("WHEN {} THEN {branch}", condition.sql));
            }
            params.extend(condition.params);
        }
        clauses.push("ELSE 0 END".to_owned());
        Ok(CompiledRules {
            excludes,
            decision: clauses.join(" "),
            params,
            uses_read_heat,
            execute_requires_heat,
            actions,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    FirstMatch,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ParameterMerge {
    Explicit,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum OnNoMatch {
    Skip,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDefaults {
    action: Option<Action>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    #[serde(rename = "name")]
    _name: Option<String>,
    #[serde(default)]
    priority: i64,
    #[serde(rename = "match")]
    condition: JsonValue,
    decision: Decision,
    action: Option<Action>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Execute,
    RequireApproval,
    Skip,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    #[serde(rename = "type")]
    kind: Option<ActionKind>,
    params: Option<serde_json::Map<String, JsonValue>>,
    #[serde(rename = "targetRef")]
    _target_ref: Option<ActionTarget>,
}

impl Action {
    fn validate_defaults(&self, kind: ActionKind) -> Result<(), PolicyError> {
        let Some(params) = self.params.clone() else {
            return Ok(());
        };
        let path = "spec.rules.defaults";
        if kind == ActionKind::PoolMigrate {
            tiering::validate_parameter_names(&params, path)
        } else {
            Invocation::parse(kind, params, false, path).map(|_| ())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ActionKind {
    #[serde(rename = "lustre.hsm.archive")]
    Archive,
    #[serde(rename = "lustre.hsm.release")]
    Release,
    #[serde(rename = "lustre.hsm.restore")]
    Restore,
    #[serde(rename = "lustre.pool_migrate")]
    PoolMigrate,
}

impl ActionKind {
    pub const ALL: [Self; 4] = [Self::Archive, Self::Release, Self::Restore, Self::PoolMigrate];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Archive => "lustre.hsm.archive",
            Self::Release => "lustre.hsm.release",
            Self::Restore => "lustre.hsm.restore",
            Self::PoolMigrate => "lustre.pool_migrate",
        }
    }

    pub const fn effect(self) -> &'static str {
        match self {
            Self::Archive => "data_copy",
            Self::Release => "data_release",
            Self::Restore | Self::PoolMigrate => "data_copy",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    pub kind: ActionKind,
    pub archive_id: u32,
    pub(crate) requires_human: bool,
    pub(crate) migration: Option<PoolMigration>,
}

impl Invocation {
    fn parse(
        kind: ActionKind, params: serde_json::Map<String, JsonValue>, requires_human: bool, path: &str,
    ) -> Result<Self, PolicyError> {
        let (archive_id, migration) = if kind == ActionKind::PoolMigrate {
            (0, Some(PoolMigration::parse(params, path)?))
        } else {
            let params: ArchiveParams = serde_path_to_error::deserialize(&JsonValue::Object(params))
                .map_err(|error| invalid(format!("{path}.action.params: {error}")))?;
            if params.archive_id.is_some() && kind != ActionKind::Archive {
                return Err(invalid(format!(
                    "{path}.action.params.archiveId is only supported for archive"
                )));
            }
            (params.archive_id.unwrap_or_default(), None)
        };
        Ok(Self {
            kind,
            archive_id,
            requires_human,
            migration,
        })
    }
}

#[derive(Deserialize)]
enum ActionTarget {
    #[serde(rename = "current.object")]
    Object,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveParams {
    #[serde(rename = "archiveId")]
    archive_id: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldCondition {
    field: String,
    op: Comparison,
    value: JsonValue,
}

impl FieldCondition {
    fn compile(self, path: &str, now: DateTime<Utc>) -> Result<Predicate, PolicyError> {
        let as_string = || {
            self.value
                .as_str()
                .ok_or_else(|| invalid(format!("{path}.value must be a string")))
        };
        match self.field.as_str() {
            "object.size" => {
                let bytes = size_bytes(&self.value)
                    .and_then(|v| i64::try_from(v).ok())
                    .ok_or_else(|| invalid(format!("{path}.value must be a size within the supported range")))?;
                Ok(Predicate::Cmp {
                    field: Field::Size,
                    cmp: self.op.ordinal(path)?,
                    value: Value::Num(bytes),
                })
            }
            "object.last_modified" if matches!(self.op, Comparison::OlderThan) => {
                let age = as_string()
                    .ok()
                    .and_then(duration_millis)
                    .and_then(|ms| i64::try_from(ms.div_ceil(1000)).ok())
                    .ok_or_else(|| invalid(format!("{path}.value must be a positive Rev.8 duration")))?;
                let cutoff = now
                    .timestamp()
                    .checked_sub(age)
                    .ok_or_else(|| invalid(format!("{path}.value exceeds the timestamp range")))?;
                Ok(Predicate::Cmp {
                    field: Field::Mtime,
                    cmp: CmpOp::Le,
                    value: Value::Num(cutoff),
                })
            }
            "object.last_modified" => Err(invalid(format!(
                "{path}.op must be older_than for object.last_modified"
            ))),
            "lustre.pool" => {
                let cmp = self.op.equality(path)?;
                let pool = as_string()?;
                if !lustre_api::migration::valid_pool_name(pool) {
                    return Err(invalid(format!("{path}.value must be a Lustre pool name")));
                }
                let predicate = Predicate::InPool { pool: pool.into() };
                Ok(if matches!(cmp, CmpOp::Ne) {
                    Predicate::Not {
                        inner: Box::new(predicate),
                    }
                } else {
                    predicate
                })
            }
            field if field.starts_with("object.tags.") => {
                let key = &field["object.tags.".len()..];
                if key.is_empty()
                    || key.len() > 64
                    || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(invalid(format!("{path}.field requires a classifier tag name")));
                }
                Ok(Predicate::Xattr {
                    key: key.into(),
                    cmp: self.op.equality(path)?,
                    value: Value::Str(as_string()?.into()),
                })
            }
            field => Err(invalid(format!("{path}.field is unsupported: {field}"))),
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Comparison {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    OlderThan,
}

impl Comparison {
    fn ordinal(self, path: &str) -> Result<CmpOp, PolicyError> {
        Ok(match self {
            Self::Eq => CmpOp::Eq,
            Self::Ne => CmpOp::Ne,
            Self::Lt => CmpOp::Lt,
            Self::Lte => CmpOp::Le,
            Self::Gt => CmpOp::Gt,
            Self::Gte => CmpOp::Ge,
            Self::OlderThan => return Err(invalid(format!("{path}.op older_than requires object.last_modified"))),
        })
    }

    fn equality(self, path: &str) -> Result<CmpOp, PolicyError> {
        match self {
            Self::Eq => Ok(CmpOp::Eq),
            Self::Ne => Ok(CmpOp::Ne),
            _ => Err(invalid(format!("{path}.op requires eq or ne"))),
        }
    }
}

pub(crate) fn validate_selector(name: &str, selectors: &BTreeMap<String, JsonValue>) -> Result<(), PolicyError> {
    ConditionCompiler::new(selectors).compile(&serde_json::json!({"selectorRef": name}), "spec.where")?;
    Ok(())
}

struct Condition {
    sql: String,
    params: Vec<SqlParam>,
    uses_read_heat: bool,
    // Conservative Boolean outcomes when every heat comparison is SQL NULL.
    can_match_without_heat: bool,
    can_fail_without_heat: bool,
}

impl Condition {
    fn indexed(predicate: Predicate) -> Self {
        let (sql, params) = rbh_predicate::to_sql(&predicate);
        Self {
            sql,
            params,
            uses_read_heat: false,
            can_match_without_heat: !matches!(predicate, Predicate::False),
            can_fail_without_heat: !matches!(predicate, Predicate::True),
        }
    }

    fn negate(mut self) -> Self {
        self.sql = format!("NOT ({})", self.sql);
        std::mem::swap(&mut self.can_match_without_heat, &mut self.can_fail_without_heat);
        self
    }

    fn join(operator: &str, conditions: Vec<Self>) -> Self {
        if conditions.is_empty() {
            return Self::indexed(if operator == "AND" {
                Predicate::True
            } else {
                Predicate::False
            });
        }
        let uses_read_heat = conditions.iter().any(|condition| condition.uses_read_heat);
        let (can_match_without_heat, can_fail_without_heat) = if operator == "AND" {
            (
                conditions.iter().all(|condition| condition.can_match_without_heat),
                conditions.iter().any(|condition| condition.can_fail_without_heat),
            )
        } else {
            (
                conditions.iter().any(|condition| condition.can_match_without_heat),
                conditions.iter().all(|condition| condition.can_fail_without_heat),
            )
        };
        let mut parts = Vec::with_capacity(conditions.len());
        let mut params = Vec::new();
        for condition in conditions {
            // A missing indexed attribute is a non-match at the boundary with a
            // heat expression. Only unavailable heat propagates as SQL NULL.
            parts.push(if uses_read_heat && !condition.uses_read_heat {
                format!("({}) IS TRUE", condition.sql)
            } else {
                condition.sql
            });
            params.extend(condition.params);
        }
        Self {
            sql: format!("({})", parts.join(&format!(" {operator} "))),
            params,
            uses_read_heat,
            can_match_without_heat,
            can_fail_without_heat,
        }
    }
}

struct ConditionCompiler<'a> {
    selectors: &'a BTreeMap<String, JsonValue>,
    stack: Vec<String>,
    referenced: BTreeSet<String>,
    remaining: usize,
    now: DateTime<Utc>,
}

impl<'a> ConditionCompiler<'a> {
    fn new(selectors: &'a BTreeMap<String, JsonValue>) -> Self {
        Self {
            selectors,
            stack: Vec::new(),
            referenced: BTreeSet::new(),
            remaining: 4096,
            now: Utc::now(),
        }
    }

    fn compile(&mut self, expr: &JsonValue, path: &str) -> Result<Condition, PolicyError> {
        self.compile_node(expr, path, 0)
    }

    fn compile_node(&mut self, expr: &JsonValue, path: &str, depth: usize) -> Result<Condition, PolicyError> {
        if depth >= 128 {
            return Err(invalid(format!("{path} exceeds the condition nesting limit")));
        }
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| invalid("condition expression is too large after resolving selectors"))?;

        if let Some(matches) = expr.as_bool() {
            return Ok(Condition::indexed(if matches {
                Predicate::True
            } else {
                Predicate::False
            }));
        }
        let fields = expr
            .as_object()
            .ok_or_else(|| invalid(format!("{path} must be a boolean or a condition object")))?;
        if fields.contains_key("field") || fields.contains_key("op") || fields.contains_key("value") {
            let condition: FieldCondition = serde_path_to_error::deserialize(expr).map_err(|error| {
                let field = error.path().to_string();
                let message = error.into_inner();
                if field == "." {
                    invalid(format!("{path}: {message}"))
                } else {
                    invalid(format!("{path}.{field}: {message}"))
                }
            })?;
            if let Some(column) = heat::score_column(&condition.field) {
                let value = condition
                    .value
                    .as_i64()
                    .filter(|value| *value >= 0)
                    .ok_or_else(|| invalid(format!("{path}.value must be a non-negative heat score")))?;
                return Ok(Condition {
                    sql: format!("read_heat.{column} {} ?", condition.op.ordinal(path)?.sql()),
                    params: vec![SqlParam::Num(value)],
                    uses_read_heat: true,
                    can_match_without_heat: false,
                    can_fail_without_heat: false,
                });
            }
            return condition.compile(path, self.now).map(Condition::indexed);
        }
        let Some((key, operand)) = fields.iter().next().filter(|_| fields.len() == 1) else {
            return Err(invalid(format!("{path} must contain exactly one condition operator")));
        };
        let operator_path = format!("{path}.{key}");
        match key.as_str() {
            "selectorRef" => {
                let name = operand
                    .as_str()
                    .ok_or_else(|| invalid(format!("{operator_path} must be a selector name")))?;
                if self.stack.iter().any(|node| node == name) {
                    return Err(invalid(format!(
                        "{operator_path} creates a selector cycle through {name}"
                    )));
                }
                let condition = self
                    .selectors
                    .get(name)
                    .ok_or_else(|| invalid(format!("{operator_path} references unknown Selector/{name}")))?;
                self.referenced.insert(name.to_owned());
                self.stack.push(name.to_owned());
                let result = self.compile_node(condition, &format!("Selector/{name}.spec.where"), depth + 1);
                self.stack.pop();
                result
            }
            "not" => Ok(self.compile_node(operand, &operator_path, depth + 1)?.negate()),
            "all" | "any" => {
                let conditions = operand
                    .as_array()
                    .filter(|conditions| !conditions.is_empty())
                    .ok_or_else(|| invalid(format!("{operator_path} must contain at least one condition")))?;
                let children = conditions
                    .iter()
                    .enumerate()
                    .map(|(index, condition)| {
                        self.compile_node(condition, &format!("{operator_path}[{index}]"), depth + 1)
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Condition::join(if key == "all" { "AND" } else { "OR" }, children))
            }
            _ => Err(invalid(format!("{operator_path} is an unsupported condition operator"))),
        }
    }
}

fn match_all() -> JsonValue {
    JsonValue::Bool(true)
}

pub(crate) fn invalid(message: impl Into<String>) -> PolicyError {
    PolicyError::InvalidPolicy(message.into())
}

pub(crate) fn valid_name(name: &str) -> bool {
    fn alphanumeric(byte: u8) -> bool {
        byte.is_ascii_lowercase() || byte.is_ascii_digit()
    }
    let bytes = name.as_bytes();
    bytes.len() <= 253
        && bytes.first().copied().is_some_and(alphanumeric)
        && bytes.last().copied().is_some_and(alphanumeric)
        && bytes
            .iter()
            .all(|&byte| alphanumeric(byte) || matches!(byte, b'-' | b'.'))
}

fn quantity(text: &str, units: &[(&str, u64)]) -> Option<u64> {
    let (number, unit) = text.split_at(text.bytes().take_while(u8::is_ascii_digit).count());
    if number.is_empty() || number.starts_with('0') {
        return None;
    }
    let (_, multiplier) = units.iter().find(|(suffix, _)| *suffix == unit)?;
    number.parse::<u64>().ok()?.checked_mul(*multiplier)
}

fn duration_millis(value: &str) -> Option<u64> {
    quantity(
        value,
        &[
            ("ms", 1),
            ("s", 1_000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", 86_400_000),
        ],
    )
}

fn size_bytes(value: &JsonValue) -> Option<u64> {
    value.as_u64().or_else(|| {
        quantity(
            value.as_str()?,
            &[
                ("B", 1),
                ("KiB", 1 << 10),
                ("MiB", 1 << 20),
                ("GiB", 1 << 30),
                ("TiB", 1 << 40),
                ("PiB", 1 << 50),
            ],
        )
    })
}
