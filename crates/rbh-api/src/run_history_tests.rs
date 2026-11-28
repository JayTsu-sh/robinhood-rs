use axum::http::StatusCode;
use axum_test::TestServer;
use lustre_api::LuFid;
use rbh_entry_store::{EntryStore, fid_codec};
use serde_json::{Value, json};

use crate::handlers::scoped_api_tests::{archive_policy, policy_test_server, state_without_database_connection};

async fn history_fixture(prefix: &str) -> (EntryStore, TestServer, u64, String) {
    let entries = EntryStore::connect(&std::env::var("RBH_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    let name = format!("history-{prefix}-{}", uuid::Uuid::new_v4().simple());
    let mut document = archive_policy("history-fixture", &name);
    document["spec"]["execution"]
        .as_object_mut()
        .unwrap()
        .remove("activation");
    let policy = sqlx::query("INSERT INTO policies (name, definition, revision, enabled) VALUES (?, ?, 2, FALSE)")
        .bind(format!("{name}-current"))
        .bind(&document)
        .execute(entries.pool())
        .await
        .unwrap()
        .last_insert_id();
    let mut current = document.clone();
    current["metadata"]["name"] = json!(format!("{name}-current"));
    sqlx::query("INSERT INTO policy_revisions (policy_id, revision, definition) VALUES (?, 1, ?), (?, 2, ?)")
        .bind(policy)
        .bind(&document)
        .bind(policy)
        .bind(&current)
        .execute(entries.pool())
        .await
        .unwrap();
    let mut state = state_without_database_connection();
    state.policy_store = rbh_policy::PolicyStore::new(entries.pool().clone());
    state.entry_store = entries.clone();
    (entries, policy_test_server(state), policy, name)
}

async fn insert_run(entries: &EntryStore, policy: u64, revision: u64, state: &str, actor: Option<&str>) -> u64 {
    sqlx::query(
        "INSERT INTO policy_runs (policy_id, revision, state, requested_by, estimated_bytes) VALUES (?, ?, ?, ?, 9007199254740993)",
    )
    .bind(policy)
    .bind(revision)
    .bind(state)
    .bind(actor)
    .execute(entries.pool())
    .await
    .unwrap()
    .last_insert_id()
}

#[tokio::test]
#[ignore = "requires isolated MariaDB"]
async fn operator_controls_are_persisted_and_history_keeps_file_evidence() {
    let (entries, server, policy, _) = history_fixture("controls").await;
    sqlx::query("UPDATE policies SET enabled = TRUE WHERE id = ?")
        .bind(policy)
        .execute(entries.pool())
        .await
        .unwrap();
    let run = insert_run(&entries, policy, 2, "queued", Some("requester")).await;
    let control = format!("/api/runs/{run}/control");
    for (action, expected) in [("pause", "paused"), ("resume", "queued"), ("cancel", "cancelled")] {
        server
            .post(&control)
            .json(&json!({"action": action}))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let current = server.get(&format!("/api/runs/{run}")).await.json::<Value>();
        assert_eq!(current["state"], expected);
        assert_eq!(current["finishedAt"].is_string(), expected == "cancelled");
    }
    server
        .post(&control)
        .json(&json!({"action": "resume"}))
        .await
        .assert_status_bad_request();
    let pending = insert_run(&entries, policy, 2, "awaiting_approval", Some("requester")).await;
    server
        .post(&format!("/api/runs/{pending}/control"))
        .json(&json!({"action": "approve"}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let approved = server.get(&format!("/api/runs/{pending}")).await.json::<Value>();
    assert_eq!(approved["state"], "running");
    assert_eq!(approved["approvedBy"], "config-operator");

    let fid = fid_codec::encode(&LuFid::new(0x200000401, 115, 0));
    sqlx::query(
        "INSERT INTO policy_operations (run_id, filesystem, fid, action, state, size, paths, attempt_count, finished_at)
         VALUES (?, 'history-fixture', ?, 'lustre.pool_migrate', 'completed', 256, '[\"/team/report.bin\"]', 3, UTC_TIMESTAMP(6))",
    ).bind(run).bind(&fid[..]).execute(entries.pool()).await.unwrap();

    let operations = server.get(&format!("/api/runs/{run}/operations?path=report.bin")).await;
    operations.assert_status_ok();
    assert_eq!(operations.json::<Value>()["items"][0]["attemptCount"], 3);
    let summary = server.get(&format!("/api/runs/summary?policyId={policy}")).await;
    summary.assert_status_ok();
    let summary = summary.json::<Value>();
    assert_eq!(summary["total"], 2);
    assert_eq!(summary["completedLogicalBytes"], "256");
    let telemetry = server.get(&format!("/api/runs/{run}/telemetry")).await;
    telemetry.assert_status_ok();
    let telemetry = telemetry.json::<Value>();
    assert_eq!(telemetry["controls"][0]["action"], "cancel");
    assert_eq!(telemetry["controls"][0]["actor"], "config-operator");
    assert_eq!(telemetry["completions"][0]["logicalBytes"], "256");
}

#[tokio::test]
#[ignore = "requires isolated MariaDB"]
async fn run_history_filters_before_paging_and_reads_pinned_revisions() {
    let (entries, server, policy, original_name) = history_fixture("runs").await;
    let old = insert_run(&entries, policy, 1, "finished", Some("operator")).await;
    let recent = insert_run(&entries, policy, 2, "deferred", None).await;
    let other = history_fixture("runs").await;
    let unrelated = insert_run(&entries, other.2, 1, "finished", None).await;
    let list = format!("/api/runs?policyId={policy}&limit=1");
    let response = server.get(&list).await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["items"][0]["id"], recent);
    assert_eq!(page["nextCursor"], recent.to_string());
    let response = server.get(&format!("{list}&beforeId={recent}")).await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], old);
    assert!(page["nextCursor"].is_null());
    let response = server.get(&format!("{list}&state=finished&runId={old}")).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["items"][0]["policyName"], original_name);
    let response = server.get(&format!("{list}&runId={unrelated}")).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["items"], json!([]));
    let response = server
        .get(&format!("/api/runs?policyName={original_name}&state=finished"))
        .await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["items"][0]["id"], old);
    for query in ["limit=0", "limit=101", "state=completed", "runId=bad"] {
        server
            .get(&format!("/api/runs?{query}"))
            .await
            .assert_status_bad_request();
    }

    let large_revision = 9_007_199_254_740_993_u64;
    sqlx::query(
        "INSERT INTO policy_revisions (policy_id, revision, definition)
        SELECT policy_id, ?, definition FROM policy_revisions WHERE policy_id = ? AND revision = 2",
    )
    .bind(large_revision)
    .bind(policy)
    .execute(entries.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE policies SET revision = ? WHERE id = ?")
        .bind(large_revision)
        .bind(policy)
        .execute(entries.pool())
        .await
        .unwrap();
    let response = server
        .get(&format!("/api/policies/{policy}?includeLatestRun=true"))
        .await;
    response.assert_status_ok();
    let row = response.json::<Value>();
    assert_eq!(row["idExact"], policy.to_string());
    assert_eq!(row["revisionExact"], "9007199254740993");

    let response = server.delete(&format!("/api/policies/{policy}")).await;
    response.assert_status(StatusCode::NO_CONTENT);
    let response = server.get(&format!("/api/runs/{old}")).await;
    response.assert_status_ok();
    let run = response.json::<Value>();
    assert_eq!(run["revision"], 1);
    assert_eq!(run["idExact"], old.to_string());
    assert_eq!(run["revisionExact"], "1");
    assert_eq!(run["policyIdExact"], policy.to_string());
    assert_eq!(run["policyName"], original_name);
    assert_eq!(run["activationMode"], "preview");
    assert_eq!(run["policyMode"], "continuous");
    assert_eq!(run["requestedBy"], "operator");
    assert_eq!(run["estimatedBytesExact"], "9007199254740993");
    assert!(run["startedAt"].is_null());
    let revision = server.get(&format!("/api/policies/{policy}/revisions/1")).await;
    revision.assert_status_ok();
    assert_eq!(
        revision.json::<Value>()["definition"]["metadata"]["name"],
        original_name
    );
    server.get("/api/runs/0").await.assert_status_not_found();
}

#[tokio::test]
#[ignore = "requires isolated MariaDB"]
async fn policy_inventory_prefers_active_runs_and_falls_back_to_latest() {
    let (entries, server, policy, _) = history_fixture("latest").await;
    let detail = format!("/api/policies/{policy}?includeLatestRun=true");
    let response = server.get(&detail).await;
    response.assert_status_ok();
    assert!(response.json::<Value>()["latestRun"].is_null());

    let active = insert_run(&entries, policy, 1, "running", None).await;
    let queued = insert_run(&entries, policy, 2, "queued", None).await;
    let latest = insert_run(&entries, policy, 2, "deferred", None).await;
    let response = server.get(&detail).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["latestRun"]["id"], active.to_string());
    let response = server
        .get("/api/policies?configuredOnly=true&includeLatestRun=true")
        .await;
    response.assert_status_ok();
    let policies = response.json::<Vec<Value>>();
    let row = policies.iter().find(|row| row["id"] == policy).unwrap();
    assert_eq!(row["latestRun"]["state"], "running");
    assert_eq!(row["latestRun"]["revision"], "1");

    for (id, expected) in [(active, queued), (queued, latest)] {
        sqlx::query("UPDATE policy_runs SET state = 'finished' WHERE id = ?")
            .bind(id)
            .execute(entries.pool())
            .await
            .unwrap();
        let response = server.get(&detail).await;
        response.assert_status_ok();
        assert_eq!(response.json::<Value>()["latestRun"]["id"], expected.to_string());
    }

    let response = server.get(&format!("/api/policies/{policy}")).await;
    response.assert_status_ok();
    assert!(response.json::<Value>().get("latestRun").is_none());
}

#[tokio::test]
#[ignore = "requires isolated MariaDB"]
async fn operation_history_filters_beyond_one_hundred_rows_and_refreshes_layout_evidence() {
    let (entries, server, policy, name) = history_fixture("files").await;
    let run = insert_run(&entries, policy, 1, "finished", Some("operator")).await;
    let plan = json!({"sourcePool":"hot","targetPool":"cold","stripeCount":2,"stripeSize":1048576});
    let evidence =
        json!({"source":"lustre_layout","pool":"cold","ostIndices":[8,9],"observedAtMicros":1795852800000000_u64});
    let mut tx = entries.pool().begin().await.unwrap();
    for oid in 1..=103 {
        let (state, reason) = match oid {
            102 => ("failed", Some("layout_swap_failed")),
            103 => ("unknown", Some("migration_reconcile_pending")),
            _ => ("completed", None),
        };
        sqlx::query(
            "INSERT INTO policy_operations (run_id, filesystem, fid, action, state, reason, attempted, size, migration_plan, completion_evidence, finished_at)
             VALUES (?, ?, ?, 'lustre.pool_migrate', ?, ?, TRUE, 9007199254740993, ?, ?, IF(? = 'completed', UTC_TIMESTAMP(6), NULL))",
        )
        .bind(run)
        .bind(&name)
        .bind(fid_codec::encode(&LuFid::new(0x200000401, oid, 0)).to_vec())
        .bind(state)
        .bind(reason)
        // Keep the unresolved fixture away from automatic migration recovery.
        .bind((state != "unknown").then_some(&plan))
        .bind((state == "completed").then_some(&evidence))
        .bind(state)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let path = format!("/api/runs/{run}/operations");
    let response = server.get(&format!("{path}?limit=100")).await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    assert_eq!(page["items"][0]["completionEvidence"], evidence);
    assert_eq!(page["items"][0]["migrationPlan"], plan);
    assert_eq!(page["items"][0]["sizeExact"], "9007199254740993");
    assert!(page["items"][0]["finishedAt"].is_string());
    let cursor = page["nextCursor"].as_str().unwrap();
    let response = server.get(&format!("{path}?afterId={cursor}&limit=100")).await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["items"].as_array().unwrap().len(), 3);
    assert!(page["nextCursor"].is_null());

    let response = server.get(&format!("{path}?exceptionsOnly=true")).await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["items"][0]["state"], "failed");
    assert_eq!(page["items"][1]["state"], "unknown");

    let response = server
        .get(&format!(
            "{path}?state=failed&reason=swap&fid=%5B0x200000401%3A0x66%3A0x0%5D&limit=1"
        ))
        .await;
    response.assert_status_ok();
    let page = response.json::<Value>();
    assert_eq!(page["items"][0]["fid"], "[0x200000401:0x66:0x0]");
    assert_eq!(page["items"][0]["reason"], "layout_swap_failed");
    assert!(page["items"][0]["completionEvidence"].is_null());
    assert!(page["items"][0]["finishedAt"].is_null());
    assert!(page["nextCursor"].is_null());
    let response = server.get(&format!("{path}?reason=%25")).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["items"], json!([]));
    let response = server.get(&format!("{path}?reason=%20layout_swap")).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["items"], json!([]));
    for query in [
        "fid=bad",
        "fid=0x200000401%3A0x100000066%3A0x0",
        "limit=101",
        "state=finished",
    ] {
        server.get(&format!("{path}?{query}")).await.assert_status_bad_request();
    }
    server.get("/api/runs/0/operations").await.assert_status_not_found();

    let detail = format!("/api/runs/{run}");
    let response = server.get(&detail).await;
    response.assert_status_ok();
    let row = response.json::<Value>();
    assert_eq!(row["state"], "finished");
    assert_eq!(row["hasPendingOperations"], true);
    assert_eq!(row["operationResults"]["matchedCount"], 103);
    assert_eq!(row["operationResults"]["pendingCount"], 1);
    assert_eq!(row["operationResults"]["completedLogicalBytes"], "909727124728840293");
    assert_eq!(
        row["operationResults"]["byState"],
        json!({"completed": 101, "failed": 1, "unknown": 1})
    );

    let response = server.get(&format!("{path}/summary?exceptionsOnly=true&limit=1")).await;
    response.assert_status_ok();
    let summary = response.json::<Value>();
    assert_eq!(summary["matchedCount"], 2);
    assert_eq!(
        summary["byReason"],
        json!([
            {"reason": "layout_swap_failed", "count": 1, "byState": {"failed": 1}},
            {"reason": "migration_reconcile_pending", "count": 1, "byState": {"unknown": 1}}
        ])
    );

    let report_url = format!("{detail}/reports/run");
    let response = server.get(&report_url).await;
    response.assert_status_ok();
    assert_eq!(
        response.header("content-disposition"),
        format!("attachment; filename=\"run-{run}.json\"")
    );
    let report = response.json::<Value>();
    assert_eq!(report["scope"]["coverage"], "all_matching_operations");
    assert_eq!(report["run"]["policyName"], name);
    assert_eq!(report["policyRevision"]["revisionExact"], "1");
    assert_eq!(report["policyRevision"]["definition"]["metadata"]["name"], name);
    assert_eq!(report["operations"].as_array().unwrap().len(), 103);
    assert_eq!(report["operations"][100]["completionEvidence"], evidence);
    assert_eq!(report["operations"][102]["state"], "unknown");
    assert!(report["operations"][102]["finishedAt"].is_null());
    assert!(report["operations"][102].get("path").is_none());
    assert!(report["operations"][102].get("retryCount").is_none());

    let exception_url = format!(
        "{detail}/reports/exceptions?state=failed&reason=swap&fid=0x200000401%3A0x66%3A0x0&afterId={}&limit=1",
        u64::MAX
    );
    let response = server.get(&exception_url).await;
    response.assert_status_ok();
    let report = response.json::<Value>();
    assert_eq!(
        report["scope"]["filters"],
        json!({
            "state":"failed", "reasonContains":"swap", "fid":"[0x200000401:0x66:0x0]", "exceptionsOnly":true
        })
    );
    assert_eq!(report["operationResults"]["matchedCount"], 1);
    assert_eq!(report["operations"].as_array().unwrap().len(), 1);
    assert_eq!(report["operations"][0]["state"], "failed");
    sqlx::query(
        "UPDATE policy_operations SET state = 'completed', reason = NULL, completion_evidence = ?,
        checked_at = '2026-11-28 06:15:00.000000' WHERE run_id = ? AND state = 'unknown'",
    )
    .bind(&evidence)
    .bind(run)
    .execute(entries.pool())
    .await
    .unwrap();
    let response = server.get(&detail).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["hasPendingOperations"], false);
    let response = server.get(&format!("{path}/summary?exceptionsOnly=true")).await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>()["byReason"],
        json!([{"reason": "layout_swap_failed", "count": 1, "byState": {"failed": 1}}])
    );
    let response = server.get(&report_url).await;
    response.assert_status_ok();
    let report = response.json::<Value>();
    assert_eq!(report["operationResults"]["pendingCount"], 0);
    assert_eq!(
        report["operationResults"]["completedLogicalBytes"],
        "918734323983581286"
    );
    assert_eq!(report["operations"][102]["completionEvidence"], evidence);
    let response = server.get(&format!("{detail}/reports/exceptions")).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["operations"].as_array().unwrap().len(), 1);
    let response = server.get(&format!("{path}?fid=0x200000401%3A0x67%3A0x0")).await;
    response.assert_status_ok();
    let row = &response.json::<Value>()["items"][0];
    assert_eq!(row["state"], "completed");
    assert_eq!(row["completionEvidence"], evidence);
    assert_eq!(row["lastCheckedAt"], "2026-11-28T06:15:00Z");
}

#[tokio::test]
#[ignore = "requires isolated MariaDB"]
async fn exception_report_covers_more_than_one_page_and_combines_the_same_reason_across_states() {
    let (entries, server, policy, name) = history_fixture("anomalies").await;
    let run = insert_run(&entries, policy, 1, "finished", None).await;
    let mut tx = entries.pool().begin().await.unwrap();
    for oid in 1..=103 {
        let (state, reason) = match oid {
            101 => ("deferred", Some("fid_missing")),
            102 => ("deferred", Some("FID_missing")),
            103 => ("unknown", None),
            _ => ("failed", Some("fid_missing")),
        };
        sqlx::query(
            "INSERT INTO policy_operations (run_id, filesystem, fid, action, state, reason, size)
             VALUES (?, ?, ?, 'lustre.pool_migrate', ?, ?, 1)",
        )
        .bind(run)
        .bind(&name)
        .bind(fid_codec::encode(&LuFid::new(0x200000401, oid, 0)).to_vec())
        .bind(state)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let base = format!("/api/runs/{run}");
    let response = server
        .get(&format!(
            "{base}/operations/summary?exceptionsOnly=true&afterId=100&limit=1"
        ))
        .await;
    response.assert_status_ok();
    let summary = response.json::<Value>();
    assert_eq!(summary["matchedCount"], 103);
    assert_eq!(summary["byState"], json!({"failed": 100, "deferred": 2, "unknown": 1}));
    assert_eq!(
        summary["byReason"],
        json!([
            {"reason": null, "count": 1, "byState": {"unknown": 1}},
            {"reason": "FID_missing", "count": 1, "byState": {"deferred": 1}},
            {"reason": "fid_missing", "count": 101, "byState": {"failed": 100, "deferred": 1}}
        ])
    );
    let response = server.get(&format!("{base}/reports/exceptions")).await;
    response.assert_status_ok();
    let report = response.json::<Value>();
    assert_eq!(report["operationResults"], summary);
    assert_eq!(report["operations"].as_array().unwrap().len(), 103);
    assert_eq!(report["operations"][102]["fid"], "[0x200000401:0x67:0x0]");
    assert!(report["operations"][102]["reason"].is_null());
}
