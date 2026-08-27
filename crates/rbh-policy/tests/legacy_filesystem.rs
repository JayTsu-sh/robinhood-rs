use rbh_entry_store::{EntryStore, FileSystemId};
use rbh_policy::PolicyStore;

fn integration_enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(value) if !value.is_empty() && value != "0")
}

#[tokio::test]
async fn legacy_policy_marker_binds_to_the_configured_lustre_id_idempotently() {
    if !integration_enabled() {
        return;
    }
    let entry_store = EntryStore::connect("mysql://root@localhost/rbh_entries_test")
        .await
        .unwrap();
    let name = format!("legacy-bind-{}", std::process::id());
    sqlx::query("DELETE FROM policies WHERE name = ?")
        .bind(&name)
        .execute(entry_store.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO policies (name, kind, definition, enabled) VALUES (?, 'alert', ?, TRUE)")
        .bind(&name)
        .bind(format!(
            r#"{{"name":"{name}","filesystem":"__legacy_lustre__","kind":"alert","trigger":"1h"}}"#
        ))
        .execute(entry_store.pool())
        .await
        .unwrap();

    let store = PolicyStore::new(entry_store.pool().clone());
    let configured = FileSystemId::new("prod-lustre").unwrap();
    assert!(store.bind_legacy_lustre_filesystem(&configured).await.unwrap() >= 1);
    assert_eq!(store.bind_legacy_lustre_filesystem(&configured).await.unwrap(), 0);
    assert_eq!(
        store.get_by_name(&name).await.unwrap().definition.filesystem,
        configured
    );

    sqlx::query("DELETE FROM policies WHERE name = ?")
        .bind(name)
        .execute(entry_store.pool())
        .await
        .unwrap();
}
