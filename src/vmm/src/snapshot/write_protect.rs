// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Write-protect based COW (copy-on-write) for async memory snapshots.
//!
//! Uses userfaultfd's write-protect mode to protect guest memory pages during
//! snapshot creation. The VM resumes immediately while a background thread
//! writes protected pages to disk. If the VM writes to a protected page, the
//! uffd handler saves the old data before unprotecting the page.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use userfaultfd::{Event, RegisterMode, Uffd, UffdBuilder};

/// Size of a memory page (4KB).
const PAGE_SIZE: usize = 4096;

/// Maximum number of COW pages to store. If the VM writes to more pages
/// than this during snapshot, additional writes proceed without saving
/// the old data (the snapshot will use current memory content for those pages).
/// 256K pages = 1GB — prevents OOM even under heavy write load.
const MAX_COW_PAGES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum WriteProtectError {
    /// Failed to create userfaultfd: {0}
    UffdCreate(userfaultfd::Error),
    /// Failed to register memory with uffd: {0}
    UffdRegister(userfaultfd::Error),
    /// Failed to write-protect pages: {0}
    WriteProtect(userfaultfd::Error),
    /// Failed to remove write protection: {0}
    RemoveProtection(userfaultfd::Error),
    /// Failed to read uffd event: {0}
    ReadEvent(userfaultfd::Error),
    /// Handler thread panicked
    HandlerPanicked,
}

/// Saved page data from COW faults — pages the VM wrote to during snapshot.
/// Key: page-aligned host virtual address. Value: old page data before the write.
pub type CowPages = Arc<Mutex<HashMap<u64, Vec<u8>>>>;

/// Manages userfaultfd write-protection for async snapshots.
///
/// Lifecycle:
/// 1. `new()` — create uffd and register guest memory regions for write-protect
/// 2. `protect()` — write-protect a range of pages
/// 3. `start_handler()` — spawn thread to handle write faults (saves old data, unprotects)
/// 4. `finish()` — unprotect remaining pages and stop handler
pub struct SnapshotWriteProtect {
    uffd: Arc<Uffd>,
    cow_pages: CowPages,
    handler_thread: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    /// Tracks which ranges are currently write-protected
    protected_ranges: Vec<(u64, u64)>, // (start_addr, len)
}

impl SnapshotWriteProtect {
    /// Create a new uffd and register memory regions for write-protect tracking.
    ///
    /// `regions` is a list of (host_virtual_address, size) pairs representing
    /// guest memory regions.
    pub fn new(regions: &[(u64, usize)]) -> Result<Self, WriteProtectError> {
        let uffd = UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false) // blocking reads for the handler thread
            .user_mode_only(false)
            .create()
            .map_err(WriteProtectError::UffdCreate)?;

        // Register each memory region for write-protect monitoring
        for &(addr, size) in regions {
            uffd.register_with_mode(
                addr as *mut std::ffi::c_void,
                size,
                RegisterMode::WRITE_PROTECT,
            )
            .map_err(WriteProtectError::UffdRegister)?;
        }

        Ok(Self {
            uffd: Arc::new(uffd),
            cow_pages: Arc::new(Mutex::new(HashMap::new())),
            handler_thread: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            protected_ranges: Vec::new(),
        })
    }

    /// Write-protect a range of memory pages.
    /// Any write to this range will trigger a uffd event.
    pub fn protect(&mut self, addr: u64, len: u64) -> Result<(), WriteProtectError> {
        self.uffd
            .write_protect(addr as *mut std::ffi::c_void, len as usize)
            .map_err(WriteProtectError::WriteProtect)?;

        self.protected_ranges.push((addr, len));
        Ok(())
    }

    /// Start the handler thread that catches write-protect faults.
    ///
    /// When the VM writes to a protected page:
    /// 1. The handler reads the old page data (snapshot-consistent copy)
    /// 2. Saves it in `cow_pages`
    /// 3. Removes write protection so the VM's write can proceed
    pub fn start_handler(&mut self) -> Result<(), WriteProtectError> {
        let uffd = Arc::clone(&self.uffd);
        let cow_pages = Arc::clone(&self.cow_pages);
        let stop_flag = Arc::clone(&self.stop_flag);

        let handle = std::thread::Builder::new()
            .name("fc_snapshot_wp".to_string())
            .spawn(move || {
                Self::handler_loop(&uffd, &cow_pages, &stop_flag);
            })
            .map_err(|_| WriteProtectError::HandlerPanicked)?;

        self.handler_thread = Some(handle);
        Ok(())
    }

    /// The handler loop that runs in a background thread.
    /// Reads uffd events and handles write-protect faults.
    fn handler_loop(uffd: &Uffd, cow_pages: &CowPages, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            let event = match uffd.read_event() {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(_) => {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }
            };

            match event {
                Event::Pagefault { addr, rw: _, .. } => {
                    let page_addr = (addr as u64) & !(PAGE_SIZE as u64 - 1);

                    // Read the old page data before the VM overwrites it.
                    // SAFETY: page_addr points to valid guest memory that is
                    // currently write-protected (so it won't change under us).
                    let mut page_data = vec![0u8; PAGE_SIZE];
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            page_addr as *const u8,
                            page_data.as_mut_ptr(),
                            PAGE_SIZE,
                        );
                    }

                    // Save the old data if under the cap.
                    if let Ok(mut pages) = cow_pages.lock() {
                        if pages.len() < MAX_COW_PAGES {
                            pages.insert(page_addr, page_data);
                        }
                        // Over cap: don't save — snapshot will use current memory
                        // for this page (minor inconsistency under extreme write load).
                    }

                    // Remove write protection so the VM's write can proceed
                    let _ = uffd.remove_write_protection(
                        page_addr as *mut std::ffi::c_void,
                        PAGE_SIZE,
                        true, // wake the faulting thread
                    );
                }
                _ => {
                    // Ignore other event types
                }
            }
        }
    }

    /// Get a reference to the COW pages collected by the handler.
    pub fn cow_pages(&self) -> CowPages {
        Arc::clone(&self.cow_pages)
    }

    /// Unprotect all remaining protected ranges and stop the handler thread.
    /// Returns the collected COW pages.
    pub fn finish(&mut self) -> Result<HashMap<u64, Vec<u8>>, WriteProtectError> {
        // Signal the handler to stop
        self.stop_flag.store(true, Ordering::Relaxed);

        // Unprotect all ranges so no more faults are generated
        for &(addr, len) in &self.protected_ranges {
            let _ = self.uffd.remove_write_protection(
                addr as *mut std::ffi::c_void,
                len as usize,
                true,
            );
        }
        self.protected_ranges.clear();

        // Wait for handler thread to exit
        if let Some(handle) = self.handler_thread.take() {
            let _ = handle.join();
        }

        let cow_pages = self.cow_pages.lock().unwrap().clone();
        Ok(cow_pages)
    }
}

impl Drop for SnapshotWriteProtect {
    fn drop(&mut self) {
        // Ensure the handler thread stops and pages are unprotected.
        self.stop_flag.store(true, Ordering::Relaxed);

        for &(addr, len) in &self.protected_ranges {
            let _ = self.uffd.remove_write_protection(
                addr as *mut std::ffi::c_void,
                len as usize,
                true,
            );
        }

        if let Some(handle) = self.handler_thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cow_pages_type() {
        // Basic test that CowPages can be created and shared
        let pages: CowPages = Arc::new(Mutex::new(HashMap::new()));
        let clone = Arc::clone(&pages);

        pages.lock().unwrap().insert(0x1000, vec![0xAA; PAGE_SIZE]);
        assert_eq!(clone.lock().unwrap().len(), 1);
    }
}
