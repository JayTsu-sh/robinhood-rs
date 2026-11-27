//! Compile the native HSM archive preview supported by the Rev.8 API.

use std::num::NonZeroU64;

use rbh_entry_store::store::{EntryStore, QueryParam};
use rbh_entry_store::{BackendKind, EntryKind, FileSystemId};
use rbh_predicate::{CmpOp, Field, Predicate, SqlParam, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::PolicyError;

const API_VERSION: &str = "hsm.storage.io/v1alpha3";
const BUILTIN_PREVIEW: &str = "builtin-preview";

pub struct CompiledPolicy {
    pub name: String,
    pub filesystem: FileSystemId,
    where_clause: String,
    params: Vec<QueryParam>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewResult {
    pub revision: u64,
    pub candidate_count: u64,
    pub estimated_bytes: u64,
}

/// Recognize versioned documents before attempting to parse the older API model.
pub fn is_versioned_document(document: &JsonValue) -> bool {
    ["apiVersion", "metadata", "spec"]
        .iter()
        .any(|key| document.get(key).is_some())
        || document.get("kind").and_then(JsonValue::as_str) == Some("Policy")
}

pub fn compile(document: &JsonValue) -> Result<CompiledPolicy, PolicyError> {
    if document.get("apiVersion").and_then(JsonValue::as_str) != Some(API_VERSION) {
        return Err(invalid(format!("apiVersion must be {API_VERSION}")));
    }
    let policy: Policy = serde_path_to_error::deserialize(document).map_err(|error| invalid(format!("{error}")))?;
    if !valid_name(&policy.metadata.name) {
        return Err(invalid(
            "metadata.name must be a lowercase resource name of 1–253 characters",
        ));
    }
    let spec = policy.spec;
    if let Trigger::Periodic { interval } = &spec.trigger {
        duration_millis(interval).ok_or_else(|| invalid("spec.trigger.interval must be a positive Rev.8 duration"))?;
    }
    for (path, reference) in [
        ("spec.execution.profileRef", spec.execution.profile_ref.as_str()),
        (
            "spec.governance.authorizationRef",
            spec.governance.authorization_ref.as_str(),
        ),
        (
            "spec.governance.approvalPolicyRef",
            spec.governance.approval_policy_ref.as_str(),
        ),
    ] {
        if reference != BUILTIN_PREVIEW {
            return Err(invalid(format!(
                "{path} must reference {BUILTIN_PREVIEW} for this preview"
            )));
        }
    }
    spec.execution.budget.run.validate()?;
    let target_condition = compile_condition(&spec.target.condition, "spec.target.where")?;
    let files = Predicate::Cmp {
        field: Field::Kind,
        cmp: CmpOp::Eq,
        value: Value::Num(EntryKind::File as i64),
    };
    let (mut where_clause, mut params) = rbh_predicate::to_sql(&Predicate::And {
        children: vec![files, target_condition],
    });
    let (rules, rule_params) = spec.rules.compile()?;
    where_clause.push_str(" AND ");
    where_clause.push_str(&rules);
    params.extend(rule_params);
    Ok(CompiledPolicy {
        name: policy.metadata.name,
        filesystem: spec.target.filesystem,
        where_clause,
        params: params
            .into_iter()
            .map(|param| match param {
                SqlParam::Num(value) => QueryParam::Int(value),
                SqlParam::Str(value) => QueryParam::Str(value),
            })
            .collect(),
    })
}

impl CompiledPolicy {
    #[tracing::instrument(skip_all, fields(filesystem = %self.filesystem))]
    pub async fn validate_filesystem(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        let config = entries
            .get_filesystem(&self.filesystem)
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?
            .ok_or_else(|| invalid(format!("spec.target.filesystem is not registered: {}", self.filesystem)))?;
        if config.backend != BackendKind::Lustre || !config.capabilities.hsm {
            return Err(invalid(format!(
                "spec.target.filesystem does not support Lustre HSM: {}",
                self.filesystem
            )));
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(filesystem = %self.filesystem, revision))]
    pub async fn preview(&self, entries: &EntryStore, revision: u64) -> Result<PreviewResult, PolicyError> {
        self.validate_filesystem(entries).await?;
        let (candidate_count, estimated_bytes) = entries
            .scoped_totals_where(&self.filesystem, &self.where_clause, &self.params)
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?;
        Ok(PreviewResult {
            revision,
            candidate_count,
            estimated_bytes,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
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
    #[serde(rename = "enabled")]
    _enabled: Option<bool>,
    #[serde(rename = "priority")]
    _priority: Option<u64>,
    #[serde(rename = "mode")]
    _mode: Option<RunMode>,
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
    #[serde(rename = "activation")]
    _activation: Option<Activation>,
    budget: Budget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    #[serde(rename = "mode")]
    _mode: ActivationMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActivationMode {
    Preview,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    run: RunBudget,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunBudget {
    #[serde(rename = "maxOperations")]
    _max_operations: NonZeroU64,
    #[serde(rename = "maxSubmitAttempts")]
    _max_submit_attempts: NonZeroU64,
    max_object_logical_bytes: JsonValue,
    max_duration: Option<String>,
}

impl RunBudget {
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

impl Rules {
    fn compile(self) -> Result<(String, Vec<SqlParam>), PolicyError> {
        if self.entries.is_empty() {
            return Err(invalid("spec.rules.entries must contain at least one rule"));
        }
        let default_action = self.defaults.and_then(|defaults| defaults.action);
        if let Some(action) = &default_action
            && action.kind.is_none()
        {
            return Err(invalid("spec.rules.defaults.action.type is required"));
        }
        let mut rules: Vec<_> = self.entries.into_iter().enumerate().collect();
        rules.sort_by_key(|(_, rule)| std::cmp::Reverse(rule.priority));
        let excludes = self
            .exclude
            .iter()
            .enumerate()
            .map(|(i, condition)| compile_condition(condition, &format!("spec.rules.exclude[{i}]")))
            .collect::<Result<Vec<_>, _>>()?;
        let (exclude, mut params) = rbh_predicate::to_sql(&Predicate::Not {
            inner: Box::new(Predicate::Or { children: excludes }),
        });
        let mut clauses = vec![exclude, "AND CASE".to_owned()];
        for (index, rule) in rules {
            let path = format!("spec.rules.entries[{index}]");
            let execute = match rule.decision {
                Decision::Execute => {
                    let action_kind = rule
                        .action
                        .as_ref()
                        .and_then(|action| action.kind)
                        .or_else(|| default_action.as_ref().and_then(|action| action.kind));
                    if action_kind.is_none() {
                        return Err(invalid(format!(
                            "{path}.action.type is required when no default action is set"
                        )));
                    }
                    true
                }
                Decision::Skip if rule.action.is_some() => {
                    return Err(invalid(format!("{path}.action cannot be set on a skip rule")));
                }
                Decision::Skip => false,
            };
            let (condition, rule_params) =
                rbh_predicate::to_sql(&compile_condition(&rule.condition, &format!("{path}.match"))?);
            clauses.push(format!("WHEN {condition} THEN {}", u8::from(execute)));
            params.extend(rule_params);
        }
        clauses.push("ELSE 0 END = 1".to_owned());
        Ok((clauses.join(" "), params))
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
    Skip,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    #[serde(rename = "type")]
    kind: Option<ActionKind>,
    #[serde(rename = "params")]
    _params: Option<ArchiveParams>,
    #[serde(rename = "targetRef")]
    _target_ref: Option<ActionTarget>,
}

#[derive(Clone, Copy, Deserialize)]
enum ActionKind {
    #[serde(rename = "lustre.hsm.archive")]
    Archive,
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
    _archive_id: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldCondition {
    field: String,
    op: Comparison,
    value: JsonValue,
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
}

impl From<Comparison> for CmpOp {
    fn from(comparison: Comparison) -> Self {
        match comparison {
            Comparison::Eq => Self::Eq,
            Comparison::Ne => Self::Ne,
            Comparison::Lt => Self::Lt,
            Comparison::Lte => Self::Le,
            Comparison::Gt => Self::Gt,
            Comparison::Gte => Self::Ge,
        }
    }
}

fn compile_condition(expr: &JsonValue, path: &str) -> Result<Predicate, PolicyError> {
    if let Some(matches) = expr.as_bool() {
        return Ok(if matches { Predicate::True } else { Predicate::False });
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
        if condition.field != "object.size" {
            return Err(invalid(format!("{path}.field is unsupported: {}", condition.field)));
        }
        let bytes = size_bytes(&condition.value)
            .and_then(|size| i64::try_from(size).ok())
            .ok_or_else(|| invalid(format!("{path}.value must be a size within the supported range")))?;
        return Ok(Predicate::Cmp {
            field: Field::Size,
            cmp: condition.op.into(),
            value: Value::Num(bytes),
        });
    }
    let Some((key, operand)) = fields.iter().next().filter(|_| fields.len() == 1) else {
        return Err(invalid(format!("{path} must contain exactly one condition operator")));
    };
    let operator_path = format!("{path}.{key}");
    match key.as_str() {
        "not" => Ok(Predicate::Not {
            inner: Box::new(compile_condition(operand, &operator_path)?),
        }),
        "all" | "any" => {
            let conditions = operand
                .as_array()
                .filter(|conditions| !conditions.is_empty())
                .ok_or_else(|| invalid(format!("{operator_path} must contain at least one condition")))?;
            let children = conditions
                .iter()
                .enumerate()
                .map(|(index, condition)| compile_condition(condition, &format!("{operator_path}[{index}]")))
                .collect::<Result<_, _>>()?;
            Ok(if key == "all" {
                Predicate::And { children }
            } else {
                Predicate::Or { children }
            })
        }
        _ => Err(invalid(format!("{operator_path} is an unsupported condition operator"))),
    }
}

fn match_all() -> JsonValue {
    JsonValue::Bool(true)
}

fn invalid(message: impl Into<String>) -> PolicyError {
    PolicyError::InvalidPolicy(message.into())
}

fn valid_name(name: &str) -> bool {
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
