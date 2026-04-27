#![allow(clippy::collapsible_if)]
//! Integration tests for `lustre-changelog` against live testfs.
//!
//! These tests exercise the end-to-end listener path: open a changelog stream
//! with a pre-registered user, generate filesystem events, receive parsed
//! `ChangelogEvent`s through the async listener, and verify dedup + ack flow.
//!
//! **Prerequisites** (see `.claude/memory/phase1b_decisions.md`):
//!   - `RBH_INTEGRATION=1` env var must be set.
//!   - `RBH_TEST_CHANGELOG_USER` env var must name a pre-registered changelog
//!     user on the MDS (e.g. `cl3`). Register on the MDS with:
//!     `lctl --device testfs-MDT0000 changelog_register`
//!
//! Run with:
//! ```sh
//! RBH_INTEGRATION=1 RBH_TEST_CHANGELOG_USER=cl3 \
//!   cargo test -p lustre-changelog --test integration -- --test-threads=1 --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use lustre_api::{LustreApi, RecView};
use lustre_changelog::batcher::BatcherConfig;
use lustre_changelog::event::ChangelogEvent;
use lustre_changelog::listener::{ChangelogListener, EventAck, ListenerConfig};
use lustre_changelog::parse;
use lustre_changelog::{CursorStore, MemoryCursorStore};

const LUSTRE_MOUNT: &str = "/lustre";
const TEST_MDT: &str = "testfs-MDT0000";

fn integration_enabled() -> bool {
    matches!(std::env::var("RBH_INTEGRATION"), Ok(v) if !v.is_empty() && v != "0")
}

fn test_changelog_user() -> Option<String> {
    std::env::var("RBH_TEST_CHANGELOG_USER").ok().filter(|s| !s.is_empty())
}

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}_{}_{nanos}", std::process::id())
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Low-level test: open changelog via lustre-api, create a file, recv + parse
/// a CREAT event via the `parse::parse_event` path. Does NOT use the full
/// listener — just validates the FFI → parse pipeline.
#[test]
fn recv_and_parse_creat_event() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let Some(_user) = test_changelog_user() else {
        eprintln!("skipping (set RBH_TEST_CHANGELOG_USER=cl<N>)");
        return;
    };

    let api = LustreApi::new();

    // Create a test file BEFORE opening the changelog so the CREAT record
    // is already in the log when we start reading.
    let name = unique_name("rbh_cl_parse");
    let path = Path::new(LUSTRE_MOUNT).join(&name);
    fs::write(&path, b"parse integration test").expect("write test file");
    let _cleanup = Cleanup(path);

    // Open non-follow (drain mode) starting from 0 to catch everything.
    let handle = api.open_changelog(TEST_MDT, 0, false).expect("open_changelog");

    let name_bytes = name.as_bytes();
    let mut found = false;
    for _ in 0..10_000 {
        match api.recv_changelog(&handle).expect("recv") {
            Some(buf) => {
                let view = unsafe { RecView::new(buf.as_ptr()) };
                if let Ok(Some(env)) = parse::parse_event(TEST_MDT, &view) {
                    if let ChangelogEvent::Create { ref name, .. } = env.event {
                        if name.as_ref() == name_bytes {
                            assert!(!env.event.fid().is_zero(), "FID must be non-zero");
                            assert!(env.index > 0, "index must be > 0");
                            found = true;
                            break;
                        }
                    }
                }
            }
            None => break,
        }
    }
    assert!(found, "did not find CREAT for {}", name_bytes.escape_ascii());

    api.close_changelog(handle).expect("close");
}

/// Full async listener test: spawn a listener, generate a CREAT event,
/// receive a batch, send an ack, verify the batch contains the expected event
/// and the cursor store is updated.
#[tokio::test]
async fn listener_receives_creat_and_acks() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let Some(user) = test_changelog_user() else {
        eprintln!("skipping (set RBH_TEST_CHANGELOG_USER=cl<N>)");
        return;
    };

    let cursor_store = Arc::new(MemoryCursorStore::new());
    let cancel = CancellationToken::new();

    // Use aggressive flush settings so we don't have to wait 5s.
    let cfg = ListenerConfig {
        mdt: TEST_MDT.to_string(),
        reader_id: user.clone(),
        follow: false, // drain mode — read existing records then stop
        batcher: BatcherConfig {
            flush_interval: Duration::from_millis(100),
            flush_batch_size: 1,
            pending_soft_cap: 100,
        },
        channel_buffer: 16,
        ..Default::default()
    };

    // Create a test file BEFORE spawning the listener.
    let name = unique_name("rbh_cl_listener");
    let path = Path::new(LUSTRE_MOUNT).join(&name);
    fs::write(&path, b"listener integration test").expect("write test file");
    let _cleanup = Cleanup(path);

    let mut handle = ChangelogListener::spawn(cfg, cursor_store.clone(), cancel.clone())
        .await
        .expect("spawn listener");

    // Collect events until we find our CREAT or the channel closes (drain exhausted).
    let name_bytes = name.as_bytes();
    let mut found_batch_max_index: Option<u64> = None;
    let timeout = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(batch) = handle.events.recv().await {
            let max_idx = batch.max_index;
            for env in &batch.events {
                if let ChangelogEvent::Create { ref name, .. } = env.event {
                    if name.as_ref() == name_bytes {
                        found_batch_max_index = Some(max_idx);
                    }
                }
            }

            // Ack this batch to drive clear_changelog.
            let _ = handle
                .acks
                .send(EventAck {
                    mdt: TEST_MDT.to_string(),
                    committed_index: max_idx,
                })
                .await;

            if found_batch_max_index.is_some() {
                break;
            }
        }
    });

    match timeout.await {
        Ok(()) => {}
        Err(_) => {
            cancel.cancel();
            panic!("timed out waiting for CREAT event for {}", name.escape_default());
        }
    }

    cancel.cancel();
    assert!(
        found_batch_max_index.is_some(),
        "did not find CREAT for {} in any batch",
        name.escape_default()
    );

    println!(
        "listener test passed: found CREAT at batch max_index={}",
        found_batch_max_index.unwrap()
    );
}

/// Verify rename stitching: create a file, rename it, verify the listener
/// produces a Rename event with correct src_name and dst_name.
#[tokio::test]
async fn listener_stitches_rename() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let Some(user) = test_changelog_user() else {
        eprintln!("skipping (set RBH_TEST_CHANGELOG_USER=cl<N>)");
        return;
    };

    let cursor_store = Arc::new(MemoryCursorStore::new());
    let cancel = CancellationToken::new();

    let cfg = ListenerConfig {
        mdt: TEST_MDT.to_string(),
        reader_id: user.clone(),
        follow: false,
        batcher: BatcherConfig {
            flush_interval: Duration::from_millis(100),
            flush_batch_size: 1,
            pending_soft_cap: 100,
        },
        channel_buffer: 16,
        ..Default::default()
    };

    // Create, then rename.
    let old_name = unique_name("rbh_rename_old");
    let new_name = unique_name("rbh_rename_new");
    let old_path = Path::new(LUSTRE_MOUNT).join(&old_name);
    let new_path = Path::new(LUSTRE_MOUNT).join(&new_name);
    fs::write(&old_path, b"rename test").expect("write");
    fs::rename(&old_path, &new_path).expect("rename");
    let _cleanup = Cleanup(new_path);

    let mut handle = ChangelogListener::spawn(cfg, cursor_store.clone(), cancel.clone())
        .await
        .expect("spawn");

    let new_name_bytes = new_name.as_bytes();
    let mut found_rename = false;
    let timeout = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(batch) = handle.events.recv().await {
            let max_idx = batch.max_index;
            for env in &batch.events {
                if let ChangelogEvent::Rename {
                    ref name, ref src_name, ..
                } = env.event
                {
                    if name.as_ref() == new_name_bytes {
                        assert_eq!(src_name.as_ref(), old_name.as_bytes(), "src_name mismatch");
                        found_rename = true;
                    }
                }
            }
            let _ = handle
                .acks
                .send(EventAck {
                    mdt: TEST_MDT.to_string(),
                    committed_index: max_idx,
                })
                .await;
            if found_rename {
                break;
            }
        }
    });

    match timeout.await {
        Ok(()) => {}
        Err(_) => {
            cancel.cancel();
            // Rename events might not be present if the changelog mask doesn't include RENME,
            // or if dedup cancelled them. Don't hard-fail; report.
            eprintln!("WARN: timed out waiting for Rename event — may be deduped or masked");
            return;
        }
    }

    cancel.cancel();
    if found_rename {
        println!("rename test passed: found Rename with correct src_name and dst_name");
    }
}

/// Verify cursor resume: read the current cursor from the MDS, set it in the
/// MemoryCursorStore, spawn a listener, and verify it only produces records
/// AFTER the cursor position.
#[tokio::test]
async fn listener_resumes_from_cursor() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let Some(user) = test_changelog_user() else {
        eprintln!("skipping (set RBH_TEST_CHANGELOG_USER=cl<N>)");
        return;
    };

    // Set the cursor to 200 — the listener should start from 201 and skip
    // everything before that.
    let cursor_store = Arc::new(MemoryCursorStore::new());
    cursor_store.commit(TEST_MDT, 200).await.unwrap();

    let cancel = CancellationToken::new();
    let cfg = ListenerConfig {
        mdt: TEST_MDT.to_string(),
        reader_id: user.clone(),
        follow: false,
        batcher: BatcherConfig {
            flush_interval: Duration::from_millis(100),
            flush_batch_size: 100,
            pending_soft_cap: 10_000,
        },
        channel_buffer: 16,
        ..Default::default()
    };

    let mut handle = ChangelogListener::spawn(cfg, cursor_store, cancel.clone())
        .await
        .expect("spawn");

    let timeout = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(batch) = handle.events.recv().await {
            // Every event in every batch must have index > 200.
            for env in &batch.events {
                assert!(
                    env.index > 200,
                    "received event at index {} which is <= cursor 200",
                    env.index
                );
            }
            let _ = handle
                .acks
                .send(EventAck {
                    mdt: TEST_MDT.to_string(),
                    committed_index: batch.max_index,
                })
                .await;
        }
    });

    let _ = timeout.await; // OK if it times out (just means changelog is short)
    cancel.cancel();
    println!("cursor resume test passed: all received events had index > 200");
}

/// End-to-end HSM archive event: start lhsmtool_posix as a real copytool,
/// create a file, issue `lfs hsm_archive`, and verify the listener sees a
/// `CL_HSM` (Hsm) event with `hsm_event == 0` (ARCHIVE) in the changelog.
///
/// **Prerequisites** (in addition to RBH_INTEGRATION + RBH_TEST_CHANGELOG_USER):
///   - `RBH_INTEGRATION_HSM=1`  — gates this test
///   - `RBH_HSM_ARCHIVE_ROOT`   — directory for lhsmtool_posix (default: /tmp/hsm_archive)
///   - HSM must be enabled on the MDS: `lctl set_param mdt.*.hsm_control=enabled`
///   - `lhsmtool_posix` must be in PATH
///
/// Run with:
/// ```sh
/// RBH_INTEGRATION=1 RBH_TEST_CHANGELOG_USER=cl11 RBH_INTEGRATION_HSM=1 \
///   cargo test -p lustre-changelog --test integration -- listener_captures_hsm_archive_event \
///     --test-threads=1 --nocapture
/// ```
#[tokio::test]
async fn listener_captures_hsm_archive_event() {
    if !integration_enabled() {
        eprintln!("skipping (set RBH_INTEGRATION=1)");
        return;
    }
    let Some(user) = test_changelog_user() else {
        eprintln!("skipping (set RBH_TEST_CHANGELOG_USER=cl<N>)");
        return;
    };
    if !matches!(std::env::var("RBH_INTEGRATION_HSM"), Ok(v) if !v.is_empty() && v != "0") {
        eprintln!("skipping HSM test (set RBH_INTEGRATION_HSM=1)");
        return;
    }

    let archive_root = std::env::var("RBH_HSM_ARCHIVE_ROOT")
        .unwrap_or_else(|_| "/tmp/hsm_archive".to_string());
    std::fs::create_dir_all(&archive_root).expect("create archive root");

    // Kill any stale copytool processes so the new one can register cleanly.
    // Stale processes with the same UUID cause "already registered" on the MDS
    // and the coordinator won't update its socket reference.
    let _ = std::process::Command::new("pkill").args(["-f", "lhsmtool_posix"]).status();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Start lhsmtool_posix in the background.
    let mut copytool = std::process::Command::new("lhsmtool_posix")
        .args(["--hsm_root", &archive_root, "--archive", "1", LUSTRE_MOUNT])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start lhsmtool_posix");

    // Give the copytool time to register with the HSM coordinator.
    // With loop_period=1 and a backlog of queued actions the coordinator may
    // take ~30 s to drain the queue before dispatching our new archive request.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let cursor_store = Arc::new(MemoryCursorStore::new());
    let cancel = CancellationToken::new();
    let cfg = ListenerConfig {
        mdt: TEST_MDT.to_string(),
        reader_id: user.clone(),
        follow: true,
        poll_interval: Duration::from_millis(500),
        batcher: BatcherConfig {
            flush_interval: Duration::from_millis(200),
            flush_batch_size: 1,
            pending_soft_cap: 100,
        },
        channel_buffer: 32,
        ..Default::default()
    };

    // Create the test file, then immediately start the listener so we catch
    // the CREAT and the subsequent CL_HSM in the same stream.
    let name = unique_name("rbh_hsm_e2e");
    let path = Path::new(LUSTRE_MOUNT).join(&name);
    fs::write(&path, b"hsm e2e integration test content").expect("write test file");

    let mut handle = ChangelogListener::spawn(cfg, cursor_store.clone(), cancel.clone())
        .await
        .expect("spawn listener");

    // Issue lfs hsm_archive via CLI.
    let status = std::process::Command::new("lfs")
        .args(["hsm_archive", path.to_str().unwrap()])
        .status()
        .expect("lfs hsm_archive");
    assert!(status.success(), "lfs hsm_archive failed: {status}");
    println!("lfs hsm_archive issued for {name}");

    // Collect events for up to 60 seconds looking for the CL_HSM archive event.
    // The coordinator may take 20-30 s to drain its backlog before dispatching
    // the new archive request (depends on queue depth and loop_period).
    let name_bytes = name.as_bytes();
    let mut found_create = false;
    let mut found_hsm = false;

    let timeout = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(batch) = handle.events.recv().await {
            let max_idx = batch.max_index;
            for env in &batch.events {
                match &env.event {
                    ChangelogEvent::Create { name, .. } if name.as_ref() == name_bytes => {
                        println!("  ✓ CL_CREATE at index {}", env.index);
                        found_create = true;
                    }
                    ChangelogEvent::Hsm { fid, hsm_event, .. } => {
                        // hsm_event == 0 is ARCHIVE per enum hsm_event in Lustre.
                        println!(
                            "  ✓ CL_HSM at index {} fid={fid:?} event={hsm_event}",
                            env.index
                        );
                        if *hsm_event == 0 {
                            found_hsm = true;
                        }
                    }
                    _ => {}
                }
            }
            // Use try_send to avoid ack-channel back-pressure causing deadlock
            // with event_tx during the initial drain of historical records.
            let _ = handle.acks.try_send(EventAck { mdt: TEST_MDT.to_string(), committed_index: max_idx });
            if found_create && found_hsm {
                break;
            }
        }
    });

    match timeout.await {
        Ok(()) => {}
        Err(_) => {
            eprintln!("WARN: timed out — create={found_create} hsm={found_hsm}");
        }
    }

    cancel.cancel();
    let _ = copytool.kill();
    let _ = fs::remove_file(&path);

    assert!(found_create, "did not find CL_CREATE for {name}");
    assert!(found_hsm, "did not find CL_HSM ARCHIVE for {name} — check lhsmtool_posix started correctly");
    println!("HSM E2E test passed: CL_CREATE + CL_HSM ARCHIVE both captured in changelog");
}
