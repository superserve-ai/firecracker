// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Block-level overlay engine: a read-only base image + a per-VM writable
//! upper layer, tracked by a dirty bitmap.
//!
//! ## Read path
//! For each block touched by the read, check the bitmap:
//! - Bit clear → read from `base` (shared across all VMs).
//! - Bit set   → read from `upper` (this VM's private layer).
//!
//! ## Write path
//! Always write to `upper`. For partial writes into a block that is still
//! clean, the block is first copied from `base` into `upper` (read-modify-
//! write), preserving any bytes not covered by the write.
//!
//! ## Reset
//! Truncate `upper` to zero bytes and call `DirtyBitmap::clear_all()`. The
//! VM returns to exactly the state it had at boot, in O(num_blocks / 64)
//! time — no file copying.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use super::dirty_bitmap::DirtyBitmap;

/// Default block size in bytes (one 4 KiB sector).
pub const DEFAULT_BLOCK_SIZE: u64 = 4096;

/// A synchronous block-level overlay engine.
///
/// `base` must be opened read-only. `upper` must be opened read-write and
/// should be a sparse file on the host filesystem so that blocks that have
/// never been written do not consume real disk space.
pub struct OverlayFileEngine {
    base: File,
    upper: File,
    bitmap: DirtyBitmap,
    block_size: u64,
}

impl OverlayFileEngine {
    /// Create an overlay engine with the default 4 KiB block size.
    pub fn new(base: File, upper: File, num_blocks: usize) -> Self {
        Self::with_block_size(base, upper, num_blocks, DEFAULT_BLOCK_SIZE)
    }

    /// Create an overlay engine with a custom block size. `block_size` must
    /// be a power of two and at least 512.
    pub fn with_block_size(
        base: File,
        upper: File,
        num_blocks: usize,
        block_size: u64,
    ) -> Self {
        assert!(block_size.is_power_of_two() && block_size >= 512);
        Self {
            base,
            upper,
            bitmap: DirtyBitmap::new(num_blocks),
            block_size,
        }
    }

    // ── Public API ─────────────────────────────────────────────────────────

    /// Read `buf.len()` bytes from the overlay starting at byte `offset`.
    ///
    /// Each block is served from `upper` if dirty, or `base` if clean.
    /// Returns the number of bytes read on success.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(0);
        }

        let start_block = offset / self.block_size;
        let end_block = (offset + len - 1) / self.block_size;

        for block_idx in start_block..=end_block {
            let block_start = block_idx * self.block_size;
            let block_end = block_start + self.block_size;

            // The sub-range of [offset, offset+len) that falls in this block.
            let read_start = offset.max(block_start);
            let read_end = (offset + len).min(block_end);

            let buf_start = (read_start - offset) as usize;
            let buf_end = (read_end - offset) as usize;
            let dest = &mut buf[buf_start..buf_end];

            if self.bitmap.is_set(block_idx as usize) {
                self.upper.read_at(dest, read_start)?;
            } else {
                self.base.read_at(dest, read_start)?;
            }
        }

        Ok(buf.len())
    }

    /// Write `buf` to the overlay starting at byte `offset`.
    ///
    /// Always writes to `upper` and marks touched blocks dirty. For partial
    /// writes into a clean block, the block is first copied from `base` into
    /// `upper` (copy-on-write) so that unmodified bytes are preserved.
    /// Returns the number of bytes written on success.
    pub fn write(&mut self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(0);
        }

        let start_block = offset / self.block_size;
        let end_block = (offset + len - 1) / self.block_size;

        for block_idx in start_block..=end_block {
            let block_start = block_idx * self.block_size;
            let block_end = block_start + self.block_size;

            // The sub-range of [offset, offset+len) that falls in this block.
            let write_start = offset.max(block_start);
            let write_end = (offset + len).min(block_end);

            let is_partial =
                write_start > block_start || write_end < block_end;

            // Copy-on-write: pull the block from base into upper before we
            // partially overwrite it, so the bytes we don't touch remain correct.
            if is_partial && !self.bitmap.is_set(block_idx as usize) {
                let mut block_buf = vec![0u8; self.block_size as usize];
                // read_at returns 0 for reads past EOF — that's fine, the
                // block is already zeroed and the upper is a sparse file.
                let _ = self.base.read_at(&mut block_buf, block_start);
                self.upper.write_at(&block_buf, block_start)?;
            }

            let buf_start = (write_start - offset) as usize;
            let buf_end = (write_end - offset) as usize;
            self.upper.write_at(&buf[buf_start..buf_end], write_start)?;
            self.bitmap.set(block_idx as usize);
        }

        Ok(buf.len())
    }

    /// Reset this VM to a clean state.
    ///
    /// Truncates `upper` to zero bytes and clears all dirty bits in the bitmap.
    /// After reset, all reads go to `base` again. `base` is unaffected.
    ///
    /// Cost: one `ftruncate` syscall + O(num_blocks / 64) to zero the bitmap.
    pub fn reset(&mut self) -> io::Result<()> {
        self.upper.set_len(0)?;
        self.bitmap.clear_all();
        Ok(())
    }

    /// Read-only access to the dirty bitmap. Useful for snapshot serialization:
    /// iterate `bitmap().iter_dirty()` to find which blocks to include.
    pub fn bitmap(&self) -> &DirtyBitmap {
        &self.bitmap
    }

    /// Number of blocks that have been written since the last reset.
    pub fn dirty_block_count(&self) -> usize {
        self.bitmap.dirty_block_count()
    }

    /// The configured block size in bytes.
    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    #[cfg(test)]
    pub fn base_file(&self) -> &File {
        &self.base
    }

    #[cfg(test)]
    pub fn upper_file(&self) -> &File {
        &self.upper
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::FileExt;

    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    // ── Helpers ─────────────────────────────────────────────────────────────

    const BS: u64 = 512; // small block size to keep test files tiny

    /// Build an engine over two fresh temp files. `base_data` is written to
    /// the base file before the engine is created.
    fn make_engine(base_data: &[u8], num_blocks: usize) -> OverlayFileEngine {
        let base_tf = TempFile::new().unwrap();
        let upper_tf = TempFile::new().unwrap();

        if !base_data.is_empty() {
            base_tf.as_file().write_all(base_data).unwrap();
        }

        // Open base read-only, upper read-write.
        let base = std::fs::OpenOptions::new()
            .read(true)
            .open(base_tf.as_path())
            .unwrap();
        let upper = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(upper_tf.as_path())
            .unwrap();

        // Keep the TempFiles alive for the test duration.
        std::mem::forget(base_tf);
        std::mem::forget(upper_tf);

        OverlayFileEngine::with_block_size(base, upper, num_blocks, BS)
    }

    fn random_block(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i * 7 + 13) as u8).collect()
    }

    // ── Construction ────────────────────────────────────────────────────────

    #[test]
    fn test_new_starts_clean() {
        let engine = make_engine(&[], 16);
        assert_eq!(engine.dirty_block_count(), 0);
        for i in 0..16 {
            assert!(!engine.bitmap().is_set(i));
        }
    }

    // ── Read path ───────────────────────────────────────────────────────────

    #[test]
    fn test_read_from_base_when_clean() {
        let data = random_block(BS as usize * 4);
        let engine = make_engine(&data, 4);

        let mut buf = vec![0u8; data.len()];
        engine.read(0, &mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn test_read_partial_block_from_base() {
        let data = random_block(BS as usize * 2);
        let engine = make_engine(&data, 2);

        // Read the second 128 bytes of block 0.
        let mut buf = vec![0u8; 128];
        engine.read(128, &mut buf).unwrap();
        assert_eq!(buf, &data[128..256]);
    }

    #[test]
    fn test_read_empty_buf_is_ok() {
        let engine = make_engine(&[], 4);
        let mut buf = [];
        let n = engine.read(0, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    // ── Write path ──────────────────────────────────────────────────────────

    #[test]
    fn test_write_goes_to_upper_not_base() {
        let base_data = vec![0u8; BS as usize];
        let mut engine = make_engine(&base_data, 1);

        let payload = vec![0xABu8; BS as usize];
        engine.write(0, &payload).unwrap();

        // Read back through overlay → should see upper data.
        let mut buf = vec![0u8; BS as usize];
        engine.read(0, &mut buf).unwrap();
        assert_eq!(buf, payload);

        // Base file must be unchanged.
        let mut base_buf = vec![0u8; BS as usize];
        engine.base_file().read_at(&mut base_buf, 0).unwrap();
        assert_eq!(base_buf, base_data);
    }

    #[test]
    fn test_write_sets_bitmap() {
        let mut engine = make_engine(&vec![0u8; BS as usize * 4], 4);
        engine.write(BS, &vec![1u8; BS as usize]).unwrap(); // write block 1
        assert!(!engine.bitmap().is_set(0));
        assert!(engine.bitmap().is_set(1));
        assert!(!engine.bitmap().is_set(2));
    }

    #[test]
    fn test_write_empty_buf_no_dirty_blocks() {
        let mut engine = make_engine(&[], 4);
        engine.write(0, &[]).unwrap();
        assert_eq!(engine.dirty_block_count(), 0);
    }

    #[test]
    fn test_write_and_read_back_data_correctness() {
        let base_data = vec![0u8; BS as usize * 8];
        let mut engine = make_engine(&base_data, 8);

        let payload: Vec<u8> = (0..BS as usize * 3).map(|i| i as u8).collect();
        engine.write(BS * 2, &payload).unwrap(); // blocks 2, 3, 4

        let mut buf = vec![0u8; BS as usize * 3];
        engine.read(BS * 2, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    // ── Multi-block write ────────────────────────────────────────────────────

    #[test]
    fn test_multi_block_write_sets_all_touched_blocks_dirty() {
        let base_data = vec![0u8; BS as usize * 6];
        let mut engine = make_engine(&base_data, 6);

        // Write spanning blocks 1 through 4.
        let payload = vec![0xCCu8; BS as usize * 4];
        engine.write(BS, &payload).unwrap();

        assert!(!engine.bitmap().is_set(0));
        assert!(engine.bitmap().is_set(1));
        assert!(engine.bitmap().is_set(2));
        assert!(engine.bitmap().is_set(3));
        assert!(engine.bitmap().is_set(4));
        assert!(!engine.bitmap().is_set(5));
        assert_eq!(engine.dirty_block_count(), 4);
    }

    #[test]
    fn test_multi_block_write_data_correctness() {
        let base_data = random_block(BS as usize * 4);
        let mut engine = make_engine(&base_data, 4);

        let payload = random_block(BS as usize * 2);
        engine.write(BS, &payload).unwrap(); // blocks 1 and 2

        // Blocks 0 and 3 are still from base.
        let mut full_buf = vec![0u8; BS as usize * 4];
        engine.read(0, &mut full_buf).unwrap();

        assert_eq!(&full_buf[..BS as usize], &base_data[..BS as usize]);
        assert_eq!(&full_buf[BS as usize..BS as usize * 3], &payload[..]);
        assert_eq!(
            &full_buf[BS as usize * 3..],
            &base_data[BS as usize * 3..]
        );
    }

    // ── Partial block write (copy-on-write) ──────────────────────────────────

    #[test]
    fn test_partial_write_on_clean_block_preserves_rest_of_block() {
        let mut base_data = vec![0u8; BS as usize];
        // Fill base block with a known pattern.
        for (i, b) in base_data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut engine = make_engine(&base_data, 1);

        // Write only the first 16 bytes of the block.
        let patch = vec![0xFFu8; 16];
        engine.write(0, &patch).unwrap();

        // The first 16 bytes should be our patch; the rest should be base data.
        let mut buf = vec![0u8; BS as usize];
        engine.read(0, &mut buf).unwrap();
        assert_eq!(&buf[..16], &patch[..]);
        assert_eq!(&buf[16..], &base_data[16..]);
    }

    #[test]
    fn test_partial_write_on_dirty_block_does_not_corrupt() {
        let base_data = vec![0xAAu8; BS as usize];
        let mut engine = make_engine(&base_data, 1);

        // First write: entire block.
        let first = vec![0x11u8; BS as usize];
        engine.write(0, &first).unwrap();

        // Second write: just the first byte.
        engine.write(0, &[0xFF]).unwrap();

        let mut buf = vec![0u8; BS as usize];
        engine.read(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xFF);
        assert_eq!(&buf[1..], &first[1..]);
    }

    // ── Reset ────────────────────────────────────────────────────────────────

    #[test]
    fn test_reset_clears_bitmap() {
        let base_data = vec![0u8; BS as usize * 4];
        let mut engine = make_engine(&base_data, 4);

        engine.write(0, &vec![1u8; BS as usize * 4]).unwrap();
        assert_eq!(engine.dirty_block_count(), 4);

        engine.reset().unwrap();
        assert_eq!(engine.dirty_block_count(), 0);
        for i in 0..4 {
            assert!(!engine.bitmap().is_set(i));
        }
    }

    #[test]
    fn test_reset_truncates_upper_to_zero() {
        let base_data = vec![0u8; BS as usize * 2];
        let mut engine = make_engine(&base_data, 2);

        engine.write(0, &vec![1u8; BS as usize * 2]).unwrap();
        // Upper file must have data now.
        let upper_size = engine.upper_file().metadata().unwrap().len();
        assert!(upper_size > 0);

        engine.reset().unwrap();
        let upper_size_after = engine.upper_file().metadata().unwrap().len();
        assert_eq!(upper_size_after, 0);
    }

    #[test]
    fn test_reset_restores_reads_to_base() {
        let base_data = random_block(BS as usize * 4);
        let mut engine = make_engine(&base_data, 4);

        // Overwrite everything with ones.
        engine.write(0, &vec![0x01u8; BS as usize * 4]).unwrap();

        // Reset.
        engine.reset().unwrap();

        // After reset, reads should return base data.
        let mut buf = vec![0u8; BS as usize * 4];
        engine.read(0, &mut buf).unwrap();
        assert_eq!(buf, base_data);
    }

    #[test]
    fn test_reset_does_not_touch_base() {
        let base_data = random_block(BS as usize);
        let mut engine = make_engine(&base_data, 1);

        engine.write(0, &vec![0xFFu8; BS as usize]).unwrap();
        engine.reset().unwrap();

        let mut base_buf = vec![0u8; BS as usize];
        engine.base_file().read_at(&mut base_buf, 0).unwrap();
        assert_eq!(base_buf, base_data);
    }

    // ── VM isolation ─────────────────────────────────────────────────────────

    #[test]
    fn test_two_vms_share_base_but_isolated() {
        // Both VMs share the same base data.
        let base_data = random_block(BS as usize * 4);

        let base_tf = TempFile::new().unwrap();
        base_tf.as_file().write_all(&base_data).unwrap();

        let open_base = || {
            std::fs::OpenOptions::new()
                .read(true)
                .open(base_tf.as_path())
                .unwrap()
        };

        let make_upper = || {
            let tf = TempFile::new().unwrap();
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(tf.as_path())
                .unwrap();
            std::mem::forget(tf);
            f
        };

        let mut vm1 = OverlayFileEngine::with_block_size(open_base(), make_upper(), 4, BS);
        let mut vm2 = OverlayFileEngine::with_block_size(open_base(), make_upper(), 4, BS);

        // VM1 writes block 0 with 0xAA.
        vm1.write(0, &vec![0xAAu8; BS as usize]).unwrap();

        // VM2 should still read the original base data from block 0.
        let mut buf = vec![0u8; BS as usize];
        vm2.read(0, &mut buf).unwrap();
        assert_eq!(buf, base_data[..BS as usize]);

        // VM1 should see its own write.
        let mut vm1_buf = vec![0u8; BS as usize];
        vm1.read(0, &mut vm1_buf).unwrap();
        assert_eq!(vm1_buf, vec![0xAAu8; BS as usize]);

        // Resetting VM1 should not affect VM2.
        vm1.reset().unwrap();
        let mut vm2_buf = vec![0u8; BS as usize];
        vm2.read(0, &mut vm2_buf).unwrap();
        assert_eq!(vm2_buf, base_data[..BS as usize]);
    }

    // ── dirty_block_count ────────────────────────────────────────────────────

    #[test]
    fn test_dirty_block_count_tracks_unique_blocks() {
        let base_data = vec![0u8; BS as usize * 8];
        let mut engine = make_engine(&base_data, 8);

        engine.write(0, &vec![1u8; BS as usize]).unwrap(); // block 0
        engine.write(0, &vec![2u8; BS as usize]).unwrap(); // block 0 again
        engine.write(BS * 3, &vec![3u8; BS as usize]).unwrap(); // block 3
        // Only 2 unique blocks dirtied.
        assert_eq!(engine.dirty_block_count(), 2);
    }

    // ── Accessor consistency ─────────────────────────────────────────────────

    #[test]
    fn test_block_size_accessor() {
        let engine = make_engine(&[], 4);
        assert_eq!(engine.block_size(), BS);
    }
}
