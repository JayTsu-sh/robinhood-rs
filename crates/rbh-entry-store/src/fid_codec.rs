//! FID ↔ BINARY(16) encoding.
//!
//! Packs `LuFid { seq: u64, oid: u32, ver: u32 }` into 16 bytes in big-endian
//! order so that BINARY(16) column indexes produce lexicographic ordering that
//! matches numerical FID ordering. This matters for range scans on the `entries`
//! table.

use lustre_api::LuFid;

/// Encode a `LuFid` into 16 bytes (big-endian: 8 seq + 4 oid + 4 ver).
pub fn encode(fid: &LuFid) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&fid.seq.to_be_bytes());
    buf[8..12].copy_from_slice(&fid.oid.to_be_bytes());
    buf[12..16].copy_from_slice(&fid.ver.to_be_bytes());
    buf
}

/// Decode a `LuFid` from 16 bytes (big-endian).
pub fn decode(buf: &[u8]) -> Option<LuFid> {
    // M4 fix: exact length check — reject buffers that aren't exactly 16 bytes.
    if buf.len() != 16 {
        return None;
    }
    let seq = u64::from_be_bytes(buf[0..8].try_into().ok()?);
    let oid = u32::from_be_bytes(buf[8..12].try_into().ok()?);
    let ver = u32::from_be_bytes(buf[12..16].try_into().ok()?);
    Some(LuFid::new(seq, oid, ver))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let fid = LuFid::new(0x200000401, 0x1a, 0);
        let buf = encode(&fid);
        assert_eq!(buf.len(), 16);
        let back = decode(&buf).unwrap();
        assert_eq!(back, fid);
    }

    #[test]
    fn ordering_preserves_seq() {
        let a = encode(&LuFid::new(1, 0, 0));
        let b = encode(&LuFid::new(2, 0, 0));
        assert!(a < b, "big-endian encoding must preserve seq ordering");
    }

    #[test]
    fn ordering_preserves_oid() {
        let a = encode(&LuFid::new(1, 10, 0));
        let b = encode(&LuFid::new(1, 20, 0));
        assert!(a < b, "big-endian encoding must preserve oid ordering within same seq");
    }

    #[test]
    fn decode_short_buffer_returns_none() {
        assert!(decode(&[0u8; 15]).is_none());
        assert!(decode(&[]).is_none());
    }

    #[test]
    fn zero_fid() {
        let fid = LuFid::ZERO;
        let buf = encode(&fid);
        assert_eq!(buf, [0u8; 16]);
        assert_eq!(decode(&buf).unwrap(), fid);
    }
}
