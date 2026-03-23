// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Delta file format for efficient snapshot persistence of overlay block devices.
//!
//! A delta file captures only the dirty blocks from an overlay, enabling O(dirty)
//! snapshot sizes instead of O(disk). The format includes CRC64 checksums for
//! integrity validation.
//!
//! Format:
//! ```text
//! [Header: 32 bytes]
//!   magic:        u64 = 0x4F564C5944454C54 ("OVLYDELT")
//!   version:      u32 = 1
//!   block_size:   u32
//!   total_blocks: u64
//!   dirty_count:  u64
//! [Bitmap section]
//!   bitmap_len:   u32
//!   bitmap_data:  [u8; bitmap_len]
//!   bitmap_crc:   u64
//! [Data section]
//!   For each dirty block (in index order):
//!     block_data: [u8; block_size]
//!   data_crc:     u64
//! ```

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

use crc64::crc64;

use super::dirty_bitmap::DirtyBitmap;

const DELTA_MAGIC: u64 = 0x4F56_4C59_4445_4C54; // "OVLYDELT"
const DELTA_VERSION: u32 = 1;

/// Statistics from a delta write or apply operation.
#[derive(Debug)]
pub struct DeltaStats {
    pub dirty_blocks: u64,
    pub bytes_written: u64,
    pub duration_us: u64,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum DeltaError {
    /// I/O error: {0}
    Io(std::io::Error),
    /// Invalid magic number: expected 0x{expected:016X}, got 0x{actual:016X}
    InvalidMagic { expected: u64, actual: u64 },
    /// Unsupported delta version: {0}
    UnsupportedVersion(u32),
    /// Bitmap CRC mismatch: expected 0x{expected:016X}, got 0x{computed:016X}
    BitmapCrcMismatch { expected: u64, computed: u64 },
    /// Data CRC mismatch: expected 0x{expected:016X}, got 0x{computed:016X}
    DataCrcMismatch { expected: u64, computed: u64 },
    /// Dirty count mismatch: header says {header}, bitmap has {bitmap}
    DirtyCountMismatch { header: u64, bitmap: u64 },
    /// Bitmap deserialization failed: {0}
    Bitmap(super::dirty_bitmap::DirtyBitmapError),
    /// Delta too large: {dirty_count} dirty blocks * {block_size} bytes exceeds limit
    TooLarge { dirty_count: u64, block_size: u32 },
}

impl From<std::io::Error> for DeltaError {
    fn from(e: std::io::Error) -> Self {
        DeltaError::Io(e)
    }
}

/// Write a delta file containing only the dirty blocks from the overlay.
pub fn write_delta(
    overlay: &mut File,
    bitmap: &DirtyBitmap,
    delta_path: &Path,
) -> Result<DeltaStats, DeltaError> {
    let start = Instant::now();
    let dirty_count = bitmap.dirty_count();
    let block_size = bitmap.block_size();
    let total_blocks = bitmap.total_blocks();

    let delta_file = File::create(delta_path)?;
    let mut writer = BufWriter::new(delta_file);

    // Write header.
    writer.write_all(&DELTA_MAGIC.to_le_bytes())?;
    writer.write_all(&DELTA_VERSION.to_le_bytes())?;
    writer.write_all(&block_size.to_le_bytes())?;
    writer.write_all(&total_blocks.to_le_bytes())?;
    writer.write_all(&dirty_count.to_le_bytes())?;

    // Write bitmap section.
    let bitmap_bytes = bitmap.serialize();
    let bitmap_len = bitmap_bytes.len() as u32;
    writer.write_all(&bitmap_len.to_le_bytes())?;
    writer.write_all(&bitmap_bytes)?;
    let bitmap_crc = crc64(0, &bitmap_bytes);
    writer.write_all(&bitmap_crc.to_le_bytes())?;

    // Write data section: read each dirty block from overlay and write it.
    let mut data_crc: u64 = 0;
    let mut block_buf = vec![0u8; block_size as usize];
    let mut bytes_written: u64 = 0;

    for block_idx in bitmap.iter_dirty() {
        let offset = block_idx * u64::from(block_size);
        overlay.seek(SeekFrom::Start(offset))?;
        overlay.read_exact(&mut block_buf)?;

        data_crc = crc64(data_crc, &block_buf);
        writer.write_all(&block_buf)?;
        bytes_written += u64::from(block_size);
    }

    writer.write_all(&data_crc.to_le_bytes())?;
    writer.flush()?;
    writer.get_ref().sync_all()?;

    Ok(DeltaStats {
        dirty_blocks: dirty_count,
        bytes_written,
        duration_us: start.elapsed().as_micros() as u64,
    })
}

/// Apply a delta file to an overlay, restoring the dirty blocks and bitmap.
pub fn apply_delta(
    overlay: &mut File,
    delta_path: &Path,
) -> Result<(DirtyBitmap, DeltaStats), DeltaError> {
    let start = Instant::now();

    let delta_file = File::open(delta_path)?;
    let mut reader = BufReader::new(delta_file);

    // Read and validate header.
    let magic = read_u64(&mut reader)?;
    if magic != DELTA_MAGIC {
        return Err(DeltaError::InvalidMagic {
            expected: DELTA_MAGIC,
            actual: magic,
        });
    }

    let version = read_u32(&mut reader)?;
    if version != DELTA_VERSION {
        return Err(DeltaError::UnsupportedVersion(version));
    }

    let block_size = read_u32(&mut reader)?;
    let total_blocks = read_u64(&mut reader)?;
    let dirty_count = read_u64(&mut reader)?;

    // Bound check: prevent OOM from malicious delta files.
    let max_data_size = total_blocks * u64::from(block_size);
    let delta_data_size = dirty_count * u64::from(block_size);
    if delta_data_size > max_data_size {
        return Err(DeltaError::TooLarge {
            dirty_count,
            block_size,
        });
    }

    // Read bitmap section.
    let bitmap_len = read_u32(&mut reader)? as usize;
    let mut bitmap_bytes = vec![0u8; bitmap_len];
    reader.read_exact(&mut bitmap_bytes)?;
    let stored_bitmap_crc = read_u64(&mut reader)?;

    // Validate bitmap CRC.
    let computed_bitmap_crc = crc64(0, &bitmap_bytes);
    if stored_bitmap_crc != computed_bitmap_crc {
        return Err(DeltaError::BitmapCrcMismatch {
            expected: stored_bitmap_crc,
            computed: computed_bitmap_crc,
        });
    }

    // Deserialize bitmap.
    let bitmap =
        DirtyBitmap::deserialize(&bitmap_bytes, block_size, total_blocks).map_err(DeltaError::Bitmap)?;

    // Validate dirty count matches bitmap.
    if bitmap.dirty_count() != dirty_count {
        return Err(DeltaError::DirtyCountMismatch {
            header: dirty_count,
            bitmap: bitmap.dirty_count(),
        });
    }

    // Read data section: write each dirty block to the overlay.
    let mut data_crc: u64 = 0;
    let mut block_buf = vec![0u8; block_size as usize];
    let mut bytes_written: u64 = 0;

    for block_idx in bitmap.iter_dirty() {
        reader.read_exact(&mut block_buf)?;
        data_crc = crc64(data_crc, &block_buf);

        let offset = block_idx * u64::from(block_size);
        overlay.seek(SeekFrom::Start(offset))?;
        overlay.write_all(&block_buf)?;
        bytes_written += u64::from(block_size);
    }

    // Validate data CRC.
    let stored_data_crc = read_u64(&mut reader)?;
    if stored_data_crc != data_crc {
        return Err(DeltaError::DataCrcMismatch {
            expected: stored_data_crc,
            computed: data_crc,
        });
    }

    overlay.flush()?;

    Ok((
        bitmap,
        DeltaStats {
            dirty_blocks: dirty_count,
            bytes_written,
            duration_us: start.elapsed().as_micros() as u64,
        },
    ))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, DeltaError> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u32(reader: &mut impl Read) -> Result<u32, DeltaError> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::io::dirty_bitmap::DEFAULT_BLOCK_SIZE;

    fn create_test_overlay(size: u64, dirty_offsets: &[(u64, &[u8])]) -> (File, DirtyBitmap) {
        let f = TempFile::new().unwrap().into_file();
        f.set_len(size).unwrap();

        let mut bitmap = DirtyBitmap::new(size, DEFAULT_BLOCK_SIZE).unwrap();
        let mut file = f;

        for (offset, data) in dirty_offsets {
            file.seek(SeekFrom::Start(*offset)).unwrap();
            file.write_all(data).unwrap();
            bitmap.set(*offset, data.len() as u32);
        }
        file.flush().unwrap();

        (file, bitmap)
    }

    #[test]
    fn test_write_and_apply_delta() {
        let disk_size: u64 = 4 * 4096; // 4 blocks

        // Create overlay with 2 dirty blocks.
        let block0_data = vec![0xAA_u8; 4096];
        let block2_data = vec![0xBB_u8; 4096];
        let (mut overlay, bitmap) = create_test_overlay(
            disk_size,
            &[(0, &block0_data), (8192, &block2_data)],
        );

        // Write delta.
        let delta_file = TempFile::new().unwrap();
        let delta_path = delta_file.as_path().to_path_buf();
        let write_stats = write_delta(&mut overlay, &bitmap, &delta_path).unwrap();

        assert_eq!(write_stats.dirty_blocks, 2);
        assert_eq!(write_stats.bytes_written, 2 * 4096);

        // Create a fresh overlay and apply the delta.
        let fresh_overlay = TempFile::new().unwrap().into_file();
        fresh_overlay.set_len(disk_size).unwrap();
        let mut fresh_overlay = fresh_overlay;

        let (restored_bitmap, apply_stats) =
            apply_delta(&mut fresh_overlay, &delta_path).unwrap();

        assert_eq!(apply_stats.dirty_blocks, 2);
        assert_eq!(apply_stats.bytes_written, 2 * 4096);

        // Verify bitmap matches.
        assert_eq!(restored_bitmap.dirty_count(), 2);
        assert!(restored_bitmap.is_set(0));
        assert!(!restored_bitmap.is_set(1));
        assert!(restored_bitmap.is_set(2));
        assert!(!restored_bitmap.is_set(3));

        // Verify data matches.
        let mut buf = vec![0u8; 4096];
        fresh_overlay.seek(SeekFrom::Start(0)).unwrap();
        fresh_overlay.read_exact(&mut buf).unwrap();
        assert_eq!(buf, block0_data);

        fresh_overlay.seek(SeekFrom::Start(8192)).unwrap();
        fresh_overlay.read_exact(&mut buf).unwrap();
        assert_eq!(buf, block2_data);
    }

    #[test]
    fn test_empty_delta() {
        let disk_size: u64 = 4 * 4096;
        let (mut overlay, bitmap) = create_test_overlay(disk_size, &[]);

        let delta_file = TempFile::new().unwrap();
        let delta_path = delta_file.as_path().to_path_buf();
        let stats = write_delta(&mut overlay, &bitmap, &delta_path).unwrap();

        assert_eq!(stats.dirty_blocks, 0);
        assert_eq!(stats.bytes_written, 0);

        let fresh = TempFile::new().unwrap().into_file();
        fresh.set_len(disk_size).unwrap();
        let mut fresh = fresh;
        let (restored_bitmap, _) = apply_delta(&mut fresh, &delta_path).unwrap();
        assert_eq!(restored_bitmap.dirty_count(), 0);
    }

    #[test]
    fn test_full_dirty_delta() {
        let disk_size: u64 = 2 * 4096;
        let data0 = vec![0xCC_u8; 4096];
        let data1 = vec![0xDD_u8; 4096];
        let (mut overlay, bitmap) =
            create_test_overlay(disk_size, &[(0, &data0), (4096, &data1)]);

        let delta_file = TempFile::new().unwrap();
        let delta_path = delta_file.as_path().to_path_buf();
        let stats = write_delta(&mut overlay, &bitmap, &delta_path).unwrap();

        assert_eq!(stats.dirty_blocks, 2);

        let fresh = TempFile::new().unwrap().into_file();
        fresh.set_len(disk_size).unwrap();
        let mut fresh = fresh;
        let (restored_bitmap, _) = apply_delta(&mut fresh, &delta_path).unwrap();
        assert_eq!(restored_bitmap.dirty_count(), 2);
    }

    #[test]
    fn test_corrupted_magic() {
        let disk_size: u64 = 4096;
        let (mut overlay, bitmap) =
            create_test_overlay(disk_size, &[(0, &vec![0xFF_u8; 4096])]);

        let delta_file = TempFile::new().unwrap();
        let delta_path = delta_file.as_path().to_path_buf();
        write_delta(&mut overlay, &bitmap, &delta_path).unwrap();

        // Corrupt the magic bytes.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&delta_path)
                .unwrap();
            f.write_all(&0u64.to_le_bytes()).unwrap();
        }

        let fresh = TempFile::new().unwrap().into_file();
        fresh.set_len(disk_size).unwrap();
        let mut fresh = fresh;
        let err = apply_delta(&mut fresh, &delta_path).unwrap_err();
        assert!(matches!(err, DeltaError::InvalidMagic { .. }));
    }

    #[test]
    fn test_corrupted_bitmap_crc() {
        let disk_size: u64 = 4096;
        let (mut overlay, bitmap) =
            create_test_overlay(disk_size, &[(0, &vec![0xFF_u8; 4096])]);

        let delta_file = TempFile::new().unwrap();
        let delta_path = delta_file.as_path().to_path_buf();
        write_delta(&mut overlay, &bitmap, &delta_path).unwrap();

        // Corrupt a byte in the bitmap section (after header 32 bytes + bitmap_len 4 bytes).
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&delta_path)
                .unwrap();
            f.seek(SeekFrom::Start(36)).unwrap();
            f.write_all(&[0xFF]).unwrap();
        }

        let fresh = TempFile::new().unwrap().into_file();
        fresh.set_len(disk_size).unwrap();
        let mut fresh = fresh;
        let err = apply_delta(&mut fresh, &delta_path).unwrap_err();
        assert!(matches!(err, DeltaError::BitmapCrcMismatch { .. }));
    }

    #[test]
    fn test_corrupted_data_crc() {
        let disk_size: u64 = 4096;
        let (mut overlay, bitmap) =
            create_test_overlay(disk_size, &[(0, &vec![0xFF_u8; 4096])]);

        let delta_file = TempFile::new().unwrap();
        let delta_path = delta_file.as_path().to_path_buf();
        write_delta(&mut overlay, &bitmap, &delta_path).unwrap();

        // Read file to find data section offset, then corrupt a data byte.
        let file_len = std::fs::metadata(&delta_path).unwrap().len();
        // Data CRC is at the end (last 8 bytes), data block is just before it.
        // Corrupt a byte in the data block.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&delta_path)
                .unwrap();
            // Seek to somewhere in the data block (well past the bitmap section).
            f.seek(SeekFrom::End(-100)).unwrap();
            f.write_all(&[0x00]).unwrap();
        }

        let fresh = TempFile::new().unwrap().into_file();
        fresh.set_len(disk_size).unwrap();
        let mut fresh = fresh;
        let err = apply_delta(&mut fresh, &delta_path).unwrap_err();
        assert!(matches!(err, DeltaError::DataCrcMismatch { .. }));
    }
}
