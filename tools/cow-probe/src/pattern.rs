//! Deterministic 4 KiB patterns used to tag probe writes.
//!
//! A block carries its own logical index and a nonce, so a raw readback can be
//! attributed to exactly one write and two writes of the same file are never
//! byte-identical.

/// Filesystem, projection and pattern granularity. Every probe offset is a
/// multiple of this; the tool refuses a filesystem that breaks that rule.
pub const BLOCK: u64 = 4096;

/// One 4 KiB block written at `block_index` for `nonce`.
pub fn block_bytes(block_index: u64, nonce: u32) -> [u8; BLOCK as usize] {
    let mut block = [0u8; BLOCK as usize];
    block[0..4].copy_from_slice(&(block_index as u32).to_le_bytes());
    block[4..8].copy_from_slice(&nonce.to_le_bytes());
    for (offset, byte) in block[16..].iter_mut().enumerate() {
        *byte = (nonce as u8)
            .wrapping_mul(31)
            .wrapping_add(block_index as u8)
            .wrapping_add(offset as u8);
    }
    block
}
