use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use bytes::Bytes;
use rbh_actions::{ActionBackend, BackendAction, BackendActionOutcome};
use rbh_entry_store::model::ScopedNamespaceEdge;
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, EntryStore, FileSystemConfig, FileSystemId, ObjectId,
    ScopedEntryRow,
};

fn enabled() -> bool {
    matches!(std::env::var("RBH_LUSTRE_INTEGRATION"), Ok(value) if !value.is_empty() && value != "0")
}

#[tokio::test]
async fn lustre_purge_uses_the_same_backend_contract_and_native_fid() {
    if !enabled() {
        return;
    }
    let db =
        std::env::var("RBH_ACTIONS_DB").unwrap_or_else(|_| "mysql://root@127.0.0.1/rbh_actions_lustre_test".into());
    let mount = PathBuf::from(std::env::var("RBH_LUSTRE_MOUNT").unwrap_or_else(|_| "/lustre".into()));
    let store = EntryStore::connect(&db).await.unwrap();
    let filesystem = FileSystemId::new("lustre-live").unwrap();
    store
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::Lustre,
            mount_path: mount.clone(),
            capabilities: BackendCapabilities {
                namespace: true,
                purge: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let path = mount.join(format!(".rbh-action-backend-{}", std::process::id()));
    fs::write(&path, b"lustre purge regression").unwrap();
    let lustre = lustre_api::LustreApi;
    let root_fid = lustre.path_to_fid(&mount).unwrap();
    let fid = lustre.path_to_fid(&path).unwrap();
    let root = EntryKey::new(filesystem.clone(), ObjectId::Lustre(root_fid));
    let key = EntryKey::new(filesystem.clone(), ObjectId::Lustre(fid));
    let metadata = fs::metadata(&path).unwrap();
    let entry = ScopedEntryRow {
        key: key.clone(),
        parent: Some(root.clone()),
        name: Bytes::copy_from_slice(path.file_name().unwrap().as_encoded_bytes()),
        kind: EntryKind::File,
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
    };
    let root_meta = fs::metadata(&mount).unwrap();
    store
        .upsert_scoped_entry(&ScopedEntryRow {
            key: root.clone(),
            parent: None,
            name: Bytes::new(),
            kind: EntryKind::Directory,
            size: root_meta.len(),
            blocks: root_meta.blocks(),
            uid: root_meta.uid(),
            gid: root_meta.gid(),
            projid: 0,
            mode: root_meta.mode(),
            nlink: root_meta.nlink() as u32,
            atime: root_meta.atime(),
            mtime: root_meta.mtime(),
            ctime: root_meta.ctime(),
            stripe_count: None,
            stripe_size: None,
            stripe_items: Vec::new(),
            pool_name: None,
            sm_status: serde_json::json!({}),
            last_seen: root_meta.ctime(),
            depth: 0,
        })
        .await
        .unwrap();
    store.upsert_scoped_entry(&entry).await.unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root.object(),
            name: entry.name.clone(),
            object: *key.object(),
        })
        .await
        .unwrap();
    let backend = ActionBackend::new(store.clone(), filesystem).await.unwrap();
    assert_eq!(backend.purge(&entry).await.unwrap(), BackendActionOutcome::Success);
    assert!(!path.exists());
    assert!(store.get_scoped_entry(&key).await.unwrap().is_none());
}
