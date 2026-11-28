use std::path::PathBuf;

use axum_test::TestServer;
use rbh_entry_store::store::EntryStore;
use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig, FileSystemId};
use rbh_lustre_resources::ResourceObservations;
use serde_json::{Value, json};

use crate::{Rev8Writers, handlers::scoped_api_tests::state_without_database_connection};

#[tokio::test]
async fn daemon_serves_the_console_and_assets_beside_the_api() {
    let assets = tempfile::tempdir().unwrap();
    std::fs::write(
        assets.path().join("index.html"),
        r#"<script src="/console/app.js"></script>"#,
    )
    .unwrap();
    std::fs::write(assets.path().join("app.js"), "console.log('ready');").unwrap();
    let state = state_without_database_connection();
    let resources = ResourceObservations::new(state.entry_store.pool().clone(), "test".to_owned(), Vec::new());
    let server = TestServer::new(crate::router_with_console(
        state,
        Rev8Writers::default(),
        resources,
        assets.path().to_owned(),
    ))
    .unwrap();

    let index = server.get("/console/").await;
    index.assert_status_ok();
    index.assert_header("cache-control", "no-cache");
    assert!(index.text().contains("/console/app.js"));
    server.get("/console/app.js").await.assert_text("console.log('ready');");
    server.get("/console/missing.js").await.assert_status_not_found();
    server.get("/api/health").await.assert_json(&json!({"status": "ok"}));
    server.get("/api/filesystems").await.assert_json(&json!([]));
}

#[tokio::test]
#[ignore = "requires a disposable MariaDB database in RBH_TEST_DATABASE_URL"]
async fn resource_inventory_reads_saved_snapshots_and_refreshes_only_the_selected_registration() {
    let temp = tempfile::tempdir().unwrap();
    let configured = |id: &str, backend, mount_path: PathBuf| FileSystemConfig {
        id: FileSystemId::new(id).unwrap(),
        backend,
        mount_path,
        capabilities: BackendCapabilities {
            stripe: true,
            ost: true,
            ..BackendCapabilities::default()
        },
    };
    let database_url = std::env::var("RBH_TEST_DATABASE_URL").unwrap();
    let store = EntryStore::connect(&database_url).await.unwrap();
    let configs = vec![
        configured("missing", BackendKind::Lustre, temp.path().join("no-mount")),
        configured("local", BackendKind::Lustre, temp.path().to_owned()),
        configured("..", BackendKind::Lustre, temp.path().to_owned()),
        configured("juicefs", BackendKind::JuiceFs, temp.path().to_owned()),
    ];
    let other_observer = ResourceObservations::new(store.pool().clone(), "observer-b".to_owned(), configs.clone());
    let restarted = ResourceObservations::new(store.pool().clone(), "observer-a".to_owned(), configs.clone());
    let resources = ResourceObservations::new(store.pool().clone(), "observer-a".to_owned(), configs);
    let server = TestServer::new(crate::router_with_console(
        state_without_database_connection(),
        Rev8Writers::default(),
        resources,
        temp.path().to_owned(),
    ))
    .unwrap();

    let target = "/api/filesystems/detail?filesystem=missing";
    let before = server.get(target).await.json::<Value>();
    assert_eq!(before["collectionStatus"], "pending");
    assert!(before["observedAt"].is_null());
    server
        .post("/api/filesystems/refresh?filesystem=missing")
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    let saved = server.get(target).await.json::<Value>();
    assert_eq!(saved["collectionStatus"], "unavailable");
    let same_filesystem = FileSystemId::new("missing").unwrap();
    let restored = restarted.get(&same_filesystem).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), saved);
    let other = other_observer.get(&same_filesystem).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(other).unwrap()["collectionStatus"], "pending");
    assert_eq!(
        server
            .get("/api/filesystems/detail?filesystem=local")
            .await
            .json::<Value>()["collectionStatus"],
        "pending"
    );
    server
        .post("/api/filesystems/refresh")
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let response = server.get("/api/filesystems").await;
    response.assert_status_ok();
    response.assert_header("cache-control", "no-store");
    let rows = response.json::<Vec<Value>>();
    assert_eq!(rows.len(), 3);
    for (row, id, mount) in [
        (&rows[0], "..", temp.path().to_owned()),
        (&rows[1], "local", temp.path().to_owned()),
        (&rows[2], "missing", temp.path().join("no-mount")),
    ] {
        assert_eq!(row["id"], id);
        assert_eq!(row["mountPath"], mount.to_str().unwrap());
        assert_eq!(row["mountStatus"], "unmounted");
        assert_eq!(row["collectionStatus"], "unavailable");
        assert_eq!(row["pools"], Value::Null);
        assert_eq!(row["tieringAvailable"], false);
        assert!(row["observedAt"].is_number());
        assert!(row["reason"].is_string());
    }

    let detail = server.get("/api/filesystems/detail?filesystem=missing").await;
    detail.assert_status_ok();
    detail.assert_header("cache-control", "no-store");
    let detail = detail.json::<Value>();
    assert_eq!(detail["id"], "missing");
    assert_eq!(detail["mount"], Value::Null);
    assert_eq!(detail["capacity"]["status"], "missing");
    assert!(detail["capacity"]["attemptedAt"].is_u64());
    assert_eq!(detail["capacity"]["sample"], Value::Null);
    server
        .get("/api/filesystems/detail?filesystem=juicefs")
        .await
        .assert_status_not_found();
    assert_eq!(
        server
            .get("/api/filesystems/detail?filesystem=..")
            .await
            .json::<Value>()["id"],
        ".."
    );
}
