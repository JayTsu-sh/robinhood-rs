use std::{
    fs::{self, File},
    io::Write,
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    sync::Arc,
    time::Duration,
};

use axum::http::StatusCode;
use axum_test::TestServer;
use bytes::Bytes;
use lustre_api::{LuFid, LustreApi};
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, EntryStore, FileSystemConfig, FileSystemId, ObjectId,
    ScopedEntryRow, fid_codec,
};
use rbh_policy::{
    PolicyStore,
    policy::{
        execution::{LustreHsm, Principal, Runtime},
        heat::HeatSampler,
    },
};
use serde_json::{Value, json};

use crate::handlers::scoped_api_tests::{
    archive_policy, integration_enabled, policy_test_server, resource_document, state_without_database_connection,
};

async fn catalog_file(entries: &EntryStore, filesystem: &FileSystemId, fid: LuFid, pool: &str) {
    entries
        .upsert_scoped_entry(&ScopedEntryRow {
            key: EntryKey::new(filesystem.clone(), ObjectId::Lustre(fid)),
            parent: None,
            name: Bytes::from_static(b"heat-fixture"),
            kind: EntryKind::File,
            size: 4096,
            blocks: 8,
            uid: 0,
            gid: 0,
            projid: 0,
            mode: 0o600,
            nlink: 1,
            atime: 0,
            mtime: 0,
            ctime: 0,
            stripe_count: None,
            stripe_size: None,
            stripe_items: vec![],
            pool_name: Some(pool.into()),
            sm_status: json!({}),
            last_seen: 0,
            depth: 1,
        })
        .await
        .unwrap();
}

fn heat_policy(filesystem: &FileSystemId) -> Value {
    let mut document = archive_policy(filesystem.as_str(), filesystem.as_str());
    document["spec"]["enabled"] = false.into();
    document["spec"]["rules"]["entries"] = json!([
        {
            "match": {"any": [
                {"field": "lustre.heat.readsample", "op": "lt", "value": 1},
                {"not": {"field": "lustre.heat.readbyte", "op": "gte", "value": 4096}}
            ]},
            "decision": "skip"
        },
        {
            "match": true,
            "decision": "execute",
            "action": {
                "type": "lustre.pool_migrate",
                "params": {"sourcePool": "cold", "targetPool": "hot", "maxFileBytes": "1GiB"}
            }
        }
    ]);
    document
}

#[tokio::test]
async fn policy_configures_minimum_pool_residence_through_rest() {
    if !integration_enabled() {
        return;
    }
    let Ok(mount) = std::env::var("RBH_TEST_LUSTRE_MOUNT") else {
        return;
    };
    let database = std::env::var("RBH_TEST_DATABASE_URL").expect("isolated MariaDB test database");
    let entries = EntryStore::connect(&database).await.unwrap();
    let filesystem = FileSystemId::new(format!("residence-{}", uuid::Uuid::new_v4().simple())).unwrap();
    entries
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::Lustre,
            mount_path: mount.into(),
            capabilities: BackendCapabilities {
                hsm: true,
                stripe: true,
                ost: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let mut state = state_without_database_connection();
    state.entry_store = entries.clone();
    state.policy_store = PolicyStore::new(entries.pool().clone());
    let server = policy_test_server(state);
    let mut policy = heat_policy(&filesystem);
    policy["spec"]["rules"]["entries"][1]["action"]["params"]["minResidence"] = "7d".into();
    create(&server, &policy).await;
}

async fn create(server: &TestServer, document: &Value) -> u64 {
    let response = server.post("/api/policies").json(document).await;
    response.assert_status(StatusCode::CREATED);
    response.json::<Value>()["id"].as_u64().unwrap()
}

async fn preview(server: &TestServer, policy: u64, fid: LuFid) -> Value {
    let fid = fid.to_string();
    let url = format!(
        "/api/policies/{policy}/preview?heatFid={}",
        fid.trim_matches(['[', ']'])
    );
    let response = server.post(&url).await;
    response.assert_status_ok();
    response.json()
}

#[tokio::test]
async fn policy_periodic_read_heat_warms_only_eligible_cold_files() {
    let Ok(mounts) = std::env::var("RBH_TEST_HEAT_BUSINESS_MOUNTS") else {
        return;
    };
    if !integration_enabled() {
        return;
    }
    let (first, second) = mounts.split_once(':').expect("two distinct business mount roots");
    let executor = std::env::var("RBH_TEST_LUSTRE_MOUNT").expect("separate executor mount root");
    let executor = Path::new(&executor);
    let database = std::env::var("RBH_TEST_DATABASE_URL").expect("isolated MariaDB test database");
    let entries = EntryStore::connect(&database).await.unwrap();
    let filesystem = FileSystemId::new(format!("warm-{}", uuid::Uuid::new_v4().simple())).unwrap();
    let config = FileSystemConfig {
        id: filesystem.clone(),
        backend: BackendKind::Lustre,
        mount_path: executor.into(),
        capabilities: BackendCapabilities {
            hsm: true,
            stripe: true,
            ost: true,
            ..Default::default()
        },
    };
    entries.register_filesystem(&config).await.unwrap();
    let store = PolicyStore::new(entries.pool().clone())
        .with_read_heat(
            config.clone(),
            serde_json::from_value(json!({
                "clients": {"reader-a": first, "reader-b": second},
                "decay": {"period_seconds": 60, "percentage": 80},
                "max_age_seconds": 60,
                "interval_seconds": 1,
                "batch_size": 8
            }))
            .unwrap(),
            [config.mount_path.clone()].into(),
        )
        .unwrap()
        .with_execution_runtime(Runtime {
            principal: Principal {
                service_account: "tiering-daemon".into(),
                roles: vec![],
            },
            hsm: Arc::new(LustreHsm),
        });
    let mut state = state_without_database_connection();
    state.entry_store = entries.clone();
    state.policy_store = store.clone();
    let server = policy_test_server(state);
    let action = "lustre.pool_migrate";
    for (kind, path, spec) in [
        (
            "ExecutionProfile",
            "execution-profiles",
            json!({
                "concurrency": {"global": 1},
                "retry": {"maxAttempts": 1, "backoff": "fixed", "initialDelay": "1s", "maxDelay": "1s", "retryableErrors": ["transient_io"]},
                "workerLease": {"ttl": "1m", "renewEvery": "10s", "onLeaseLost": "stop_and_reconcile"},
                "circuitBreaker": {"scopes": [{
                    "domain": {"key": "policy"}, "errorClasses": ["backend_unavailable"], "evaluation": "any",
                    "conditions": [{"type": "consecutive_failures", "threshold": 3}],
                    "onOpen": "suspend_submissions", "resetMode": "manual"
                }]}
            }),
        ),
        (
            "AuthorizationPolicy",
            "authorization-policies",
            json!({
                "defaultDecision": "deny",
                "statements": [{
                    "id": "tiering", "actions": [action], "decision": "allow",
                    "subjects": {"serviceAccounts": ["tiering-daemon"]},
                    "scope": {"filesystems": [filesystem], "objectTypes": ["file"]}
                }]
            }),
        ),
        (
            "ApprovalPolicy",
            "approval-policies",
            json!({
                "defaultDecision": "deny",
                "requirements": [{"id": "tiering", "actions": [action], "mode": "not_required"}],
                "floorDefaults": {"minApprovers": 1, "approverRoles": ["approver"]}
            }),
        ),
        (
            "SystemSafetyPolicy",
            "system-safety-policies",
            json!({
                "scope": {"filesystems": [filesystem]}, "allowedActions": [action]
            }),
        ),
    ] {
        server
            .post(&format!("/api/{path}"))
            .json(&resource_document(kind, filesystem.as_str(), spec))
            .await
            .assert_status(StatusCode::CREATED);
    }

    let lab = executor.join(".rbh-tiering-lab/hot");
    let dir = tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir_in(lab).unwrap();
        fs::write(dir.path().join("active"), [5; 4096]).unwrap();
        fs::write(dir.path().join("quiet"), [5; 4096]).unwrap();
        fs::hard_link(dir.path().join("active"), dir.path().join("alias")).unwrap();
        dir
    })
    .await
    .unwrap();
    let path = dir.path().join("active");
    let alias = dir.path().join("alias");
    let quiet_path = dir.path().join("quiet");
    let files = [path.clone(), quiet_path.clone()];
    let (inode, rows) = tokio::task::spawn_blocking(move || {
        (
            fs::metadata(&files[0]).unwrap().ino(),
            files.map(|file| rbh_fs_scan::build_entry(&LustreApi, &file, None, 1).unwrap()),
        )
    })
    .await
    .unwrap();
    let fid = rows[0].fid;
    for row in rows {
        assert_eq!(row.pool_name.as_deref(), Some("hot"));
        entries.upsert_lustre_entry(&filesystem, &row).await.unwrap();
    }
    let check_placement = async |file: &Path, pool: &'static str| {
        let (file, mount) = (file.to_owned(), executor.to_owned());
        tokio::task::spawn_blocking(move || {
            let layout = LustreApi.get_ost_layout(&file).unwrap();
            assert_eq!(layout.pool_name.as_deref(), Some(pool));
            assert!(!layout.stripe_items.is_empty());
            let members = LustreApi.pool_members(&mount, pool).unwrap();
            assert!(layout.stripe_items.iter().all(|ost| members.contains(ost)));
        })
        .await
        .unwrap();
    };
    let check_identity = async || {
        let names = [path.clone(), alias.clone()];
        tokio::task::spawn_blocking(move || {
            for name in names {
                assert_eq!(LustreApi.path_to_fid(&name).unwrap(), fid);
                let metadata = fs::metadata(&name).unwrap();
                assert_eq!((metadata.ino(), metadata.nlink()), (inode, 2));
                assert_eq!(fs::read(name).unwrap(), [5; 4096]);
            }
        })
        .await
        .unwrap();
    };

    let mut cooling = archive_policy(filesystem.as_str(), &format!("{filesystem}-cool"));
    cooling["spec"]["trigger"] = json!({"type": "periodic", "interval": "10ms"});
    cooling["spec"]["mode"] = "one_shot".into();
    cooling["spec"]["execution"]["activation"]["mode"] = "enforced".into();
    cooling["spec"]["execution"]["budget"]["run"]["maxDuration"] = "1m".into();
    cooling["spec"]["execution"]["profileRef"] = json!(filesystem);
    cooling["spec"]["governance"] = json!({"authorizationRef": filesystem, "approvalPolicyRef": filesystem});
    cooling["spec"]["rules"]["entries"][0]["action"] = json!({
        "type": action, "params": {"sourcePool": "hot", "targetPool": "cold", "maxFileBytes": "1MiB"}
    });
    create(&server, &cooling).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    store.process_policy_queue(&entries).await.unwrap();
    check_placement(&path, "cold").await;
    check_placement(&quiet_path, "cold").await;
    check_identity().await;

    let mut warming = heat_policy(&filesystem);
    warming["metadata"]["name"] = format!("{filesystem}-warm").into();
    warming["spec"]["enabled"] = true.into();
    warming["spec"]["trigger"] = cooling["spec"]["trigger"].clone();
    warming["spec"]["execution"] = cooling["spec"]["execution"].clone();
    warming["spec"]["governance"] = cooling["spec"]["governance"].clone();
    warming["spec"]["rules"]["entries"][1]["action"]["params"]["minResidence"] = "1h".into();
    let mut bypass = warming.clone();
    let rules = bypass["spec"]["rules"]["entries"].as_array_mut().unwrap();
    let mut first_execute = rules[1].clone();
    first_execute["match"] = json!({"field": "lustre.heat.readsample", "op": "gte", "value": 1});
    rules[0] = first_execute;
    server
        .post("/api/policies")
        .json(&bypass)
        .await
        .assert_status_bad_request();
    let warm_id = create(&server, &warming).await;
    let read_path = Path::new(first).join(path.strip_prefix(executor).unwrap());
    let _reader = tokio::task::spawn_blocking(move || {
        let reader = File::open(read_path).unwrap();
        for _ in 0..32 {
            assert_eq!(reader.read_at(&mut [0; 4096], 0).unwrap(), 4096);
        }
        reader
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut sampler = HeatSampler::default();
    assert_eq!(
        store.sample_read_heat_batch(&filesystem, &mut sampler).await.unwrap(),
        0
    );
    store.process_policy_queue(&entries).await.unwrap();
    assert_eq!(preview(&server, warm_id, fid).await["candidateCount"], 0);
    check_placement(&path, "cold").await;

    warming["spec"]["rules"]["entries"][1]["action"]["params"]["minResidence"] = "1ms".into();
    server
        .put(&format!("/api/policies/{warm_id}"))
        .json(&warming)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        preview(&server, warm_id, fid).await["readHeat"]["deferredByReason"],
        json!({"heat_sample_missing": 2})
    );
    assert_eq!(
        store.sample_read_heat_batch(&filesystem, &mut sampler).await.unwrap(),
        2
    );
    let ready = preview(&server, warm_id, fid).await;
    assert_eq!(ready["candidateCount"], 1, "{ready}");
    assert_eq!(ready["readHeat"]["observations"][1]["readSamples"], 0, "{ready}");

    sqlx::query(
        "UPDATE lustre_read_heat SET observed_at = UTC_TIMESTAMP(6) - INTERVAL 61 SECOND
         WHERE filesystem_id = ? AND object_id = ? AND client_id = 'reader-b'",
    )
    .bind(filesystem.as_str())
    .bind(fid_codec::encode(&fid).to_vec())
    .execute(entries.pool())
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    store.process_policy_queue(&entries).await.unwrap();
    let runs = server.get(&format!("/api/policies/{warm_id}/runs")).await;
    runs.assert_status_ok();
    assert_eq!(
        runs.json::<Vec<Value>>().last().unwrap()["heatDeferredByReason"],
        json!({"heat_sample_stale": 1})
    );
    check_placement(&path, "cold").await;

    assert_eq!(
        store.sample_read_heat_batch(&filesystem, &mut sampler).await.unwrap(),
        2
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    store.process_policy_queue(&entries).await.unwrap();
    check_placement(&path, "hot").await;
    check_placement(&quiet_path, "cold").await;
    check_identity().await;
}

#[tokio::test]
async fn policy_preview_uses_read_heat_from_the_configured_business_mounts() {
    let Ok(mounts) = std::env::var("RBH_TEST_HEAT_BUSINESS_MOUNTS") else {
        return;
    };
    if !integration_enabled() {
        return;
    }
    let (first, second) = mounts.split_once(':').expect("two distinct business mount roots");
    let executor = std::env::var("RBH_TEST_LUSTRE_MOUNT").expect("separate executor mount root");
    let database = std::env::var("RBH_TEST_DATABASE_URL").expect("isolated MariaDB test database");
    let entries = EntryStore::connect(&database).await.unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let filesystem = FileSystemId::new(format!("heat-{suffix}")).unwrap();
    let unconfigured = FileSystemId::new(format!("unconfigured-{suffix}")).unwrap();
    let config = FileSystemConfig {
        id: filesystem.clone(),
        backend: BackendKind::Lustre,
        mount_path: executor.into(),
        capabilities: BackendCapabilities {
            hsm: true,
            stripe: true,
            ost: true,
            ..Default::default()
        },
    };
    entries.register_filesystem(&config).await.unwrap();
    entries
        .register_filesystem(&FileSystemConfig {
            id: unconfigured.clone(),
            ..config.clone()
        })
        .await
        .unwrap();
    let directory = Path::new(first).join(".rbh-tiering-lab/cold");
    let mut fixture = tempfile::NamedTempFile::new_in(&directory).unwrap();
    fixture.write_all(&[5; 4096]).unwrap();
    fixture.as_file().sync_all().unwrap();
    let fid = LustreApi.path_to_fid(fixture.path()).unwrap();
    let second_file = tempfile::NamedTempFile::new_in(&directory).unwrap();
    second_file.as_file().set_len(4096).unwrap();
    let second_fid = LustreApi.path_to_fid(second_file.path()).unwrap();
    for cold in [fid, second_fid] {
        catalog_file(&entries, &filesystem, cold, "cold").await;
    }
    catalog_file(&entries, &filesystem, LuFid::new(0x200000401, 1, 0), "hot").await;
    catalog_file(&entries, &unconfigured, fid, "cold").await;

    let second_client = match std::env::var("RBH_TEST_HEAT_AGENT_ENDPOINT") {
        Ok(endpoint) => json!({
            "endpoint": endpoint,
            "token_file": std::env::var("RBH_TEST_HEAT_AGENT_TOKEN_FILE").expect("agent token file"),
        }),
        Err(_) => json!(second),
    };
    let policy_store = PolicyStore::new(entries.pool().clone())
        .with_read_heat(
            config.clone(),
            serde_json::from_value(json!({
                "clients": {"reader-a": first, "reader-b": second_client},
                "decay": {"period_seconds": 60, "percentage": 80},
                "max_age_seconds": 60,
                "interval_seconds": 1,
                "batch_size": 1
            }))
            .unwrap(),
            [config.mount_path.clone(), directory.join("unmounted-peer")].into(),
        )
        .unwrap();
    let mut state = state_without_database_connection();
    state.entry_store = entries.clone();
    state.policy_store = policy_store.clone();
    let server = policy_test_server(state);

    let mut document = heat_policy(&filesystem);
    let id = create(&server, &document).await;
    let mut sampler = HeatSampler::default();
    assert_eq!(
        policy_store
            .sample_read_heat_batch(&filesystem, &mut sampler)
            .await
            .unwrap(),
        0
    );
    let unsampled = preview(&server, id, fid).await;
    assert_eq!(unsampled["candidateCount"], 0, "{unsampled}");
    assert_eq!(
        unsampled["readHeat"]["deferredByReason"],
        json!({"heat_sample_missing": 2}),
        "{unsampled}"
    );

    document["spec"]["enabled"] = true.into();
    server
        .put(&format!("/api/policies/{id}"))
        .json(&document)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        policy_store
            .sample_read_heat_batch(&filesystem, &mut sampler)
            .await
            .unwrap(),
        1
    );
    let first_batch = preview(&server, id, fid).await;
    assert_eq!(
        first_batch["readHeat"]["deferredByReason"],
        json!({"heat_sample_missing": 1}),
        "{first_batch}"
    );

    assert_eq!(
        policy_store
            .sample_read_heat_batch(&filesystem, &mut sampler)
            .await
            .unwrap(),
        1
    );
    let zero = preview(&server, id, fid).await;
    assert_eq!(zero["candidateCount"], 0, "{zero}");
    assert_eq!(zero["readHeat"]["deferredByReason"], json!({}), "{zero}");
    assert_eq!(zero["readHeat"]["observations"][0]["readSamples"], 0, "{zero}");
    assert_eq!(zero["readHeat"]["observations"][1]["readBytes"], 0, "{zero}");

    for _ in 0..32 {
        assert_eq!(fixture.as_file().read_at(&mut [0; 4096], 0).unwrap(), 4096);
    }
    for _ in 0..2 {
        assert_eq!(
            policy_store
                .sample_read_heat_batch(&filesystem, &mut sampler)
                .await
                .unwrap(),
            1
        );
    }
    let read = preview(&server, id, fid).await;
    assert_eq!(read["candidateCount"], 1, "{read}");
    assert_eq!(read["estimatedBytes"], 4096, "{read}");
    assert_eq!(read["readHeat"]["clients"], json!(["reader-a", "reader-b"]));
    assert_eq!(
        read["readHeat"]["decay"],
        json!({"periodSeconds": 60, "percentage": 80})
    );
    assert!(
        read["readHeat"]["observations"][0]["readSamples"].as_u64().unwrap() > 0,
        "{read}"
    );
    assert_eq!(read["readHeat"]["observations"][1]["readSamples"], 0, "{read}");

    let mut other_document = heat_policy(&unconfigured);
    other_document["spec"]["enabled"] = true.into();
    let other = create(&server, &other_document).await;
    assert_eq!(
        policy_store
            .sample_read_heat_batch(&unconfigured, &mut HeatSampler::default())
            .await
            .unwrap(),
        0,
    );
    let scoped = preview(&server, other, fid).await;
    assert_eq!(scoped["candidateCount"], 0, "{scoped}");
    assert_eq!(
        scoped["readHeat"]["deferredByReason"],
        json!({"heat_not_configured": 1}),
        "{scoped}"
    );

    // Inject an expired timestamp for one necessary client without changing
    // the time or configuration of the actual test Lustre clients.
    sqlx::query(
        "UPDATE lustre_read_heat SET observed_at = UTC_TIMESTAMP(6) - INTERVAL 61 SECOND
                 WHERE filesystem_id = ? AND object_id = ? AND client_id = 'reader-b'",
    )
    .bind(filesystem.as_str())
    .bind(fid_codec::encode(&fid).to_vec())
    .execute(entries.pool())
    .await
    .unwrap();
    let expired = preview(&server, id, fid).await;
    assert_eq!(expired["candidateCount"], 0, "{expired}");
    assert_eq!(
        expired["readHeat"]["deferredByReason"],
        json!({"heat_sample_stale": 1}),
        "{expired}"
    );
    assert_eq!(
        expired["readHeat"]["observations"][1]["status"], "heat_sample_stale",
        "{expired}"
    );

    sqlx::query(
        "UPDATE lustre_read_heat SET observed_at = UTC_TIMESTAMP(6), decay_percentage = 50
                 WHERE filesystem_id = ? AND object_id = ? AND client_id = 'reader-b'",
    )
    .bind(filesystem.as_str())
    .bind(fid_codec::encode(&fid).to_vec())
    .execute(entries.pool())
    .await
    .unwrap();
    let mismatch = preview(&server, id, fid).await;
    assert_eq!(mismatch["candidateCount"], 0, "{mismatch}");
    assert_eq!(
        mismatch["readHeat"]["deferredByReason"],
        json!({"heat_decay_mismatch": 1}),
        "{mismatch}"
    );
    assert_eq!(
        mismatch["readHeat"]["observations"][1]["decayPercentage"], 50,
        "{mismatch}"
    );
}
