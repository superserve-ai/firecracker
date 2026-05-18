// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bake a `rootfs.delta` into its `base.ext4` and zero the overlay side-car.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use vmm::devices::virtio::block::virtio::io::delta::{DeltaError, apply_delta, write_delta};
use vmm::devices::virtio::block::virtio::io::dirty_bitmap::{DirtyBitmap, DirtyBitmapError};
use vmm::persist::{read_overlay_sidecar_devices, write_overlay_sidecar_devices};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum FlattenError {
    /// Could not open base image: {0}
    OpenBase(io::Error),
    /// Apply delta: {0}
    ApplyDelta(DeltaError),
    /// fsync base: {0}
    SyncBase(io::Error),
    /// Write empty delta: {0}
    WriteEmptyDelta(DeltaError),
    /// Rename empty delta: {0}
    RenameDelta(io::Error),
    /// Side-car not found: {0:?}
    SidecarNotFound(PathBuf),
    /// Read side-car: {0}
    ReadSidecar(io::Error),
    /// Deserialize side-car bitmap: {0}
    DeserializeBitmap(DirtyBitmapError),
    /// Write side-car: {0}
    WriteSidecar(io::Error),
    /// Rename side-car: {0}
    RenameSidecar(io::Error),
}

#[derive(Debug, Subcommand)]
pub enum FlattenSubCommand {
    /// Bake the rootfs.delta into base.ext4 and zero the overlay side-car in place.
    Run {
        /// Path to base.ext4.
        #[arg(short, long)]
        base_path: PathBuf,
        /// Path to rootfs.delta.
        #[arg(short, long)]
        delta_path: PathBuf,
        /// Path to vmstate.snap.overlay.
        #[arg(short, long)]
        sidecar_path: PathBuf,
    },
}

pub fn flatten_command(cmd: FlattenSubCommand) -> Result<(), FlattenError> {
    match cmd {
        FlattenSubCommand::Run {
            base_path,
            delta_path,
            sidecar_path,
        } => flatten(&base_path, &delta_path, &sidecar_path),
    }
}

fn flatten(base_path: &Path, delta_path: &Path, sidecar_path: &Path) -> Result<(), FlattenError> {
    // NotFound here is a user typo, not the "no devices" the restore path tolerates.
    if !sidecar_path.exists() {
        return Err(FlattenError::SidecarNotFound(sidecar_path.to_path_buf()));
    }

    let mut base = OpenOptions::new()
        .read(true)
        .write(true)
        .open(base_path)
        .map_err(FlattenError::OpenBase)?;
    let (mut bitmap, _stats) =
        apply_delta(&mut base, delta_path).map_err(FlattenError::ApplyDelta)?;
    base.sync_all().map_err(FlattenError::SyncBase)?;

    // Side-car before delta: a crash after this keeps both restore branches
    // correct (an old-form delta is wasteful to replay but never corrupts).
    let mut devices =
        read_overlay_sidecar_devices(sidecar_path).map_err(FlattenError::ReadSidecar)?;
    for (_id, state) in devices.iter_mut() {
        let mut dev_bitmap =
            DirtyBitmap::deserialize(&state.dirty_bitmap, state.block_size, state.total_blocks)
                .map_err(FlattenError::DeserializeBitmap)?;
        dev_bitmap.clear();
        state.dirty_bitmap = dev_bitmap.serialize();
    }
    let tmp_sidecar = with_extra_ext(sidecar_path, "flatten.tmp");
    if let Err(e) = write_overlay_sidecar_devices(devices, &tmp_sidecar) {
        let _ = std::fs::remove_file(&tmp_sidecar);
        return Err(FlattenError::WriteSidecar(e));
    }
    if let Err(e) = std::fs::rename(&tmp_sidecar, sidecar_path) {
        let _ = std::fs::remove_file(&tmp_sidecar);
        return Err(FlattenError::RenameSidecar(e));
    }

    // write_delta on a cleared bitmap iterates zero blocks, so the `overlay`
    // arg is never read — handing it the already-open base file is safe.
    bitmap.clear();
    let tmp_delta = with_extra_ext(delta_path, "flatten.tmp");
    if let Err(e) = write_delta(&mut base, &bitmap, &tmp_delta) {
        let _ = std::fs::remove_file(&tmp_delta);
        return Err(FlattenError::WriteEmptyDelta(e));
    }
    if let Err(e) = std::fs::rename(&tmp_delta, delta_path) {
        let _ = std::fs::remove_file(&tmp_delta);
        return Err(FlattenError::RenameDelta(e));
    }

    Ok(())
}

fn with_extra_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s: OsString = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    s.into()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use vmm::devices::virtio::block::virtio::persist::OverlayState;
    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    const BLOCK_SIZE: u32 = 4096;
    const TOTAL_BLOCKS: u64 = 8;
    const DISK_SIZE: u64 = BLOCK_SIZE as u64 * TOTAL_BLOCKS;

    // Caller must keep the returned TempFiles in scope — they're deleted on drop.
    fn build_synthetic_snapshot(
        base_fill: u8,
        dirty_blocks: &[(u64, u8)],
    ) -> (TempFile, TempFile, TempFile) {
        let base = TempFile::new().unwrap();
        let delta = TempFile::new().unwrap();
        let sidecar = TempFile::new().unwrap();
        let overlay_src = TempFile::new().unwrap();

        let base_bytes = vec![base_fill; DISK_SIZE as usize];
        fs::write(base.as_path(), &base_bytes).unwrap();

        let mut overlay_bytes = vec![0u8; DISK_SIZE as usize];
        for &(idx, fill) in dirty_blocks {
            let off = (idx * u64::from(BLOCK_SIZE)) as usize;
            overlay_bytes[off..off + BLOCK_SIZE as usize].fill(fill);
        }
        fs::write(overlay_src.as_path(), &overlay_bytes).unwrap();

        let mut bitmap = DirtyBitmap::new(DISK_SIZE, BLOCK_SIZE).unwrap();
        for &(idx, _) in dirty_blocks {
            bitmap.set(idx * u64::from(BLOCK_SIZE), BLOCK_SIZE);
        }

        let mut overlay_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(overlay_src.as_path())
            .unwrap();
        write_delta(&mut overlay_file, &bitmap, delta.as_path()).unwrap();

        let state = OverlayState {
            base_path: base.as_path().to_string_lossy().into(),
            overlay_path: overlay_src.as_path().to_string_lossy().into(),
            dirty_bitmap: bitmap.serialize(),
            block_size: BLOCK_SIZE,
            total_blocks: TOTAL_BLOCKS,
            delta_dir: None,
        };
        write_overlay_sidecar_devices(
            vec![("test_drive".to_string(), state)],
            sidecar.as_path(),
        )
        .unwrap();

        // flatten() never reads overlay_path from state, so dropping is safe.
        drop(overlay_src);
        (base, delta, sidecar)
    }

    #[test]
    fn test_flatten_bakes_blocks_into_base() {
        let dirty = vec![(1u64, 0x11), (3, 0x33), (7, 0x77)];
        let (base, delta, sidecar) = build_synthetic_snapshot(0xAA, &dirty);

        flatten(base.as_path(), delta.as_path(), sidecar.as_path()).unwrap();

        let bytes = fs::read(base.as_path()).unwrap();
        for block_idx in 0..TOTAL_BLOCKS {
            let off = (block_idx * u64::from(BLOCK_SIZE)) as usize;
            let block = &bytes[off..off + BLOCK_SIZE as usize];
            let expected = dirty
                .iter()
                .find(|(i, _)| *i == block_idx)
                .map(|(_, f)| *f)
                .unwrap_or(0xAA);
            assert!(
                block.iter().all(|b| *b == expected),
                "block {block_idx}: expected fill 0x{expected:02X}, got first byte 0x{:02X}",
                block[0]
            );
        }
    }

    #[test]
    fn test_flatten_produces_empty_delta() {
        let (base, delta, sidecar) = build_synthetic_snapshot(0xAA, &[(2u64, 0x22), (5, 0x55)]);

        flatten(base.as_path(), delta.as_path(), sidecar.as_path()).unwrap();

        // Round-trip through apply_delta proves the rewritten delta's format and CRCs are valid.
        let dummy = TempFile::new().unwrap();
        fs::write(dummy.as_path(), vec![0u8; DISK_SIZE as usize]).unwrap();
        let mut dummy_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dummy.as_path())
            .unwrap();
        let (bitmap, _stats) = apply_delta(&mut dummy_file, delta.as_path()).unwrap();
        assert_eq!(bitmap.dirty_count(), 0);
    }

    #[test]
    fn test_flatten_zeros_sidecar_bitmaps() {
        let (base, delta, sidecar) = build_synthetic_snapshot(0xAA, &[(0u64, 0x01), (6, 0x06)]);

        flatten(base.as_path(), delta.as_path(), sidecar.as_path()).unwrap();

        let devices = read_overlay_sidecar_devices(sidecar.as_path()).unwrap();
        assert_eq!(devices.len(), 1);
        let bm = DirtyBitmap::deserialize(
            &devices[0].1.dirty_bitmap,
            devices[0].1.block_size,
            devices[0].1.total_blocks,
        )
        .unwrap();
        assert_eq!(bm.dirty_count(), 0);
    }

    #[test]
    fn test_flatten_idempotent() {
        let (base, delta, sidecar) = build_synthetic_snapshot(0xAA, &[(1u64, 0x11), (4, 0x44)]);

        flatten(base.as_path(), delta.as_path(), sidecar.as_path()).unwrap();
        flatten(base.as_path(), delta.as_path(), sidecar.as_path()).unwrap();
    }

    #[test]
    fn test_flatten_errors_on_missing_sidecar() {
        let base = TempFile::new().unwrap();
        fs::write(base.as_path(), vec![0u8; DISK_SIZE as usize]).unwrap();

        let result = flatten(
            base.as_path(),
            Path::new("/nonexistent/delta"),
            Path::new("/nonexistent/sidecar"),
        );
        assert!(matches!(result, Err(FlattenError::SidecarNotFound(_))));
    }
}
