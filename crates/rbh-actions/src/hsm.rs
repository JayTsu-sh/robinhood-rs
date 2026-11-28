//! FID-scoped Lustre HSM request primitives shared by the policy executors.

use std::path::{Path, PathBuf};

use lustre_api::{HsmAction, HsmRequestBuilder, LuFid, LustreApi};

pub fn hsm_fid_path(mount: &Path, fid: LuFid) -> PathBuf {
    mount.join(".lustre/fid").join(fid.to_string())
}

pub fn submit_hsm(
    lustre: LustreApi, mount: &Path, fid: LuFid, action: HsmAction, archive_id: u32, hints: Option<Vec<u8>>,
) -> lustre_api::error::Result<()> {
    let mut request = HsmRequestBuilder::new(action).archive_id(archive_id).add_fid(fid);
    if let Some(hints) = hints {
        request = request.data(hints);
    }
    request.submit(&lustre, mount)
}
