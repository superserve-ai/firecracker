// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(target_arch = "x86_64")]
use kvm_bindings::KVM_IRQCHIP_IOAPIC;
use kvm_bindings::{
    KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQ_ROUTING_MSI, KVM_MSI_VALID_DEVID, KvmIrqRouting,
    kvm_irq_routing_entry, kvm_userspace_memory_region,
};
use kvm_ioctls::VmFd;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use vmm_sys_util::errno;
use vmm_sys_util::eventfd::EventFd;

pub use crate::arch::{ArchVm as Vm, ArchVmError, VmState};
use crate::arch::{GSI_MSI_END, host_page_size};
use crate::logger::info;
use crate::pci::{DeviceRelocation, DeviceRelocationError, PciDevice};
use crate::persist::{CreateSnapshotError, file_ranges};
use crate::utils::u64_to_usize;
use crate::vmm_config::snapshot::SnapshotType;
use crate::vstate::bus::Bus;
use crate::vstate::interrupts::{InterruptError, MsixVector, MsixVectorConfig, MsixVectorGroup};
use crate::vstate::memory::{
    GuestMemory, GuestMemoryExtension, GuestMemoryMmap, GuestMemoryRegion,
    GuestMemorySlot, GuestMemoryState, GuestRegionMmap, GuestRegionMmapExt, GuestRegionType,
    KvmSubSlot, MemoryError, split_logged, unlogged_ranges,
};
use crate::vstate::resources::ResourceAllocator;
use crate::vstate::vcpu::VcpuError;
use crate::{DirtyBitmap, Vcpu, mem_size_mib};

#[derive(Debug, Serialize, Deserialize)]
/// A struct representing an interrupt line used by some device of the microVM
pub struct RoutingEntry {
    entry: kvm_irq_routing_entry,
    masked: bool,
}

/// Outcome of a diff memory dump, used by the caller to derive the memory
/// file's presence side-car.
#[derive(Debug)]
pub(crate) struct DiffDump {
    /// Bitmap over the memory file's pages (one bit per host page, LSB-first
    /// per word) of exactly the pages this dump wrote.
    pub dumped_pages: Vec<u64>,
    /// True when the diff was merged into a pre-existing same-size file,
    /// whose earlier pages remain in the file but are not in `dumped_pages`.
    pub merged: bool,
}

/// Architecture independent parts of a VM.
#[derive(Debug)]
pub struct VmCommon {
    /// The KVM file descriptor used to access this Vm.
    pub fd: VmFd,
    max_memslots: u32,
    /// The guest memory of this Vm.
    pub guest_memory: GuestMemoryMmap,
    next_kvm_slot: AtomicU32,
    /// Whether no memory dump has run yet. Until one does, an all-zero page in a zero-base
    /// sub-slot equals the hole it was restored from; after, the file may hold what that
    /// dump wrote there, so a page zeroed since must be written.
    first_dump: AtomicBool,
    /// Interrupts used by Vm's devices
    pub interrupts: Mutex<HashMap<u32, RoutingEntry>>,
    /// Allocator for VM resources
    pub resource_allocator: Mutex<ResourceAllocator>,
    /// MMIO bus
    pub mmio_bus: Arc<Bus>,
    /// Test-only countdown that forces the Nth-next `set_user_memory_region` call to fail, used
    /// to exercise partial-failure handling. 0 means never fail.
    #[cfg(test)]
    fail_set_user_memory_region_in: AtomicU32,
}

/// Errors associated with the wrappers over KVM ioctls.
/// Needs `rustfmt::skip` to make multiline comments work
#[rustfmt::skip]
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VmError {
    /// Cannot set the memory regions: {0}
    SetUserMemoryRegion(kvm_ioctls::Error),
    /// Failed to create VM: {0}
    CreateVm(kvm_ioctls::Error),
    /// Failed to get KVM's dirty log: {0}
    GetDirtyLog(kvm_ioctls::Error),
    /// {0}
    Arch(#[from] ArchVmError),
    /// Error during eventfd operations: {0}
    EventFd(std::io::Error),
    /// Failed to create vcpu: {0}
    CreateVcpu(VcpuError),
    /// The number of configured slots is bigger than the maximum reported by KVM: {0}
    NotEnoughMemorySlots(u32),
    /// Failed to add a memory region: {0}
    InsertRegion(#[from] vm_memory::GuestRegionCollectionError),
    /// Error calling mincore: {0}
    Mincore(vmm_sys_util::errno::Error),
    /// ResourceAllocator error: {0}
    ResourceAllocator(#[from] vm_allocator::Error),
    /// MemoryError error: {0}
    MemoryError(#[from] MemoryError),
}

/// Contains Vm functions that are usable across CPU architectures
impl Vm {
    /// Create a KVM VM
    pub fn create_common(kvm: &crate::vstate::kvm::Kvm) -> Result<VmCommon, VmError> {
        // It is known that KVM_CREATE_VM occasionally fails with EINTR on heavily loaded machines
        // with many VMs.
        //
        // The behavior itself that KVM_CREATE_VM can return EINTR is intentional. This is because
        // the KVM_CREATE_VM path includes mm_take_all_locks() that is CPU intensive and all CPU
        // intensive syscalls should check for pending signals and return EINTR immediately to allow
        // userland to remain interactive.
        // https://lists.nongnu.org/archive/html/qemu-devel/2014-01/msg01740.html
        //
        // However, it is empirically confirmed that, even though there is no pending signal,
        // KVM_CREATE_VM returns EINTR.
        // https://lore.kernel.org/qemu-devel/8735e0s1zw.wl-maz@kernel.org/
        //
        // To mitigate it, QEMU does an infinite retry on EINTR that greatly improves reliabiliy:
        // - https://github.com/qemu/qemu/commit/94ccff133820552a859c0fb95e33a539e0b90a75
        // - https://github.com/qemu/qemu/commit/bbde13cd14ad4eec18529ce0bf5876058464e124
        //
        // Similarly, we do retries up to 5 times. Although Firecracker clients are also able to
        // retry, they have to start Firecracker from scratch. Doing retries in Firecracker makes
        // recovery faster and improves reliability.
        const MAX_ATTEMPTS: u32 = 5;
        let mut attempt = 1;
        let fd = loop {
            match kvm.fd.create_vm() {
                Ok(fd) => break fd,
                Err(e) if e.errno() == libc::EINTR && attempt < MAX_ATTEMPTS => {
                    info!("Attempt #{attempt} of KVM_CREATE_VM returned EINTR");
                    // Exponential backoff (1us, 2us, 4us, and 8us => 15us in total)
                    std::thread::sleep(std::time::Duration::from_micros(2u64.pow(attempt - 1)));
                }
                Err(e) => return Err(VmError::CreateVm(e)),
            }

            attempt += 1;
        };

        Ok(VmCommon {
            fd,
            max_memslots: kvm.max_nr_memslots(),
            guest_memory: GuestMemoryMmap::default(),
            next_kvm_slot: AtomicU32::new(0),
            first_dump: AtomicBool::new(true),
            interrupts: Mutex::new(HashMap::with_capacity(GSI_MSI_END as usize + 1)),
            resource_allocator: Mutex::new(ResourceAllocator::new()),
            mmio_bus: Arc::new(Bus::new()),
            #[cfg(test)]
            fail_set_user_memory_region_in: AtomicU32::new(0),
        })
    }

    /// Creates the specified number of [`Vcpu`]s.
    ///
    /// The returned [`EventFd`] is written to whenever any of the vcpus exit.
    pub fn create_vcpus(&mut self, vcpu_count: u8) -> Result<(Vec<Vcpu>, EventFd), VmError> {
        self.arch_pre_create_vcpus(vcpu_count)?;

        let exit_evt = EventFd::new(libc::EFD_NONBLOCK).map_err(VmError::EventFd)?;

        let mut vcpus = Vec::with_capacity(vcpu_count as usize);
        for cpu_idx in 0..vcpu_count {
            let exit_evt = exit_evt.try_clone().map_err(VmError::EventFd)?;
            let vcpu = Vcpu::new(cpu_idx, self, exit_evt).map_err(VmError::CreateVcpu)?;
            vcpus.push(vcpu);
        }

        self.arch_post_create_vcpus(vcpu_count)?;

        Ok((vcpus, exit_evt))
    }

    /// Reserves the next `slot_cnt` contiguous kvm slot ids and returns the first one
    pub fn next_kvm_slot(&self, slot_cnt: u32) -> Option<u32> {
        let next = self
            .common
            .next_kvm_slot
            .fetch_add(slot_cnt, Ordering::Relaxed);
        if self.common.max_memslots <= next {
            None
        } else {
            Some(next)
        }
    }

    pub(crate) fn set_user_memory_region(
        &self,
        region: kvm_userspace_memory_region,
    ) -> Result<(), VmError> {
        #[cfg(test)]
        if self.common.fail_set_user_memory_region_in.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |n| (n > 0).then(|| n - 1),
        ) == Ok(1)
        {
            return Err(VmError::SetUserMemoryRegion(kvm_ioctls::Error::new(
                libc::ENOMEM,
            )));
        }
        // SAFETY: Safe because the fd is a valid KVM file descriptor.
        unsafe {
            self.fd()
                .set_user_memory_region(region)
                .map_err(VmError::SetUserMemoryRegion)
        }
    }

    /// Test-only: arm the fault injector so the `n`th subsequent call to
    /// [`Self::set_user_memory_region`] fails (n == 1 fails the very next call).
    #[cfg(test)]
    pub(crate) fn fail_next_set_user_memory_region(&self, n: u32) {
        self.common
            .fail_set_user_memory_region_in
            .store(n, Ordering::Relaxed);
    }

    fn register_memory_region(&mut self, region: Arc<GuestRegionMmapExt>) -> Result<(), VmError> {
        let new_guest_memory = self
            .common
            .guest_memory
            .insert_region(Arc::clone(&region))?;

        region
            .slots()
            .try_for_each(|(ref slot, plugged)| match plugged {
                // if the slot is plugged, add it to kvm user memory regions
                true => slot
                    .kvm_regions()
                    .into_iter()
                    .try_for_each(|r| self.set_user_memory_region(r)),
                // if the slot is not plugged, protect accesses to it
                false => slot.protect(true).map_err(VmError::MemoryError),
            })?;

        self.common.guest_memory = new_guest_memory;

        Ok(())
    }

    /// Register a list of new memory regions to this [`Vm`].
    pub fn register_dram_memory_regions(
        &mut self,
        regions: Vec<GuestRegionMmap>,
    ) -> Result<(), VmError> {
        for region in regions {
            let next_slot = self
                .next_kvm_slot(1)
                .ok_or(VmError::NotEnoughMemorySlots(self.common.max_memslots))?;

            let arcd_region =
                Arc::new(GuestRegionMmapExt::dram_from_mmap_region(region, next_slot));

            self.register_memory_region(arcd_region)?
        }

        Ok(())
    }

    /// Register a new hotpluggable region to this [`Vm`].
    pub fn register_hotpluggable_memory_region(
        &mut self,
        region: GuestRegionMmap,
        slot_size: usize,
    ) -> Result<(), VmError> {
        // caller should ensure the slot size divides the region length.
        assert!(region.len().is_multiple_of(slot_size as u64));
        let slot_cnt = (region.len() / (slot_size as u64))
            .try_into()
            .map_err(|_| VmError::NotEnoughMemorySlots(self.common.max_memslots))?;
        let slot_from = self
            .next_kvm_slot(slot_cnt)
            .ok_or(VmError::NotEnoughMemorySlots(self.common.max_memslots))?;
        let arcd_region = Arc::new(GuestRegionMmapExt::hotpluggable_from_mmap_region(
            region, slot_from, slot_size,
        ));

        self.register_memory_region(arcd_region)
    }

    /// Register a list of new memory regions to this [`Vm`]. `hole_slots` are memory file
    /// ranges holding no snapshot data, for [`Self::split_hole_slots`], which leaves
    /// `reserved_slots` KVM slots free for devices restored afterwards.
    ///
    /// Note: regions and state.regions need to be in the same order.
    pub fn restore_memory_regions(
        &mut self,
        regions: Vec<GuestRegionMmap>,
        state: &GuestMemoryState,
        hole_slots: &[(u64, u64)],
        reserved_slots: u32,
    ) -> Result<(), VmError> {
        let mut exts = Vec::with_capacity(state.regions.len());
        for (region, state) in regions.into_iter().zip(state.regions.iter()) {
            let slot_cnt = state
                .plugged
                .len()
                .try_into()
                .map_err(|_| VmError::NotEnoughMemorySlots(self.common.max_memslots))?;

            let next_slot = self
                .next_kvm_slot(slot_cnt)
                .ok_or(VmError::NotEnoughMemorySlots(self.common.max_memslots))?;

            exts.push(GuestRegionMmapExt::from_state(region, state, next_slot)?);
        }
        if !hole_slots.is_empty() {
            self.split_hole_slots(&mut exts, unlogged_ranges(state, hole_slots), reserved_slots);
        }
        for ext in exts {
            self.register_memory_region(Arc::new(ext))?;
        }

        Ok(())
    }

    /// Backs the `unlogged` ranges of each region with KVM slots without dirty logging,
    /// advised for transparent huge pages: KVM maps a logged slot 4 KiB at a time. A DRAM
    /// region is split into several KVM slots, the first keeping its own number; a hotplug
    /// slot is unlogged whole. Without enough KVM slots, DRAM stays one logged slot.
    fn split_hole_slots(
        &self,
        exts: &mut [GuestRegionMmapExt],
        unlogged: Vec<Vec<(usize, usize)>>,
        reserved_slots: u32,
    ) {
        let dram_pieces: Vec<_> = exts
            .iter()
            .zip(&unlogged)
            .map(|(ext, ranges)| match ext.region_type {
                GuestRegionType::Dram if !ranges.is_empty() => {
                    split_logged(u64_to_usize(ext.len()), ranges)
                }
                _ => Vec::new(),
            })
            .collect();
        let extra: usize = dram_pieces.iter().map(|p| p.len().saturating_sub(1)).sum();
        let fits = u32::try_from(extra)
            .ok()
            .and_then(|n| {
                self.common
                    .next_kvm_slot
                    .load(Ordering::Relaxed)
                    .checked_add(n)?
                    .checked_add(reserved_slots)
            })
            .is_some_and(|end| end <= self.common.max_memslots);
        if !fits {
            warn!("hole_huge_pages: {extra} more KVM slots needed than free; DRAM stays logged");
        }
        let (mut slots, mut bytes) = (0, 0);
        for ((ext, ranges), pieces) in exts.iter_mut().zip(unlogged).zip(dram_pieces) {
            ext.sub_slots = match ext.region_type {
                GuestRegionType::Hotpluggable => ranges
                    .into_iter()
                    .map(|(offset, len)| KvmSubSlot {
                        slot: ext.slot_from + u32::try_from(offset / ext.slot_size).unwrap(),
                        offset,
                        len,
                        logged: false,
                        zero_base: true,
                    })
                    .collect(),
                GuestRegionType::Dram if fits => pieces
                    .into_iter()
                    .enumerate()
                    .map(|(i, (offset, len, logged))| KvmSubSlot {
                        slot: match i {
                            0 => ext.slot_from,
                            _ => self.next_kvm_slot(1).expect("KVM slots counted above"),
                        },
                        offset,
                        len,
                        logged,
                        zero_base: !logged,
                    })
                    .collect(),
                GuestRegionType::Dram => Vec::new(),
            };
            for sub in ext.sub_slots.iter().filter(|s| !s.logged) {
                // SAFETY: the range is within this region's live mapping.
                let ret = unsafe {
                    libc::madvise(
                        ext.as_ptr().add(sub.offset).cast(),
                        sub.len,
                        libc::MADV_HUGEPAGE,
                    )
                };
                if ret < 0 {
                    warn!(
                        "madvise(MADV_HUGEPAGE) failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
                slots += 1;
                bytes += sub.len;
            }
        }
        info!("hole_huge_pages: {slots} KVM slots without dirty logging cover {bytes} bytes");
    }

    /// Gets a reference to the kvm file descriptor owned by this VM.
    pub fn fd(&self) -> &VmFd {
        &self.common.fd
    }

    /// Gets a reference to this [`Vm`]'s [`GuestMemoryMmap`] object
    pub fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.common.guest_memory
    }

    /// Gets a mutable reference to this [`Vm`]'s [`ResourceAllocator`] object
    pub fn resource_allocator(&self) -> MutexGuard<'_, ResourceAllocator> {
        self.common
            .resource_allocator
            .lock()
            .expect("Poisoned lock")
    }

    /// Resets the KVM dirty bitmap for each of the guest's memory regions.
    pub fn reset_dirty_bitmap(&self) {
        self.guest_memory()
            .iter()
            .flat_map(|region| region.plugged_slots())
            .for_each(|mem_slot| {
                for sub in mem_slot.sub_slots.iter().filter(|s| s.logged) {
                    let _ = self.fd().get_dirty_log(sub.slot, sub.len);
                }
            });
    }

    /// Bitmap over memory file pages of those resident in guest memory, plugged slots
    /// only: what the guest had populated, found by `mincore` on the mappings. Also
    /// returns the file's page count.
    pub fn resident_pages(&self) -> Result<(Vec<u64>, usize), VmError> {
        let page_size = host_page_size();
        let total: usize = self
            .guest_memory()
            .iter()
            .flat_map(|region| region.slots())
            .map(|(slot, _)| slot.slice.len())
            .sum();
        let mut bits = vec![0u64; (total / page_size).div_ceil(64)];
        let mut base = 0usize;
        for (slot, plugged) in self.guest_memory().iter().flat_map(|region| region.slots()) {
            let pages = slot.slice.len() / page_size;
            if plugged {
                let resident =
                    mincore_bitmap(slot.slice.ptr_guard_mut().as_ptr(), slot.slice.len())?;
                or_bits_at(&mut bits, base, &resident, pages);
            }
            base += pages;
        }
        Ok((bits, base))
    }

    /// Retrieves the KVM dirty bitmap for each of the guest's memory regions.
    pub fn get_dirty_bitmap(&self) -> Result<DirtyBitmap, VmError> {
        // mincore misses swapped-out pages, so with swap every hole page counts as dirty and
        // only the zero check narrows it.
        let swap = self
            .guest_memory()
            .iter()
            .any(|r| r.sub_slots.iter().any(|s| s.zero_base))
            && swap_active();
        if swap {
            static WARN: std::sync::Once = std::sync::Once::new();
            WARN.call_once(|| {
                warn!("hole_huge_pages: swap active or unknown; diffs scan whole holes")
            });
        }
        self.guest_memory()
            .iter()
            .flat_map(|region| region.plugged_slots())
            .map(|mem_slot| {
                let bitmap = slot_dirty_bitmap(&mem_slot, swap, |slot, len| {
                    self.fd()
                        .get_dirty_log(slot, len)
                        .map_err(VmError::GetDirtyLog)
                })?;
                Ok((mem_slot.slot, bitmap))
            })
            .collect()
    }

    /// Takes a snapshot of the virtual machine running inside the given [`Vmm`] and saves it to
    /// `mem_file_path`.
    ///
    /// If `snapshot_type` is [`SnapshotType::Diff`], and `mem_file_path` exists and is a snapshot
    /// file of matching size, then the diff snapshot will be directly merged into the existing
    /// snapshot. Otherwise, existing files are simply overwritten.
    ///
    /// For diff snapshots, returns which file pages this dump wrote and whether it merged into a
    /// pre-existing same-size file (whose earlier pages remain in the file but are not covered by
    /// `dumped_pages`); the caller derives the memory file's presence side-car from this. Full
    /// snapshots return `None` — every page of a full dump is authoritative.
    /// `zero_ranges` are memory file `(offset, len)` ranges, page aligned, that hold zeros
    /// unless the guest has written them since: a diff punches them out of the file and reports
    /// them as dumped, and writes the dirty pages among them over the holes.
    /// `zero_skip` lets the first dump leave out all-zero pages of zero-base sub-slots, which
    /// read as zero holes of the snapshot they were restored from.
    /// When `untrusted_merge` is set, an existing same-size balloon image has no reliable
    /// presence map. Write a full image so an old zero hole cannot inherit base data.
    pub(crate) fn snapshot_memory_to_file(
        &self,
        mem_file_path: &Path,
        snapshot_type: SnapshotType,
        zero_ranges: &[(u64, u64)],
        zero_skip: bool,
        balloon_layout: Option<&GuestMemoryState>,
        untrusted_merge: bool,
    ) -> Result<Option<DiffDump>, CreateSnapshotError> {
        use self::CreateSnapshotError::*;

        // Need to check this here, as we create the file in the line below
        let file_existed = mem_file_path.exists();

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(mem_file_path)
            .map_err(|err| MemoryBackingFile("open", err))?;

        // Determine what size our total memory area is.
        let mem_size_mib = mem_size_mib(self.guest_memory());
        let expected_size = mem_size_mib * 1024 * 1024;

        // Every dump counts, a failed one too: it may have written part of the file.
        let zero_skip = self.common.first_dump.swap(false, Ordering::Relaxed) && zero_skip;
        let mut merged = false;
        if file_existed {
            let file_size = file
                .metadata()
                .map_err(|e| MemoryBackingFile("get_metadata", e))?
                .len();

            // Here we only truncate the file if the size mismatches.
            // - For full snapshots, the entire file's contents will be overwritten anyway. We have
            //   to avoid truncating here to deal with the edge case where it represents the
            //   snapshot file from which this very microVM was loaded (as modifying the memory file
            //   would be reflected in the mmap of the file, meaning a truncate operation would zero
            //   out guest memory, and thus corrupt the VM).
            // - For diff snapshots, we want to merge the diff layer directly into the file.
            if file_size != expected_size {
                file.set_len(0)
                    .map_err(|err| MemoryBackingFile("truncate", err))?;
            } else {
                merged = true;
            }
        }

        // Set the length of the file to the full size of the memory area.
        file.set_len(expected_size)
            .map_err(|e| MemoryBackingFile("set_length", e))?;

        let snapshot_type = if merged && untrusted_merge && balloon_layout.is_some() {
            SnapshotType::Full
        } else {
            snapshot_type
        };
        let (discarded, discarded_file) = balloon_layout.map_or_else(Default::default, |layout| {
            let guest = self.guest_memory().discarded_ranges(snapshot_type);
            let file = file_ranges(layout, &guest);
            (guest, file)
        });
        let diff_dump = match snapshot_type {
            SnapshotType::Diff => {
                // A file created above is already sparse; only a merge can hold old contents.
                if merged {
                    hole_ranges(&mut file, zero_ranges.iter().chain(&discarded_file))?;
                }
                let dirty_bitmap = self.get_dirty_bitmap()?;
                let mut dumped_pages = self.guest_memory().dump_dirty(
                    &mut file,
                    &dirty_bitmap,
                    zero_skip,
                    &discarded,
                )?;
                let page_size = host_page_size() as u64;
                for &(offset, len) in zero_ranges.iter().chain(&discarded_file) {
                    for page in offset / page_size..(offset + len) / page_size {
                        let page = u64_to_usize(page);
                        if let Some(word) = dumped_pages.get_mut(page >> 6) {
                            *word |= 1u64 << (page & 63);
                        }
                    }
                }
                Some(DiffDump {
                    dumped_pages,
                    merged,
                })
            }
            SnapshotType::Full => {
                if merged {
                    hole_ranges(&mut file, discarded_file.iter())?;
                }
                self.guest_memory().dump(&mut file, &discarded)?;
                self.reset_dirty_bitmap();
                self.guest_memory().reset_dirty();
                None
            }
        };

        #[cfg(test)]
        if tests::FAIL_AFTER_DUMP.replace(false) {
            return Err(MemoryBackingFile("flush", std::io::Error::other("injected")));
        }
        file.flush()
            .map_err(|err| MemoryBackingFile("flush", err))?;
        file.sync_all()
            .map_err(|err| MemoryBackingFile("sync_all", err))?;
        Ok(diff_dump)
    }

    /// Register a device IRQ
    pub fn register_irq(&self, fd: &EventFd, gsi: u32) -> Result<(), errno::Error> {
        self.common.fd.register_irqfd(fd, gsi)?;

        let mut entry = kvm_irq_routing_entry {
            gsi,
            type_: KVM_IRQ_ROUTING_IRQCHIP,
            ..Default::default()
        };
        #[cfg(target_arch = "x86_64")]
        {
            entry.u.irqchip.irqchip = KVM_IRQCHIP_IOAPIC;
        }
        #[cfg(target_arch = "aarch64")]
        {
            entry.u.irqchip.irqchip = 0;
        }
        entry.u.irqchip.pin = gsi;

        self.common
            .interrupts
            .lock()
            .expect("Poisoned lock")
            .insert(
                gsi,
                RoutingEntry {
                    entry,
                    masked: false,
                },
            );
        Ok(())
    }

    /// Register an MSI device interrupt
    pub fn register_msi(
        &self,
        route: &MsixVector,
        masked: bool,
        config: MsixVectorConfig,
    ) -> Result<(), errno::Error> {
        let mut entry = kvm_irq_routing_entry {
            gsi: route.gsi,
            type_: KVM_IRQ_ROUTING_MSI,
            ..Default::default()
        };
        entry.u.msi.address_lo = config.low_addr;
        entry.u.msi.address_hi = config.high_addr;
        entry.u.msi.data = config.data;

        if self.common.fd.check_extension(kvm_ioctls::Cap::MsiDevid) {
            // According to KVM documentation:
            // https://docs.kernel.org/virt/kvm/api.html#kvm-set-gsi-routing
            //
            // if the capability is set, we need to set the flag and provide a valid unique device
            // ID. "For PCI, this is usually a BDF identifier in the lower 16 bits".
            //
            // The layout of `config.devid` is:
            //
            // |---- 16 bits ----|-- 8 bits --|-- 5 bits --|-- 3 bits --|
            // |      segment    |     bus    |   device   |  function  |
            //
            // For the time being, we are using a single PCI segment and a single bus per segment
            // so just passing config.devid should be fine.
            entry.flags = KVM_MSI_VALID_DEVID;
            entry.u.msi.__bindgen_anon_1.devid = config.devid;
        }

        self.common
            .interrupts
            .lock()
            .expect("Poisoned lock")
            .insert(route.gsi, RoutingEntry { entry, masked });

        Ok(())
    }

    /// Create a group of MSI-X interrupts
    pub fn create_msix_group(vm: Arc<Vm>, count: u16) -> Result<MsixVectorGroup, InterruptError> {
        debug!("Creating new MSI group with {count} vectors");
        let mut vectors = Vec::with_capacity(count as usize);
        for gsi in vm
            .resource_allocator()
            .allocate_gsi_msi(count as u32)?
            .iter()
        {
            vectors.push(MsixVector::new(*gsi, false)?);
        }

        Ok(MsixVectorGroup { vm, vectors })
    }

    /// Set GSI routes to KVM
    pub fn set_gsi_routes(&self) -> Result<(), InterruptError> {
        let entries = self.common.interrupts.lock().expect("Poisoned lock");
        let mut routes = KvmIrqRouting::new(0)?;

        for entry in entries.values() {
            if entry.masked {
                continue;
            }
            routes.push(entry.entry)?;
        }

        self.common.fd.set_gsi_routing(&routes)?;
        Ok(())
    }
}

/// Dirty bitmap of `mem_slot`, assembled from its KVM slots: `get_log` for a logged one,
/// host presence for an unlogged one, which nothing populates but the guest and devices.
/// With `swap`, every page of a zero-base one, as presence misses swapped-out pages.
fn slot_dirty_bitmap(
    mem_slot: &GuestMemorySlot<'_>,
    swap: bool,
    mut get_log: impl FnMut(u32, usize) -> Result<Vec<u64>, VmError>,
) -> Result<Vec<u64>, VmError> {
    let page_size = host_page_size();
    match mem_slot.sub_slots[..] {
        [sub] if sub.logged => return get_log(sub.slot, sub.len),
        [sub] if !sub.zero_base => {
            return mincore_bitmap(mem_slot.slice.ptr_guard_mut().as_ptr(), sub.len);
        }
        _ => {}
    }
    let mut bitmap = vec![0u64; (mem_slot.slice.len() / page_size).div_ceil(64)];
    for sub in &mem_slot.sub_slots {
        let bits = match sub.logged {
            true => get_log(sub.slot, sub.len)?,
            false if swap && sub.zero_base => vec![u64::MAX; (sub.len / page_size).div_ceil(64)],
            false => {
                // SAFETY: the sub-slot is within the slot's mapping.
                let addr = unsafe { mem_slot.slice.ptr_guard_mut().as_ptr().add(sub.offset) };
                mincore_bitmap(addr, sub.len)?
            }
        };
        or_bits_at(
            &mut bitmap,
            sub.offset / page_size,
            &bits,
            sub.len / page_size,
        );
    }
    Ok(bitmap)
}

/// Whether `/proc/swaps` lists a swap area; unreadable counts as yes.
fn swap_active() -> bool {
    std::fs::read_to_string("/proc/swaps").map_or(true, |s| s.lines().count() > 1)
}

/// ORs the first `pages` bits of `src` into `dst` starting at bit `base`, a word at
/// a time when `base` is word aligned (slots are, being whole MiB), else bit by bit.
fn or_bits_at(dst: &mut [u64], base: usize, src: &[u64], pages: usize) {
    if base % 64 == 0 {
        for (i, w) in src.iter().enumerate().take(pages.div_ceil(64)) {
            let keep = pages - i * 64;
            let w = if keep < 64 { w & ((1u64 << keep) - 1) } else { *w };
            dst[base / 64 + i] |= w;
        }
        return;
    }
    for page in (0..pages).filter(|p| src[p / 64] >> (p % 64) & 1 == 1) {
        dst[(base + page) / 64] |= 1 << ((base + page) % 64);
    }
}

/// Use `mincore(2)` to overapproximate the dirty bitmap for the given memslot. To be used
/// if a diff snapshot is requested, but dirty page tracking wasn't enabled.
fn mincore_bitmap(addr: *mut u8, len: usize) -> Result<Vec<u64>, VmError> {
    // TODO: Once Host 5.10 goes out of support, we can make this more robust and work on
    // swap-enabled systems, by doing mlock2(MLOCK_ONFAULT)/munlock() in this function (to
    // force swapped-out pages to get paged in, so that mincore will consider them incore).
    // However, on AMD (m6a/m7a) 5.10, doing so introduces a 100%/30ms regression to snapshot
    // creation, even if swap is disabled, so currently it cannot be done.

    // Mincore always works at PAGE_SIZE granularity, even if the VMA we are dealing with
    // is a hugetlbfs VMA (e.g. to report a single hugepage as "present", mincore will
    // give us 512 4k markers with the lowest bit set).
    let page_size = host_page_size();
    let mut mincore_bitmap = vec![0u8; len / page_size];
    let mut bitmap = vec![0u64; (len / page_size).div_ceil(64)];

    // SAFETY: The safety invariants of GuestRegionMmap ensure that region.as_ptr() is a valid
    // userspace mapping of size region.len() bytes. The bitmap has exactly one byte for each
    // page in this userspace mapping. Note that mincore does not operate on bitmaps like
    // KVM_MEM_LOG_DIRTY_PAGES, but rather it uses 8 bits per page (e.g. 1 byte), setting the
    // least significant bit to 1 if the page corresponding to a byte is in core (available in
    // the page cache and resolvable via just a minor page fault).
    let r = unsafe { libc::mincore(addr.cast(), len, mincore_bitmap.as_mut_ptr()) };

    if r != 0 {
        return Err(VmError::Mincore(vmm_sys_util::errno::Error::last()));
    }

    for (page_idx, b) in mincore_bitmap.iter().enumerate() {
        bitmap[page_idx / 64] |= (*b as u64 & 0x1) << (page_idx as u64 % 64);
    }

    Ok(bitmap)
}

impl DeviceRelocation for Vm {
    fn move_bar(
        &self,
        _old_base: u64,
        _new_base: u64,
        _len: u64,
        _pci_dev: &mut dyn PciDevice,
    ) -> Result<(), DeviceRelocationError> {
        Err(DeviceRelocationError::NotSupported)
    }
}

/// Makes the memory file `(offset, len)` ranges read as zeros.
fn hole_ranges<'a>(
    file: &mut std::fs::File,
    ranges: impl Iterator<Item = &'a (u64, u64)>,
) -> Result<(), CreateSnapshotError> {
    for &(offset, len) in ranges {
        if punch_hole(file, offset, len).is_err() {
            // Filesystems without hole punching get the zeros written.
            write_zeros(file, offset, len)
                .map_err(|err| CreateSnapshotError::MemoryBackingFile("write_zeros", err))?;
        }
    }
    Ok(())
}

fn write_zeros(file: &mut std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    // Seek and write: the default seccomp filters allow these, not pwrite64. The dump that
    // follows writes from the start of the file, so the cursor goes back there.
    let zeros = vec![0u8; 1 << 20];
    file.seek(SeekFrom::Start(offset))?;
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(zeros.len() as u64);
        file.write_all(&zeros[..u64_to_usize(n)])?;
        done += n;
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn punch_hole(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    // SAFETY: fallocate on a valid open fd; KEEP_SIZE leaves the file length alone.
    let ret = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            len,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::Ordering;

    use vm_memory::GuestAddress;
    use vm_memory::mmap::MmapRegionBuilder;

    use super::*;
    use crate::snapshot::Persist;
    use crate::test_utils::single_region_mem_raw;
    use crate::utils::mib_to_bytes;
    use crate::vstate::kvm::Kvm;
    use crate::vstate::memory::GuestRegionMmap;

    #[test]
    fn or_bits_at_matches_bit_by_bit() {
        let src = vec![u64::MAX, 0b1011];
        for (base, pages) in [(0, 70), (64, 66), (3, 70), (128, 64)] {
            let mut fast = vec![0u64; 4];
            or_bits_at(&mut fast, base, &src, pages);
            let mut slow = vec![0u64; 4];
            for p in (0..pages).filter(|p| src[p / 64] >> (p % 64) & 1 == 1) {
                slow[(base + p) / 64] |= 1 << ((base + p) % 64);
            }
            assert_eq!(fast, slow, "base {base}, pages {pages}");
        }
    }

    #[test]
    fn slot_dirty_bitmap_assembles_logged_and_unlogged_sub_slots() {
        use crate::vmm_config::machine_config::HugePageConfig;
        use crate::vstate::memory::{Bytes, MemoryRegionAddress, anonymous};

        let page_size = host_page_size();
        let regions = anonymous(
            [(GuestAddress(0), page_size * 8)].into_iter(),
            true,
            HugePageConfig::None,
        )
        .unwrap();
        let mut region =
            GuestRegionMmapExt::dram_from_mmap_region(regions.into_iter().next().unwrap(), 0);
        region.sub_slots = vec![
            KvmSubSlot {
                slot: 0,
                offset: 0,
                len: page_size * 2,
                logged: true,
                zero_base: false,
            },
            KvmSubSlot {
                slot: 5,
                offset: page_size * 2,
                len: page_size * 3,
                logged: false,
                zero_base: true,
            },
            KvmSubSlot {
                slot: 6,
                offset: page_size * 5,
                len: page_size * 3,
                logged: true,
                zero_base: false,
            },
        ];
        // Only page 3 of the unlogged pages is populated.
        region
            .write_slice(&[1], MemoryRegionAddress((page_size * 3) as u64))
            .unwrap();
        let mut calls = Vec::new();
        let mut get_log = |slot, len| {
            calls.push((slot, len));
            Ok(vec![if slot == 0 { 0b10 } else { 0b001 }])
        };
        let bitmap = slot_dirty_bitmap(&region.mem_slot(0), false, &mut get_log).unwrap();
        assert_eq!(bitmap, vec![0b10_1010]);
        // With swap the whole hole counts as dirty.
        let bitmap = slot_dirty_bitmap(&region.mem_slot(0), true, &mut get_log).unwrap();
        assert_eq!(bitmap, vec![0b11_1110]);
        assert_eq!(calls, [(0, page_size * 2), (6, page_size * 3)].repeat(2));
    }

    #[test]
    fn test_restore_hole_slots() {
        use crate::vmm_config::machine_config::HugePageConfig;
        use crate::vstate::memory::{GuestMemoryRegionState, anonymous};

        let mib = |n: u64| n << 20;
        let state = GuestMemoryState {
            regions: vec![
                GuestMemoryRegionState {
                    base_address: 0,
                    size: mib_to_bytes(64),
                    region_type: GuestRegionType::Dram,
                    plugged: vec![true],
                },
                GuestMemoryRegionState {
                    base_address: mib(1024),
                    size: mib_to_bytes(256),
                    region_type: GuestRegionType::Hotpluggable,
                    plugged: vec![true, false],
                },
            ],
        };
        let restore = |vm: &mut Vm, holes: &[(u64, u64)], reserved: u32| {
            let regions = anonymous(state.regions(), true, HugePageConfig::None).unwrap();
            vm.restore_memory_regions(regions, &state, holes, reserved).unwrap();
        };
        let logged =
            |vm: &Vm, slot: u32, len: u64| vm.fd().get_dirty_log(slot, u64_to_usize(len)).is_ok();

        // No holes: the same single logged KVM slot per slot as without the option.
        let (_, mut vm) = setup_vm();
        restore(&mut vm, &[], 0);
        let dram = vm.guest_memory().iter().next().unwrap();
        assert!(dram.sub_slots.is_empty());
        assert!(logged(&vm, 0, mib(64)) && logged(&vm, 1, mib(128)));

        // A DRAM hole gets its own unlogged KVM slot; the hotplug region is all hole.
        let (_, mut vm) = setup_vm();
        restore(&mut vm, &[(mib(8), mib(16)), (mib(64), mib(320))], 0);
        let dram = vm.guest_memory().iter().next().unwrap();
        let ids: Vec<_> = dram.sub_slots.iter().map(|s| (s.slot, s.logged)).collect();
        assert_eq!(ids, vec![(0, true), (3, false), (4, true)]);
        assert!(logged(&vm, 0, mib(8)) && !logged(&vm, 3, mib(8)) && logged(&vm, 4, mib(48)));
        assert!(!logged(&vm, 1, mib(128)));
        let bitmap = vm.get_dirty_bitmap().unwrap();
        assert_eq!(
            bitmap[&0].len(),
            u64_to_usize(mib(64)) / host_page_size() / 64
        );

        // Unlogged across virtio-mem unplug and plug.
        let hp = vm.guest_memory().iter().nth(1).unwrap();
        hp.update_slot(&vm, &hp.mem_slot(2), true).unwrap();
        assert!(!logged(&vm, 2, mib(128)));
        hp.update_slot(&vm, &hp.mem_slot(1), false).unwrap();
        hp.update_slot(&vm, &hp.mem_slot(1), true).unwrap();
        assert!(!logged(&vm, 1, mib(128)));

        // Without free KVM slots for the split, DRAM stays one logged slot.
        let (_, mut vm) = setup_vm();
        vm.common.max_memslots = 4;
        restore(&mut vm, &[(mib(8), mib(16)), (mib(64), mib(320))], 0);
        let dram = vm.guest_memory().iter().next().unwrap();
        assert!(dram.sub_slots.is_empty());
        assert!(logged(&vm, 0, mib(64)) && !logged(&vm, 1, mib(128)));

        // A slot a restored device needs is never spent on the split.
        let (_, mut vm) = setup_vm();
        vm.common.max_memslots = 5;
        restore(&mut vm, &[(mib(8), mib(16)), (mib(64), mib(320))], 1);
        let dram = vm.guest_memory().iter().next().unwrap();
        assert!(dram.sub_slots.is_empty());
        assert_eq!(vm.next_kvm_slot(1), Some(3));
    }

    // Auxiliary function being used throughout the tests.
    pub(crate) fn setup_vm() -> (Kvm, Vm) {
        let kvm = Kvm::new(vec![]).expect("Cannot create Kvm");
        let vm = Vm::new(&kvm).expect("Cannot create new vm");
        (kvm, vm)
    }

    // Auxiliary function being used throughout the tests.
    pub(crate) fn setup_vm_with_memory(mem_size: usize) -> (Kvm, Vm) {
        let (kvm, mut vm) = setup_vm();
        let gm = single_region_mem_raw(mem_size);
        vm.register_dram_memory_regions(gm).unwrap();
        (kvm, vm)
    }

    #[test]
    fn test_new() {
        // Testing with a valid /dev/kvm descriptor.
        let kvm = Kvm::new(vec![]).expect("Cannot create Kvm");
        Vm::new(&kvm).unwrap();
    }

    #[test]
    fn test_register_memory_regions() {
        let (_, mut vm) = setup_vm();

        // Trying to set a memory region with a size that is not a multiple of GUEST_PAGE_SIZE
        // will result in error.
        let gm = single_region_mem_raw(0x10);
        let res = vm.register_dram_memory_regions(gm);
        assert_eq!(
            res.unwrap_err().to_string(),
            "Cannot set the memory regions: Invalid argument (os error 22)"
        );

        let gm = single_region_mem_raw(0x1000);
        let res = vm.register_dram_memory_regions(gm);
        res.unwrap();
    }

    #[test]
    fn test_too_many_regions() {
        let (kvm, mut vm) = setup_vm();
        let max_nr_regions = kvm.max_nr_memslots();

        // SAFETY: valid mmap parameters
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                0x1000,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };

        assert_ne!(ptr, libc::MAP_FAILED);

        for i in 0..=max_nr_regions {
            // SAFETY: we assert above that the ptr is valid, and the size matches what we passed to
            // mmap
            let region = unsafe {
                MmapRegionBuilder::new(0x1000)
                    .with_raw_mmap_pointer(ptr.cast())
                    .build()
                    .unwrap()
            };

            let region = GuestRegionMmap::new(region, GuestAddress(i as u64 * 0x1000)).unwrap();

            let res = vm.register_dram_memory_regions(vec![region]);

            if max_nr_regions <= i {
                assert!(
                    matches!(res, Err(VmError::NotEnoughMemorySlots(v)) if v == max_nr_regions),
                    "{:?} at iteration {}",
                    res,
                    i
                );
            } else {
                res.unwrap_or_else(|_| {
                    panic!(
                        "to be able to insert more regions in iteration {i} - max_nr_memslots: \
                         {max_nr_regions} - num_regions: {}",
                        vm.guest_memory().num_regions()
                    )
                });
            }
        }
    }

    #[test]
    fn test_snapshot_holes_discarded_pages_the_guest_left_zero() {
        use std::io::{Read, Seek, SeekFrom, Write};
        use std::os::unix::fs::MetadataExt;

        use vmm_sys_util::tempfile::TempFile;

        use crate::vmm_config::machine_config::HugePageConfig;
        use crate::vstate::memory::{Bytes, GuestMemoryExtension, anonymous};

        let ps = host_page_size();
        let size = mib_to_bytes(1);
        let page = |i: usize| GuestAddress((i * ps) as u64);
        for snapshot_type in [SnapshotType::Diff, SnapshotType::Full] {
            let (_kvm, mut vm) = setup_vm();
            let regions = anonymous(
                [(GuestAddress(0), size)].into_iter(),
                true,
                HugePageConfig::None,
            )
            .unwrap();
            vm.register_dram_memory_regions(regions).unwrap();
            let mem = vm.guest_memory();

            // An earlier image of the same size, so both dumps write into its contents.
            let tmp = TempFile::new().unwrap();
            tmp.as_file().write_all(&vec![0xaa; size]).unwrap();
            tmp.as_file().sync_all().unwrap();

            // Pages 1-3 are given back after being written: page 1 is left alone, page 2 is
            // written again, page 3 is written again with zeros.
            for i in 0..4 {
                mem.write(&vec![1u8; ps], page(i)).unwrap();
            }
            mem.discard_free_range(page(1), ps * 3).unwrap();
            mem.write(&vec![2u8; ps], page(2)).unwrap();
            mem.write(&vec![0u8; ps], page(3)).unwrap();

            let dump = vm
                .snapshot_memory_to_file(
                    tmp.as_path(),
                    snapshot_type,
                    &[],
                    false,
                    Some(&mem.describe()),
                    false,
                )
                .unwrap();
            if let Some(dump) = dump {
                // Holed pages count as written, so a layered restore reads them as zeros.
                assert!(dump.merged);
                assert_eq!(dump.dumped_pages[0] & 0b1111, 0b1111);
            }

            let mut file = tmp.as_file();
            let mut back = vec![0u8; size];
            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_exact(&mut back).unwrap();
            let pages: Vec<&[u8]> = back.chunks(ps).collect();
            assert!(pages[0].iter().all(|&b| b == 1));
            assert!(pages[1].iter().all(|&b| b == 0));
            assert!(pages[2].iter().all(|&b| b == 2));
            assert!(pages[3].iter().all(|&b| b == 0));
            // A diff leaves the rest of the earlier image; a full image writes zeros.
            let rest = if snapshot_type == SnapshotType::Diff { 0xaa } else { 0 };
            assert!(pages[4..].iter().all(|p| p.iter().all(|&b| b == rest)));
            // Pages 1 and 3 are holes.
            assert_eq!(file.metadata().unwrap().blocks() * 512, (size - 2 * ps) as u64);
        }
    }

    #[test]
    fn untrusted_balloon_merge_rebuilds_old_zero_holes_and_inherited_pages() {
        use std::os::unix::fs::FileExt;

        use vmm_sys_util::tempfile::TempFile;

        use crate::vmm_config::machine_config::HugePageConfig;
        use crate::vstate::memory::{Bytes, anonymous};

        let (_kvm, mut vm) = setup_vm();
        let size = mib_to_bytes(1);
        let ps = host_page_size();
        let regions = anonymous(
            [(GuestAddress(0), size)].into_iter(),
            true,
            HugePageConfig::None,
        )
        .unwrap();
        vm.register_dram_memory_regions(regions).unwrap();
        let mem = vm.guest_memory();
        // The target lost its presence map. Page 0 was an authoritative zero in the
        // overlay, while page 1 was inherited. Neither has been dirtied since restore.
        let target = TempFile::new().unwrap();
        target.as_file().set_len(size as u64).unwrap();
        mem.write(&vec![0x5a; ps], GuestAddress(ps as u64)).unwrap();
        mem.reset_dirty();
        let diff = vm
            .snapshot_memory_to_file(
                target.as_path(),
                SnapshotType::Diff,
                &[],
                false,
                Some(&mem.describe()),
                true,
            )
            .unwrap();
        assert!(diff.is_none()); // derive_presence marks the entire image authoritative.
        let mut data = vec![0; 2 * ps];
        target.as_file().read_exact_at(&mut data, 0).unwrap();
        assert!(data[..ps].iter().all(|&byte| byte == 0));
        assert!(data[ps..].iter().all(|&byte| byte == 0x5a));
    }

    thread_local! {
        /// Fails this thread's next memory save after its dump, as a failed flush would.
        pub(super) static FAIL_AFTER_DUMP: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    #[test]
    fn test_failed_diff_save_keeps_discarded_pages_rewritten_since() {
        use vmm_sys_util::tempfile::TempFile;

        use crate::vmm_config::machine_config::HugePageConfig;
        use crate::vstate::memory::{Bytes, GuestMemoryExtension, anonymous};

        let ps = host_page_size();
        let size = mib_to_bytes(1);
        let (_kvm, mut vm) = setup_vm();
        let regions = anonymous(
            [(GuestAddress(0), size)].into_iter(),
            true,
            HugePageConfig::None,
        )
        .unwrap();
        vm.register_dram_memory_regions(regions).unwrap();
        let mem = vm.guest_memory();
        let tmp = TempFile::new().unwrap();
        tmp.as_file().set_len(size as u64).unwrap();

        // Page 1 is given back, then written again.
        mem.discard_free_range(GuestAddress(ps as u64), ps).unwrap();
        mem.write(&vec![2u8; ps], GuestAddress(ps as u64)).unwrap();
        let save = || {
            vm.snapshot_memory_to_file(
                tmp.as_path(),
                SnapshotType::Diff,
                &[],
                false,
                Some(&mem.describe()),
                false,
            )
        };

        // The dump spends the dirty bits, then the save fails; the retry holes page 1 again.
        FAIL_AFTER_DUMP.set(true);
        save().unwrap_err();
        save().unwrap();

        let mut back = vec![0u8; ps];
        std::os::unix::fs::FileExt::read_exact_at(tmp.as_file(), &mut back, ps as u64).unwrap();
        assert_eq!(back, vec![2u8; ps]);
    }

    #[test]
    fn test_create_vcpus() {
        let vcpu_count = 2;
        let (_, mut vm) = setup_vm_with_memory(mib_to_bytes(128));

        let (vcpu_vec, _) = vm.create_vcpus(vcpu_count).unwrap();

        assert_eq!(vcpu_vec.len(), vcpu_count as usize);
    }

    fn enable_irqchip(vm: &mut Vm) {
        #[cfg(target_arch = "x86_64")]
        vm.setup_irqchip().unwrap();
        #[cfg(target_arch = "aarch64")]
        vm.setup_irqchip(1).unwrap();
    }

    fn create_msix_group(vm: &Arc<Vm>) -> MsixVectorGroup {
        Vm::create_msix_group(vm.clone(), 4).unwrap()
    }

    #[test]
    fn test_msi_vector_group_new() {
        let (_, vm) = setup_vm_with_memory(mib_to_bytes(128));
        let vm = Arc::new(vm);
        let msix_group = create_msix_group(&vm);
        assert_eq!(msix_group.num_vectors(), 4);
    }

    #[test]
    fn test_msi_vector_group_enable_disable() {
        let (_, mut vm) = setup_vm_with_memory(mib_to_bytes(128));
        enable_irqchip(&mut vm);
        let vm = Arc::new(vm);
        let msix_group = create_msix_group(&vm);

        // Initially all vectors are disabled
        for route in &msix_group.vectors {
            assert!(!route.enabled.load(Ordering::Acquire))
        }

        // Enable works
        msix_group.enable().unwrap();
        for route in &msix_group.vectors {
            assert!(route.enabled.load(Ordering::Acquire));
        }
        // Enabling an enabled group doesn't error out
        msix_group.enable().unwrap();

        // Disable works
        msix_group.disable().unwrap();
        for route in &msix_group.vectors {
            assert!(!route.enabled.load(Ordering::Acquire))
        }
        // Disabling a disabled group doesn't error out
    }

    #[test]
    fn test_msi_vector_group_trigger() {
        let (_, mut vm) = setup_vm_with_memory(mib_to_bytes(128));
        enable_irqchip(&mut vm);

        let vm = Arc::new(vm);
        let msix_group = create_msix_group(&vm);

        // We can now trigger all vectors
        for i in 0..4 {
            msix_group.trigger(i).unwrap()
        }

        // We can't trigger an invalid vector
        msix_group.trigger(4).unwrap_err();
    }

    #[test]
    fn test_msi_vector_group_notifier() {
        let (_, vm) = setup_vm_with_memory(mib_to_bytes(128));
        let vm = Arc::new(vm);
        let msix_group = create_msix_group(&vm);

        for i in 0..4 {
            assert!(msix_group.notifier(i).is_some());
        }

        assert!(msix_group.notifier(4).is_none());
    }

    #[test]
    fn test_msi_vector_group_update_invalid_vector() {
        let (_, mut vm) = setup_vm_with_memory(mib_to_bytes(128));
        enable_irqchip(&mut vm);
        let vm = Arc::new(vm);
        let msix_group = create_msix_group(&vm);
        let config = MsixVectorConfig {
            high_addr: 0x42,
            low_addr: 0x12,
            data: 0x12,
            devid: 0xafa,
        };
        msix_group.update(0, config, true, true).unwrap();
        msix_group.update(4, config, true, true).unwrap_err();
    }

    #[test]
    fn test_msi_vector_group_update() {
        let (_, mut vm) = setup_vm_with_memory(mib_to_bytes(128));
        enable_irqchip(&mut vm);
        let vm = Arc::new(vm);
        assert!(vm.common.interrupts.lock().unwrap().is_empty());
        let msix_group = create_msix_group(&vm);

        // Set some configuration for the vectors. Initially all are masked
        let mut config = MsixVectorConfig {
            high_addr: 0x42,
            low_addr: 0x13,
            data: 0x12,
            devid: 0xafa,
        };
        for i in 0..4 {
            config.data = 0x12 * i;
            msix_group.update(i as usize, config, true, false).unwrap();
        }

        // All vectors should be disabled
        for vector in &msix_group.vectors {
            assert!(!vector.enabled.load(Ordering::Acquire));
        }

        for i in 0..4 {
            let gsi = crate::arch::GSI_MSI_START + i;
            let interrupts = vm.common.interrupts.lock().unwrap();
            let kvm_route = interrupts.get(&gsi).unwrap();
            assert!(kvm_route.masked);
            assert_eq!(kvm_route.entry.gsi, gsi);
            assert_eq!(kvm_route.entry.type_, KVM_IRQ_ROUTING_MSI);
            // SAFETY: because we know we setup MSI routes.
            unsafe {
                assert_eq!(kvm_route.entry.u.msi.address_hi, 0x42);
                assert_eq!(kvm_route.entry.u.msi.address_lo, 0x13);
                assert_eq!(kvm_route.entry.u.msi.data, 0x12 * i);
            }
        }

        // Simply enabling the vectors should not update the registered IRQ routes
        msix_group.enable().unwrap();
        for i in 0..4 {
            let gsi = crate::arch::GSI_MSI_START + i;
            let interrupts = vm.common.interrupts.lock().unwrap();
            let kvm_route = interrupts.get(&gsi).unwrap();
            assert!(kvm_route.masked);
            assert_eq!(kvm_route.entry.gsi, gsi);
            assert_eq!(kvm_route.entry.type_, KVM_IRQ_ROUTING_MSI);
            // SAFETY: because we know we setup MSI routes.
            unsafe {
                assert_eq!(kvm_route.entry.u.msi.address_hi, 0x42);
                assert_eq!(kvm_route.entry.u.msi.address_lo, 0x13);
                assert_eq!(kvm_route.entry.u.msi.data, 0x12 * i);
            }
        }

        // Updating the config of a vector should enable its route (and only its route)
        config.data = 0;
        msix_group.update(0, config, false, true).unwrap();
        for i in 0..4 {
            let gsi = crate::arch::GSI_MSI_START + i;
            let interrupts = vm.common.interrupts.lock().unwrap();
            let kvm_route = interrupts.get(&gsi).unwrap();
            assert_eq!(kvm_route.masked, i != 0);
            assert_eq!(kvm_route.entry.gsi, gsi);
            assert_eq!(kvm_route.entry.type_, KVM_IRQ_ROUTING_MSI);
            // SAFETY: because we know we setup MSI routes.
            unsafe {
                assert_eq!(kvm_route.entry.u.msi.address_hi, 0x42);
                assert_eq!(kvm_route.entry.u.msi.address_lo, 0x13);
                assert_eq!(kvm_route.entry.u.msi.data, 0x12 * i);
            }
        }
    }

    #[test]
    fn test_msi_vector_group_persistence() {
        let (_, mut vm) = setup_vm_with_memory(mib_to_bytes(128));
        enable_irqchip(&mut vm);
        let vm = Arc::new(vm);
        let msix_group = create_msix_group(&vm);

        msix_group.enable().unwrap();
        let state = msix_group.save();
        let restored_group = MsixVectorGroup::restore(vm, &state).unwrap();

        assert_eq!(msix_group.num_vectors(), restored_group.num_vectors());
        // Even if an MSI group is enabled, we don't save it as such. During restoration, the PCI
        // transport will make sure the correct config is set for the vectors and enable them
        // accordingly.
        for (id, vector) in msix_group.vectors.iter().enumerate() {
            let new_vector = &restored_group.vectors[id];
            assert_eq!(vector.gsi, new_vector.gsi);
            assert!(!new_vector.enabled.load(Ordering::Acquire));
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_restore_state_resource_allocator() {
        use vm_allocator::AllocPolicy;

        let (_, mut vm) = setup_vm_with_memory(0x1000);
        vm.setup_irqchip().unwrap();

        // Allocate a GSI and some memory and make sure they are still allocated after restore
        let (gsi, range) = {
            let mut resource_allocator = vm.resource_allocator();

            let gsi = resource_allocator.allocate_gsi_msi(1).unwrap()[0];
            let range = resource_allocator
                .allocate_32bit_mmio_memory(1024, 1024, AllocPolicy::FirstMatch)
                .unwrap();
            (gsi, range)
        };

        let state = vm.save_state().unwrap();
        let serialized_data = bitcode::serialize(&state).unwrap();

        let restored_state: VmState = bitcode::deserialize(&serialized_data).unwrap();
        vm.restore_state(&restored_state, None).unwrap();

        let mut resource_allocator = vm.resource_allocator();
        let gsi_new = resource_allocator.allocate_gsi_msi(1).unwrap()[0];
        assert_eq!(gsi + 1, gsi_new);

        resource_allocator
            .allocate_32bit_mmio_memory(1024, 1024, AllocPolicy::ExactMatch(range))
            .unwrap_err();
        let range_new = resource_allocator
            .allocate_32bit_mmio_memory(1024, 1024, AllocPolicy::FirstMatch)
            .unwrap();
        assert_eq!(range + 1024, range_new);
    }
}
