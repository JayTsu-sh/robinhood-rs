//! MDT discovery and changelog user management.
//!
//! MDT enumeration uses `liblustreapi` directly (`llapi_get_obd_count` +
//! `llapi_obd_statfs`) per the phase1a directive to prefer FFI over subprocess
//! when a library function exists.
//!
//! Changelog user registration / listing / deregistration go through `lctl`
//! subprocesses because no `llapi_changelog_register` function exists in
//! liblustreapi. **These three methods only work when invoked on the MDS host**
//! where the MDT kernel device lives. On a Lustre client host, `lctl --device
//! testfs-MDT0000 changelog_register` returns "No device found". This matches
//! robinhood-C's operational model: the operator pre-registers a changelog
//! user on the MDS once, then the daemon (running anywhere) consumes records
//! using the resulting `cl<N>` id passed through config.

use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use core::ffi::{c_char, c_int};

use tracing::debug;

use crate::LustreApi;
use crate::error::{LustreApiError, Result, check_rc};
use crate::sys;

/// Metadata about one registered changelog user (e.g. `cl1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogUser {
    /// Short user id, e.g. `"cl1"`.
    pub id: String,
    /// Current record index this user has consumed up to.
    pub current_index: i64,
}

impl LustreApi {
    /// List active MDT short names (e.g. `["testfs-MDT0000"]`).
    ///
    /// Uses `llapi_get_obd_count` to get the MDT count and `llapi_obd_statfs`
    /// with `type = LL_STATFS_LMV` to read each MDT's UUID, one index at a
    /// time. The returned names have the `_UUID` suffix stripped, matching
    /// what `llapi_changelog_start` expects as the `mdtname` parameter.
    ///
    /// `mount` must be a path inside a Lustre mount (typically the root, e.g.
    /// `/lustre`). The liblustreapi call wants a mutable C string but
    /// documents that it does NOT modify the buffer.
    #[tracing::instrument(name = "lustre.active_mdt_names", skip(self), fields(mount = %mount.display()))]
    pub fn active_mdt_names(&self, mount: &Path) -> Result<Vec<String>> {
        let mut c_mount = CString::new(mount.as_os_str().as_bytes())?.into_bytes_with_nul();

        let mut count: c_int = 0;
        // SAFETY: c_mount is a local NUL-terminated buffer; count is a local
        // out-param. llapi_get_obd_count does not retain the pointer.
        let rc = unsafe {
            sys::llapi_get_obd_count(c_mount.as_mut_ptr() as *mut c_char, &mut count, 1 /* is_mdt */)
        };
        check_rc(rc, "llapi_get_obd_count")?;

        if count < 0 {
            return Err(LustreApiError::IntegerOverflow("llapi_get_obd_count returned negative"));
        }
        let count = count as u32;
        debug!(count, "MDTs reported by llapi_get_obd_count");

        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut stat_buf: MaybeUninit<sys::obd_statfs> = MaybeUninit::uninit();
            let mut uuid_buf: MaybeUninit<sys::obd_uuid> = MaybeUninit::uninit();

            // SAFETY: both out-params live on the stack for the duration of the call.
            // llapi_obd_statfs writes into them on success.
            let rc = unsafe {
                sys::llapi_obd_statfs(
                    c_mount.as_mut_ptr() as *mut c_char,
                    sys::LL_STATFS_LMV,
                    index,
                    stat_buf.as_mut_ptr(),
                    uuid_buf.as_mut_ptr(),
                )
            };
            if rc < 0 {
                // Sparse index space: `llapi_get_obd_count` returns the max slot count
                // (e.g. 64), not the number of registered MDTs. Empty slots return
                // `-ENODEV` ("that slot is gone") or `-EAGAIN` ("that slot is a gap
                // between populated indices"). Robinhood-C handles both the same way
                // for OSTs (common/lustre_tools.c:~735): skip and keep iterating.
                if rc == -libc::ENODEV || rc == -libc::EAGAIN {
                    continue;
                }
                check_rc(rc, "llapi_obd_statfs")?;
            }

            // SAFETY: rc >= 0 means uuid_buf is initialized. The UUID is a
            // NUL-terminated ASCII string in a fixed-size c_char array.
            let uuid = unsafe { uuid_buf.assume_init() };
            let name_bytes: &[c_char] = &uuid.uuid;
            // SAFETY: `name_bytes` points at the start of a NUL-terminated
            // string inside the fixed `[c_char; 40]` array.
            let cstr = unsafe { CStr::from_ptr(name_bytes.as_ptr()) };
            let full = cstr.to_string_lossy();
            let trimmed = full.strip_suffix("_UUID").unwrap_or(&full);
            out.push(trimmed.to_owned());
        }

        debug!(count = out.len(), mdts = ?out, "active MDTs discovered");
        Ok(out)
    }

    /// List all changelog users currently registered on `mdt_name`.
    ///
    /// Invokes `lctl get_param mdd.<mdt>.changelog_users`. **Only works on
    /// the MDS host** where the `mdd` device exists.
    #[tracing::instrument(name = "lustre.changelog_users", skip(self))]
    pub fn changelog_users(&self, mdt_name: &str) -> Result<Vec<ChangelogUser>> {
        let param = format!("mdd.{mdt_name}.changelog_users");
        let output = Command::new("lctl").arg("get_param").arg(&param).output()?;
        if !output.status.success() {
            return Err(LustreApiError::Ffi {
                op: "lctl get_param changelog_users",
                errno: output.status.code().unwrap_or(-1),
                strerror: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut users = Vec::new();
        let mut in_id_section = false;
        for raw in stdout.lines() {
            let line = raw.trim();
            if line.starts_with("ID") && line.contains("index") && line.contains("mask") {
                in_id_section = true;
                continue;
            }
            if !in_id_section || line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            // `cl1-0` → `cl1` (the `-0` suffix is the MDT device index).
            let id = match parts[0].split_once('-') {
                Some((id, _)) => id.to_string(),
                None => parts[0].to_string(),
            };
            // Index may be suffixed with `(idle)`; strip non-digits.
            let idx_str: String = parts[1].chars().take_while(|c| c.is_ascii_digit()).collect();
            // H-8 fix: skip entries that fail to parse instead of using sentinel -1.
            let Ok(current_index) = idx_str.parse::<i64>() else {
                continue;
            };
            users.push(ChangelogUser { id, current_index });
        }
        debug!(mdt = %mdt_name, count = users.len(), "changelog users listed");
        Ok(users)
    }

    /// Register a fresh changelog user on `mdt_name`. Returns the assigned id
    /// (e.g. `"cl1"`).
    ///
    /// **Only works on the MDS host** (the MDT kernel device must exist
    /// locally for `lctl --device` to address it). On a Lustre client, the
    /// operator should pre-register a user on the MDS and pass the resulting
    /// `cl<N>` id to the daemon via config. This matches robinhood-C's
    /// operational model (see chglog_reader_config.c:106).
    ///
    /// Invokes `lctl --device <mdt> changelog_register` and parses the user id
    /// from stdout: `"<fsname>-<mdt>: Registered changelog userid 'cl1'"`.
    #[tracing::instrument(name = "lustre.register_changelog_user", skip(self))]
    pub fn register_changelog_user(&self, mdt_name: &str) -> Result<String> {
        let output = Command::new("lctl")
            .arg("--device")
            .arg(mdt_name)
            .arg("changelog_register")
            .output()?;
        if !output.status.success() {
            return Err(LustreApiError::Ffi {
                op: "lctl changelog_register",
                errno: output.status.code().unwrap_or(-1),
                strerror: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(open) = line.find('\'')
                && let Some(close) = line[open + 1..].find('\'')
            {
                let id = &line[open + 1..open + 1 + close];
                debug!(mdt = %mdt_name, user = %id, "registered changelog user");
                return Ok(id.to_string());
            }
        }
        Err(LustreApiError::Ffi {
            op: "lctl changelog_register (parse)",
            errno: -1,
            strerror: format!("failed to parse user id from output: {}", stdout.trim()),
        })
    }

    /// Deregister a changelog user via `lctl --device <mdt> changelog_deregister
    /// <user_id>`. **Only works on the MDS host.**
    #[tracing::instrument(name = "lustre.deregister_changelog_user", skip(self))]
    pub fn deregister_changelog_user(&self, mdt_name: &str, user_id: &str) -> Result<()> {
        let output = Command::new("lctl")
            .arg("--device")
            .arg(mdt_name)
            .arg("changelog_deregister")
            .arg(user_id)
            .output()?;
        if !output.status.success() {
            return Err(LustreApiError::Ffi {
                op: "lctl changelog_deregister",
                errno: output.status.code().unwrap_or(-1),
                strerror: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        debug!(mdt = %mdt_name, user = %user_id, "deregistered changelog user");
        Ok(())
    }
}
