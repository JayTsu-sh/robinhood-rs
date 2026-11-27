use std::{io::Write, os::unix::fs::FileExt, path::Path};

use axum::http::StatusCode;
use axum_test::TestServer;
use bytes::Bytes;
use lustre_api::{LuFid, LustreApi};
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, EntryStore, FileSystemConfig, FileSystemId, ObjectId,
    ScopedEntryRow, fid_codec,
};
use rbh_policy::{PolicyStore, rev8::heat::HeatSampler};
use serde_json::{Value, json};

use crate::handlers::scoped_api_tests::{
    integration_enabled, rev8_archive_policy, rev8_test_server, state_without_database_connection,
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
    let mut document = rev8_archive_policy(filesystem.as_str(), filesystem.as_str());
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
async fn rev8_preview_uses_read_heat_from_the_configured_business_mounts() {
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
    let server = rev8_test_server(state);

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
