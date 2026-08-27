use std::fs;
use std::os::unix::fs::MetadataExt;

use bytes::Bytes;
use rbh_actions::{ActionBackend, BackendAction, BackendActionOutcome};
use rbh_entry_store::model::ScopedNamespaceEdge;
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, EntryStore, FileSystemConfig, FileSystemId, ObjectId,
    ScopedEntryRow,
};

const DB: &str = "mysql://root@127.0.0.1/rbh_actions_test";

fn enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(value) if !value.is_empty() && value != "0")
}

fn row(key: EntryKey, parent: Option<EntryKey>, name: &[u8], metadata: &fs::Metadata) -> ScopedEntryRow {
    ScopedEntryRow {
        key,
        parent,
        name: Bytes::copy_from_slice(name),
        kind: if metadata.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        size: metadata.len(),
        blocks: metadata.blocks(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        projid: 0,
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        atime: metadata.atime(),
        mtime: metadata.mtime(),
        ctime: metadata.ctime(),
        stripe_count: None,
        stripe_size: None,
        stripe_items: Vec::new(),
        pool_name: None,
        sm_status: serde_json::json!({}),
        last_seen: metadata.ctime(),
        depth: 1,
    }
}

#[tokio::test]
async fn juicefs_purge_uses_native_inode_namespace_and_persists_removal() {
    if !enabled() {
        return;
    }
    let admin = sqlx::MySqlPool::connect("mysql://root@127.0.0.1/mysql").await.unwrap();
    sqlx::query("DROP DATABASE IF EXISTS rbh_actions_test")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE rbh_actions_test")
        .execute(&admin)
        .await
        .unwrap();
    let store = EntryStore::connect(DB).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mount = temp.path().join("jfs");
    fs::create_dir(&mount).unwrap();
    let path = mount.join("victim");
    fs::write(&path, b"purge me").unwrap();
    let filesystem = FileSystemId::new("juice-purge").unwrap();
    store
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::JuiceFs,
            mount_path: mount.clone(),
            capabilities: BackendCapabilities {
                namespace: true,
                purge: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let root = EntryKey::new(
        filesystem.clone(),
        ObjectId::JuiceFs(fs::metadata(&mount).unwrap().ino()),
    );
    let key = EntryKey::new(
        filesystem.clone(),
        ObjectId::JuiceFs(fs::metadata(&path).unwrap().ino()),
    );
    let root_row = row(root.clone(), None, b"", &fs::metadata(&mount).unwrap());
    let entry = row(
        key.clone(),
        Some(root.clone()),
        b"victim",
        &fs::metadata(&path).unwrap(),
    );
    store.upsert_scoped_entry(&root_row).await.unwrap();
    store.upsert_scoped_entry(&entry).await.unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root.object(),
            name: Bytes::from_static(b"victim"),
            object: *key.object(),
        })
        .await
        .unwrap();

    let backend = ActionBackend::new(store.clone(), filesystem.clone()).await.unwrap();
    assert_eq!(backend.purge(&entry).await.unwrap(), BackendActionOutcome::Success);
    assert!(!path.exists());
    assert!(store.get_scoped_entry(&key).await.unwrap().is_none());
    let removed = store.list_scoped_removed(key.filesystem(), None, 10, 0).await.unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].entry.sm_status["action"]["state"], "success");

    let stale_key = EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(u64::MAX - 10));
    let stale = ScopedEntryRow {
        key: stale_key.clone(),
        parent: Some(root.clone()),
        name: Bytes::from_static(b"already-gone"),
        ..entry.clone()
    };
    store.upsert_scoped_entry(&stale).await.unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root.object(),
            name: stale.name.clone(),
            object: *stale_key.object(),
        })
        .await
        .unwrap();
    assert_eq!(
        backend.purge(&stale).await.unwrap(),
        BackendActionOutcome::AlreadyMissing
    );
    assert!(store.get_scoped_entry(&stale_key).await.unwrap().is_none());
    assert!(
        store
            .list_scoped_removed(stale_key.filesystem(), None, 10, 0)
            .await
            .unwrap()
            .iter()
            .any(|removed| removed.entry.key == stale_key
                && removed.entry.sm_status["action"]["state"] == "already_missing")
    );

    let raced_path = mount.join("raced");
    fs::write(&raced_path, b"original").unwrap();
    let raced_key = EntryKey::new(
        filesystem.clone(),
        ObjectId::JuiceFs(fs::metadata(&raced_path).unwrap().ino()),
    );
    let raced = row(
        raced_key.clone(),
        Some(root.clone()),
        b"raced",
        &fs::metadata(&raced_path).unwrap(),
    );
    store.upsert_scoped_entry(&raced).await.unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root.object(),
            name: raced.name.clone(),
            object: *raced_key.object(),
        })
        .await
        .unwrap();
    fs::remove_file(&raced_path).unwrap();
    fs::write(&raced_path, b"replacement must survive").unwrap();
    assert_eq!(
        backend.purge(&raced).await.unwrap(),
        BackendActionOutcome::AlreadyMissing
    );
    assert_eq!(fs::read(&raced_path).unwrap(), b"replacement must survive");

    let busy_path = mount.join("busy");
    fs::create_dir(&busy_path).unwrap();
    fs::write(busy_path.join("child"), b"still here").unwrap();
    let busy_key = EntryKey::new(
        filesystem.clone(),
        ObjectId::JuiceFs(fs::metadata(&busy_path).unwrap().ino()),
    );
    let busy = row(
        busy_key.clone(),
        Some(root.clone()),
        b"busy",
        &fs::metadata(&busy_path).unwrap(),
    );
    store.upsert_scoped_entry(&busy).await.unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root.object(),
            name: busy.name.clone(),
            object: *busy_key.object(),
        })
        .await
        .unwrap();
    assert!(matches!(
        backend.purge(&busy).await.unwrap(),
        BackendActionOutcome::Failed { retryable: false, .. }
    ));
    let persisted = store.get_scoped_entry(&busy_key).await.unwrap().unwrap();
    assert_eq!(persisted.sm_status["action"]["kind"], "purge");
    assert_eq!(persisted.sm_status["action"]["retryable"], false);
}
