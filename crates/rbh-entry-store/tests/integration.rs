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
use rbh_entry_store::model::{EntryKind, EntryRow};
use rbh_entry_store::store::{EntryStore, MariaDbCursorStore};
use sqlx::MySql;
use sqlx::mysql::MySqlPoolOptions;

const TEST_DB_URL: &str = "mysql://root@localhost/rbh_entries_test";

fn integration_enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(v) if !v.is_empty() && v != "0")
}

/// Reset the test database: drop all tables, let migrations recreate them.
async fn reset_db(pool: &sqlx::Pool<MySql>) {
    // Drop tables in reverse dependency order.
    for table in &[
        "stripe_items",
        "names",
        "removed_entries",
        "changelog_cursor",
        "entries",
        "_sqlx_migrations",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(pool)
            .await;
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
