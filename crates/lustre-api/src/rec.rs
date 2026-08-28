//! Raw changelog record accessors.
//!
//! `llapi_changelog_recv` returns a `*mut changelog_rec` that points at a
//! packed C struct followed by variable-length extension data. The layout is
//! driven by bits in `cr_flags`:
//!
//! ```text
//! +----------------------------+  <- rec
//! | changelog_rec (header)     |
//! +----------------------------+
//! | changelog_ext_rename       |  (iff CLF_RENAME)
//! +----------------------------+
//! | changelog_ext_jobid        |  (iff CLF_JOBID)
//! +----------------------------+
//! | changelog_ext_extra_flags  |  (iff CLF_EXTRA_FLAGS)
//! +----------------------------+
//! | changelog_ext_uidgid       |  (iff CLFE_UIDGID in cr_extra_flags)
//! +----------------------------+
//! | changelog_ext_nid          |  (iff CLFE_NID)
//! +----------------------------+
//! | changelog_ext_openmode     |  (iff CLFE_OPEN)
//! +----------------------------+
//! | changelog_ext_xattr        |  (iff CLFE_XATTR)
//! +----------------------------+
//! | target name (NUL-term)     |
//! | [source name (NUL-term)]   |  (iff CLF_RENAME and source name present)
//! +----------------------------+
//! ```
//!
//! This module exposes a safe [`RecView`] borrow that hides the pointer math
//! behind typed accessors. Phase 1b (`lustre-changelog`) consumes `RecView` to
//! build its `ChangelogEvent` domain enum.

use core::mem::size_of;
use core::ptr::{addr_of, read_unaligned};
use std::ffi::CStr;

use serde::{Deserialize, Serialize};

use crate::fid::LuFid;
use crate::sys;

/// Typed enum over the `cr_type` field.
///
/// Values match `enum changelog_rec_type` in
/// `/usr/include/linux/lustre/lustre_user.h`. The `Unknown(n)` variant is
/// forward-compatibility insurance against future Lustre versions adding new
/// record types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangelogEventType {
    Mark,
    Create,
    Mkdir,
    Hardlink,
    Softlink,
    Mknod,
    Unlink,
    Rmdir,
    Rename,
    RenameTo,
    Open,
    Close,
    Layout,
    Truncate,
    SetAttr,
    XAttr,
    Hsm,
    MTime,
    CTime,
    ATime,
    Migrate,
    FlrWrite,
    Resync,
    GetXattr,
    NoOpen,
    Unknown(u32),
}

impl ChangelogEventType {
    /// Convert a raw `cr_type` value to the typed enum.
    pub fn from_raw(n: u32) -> Self {
        match n {
            0 => Self::Mark,
            1 => Self::Create,
            2 => Self::Mkdir,
            3 => Self::Hardlink,
            4 => Self::Softlink,
            5 => Self::Mknod,
            6 => Self::Unlink,
            7 => Self::Rmdir,
            8 => Self::Rename,
            9 => Self::RenameTo,
            10 => Self::Open,
            11 => Self::Close,
            12 => Self::Layout,
            13 => Self::Truncate,
            14 => Self::SetAttr,
            15 => Self::XAttr,
            16 => Self::Hsm,
            17 => Self::MTime,
            18 => Self::CTime,
            19 => Self::ATime,
            20 => Self::Migrate,
            21 => Self::FlrWrite,
            22 => Self::Resync,
            23 => Self::GetXattr,
            24 => Self::NoOpen,
            other => Self::Unknown(other),
        }
    }

    /// Short uppercase name matching `lctl`/`lfs` output (`"CREAT"`, `"UNLNK"`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mark => "MARK",
            Self::Create => "CREAT",
            Self::Mkdir => "MKDIR",
            Self::Hardlink => "HLINK",
            Self::Softlink => "SLINK",
            Self::Mknod => "MKNOD",
            Self::Unlink => "UNLNK",
            Self::Rmdir => "RMDIR",
            Self::Rename => "RENME",
            Self::RenameTo => "RNMTO",
            Self::Open => "OPEN",
            Self::Close => "CLOSE",
            Self::Layout => "LYOUT",
            Self::Truncate => "TRUNC",
            Self::SetAttr => "SATTR",
            Self::XAttr => "XATTR",
            Self::Hsm => "HSM",
            Self::MTime => "MTIME",
            Self::CTime => "CTIME",
            Self::ATime => "ATIME",
            Self::Migrate => "MIGRT",
            Self::FlrWrite => "FLRW",
            Self::Resync => "RESYNC",
            Self::GetXattr => "GXATR",
            Self::NoOpen => "NOPEN",
            Self::Unknown(_) => "UNKNOWN",
        }
    }
}

impl From<u32> for ChangelogEventType {
    fn from(n: u32) -> Self {
        Self::from_raw(n)
    }
}

impl core::fmt::Display for ChangelogEventType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unknown(n) => write!(f, "UNKNOWN({n})"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// A safe borrow over a raw `struct changelog_rec *` returned by
/// `llapi_changelog_recv`.
///
/// All accessor methods are safe — they handle packed-struct unaligned reads
/// internally via `core::ptr::read_unaligned`.
///
/// # Lifetime
///
/// The `'a` lifetime parameter is tied to the underlying record buffer: the
/// caller must guarantee that `llapi_changelog_free` is NOT called until every
/// `RecView` borrow is dropped. In practice Phase 1b's listener holds a
/// [`crate::changelog::RecvBuf`] which owns the allocation, then constructs a
/// short-lived `RecView` for field extraction before dropping both.
pub struct RecView<'a> {
    rec: *const sys::changelog_rec,
    _marker: core::marker::PhantomData<&'a sys::changelog_rec>,
}

impl<'a> RecView<'a> {
    /// Wrap a raw record pointer.
    ///
    /// # Safety
    ///
    /// * `rec` must be a valid, non-null pointer returned by `llapi_changelog_recv`.
    /// * The underlying allocation must outlive this `RecView`; do not call
    ///   `llapi_changelog_free` while any view of this record exists.
    #[inline]
    pub unsafe fn new(rec: *const sys::changelog_rec) -> Self {
        debug_assert!(!rec.is_null(), "RecView::new called with null pointer");
        Self {
            rec,
            _marker: core::marker::PhantomData,
        }
    }

    // --- simple header accessors --------------------------------------------

    /// Record index. Monotonically increasing per MDT; used as the cursor position.
    #[inline]
    pub fn index(&self) -> u64 {
        // SAFETY: `self.rec` is a valid pointer per the construction invariant;
        // `cr_index` is a `__u64` at a fixed offset in the packed struct.
        unsafe { read_unaligned(addr_of!((*self.rec).cr_index)) }
    }

    /// Raw packed Lustre timestamp (`seconds << 30 | nanoseconds`).
    ///
    /// Consumers must shift right by 30 bits when they need Unix seconds.
    #[inline]
    pub fn time(&self) -> u64 {
        unsafe { read_unaligned(addr_of!((*self.rec).cr_time)) }
    }

    /// Typed event kind.
    #[inline]
    pub fn event_type(&self) -> ChangelogEventType {
        let raw: u32 = unsafe { read_unaligned(addr_of!((*self.rec).cr_type)) };
        // Low 12 bits identify the event; high bits are reserved for flags.
        ChangelogEventType::from_raw(raw & 0x0fff)
    }

    /// Raw `cr_flags` — includes structural bits (CLF_VERSION etc. in the
    /// upper nibble) and type-specific flags in the lower 12 bits. Public so
    /// `lustre-changelog::parse` can extract per-type flags (e.g.
    /// `CLF_UNLINK_LAST`, HSM event/error bitfields).
    #[inline]
    pub fn flags(&self) -> u16 {
        unsafe { read_unaligned(addr_of!((*self.rec).cr_flags)) }
    }

    /// Internal alias — keeps existing offset-calculation code readable.
    #[inline]
    fn cr_flags(&self) -> u16 {
        self.flags()
    }

    /// Raw `cr_namelen` — total byte length of the trailing name region
    /// (including NUL terminators).
    #[inline]
    fn cr_namelen(&self) -> u16 {
        unsafe { read_unaligned(addr_of!((*self.rec).cr_namelen)) }
    }

    /// Target FID — the file or directory this record is about.
    ///
    /// For `MARK` records this field holds marker flags instead; callers should
    /// check `event_type()` first.
    #[inline]
    pub fn target_fid(&self) -> LuFid {
        // cr_union.cr_tfid is at offset `size_of::<header up to union>`. bindgen
        // represents the union as `__bindgen_anon_1`; we read it as lu_fid since
        // that's the first variant.
        unsafe {
            let p = addr_of!((*self.rec).__bindgen_anon_1) as *const sys::lu_fid;
            LuFid::from_raw(read_unaligned(p))
        }
    }

    /// Parent directory FID.
    #[inline]
    pub fn parent_fid(&self) -> LuFid {
        unsafe {
            let p = addr_of!((*self.rec).cr_pfid);
            LuFid::from_raw(read_unaligned(p))
        }
    }

    // --- extension-offset math ----------------------------------------------

    /// Mirrors Lustre's `changelog_rec_offset` static inline.
    ///
    /// `crf` is a subset of the record's cr_flags (mask with
    /// CLF_RENAME / CLF_JOBID / CLF_EXTRA_FLAGS as needed by the caller).
    /// `cref` is a subset of the extra-flags byte.
    fn rec_offset(crf: u16, cref: u32) -> usize {
        let mut off = size_of::<sys::changelog_rec>();
        if crf & sys::CLF_RENAME != 0 {
            off += size_of::<sys::bindings::changelog_ext_rename>();
        }
        if crf & sys::CLF_JOBID != 0 {
            off += size_of::<sys::bindings::changelog_ext_jobid>();
        }
        if crf & sys::CLF_EXTRA_FLAGS != 0 {
            off += size_of::<sys::bindings::changelog_ext_extra_flags>();
            if cref & sys::CLFE_UIDGID != 0 {
                off += size_of::<sys::bindings::changelog_ext_uidgid>();
            }
            if cref & sys::CLFE_NID != 0 {
                off += size_of::<sys::bindings::changelog_ext_nid>();
            }
            if cref & sys::CLFE_OPEN != 0 {
                off += size_of::<sys::bindings::changelog_ext_openmode>();
            }
            if cref & sys::CLFE_XATTR != 0 {
                off += size_of::<sys::bindings::changelog_ext_xattr>();
            }
        }
        off
    }

    /// Extra-flags byte — returns 0 if `CLF_EXTRA_FLAGS` is unset.
    fn extra_flags(&self) -> u32 {
        let crf = self.cr_flags();
        if crf & sys::CLF_EXTRA_FLAGS == 0 {
            return 0;
        }
        let off = Self::rec_offset(
            crf & (sys::CLF_VERSION | sys::CLF_RENAME | sys::CLF_JOBID),
            sys::CLFE_INVALID,
        );
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const sys::bindings::changelog_ext_extra_flags;
            read_unaligned(addr_of!((*p).cr_extra_flags)) as u32
        }
    }

    // --- optional extension accessors ---------------------------------------

    /// Source FID for a rename record (the file being moved). `None` for
    /// non-rename records.
    pub fn rename_source_fid(&self) -> Option<LuFid> {
        let crf = self.cr_flags();
        if crf & sys::CLF_RENAME == 0 {
            return None;
        }
        let off = Self::rec_offset(crf & sys::CLF_VERSION, sys::CLFE_INVALID);
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const sys::bindings::changelog_ext_rename;
            let raw = read_unaligned(addr_of!((*p).cr_sfid));
            Some(LuFid::from_raw(raw))
        }
    }

    /// Source parent FID for a rename record. `None` for non-renames.
    pub fn rename_source_parent_fid(&self) -> Option<LuFid> {
        let crf = self.cr_flags();
        if crf & sys::CLF_RENAME == 0 {
            return None;
        }
        let off = Self::rec_offset(crf & sys::CLF_VERSION, sys::CLFE_INVALID);
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const sys::bindings::changelog_ext_rename;
            let raw = read_unaligned(addr_of!((*p).cr_spfid));
            Some(LuFid::from_raw(raw))
        }
    }

    /// UID of the process that triggered the event. `None` if `CLFE_UIDGID`
    /// isn't set in the record's extra flags.
    pub fn uid(&self) -> Option<u32> {
        let cref = self.extra_flags();
        if cref & sys::CLFE_UIDGID == 0 {
            return None;
        }
        let crf = self.cr_flags() & (sys::CLF_VERSION | sys::CLF_RENAME | sys::CLF_JOBID | sys::CLF_EXTRA_FLAGS);
        let off = Self::rec_offset(crf, sys::CLFE_INVALID);
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const sys::bindings::changelog_ext_uidgid;
            let raw = read_unaligned(addr_of!((*p).cr_uid));
            Some(raw as u32)
        }
    }

    /// GID — companion to [`uid`](Self::uid).
    pub fn gid(&self) -> Option<u32> {
        let cref = self.extra_flags();
        if cref & sys::CLFE_UIDGID == 0 {
            return None;
        }
        let crf = self.cr_flags() & (sys::CLF_VERSION | sys::CLF_RENAME | sys::CLF_JOBID | sys::CLF_EXTRA_FLAGS);
        let off = Self::rec_offset(crf, sys::CLFE_INVALID);
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const sys::bindings::changelog_ext_uidgid;
            let raw = read_unaligned(addr_of!((*p).cr_gid));
            Some(raw as u32)
        }
    }

    /// Jobid for the process that triggered the event. `None` if `CLF_JOBID`
    /// isn't set. The returned slice is a NUL-terminated C string interpreted
    /// as raw bytes (not guaranteed UTF-8).
    pub fn jobid(&self) -> Option<&'a [u8]> {
        let crf = self.cr_flags();
        if crf & sys::CLF_JOBID == 0 {
            return None;
        }
        let off = Self::rec_offset(crf & (sys::CLF_VERSION | sys::CLF_RENAME), sys::CLFE_INVALID);
        // SAFETY: caller of `RecView::new` promised the record is well-formed
        // with all extension data in-place; the jobid extension is a fixed-size
        // [c_char; 32] right after the rename ext (if any).
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const i8;
            Some(CStr::from_ptr(p).to_bytes())
        }
    }

    // --- name and source-name accessors -------------------------------------

    /// Primary name — null-terminated, raw bytes (NOT UTF-8 guaranteed).
    ///
    /// This is the filename of the target entry as it appears in its parent.
    pub fn name(&self) -> &'a [u8] {
        let crf = self.cr_flags();
        let cref = self.extra_flags();
        let off = Self::rec_offset(crf & sys::CLF_SUPPORTED, cref & sys::CLFE_SUPPORTED);
        // SAFETY: same construction invariant; the name is a NUL-terminated
        // string immediately after all optional extensions.
        unsafe {
            let p = (self.rec as *const u8).add(off) as *const i8;
            CStr::from_ptr(p).to_bytes()
        }
    }

    /// Source name for a rename record. `None` for non-renames or if no source
    /// name is stored.
    ///
    /// The source name is placed immediately after the primary name's NUL
    /// terminator, within the `cr_namelen`-byte trailing region.
    pub fn source_name(&self) -> Option<&'a [u8]> {
        if self.cr_flags() & sys::CLF_RENAME == 0 {
            return None;
        }
        let crf = self.cr_flags();
        let cref = self.extra_flags();
        let names_off = Self::rec_offset(crf & sys::CLF_SUPPORTED, cref & sys::CLFE_SUPPORTED);
        let namelen = self.cr_namelen() as usize;

        // SAFETY: names region spans `namelen` bytes starting at `names_off`.
        // The primary name's NUL terminator separates it from the source name.
        unsafe {
            let base = (self.rec as *const u8).add(names_off);
            let primary = CStr::from_ptr(base as *const i8).to_bytes();
            let consumed = primary.len() + 1; // include terminator
            if consumed >= namelen {
                return None;
            }
            let remaining = namelen - consumed;
            let sname_ptr = base.add(consumed);
            let slice = core::slice::from_raw_parts(sname_ptr, remaining);
            // Source name is also NUL-terminated inside `remaining`.
            let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
            Some(&slice[..end])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_from_raw_known() {
        assert_eq!(ChangelogEventType::from_raw(1), ChangelogEventType::Create);
        assert_eq!(ChangelogEventType::from_raw(6), ChangelogEventType::Unlink);
        assert_eq!(ChangelogEventType::from_raw(8), ChangelogEventType::Rename);
        assert_eq!(ChangelogEventType::from_raw(16), ChangelogEventType::Hsm);
    }

    #[test]
    fn event_type_from_raw_unknown() {
        assert_eq!(ChangelogEventType::from_raw(99), ChangelogEventType::Unknown(99));
    }

    #[test]
    fn event_type_display() {
        assert_eq!(ChangelogEventType::Create.to_string(), "CREAT");
        assert_eq!(ChangelogEventType::Unlink.to_string(), "UNLNK");
        assert_eq!(ChangelogEventType::Rename.to_string(), "RENME");
        assert_eq!(ChangelogEventType::Unknown(99).to_string(), "UNKNOWN(99)");
    }

    #[test]
    fn event_type_from_u32() {
        let t: ChangelogEventType = 1u32.into();
        assert_eq!(t, ChangelogEventType::Create);
    }

    #[test]
    fn event_type_as_str_matches_lctl() {
        // Values selected match `lctl get_param -n mdd.*.changelog_users` output format.
        assert_eq!(ChangelogEventType::Mkdir.as_str(), "MKDIR");
        assert_eq!(ChangelogEventType::Hardlink.as_str(), "HLINK");
        assert_eq!(ChangelogEventType::Softlink.as_str(), "SLINK");
    }
}
