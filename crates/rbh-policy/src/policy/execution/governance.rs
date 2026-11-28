use chrono::{DateTime, Datelike, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::Value;

use super::{Principal, TerminalError};
use crate::{
    PolicyError,
    policy::{ActionKind, BudgetLimits, CompiledPolicy, Invocation, duration_millis, invalid, resources, size_bytes},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Window {
    timezone: Tz,
    weekly: Vec<WindowEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowEntry {
    days: Vec<Day>,
    start: NaiveTime,
    end: NaiveTime,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Window {
    pub(crate) fn parse(value: &Value) -> Result<Self, PolicyError> {
        let window: Self = serde_json::from_value(value.clone())
            .map_err(|error| invalid(format!("execution admissionWindow: {error}")))?;
        if window
            .weekly
            .iter()
            .any(|entry| entry.days.is_empty() || entry.start == entry.end)
        {
            return Err(invalid(
                "admissionWindow intervals require days and distinct start/end times",
            ));
        }
        Ok(window)
    }

    pub(crate) fn contains(&self, now: DateTime<Utc>) -> bool {
        let local = now.with_timezone(&self.timezone);
        let today = local.weekday().num_days_from_monday();
        let previous_day = (today + 6) % 7;
        let time = local.time();
        self.weekly.iter().any(|slot| {
            let on = |day| slot.days.iter().any(|value| *value as u32 == day);
            if slot.start < slot.end {
                on(today) && slot.start <= time && time < slot.end
            } else {
                (on(today) && time >= slot.start) || (on(previous_day) && time < slot.end)
            }
        })
    }
}

pub(crate) enum Decision {
    Allow,
    Defer(String),
}

pub(crate) struct Admission {
    authorization: Value,
    approval: Value,
    pub safety: Vec<Value>,
    principal: Principal,
    windows: Vec<Window>,
    pub profile: Profile,
}

pub(crate) struct Profile {
    pub name: String,
    pub slots: u64,
    pub breakers: Vec<Breaker>,
}

pub(crate) struct Breaker {
    pub classes: Vec<TerminalError>,
    pub threshold: u64,
}

impl Admission {
    pub(crate) fn compile(
        policy: &CompiledPolicy, principal: Principal, profile_name: &str, profile: Value, authorization: Value,
        approval: Value, safety: Vec<Value>,
    ) -> Result<Self, PolicyError> {
        if profile["retry"]["maxAttempts"].as_u64() != Some(1) {
            return Err(invalid(
                "enforced native HSM requires ExecutionProfile retry.maxAttempts=1",
            ));
        }
        let mut breakers = Vec::new();
        for scope in profile["circuitBreaker"]["scopes"].as_array().into_iter().flatten() {
            if scope["domain"]["key"] != "policy" || scope["resetMode"] != "manual" {
                return Err(invalid(
                    "enforced native HSM supports policy-scoped circuit breakers with manual reset",
                ));
            }
            let classes = scope["errorClasses"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|value| {
                    value.as_str().and_then(TerminalError::resolve).ok_or_else(|| {
                        invalid(format!(
                            "enforced native HSM cannot classify terminal breaker error {value}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            for condition in scope["conditions"].as_array().into_iter().flatten() {
                if condition["type"] != "consecutive_failures" {
                    return Err(invalid(
                        "enforced native HSM supports consecutive_failures circuit breakers",
                    ));
                }
                breakers.push(Breaker {
                    classes: classes.clone(),
                    threshold: condition["threshold"].as_u64().expect("validated count threshold"),
                });
            }
        }
        let slots = profile["concurrency"]["global"]
            .as_u64()
            .ok_or_else(|| invalid("execution profile must declare global concurrency"))?;
        let windows = [profile.get("admissionWindow"), policy.window.as_ref()]
            .into_iter()
            .flatten()
            .map(Window::parse)
            .collect::<Result<_, _>>()?;
        let admission = Self {
            authorization,
            approval,
            safety,
            principal,
            windows,
            profile: Profile {
                name: profile_name.into(),
                slots,
                breakers,
            },
        };
        for invocation in &policy.actions {
            let filesystem = policy.filesystem.as_str();
            match admission.governance(filesystem, invocation) {
                Decision::Defer(reason) => return Err(invalid(reason)),
                Decision::Allow => {}
            }
            let statements = admission.statements(filesystem, invocation.kind);
            if statements.iter().any(|statement| {
                let scope = &statement["scope"];
                scope.get("namespacePrefixes").is_some() || scope.get("ostNames").is_some()
            }) {
                return Err(invalid(
                    "enforced native HSM does not support namespacePrefixes or ostNames authorization scopes",
                ));
            }
            if invocation.kind == ActionKind::PoolMigrate
                && statements
                    .iter()
                    .any(|statement| statement["scope"].get("archiveIds").is_some())
            {
                return Err(invalid(
                    "pool migration does not support archiveIds authorization scopes",
                ));
            }
            if matches!(invocation.kind, ActionKind::Archive | ActionKind::PoolMigrate) {
                if let Decision::Defer(reason) = admission.authorize_archive_id(&statements, invocation.archive_id) {
                    return Err(invalid(reason));
                }
            } else if !statements.iter().any(|statement| statement["decision"] == "allow")
                || statements
                    .iter()
                    .any(|statement| statement["decision"] == "deny" && statement["scope"].get("archiveIds").is_none())
            {
                return Err(invalid(format!("authorization denied {}", invocation.kind.name())));
            }
        }
        Ok(admission)
    }

    pub(crate) fn window_open(&self, now: DateTime<Utc>) -> bool {
        self.windows.iter().all(|window| window.contains(now))
    }

    pub(crate) fn window_check(&self) -> impl Fn() -> bool + Send + use<> {
        let windows = self.windows.clone();
        move || {
            let now = Utc::now();
            windows.iter().all(|window| window.contains(now))
        }
    }

    pub(crate) fn authorize(&self, filesystem: &str, invocation: &Invocation) -> Decision {
        self.authorize_archive_id(&self.statements(filesystem, invocation.kind), invocation.archive_id)
    }

    fn governance(&self, filesystem: &str, invocation: &Invocation) -> Decision {
        let action = invocation.kind;
        let matching: Vec<_> = self
            .safety
            .iter()
            .filter(|spec| resources::safety_matches(spec, filesystem, action))
            .collect();
        if matching.is_empty() {
            return Decision::Defer(format!("no applicable SystemSafetyPolicy for {}", action.name()));
        }
        if matching
            .iter()
            .any(|spec| !allows(&spec["allowedActions"], action.name()))
        {
            return Decision::Defer(format!("SystemSafetyPolicy denies {}", action.name()));
        }
        let requires_human = matching
            .iter()
            .flat_map(|spec| spec["effectRules"].as_array().into_iter().flatten())
            .any(|rule| rule["effect"] == action.effect() && rule["minimumApproval"] == "human");
        let requirements: Vec<_> = self.approval["requirements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|rule| resources::approval_matches(rule, filesystem, action))
            .collect();
        if requirements.is_empty() || requirements.iter().any(|rule| rule["mode"] == "deny") {
            return Decision::Defer(format!("ApprovalPolicy denies {}", action.name()));
        }
        if invocation.requires_human || requires_human || requirements.iter().any(|rule| rule["mode"] == "required") {
            return Decision::Defer(format!("human approval is required for {}", action.name()));
        }
        Decision::Allow
    }

    fn statements(&self, filesystem: &str, action: ActionKind) -> Vec<&Value> {
        self.authorization["statements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|statement| {
                let subjects = &statement["subjects"];
                let scope = &statement["scope"];
                allows(&subjects["serviceAccounts"], &self.principal.service_account)
                    && subjects["roles"].as_array().is_none_or(|roles| {
                        self.principal
                            .roles
                            .iter()
                            .any(|role| roles.iter().any(|value| value == role))
                    })
                    && includes(&statement["actions"], action.name())
                    && allows(&scope["filesystems"], filesystem)
                    && allows(&scope["objectTypes"], "file")
            })
            .collect()
    }

    fn authorize_archive_id(&self, statements: &[&Value], archive_id: u32) -> Decision {
        let mut allowed = false;
        for statement in statements {
            let denial = statement["decision"] == "deny";
            let matches = statement["scope"]["archiveIds"].as_array().is_none_or(|ids| {
                if archive_id == 0 {
                    denial && !ids.is_empty()
                } else {
                    ids.iter().any(|id| id == archive_id)
                }
            });
            if matches && denial {
                return Decision::Defer(format!("authorization denied by {}", statement["id"]));
            }
            allowed |= matches;
        }
        if allowed {
            Decision::Allow
        } else {
            Decision::Defer("no matching AuthorizationPolicy allow".into())
        }
    }
}

impl BudgetLimits {
    pub(crate) fn from_safety(spec: &Value) -> Self {
        let limits = &spec["limits"];
        Self {
            operations: limits["maxOperations"].as_u64().unwrap_or(u64::MAX),
            attempts: limits["maxSubmitAttempts"].as_u64().unwrap_or(u64::MAX),
            bytes: size_bytes(&limits["maxObjectLogicalBytes"]).unwrap_or(u64::MAX),
            duration_ms: limits["maxDuration"].as_str().and_then(duration_millis),
        }
    }
}

fn allows(values: &Value, target: &str) -> bool {
    values.is_null() || includes(values, target)
}

fn includes(values: &Value, target: &str) -> bool {
    values
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item == target))
}
