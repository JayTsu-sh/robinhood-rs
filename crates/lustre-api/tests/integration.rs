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
    let (path, _cleanup) = make_test_file("rbh_hsm", b"hsm test");

    let state = api.hsm_state_get(&path).expect("hsm_state_get");
    println!("{} hsm state: {state:?}", path.display());
    // A brand-new file that HSM has never touched should have either empty
    // flags or HS_EXISTS only.
    assert!(
        state.states.is_empty() || state.states == HsmState::EXISTS,
        "unexpected HSM state on fresh file: {:?}",
        state.states,
    );
}
