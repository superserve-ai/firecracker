// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring virtio-mem devices.

use std::sync::Arc;

use bitvec::vec::BitVec;
use serde::{Deserialize, Serialize};
use vm_memory::Address;

use crate::Vm;
use crate::devices::virtio::device::VirtioDeviceType;
use crate::devices::virtio::generated::virtio_ids::VIRTIO_ID_MEM;
use crate::devices::virtio::generated::virtio_mem::virtio_mem_config;
use crate::devices::virtio::mem::{MEM_NUM_QUEUES, VirtioMem, VirtioMemError};
use crate::devices::virtio::persist::{PersistError as VirtioStateError, VirtioDeviceState};
use crate::devices::virtio::queue::FIRECRACKER_MAX_QUEUE_SIZE;
use crate::snapshot::Persist;
use crate::utils::usize_to_u64;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionMmap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtioMemState {
    pub virtio_state: VirtioDeviceState,
    addr: u64,
    region_size: u64,
    block_size: u64,
    usable_region_size: u64,
    requested_size: u64,
    slot_size: usize,
    plugged_blocks: Vec<bool>,
}

impl VirtioMemState {
    /// Guest-physical (start, len) runs of the blocks the guest has not plugged.
    pub(crate) fn unplugged_ranges(&self) -> Vec<(u64, u64)> {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for (i, _) in self.plugged_blocks.iter().enumerate().filter(|(_, p)| !**p) {
            let start = self.addr + usize_to_u64(i) * self.block_size;
            match ranges.last_mut() {
                Some(last) if last.0 + last.1 == start => last.1 += self.block_size,
                _ => ranges.push((start, self.block_size)),
            }
        }
        ranges
    }
}

#[derive(Debug)]
pub struct VirtioMemConstructorArgs {
    vm: Arc<Vm>,
}

impl VirtioMemConstructorArgs {
    pub fn new(vm: Arc<Vm>) -> Self {
        Self { vm }
    }
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VirtioMemPersistError {
    /// Create virtio-mem: {0}
    CreateVirtioMem(#[from] VirtioMemError),
    /// Virtio state: {0}
    VirtioState(#[from] VirtioStateError),
}

impl Persist<'_> for VirtioMem {
    type State = VirtioMemState;
    type ConstructorArgs = VirtioMemConstructorArgs;
    type Error = VirtioMemPersistError;

    fn save(&self) -> Self::State {
        VirtioMemState {
            virtio_state: VirtioDeviceState::from_device(self),
            addr: self.config.addr,
            region_size: self.config.region_size,
            block_size: self.config.block_size,
            usable_region_size: self.config.usable_region_size,
            plugged_blocks: self.plugged_blocks.iter().by_vals().collect(),
            requested_size: self.config.requested_size,
            slot_size: self.slot_size,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let queues = state.virtio_state.build_queues_checked(
            constructor_args.vm.guest_memory(),
            VirtioDeviceType::Mem,
            MEM_NUM_QUEUES,
            FIRECRACKER_MAX_QUEUE_SIZE,
        )?;

        let plugged_blocks = BitVec::from_iter(state.plugged_blocks.iter());

        let config = virtio_mem_config {
            addr: state.addr,
            region_size: state.region_size,
            block_size: state.block_size,
            usable_region_size: state.usable_region_size,
            plugged_size: usize_to_u64(plugged_blocks.count_ones()) * state.block_size,
            requested_size: state.requested_size,
            ..Default::default()
        };

        let mut virtio_mem = VirtioMem::from_state(
            constructor_args.vm,
            queues,
            config,
            state.slot_size,
            plugged_blocks,
        )?;
        virtio_mem.set_avail_features(state.virtio_state.avail_features);
        virtio_mem.set_acked_features(state.virtio_state.acked_features);

        Ok(virtio_mem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::device::VirtioDevice;
    use crate::devices::virtio::mem::device::test_utils::default_virtio_mem;
    use crate::vstate::vm::tests::setup_vm_with_memory;

    fn state_with_blocks(plugged_blocks: Vec<bool>) -> VirtioMemState {
        VirtioMemState {
            virtio_state: VirtioDeviceState {
                device_type: VirtioDeviceType::Mem,
                avail_features: 0,
                acked_features: 0,
                queues: Vec::new(),
                activated: true,
            },
            addr: 0x1_0000_0000,
            region_size: 0x80_0000,
            block_size: 0x20_0000,
            usable_region_size: 0x80_0000,
            requested_size: 0,
            // Two blocks per slot, so block 1 below is unplugged inside a plugged slot.
            slot_size: 0x40_0000,
            plugged_blocks,
        }
    }

    #[test]
    fn test_unplugged_ranges() {
        let state = state_with_blocks(vec![true; 4]);
        assert!(state.unplugged_ranges().is_empty());

        let state = state_with_blocks(vec![false; 4]);
        assert_eq!(state.unplugged_ranges(), vec![(0x1_0000_0000, 0x80_0000)]);

        let state = state_with_blocks(vec![true, false, false, true]);
        assert_eq!(state.unplugged_ranges(), vec![(0x1_0020_0000, 0x40_0000)]);

        let state = state_with_blocks(vec![false, true, false, false]);
        assert_eq!(
            state.unplugged_ranges(),
            vec![(0x1_0000_0000, 0x20_0000), (0x1_0040_0000, 0x40_0000)]
        );
    }

    #[test]
    fn test_save_state() {
        let dev = default_virtio_mem();
        let state = dev.save();

        assert_eq!(state.addr, dev.config.addr);
        assert_eq!(state.region_size, dev.config.region_size);
        assert_eq!(state.block_size, dev.config.block_size);
        assert_eq!(state.usable_region_size, dev.config.usable_region_size);
        assert_eq!(
            state.plugged_blocks.iter().collect::<BitVec>(),
            dev.plugged_blocks
        );
        assert_eq!(state.requested_size, dev.config.requested_size);
        assert_eq!(state.slot_size, dev.slot_size);
    }

    #[test]
    fn test_save_restore_state() {
        let mut original_dev = default_virtio_mem();
        original_dev.set_acked_features(original_dev.avail_features());
        let state = original_dev.save();

        // Create a "new" VM for restore
        let (_, vm) = setup_vm_with_memory(0x1000);
        let vm = Arc::new(vm);
        let constructor_args = VirtioMemConstructorArgs::new(vm);
        let restored_dev = VirtioMem::restore(constructor_args, &state).unwrap();

        assert_eq!(original_dev.config, restored_dev.config);
        assert_eq!(original_dev.slot_size, restored_dev.slot_size);
        assert_eq!(original_dev.avail_features(), restored_dev.avail_features());
        assert_eq!(original_dev.acked_features(), restored_dev.acked_features());
    }
}
