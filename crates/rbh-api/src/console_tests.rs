use std::path::PathBuf;

use axum_test::TestServer;
use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig, FileSystemId};
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
    let server = TestServer::new(crate::router_with_console(
        state_without_database_connection(),
        Rev8Writers::default(),
        Vec::new(),
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
async fn resource_inventory_uses_daemon_registration_and_detects_unavailable_mounts() {
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
    let server = TestServer::new(crate::router_with_console(
        state_without_database_connection(),
        Rev8Writers::default(),
        vec![
            configured("missing", BackendKind::Lustre, temp.path().join("no-mount")),
            configured("local", BackendKind::Lustre, temp.path().to_owned()),
            configured("juicefs", BackendKind::JuiceFs, temp.path().to_owned()),
        ],
        temp.path().to_owned(),
    ))
    .unwrap();

    let response = server.get("/api/filesystems").await;
    response.assert_status_ok();
    response.assert_header("cache-control", "no-store");
    let rows = response.json::<Vec<Value>>();
    assert_eq!(rows.len(), 2);
    for (row, id, mount) in [
        (&rows[0], "local", temp.path().to_owned()),
        (&rows[1], "missing", temp.path().join("no-mount")),
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
}
