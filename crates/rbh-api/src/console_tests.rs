use std::{path::PathBuf, time::Duration};

use axum_test::TestServer;
use rbh_entry_store::store::EntryStore;
use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig, FileSystemId};
use rbh_lustre_resources::ResourceObservations;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};

use crate::{PolicyWriters, handlers::scoped_api_tests::state_without_database_connection};

async fn read_event_line(stream: &mut BufReader<TcpStream>, expected: &str) {
    timeout(Duration::from_secs(5), async {
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0, "missing {expected}");
            if line.trim_end() == expected {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected}"));
}

async fn open_resource_events(server: &TestServer) -> BufReader<TcpStream> {
    let address = server.server_address().unwrap();
    let mut socket = TcpStream::connect((address.host_str().unwrap(), address.port().unwrap()))
        .await
        .unwrap();
    socket
        .write_all(b"GET /api/filesystems/events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut stream = BufReader::new(socket);
    read_event_line(&mut stream, "HTTP/1.1 200 OK").await;
    read_event_line(&mut stream, "content-type: text/event-stream").await;
    read_event_line(&mut stream, ": connected").await;
    stream
}

async fn expect_resource_event(stream: &mut BufReader<TcpStream>, kind: &str, filesystem: &str) {
    read_event_line(stream, &format!("event: {kind}")).await;
    let mut data = String::new();
    timeout(Duration::from_secs(5), stream.read_line(&mut data))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(data.trim_end(), format!("data: {filesystem}"));
}

#[tokio::test]
#[ignore = "requires isolated MariaDB"]
async fn console_registration_is_authorized_and_restored_without_mounting() {
    use axum::http::{StatusCode, header::AUTHORIZATION};
    use sha2::{Digest, Sha256};

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let entries = EntryStore::connect(&std::env::var("RBH_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    let mut app = state_without_database_connection();
    app.entry_store = entries.clone();
    app.policy_store = rbh_policy::PolicyStore::new(entries.pool().clone()).with_configured_filesystems([]);
    let observer = uuid::Uuid::new_v4().simple().to_string();
    let resources = || ResourceObservations::new(entries.pool().clone(), observer.clone(), Vec::new());
    let admin = |resources| {
        crate::ResourceAdministration::new(app.clone(), resources, observer.clone(), Some(root.clone())).unwrap()
    };
    let writer = PolicyWriters::from_digest_json(
        &json!({"mount-operator": hex::encode(Sha256::digest("mount-rest-test"))}).to_string(),
    )
    .unwrap();
    let initial = resources();
    let mut server = TestServer::builder()
        .http_transport()
        .build(
            admin(initial.clone())
                .routes(writer.clone())
                .merge(crate::console::router(initial.clone(), directory.path().to_owned())),
        )
        .unwrap();
    let mut events = open_resource_events(&server).await;
    let id = FileSystemId::new(format!("mount-{}", &observer[..12])).unwrap();
    let endpoint = "/api/filesystems/management";
    let request = json!({
        "id": id, "name": "Lustre 测试资源",
        "mount": { "type": "managed", "filesystem": "testfs", "mgsNids": "192.0.2.1@tcp",
          "mountNow": false, "autoMount": true }
    });
    server
        .post(endpoint)
        .json(&request)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server.add_header(AUTHORIZATION, "Bearer mount-rest-test");
    let unsafe_path = json!({
        "id": id, "name": "Lustre 测试资源",
        "mount": { "type": "managed", "filesystem": "testfs", "mgsNids": "192.0.2.1@tcp", "path": "/tmp" }
    });
    server
        .post(endpoint)
        .json(&unsafe_path)
        .await
        .assert_status_bad_request();
    server
        .post(endpoint)
        .json(&request)
        .await
        .assert_status(StatusCode::CREATED);
    assert!(initial.contains(&id));
    assert!(!root.join(id.as_str()).exists());
    expect_resource_event(&mut events, "management", id.as_str()).await;
    expect_resource_event(&mut events, "snapshot", id.as_str()).await;
    let resource = format!("{endpoint}/{id}");
    server
        .post(&resource)
        .json(&json!({"action":"rename","name":"已改名"}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    expect_resource_event(&mut events, "management", id.as_str()).await;
    let view = server.get(endpoint).await.json::<Value>();
    assert_eq!(view["resources"][0]["name"], "已改名");
    server
        .post(&resource)
        .json(&json!({"action":"set_tier","pool":"cold","tier":"cold"}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let mount_path = root.join(id.as_str());
    std::fs::create_dir(&mount_path).unwrap();
    std::fs::write(mount_path.join("occupied"), "mount fixture").unwrap();
    server
        .post(&resource)
        .json(&json!({"action":"mount"}))
        .await
        .assert_status_bad_request();
    let next = resources();
    let restarted = admin(next.clone());
    restarted.restore().await.unwrap();
    assert!(next.contains(&id));
    let mut server = TestServer::new(restarted.routes(writer)).unwrap();
    server.add_header(AUTHORIZATION, "Bearer mount-rest-test");
    let view = server.get(endpoint).await.json::<Value>();
    assert_eq!(view["resources"][0]["name"], "已改名");
    assert_eq!(view["resources"][0]["tiers"]["cold"], "cold");
    assert_eq!(view["resources"][0]["mounted"], false);
    assert_eq!(view["resources"][0]["desiredMounted"], true);
    assert_eq!(view["resources"][0]["lastMountError"], "挂载目录必须为空");
    server
        .post(&resource)
        .json(&json!({"action":"remove"}))
        .await
        .assert_status_bad_request();
    server
        .post(&resource)
        .json(&json!({"action":"unmount"}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let view = server.get(endpoint).await.json::<Value>();
    assert_eq!(view["resources"][0]["id"], id.as_str());
    assert_eq!(view["resources"][0]["desiredMounted"], false);
    assert_eq!(view["resources"][0]["unmountPending"], false);
    assert!(view["resources"][0].get("lastMountError").is_none());
    assert!(next.contains(&id));
    assert_eq!(
        entries.get_filesystem(&id).await.unwrap().unwrap().mount_path,
        mount_path
    );

    sqlx::query(
        "UPDATE console_lustre_resources SET metadata = JSON_SET(metadata, '$.unmountPending', JSON_EXTRACT('true', '$'))
         WHERE observer_id = ? AND filesystem_id = ?",
    )
    .bind(observer.as_bytes())
    .bind(id.as_str())
    .execute(entries.pool())
    .await
    .unwrap();
    let restarted = admin(resources());
    restarted.restore().await.unwrap();
    let server = TestServer::builder()
        .build(restarted.routes(PolicyWriters::default()))
        .unwrap();
    let view = server.get(endpoint).await.json::<Value>();
    assert_eq!(view["resources"][0]["unmountPending"], false);
    assert_eq!(view["resources"][0]["desiredMounted"], false);
    assert_eq!(view["resources"][0]["mounted"], false);
}

#[tokio::test]
async fn console_resource_stream_publishes_mount_updates_without_registrations_and_closes_on_shutdown() {
    let assets = tempfile::tempdir().unwrap();
    let state = state_without_database_connection();
    let resources = ResourceObservations::new(state.entry_store.pool().clone(), "test".to_owned(), Vec::new());
    let server = TestServer::builder()
        .http_transport()
        .build(crate::router_with_console(
            state,
            PolicyWriters::default(),
            resources.clone(),
            assets.path().to_owned(),
        ))
        .unwrap();

    let mut events = open_resource_events(&server).await;
    resources.management_changed(None);
    expect_resource_event(&mut events, "management", "*").await;
    resources.stop();
    timeout(Duration::from_secs(5), events.read_to_end(&mut Vec::new()))
        .await
        .expect("stopping resource observations must close the stream")
        .unwrap();
}

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
        PolicyWriters::default(),
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
    let server = TestServer::builder()
        .http_transport()
        .build(crate::router_with_console(
            state_without_database_connection(),
            PolicyWriters::default(),
            resources,
            temp.path().to_owned(),
        ))
        .unwrap();
    let mut events = open_resource_events(&server).await;

    let target = "/api/filesystems/detail?filesystem=missing";
    let before = server.get(target).await.json::<Value>();
    assert_eq!(before["collectionStatus"], "pending");
    assert!(before["observedAt"].is_null());
    server
        .post("/api/filesystems/refresh?filesystem=missing")
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    expect_resource_event(&mut events, "snapshot", "missing").await;
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
