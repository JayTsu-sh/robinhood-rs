use std::path::PathBuf;

use axum_test::TestServer;
use lustre_api::{LuFid, LustreApi};
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, EntryStore, FileSystemConfig, FileSystemId, ObjectId,
    ScopedEntryRow, model::ScopedNamespaceEdge,
};
use rbh_policy::PolicyStore;
use serde_json::json;

use crate::handlers::scoped_api_tests::{rev8_archive_policy, rev8_test_server, state_without_database_connection};

#[tokio::test]
#[ignore = "requires isolated MariaDB and a Lustre mount with hot and cold pools"]
async fn scoped_draft_preview_evaluates_all_links_without_creating_a_policy_or_run() {
    let database_url = std::env::var("RBH_TEST_DATABASE_URL").unwrap();
    let mount = PathBuf::from(std::env::var_os("RBH_TEST_LUSTRE_MOUNT").unwrap());
    let entries = EntryStore::connect(&database_url).await.unwrap();
    let filesystem = FileSystemId::new(format!("scope-{}", uuid::Uuid::new_v4().simple())).unwrap();
    entries
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::Lustre,
            mount_path: mount.clone(),
            capabilities: BackendCapabilities {
                hsm: true,
                stripe: true,
                ost: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let root = ObjectId::Lustre(LustreApi.path_to_fid(&mount).unwrap());
    let indexed = |oid| ObjectId::Lustre(LuFid::new(0x200000401, oid, 0));
    let allowed = indexed(1);
    let nested = indexed(2);
    let sibling = indexed(3);
    let excluded = indexed(4);
    let excluded_child = indexed(5);
    let unavailable = LuFid::new(0x200000401, 30, 0);
    let write = async |object, kind, size, nlink, parent| {
        entries
            .upsert_scoped_entry(&ScopedEntryRow {
                key: EntryKey::new(filesystem.clone(), object),
                parent,
                name: Default::default(),
                kind,
                size,
                blocks: 0,
                uid: 0,
                gid: 0,
                projid: 0,
                mode: 0,
                nlink,
                atime: 10,
                mtime: 10,
                ctime: 10,
                stripe_count: None,
                stripe_size: None,
                stripe_items: Vec::new(),
                pool_name: (kind == EntryKind::File).then(|| "hot".into()),
                sm_status: json!({}),
                last_seen: 0,
                depth: 0,
            })
            .await
            .unwrap();
    };
    let link = async |object, parent, name: &str| {
        entries
            .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
                filesystem: filesystem.clone(),
                parent,
                name: bytes::Bytes::copy_from_slice(name.as_bytes()),
                object,
            })
            .await
            .unwrap();
    };
    write(root, EntryKind::Directory, 0, 2, None).await;
    for (dir, parent, name) in [
        (allowed, root, "team"),
        (nested, allowed, "project"),
        (sibling, root, "team-old"),
        (excluded, allowed, "archive"),
        (excluded_child, excluded, "nested"),
    ] {
        write(
            dir,
            EntryKind::Directory,
            0,
            2,
            Some(EntryKey::new(filesystem.clone(), parent)),
        )
        .await;
        link(dir, parent, name).await;
    }
    for (oid, bytes, count, paths) in [
        (20, 100, 2, &[(allowed, "a"), (nested, "a")][..]),
        (21, 200, 2, &[(allowed, "b"), (sibling, "b")][..]),
        (22, 300, 2, &[(allowed, "c"), (excluded_child, "c")][..]),
        (23, 400, 2, &[(allowed, "d")][..]),
        (24, 500, 1, &[(sibling, "e")][..]),
        (25, 600, 1, &[(indexed(999), "orphan")][..]),
        (26, 10, 1, &[(allowed, "too-small")][..]),
        (27, 700, 1, &[][..]),
        (28, 800, 1, &[(allowed, "f"), (indexed(999), "stale")][..]),
        (29, 900, 1, &[(allowed, "g"), (sibling, "stale")][..]),
    ] {
        write(indexed(oid), EntryKind::File, bytes, count, None).await;
        for (parent, name) in paths {
            link(indexed(oid), *parent, name).await;
        }
    }

    let mut state = state_without_database_connection();
    // A missing live observation can coincide with an apparently complete
    // set of catalog links. Keep the previous count for ordinary readers.
    write(ObjectId::Lustre(unavailable), EntryKind::File, 1000, 1, None).await;
    entries
        .link_lustre_name(
            &filesystem,
            &unavailable,
            LuFid::new(0x200000401, 1, 0),
            b"unavailable",
            rbh_entry_store::store::LustreLinkState::Unavailable,
            20,
        )
        .await
        .unwrap();
    state.entry_store = entries.clone();
    state.policy_store = PolicyStore::new(entries.pool().clone()).with_configured_filesystems([filesystem.clone()]);
    let server = TestServer::new(crate::router(state.clone())).unwrap();
    let mut document = rev8_archive_policy(filesystem.as_str(), filesystem.as_str());
    document["spec"]["enabled"] = false.into();
    document["spec"]["target"]["directoryScope"] = json!({"root": "/team", "excludedSubtrees": ["/team/archive"]});
    document["spec"]["rules"]["entries"][0]["action"] = json!({
        "type": "lustre.pool_migrate",
        "params": {"sourcePool": "hot", "targetPool": "cold", "maxFileBytes": "1GiB"}
    });

    let response = server.post("/api/policies/preview").json(&document).await;
    response.assert_status_ok();
    let preview = response.json::<serde_json::Value>();
    assert_eq!(preview["revision"], serde_json::Value::Null);
    assert_eq!(preview["candidateCount"], 1);
    assert_eq!(preview["estimatedBytes"], 100);
    assert_eq!(
        preview["directoryScope"],
        json!({
            "excludedByReason": {"link_in_excluded_subtree": 1, "link_outside_scope": 2},
            "deferredByReason": {"incomplete_link_evidence": 6}
        })
    );
    let entry = server
        .get(&format!("/api/entries/{unavailable}?filesystem={filesystem}"))
        .await;
    entry.assert_status_ok();
    assert_eq!(entry.json::<serde_json::Value>()["nlink"], 1);
    assert!(preview["resourceBindings"].as_array().unwrap().iter().any(|binding| {
        binding["name"] == "lustre.pool_migrate" && binding["revision"].as_u64().is_some_and(|revision| revision > 0)
    }));
    let mut ordered = document.clone();
    let rules = &mut ordered["spec"]["rules"];
    let action = rules["entries"][0]["action"].clone();
    rules["defaults"] = json!({"action": action});
    rules["entries"] = json!([
        {"name": "keep", "priority": 2, "match": true, "decision": "skip"},
        {"name": "migrate", "priority": 1, "match": {"field": "object.size", "op": "gte", "value": 64}, "decision": "execute"}
    ]);
    let response = server.post("/api/policies/preview").json(&ordered).await;
    response.assert_status_ok();
    assert_eq!(response.json::<serde_json::Value>()["candidateCount"], 0);
    ordered["spec"]["rules"]["entries"][1]["priority"] = 3.into();
    let response = server.post("/api/policies/preview").json(&ordered).await;
    response.assert_status_ok();
    assert_eq!(response.json::<serde_json::Value>()["candidateCount"], 1);
    ordered["spec"]["rules"]["entries"].as_array_mut().unwrap().pop();
    let response = server.post("/api/policies/preview").json(&ordered).await;
    response.assert_status_ok();
    let dormant = response.json::<serde_json::Value>();
    assert_eq!(dormant["candidateCount"], 0);
    assert!(
        dormant["resourceBindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|binding| { binding["name"] == "lustre.pool_migrate" })
    );
    let mut heat_preview = document.clone();
    heat_preview["spec"]["target"]["where"] = json!({"field": "lustre.heat.readsample", "op": "gte", "value": 3});
    let response = server.post("/api/policies/preview").json(&heat_preview).await;
    response.assert_status_ok();
    let heat = response.json::<serde_json::Value>();
    assert_eq!(heat["candidateCount"], 0);
    assert_eq!(heat["directoryScope"], preview["directoryScope"]);
    assert_eq!(heat["readHeat"]["deferredByReason"], json!({"heat_not_configured": 1}));

    let listed = server.get("/api/policies?configuredOnly=true").await;
    listed.assert_status_ok();
    assert_eq!(listed.json::<serde_json::Value>(), json!([]));
    let runs: u64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS UNSIGNED) FROM rev8_runs")
        .fetch_one(entries.pool())
        .await
        .unwrap();
    assert_eq!(runs, 0);

    for directory_scope in [
        json!({"root": "team"}),
        json!({"root": "/team/../team"}),
        json!({"root": "/team", "excludedSubtrees": ["/team-old"]}),
        json!({"root": "/team", "excludedSubtrees": ["/team"]}),
    ] {
        let mut invalid = document.clone();
        invalid["spec"]["target"]["directoryScope"] = directory_scope;
        let response = server.post("/api/policies/preview").json(&invalid).await;
        response.assert_status_bad_request();
        assert!(response.text().contains("spec.target.directoryScope"));
    }

    for invalid_pool in ["nonexistent", "hot"] {
        let mut invalid = document.clone();
        invalid["spec"]["rules"]["entries"][0]["action"]["params"]["targetPool"] = invalid_pool.into();
        let response = server.post("/api/policies/preview").json(&invalid).await;
        response.assert_status_bad_request();
        assert!(response.text().contains("targetPool"));
    }
    let mut invalid = document.clone();
    invalid["spec"]["target"]["filesystem"] = "other-filesystem".into();
    let response = server.post("/api/policies/preview").json(&invalid).await;
    response.assert_status_bad_request();
    assert!(response.text().contains("not configured"));

    let writer = rev8_test_server(state);
    let mut enabled = document.clone();
    enabled["spec"]["enabled"] = true.into();
    enabled["spec"]["rules"]["entries"][0]["action"]["params"]["targetPool"] = "nonexistent".into();
    let response = writer.post("/api/policies").json(&enabled).await;
    response.assert_status_bad_request();
    assert!(response.text().contains("targetPool"));
    let created = writer.post("/api/policies").json(&document).await;
    created.assert_status(axum::http::StatusCode::CREATED);
    let id = created.json::<serde_json::Value>()["id"].as_u64().unwrap();
    let response = server.post(&format!("/api/policies/{id}/preview")).await;
    response.assert_status_ok();
    let mut expected = preview;
    expected["revision"] = 1.into();
    assert_eq!(response.json::<serde_json::Value>(), expected);
    let policy = server.get(&format!("/api/policies/{id}")).await;
    policy.assert_status_ok();
    assert_eq!(policy.json::<serde_json::Value>()["revision"], 1);
    let runs = server.get(&format!("/api/policies/{id}/runs")).await;
    runs.assert_status_ok();
    assert_eq!(runs.json::<serde_json::Value>(), json!([]));

    entries
        .link_lustre_name(
            &filesystem,
            &unavailable,
            LuFid::new(0x200000401, 1, 0),
            b"unavailable",
            rbh_entry_store::store::LustreLinkState::Present(1),
            21,
        )
        .await
        .unwrap();
    entries
        .unlink_lustre_name(
            &filesystem,
            &unavailable,
            LuFid::new(0x200000401, 1, 0),
            b"already-removed",
            rbh_entry_store::store::LustreLinkState::Unavailable,
            22,
        )
        .await
        .unwrap();
    entries
        .link_lustre_name(
            &filesystem,
            &unavailable,
            LuFid::new(0x200000401, 1, 0),
            b"unavailable",
            rbh_entry_store::store::LustreLinkState::Unavailable,
            23,
        )
        .await
        .unwrap();
    let response = server.post(&format!("/api/policies/{id}/preview")).await;
    response.assert_status_ok();
    let recovered = response.json::<serde_json::Value>();
    assert_eq!(recovered["candidateCount"], 2);
    assert_eq!(recovered["estimatedBytes"], 1100);
    assert_eq!(
        recovered["directoryScope"]["deferredByReason"]["incomplete_link_evidence"],
        5
    );

    // Fill a second page while keeping every FID's hard links together.
    let large = 9_007_199_254_740_993_u64;
    for oid in 1000..1513 {
        let size = if oid == 1512 { large } else { 64 };
        write(indexed(oid), EntryKind::File, size, 2, None).await;
        link(indexed(oid), allowed, &format!("batch-{oid}")).await;
        link(indexed(oid), nested, &format!("batch-{oid}")).await;
    }
    let response = server.post("/api/policies/preview").json(&document).await;
    response.assert_status_ok();
    let preview = response.json::<serde_json::Value>();
    let expected = large + 512 * 64 + 1100;
    assert_eq!(preview["candidateCount"], 515);
    assert_eq!(preview["estimatedBytes"], expected);
    assert_eq!(preview["estimatedBytesExact"], expected.to_string());
}
