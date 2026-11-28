use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{MySql, Transaction, types::Json};

use super::{POLICY_REVISION_FIELDS, PolicyRevision, PolicyStore, record_policy_revision};
use crate::{
    PolicyError,
    policy::{
        self, ActionKind, BUILTIN_PREVIEW, CompiledPolicy, Policy,
        execution::{Admission, Principal},
        resources::{
            ARCHIVE, ResourceBinding, ResourceKind, action_contract, contract_revision, safety_matches,
            validate_approval_floor,
        },
    },
};

const CURRENT_RESOURCES: &str = "SELECT CAST(r.kind AS CHAR CHARACTER SET utf8mb4) AS kind, \
    CAST(r.name AS CHAR CHARACTER SET utf8mb4) AS name, r.revision, r.definition, r.modified_by \
    FROM policy_resource_heads h JOIN policy_resource_revisions r USING (kind, name, revision) WHERE NOT h.deleted";
const HISTORICAL_RESOURCE: &str = "SELECT CAST(kind AS CHAR CHARACTER SET utf8mb4) AS kind, \
    CAST(name AS CHAR CHARACTER SET utf8mb4) AS name, revision, definition, modified_by \
    FROM policy_resource_revisions WHERE kind = ? AND name = ? AND revision = ?";

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRevision {
    #[sqlx(try_from = "String")]
    pub kind: ResourceKind,
    pub name: String,
    pub revision: u64,
    pub definition: Option<Json<Value>>,
    pub modified_by: String,
}

impl ResourceRevision {
    fn binding(&self) -> ResourceBinding {
        ResourceBinding {
            kind: self.kind,
            name: self.name.clone(),
            revision: self.revision,
            server_definition: None,
        }
    }
}

impl PolicyStore {
    pub(super) async fn lock_configuration(&self) -> Result<Transaction<'_, MySql>, PolicyError> {
        // Configuration writes are infrequent. A shared row serializes reference
        // resolution, updates and deletion across daemon processes.
        let mut tx = self.pool.begin().await?;
        sqlx::query_scalar::<_, u8>("SELECT id FROM policy_configuration_lock WHERE id = 1 FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
        Ok(tx)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_resources(&self, kind: ResourceKind) -> Result<Vec<ResourceRevision>, PolicyError> {
        let sql = format!("{CURRENT_RESOURCES} AND h.kind = ? ORDER BY h.name");
        let mut resources = sqlx::query_as(&sql).bind(kind.as_str()).fetch_all(&self.pool).await?;
        resources.extend(builtin_preview(kind));
        resources.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(resources)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_resource(
        &self, kind: ResourceKind, name: &str, revision: Option<u64>,
    ) -> Result<ResourceRevision, PolicyError> {
        if !policy::valid_name(name) {
            return Err(not_found(kind, name));
        }
        if name == BUILTIN_PREVIEW && kind.has_builtin_preview() {
            return builtin_preview(kind)
                .filter(|resource| revision.is_none_or(|revision| revision == resource.revision))
                .ok_or_else(|| not_found(kind, name));
        }
        let row = if let Some(revision) = revision {
            sqlx::query_as(HISTORICAL_RESOURCE)
                .bind(kind.as_str())
                .bind(name)
                .bind(revision)
                .fetch_optional(&self.pool)
                .await?
        } else {
            let sql = format!("{CURRENT_RESOURCES} AND h.kind = ? AND h.name = ?");
            sqlx::query_as(&sql)
                .bind(kind.as_str())
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
        };
        row.ok_or_else(|| not_found(kind, name))
    }

    #[tracing::instrument(skip(self, document))]
    pub async fn save_resource(
        &self, kind: ResourceKind, path_name: Option<&str>, document: Value, actor: &str,
    ) -> Result<ResourceRevision, PolicyError> {
        let name = kind.validate(&document)?.to_owned();
        if path_name.is_some_and(|path_name| path_name != name) {
            return Err(PolicyError::InvalidPolicy(
                "metadata.name must match the resource URL".into(),
            ));
        }
        let mut tx = self.lock_configuration().await?;
        let head = read_head(&mut tx, kind, &name).await?;
        match (path_name, head) {
            (None, Some((_, false))) => return Err(PolicyError::NameConflict(name)),
            (Some(_), None | Some((_, true))) => return Err(not_found(kind, &name)),
            _ => {}
        }
        if kind == ResourceKind::Selector {
            let resources = current_resources(&mut tx).await?;
            let mut selectors = selector_map(&resources);
            selectors.insert(name.clone(), document["spec"]["where"].clone());
            policy::validate_selector(&name, &selectors)?;
        }
        let revision = head.map_or(0, |(revision, _)| revision) + 1;
        record_resource(&mut tx, kind, &name, revision, Some(&document), actor).await?;
        if kind == ResourceKind::SystemSafetyPolicy {
            rebind_safety(&mut tx, actor).await?;
        }
        tx.commit().await?;
        Ok(ResourceRevision {
            kind,
            name,
            revision,
            definition: Some(Json(document)),
            modified_by: actor.into(),
        })
    }

    #[tracing::instrument(skip(self))]
    pub async fn delete_resource(&self, kind: ResourceKind, name: &str, actor: &str) -> Result<(), PolicyError> {
        if !policy::valid_name(name) {
            return Err(not_found(kind, name));
        }
        if name == BUILTIN_PREVIEW && kind.has_builtin_preview() {
            return Err(PolicyError::ResourceInUse(format!(
                "{}/{name} is registered by the server",
                kind.as_str()
            )));
        }
        let mut tx = self.lock_configuration().await?;
        let Some((revision, false)) = read_head(&mut tx, kind, name).await? else {
            return Err(not_found(kind, name));
        };
        let active = enabled_policies(&mut tx).await?;
        if let Some(policy) = active.iter().find(|policy| {
            policy.resource_bindings.as_ref().is_some_and(|bindings| {
                bindings
                    .iter()
                    .any(|binding| binding.kind == kind && binding.name == name)
            })
        }) {
            return Err(PolicyError::ResourceInUse(format!(
                "{}/{name} is referenced by policy {}",
                kind.as_str(),
                policy.id
            )));
        }
        if kind == ResourceKind::Selector {
            let resources = current_resources(&mut tx).await?;
            let selectors = selector_map(&resources);
            if let Some((owner, _)) = selectors
                .iter()
                .find(|(owner, expr)| owner.as_str() != name && contains_selector_ref(expr, name))
            {
                return Err(PolicyError::ResourceInUse(format!(
                    "Selector/{name} is referenced by Selector/{owner}"
                )));
            }
        }
        record_resource(&mut tx, kind, name, revision + 1, None, actor).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn bind_policy(
        tx: &mut Transaction<'_, MySql>, policy: Policy,
    ) -> Result<(CompiledPolicy, Vec<ResourceBinding>), PolicyError> {
        let resources = current_resources(tx).await?;
        let mut bindings = Vec::new();
        for (kind, name, path) in policy.references() {
            let (resource, builtin) = if name == BUILTIN_PREVIEW {
                (builtin_preview(kind).ok_or_else(|| not_found(kind, name))?, true)
            } else {
                (
                    resources
                        .iter()
                        .find(|resource| resource.kind == kind && resource.name == name)
                        .cloned()
                        .ok_or_else(|| {
                            PolicyError::InvalidPolicy(format!("{path} references unknown {}/{name}", kind.as_str()))
                        })?,
                    false,
                )
            };
            let mut binding = resource.binding();
            if builtin {
                binding.server_definition = resource.definition.map(|definition| definition.0);
            }
            bindings.push(binding);
        }
        let compiled = policy.compile(&selector_map(&resources))?;
        let actions = compiled
            .actions
            .iter()
            .map(|invocation| invocation.kind)
            .chain(compiled.pool_migration().map(|_| ActionKind::PoolMigrate))
            .collect::<std::collections::BTreeSet<_>>();
        let safety = applicable_safety(&resources, compiled.filesystem.as_str(), actions.iter().copied());
        check_approval_floor(
            tx,
            &bindings,
            &safety,
            compiled.filesystem.as_str(),
            actions.iter().copied(),
        )
        .await?;
        bindings.extend(
            resources
                .iter()
                .filter(|resource| {
                    resource.kind == ResourceKind::Selector && compiled.selectors.contains(&resource.name)
                })
                .map(ResourceRevision::binding),
        );
        bindings.extend(safety.into_iter().map(ResourceRevision::binding));
        bindings.extend(actions.into_iter().map(|action| ResourceBinding {
            kind: ResourceKind::ActionContract,
            name: action.name().into(),
            revision: contract_revision(action),
            server_definition: Some(action_contract(action)),
        }));
        sort_bindings(&mut bindings);
        Ok((compiled, bindings))
    }

    pub(super) async fn compile_admission(
        tx: &mut Transaction<'_, MySql>, policy: &CompiledPolicy, bindings: &[ResourceBinding], principal: Principal,
    ) -> Result<Admission, PolicyError> {
        let mut profile = None;
        let mut authorization = Value::Null;
        let mut approval = Value::Null;
        let mut safety = Vec::new();
        for binding in bindings {
            let output = match binding.kind {
                ResourceKind::ExecutionProfile => {
                    profile = Some((binding.name.as_str(), bound_spec(tx, binding).await?));
                    continue;
                }
                ResourceKind::AuthorizationPolicy => &mut authorization,
                ResourceKind::ApprovalPolicy => &mut approval,
                ResourceKind::SystemSafetyPolicy => {
                    safety.push(bound_spec(tx, binding).await?);
                    continue;
                }
                _ => continue,
            };
            *output = bound_spec(tx, binding).await?;
        }
        let (profile_name, profile) = profile.ok_or_else(|| policy::invalid("missing execution profile binding"))?;
        Admission::compile(
            policy,
            principal,
            profile_name,
            profile,
            authorization,
            approval,
            safety,
        )
    }

    pub(super) async fn compile_revision(
        &self, policy: Policy, bindings: &[ResourceBinding],
    ) -> Result<CompiledPolicy, PolicyError> {
        let mut selectors = BTreeMap::new();
        for binding in bindings.iter().filter(|binding| binding.kind == ResourceKind::Selector) {
            let resource = self
                .get_resource(binding.kind, &binding.name, Some(binding.revision))
                .await?;
            let definition = resource
                .definition
                .ok_or_else(|| not_found(binding.kind, &binding.name))?;
            selectors.insert(binding.name.clone(), definition["spec"]["where"].clone());
        }
        policy.compile(&selectors)
    }
}

async fn enabled_policies(tx: &mut Transaction<'_, MySql>) -> Result<Vec<PolicyRevision>, PolicyError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {POLICY_REVISION_FIELDS}
         FROM policies p JOIN policy_revisions r ON p.id = r.policy_id AND p.revision = r.revision
         WHERE p.name IS NOT NULL AND COALESCE(JSON_UNQUOTE(JSON_EXTRACT(r.definition, '$.spec.enabled')), 'true') <> 'false'"
    ))
    .fetch_all(&mut **tx)
    .await?)
}

async fn rebind_safety(tx: &mut Transaction<'_, MySql>, actor: &str) -> Result<(), PolicyError> {
    let resources = current_resources(tx).await?;
    for policy in enabled_policies(tx).await? {
        let Some(Json(bindings)) = policy.resource_bindings else {
            continue;
        };
        let input = policy::parse(&policy.definition)?;
        let filesystem = input.filesystem().as_str();
        let mut actions: Vec<_> = bindings
            .iter()
            .filter(|binding| binding.kind == ResourceKind::ActionContract)
            .filter_map(|binding| ActionKind::from_name(&binding.name))
            .collect();
        if actions.is_empty() {
            actions.push(ActionKind::Archive);
        }
        let safety = applicable_safety(&resources, filesystem, actions.iter().copied());
        let mut revised = bindings.clone();
        revised.retain(|binding| binding.kind != ResourceKind::SystemSafetyPolicy);
        revised.extend(safety.iter().map(|resource| resource.binding()));
        sort_bindings(&mut revised);
        if revised == bindings {
            continue;
        }
        check_approval_floor(tx, &revised, &safety, filesystem, actions.into_iter()).await?;
        // A due one-shot loses its timer when it is queued. Wake the new
        // revision if the old timer already fired. The enqueue path checks
        // whether that definition has already evaluated or dispatched work.
        sqlx::query(
            "UPDATE policies SET revision = revision + 1, next_run = CASE
                WHEN next_run IS NULL AND JSON_UNQUOTE(JSON_EXTRACT(definition, '$.spec.mode')) = 'one_shot'
                AND EXISTS (
                    SELECT 1 FROM policy_runs WHERE policy_id = ? AND revision = ?
                    AND requested_by IS NULL AND state IN ('queued', 'running')
                ) THEN UTC_TIMESTAMP(6) ELSE next_run END WHERE id = ?",
        )
        .bind(policy.id)
        .bind(policy.revision)
        .bind(policy.id)
        .execute(&mut **tx)
        .await?;
        record_policy_revision(tx, policy.id, revised, actor).await?;
    }
    Ok(())
}

fn sort_bindings(bindings: &mut [ResourceBinding]) {
    bindings.sort_by(|left, right| (left.kind, &left.name).cmp(&(right.kind, &right.name)));
}

fn applicable_safety<'a>(
    resources: &'a [ResourceRevision], filesystem: &str, actions: impl Iterator<Item = ActionKind> + Clone,
) -> Vec<&'a ResourceRevision> {
    resources
        .iter()
        .filter(|resource| {
            resource.kind == ResourceKind::SystemSafetyPolicy
                && resource.definition.as_ref().is_some_and(|definition| {
                    actions
                        .clone()
                        .any(|action| safety_matches(&definition["spec"], filesystem, action))
                })
        })
        .collect()
}

async fn check_approval_floor(
    tx: &mut Transaction<'_, MySql>, bindings: &[ResourceBinding], safety: &[&ResourceRevision], filesystem: &str,
    actions: impl Iterator<Item = ActionKind>,
) -> Result<(), PolicyError> {
    for action in actions {
        let needs_human = safety
            .iter()
            .filter_map(|resource| resource.definition.as_ref())
            .filter(|definition| safety_matches(&definition["spec"], filesystem, action))
            .flat_map(|definition| definition["spec"]["effectRules"].as_array().into_iter().flatten())
            .any(|rule| rule["effect"] == action.effect() && rule["minimumApproval"] == "human");
        if needs_human {
            let approval = bindings
                .iter()
                .find(|binding| binding.kind == ResourceKind::ApprovalPolicy)
                .ok_or_else(|| PolicyError::InvalidPolicy("policy revision is missing its ApprovalPolicy".into()))?;
            validate_approval_floor(&bound_spec(tx, approval).await?, filesystem, action)?;
        }
    }
    Ok(())
}

async fn bound_spec(tx: &mut Transaction<'_, MySql>, binding: &ResourceBinding) -> Result<Value, PolicyError> {
    if let Some(definition) = &binding.server_definition {
        return Ok(definition["spec"].clone());
    }
    if binding.name == BUILTIN_PREVIEW {
        let resource = builtin_preview(binding.kind).ok_or_else(|| not_found(binding.kind, &binding.name))?;
        if resource.revision == binding.revision {
            return Ok(resource.definition.expect("built-in resource definition")["spec"].clone());
        }
    }
    let revision: ResourceRevision = sqlx::query_as(HISTORICAL_RESOURCE)
        .bind(binding.kind.as_str())
        .bind(&binding.name)
        .bind(binding.revision)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| not_found(binding.kind, &binding.name))?;
    let definition = revision
        .definition
        .ok_or_else(|| not_found(binding.kind, &binding.name))?;
    Ok(definition["spec"].clone())
}

fn not_found(kind: ResourceKind, name: &str) -> PolicyError {
    PolicyError::ResourceNotFound(format!("{}/{name}", kind.as_str()))
}

fn builtin_preview(kind: ResourceKind) -> Option<ResourceRevision> {
    let spec = match kind {
        ResourceKind::ExecutionProfile => json!({
            "concurrency": {"global": 1},
            "retry": {
                "maxAttempts": 1, "backoff": "fixed", "initialDelay": "1s", "maxDelay": "1s",
                "retryableErrors": ["transient_io"]
            },
            "workerLease": {"ttl": "1m", "renewEvery": "10s", "onLeaseLost": "stop_and_reconcile"},
            "circuitBreaker": {"scopes": [{
                "domain": {"key": "policy"}, "errorClasses": ["transient_io"], "evaluation": "any",
                "conditions": [{"type": "consecutive_failures", "threshold": 1}],
                "onOpen": "suspend_submissions", "resetMode": "manual"
            }]}
        }),
        ResourceKind::AuthorizationPolicy => json!({
            "defaultDecision": "deny", "statements": [{
                "id": "preview-only", "decision": "deny", "subjects": {}, "actions": [ARCHIVE], "scope": {}
            }]
        }),
        ResourceKind::ApprovalPolicy => json!({"defaultDecision": "deny"}),
        _ => return None,
    };
    Some(ResourceRevision {
        kind,
        name: BUILTIN_PREVIEW.into(),
        revision: 1,
        definition: Some(Json(json!({
            "apiVersion": policy::API_VERSION, "kind": kind, "metadata": {"name": BUILTIN_PREVIEW}, "spec": spec
        }))),
        modified_by: "server".into(),
    })
}

async fn current_resources(tx: &mut Transaction<'_, MySql>) -> Result<Vec<ResourceRevision>, PolicyError> {
    Ok(sqlx::query_as(&format!("{CURRENT_RESOURCES} ORDER BY h.kind, h.name"))
        .fetch_all(&mut **tx)
        .await?)
}

fn selector_map(resources: &[ResourceRevision]) -> BTreeMap<String, Value> {
    resources
        .iter()
        .filter(|resource| resource.kind == ResourceKind::Selector)
        .filter_map(|resource| {
            Some((
                resource.name.clone(),
                resource.definition.as_ref()?["spec"]["where"].clone(),
            ))
        })
        .collect()
}

fn contains_selector_ref(expr: &Value, name: &str) -> bool {
    if expr["selectorRef"] == name {
        return true;
    }
    if let Some(child) = expr.get("not") {
        return contains_selector_ref(child, name);
    }
    ["all", "any"]
        .iter()
        .filter_map(|operator| expr[*operator].as_array())
        .any(|children| children.iter().any(|child| contains_selector_ref(child, name)))
}

async fn read_head(
    tx: &mut Transaction<'_, MySql>, kind: ResourceKind, name: &str,
) -> Result<Option<(u64, bool)>, PolicyError> {
    Ok(
        sqlx::query_as("SELECT revision, deleted FROM policy_resource_heads WHERE kind = ? AND name = ?")
            .bind(kind.as_str())
            .bind(name)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

async fn record_resource(
    tx: &mut Transaction<'_, MySql>, kind: ResourceKind, name: &str, revision: u64, document: Option<&Value>,
    actor: &str,
) -> Result<(), PolicyError> {
    sqlx::query(
        "INSERT INTO policy_resource_heads (kind, name, revision, deleted) VALUES (?, ?, ?, ?) \
        ON DUPLICATE KEY UPDATE revision = VALUES(revision), deleted = VALUES(deleted)",
    )
    .bind(kind.as_str())
    .bind(name)
    .bind(revision)
    .bind(document.is_none())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO policy_resource_revisions (kind, name, revision, definition, modified_by) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(kind.as_str())
    .bind(name)
    .bind(revision)
    .bind(document.map(Json))
    .bind(actor)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
