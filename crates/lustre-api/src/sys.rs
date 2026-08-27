//! Raw FFI layer — bindgen output + hand-written `llapi_*` function declarations.
//!
//! Types, constants, and struct layouts come from bindgen (generated at build time from
//! `/usr/include/lustre/*`). Function declarations are hand-written for clarity and
//! stable error messages per phase1a_decisions.
//!
//! Do NOT use this module from outside the crate. The public API lives in
//! `lustre_api::{LustreApi, fid, ...}`.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use core::ffi::{c_char, c_int, c_longlong, c_uint, c_void};

/// Bindgen-generated type / constant bindings. Private.
#[allow(warnings)]
#[allow(clippy::all)]
pub(crate) mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

// ---------------------------------------------------------------------------
// Typed aliases for constants.
//
// bindgen emits CLF_* / CHANGELOG_FLAG_* / HS_* / HUA_* as `u32` or `c_int`.
// The changelog record `cr_flags` field is `u16`, so CLF_* are narrowed here to
// avoid casts at every use site.
// ---------------------------------------------------------------------------

pub(crate) const CLF_VERSION: u16 = bindings::changelog_rec_flags_CLF_VERSION as u16;
pub(crate) const CLF_RENAME: u16 = bindings::changelog_rec_flags_CLF_RENAME as u16;
pub(crate) const CLF_JOBID: u16 = bindings::changelog_rec_flags_CLF_JOBID as u16;
pub(crate) const CLF_EXTRA_FLAGS: u16 = bindings::changelog_rec_flags_CLF_EXTRA_FLAGS as u16;
pub(crate) const CLF_SUPPORTED: u16 = bindings::changelog_rec_flags_CLF_SUPPORTED as u16;

pub(crate) const CLFE_INVALID: u32 = bindings::changelog_rec_extra_flags_CLFE_INVALID;
pub(crate) const CLFE_UIDGID: u32 = bindings::changelog_rec_extra_flags_CLFE_UIDGID;
pub(crate) const CLFE_NID: u32 = bindings::changelog_rec_extra_flags_CLFE_NID;
pub(crate) const CLFE_OPEN: u32 = bindings::changelog_rec_extra_flags_CLFE_OPEN;
pub(crate) const CLFE_XATTR: u32 = bindings::changelog_rec_extra_flags_CLFE_XATTR;
pub(crate) const CLFE_SUPPORTED: u32 = bindings::changelog_rec_extra_flags_CLFE_SUPPORTED;

pub(crate) const CHANGELOG_FLAG_FOLLOW: c_int = bindings::changelog_send_flag_CHANGELOG_FLAG_FOLLOW as c_int;
pub(crate) const CHANGELOG_FLAG_BLOCK: c_int = bindings::changelog_send_flag_CHANGELOG_FLAG_BLOCK as c_int;
pub(crate) const CHANGELOG_FLAG_JOBID: c_int = bindings::changelog_send_flag_CHANGELOG_FLAG_JOBID as c_int;
pub(crate) const CHANGELOG_FLAG_EXTRA_FLAGS: c_int = bindings::changelog_send_flag_CHANGELOG_FLAG_EXTRA_FLAGS as c_int;

pub(crate) const HS_EXISTS: u32 = bindings::hsm_states_HS_EXISTS;
pub(crate) const HS_DIRTY: u32 = bindings::hsm_states_HS_DIRTY;
pub(crate) const HS_RELEASED: u32 = bindings::hsm_states_HS_RELEASED;
pub(crate) const HS_ARCHIVED: u32 = bindings::hsm_states_HS_ARCHIVED;
pub(crate) const HS_NORELEASE: u32 = bindings::hsm_states_HS_NORELEASE;
pub(crate) const HS_NOARCHIVE: u32 = bindings::hsm_states_HS_NOARCHIVE;
pub(crate) const HS_LOST: u32 = bindings::hsm_states_HS_LOST;

pub(crate) const HUA_NONE: u32 = bindings::hsm_user_action_HUA_NONE;
pub(crate) const HUA_ARCHIVE: u32 = bindings::hsm_user_action_HUA_ARCHIVE;
pub(crate) const HUA_RESTORE: u32 = bindings::hsm_user_action_HUA_RESTORE;
pub(crate) const HUA_RELEASE: u32 = bindings::hsm_user_action_HUA_RELEASE;
pub(crate) const HUA_REMOVE: u32 = bindings::hsm_user_action_HUA_REMOVE;
pub(crate) const HUA_CANCEL: u32 = bindings::hsm_user_action_HUA_CANCEL;

// Hardcoded — these are plain `#define` macros in `/usr/include/linux/lustre/lustre_user.h`
// that bindgen's default settings don't expose. Values are ABI-stable since Lustre 2.0
// and used by the MDT enumeration path in `mdt::active_mdt_names`.
pub(crate) const LL_STATFS_LMV: u32 = 1; // lustre_user.h line 723
pub(crate) const LL_STATFS_LOV: u32 = 2; // lustre_user.h line 724
pub(crate) const UUID_MAX: usize = 40; // lustre_user.h line 1388

// ---------------------------------------------------------------------------
// Re-exports of bindgen-generated structs used in this Stage A subset.
//
// Additional struct re-exports (changelog_ext_* extension types, hsm_extent,
// hsm_request, hsm_user_item) come back in Stage B / D when `rec.rs` and
// `hsm.rs` start using them. Keeping the surface minimal until then avoids
// `unused_imports` warnings under `-D warnings`.
// ---------------------------------------------------------------------------

pub(crate) use bindings::{changelog_rec, hsm_user_request, hsm_user_state, lu_fid, obd_statfs, obd_uuid};

// ---------------------------------------------------------------------------
// Hand-written `extern "C"` declarations for liblustreapi functions.
//
// Rust 2024 requires the entire extern block to be marked `unsafe`; each function
// inside is implicitly unsafe to call.
//
// Signatures are verified against /usr/include/lustre/lustreapi.h on the build host
// (Lustre 2.17.0). Any drift against a newer lustreapi will surface as a link error
// at build time.
// ---------------------------------------------------------------------------

unsafe extern "C" {
    // Changelog stream lifecycle --------------------------------------------

    pub(crate) fn llapi_changelog_start(
        priv_ptr: *mut *mut c_void, flags: c_int, mdtname: *const c_char, startrec: c_longlong,
    ) -> c_int;

    pub(crate) fn llapi_changelog_recv(priv_ptr: *mut c_void, rec: *mut *mut changelog_rec) -> c_int;

    pub(crate) fn llapi_changelog_free(rec: *mut *mut changelog_rec) -> c_int;

    pub(crate) fn llapi_changelog_fini(priv_ptr: *mut *mut c_void) -> c_int;

    pub(crate) fn llapi_changelog_clear(mdtname: *const c_char, idstr: *const c_char, endrec: c_longlong) -> c_int;

    pub(crate) fn llapi_changelog_set_xflags(priv_ptr: *mut c_void, extra_flags: c_uint) -> c_int;

    // FID ↔ path ------------------------------------------------------------

    pub(crate) fn llapi_fid2path(
        device: *const c_char, fidstr: *const c_char, buf: *mut c_char, buflen: c_int, recno: *mut c_longlong,
        linkno: *mut c_int,
    ) -> c_int;

    pub(crate) fn llapi_path2fid(path: *const c_char, fid: *mut lu_fid) -> c_int;

    pub(crate) fn llapi_root_path_open(device: *const c_char, outfd: *mut c_int) -> c_int;

    pub(crate) fn llapi_fid2path_at(
        mnt_fd: c_int, fid: *const lu_fid, buf: *mut c_char, buflen: c_int, recno: *mut c_longlong, linkno: *mut c_int,
    ) -> c_int;

    pub(crate) fn llapi_search_mounts(
        pathname: *const c_char, index: c_int, mntdir: *mut c_char, fsname: *mut c_char,
    ) -> c_int;

    // Layout / stripe info --------------------------------------------------

    pub(crate) fn llapi_layout_get_by_path(path: *const c_char, flags: u32) -> *mut c_void;

    pub(crate) fn llapi_layout_stripe_count_get(layout: *const c_void, count: *mut u64) -> c_int;

    pub(crate) fn llapi_layout_stripe_size_get(layout: *const c_void, size: *mut u64) -> c_int;

    pub(crate) fn llapi_layout_ost_index_get(layout: *const c_void, stripe_number: u64, index: *mut u64) -> c_int;

    pub(crate) fn llapi_layout_pool_name_get(layout: *const c_void, buf: *mut c_char, buflen: usize) -> c_int;

    pub(crate) fn llapi_layout_free(layout: *mut c_void);

    // MDT / OBD enumeration -------------------------------------------------

    pub(crate) fn llapi_get_obd_count(mnt: *mut c_char, count: *mut c_int, is_mdt: c_int) -> c_int;

    pub(crate) fn llapi_obd_statfs(
        path: *mut c_char, type_: u32, index: u32, stat_buf: *mut obd_statfs, uuid_buf: *mut obd_uuid,
    ) -> c_int;

    // HSM -------------------------------------------------------------------

    pub(crate) fn llapi_hsm_state_get(path: *const c_char, hus: *mut hsm_user_state) -> c_int;

    pub(crate) fn llapi_hsm_state_set(path: *const c_char, setmask: u64, clearmask: u64, archive_id: u32) -> c_int;

    pub(crate) fn llapi_hsm_request(path: *const c_char, request: *const hsm_user_request) -> c_int;
}
