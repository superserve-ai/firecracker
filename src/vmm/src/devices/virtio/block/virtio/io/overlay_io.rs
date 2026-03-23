// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Synchronous copy-on-write overlay file engine.
//!
//! Routes reads between a shared read-only base image and a per-VM sparse overlay
//! file using a dirty bitmap. Writes always go to the overlay.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use vm_memory::{GuestMemoryError, ReadVolatile, WriteVolatile};

use super::delta;
use super::dirty_bitmap::{DirtyBitmap, DirtyBitmapError};
use crate::vstate::memory::{GuestAddress, GuestMemory, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum OverlayIoError {
    /// Base read seek: {0}
    BaseSeek(std::io::Error),
    /// Base read transfer: {0}
    BaseTransfer(GuestMemoryError),
    /// Overlay seek: {0}
    OverlaySeek(std::io::Error),
    /// Overlay read transfer: {0}
    OverlayReadTransfer(GuestMemoryError),
    /// Overlay write transfer: {0}
    OverlayWriteTransfer(GuestMemoryError),
    /// Overlay flush: {0}
    OverlayFlush(std::io::Error),
    /// Overlay sync: {0}
    OverlaySync(std::io::Error),
    /// Size mismatch: base={base_size}, overlay={overlay_size}
    SizeMismatch { base_size: u64, overlay_size: u64 },
    /// Overlay engine cannot be created via from_file — use DiskProperties::new_overlay()
    NotConstructibleFromFile,
    /// Bitmap error: {0}
    Bitmap(DirtyBitmapError),
}

#[derive(Debug)]
pub struct OverlayFileEngine {
    base: File,
    overlay: File,
    bitmap: DirtyBitmap,
}

// OverlayFileEngine contains File and DirtyBitmap (Vec-backed), both of which are Send.
// No manual unsafe impl needed — derived automatically.

impl OverlayFileEngine {
    /// Create a new overlay engine from a read-only base file and a writable overlay file.
    ///
    /// The overlay file must have the same logical size as the base file (sparse is fine).
    /// If `bitmap` is `None`, a fresh empty bitmap is created.
    pub fn from_files(
        base: File,
        overlay: File,
        disk_size: u64,
        block_size: u32,
        bitmap: Option<DirtyBitmap>,
    ) -> Result<Self, OverlayIoError> {
        let bitmap = match bitmap {
            Some(bm) => bm,
            None => DirtyBitmap::new(disk_size, block_size).map_err(OverlayIoError::Bitmap)?,
        };

        Ok(Self {
            base,
            overlay,
            bitmap,
        })
    }

    /// Update the overlay file handle.
    /// Note: this does NOT reset the bitmap. Callers must ensure the new overlay
    /// is consistent with the current bitmap state. Hot-update of overlay devices
    /// is rejected at the VirtioBlock level to prevent data corruption.
    pub(crate) fn update_overlay(&mut self, overlay: File) {
        self.overlay = overlay;
    }

    /// Get a reference to the dirty bitmap.
    pub fn bitmap(&self) -> &DirtyBitmap {
        &self.bitmap
    }

    /// Get a reference to the overlay file.
    #[cfg(test)]
    pub fn overlay_file(&self) -> &File {
        &self.overlay
    }

    /// Get a mutable reference to the overlay file (for delta export).
    pub fn overlay_file_mut(&mut self) -> &mut File {
        &mut self.overlay
    }

    /// Discard blocks in the overlay: clear bitmap bits and punch holes in the overlay file.
    pub fn discard(&mut self, offset: u64, len: u64) -> Result<(), OverlayIoError> {
        if len == 0 {
            return Ok(());
        }

        // Clear bitmap bits for the discarded range.
        let block_size = u64::from(self.bitmap.block_size());
        let start_block = offset / block_size;
        let end_offset = offset.saturating_add(len).saturating_sub(1);
        let end_block = end_offset / block_size;
        let clamped_end = end_block.min(self.bitmap.total_blocks() - 1);

        for block in start_block..=clamped_end {
            self.bitmap.unset(block);
        }

        // Punch a hole in the overlay file to reclaim host disk space.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let fd = self.overlay.as_raw_fd();
            // SAFETY: fallocate with FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE is safe
            // on a valid fd with valid offset/len.
            let ret = unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    offset as i64,
                    len as i64,
                )
            };
            if ret != 0 {
                // Hole punching failure is non-fatal — the bitmap is already cleared,
                // so reads will go to base. We just don't reclaim the space.
                let _ = std::io::Error::last_os_error();
            }
        }

        Ok(())
    }

    /// Write a delta file containing only dirty blocks from this overlay.
    pub fn write_delta(
        &mut self,
        delta_path: &std::path::Path,
    ) -> Result<delta::DeltaStats, delta::DeltaError> {
        delta::write_delta(&mut self.overlay, &self.bitmap, delta_path)
    }

    /// Read from the appropriate source (base or overlay) based on the dirty bitmap.
    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, OverlayIoError> {
        match self.bitmap.is_range_uniform(offset, count) {
            Some(false) => {
                // All clean — read entirely from base.
                self.read_from_base(offset, mem, addr, count)
            }
            Some(true) => {
                // All dirty — read entirely from overlay.
                self.read_from_overlay(offset, mem, addr, count)
            }
            None => {
                // Mixed — split into contiguous runs from the same source.
                self.read_mixed(offset, mem, addr, count)
            }
        }
    }

    /// Write to the overlay and mark blocks as dirty.
    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, OverlayIoError> {
        self.overlay
            .seek(SeekFrom::Start(offset))
            .map_err(OverlayIoError::OverlaySeek)?;
        mem.get_slice(addr, count as usize)
            .and_then(|slice| Ok(self.overlay.write_all_volatile(&slice)?))
            .map_err(OverlayIoError::OverlayWriteTransfer)?;

        self.bitmap.set(offset, count);
        Ok(count)
    }

    /// Flush the overlay file to disk. Base is read-only and never needs flushing.
    pub fn flush(&mut self) -> Result<(), OverlayIoError> {
        self.overlay
            .flush()
            .map_err(OverlayIoError::OverlayFlush)?;
        self.overlay
            .sync_all()
            .map_err(OverlayIoError::OverlaySync)
    }

    /// Read a contiguous range from the base file.
    fn read_from_base(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, OverlayIoError> {
        self.base
            .seek(SeekFrom::Start(offset))
            .map_err(OverlayIoError::BaseSeek)?;
        mem.get_slice(addr, count as usize)
            .and_then(|mut slice| Ok(self.base.read_exact_volatile(&mut slice)?))
            .map_err(OverlayIoError::BaseTransfer)?;
        Ok(count)
    }

    /// Read a contiguous range from the overlay file.
    fn read_from_overlay(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, OverlayIoError> {
        self.overlay
            .seek(SeekFrom::Start(offset))
            .map_err(OverlayIoError::OverlaySeek)?;
        mem.get_slice(addr, count as usize)
            .and_then(|mut slice| Ok(self.overlay.read_exact_volatile(&mut slice)?))
            .map_err(OverlayIoError::OverlayReadTransfer)?;
        Ok(count)
    }

    /// Handle a read that spans both dirty and clean blocks.
    ///
    /// Splits the range into contiguous runs from the same source and reads each
    /// run separately. This is the slow path — most reads hit the fast path above.
    fn read_mixed(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, OverlayIoError> {
        let block_size = u64::from(self.bitmap.block_size());
        let end_offset = offset + u64::from(count);

        let mut current_offset = offset;
        let mut current_addr = addr;

        while current_offset < end_offset {
            let current_block = current_offset / block_size;
            let is_dirty = self.bitmap.is_set(current_block);

            // Find the end of this contiguous run of same-source blocks.
            let mut run_end_block = current_block + 1;
            while run_end_block * block_size < end_offset {
                if self.bitmap.is_set(run_end_block) != is_dirty {
                    break;
                }
                run_end_block += 1;
            }

            // Calculate the byte range for this run, clamped to the request bounds.
            let run_byte_start = current_offset;
            let run_byte_end = (run_end_block * block_size).min(end_offset);
            let run_len = (run_byte_end - run_byte_start) as u32;

            if is_dirty {
                self.read_from_overlay(run_byte_start, mem, current_addr, run_len)?;
            } else {
                self.read_from_base(run_byte_start, mem, current_addr, run_len)?;
            }

            current_offset = run_byte_end;
            current_addr = GuestAddress(current_addr.0 + u64::from(run_len));
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::io::dirty_bitmap::DEFAULT_BLOCK_SIZE;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory;
    use crate::vstate::memory::{Bytes, GuestRegionMmapExt};

    const FILE_LEN: u64 = 16384; // 4 blocks of 4KB
    const MEM_LEN: usize = 16384;

    fn create_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_regions(
            memory::anonymous(
                [(GuestAddress(0), MEM_LEN)].into_iter(),
                true,
                HugePageConfig::None,
            )
            .unwrap()
            .into_iter()
            .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
            .collect(),
        )
        .unwrap()
    }

    fn create_base_file(data: &[u8]) -> File {
        let f = TempFile::new().unwrap().into_file();
        use std::io::Write;
        (&f).write_all(data).unwrap();
        f
    }

    fn create_overlay_file(size: u64) -> File {
        let f = TempFile::new().unwrap().into_file();
        f.set_len(size).unwrap();
        f
    }

    fn create_engine(base_data: &[u8]) -> OverlayFileEngine {
        let base = create_base_file(base_data);
        let overlay = create_overlay_file(base_data.len() as u64);
        OverlayFileEngine::from_files(
            base,
            overlay,
            base_data.len() as u64,
            DEFAULT_BLOCK_SIZE,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_read_from_base_only() {
        let base_data: Vec<u8> = (0..FILE_LEN).map(|i| (i % 251) as u8).collect();
        let mut engine = create_engine(&base_data);
        let mem = create_mem();

        // Read first 512 bytes — should come from base.
        engine.read(0, &mem, GuestAddress(0), 512).unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, &base_data[..512]);
    }

    #[test]
    fn test_write_then_read_from_overlay() {
        let base_data = vec![0xAA_u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);

        // Write different data to overlay.
        let write_data = vec![0xBB_u8; 512];
        let mem = create_mem();
        mem.write(&write_data, GuestAddress(0)).unwrap();
        engine.write(0, &mem, GuestAddress(0), 512).unwrap();

        // Read back — should get overlay data, not base.
        let mem = create_mem();
        engine.read(0, &mem, GuestAddress(0), 512).unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, write_data);
    }

    #[test]
    fn test_base_not_modified_after_write() {
        let base_data = vec![0xAA_u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);

        // Write to overlay.
        let write_data = vec![0xBB_u8; 4096];
        let mem = create_mem();
        mem.write(&write_data, GuestAddress(0)).unwrap();
        engine.write(0, &mem, GuestAddress(0), 4096).unwrap();

        // Read from base directly — should still be original data.
        let mem = create_mem();
        engine.read_from_base(0, &mem, GuestAddress(0), 512).unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, vec![0xAA_u8; 512]);
    }

    #[test]
    fn test_mixed_read() {
        let base_data = vec![0xAA_u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);

        // Write to block 0 only (first 4KB).
        let write_data = vec![0xBB_u8; 4096];
        let mem = create_mem();
        mem.write(&write_data, GuestAddress(0)).unwrap();
        engine.write(0, &mem, GuestAddress(0), 4096).unwrap();

        // Read 8KB spanning block 0 (dirty) and block 1 (clean).
        let mem = create_mem();
        engine.read(0, &mem, GuestAddress(0), 8192).unwrap();

        let mut buf = vec![0u8; 8192];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();

        // First 4KB should be overlay data (0xBB).
        assert_eq!(&buf[..4096], &vec![0xBB_u8; 4096]);
        // Second 4KB should be base data (0xAA).
        assert_eq!(&buf[4096..8192], &vec![0xAA_u8; 4096]);
    }

    #[test]
    fn test_write_updates_bitmap() {
        let base_data = vec![0u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);

        assert_eq!(engine.bitmap().dirty_count(), 0);

        let mem = create_mem();
        engine.write(0, &mem, GuestAddress(0), 512).unwrap();
        assert_eq!(engine.bitmap().dirty_count(), 1);

        // Write to a different block.
        engine.write(4096, &mem, GuestAddress(0), 512).unwrap();
        assert_eq!(engine.bitmap().dirty_count(), 2);
    }

    #[test]
    fn test_flush() {
        let base_data = vec![0u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);
        // Flush should succeed even with no writes.
        engine.flush().unwrap();
    }

    #[test]
    fn test_read_at_offset() {
        let base_data: Vec<u8> = (0..FILE_LEN).map(|i| (i % 251) as u8).collect();
        let mut engine = create_engine(&base_data);
        let mem = create_mem();

        let offset = 4096u64;
        let count = 512u32;
        engine
            .read(offset, &mem, GuestAddress(0), count)
            .unwrap();

        let mut buf = vec![0u8; count as usize];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, &base_data[offset as usize..(offset as usize + count as usize)]);
    }

    #[test]
    fn test_write_at_offset_then_read() {
        let base_data = vec![0xAA_u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);

        // Write at offset 4096 (block 1).
        let write_data = vec![0xCC_u8; 512];
        let mem = create_mem();
        mem.write(&write_data, GuestAddress(0)).unwrap();
        engine.write(4096, &mem, GuestAddress(0), 512).unwrap();

        // Read back from same offset.
        let mem = create_mem();
        engine.read(4096, &mem, GuestAddress(0), 512).unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, write_data);

        // Block 0 should still be base data.
        let mem = create_mem();
        engine.read(0, &mem, GuestAddress(0), 512).unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, vec![0xAA_u8; 512]);
    }

    #[test]
    fn test_overwrite_same_block() {
        let base_data = vec![0xAA_u8; FILE_LEN as usize];
        let mut engine = create_engine(&base_data);

        // First write.
        let data1 = vec![0xBB_u8; 512];
        let mem = create_mem();
        mem.write(&data1, GuestAddress(0)).unwrap();
        engine.write(0, &mem, GuestAddress(0), 512).unwrap();

        // Overwrite same location.
        let data2 = vec![0xCC_u8; 512];
        let mem = create_mem();
        mem.write(&data2, GuestAddress(0)).unwrap();
        engine.write(0, &mem, GuestAddress(0), 512).unwrap();

        // Should get the second write.
        let mem = create_mem();
        engine.read(0, &mem, GuestAddress(0), 512).unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, data2);

        // Bitmap should still show 1 dirty block.
        assert_eq!(engine.bitmap().dirty_count(), 1);
    }
}
