//! Data structures passed to adapters, plus the rbhext_tool wire
//! protocol for the future C/S mode (not wired up yet — placeholder
//! for the `rbhext_tool_clnt` / `rbhext_tool_svr` behaviour).

use std::path::Path;

/// Which verb the policy wants from the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupOp {
    Archive,
    Restore,
    Remove,
}

/// One backup operation. Borrowed so callers can stack-allocate.
pub struct ToolInvocation<'a> {
    pub op: BackupOp,
    /// Absolute Lustre-side path for ARCHIVE / RESTORE / REMOVE.
    pub src: &'a Path,
    /// Backend destination. `None` for REMOVE on some setups where the
    /// adapter derives its own backend path from `src`.
    pub dest: Option<&'a Path>,
    /// Freeform tag interpreted by the adapter (tier selector, etc.).
    pub hints: Option<&'a str>,
    /// HSM archive id if known; 0 when irrelevant.
    pub archive_id: u32,
}
