use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{API_VERSION, ActionKind, BUILTIN_PREVIEW, duration_millis, invalid, size_bytes};
use crate::PolicyError;

pub const ARCHIVE: &str = "lustre.hsm.archive";
pub fn contract_revision(action: ActionKind) -> u64 {
    if action == ActionKind::Archive { 2 } else { 1 }
}

pub fn original_archive_contract() -> Value {
    let mut contract = action_contract(ActionKind::Archive);
    contract["spec"]["completionGuarantees"][0]["evidence"] = "hsm_completion_and_authoritative_state".into();
    contract["spec"]["preconditions"]["beforeSubmit"]
        .as_array_mut()
        .expect("contract preconditions")
        .pop();
    contract
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ResourceKind {
    Selector,
    ExecutionProfile,
    AuthorizationPolicy,
    ApprovalPolicy,
    SystemSafetyPolicy,
    ActionContract,
}

impl ResourceKind {
    pub const MANAGED: [Self; 5] = [
        Self::Selector,
        Self::ExecutionProfile,
        Self::AuthorizationPolicy,
        Self::ApprovalPolicy,
        Self::SystemSafetyPolicy,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Selector => "Selector",
            Self::ExecutionProfile => "ExecutionProfile",
            Self::AuthorizationPolicy => "AuthorizationPolicy",
            Self::ApprovalPolicy => "ApprovalPolicy",
            Self::SystemSafetyPolicy => "SystemSafetyPolicy",
            Self::ActionContract => "ActionContract",
        }
    }

    pub fn from_name(kind: &str) -> Option<Self> {
        Self::MANAGED.into_iter().find(|value| value.as_str() == kind)
    }

    pub fn has_builtin_preview(self) -> bool {
        matches!(
            self,
            Self::ExecutionProfile | Self::AuthorizationPolicy | Self::ApprovalPolicy
        )
    }

    fn schema(self) -> &'static str {
        match self {
            Self::Selector => include_str!("schemas/selector.schema.json"),
            Self::ExecutionProfile => include_str!("schemas/execution-profile.schema.json"),
            Self::AuthorizationPolicy => include_str!("schemas/authorization.schema.json"),
            Self::ApprovalPolicy => include_str!("schemas/approval.schema.json"),
            Self::SystemSafetyPolicy => include_str!("schemas/system-safety.schema.json"),
            Self::ActionContract => unreachable!("contracts are registered by the server"),
        }
    }

    pub fn validate(self, document: &Value) -> Result<&str, PolicyError> {
        static SCHEMAS: OnceLock<BTreeMap<ResourceKind, Validator>> = OnceLock::new();
        let schemas = SCHEMAS.get_or_init(|| {
            Self::MANAGED
                .into_iter()
                .map(|kind| {
                    let schema = serde_json::from_str(kind.schema()).expect("valid embedded Rev.8 schema JSON");
                    let validator = jsonschema::validator_for(&schema).expect("valid embedded Rev.8 schema");
                    (kind, validator)
                })
                .collect()
        });
        let validator = schemas
            .get(&self)
            .ok_or_else(|| invalid("ActionContract is read-only"))?;
        validator
            .validate(document)
            .map_err(|error| invalid(format!("{}: {error}", error.instance_path())))?;
        let name = document["metadata"]["name"].as_str().expect("schema requires a name");
        if self.has_builtin_preview() && name == BUILTIN_PREVIEW {
            return Err(invalid("metadata.name is reserved by the server"));
        }
        let spec = &document["spec"];
        if matches!(self, Self::AuthorizationPolicy | Self::ApprovalPolicy) && spec["defaultDecision"] != "deny" {
            return Err(invalid("spec.defaultDecision must be deny"));
        }
        match self {
            Self::Selector if spec.get("roots").is_some() || spec.get("maxDepth").is_some() => {
                return Err(invalid(
                    "spec.roots and spec.maxDepth are unsupported in indexed preview",
                ));
            }
            Self::ExecutionProfile => validate_profile(spec)?,
            Self::AuthorizationPolicy => {
                let statements = spec["statements"].as_array().expect("schema requires statements");
                unique_ids(statements, "spec.statements")?;
                for statement in statements {
                    validate_actions(&statement["actions"], "spec.statements.actions")?;
                }
            }
            Self::ApprovalPolicy => validate_approval(spec)?,
            Self::SystemSafetyPolicy => {
                validate_actions(&spec["scope"]["actions"], "spec.scope.actions")?;
                validate_actions(&spec["allowedActions"], "spec.allowedActions")?;
                if let Some(bytes) = spec["limits"].get("maxObjectLogicalBytes")
                    && size_bytes(bytes).is_none()
                {
                    return Err(invalid("spec.limits.maxObjectLogicalBytes is out of range"));
                }
                if let Some(duration) = spec["limits"].get("maxDuration") {
                    read_duration(duration, "spec.limits.maxDuration")?;
                }
            }
            _ => {}
        }
        Ok(name)
    }
}

impl TryFrom<String> for ResourceKind {
    type Error = PolicyError;

    fn try_from(kind: String) -> Result<Self, Self::Error> {
        Self::from_name(&kind).ok_or_else(|| PolicyError::Store(format!("invalid persisted resource kind: {kind}")))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResourceBinding {
    pub kind: ResourceKind,
    pub name: String,
    pub revision: u64,
    #[serde(rename = "serverDefinition", skip_serializing_if = "Option::is_none")]
    pub server_definition: Option<Value>,
}

// Published server revisions are append-only. A new contract goes in a new
// revision and leaves the existing builder available for historical reads.
pub fn action_contract(action: ActionKind) -> Value {
    let (completion, readiness) = match action {
        ActionKind::Archive => ("archive_completed", "file_archive_eligible"),
        ActionKind::Release => ("release_completed", "file_release_eligible"),
        ActionKind::Restore => ("restore_completed", "file_restore_eligible"),
    };
    let name = action.name();
    let effect = action.effect();
    json!({
        "apiVersion": API_VERSION, "kind": "ActionContract", "metadata": {"name": name},
        "spec": {
            "targetTypes": ["file"],
            "effect": {"category": effect, "reversibility": "recoverable"},
            "identity": {"objectKey": "filesystem_fid", "required": ["filesystemId", "fid", "sourceGeneration"]},
            "idempotency": {"strategy": "operation_generation", "reconciliationRequired": true},
            "metering": [
                {"key": "maxOperations", "required": true, "unit": "count", "basis": "logical_operation", "chargeAt": "admission"},
                {"key": "maxSubmitAttempts", "required": true, "unit": "count", "basis": "submit_attempt", "chargeAt": "before_submit"},
                {"key": "maxObjectLogicalBytes", "required": true, "unit": "bytes", "basis": "object_logical_size_at_admission", "chargeAt": "admission"}
            ],
            "compatibility": {"default": "reject"},
            "completionGuarantees": [{"name": completion, "evidence": "authoritative_hsm_state"}],
            "preconditions": {"beforeSubmit": [
                {"name": "source_generation_matches", "requiredWhen": "always"},
                {"name": readiness, "requiredWhen": "always"}
            ]},
            "safety": {"objectChangePolicy": "defer", "authoritativeValidation": "required"}
        }
    })
}

pub(crate) fn safety_matches(spec: &Value, filesystem: &str, action: ActionKind) -> bool {
    let scope = &spec["scope"];
    [
        ("filesystems", filesystem),
        ("actions", action.name()),
        ("effectCategories", action.effect()),
    ]
    .into_iter()
    .all(|(field, target)| {
        scope[field]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item == target))
    })
}

pub(crate) fn approval_matches(rule: &Value, filesystem: &str, action: ActionKind) -> bool {
    rule["scope"]["filesystems"]
        .as_array()
        .is_none_or(|values| values.iter().any(|value| value == filesystem))
        && [(&rule["actions"], action.name()), (&rule["effects"], action.effect())]
            .into_iter()
            .any(|(values, target)| {
                values
                    .as_array()
                    .is_some_and(|values| values.iter().any(|value| value == target))
            })
}

pub(crate) fn validate_approval_floor(
    approval: &Value, filesystem: &str, action: ActionKind,
) -> Result<(), PolicyError> {
    if approval["floorDefaults"].is_object() {
        return Ok(());
    }
    let modes: Vec<_> = approval["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| approval_matches(rule, filesystem, action))
        .map(|rule| &rule["mode"])
        .collect();
    if !modes.is_empty() && modes.iter().all(|mode| *mode == "not_required") {
        return Err(invalid(format!(
            "APPROVAL_FLOOR_UNSATISFIABLE: {} approvalPolicy requires floorDefaults",
            action.name()
        )));
    }
    Ok(())
}

fn validate_actions(actions: &Value, field: &str) -> Result<(), PolicyError> {
    if let Some(actions) = actions.as_array()
        && let Some(action) = actions
            .iter()
            .find(|action| action.as_str().and_then(ActionKind::from_name).is_none())
    {
        return Err(invalid(format!(
            "{field} references an unregistered ActionContract: {action}"
        )));
    }
    Ok(())
}

fn unique_ids(items: &[Value], field: &str) -> Result<(), PolicyError> {
    let mut seen = BTreeSet::new();
    for item in items {
        let id = item["id"].as_str().expect("schema requires an id");
        if !seen.insert(id) {
            return Err(invalid(format!("{field} repeats id {id}")));
        }
    }
    Ok(())
}

fn validate_approval(spec: &Value) -> Result<(), PolicyError> {
    let Some(requirements) = spec["requirements"].as_array() else {
        return Ok(());
    };
    unique_ids(requirements, "spec.requirements")?;
    let grant_fields = ["minApprovers", "approverRoles", "separationOfDuties", "ttl", "binding"];
    for item in requirements {
        let id = &item["id"];
        if !["effects", "actions"]
            .into_iter()
            .any(|field| item[field].as_array().is_some_and(|items| !items.is_empty()))
        {
            return Err(invalid(format!("spec.requirements {id} requires effects or actions")));
        }
        validate_actions(&item["actions"], "spec.requirements.actions")?;
        let required = item["mode"] == "required";
        if let Some(field) = grant_fields
            .iter()
            .find(|field| item.get(**field).is_some() != required)
        {
            let requirement = if required { "requires" } else { "must omit" };
            return Err(invalid(format!("spec.requirements {id} {requirement} {field}")));
        }
        if required {
            read_duration(&item["ttl"], "spec.requirements.ttl")?;
        }
    }
    Ok(())
}

fn read_duration(value: &Value, field: &str) -> Result<u64, PolicyError> {
    value
        .as_str()
        .and_then(duration_millis)
        .ok_or_else(|| invalid(format!("{field} must be a positive Rev.8 duration")))
}

fn validate_profile(spec: &Value) -> Result<(), PolicyError> {
    if let Some(window) = spec.get("admissionWindow") {
        if window["timezone"]
            .as_str()
            .and_then(|zone| zone.parse::<chrono_tz::Tz>().ok())
            .is_none()
        {
            return Err(invalid("spec.admissionWindow.timezone must be an IANA timezone"));
        }
        if !window["weekly"].is_array() {
            return Err(invalid("spec.admissionWindow.weekly is required"));
        }
    }
    let retry = &spec["retry"];
    let initial = read_duration(&retry["initialDelay"], "spec.retry.initialDelay")?;
    let max = read_duration(&retry["maxDelay"], "spec.retry.maxDelay")?;
    if initial > max {
        return Err(invalid("spec.retry.initialDelay exceeds spec.retry.maxDelay"));
    }
    if retry["retryableErrors"]
        .as_array()
        .is_some_and(|errors| errors.iter().any(|error| error == "unknown_submission"))
    {
        return Err(invalid("spec.retry.retryableErrors cannot contain unknown_submission"));
    }
    let lease = &spec["workerLease"];
    let ttl = read_duration(&lease["ttl"], "spec.workerLease.ttl")?;
    let renew = read_duration(&lease["renewEvery"], "spec.workerLease.renewEvery")?;
    if renew >= ttl {
        return Err(invalid("spec.workerLease.renewEvery must be shorter than ttl"));
    }
    for scope in spec["circuitBreaker"]["scopes"]
        .as_array()
        .expect("schema requires scopes")
    {
        for condition in scope["conditions"].as_array().expect("schema requires conditions") {
            let kind = condition["type"].as_str().expect("schema requires a condition type");
            if kind != "consecutive_failures" {
                read_duration(&condition["window"], "spec.circuitBreaker.conditions.window")?;
            }
            if kind == "failure_rate" {
                if condition["threshold"].as_f64().is_none_or(|threshold| threshold > 1.0)
                    || condition["minSamples"].as_u64().is_none()
                {
                    return Err(invalid("failure_rate requires threshold in [0, 1] and minSamples"));
                }
            } else if condition["threshold"].as_u64().is_none_or(|count| count == 0) {
                return Err(invalid("circuit breaker count threshold must be a positive integer"));
            }
        }
    }
    Ok(())
}
