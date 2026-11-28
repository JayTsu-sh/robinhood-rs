//! Integration tests for `lustre-api` against the real testfs at `/lustre`.
//!
//! Gated behind `RBH_INTEGRATION=1` so CI without a Lustre mount still passes:
//!
//! ```sh
//! RBH_INTEGRATION=1 cargo test -p lustre-api --test integration -- \
//!     --test-threads=1 --nocapture
//! ```
//!
//! ## Scope of Phase 1a integration tests
//!
//! These tests exercise the FFI paths that work from a **Lustre client**:
//! MDT enumeration (`llapi_get_obd_count` + `llapi_obd_statfs`), FID/path
//! round trip, stripe info, and HSM state get.
//!
//! The changelog user management functions (`register_changelog_user`,
//! `deregister_changelog_user`, `changelog_users`) wrap `lctl --device
//! testfs-MDT0000 ...` subprocesses which only work on the **MDS host** —
//! the MDT kernel device must exist locally for `lctl` to address it. Phase
//! 1b's end-to-end listener test will exercise those paths with a
//! pre-registered user id passed via env var.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lustre_api::{HsmState, LustreApi};

const LUSTRE_MOUNT: &str = "/lustre";
const EXPECTED_MDT: &str = "testfs-MDT0000";

/// Returns `true` iff `RBH_INTEGRATION` is set to a non-empty non-"0" value.
fn integration_enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(v) if !v.is_empty() && v != "0")
}

/// Generate a filename unique across processes and test runs.
fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}_{}_{nanos}", std::process::id())
}

/// RAII: delete a file on drop. Ignores errors.
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Helper: create a test file on /lustre; return its path and a `Cleanup` guard.
fn make_test_file(prefix: &str, body: &[u8]) -> (PathBuf, Cleanup) {
    let name = unique_name(prefix);
    let path = Path::new(LUSTRE_MOUNT).join(&name);
    fs::write(&path, body).expect("write test file");
    let cleanup = Cleanup(path.clone());
    (path, cleanup)
}

#[test]
fn active_mdt_names_returns_testfs_mdt0000() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let api = LustreApi::new();
    let mdts = api.active_mdt_names(Path::new(LUSTRE_MOUNT)).expect("active_mdt_names");
    println!("active MDTs: {mdts:?}");
    assert!(
        mdts.iter().any(|m| m == EXPECTED_MDT),
        "expected {EXPECTED_MDT} in {mdts:?}",
    );
}

#[test]
fn path_to_fid_and_back_roundtrip() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let api = LustreApi::new();
    let (path, _cleanup) = make_test_file("rbh_pt_fid", b"lustre-api integration");

    let fid = api.path_to_fid(&path).expect("path_to_fid");
    assert!(!fid.is_zero(), "FID must be non-zero");
    println!("created {} with FID {fid}", path.display());

    // Pass the mount point as the "device" — that's the client-compatible form
    // (MDT device names like testfs-MDT0000 only work on the MDS host).
    let back = api
        .fid_to_path(LUSTRE_MOUNT, &fid)
        .expect("fid_to_path via mount point");
    println!("fid_to_path returned: {}", back.display());

    let basename = path.file_name().unwrap().to_string_lossy().into_owned();
    let back_str = back.to_string_lossy();
    assert!(
        back_str.ends_with(&basename),
        "fid_to_path returned {back_str:?}, expected suffix {basename:?}",
    );
}

#[test]
fn stripe_info_on_real_file() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let api = LustreApi::new();

    // Prefer an existing striped file if one is handy.
    let mut candidate: Option<PathBuf> = None;
    for p in ["/lustre/testfile_stripe2", "/lustre/final_a.txt"] {
        let path = PathBuf::from(p);
        if path.exists() {
            candidate = Some(path);
            break;
        }
    }
    let cleanup;
    let path = match candidate {
        Some(p) => {
            cleanup = None;
            p
        }
        None => {
            let (p, c) = make_test_file("rbh_stripe", b"stripe test");
            cleanup = Some(c);
            p
        }
    };

    let info = api.get_stripe_info(&path).expect("get_stripe_info");
    println!("{} stripe: {info:?}", path.display());
    assert!(info.count >= 1, "stripe count >= 1");
    assert!(info.size > 0, "stripe size > 0");

    drop(cleanup);
}

#[test]
fn hsm_state_get_on_plain_file() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let api = LustreApi::new();
    let mount = std::env::var_os("RBH_TEST_LUSTRE_MOUNT").map_or_else(|| PathBuf::from(LUSTRE_MOUNT), PathBuf::from);
    let path = mount.join(unique_name("rbh_hsm"));
    let _cleanup = Cleanup(path.clone());
    fs::write(&path, b"hsm test").expect("write test file");

    let state = api.hsm_state_and_action(&path).expect("authoritative HSM state");
    assert!(!state.has_active_action(), "{state:?}");
    println!("{} hsm state: {state:?}", path.display());
    // A brand-new file that HSM has never touched should have either empty
    // flags or HS_EXISTS only.
    assert!(
        state.states.is_empty() || state.states == HsmState::EXISTS,
        "unexpected HSM state on fresh file: {:?}",
        state.states,
    );
}

#[test]
fn pool_migration_preserves_the_fid_hard_link_and_sparse_data() {
    use lustre_api::migration::{FileVersion, MigrationLimits, MigrationOwnership, MigrationPlacement, MigrationPlan};
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::time::{Duration, Instant};

    let Some(root) = std::env::var_os("RBH_TEST_MIGRATION_HOT_DIR") else {
        return;
    };
    let mount = std::env::var_os("RBH_TEST_LUSTRE_MOUNT").expect("set the Lustre mount");
    let mount = Path::new(&mount);
    let path = Path::new(&root).join(unique_name("rbh_sparse"));
    let file = fs::File::create_new(&path).unwrap();
    let _cleanup = Cleanup(path.clone());
    file.write_all_at(b"hot", 0).unwrap();
    file.write_all_at(b"cold", 2 * 1024 * 1024).unwrap();
    file.set_len(6 * 1024 * 1024).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let link = path.with_extension("link");
    fs::hard_link(&path, &link).unwrap();
    let _cleanup_link = Cleanup(link.clone());
    let bytes = fs::read(&path).unwrap();
    let metadata = fs::metadata(&path).unwrap();

    let api = LustreApi;
    let fid = api.path_to_fid(&path).unwrap();
    let stripe = api.migration_source_layout(mount, &fid).unwrap();
    assert_eq!(stripe.pool.as_deref(), Some("hot"));
    let plan = MigrationPlan {
        source_pool: "hot".into(),
        target_pool: "cold".into(),
        stripe_count: 1,
        stripe_size: 1024 * 1024,
    };
    let generation = FileVersion::from(&metadata);
    assert!(matches!(
        api.migrate_pool(mount, &fid, &plan, generation, Instant::now()),
        Err(lustre_api::migration::MigrationError::Deferred(
            "migration_deadline_elapsed"
        ))
    ));
    assert_eq!(api.migration_source_layout(mount, &fid).unwrap(), stripe);
    let fsname = api.hsm_filesystem(mount).unwrap();
    let limits = MigrationLimits {
        deadline: Instant::now() + Duration::from_secs(15),
        expected_access: None,
        before_swap: || false,
    };
    assert!(matches!(
        api.migrate_reserved_pool(
            mount,
            &fid,
            &plan,
            generation,
            limits,
            MigrationOwnership::acquire(&fsname).unwrap()
        ),
        Err(lustre_api::migration::MigrationError::Deferred(
            "admission_window_closed"
        ))
    ));
    assert_eq!(api.migration_source_layout(mount, &fid).unwrap(), stripe);

    let deadline = Instant::now() + Duration::from_secs(3);
    let entered = std::cell::Cell::new(false);
    let limits = MigrationLimits {
        deadline,
        expected_access: None,
        before_swap: || {
            entered.set(true);
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            true
        },
    };
    assert!(matches!(
        api.migrate_reserved_pool(
            mount,
            &fid,
            &plan,
            generation,
            limits,
            MigrationOwnership::acquire(&fsname).unwrap()
        ),
        Err(lustre_api::migration::MigrationError::Deferred(
            "migration_deadline_elapsed"
        ))
    ));
    assert!(entered.get(), "the final check must have crossed the deadline");
    assert_eq!(api.migration_source_layout(mount, &fid).unwrap(), stripe);

    let target = api
        .migrate_pool(mount, &fid, &plan, generation, Instant::now() + Duration::from_secs(15))
        .unwrap();
    assert_eq!(target.layout.pool_name.as_deref(), Some("cold"));
    assert_eq!(target.layout.stripe_items.len(), 1);
    assert!(
        api.pool_members(mount, "cold")
            .unwrap()
            .contains(&target.layout.stripe_items[0])
    );
    assert!(matches!(
        api.reconcile_pool_migration(mount, &fid, &plan, generation, &fsname)
            .unwrap(),
        MigrationPlacement::Target(_)
    ));
    assert_eq!(api.path_to_fid(&link).unwrap(), fid);
    assert_eq!(api.path_to_fid(&path).unwrap(), fid);
    assert_eq!(fs::read(&link).unwrap(), bytes);
    let after = fs::metadata(&path).unwrap();
    assert_eq!(metadata.ino(), after.ino());
    assert_eq!(after.nlink(), 2);
    assert_eq!(metadata.mtime_nsec(), after.mtime_nsec());
    assert!(after.blocks() * 512 < after.size() / 2);

    let modified = fs::OpenOptions::new().write(true).open(&link).unwrap();
    modified.write_all_at(b"changed after migration", 0).unwrap();
    modified.set_len(metadata.size() + 1).unwrap();
    modified.sync_all().unwrap();
    assert_ne!(FileVersion::from(&modified.metadata().unwrap()), generation);
    assert!(matches!(
        api.reconcile_pool_migration(mount, &fid, &plan, generation, &fsname)
            .unwrap(),
        MigrationPlacement::Target(_)
    ));
}
