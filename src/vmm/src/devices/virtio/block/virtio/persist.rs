// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring block devices.

use device::ConfigSpace;
use serde::{Deserialize, Serialize};
use vmm_sys_util::eventfd::EventFd;

use super::device::DiskProperties;
use super::*;
use crate::devices::virtio::block::persist::BlockConstructorArgs;
use crate::devices::virtio::block::virtio::device::FileEngineType;
use crate::devices::virtio::block::virtio::io::dirty_bitmap::DEFAULT_BLOCK_SIZE;
use crate::devices::virtio::block::virtio::io::FileEngine;
use crate::devices::virtio::block::virtio::metrics::BlockMetricsPerDevice;
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDeviceType};
use crate::devices::virtio::generated::virtio_blk::VIRTIO_BLK_F_RO;
use crate::devices::virtio::persist::VirtioDeviceState;
use crate::rate_limiter::RateLimiter;
use crate::rate_limiter::persist::RateLimiterState;
use crate::snapshot::Persist;

/// Holds info about block's file engine type. Gets saved in snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEngineTypeState {
    /// Sync File Engine.
    // If the snap version does not contain the `FileEngineType`, it must have been snapshotted
    // on a VM using the Sync backend.
    #[default]
    Sync,
    /// Async File Engine.
    Async,
    /// Overlay File Engine (sync COW with dirty bitmap).
    Overlay,
}

impl From<FileEngineType> for FileEngineTypeState {
    fn from(file_engine_type: FileEngineType) -> Self {
        match file_engine_type {
            FileEngineType::Sync => FileEngineTypeState::Sync,
            FileEngineType::Async => FileEngineTypeState::Async,
            FileEngineType::Overlay => FileEngineTypeState::Overlay,
        }
    }
}

impl From<FileEngineTypeState> for FileEngineType {
    fn from(file_engine_type_state: FileEngineTypeState) -> Self {
        match file_engine_type_state {
            FileEngineTypeState::Sync => FileEngineType::Sync,
            FileEngineTypeState::Async => FileEngineType::Async,
            FileEngineTypeState::Overlay => FileEngineType::Overlay,
        }
    }
}

/// Overlay-specific state persisted in snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayState {
    /// Path to the read-only base image.
    pub base_path: String,
    /// Path to the writable overlay file.
    pub overlay_path: String,
    /// Serialized dirty bitmap bytes.
    pub dirty_bitmap: Vec<u8>,
    /// Block size used for dirty tracking.
    pub block_size: u32,
    /// Total number of blocks in the bitmap.
    pub total_blocks: u64,
    /// Optional delta directory for cloning. Not serialized — set at restore time.
    #[serde(skip)]
    pub delta_dir: Option<std::path::PathBuf>,
}

/// Holds info about the block device. Gets saved in snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtioBlockState {
    pub id: String,
    partuuid: Option<String>,
    cache_type: CacheType,
    root_device: bool,
    pub disk_path: String,
    pub virtio_state: VirtioDeviceState,
    rate_limiter_state: RateLimiterState,
    file_engine_type: FileEngineTypeState,
    /// Overlay state, present only for overlay-backed block devices.
    ///
    /// `#[serde(skip)]` keeps this field out of the bitcode-serialized snapshot
    /// payload entirely so that snapshots remain byte-compatible with vanilla
    /// Firecracker (which doesn't know about overlay). The overlay state is
    /// persisted out-of-band in a side-car file (`<vmstate.snap>.overlay`)
    /// written by `persist::write_overlay_sidecar` and read back by
    /// `persist::read_overlay_sidecar`. `#[serde(default)]` ensures the field
    /// is `None` when the snapshot is loaded; the side-car loader then
    /// populates it for overlay-backed devices.
    #[serde(skip, default)]
    pub overlay_state: Option<OverlayState>,
}

impl VirtioBlock {
    fn restore_overlay_from_bitmap(
        overlay: &OverlayState,
    ) -> Result<DiskProperties, VirtioBlockError> {
        use crate::devices::virtio::block::virtio::io::dirty_bitmap::DirtyBitmap;

        let bitmap = DirtyBitmap::deserialize(
            &overlay.dirty_bitmap,
            overlay.block_size,
            overlay.total_blocks,
        )
        .map_err(|e| {
            VirtioBlockError::FileEngine(io::BlockIoError::Overlay(
                io::OverlayIoError::Bitmap(e),
            ))
        })?;

        DiskProperties::new_overlay_with_bitmap(
            overlay.base_path.clone(),
            overlay.overlay_path.clone(),
            overlay.block_size,
            bitmap,
        )
    }
}

impl Persist<'_> for VirtioBlock {
    type State = VirtioBlockState;
    type ConstructorArgs = BlockConstructorArgs;
    type Error = VirtioBlockError;

    fn save(&self) -> Self::State {
        let overlay_state = if let FileEngine::Overlay(ref engine) = self.disk.file_engine {
            let bitmap = engine.bitmap();
            Some(OverlayState {
                base_path: self.disk.base_path.clone().unwrap_or_default(),
                overlay_path: self.disk.file_path.clone(),
                dirty_bitmap: bitmap.serialize(),
                block_size: bitmap.block_size(),
                total_blocks: bitmap.total_blocks(),
                delta_dir: None,
            })
        } else {
            None
        };

        VirtioBlockState {
            id: self.id.clone(),
            partuuid: self.partuuid.clone(),
            cache_type: self.cache_type,
            root_device: self.root_device,
            disk_path: self.disk.file_path.clone(),
            virtio_state: VirtioDeviceState::from_device(self),
            rate_limiter_state: self.rate_limiter.save(),
            file_engine_type: FileEngineTypeState::from(self.file_engine_type()),
            overlay_state,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let is_read_only = state.virtio_state.avail_features & (1u64 << VIRTIO_BLK_F_RO) != 0;
        let rate_limiter = RateLimiter::restore((), &state.rate_limiter_state)
            .map_err(VirtioBlockError::RateLimiter)?;

        let disk_properties = if let Some(ref overlay) = state.overlay_state {
            // Check if a delta directory was set (for cloning).
            let delta_path = overlay
                .delta_dir
                .as_ref()
                .map(|dir| dir.join(format!("{}.delta", state.id)));

            if let Some(ref delta_path) = delta_path {
                if delta_path.exists() {
                    // Clone path: apply delta to a fresh overlay.
                    DiskProperties::new_overlay_from_delta(
                        overlay.base_path.clone(),
                        overlay.overlay_path.clone(),
                        delta_path,
                    )?
                } else {
                    // No delta file — fall through to bitmap restore.
                    Self::restore_overlay_from_bitmap(overlay)?
                }
            } else {
                // Normal restore: use the serialized bitmap.
                Self::restore_overlay_from_bitmap(overlay)?
            }
        } else {
            DiskProperties::new(
                state.disk_path.clone(),
                is_read_only,
                state.file_engine_type.into(),
            )?
        };

        let queue_evts = [EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioBlockError::EventFd)?];

        let queues = state
            .virtio_state
            .build_queues_checked(
                &constructor_args.mem,
                VirtioDeviceType::Block,
                BLOCK_NUM_QUEUES,
                FIRECRACKER_MAX_QUEUE_SIZE,
            )
            .map_err(VirtioBlockError::Persist)?;

        let avail_features = state.virtio_state.avail_features;
        let acked_features = state.virtio_state.acked_features;

        // Match the live-create path: when the device is overlay-backed, the
        // VIRTIO_BLK_F_DISCARD config-space fields must be valid per
        // virtio-spec 1.2 §5.2.4. avail_features comes from the snapshot, so
        // we key off the file-engine type rather than re-checking the bit.
        let mut config_space = ConfigSpace {
            capacity: disk_properties.nsectors.to_le(),
            ..Default::default()
        };
        if state.file_engine_type == FileEngineTypeState::Overlay {
            use crate::devices::virtio::block::virtio::SECTOR_SIZE;
            use crate::devices::virtio::block::virtio::io::dirty_bitmap::DEFAULT_BLOCK_SIZE;
            config_space.max_discard_sectors = u32::MAX.to_le();
            config_space.max_discard_seg = 1u32.to_le();
            config_space.discard_sector_alignment = (DEFAULT_BLOCK_SIZE / SECTOR_SIZE).to_le();
        }

        Ok(VirtioBlock {
            avail_features,
            acked_features,
            config_space,
            activate_evt: EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioBlockError::EventFd)?,

            queues,
            queue_evts,
            device_state: DeviceState::Inactive,

            id: state.id.clone(),
            partuuid: state.partuuid.clone(),
            cache_type: state.cache_type,
            root_device: state.root_device,
            read_only: is_read_only,

            disk: disk_properties,
            rate_limiter,
            is_io_engine_throttled: false,
            metrics: BlockMetricsPerDevice::alloc(state.id.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::device::VirtioBlockConfig;
    use crate::devices::virtio::device::VirtioDevice;
    use crate::devices::virtio::test_utils::{default_interrupt, default_mem};

    #[test]
    fn test_cache_semantic_ser() {
        // We create the backing file here so that it exists for the whole lifetime of the test.
        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();

        let config = VirtioBlockConfig {
            drive_id: "test".to_string(),
            path_on_host: f.as_path().to_str().unwrap().to_string(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            cache_type: CacheType::Writeback,
            rate_limiter: None,
            file_engine_type: FileEngineType::default(),
            base_path: None,
        };

        let block = VirtioBlock::new(config).unwrap();

        // Save the block device.
        let block_state = block.save();
        let _serialized_data = bitcode::serialize(&block_state).unwrap();
    }

    #[test]
    fn test_file_engine_type() {
        // Test conversions between FileEngineType and FileEngineTypeState.
        assert_eq!(
            FileEngineTypeState::Async,
            FileEngineTypeState::from(FileEngineType::Async)
        );
        assert_eq!(
            FileEngineTypeState::Sync,
            FileEngineTypeState::from(FileEngineType::Sync)
        );
        assert_eq!(FileEngineType::Async, FileEngineTypeState::Async.into());
        assert_eq!(FileEngineType::Sync, FileEngineTypeState::Sync.into());
        // Test default impl.
        assert_eq!(FileEngineTypeState::default(), FileEngineTypeState::Sync);
    }

    #[test]
    fn test_persistence() {
        // We create the backing file here so that it exists for the whole lifetime of the test.
        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();

        let config = VirtioBlockConfig {
            drive_id: "test".to_string(),
            path_on_host: f.as_path().to_str().unwrap().to_string(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            cache_type: CacheType::Unsafe,
            rate_limiter: None,
            file_engine_type: FileEngineType::default(),
            base_path: None,
        };

        let block = VirtioBlock::new(config).unwrap();
        let guest_mem = default_mem();

        // Save the block device.
        let block_state = block.save();
        let serialized_data = bitcode::serialize(&block_state).unwrap();

        // Restore the block device.
        let restored_state = bitcode::deserialize(&serialized_data).unwrap();
        let restored_block =
            VirtioBlock::restore(BlockConstructorArgs { mem: guest_mem }, &restored_state).unwrap();

        // Test that virtio specific fields are the same.
        assert_eq!(restored_block.device_type(), VirtioDeviceType::Block);
        assert_eq!(restored_block.avail_features(), block.avail_features());
        assert_eq!(restored_block.acked_features(), block.acked_features());
        assert_eq!(restored_block.queues(), block.queues());
        assert!(!block.is_activated());
        assert!(!restored_block.is_activated());

        // Test that block specific fields are the same.
        assert_eq!(restored_block.disk.file_path, block.disk.file_path);
    }

    #[test]
    fn test_overlay_persistence() {
        use std::io::Write;

        // Create base image with known data.
        let base_file = TempFile::new().unwrap();
        let base_data = vec![0xAA_u8; 0x1000];
        base_file.as_file().write_all(&base_data).unwrap();

        // Create overlay file (empty, will be sized to match base).
        let overlay_file = TempFile::new().unwrap();

        let base_path = base_file.as_path().to_str().unwrap().to_string();
        let overlay_path = overlay_file.as_path().to_str().unwrap().to_string();

        let config = VirtioBlockConfig {
            drive_id: "overlay_test".to_string(),
            path_on_host: overlay_path.clone(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            cache_type: CacheType::Unsafe,
            rate_limiter: None,
            file_engine_type: FileEngineType::Overlay,
            base_path: Some(base_path.clone()),
        };

        let block = VirtioBlock::new(config).unwrap();

        // Verify overlay state is present in the in-memory save.
        let block_state = block.save();
        assert!(block_state.overlay_state.is_some());

        let overlay_state = block_state.overlay_state.as_ref().unwrap();
        assert_eq!(overlay_state.base_path, base_path);
        assert_eq!(overlay_state.overlay_path, overlay_path);
        assert_eq!(overlay_state.block_size, 4096);

        // overlay_state is `#[serde(skip)]` so it does NOT survive bitcode
        // round-trip — it's persisted out-of-band in the side-car file by
        // `crate::persist::write_overlay_sidecar`. After deserialization the
        // field is `None`; production code re-injects it from the side-car.
        let serialized = bitcode::serialize(&block_state).unwrap();
        let mut restored_state: VirtioBlockState = bitcode::deserialize(&serialized).unwrap();
        assert!(restored_state.overlay_state.is_none());

        // Simulate the side-car re-injecting the overlay state, then restore.
        restored_state.overlay_state = block_state.overlay_state.clone();
        let guest_mem = default_mem();
        let restored_block =
            VirtioBlock::restore(BlockConstructorArgs { mem: guest_mem }, &restored_state).unwrap();

        // Verify restored device is overlay type.
        assert_eq!(restored_block.file_engine_type(), FileEngineType::Overlay);
        assert_eq!(restored_block.disk.file_path, overlay_path);
        assert_eq!(restored_block.disk.base_path.as_deref(), Some(base_path.as_str()));
    }

    #[test]
    fn test_old_snapshot_without_overlay_state() {
        // Simulate restoring an old snapshot that has no overlay_state field.
        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();

        let config = VirtioBlockConfig {
            drive_id: "test".to_string(),
            path_on_host: f.as_path().to_str().unwrap().to_string(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            cache_type: CacheType::Unsafe,
            rate_limiter: None,
            file_engine_type: FileEngineType::default(),
            base_path: None,
        };

        let block = VirtioBlock::new(config).unwrap();
        let block_state = block.save();

        // overlay_state should be None for non-overlay devices.
        assert!(block_state.overlay_state.is_none());

        // Serialize, deserialize, restore — should work fine.
        let serialized = bitcode::serialize(&block_state).unwrap();
        let restored_state: VirtioBlockState = bitcode::deserialize(&serialized).unwrap();
        assert!(restored_state.overlay_state.is_none());

        let guest_mem = default_mem();
        let restored_block =
            VirtioBlock::restore(BlockConstructorArgs { mem: guest_mem }, &restored_state).unwrap();
        assert_eq!(restored_block.file_engine_type(), FileEngineType::Sync);
    }
}
