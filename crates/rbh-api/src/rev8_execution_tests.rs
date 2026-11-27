use std::{
    collections::HashMap,
    ops::ControlFlow,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::http::StatusCode;
use axum_test::TestServer;
use bytes::Bytes;
use lustre_api::{HsmState, HsmUserStateInfo, LuFid};
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, EntryStore, FileSystemConfig, FileSystemId, ObjectId,
    ScopedEntryRow,
};
use rbh_policy::{
    PolicyStore,
    rev8::{
        ActionKind, Invocation,
        execution::{
            FidObservation, Generation, NativeHsm, Observation, Principal, Runtime, Submission, SubmissionGate,
            TerminalError,
        },
    },
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::handlers::scoped_api_tests::{
    integration_enabled, resource_document, rev8_archive_policy, rev8_test_server, state_without_database_connection,
};

#[derive(Default)]
struct HsmFixture {
    files: Mutex<HashMap<LuFid, Observation>>,
    finish_after_observation: Mutex<Option<(LuFid, HsmUserStateInfo)>>,
    submitted: Mutex<Vec<(LuFid, ActionKind, u32)>>,
    interrupt_request: AtomicBool,
    reject_request: AtomicBool,
    sent: Notify,
    hold_observation: Mutex<Option<LuFid>>,
    observation_started: Notify,
    resume_observation: Notify,
    hold_submission: AtomicBool,
    submission_started: Notify,
    resume_submission: Notify,
    missing_on_other_source: AtomicBool,
}

#[async_trait::async_trait]
impl NativeHsm for HsmFixture {
    async fn filesystem_name(&self, mount: &Path) -> Result<String, String> {
        if mount == Path::new("/lustre-fixture") {
            Ok("fixture".into())
        } else {
            Err("not mounted".into())
        }
    }

    async fn observe(&self, _: &Path, fid: LuFid) -> Result<FidObservation, String> {
        let held = {
            let mut hold = self.hold_observation.lock().unwrap();
            *hold == Some(fid) && hold.take().is_some()
        };
        if held {
            self.observation_started.notify_one();
            self.resume_observation.notified().await;
        }
        let mut finish = self.finish_after_observation.lock().unwrap();
        let mut files = self.files.lock().unwrap();
        let observed = files.get(&fid).cloned();
        if finish.as_ref().is_some_and(|(target, _)| *target == fid)
            && let Some((_, completed)) = finish.take()
            && let Some(current) = files.get_mut(&fid)
        {
            current.state = completed;
        }
        Ok(observed.map_or_else(
            || FidObservation::Missing {
                mount_source_id: if self.missing_on_other_source.load(Ordering::SeqCst) {
                    "other-fixture"
                } else {
                    "fixture"
                }
                .into(),
            },
            FidObservation::Present,
        ))
    }

    async fn submit(
        &self, _: &Path, fid: LuFid, invocation: Invocation, _: &Generation, gate: SubmissionGate,
    ) -> Submission {
        if self.hold_submission.swap(false, Ordering::SeqCst) {
            self.submission_started.notify_one();
            self.resume_submission.notified().await;
        }
        if let ControlFlow::Break(reason) = gate.check() {
            return Submission::Deferred(reason);
        }
        self.submitted
            .lock()
            .unwrap()
            .push((fid, invocation.kind, invocation.archive_id));
        self.sent.notify_one();
        if self.interrupt_request.load(Ordering::SeqCst) {
            std::future::pending().await
        } else if self.reject_request.load(Ordering::SeqCst) {
            Submission::Rejected {
                error: "fixture failure before native submit".into(),
                class: Some(TerminalError::BackendUnavailable),
            }
        } else {
            Submission::Accepted
        }
    }
}

struct Scenario {
    entries: EntryStore,
    filesystem: FileSystemId,
    hsm: Arc<HsmFixture>,
    stores: PolicyStore,
    server: TestServer,
}

impl Scenario {
    async fn setup() -> Self {
        let database_url =
            std::env::var("RBH_TEST_DATABASE_URL").unwrap_or_else(|_| "mysql://root@localhost/rbh_entries_test".into());
        let entries = EntryStore::connect(&database_url).await.unwrap();
        let filesystem = FileSystemId::new(format!("hsm-execution-{}", uuid::Uuid::new_v4().simple())).unwrap();
        entries
            .register_filesystem(&FileSystemConfig {
                id: filesystem.clone(),
                backend: BackendKind::Lustre,
                mount_path: "/lustre-fixture".into(),
                capabilities: BackendCapabilities {
                    hsm: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let hsm = Arc::new(HsmFixture::default());
        let stores = open_policy_store(&entries, hsm.clone());
        let mut state = state_without_database_connection();
        state.entry_store = entries.clone();
        state.policy_store = stores.clone();
        let server = rev8_test_server(state);
        Self {
            entries,
            filesystem,
            hsm,
            stores,
            server,
        }
    }

    async fn file(&self, oid: u32, size: u64, states: HsmState, archive_id: u32) -> LuFid {
        let fid = LuFid::new(0x200000401, oid, 0);
        self.hsm.files.lock().unwrap().insert(
            fid,
            Observation {
                generation: Generation {
                    filesystem_name: "fixture".into(),
                    mount_source_id: Some("fixture".into()),
                    size,
                    mtime: 10,
                    mtime_nanos: 0,
                    ctime: 10,
                    ctime_nanos: 0,
                },
                state: HsmUserStateInfo {
                    states,
                    archive_id,
                    ..Default::default()
                },
            },
        );
        self.entries
            .upsert_scoped_entry(&ScopedEntryRow {
                key: EntryKey::new(self.filesystem.clone(), ObjectId::Lustre(fid)),
                parent: None,
                name: Bytes::from(format!("file-{oid}")),
                kind: EntryKind::File,
                size,
                blocks: 1,
                uid: 0,
                gid: 0,
                projid: 0,
                mode: 0o600,
                nlink: 1,
                atime: 10,
                mtime: 10,
                ctime: 10,
                stripe_count: None,
                stripe_size: None,
                stripe_items: Vec::new(),
                pool_name: None,
                sm_status: json!({}),
                last_seen: 0,
                depth: 1,
            })
            .await
            .unwrap();
        fid
    }

    fn resource(&self, kind: &str, spec: Value) -> Value {
        resource_document(kind, self.filesystem.as_str(), spec)
    }

    fn policy(&self, suffix: &str, action: &str) -> Value {
        let mut policy = rev8_archive_policy(self.filesystem.as_str(), &format!("{}-{suffix}", self.filesystem));
        policy["spec"]["trigger"] = json!({"type": "manual"});
        policy["spec"]["execution"]["activation"]["mode"] = "enforced".into();
        policy["spec"]["execution"]["budget"]["run"]["maxDuration"] = "5m".into();
        policy["spec"]["execution"]["profileRef"] = self.filesystem.as_str().into();
        policy["spec"]["governance"] =
            json!({"authorizationRef": self.filesystem, "approvalPolicyRef": self.filesystem});
        policy["spec"]["rules"]["entries"][0]["action"] = json!({"type": action});
        policy
    }

    async fn create(&self, policy: &Value) -> u64 {
        let response = self.server.post("/api/policies").json(policy).await;
        response.assert_status(StatusCode::CREATED);
        response.json::<Value>()["id"].as_u64().unwrap()
    }

    async fn queue(&self, policy: u64) -> u64 {
        let response = self.server.post(&format!("/api/policies/{policy}/run")).await;
        response.assert_status(StatusCode::ACCEPTED);
        response.json::<Value>()["runId"].as_u64().unwrap()
    }

    async fn operations(&self, policy: u64, run: u64) -> Vec<Value> {
        let response = self
            .server
            .get(&format!("/api/policies/{policy}/runs/{run}/operations"))
            .await;
        response.assert_status_ok();
        response.json()
    }

    async fn tick(&self) {
        self.stores.process_rev8(&self.entries).await.unwrap();
    }

    async fn reconcile_until(&self, store: &PolicyStore, mut observed: impl AsyncFnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                store.process_rev8(&self.entries).await.unwrap();
                if observed().await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reconciliation did not publish the expected observation");
    }

    async fn checked(&self, operation: &Value) -> bool {
        sqlx::query_scalar("SELECT checked_at IS NOT NULL FROM rev8_operations WHERE id = ?")
            .bind(operation["id"].as_u64().unwrap())
            .fetch_one(self.entries.pool())
            .await
            .unwrap()
    }

    fn submitted(&self) -> Vec<(LuFid, ActionKind, u32)> {
        self.hsm.submitted.lock().unwrap().clone()
    }
}

fn open_policy_store(entries: &EntryStore, hsm: Arc<HsmFixture>) -> PolicyStore {
    PolicyStore::new(entries.pool().clone()).with_rev8_runtime(Runtime {
        principal: Principal {
            service_account: "hsm-controller".into(),
            roles: vec!["storage-operator".into()],
        },
        hsm,
    })
}

#[tokio::test]
async fn rev8_enforced_runs_check_governance_record_acceptance_and_recover_after_policy_changes() {
    if !integration_enabled() {
        return;
    }
    let env = Scenario::setup().await;
    let server = &env.server;
    let archive = "lustre.hsm.archive";
    let release = "lustre.hsm.release";
    let restore = "lustre.hsm.restore";
    let actions = [archive, release, restore];
    let first = env.file(1, 128, HsmState::empty(), 0).await;
    let protected = env.file(2, 256, HsmState::NOARCHIVE, 0).await;
    let copies = HsmState::EXISTS | HsmState::ARCHIVED;

    let mut policy = env.policy("archive", archive);
    policy["spec"]["rules"]["entries"][0]["action"]["params"] = json!({"archiveId": 17});
    let profile = env.resource("ExecutionProfile", json!({
        "concurrency": {"global": 1},
        "retry": {"maxAttempts": 1, "backoff": "fixed", "initialDelay": "1s", "maxDelay": "1s", "retryableErrors": ["transient_io"]},
        "workerLease": {"ttl": "1m", "renewEvery": "10s", "onLeaseLost": "stop_and_reconcile"},
        "circuitBreaker": {"scopes": [{"domain": {"key": "policy"}, "errorClasses": ["backend_unavailable"], "evaluation": "any",
            "conditions": [{"type": "consecutive_failures", "threshold": 1}], "onOpen": "suspend_submissions", "resetMode": "manual"}]}
    }));
    let mut authorization = env.resource(
        "AuthorizationPolicy",
        json!({
            "defaultDecision": "deny", "statements": [{
                "id": "workload", "decision": "allow", "actions": actions,
                "subjects": {"serviceAccounts": ["config-operator"], "roles": ["storage-operator"]},
                "scope": {"filesystems": [env.filesystem], "objectTypes": ["file"], "archiveIds": [17]}
            }]
        }),
    );
    let mut approval = env.resource(
        "ApprovalPolicy",
        json!({
            "defaultDecision": "deny", "requirements": [{"id": "native", "actions": actions, "mode": "not_required"}],
            "floorDefaults": {"minApprovers": 1, "approverRoles": ["approver"]}
        }),
    );
    for (path, document) in [
        ("execution-profiles", &profile),
        ("authorization-policies", &authorization),
        ("approval-policies", &approval),
    ] {
        server
            .post(&format!("/api/{path}"))
            .json(document)
            .await
            .assert_status(StatusCode::CREATED);
    }
    let rejected = server.post("/api/policies").json(&policy).await;
    rejected.assert_status_bad_request();
    assert!(
        rejected.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("SystemSafetyPolicy")
    );

    let mut safety = env.resource(
        "SystemSafetyPolicy",
        json!({
            "scope": {"filesystems": [env.filesystem]}, "limits": {"maxUnknownOutstanding": 1},
            "effectRules": [{"effect": "data_copy", "minimumApproval": "human", "allowAutoBreakerReset": false}]
        }),
    );
    server
        .post("/api/system-safety-policies")
        .json(&safety)
        .await
        .assert_status(StatusCode::CREATED);
    let rejected = server.post("/api/policies").json(&policy).await;
    rejected.assert_status_bad_request();
    assert!(
        rejected.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("human approval")
    );
    safety["spec"]["effectRules"][0]["minimumApproval"] = "none".into();
    let safety_url = format!("/api/system-safety-policies/{}", env.filesystem);
    server.put(&safety_url).json(&safety).await.assert_status_ok();
    let rejected = server.post("/api/policies").json(&policy).await;
    rejected.assert_status_bad_request();
    assert!(
        rejected.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("AuthorizationPolicy")
    );
    authorization["spec"]["statements"][0]["subjects"]["serviceAccounts"] = json!(["hsm-controller"]);
    server
        .put(&format!("/api/authorization-policies/{}", env.filesystem))
        .json(&authorization)
        .await
        .assert_status_ok();

    let id = env.create(&policy).await;
    let base = format!("/api/policies/{id}");
    let preview = server.post(&format!("{base}/preview")).await;
    preview.assert_status_ok();
    assert_eq!(preview.json::<Value>()["candidateCount"], 2);
    assert!(env.submitted().is_empty());

    let run = env.queue(id).await;
    env.tick().await;
    let operations = env.operations(id, run).await;
    assert_eq!(operations[0]["state"], "submitted");
    assert!(operations[0]["acceptedAt"].is_string());
    assert_eq!(operations[0]["principal"]["serviceAccount"], "hsm-controller");
    assert_eq!(operations[1]["reason"], "system_safety_outstanding_limit");
    assert_eq!(env.submitted(), vec![(first, ActionKind::Archive, 17)]);

    // A stalled read of a previously accepted request must leave the worker
    // available to another scope and keep the original FID reserved.
    let plain = FileSystemId::new(format!("{}-plain", env.filesystem)).unwrap();
    env.entries
        .register_filesystem(&FileSystemConfig {
            id: plain.clone(),
            backend: BackendKind::Lustre,
            mount_path: "/lustre-fixture".into(),
            capabilities: BackendCapabilities {
                hsm: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let mut plain_preview = rev8_archive_policy(plain.as_str(), plain.as_str());
    plain_preview["spec"]["trigger"] = json!({"type": "manual"});
    let plain_policy = env.create(&plain_preview).await;
    let plain_run = env.queue(plain_policy).await;
    *env.hsm.hold_observation.lock().unwrap() = Some(first);
    tokio::time::timeout(Duration::from_secs(3), async {
        env.tick().await;
        env.hsm.observation_started.notified().await;
        env.tick().await;
    })
    .await
    .expect("native HSM reconciliation blocked the scheduling tick");
    let plain_runs = server
        .get(&format!("/api/policies/{plain_policy}/runs"))
        .await
        .json::<Vec<Value>>();
    assert_eq!(plain_runs[0]["id"], plain_run);
    assert_eq!(plain_runs[0]["state"], "previewed");
    assert_eq!(env.operations(id, run).await[0]["state"], "submitted");
    env.hsm.resume_observation.notify_one();

    safety["spec"]["limits"]["maxUnknownOutstanding"] = 2.into();
    server.put(&safety_url).json(&safety).await.assert_status_ok();
    let limited = env.queue(id).await;
    env.reconcile_until(&env.stores, async || env.operations(id, limited).await.len() == 2)
        .await;
    assert!(
        env.operations(id, limited)
            .await
            .iter()
            .all(|row| row["reason"] == "profile_concurrency_limit")
    );
    assert_eq!(env.submitted().len(), 1);
    let mut one_shot = policy.clone();
    one_shot["metadata"]["name"] = format!("{}-one-shot", env.filesystem).into();
    one_shot["spec"]["mode"] = "one_shot".into();
    one_shot["spec"]["rules"]["entries"][0]["match"] = json!({"field": "object.size", "op": "eq", "value": 256});
    let once = env.create(&one_shot).await;
    let once_run = env.queue(once).await;
    env.tick().await;
    let once_url = format!("/api/policies/{once}/runs");
    let once_status = server.get(&once_url).await.json::<Vec<Value>>();
    assert_eq!(once_status[0]["state"], "running");
    assert_eq!(once_status[0]["reason"], "profile_concurrency_limit");
    let queued = env.queue(id).await;
    policy["spec"]["enabled"] = false.into();
    server
        .put(&base)
        .json(&policy)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server.post(&format!("{base}/run")).await.assert_status_bad_request();
    server.delete(&base).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(env.operations(id, run).await[0]["state"], "submitted");

    let fresh = open_policy_store(&env.entries, env.hsm.clone());
    env.hsm.files.lock().unwrap().get_mut(&first).unwrap().state = HsmUserStateInfo {
        states: copies,
        archive_id: 17,
        ..Default::default()
    };
    env.reconcile_until(&fresh, async || {
        let runs = server.get(&once_url).await.json::<Vec<Value>>();
        runs[0]["state"] == "finished"
    })
    .await;
    assert_eq!(
        env.operations(once, once_run).await[0]["reason"],
        "hsm_state_ineligible"
    );
    safety["spec"]["limits"]["maxUnknownOutstanding"] = 3.into();
    server.put(&safety_url).json(&safety).await.assert_status_ok();
    assert_eq!(
        env.queue(once).await,
        once_run,
        "a safety revision must not repeat a finished one-shot"
    );
    let completed = &env.operations(id, run).await[0];
    assert_eq!(completed["state"], "completed");
    assert_eq!(completed["revision"], 1);
    let runs = server.get(&format!("{base}/runs")).await.json::<Vec<Value>>();
    assert_eq!(
        runs.iter().find(|row| row["id"] == queued).unwrap()["state"],
        "cancelled"
    );
    assert_eq!(env.submitted().len(), 1);

    // A periodic preview can be processed after rebuilding the store, with no native submit.
    let mut preview_only = env.policy("periodic-preview", archive);
    preview_only["spec"]["execution"]["activation"]["mode"] = "preview".into();
    preview_only["spec"]["trigger"] = json!({"type": "periodic", "interval": "1ms"});
    preview_only["spec"]["mode"] = "one_shot".into();
    let preview_id = env.create(&preview_only).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    fresh.process_rev8(&env.entries).await.unwrap();
    let runs = server
        .get(&format!("/api/policies/{preview_id}/runs"))
        .await
        .json::<Vec<Value>>();
    assert_eq!(runs[0]["state"], "previewed");
    assert_eq!(runs[0]["candidateCount"], 2);
    assert_eq!(env.submitted().len(), 1);

    // Let an older manual preview take the worker's first slot, leaving
    // the periodic one-shot queued when its safety bindings change.
    preview_only["metadata"]["name"] = format!("{}-queued-preview", env.filesystem).into();
    let queued_preview = env.create(&preview_only).await;
    let mut earlier = preview_only.clone();
    earlier["metadata"]["name"] = format!("{}-earlier-preview", env.filesystem).into();
    earlier["spec"]["trigger"] = json!({"type": "manual"});
    let earlier = env.create(&earlier).await;
    env.queue(earlier).await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    env.tick().await;
    let queued_url = format!("/api/policies/{queued_preview}/runs");
    let runs = server.get(&queued_url).await.json::<Vec<Value>>();
    assert_eq!(runs[0]["state"], "queued");
    safety["spec"]["limits"]["maxUnknownOutstanding"] = 4.into();
    server.put(&safety_url).json(&safety).await.assert_status_ok();
    env.tick().await;
    env.tick().await;
    let runs = server.get(&queued_url).await.json::<Vec<Value>>();
    assert_eq!(
        runs.len(),
        2,
        "a queued automatic one-shot must follow the new safety revision"
    );
    assert_eq!(runs[0]["state"], "cancelled");
    assert_eq!(runs[1]["state"], "previewed");

    // Denying approval explicitly prevents a previously valid unattended policy
    // from being enabled against the newly bound revision.
    approval["spec"]["requirements"][0]["mode"] = "deny".into();
    let approval_url = format!("/api/approval-policies/{}", env.filesystem);
    server.put(&approval_url).json(&approval).await.assert_status_ok();
    let release_policy = env.policy("release", release);
    let rejected = server.post("/api/policies").json(&release_policy).await;
    rejected.assert_status_bad_request();
    assert!(
        rejected.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("ApprovalPolicy")
    );
    approval["spec"]["requirements"][0]["mode"] = "not_required".into();
    server.put(&approval_url).json(&approval).await.assert_status_ok();

    let release_id = env.create(&release_policy).await;
    let mut restore_policy = env.policy("restore", restore);
    restore_policy["spec"]["rules"]["entries"][0]["match"] = json!({"field": "object.size", "op": "eq", "value": 128});
    let restore_id = env.create(&restore_policy).await;
    let release_run = env.queue(release_id).await;
    env.hsm.sent.notified().await;
    env.hsm.interrupt_request.store(true, Ordering::SeqCst);
    let worker = {
        let entries = env.entries.clone();
        tokio::spawn(async move { fresh.process_rev8(&entries).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), env.hsm.sent.notified())
        .await
        .unwrap();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(env.operations(release_id, release_run).await[0]["state"], "submitting");
    env.hsm.interrupt_request.store(false, Ordering::SeqCst);

    let reopened = open_policy_store(&env.entries, env.hsm.clone());
    env.reconcile_until(&reopened, async || {
        env.operations(release_id, release_run).await[0]["state"] == "unknown"
    })
    .await;
    assert_eq!(
        env.submitted(),
        vec![(first, ActionKind::Archive, 17), (first, ActionKind::Release, 17)]
    );
    env.hsm.files.lock().unwrap().get_mut(&first).unwrap().state.states = copies | HsmState::RELEASED;

    let restore_run = env.queue(restore_id).await;
    let inconclusive_hsm = Arc::new(HsmFixture::default());
    inconclusive_hsm.missing_on_other_source.store(true, Ordering::SeqCst);
    let inconclusive_worker = open_policy_store(&env.entries, inconclusive_hsm);
    let release_op = env.operations(release_id, release_run).await[0]["id"].as_u64().unwrap();
    env.reconcile_until(&inconclusive_worker, async || {
        sqlx::query_scalar(
            "SELECT o.checked_at >= r.created_at FROM rev8_operations o JOIN rev8_runs r ON r.id = ? WHERE o.id = ?",
        )
        .bind(restore_run)
        .bind(release_op)
        .fetch_one(env.entries.pool())
        .await
        .unwrap()
    })
    .await;
    assert!(env.operations(restore_id, restore_run).await.is_empty());
    let reopened = open_policy_store(&env.entries, env.hsm.clone());
    reopened.process_rev8(&env.entries).await.unwrap();
    assert!(env.operations(restore_id, restore_run).await.is_empty());
    let restore_runs = server
        .get(&format!("/api/policies/{restore_id}/runs"))
        .await
        .json::<Vec<Value>>();
    assert_eq!(restore_runs[0]["reason"], "native_hsm_reconciliation_pending");
    env.reconcile_until(&reopened, async || {
        env.operations(restore_id, restore_run)
            .await
            .first()
            .is_some_and(|op| op["state"] == "submitted")
    })
    .await;
    assert_eq!(env.operations(release_id, release_run).await[0]["state"], "completed");
    assert_eq!(env.submitted()[2], (first, ActionKind::Restore, 17));

    env.hsm.files.lock().unwrap().get_mut(&first).unwrap().state.states = copies;
    env.reconcile_until(&reopened, async || {
        env.operations(restore_id, restore_run).await[0]["state"] == "completed"
    })
    .await;

    env.hsm.reject_request.store(true, Ordering::SeqCst);
    let failed_run = env.queue(release_id).await;
    reopened.process_rev8(&env.entries).await.unwrap();
    assert_eq!(env.operations(release_id, failed_run).await[0]["state"], "failed");
    let call_count = env.submitted().len();
    let blocked_run = env.queue(release_id).await;
    reopened.process_rev8(&env.entries).await.unwrap();
    let runs = server
        .get(&format!("/api/policies/{release_id}/runs"))
        .await
        .json::<Vec<Value>>();
    assert_eq!(
        runs.iter().find(|row| row["id"] == blocked_run).unwrap()["reason"],
        "policy_circuit_breaker_open"
    );
    server
        .post(&format!("/api/policies/{release_id}/breaker/reset"))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    assert_eq!(env.submitted().len(), call_count);

    let unauthenticated = TestServer::new(crate::router({
        let mut state = state_without_database_connection();
        state.policy_store = env.stores.clone();
        state
    }))
    .unwrap();
    unauthenticated
        .post(&format!("/api/policies/{restore_id}/run"))
        .await
        .assert_status_unauthorized();
    unauthenticated
        .delete(&format!("/api/policies/{restore_id}"))
        .await
        .assert_status_unauthorized();
    assert!(
        env.hsm.files.lock().unwrap()[&protected]
            .state
            .states
            .contains(HsmState::NOARCHIVE)
    );

    // Rebind while the first batch is observing a file, before any submit.
    // The old batch records a deferral and the new revision gets one attempt.
    env.hsm.reject_request.store(false, Ordering::SeqCst);
    let racing = env.file(3, 512, HsmState::empty(), 0).await;
    let mut racing_policy = policy.clone();
    racing_policy["metadata"]["name"] = format!("{}-racing-one-shot", env.filesystem).into();
    racing_policy["spec"]["enabled"] = true.into();
    racing_policy["spec"]["trigger"] = json!({"type": "periodic", "interval": "1ms"});
    racing_policy["spec"]["mode"] = "one_shot".into();
    racing_policy["spec"]["rules"]["entries"][0]["match"] = json!({"field": "object.size", "op": "eq", "value": 512});
    let racing_id = env.create(&racing_policy).await;
    *env.hsm.hold_observation.lock().unwrap() = Some(racing);
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let running = {
        let entries = env.entries.clone();
        let store = env.stores.clone();
        tokio::spawn(async move { store.process_rev8(&entries).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        env.hsm.observation_started.notified(),
    )
    .await
    .unwrap();
    safety["spec"]["limits"]["maxUnknownOutstanding"] = 5.into();
    server.put(&safety_url).json(&safety).await.assert_status_ok();
    env.hsm.resume_observation.notify_one();
    running.await.unwrap().unwrap();
    env.tick().await;
    let racing_runs = server
        .get(&format!("/api/policies/{racing_id}/runs"))
        .await
        .json::<Vec<Value>>();
    assert_eq!(racing_runs.len(), 2);
    assert_eq!(racing_runs[0]["state"], "cancelled");
    assert_eq!(racing_runs[1]["revision"], 2);
    assert_eq!(racing_runs[1]["state"], "finished");
    assert_eq!(env.submitted().last(), Some(&(racing, ActionKind::Archive, 17)));
    assert_eq!(env.submitted().len(), call_count + 1);

    // Clear the prior Archive's outstanding slot before testing a new FID.
    env.hsm.files.lock().unwrap().get_mut(&racing).unwrap().state = HsmUserStateInfo {
        states: copies,
        archive_id: 17,
        ..Default::default()
    };
    let racing_run = racing_runs[1]["id"].as_u64().unwrap();
    env.reconcile_until(&env.stores, async || {
        env.operations(racing_id, racing_run).await[0]["state"] == "completed"
    })
    .await;
    let late = env.file(4, 1024, HsmState::empty(), 0).await;
    let mut budgeted = env.policy("budgeted", archive);
    budgeted["spec"]["execution"]["budget"]["run"]["maxDuration"] = "2s".into();
    budgeted["spec"]["rules"]["entries"][0]["match"] = json!({"field": "object.size", "op": "eq", "value": 1024});
    budgeted["spec"]["rules"]["entries"][0]["action"]["params"] = json!({"archiveId": 17});
    let budgeted_id = env.create(&budgeted).await;
    let late_run = env.queue(budgeted_id).await;
    env.hsm.hold_submission.store(true, Ordering::SeqCst);
    tokio::join!(env.tick(), async {
        tokio::time::timeout(std::time::Duration::from_secs(5), env.hsm.submission_started.notified())
            .await
            .unwrap();
        assert_eq!(env.operations(budgeted_id, late_run).await[0]["state"], "submitting");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        env.hsm.resume_submission.notify_one();
    });
    let operations = env.operations(budgeted_id, late_run).await;
    assert_eq!(operations[0]["state"], "deferred");
    assert_eq!(operations[0]["reason"], "policy_run_duration");
    assert_eq!(operations[0]["attempted"], false);
    assert!(operations[0]["acceptedAt"].is_null());
    assert_eq!(env.submitted().len(), call_count + 1);

    let next_run = env.queue(budgeted_id).await;
    env.tick().await;
    assert_eq!(env.operations(budgeted_id, next_run).await[0]["state"], "submitted");
    assert_eq!(env.submitted().last(), Some(&(late, ActionKind::Archive, 17)));

    const HUA_NONE: u32 = 1;
    const HUA_ARCHIVE: u32 = 10;
    const HPS_WAITING: u32 = 1;
    {
        let mut files = env.hsm.files.lock().unwrap();
        let state = &mut files.get_mut(&late).unwrap().state;
        state.in_progress_state = HPS_WAITING;
        state.in_progress_action = HUA_ARCHIVE;
    }
    let pending = &env.operations(budgeted_id, next_run).await[0];
    env.reconcile_until(&env.stores, async || env.checked(pending).await)
        .await;
    assert_eq!(env.operations(budgeted_id, next_run).await[0]["state"], "submitted");

    // A later query reports NOOP and the requested archive is still absent.
    env.hsm
        .files
        .lock()
        .unwrap()
        .get_mut(&late)
        .unwrap()
        .state
        .in_progress_action = HUA_NONE;
    let fresh = open_policy_store(&env.entries, env.hsm.clone());
    env.reconcile_until(&fresh, async || {
        env.operations(budgeted_id, next_run).await[0]["state"] == "failed"
    })
    .await;
    let terminal = &env.operations(budgeted_id, next_run).await[0];
    assert_eq!(terminal["state"], "failed");
    assert_eq!(terminal["reason"], "hsm_target_state_not_reached");
    assert_eq!(terminal["completionEvidence"]["source"], "lustre_hsm_current_action");
    assert!(terminal["acceptedAt"].is_string());

    let retry = env.queue(budgeted_id).await;
    env.tick().await;
    assert_eq!(env.operations(budgeted_id, retry).await[0]["state"], "submitted");
    env.hsm.files.lock().unwrap().remove(&late).unwrap();
    env.hsm.missing_on_other_source.store(true, Ordering::SeqCst);
    let pending = &env.operations(budgeted_id, retry).await[0];
    env.reconcile_until(&env.stores, async || env.checked(pending).await)
        .await;
    assert_eq!(env.operations(budgeted_id, retry).await[0]["state"], "submitted");
    env.hsm.missing_on_other_source.store(false, Ordering::SeqCst);
    env.reconcile_until(&env.stores, async || {
        env.operations(budgeted_id, retry).await[0]["state"] == "failed"
    })
    .await;
    let missing = &env.operations(budgeted_id, retry).await[0];
    assert_eq!(missing["state"], "failed");
    assert_eq!(missing["reason"], "fid_missing");
    assert_eq!(missing["completionEvidence"]["source"], "lustre_fid_lookup");

    // Archive completes between the initial flags and current-action queries.
    let finishing = env.file(5, 2048, HsmState::empty(), 0).await;
    let mut confirming = budgeted;
    confirming["metadata"]["name"] = format!("{}-confirming", env.filesystem).into();
    confirming["spec"]["rules"]["entries"][0]["match"]["value"] = 2048.into();
    let confirming_id = env.create(&confirming).await;
    let confirming_run = env.queue(confirming_id).await;
    env.tick().await;
    assert_eq!(
        env.operations(confirming_id, confirming_run).await[0]["state"],
        "submitted"
    );
    let idle = HsmUserStateInfo {
        in_progress_action: HUA_NONE,
        in_progress_state: HPS_WAITING,
        ..Default::default()
    };
    env.hsm.files.lock().unwrap().get_mut(&finishing).unwrap().state = idle.clone();
    *env.hsm.finish_after_observation.lock().unwrap() = Some((
        finishing,
        HsmUserStateInfo {
            states: copies,
            archive_id: 17,
            ..idle
        },
    ));
    env.tick().await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while env.hsm.finish_after_observation.lock().unwrap().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background recovery did not observe the archive completing");

    // A worker with only an unrelated mount can advance the polling cursor
    // while the original worker holds authoritative evidence.
    let other_hsm = Arc::new(HsmFixture::default());
    other_hsm.missing_on_other_source.store(true, Ordering::SeqCst);
    let other_worker = open_policy_store(&env.entries, other_hsm);
    let pending = &env.operations(confirming_id, confirming_run).await[0];
    env.reconcile_until(&other_worker, async || env.checked(pending).await)
        .await;
    assert_eq!(
        env.operations(confirming_id, confirming_run).await[0]["state"],
        "submitted"
    );
    env.tick().await;
    assert_eq!(
        env.operations(confirming_id, confirming_run).await[0]["state"],
        "completed"
    );
}
