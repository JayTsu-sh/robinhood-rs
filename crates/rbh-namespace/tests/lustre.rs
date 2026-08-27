use std::fs;

use rbh_entry_store::store::EntryStore;
use rbh_entry_store::{BackendCapabilities, BackendKind, EntryKey, FileSystemConfig, FileSystemId, ObjectId};
use rbh_namespace::{NamespaceAdapter, NamespaceError, NamespaceTarget};

#[tokio::test]
async fn lustre_adapter_roundtrips_native_fid_and_reports_deleted_path_as_stale() {
    let (Ok(database), Ok(mount)) = (
        std::env::var("RBH_TEST_DATABASE_URL"),
        std::env::var("RBH_TEST_LUSTRE_MOUNT"),
    ) else {
        return;
    };
    let store = EntryStore::connect(&database).await.unwrap();
    let filesystem = FileSystemId::new("lustre-namespace-live").unwrap();
    store
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::Lustre,
            mount_path: mount.clone().into(),
            capabilities: BackendCapabilities {
                namespace: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let adapter = NamespaceAdapter::new(store, filesystem.clone()).await.unwrap();
    let temp = tempfile::Builder::new()
        .prefix("rbh-namespace-")
        .tempdir_in(&mount)
        .unwrap();
    let path = temp.path().join("roundtrip");
    fs::write(&path, b"lustre namespace").unwrap();

    let by_path = adapter.resolve(NamespaceTarget::Path(path.clone())).await.unwrap();
    assert_eq!(by_path.key.filesystem(), &filesystem);
    assert!(matches!(by_path.key.object(), ObjectId::Lustre(_)));
    let key = by_path.key;
    assert_eq!(
        adapter
            .resolve(NamespaceTarget::Object(key.clone()))
            .await
            .unwrap()
            .stat,
        by_path.stat
    );

    fs::remove_file(path).unwrap();
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(key)).await,
        Err(NamespaceError::StalePath(_))
    ));

    let wrong = EntryKey::new(
        FileSystemId::new("other-lustre").unwrap(),
        ObjectId::Lustre(lustre_api::LuFid::new(0, 0, 0)),
    );
    assert!(matches!(
        adapter.resolve(NamespaceTarget::Object(wrong)).await,
        Err(NamespaceError::WrongFilesystem { .. })
    ));
}
