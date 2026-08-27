use std::fs;
use std::os::unix::fs::MetadataExt;

use bytes::Bytes;
use rbh_entry_store::model::ScopedNamespaceEdge;
use rbh_entry_store::store::EntryStore;
use rbh_entry_store::{
    BackendCapabilities, BackendKind, EntryKey, EntryKind, FileSystemConfig, FileSystemId, ObjectId, ScopedEntryRow,
};
use rbh_namespace::{NamespaceAdapter, NamespaceError, NamespaceTarget};
use sqlx::mysql::MySqlPoolOptions;

const DB: &str = "mysql://root@127.0.0.1/rbh_namespace_test";

fn enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(value) if value != "0" && !value.is_empty())
}

fn row(
    key: EntryKey, parent: Option<EntryKey>, name: &[u8], kind: EntryKind, metadata: &fs::Metadata,
) -> ScopedEntryRow {
    let depth = if parent.is_some() { 1 } else { 0 };
    ScopedEntryRow {
        key,
        parent,
        name: Bytes::copy_from_slice(name),
        kind,
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
        depth,
    }
}

#[tokio::test]
async fn juicefs_adapter_defines_hardlink_rename_delete_and_ownership_behavior() {
    if !enabled() {
        return;
    }
    let admin = MySqlPoolOptions::new()
        .connect("mysql://root@127.0.0.1/mysql")
        .await
        .unwrap();
    sqlx::query("DROP DATABASE IF EXISTS rbh_namespace_test")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE rbh_namespace_test")
        .execute(&admin)
        .await
        .unwrap();
    let store = EntryStore::connect(DB).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mount = temp.path().join("jfs");
    fs::create_dir(&mount).unwrap();
    let first_path = mount.join("first");
    let hard_path = mount.join("hard");
    fs::write(&first_path, b"namespace").unwrap();
    fs::hard_link(&first_path, &hard_path).unwrap();

    let filesystem = FileSystemId::new("juice-a").unwrap();
    let other = FileSystemId::new("juice-b").unwrap();
    for id in [&filesystem, &other] {
        store
            .register_filesystem(&FileSystemConfig {
                id: id.clone(),
                backend: BackendKind::JuiceFs,
                mount_path: mount.clone(),
                capabilities: BackendCapabilities {
                    namespace: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
    }
    let root_key = EntryKey::new(
        filesystem.clone(),
        ObjectId::JuiceFs(fs::metadata(&mount).unwrap().ino()),
    );
    let file_key = EntryKey::new(
        filesystem.clone(),
        ObjectId::JuiceFs(fs::metadata(&first_path).unwrap().ino()),
    );
    store
        .upsert_scoped_entry(&row(
            root_key.clone(),
            None,
            b"",
            EntryKind::Directory,
            &fs::metadata(&mount).unwrap(),
        ))
        .await
        .unwrap();
    store
        .upsert_scoped_entry(&row(
            file_key.clone(),
            Some(root_key.clone()),
            b"first",
            EntryKind::File,
            &fs::metadata(&first_path).unwrap(),
        ))
        .await
        .unwrap();
    for name in [b"first".as_slice(), b"hard".as_slice()] {
        store
            .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
                filesystem: filesystem.clone(),
                parent: *root_key.object(),
                name: Bytes::copy_from_slice(name),
                object: *file_key.object(),
            })
            .await
            .unwrap();
    }

    let adapter = NamespaceAdapter::new(store.clone(), filesystem.clone()).await.unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("foreign"), b"foreign").unwrap();
    std::os::unix::fs::symlink(&outside, mount.join("escape")).unwrap();
    assert!(matches!(
        adapter
            .resolve(NamespaceTarget::Path(mount.join("escape/foreign")))
            .await,
        Err(NamespaceError::OutsideFilesystem { .. })
    ));
    let foreign_metadata = fs::metadata(outside.join("foreign")).unwrap();
    let foreign_key = EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(foreign_metadata.ino()));
    store
        .upsert_scoped_entry(&row(
            foreign_key.clone(),
            Some(root_key.clone()),
            b"escape/foreign",
            EntryKind::File,
            &foreign_metadata,
        ))
        .await
        .unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root_key.object(),
            name: Bytes::from_static(b"escape/foreign"),
            object: *foreign_key.object(),
        })
        .await
        .unwrap();
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(foreign_key)).await,
        Err(NamespaceError::OutsideFilesystem { .. })
    ));
    let resolved = adapter
        .resolve(NamespaceTarget::Object(file_key.clone()))
        .await
        .unwrap();
    assert!(resolved.path == first_path || resolved.path == hard_path);
    assert_eq!(resolved.stat.inode, fs::metadata(&first_path).unwrap().ino());
    assert_eq!(
        adapter
            .resolve(NamespaceTarget::Path(hard_path.clone()))
            .await
            .unwrap()
            .key,
        file_key
    );

    let wrong = EntryKey::new(other, *file_key.object());
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(wrong)).await,
        Err(NamespaceError::WrongFilesystem { .. })
    ));

    let orphan_key = EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(u64::MAX - 1));
    store
        .upsert_scoped_entry(&ScopedEntryRow {
            key: orphan_key.clone(),
            parent: Some(EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(u64::MAX))),
            name: Bytes::from_static(b"orphan"),
            ..row(
                file_key.clone(),
                Some(root_key.clone()),
                b"orphan",
                EntryKind::File,
                &fs::metadata(&first_path).unwrap(),
            )
        })
        .await
        .unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: ObjectId::JuiceFs(u64::MAX),
            name: Bytes::from_static(b"orphan"),
            object: *orphan_key.object(),
        })
        .await
        .unwrap();
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(orphan_key)).await,
        Err(NamespaceError::MissingParent(_))
    ));

    fs::remove_file(&first_path).unwrap();
    assert_eq!(
        adapter
            .resolve(NamespaceTarget::Object(file_key.clone()))
            .await
            .unwrap()
            .path,
        hard_path
    );
    fs::rename(&hard_path, mount.join("renamed")).unwrap();
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(file_key.clone())).await,
        Err(NamespaceError::StalePath(_))
    ));
    store
        .remove_scoped_namespace_edge(&filesystem, *root_key.object(), b"hard")
        .await
        .unwrap();
    store
        .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: *root_key.object(),
            name: Bytes::from_static(b"renamed"),
            object: *file_key.object(),
        })
        .await
        .unwrap();
    assert_eq!(
        adapter
            .resolve(NamespaceTarget::Object(file_key.clone()))
            .await
            .unwrap()
            .path,
        mount.join("renamed")
    );
    fs::remove_file(mount.join("renamed")).unwrap();
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(file_key)).await,
        Err(NamespaceError::StalePath(_))
    ));
}
