//! Integration tests for `rbh-entry-store` against local MariaDB.
//!
//! Requires `RBH_INTEGRATION=1` and MariaDB on localhost accessible as root
//! with no password. Uses the `rbh_entries_test` database (dropped and
//! recreated per test run).
//!
//! ```sh
//! RBH_INTEGRATION=1 cargo test -p rbh-entry-store --test integration -- --test-threads=1 --nocapture
//! ```

use bytes::Bytes;
use lustre_api::LuFid;
use lustre_changelog::CursorStore;
use rbh_entry_store::model::{
    BackendCapabilities, BackendKind, BaselineState, EntryKey, EntryKind, EntryRow, FileSystemConfig, FileSystemId,
    ObjectId, ScopedEntryRow, ScopedNamespaceEdge,
};
use rbh_entry_store::store::{EntryStore, MariaDbCursorStore};
use sqlx::MySql;
use sqlx::mysql::MySqlPoolOptions;

const TEST_DB_URL: &str = "mysql://root@localhost/rbh_entries_test";

fn integration_enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(v) if !v.is_empty() && v != "0")
}

/// Reset the test database: drop all tables, let migrations recreate them.
async fn reset_db(pool: &sqlx::Pool<MySql>) {
    // Drop tables in reverse dependency order (policies has no FK deps).
    for table in &[
        "filesystem_baselines",
        "scoped_namespace_edges",
        "scoped_entries",
        "filesystems",
        "stripe_items",
        "names",
        "removed_entries",
        "changelog_cursor",
        "entries",
        "policies",
        "classifiers",
        "_sqlx_migrations",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(pool)
            .await;
    }
}

#[tokio::test]
async fn juicefs_baseline_state_and_hardlink_edges_are_durable_and_idempotent() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");
    let filesystem = FileSystemId::new("juice-baseline").unwrap();
    store
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::JuiceFs,
            mount_path: "/jfs".into(),
            capabilities: BackendCapabilities {
                changelog: true,
                namespace: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    store
        .set_baseline_state(&filesystem, BaselineState::Scanning, None, None)
        .await
        .unwrap();
    let edge = ScopedNamespaceEdge {
        filesystem: filesystem.clone(),
        parent: ObjectId::JuiceFs(1),
        name: Bytes::from_static(b"second-name"),
        object: ObjectId::JuiceFs(42),
    };
    store.upsert_scoped_namespace_edge(&edge).await.unwrap();
    store.upsert_scoped_namespace_edge(&edge).await.unwrap();
    assert_eq!(
        store.list_scoped_namespace_edges(&filesystem).await.unwrap(),
        vec![edge]
    );
    store
        .set_baseline_state(&filesystem, BaselineState::Ready, Some(9001), None)
        .await
        .unwrap();
    let baseline = store.get_baseline(&filesystem).await.unwrap().unwrap();
    assert_eq!(baseline.state, BaselineState::Ready);
    assert_eq!(baseline.last_version, Some(9001));
    assert!(baseline.scan_started_at.is_some());
    assert!(baseline.completed_at.is_some());
}

#[tokio::test]
async fn scoped_identities_isolate_the_same_native_object_id() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let first_id = FileSystemId::new("juicefs-a").unwrap();
    let second_id = FileSystemId::new("juicefs-b").unwrap();
    for id in [first_id.clone(), second_id.clone()] {
        store
            .register_filesystem(&FileSystemConfig {
                id,
                backend: BackendKind::JuiceFs,
                mount_path: "/mnt/juicefs".into(),
                capabilities: BackendCapabilities {
                    changelog: true,
                    namespace: true,
                    ..BackendCapabilities::default()
                },
            })
            .await
            .expect("register filesystem");
    }

    let first_key = EntryKey::new(first_id.clone(), ObjectId::JuiceFs(42));
    let second_key = EntryKey::new(second_id, ObjectId::JuiceFs(42));
    let scoped_entry = |key: EntryKey, name: &'static [u8]| ScopedEntryRow {
        key,
        parent: None,
        name: Bytes::from_static(name),
        kind: EntryKind::File,
        size: 1024,
        blocks: 8,
        uid: 1000,
        gid: 100,
        projid: 0,
        mode: 0o644,
        nlink: 1,
        atime: 1_775_955_820,
        mtime: 1_775_955_820,
        ctime: 1_775_955_820,
        stripe_count: None,
        stripe_size: None,
        pool_name: None,
        sm_status: serde_json::json!({}),
        last_seen: 1_775_955_820,
        depth: 1,
    };
    let first = scoped_entry(first_key.clone(), b"first.txt");
    let second = scoped_entry(second_key.clone(), b"second.txt");
    store
        .upsert_scoped_entry(&first)
        .await
        .expect("upsert first scoped entry");
    store
        .upsert_scoped_entry(&second)
        .await
        .expect("upsert second scoped entry");

    assert_eq!(store.get_scoped_entry(&first_key).await.unwrap(), Some(first));
    assert_eq!(store.get_scoped_entry(&second_key).await.unwrap(), Some(second));
    let config = store
        .get_filesystem(&first_id)
        .await
        .unwrap()
        .expect("registered filesystem");
    assert_eq!(config.backend, BackendKind::JuiceFs);
    assert!(config.capabilities.changelog);

    // The expand migration must leave the legacy Lustre catalog path usable.
    let legacy = make_entry(0x200000401, 0x42, "legacy-lustre.txt");
    store.upsert_entry(&legacy).await.expect("upsert legacy entry");
    assert_eq!(store.get_entry(&legacy.fid).await.unwrap().unwrap().name, legacy.name);
}

#[tokio::test]
async fn lustre_scan_batch_populates_filesystem_scoped_baseline() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");
    let filesystem = FileSystemId::new("lustre-archive").unwrap();
    store
        .register_filesystem(&FileSystemConfig {
            id: filesystem.clone(),
            backend: BackendKind::Lustre,
            mount_path: "/lustre".into(),
            capabilities: BackendCapabilities {
                namespace: true,
                stripe: true,
                ..BackendCapabilities::default()
            },
        })
        .await
        .unwrap();
    let rows = [make_entry(0x200000401, 10, "a"), make_entry(0x200000401, 11, "b")];
    store.upsert_lustre_scan_batch(&filesystem, &rows).await.unwrap();

    for row in rows {
        assert_eq!(store.get_entry(&row.fid).await.unwrap().unwrap().name, row.name);
        let entry = ScopedEntryRow::from_lustre(filesystem.clone(), &row);
        assert_eq!(store.get_scoped_entry(&entry.key).await.unwrap(), Some(entry));
    }
}

fn make_entry(seq: u64, oid: u32, name: &str) -> EntryRow {
    EntryRow {
        fid: LuFid::new(seq, oid, 0),
        parent_fid: Some(LuFid::new(seq, 1, 0)),
        name: Bytes::copy_from_slice(name.as_bytes()),
        kind: EntryKind::File,
        size: 1024,
        blocks: 8,
        uid: 1000,
        gid: 100,
        projid: 0,
        mode: 0o644,
        nlink: 1,
        atime: 1_775_955_820,
        mtime: 1_775_955_820,
        ctime: 1_775_955_820,
        stripe_count: Some(2),
        stripe_size: Some(4_194_304),
        pool_name: None,
        sm_status: serde_json::json!({}),
        last_seen: 1_775_955_820,
        depth: 1,
    }
}

#[tokio::test]
async fn upsert_and_get_roundtrip() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let entry = make_entry(0x200000401, 0x42, "test_file.txt");
    store.upsert_entry(&entry).await.expect("upsert");

    let back = store.get_entry(&entry.fid).await.expect("get").expect("not found");
    assert_eq!(back.fid, entry.fid);
    assert_eq!(back.name.as_ref(), b"test_file.txt");
    assert_eq!(back.size, 1024);
    assert_eq!(back.uid, 1000);
    assert_eq!(back.kind, EntryKind::File);
    assert_eq!(back.stripe_count, Some(2));
    println!("upsert+get roundtrip passed");
}

#[tokio::test]
async fn upsert_batch_and_count() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let entries: Vec<EntryRow> = (0..100u32)
        .map(|i| make_entry(0x200000401, i + 100, &format!("batch_{i}.dat")))
        .collect();

    store.upsert_batch(&entries).await.expect("upsert_batch");
    let count = store.entry_count().await.expect("count");
    assert_eq!(count, 100, "expected 100 entries after batch upsert");
    println!("batch upsert: 100 entries inserted");
}

#[tokio::test]
async fn remove_entry_moves_to_removed_entries() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let entry = make_entry(0x200000401, 0x99, "doomed.txt");
    store.upsert_entry(&entry).await.expect("upsert");

    let rm_time = 1_775_960_000i64;
    store.remove_entry(&entry.fid, rm_time).await.expect("remove");

    // Should be gone from entries.
    assert!(
        store.get_entry(&entry.fid).await.expect("get").is_none(),
        "entry should be gone"
    );

    // Should be in removed_entries.
    let row = sqlx::query("SELECT rm_time FROM removed_entries WHERE fid = ?")
        .bind(rbh_entry_store::fid_codec::encode(&entry.fid).as_slice())
        .fetch_one(store.pool())
        .await
        .expect("query removed_entries");
    let stored_rm: i64 = sqlx::Row::try_get(&row, "rm_time").unwrap();
    assert_eq!(stored_rm, rm_time);
    println!("remove_entry: moved to removed_entries with rm_time={rm_time}");
}

#[tokio::test]
async fn rename_entry_atomically_replaces_destination() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");
    let parent = LuFid::new(0x200000401, 1, 0);
    let mut source = make_entry(0x200000401, 2, "source");
    source.parent_fid = Some(parent);
    let mut destination = make_entry(0x200000401, 3, "destination");
    destination.parent_fid = Some(parent);
    store.upsert_entry(&source).await.unwrap();
    store.upsert_entry(&destination).await.unwrap();

    source.name = destination.name.clone();
    store.rename_entry(&source, 1_775_960_000).await.unwrap();

    assert_eq!(
        store.get_entry(&source.fid).await.unwrap().unwrap().name,
        destination.name
    );
    assert!(store.get_entry(&destination.fid).await.unwrap().is_none());
    assert!(store.get_removed(&destination.fid).await.unwrap().is_some());
}

#[tokio::test]
async fn cursor_store_mariadb_roundtrip() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    // Run migrations so the changelog_cursor table exists.
    let _store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let cursor = MariaDbCursorStore::new(pool);

    // Initially empty.
    assert_eq!(cursor.get("testfs-MDT0000").await.unwrap(), None);

    // Commit and read back.
    cursor.commit("testfs-MDT0000", 42).await.unwrap();
    assert_eq!(cursor.get("testfs-MDT0000").await.unwrap(), Some(42));

    // Monotonic advance.
    cursor.commit("testfs-MDT0000", 100).await.unwrap();
    assert_eq!(cursor.get("testfs-MDT0000").await.unwrap(), Some(100));

    // Backwards commit is silently capped by GREATEST().
    cursor.commit("testfs-MDT0000", 50).await.unwrap();
    assert_eq!(
        cursor.get("testfs-MDT0000").await.unwrap(),
        Some(100),
        "should stay at 100"
    );

    println!("MariaDB cursor store roundtrip passed");
}

#[tokio::test]
async fn upsert_is_idempotent() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let mut entry = make_entry(0x200000401, 0x55, "idempotent.txt");
    store.upsert_entry(&entry).await.expect("first upsert");

    // Update size and upsert again — should overwrite, not duplicate.
    entry.size = 2048;
    entry.last_seen = 1_775_960_000;
    store.upsert_entry(&entry).await.expect("second upsert");

    let count = store.entry_count().await.expect("count");
    assert_eq!(count, 1, "should be exactly 1 entry after idempotent upsert");

    let back = store.get_entry(&entry.fid).await.expect("get").unwrap();
    assert_eq!(back.size, 2048, "size should be updated");
    assert_eq!(back.last_seen, 1_775_960_000, "last_seen should be updated");
    println!("idempotent upsert passed");
}

#[tokio::test]
async fn sweep_orphans_moves_stale_files() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let fresh_ts = 1_900_000_000; // very future
    let stale_ts = 1_700_000_000; // well before fresh

    // Two fresh files, two stale files, one stale directory (must be spared).
    let mut a = make_entry(0x301, 0x01, "fresh1.txt");
    a.last_seen = fresh_ts;
    let mut b = make_entry(0x301, 0x02, "fresh2.txt");
    b.last_seen = fresh_ts;
    let mut c = make_entry(0x301, 0x03, "stale1.txt");
    c.last_seen = stale_ts;
    let mut d = make_entry(0x301, 0x04, "stale2.txt");
    d.last_seen = stale_ts;
    let mut dir = make_entry(0x301, 0x05, "staledir");
    dir.kind = EntryKind::Directory;
    dir.last_seen = stale_ts;

    store.upsert_batch(&[a, b, c, d, dir]).await.expect("upsert");
    assert_eq!(store.entry_count().await.unwrap(), 5);

    // Dry-run first: should report 2 candidates (two stale files; dir spared).
    let dry = store.sweep_orphans(fresh_ts - 1_000, 100, true).await.expect("dry run");
    assert_eq!(dry, 2, "dry run should count exactly 2 stale files");
    assert_eq!(store.entry_count().await.unwrap(), 5, "dry run must not delete");

    // Real sweep: both stale files move into removed_entries.
    let swept = store.sweep_orphans(fresh_ts - 1_000, 100, false).await.expect("sweep");
    assert_eq!(swept, 2);
    assert_eq!(store.entry_count().await.unwrap(), 3, "dir + 2 fresh remain");

    let rm = store.list_removed(None, 100, 0).await.expect("list_removed");
    assert!(rm.len() >= 2, "removed_entries has the 2 stale files");
    println!("sweep_orphans passed");
}

#[tokio::test]
async fn subtree_totals_walks_parent_edge() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    // Build a tiny tree:
    //   root (dir)
    //    ├─ a.txt (1000)
    //    ├─ sub (dir)
    //    │   ├─ b.txt (2000)
    //    │   └─ c.txt (3000)
    let root_fid = LuFid::new(0x401, 0x01, 0);
    let sub_fid = LuFid::new(0x401, 0x02, 0);
    let a_fid = LuFid::new(0x401, 0x03, 0);
    let b_fid = LuFid::new(0x401, 0x04, 0);
    let c_fid = LuFid::new(0x401, 0x05, 0);

    let mk = |fid: LuFid, parent: Option<LuFid>, name: &str, size: u64, kind: EntryKind| {
        let mut e = make_entry(0x401, 0, name);
        e.fid = fid;
        e.parent_fid = parent;
        e.kind = kind;
        e.size = size;
        e
    };
    let root = mk(root_fid, None, "root", 0, EntryKind::Directory);
    let sub = mk(sub_fid, Some(root_fid), "sub", 0, EntryKind::Directory);
    let a = mk(a_fid, Some(root_fid), "a.txt", 1000, EntryKind::File);
    let b = mk(b_fid, Some(sub_fid), "b.txt", 2000, EntryKind::File);
    let c = mk(c_fid, Some(sub_fid), "c.txt", 3000, EntryKind::File);

    store.upsert_batch(&[root, sub, a, b, c]).await.expect("upsert");

    // Under root: 5 entries, 6000 bytes total.
    let (n, bytes) = store.subtree_totals(&root_fid).await.expect("totals root");
    assert_eq!(n, 5);
    assert_eq!(bytes, 6000);

    // Under sub only: 3 entries (sub + b + c), 5000 bytes.
    let (n, bytes) = store.subtree_totals(&sub_fid).await.expect("totals sub");
    assert_eq!(n, 3);
    assert_eq!(bytes, 5000);

    // A missing FID returns zero.
    let ghost = LuFid::new(0xdead, 0xbeef, 0);
    let (n, bytes) = store.subtree_totals(&ghost).await.expect("totals missing");
    assert_eq!(n, 0);
    assert_eq!(bytes, 0);

    println!("subtree_totals passed");
}

#[tokio::test]
async fn depth_field_roundtrips() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(TEST_DB_URL)
        .await
        .expect("connect");
    reset_db(&pool).await;
    let store = EntryStore::connect(TEST_DB_URL).await.expect("store connect");

    let mut entry = make_entry(0x200000501, 0x01, "deep_file.txt");
    entry.depth = 7;
    store.upsert_entry(&entry).await.expect("upsert");

    let back = store.get_entry(&entry.fid).await.expect("get").expect("not found");
    assert_eq!(back.depth, 7, "depth should round-trip through DB");
    println!("depth field roundtrip passed (depth={})", back.depth);
}
