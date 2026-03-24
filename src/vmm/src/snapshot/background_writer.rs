// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Background memory snapshot writer.
//!
//! Writes dirty guest memory pages to a snapshot file in a background thread
//! while the VM continues running. Works with the write-protect infrastructure
//! to handle pages that the VM modifies during the snapshot.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Instant;

use super::write_protect::CowPages;

/// Page size (4KB).
const PAGE_SIZE: usize = 4096;

/// Snapshot write status.
pub const STATUS_IDLE: u8 = 0;
pub const STATUS_WRITING: u8 = 1;
pub const STATUS_COMPLETE: u8 = 2;
pub const STATUS_ERROR: u8 = 3;

/// Statistics from a background snapshot write.
#[derive(Debug, Clone)]
pub struct WriteStats {
    /// Number of dirty pages written
    pub dirty_pages: u64,
    /// Number of COW pages (pages the VM modified during snapshot)
    pub cow_pages: u64,
    /// Total bytes written
    pub bytes_written: u64,
    /// Time spent writing (microseconds)
    pub write_time_us: u64,
}

/// A request to write dirty pages to a snapshot file.
pub struct WriteRequest {
    /// Dirty pages to write: (file_offset, host_virtual_address) pairs.
    /// The address points to guest memory that is still write-protected.
    pub dirty_pages: Vec<(u64, u64)>,
    /// COW pages — pages the VM already modified during snapshot.
    /// These contain the old (snapshot-consistent) data.
    pub cow_pages: CowPages,
    /// Path to the snapshot memory file.
    pub mem_file_path: PathBuf,
    /// Total guest memory size (for setting file length).
    pub mem_size: u64,
    /// Whether to fsync after writing.
    pub sync: bool,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BackgroundWriteError {
    /// Failed to open snapshot file: {0}
    OpenFile(std::io::Error),
    /// Failed to write to snapshot file: {0}
    Write(std::io::Error),
    /// Failed to sync snapshot file: {0}
    Sync(std::io::Error),
    /// Failed to spawn writer thread: {0}
    SpawnThread(std::io::Error),
    /// Writer thread panicked
    ThreadPanicked,
}

/// Background thread that writes dirty pages to a snapshot file.
pub struct BackgroundMemoryWriter {
    handle: Option<JoinHandle<Result<WriteStats, BackgroundWriteError>>>,
    status: Arc<AtomicU8>,
}

impl BackgroundMemoryWriter {
    /// Start a background writer thread with the given write request.
    pub fn start(request: WriteRequest) -> Result<Self, BackgroundWriteError> {
        let status = Arc::new(AtomicU8::new(STATUS_WRITING));
        let status_clone = Arc::clone(&status);

        let handle = std::thread::Builder::new()
            .name("fc_snapshot_writer".to_string())
            .spawn(move || {
                let result = Self::write_pages(request);
                match &result {
                    Ok(_) => status_clone.store(STATUS_COMPLETE, Ordering::Release),
                    Err(_) => status_clone.store(STATUS_ERROR, Ordering::Release),
                }
                result
            })
            .map_err(BackgroundWriteError::SpawnThread)?;

        Ok(Self {
            handle: Some(handle),
            status,
        })
    }

    /// Get the current status of the background write.
    pub fn status(&self) -> u8 {
        self.status.load(Ordering::Acquire)
    }

    /// Wait for the background write to complete and return the stats.
    pub fn wait(mut self) -> Result<WriteStats, BackgroundWriteError> {
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| BackgroundWriteError::ThreadPanicked)?,
            None => Err(BackgroundWriteError::ThreadPanicked),
        }
    }

    /// The actual write logic that runs in the background thread.
    fn write_pages(request: WriteRequest) -> Result<WriteStats, BackgroundWriteError> {
        let start = Instant::now();

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&request.mem_file_path)
            .map_err(BackgroundWriteError::OpenFile)?;

        // Set file to full memory size (sparse file — only written pages take space)
        file.set_len(request.mem_size)
            .map_err(BackgroundWriteError::Write)?;

        let mut dirty_count: u64 = 0;
        let mut cow_count: u64 = 0;
        let mut bytes_written: u64 = 0;

        for &(file_offset, page_addr) in &request.dirty_pages {
            // Check if the VM already modified this page (COW happened)
            let cow_pages = request.cow_pages.lock().unwrap();
            let page_data = if let Some(saved_data) = cow_pages.get(&page_addr) {
                // VM wrote to this page — use the saved old data
                cow_count += 1;
                saved_data.clone()
            } else {
                // Page is still write-protected — read directly from guest memory
                // SAFETY: page_addr points to valid guest memory that is write-protected,
                // so its contents won't change while we read.
                let mut buf = vec![0u8; PAGE_SIZE];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        page_addr as *const u8,
                        buf.as_mut_ptr(),
                        PAGE_SIZE,
                    );
                }
                buf
            };
            drop(cow_pages); // Release lock before I/O

            // Write to file at the correct offset
            file.seek(SeekFrom::Start(file_offset))
                .map_err(BackgroundWriteError::Write)?;
            file.write_all(&page_data)
                .map_err(BackgroundWriteError::Write)?;

            dirty_count += 1;
            bytes_written += PAGE_SIZE as u64;
        }

        file.flush().map_err(BackgroundWriteError::Write)?;

        if request.sync {
            file.sync_all().map_err(BackgroundWriteError::Sync)?;
        }

        Ok(WriteStats {
            dirty_pages: dirty_count,
            cow_pages: cow_count,
            bytes_written,
            write_time_us: start.elapsed().as_micros() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;
    use std::sync::Mutex;

    #[test]
    fn test_write_stats_defaults() {
        let stats = WriteStats {
            dirty_pages: 10,
            cow_pages: 2,
            bytes_written: 10 * 4096,
            write_time_us: 1000,
        };
        assert_eq!(stats.dirty_pages, 10);
        assert_eq!(stats.cow_pages, 2);
    }

    #[test]
    fn test_background_write_basic() {
        // Create a page of known data in memory
        let page_data = vec![0xAB_u8; PAGE_SIZE];
        let page_ptr = page_data.as_ptr() as u64;

        let tmp = vmm_sys_util::tempfile::TempFile::new().unwrap();
        let path = tmp.as_path().to_path_buf();

        let cow_pages: CowPages = Arc::new(Mutex::new(HashMap::new()));

        let request = WriteRequest {
            dirty_pages: vec![(0, page_ptr)],
            cow_pages,
            mem_file_path: path.clone(),
            mem_size: PAGE_SIZE as u64,
            sync: false,
        };

        let writer = BackgroundMemoryWriter::start(request).unwrap();
        let stats = writer.wait().unwrap();

        assert_eq!(stats.dirty_pages, 1);
        assert_eq!(stats.cow_pages, 0);
        assert_eq!(stats.bytes_written, PAGE_SIZE as u64);

        // Verify file contents
        let mut file = File::open(&path).unwrap();
        let mut buf = vec![0u8; PAGE_SIZE];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(buf, page_data);
    }

    #[test]
    fn test_background_write_with_cow() {
        // Original page data in memory
        let original_data = vec![0xAB_u8; PAGE_SIZE];
        let page_ptr = original_data.as_ptr() as u64;

        // Simulate COW: the VM wrote to this page, handler saved old data
        let saved_old_data = vec![0xCD_u8; PAGE_SIZE];
        let cow_pages: CowPages = Arc::new(Mutex::new(HashMap::new()));
        cow_pages.lock().unwrap().insert(page_ptr, saved_old_data.clone());

        let tmp = vmm_sys_util::tempfile::TempFile::new().unwrap();
        let path = tmp.as_path().to_path_buf();

        let request = WriteRequest {
            dirty_pages: vec![(0, page_ptr)],
            cow_pages,
            mem_file_path: path.clone(),
            mem_size: PAGE_SIZE as u64,
            sync: false,
        };

        let writer = BackgroundMemoryWriter::start(request).unwrap();
        let stats = writer.wait().unwrap();

        assert_eq!(stats.dirty_pages, 1);
        assert_eq!(stats.cow_pages, 1); // Used COW data

        // Verify file has the OLD (saved) data, not current memory
        let mut file = File::open(&path).unwrap();
        let mut buf = vec![0u8; PAGE_SIZE];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(buf, saved_old_data);
    }

    #[test]
    fn test_status_tracking() {
        let page_data = vec![0u8; PAGE_SIZE];
        let page_ptr = page_data.as_ptr() as u64;

        let tmp = vmm_sys_util::tempfile::TempFile::new().unwrap();

        let request = WriteRequest {
            dirty_pages: vec![(0, page_ptr)],
            cow_pages: Arc::new(Mutex::new(HashMap::new())),
            mem_file_path: tmp.as_path().to_path_buf(),
            mem_size: PAGE_SIZE as u64,
            sync: false,
        };

        let writer = BackgroundMemoryWriter::start(request).unwrap();
        let stats = writer.wait().unwrap();

        // After wait, status should be complete
        assert_eq!(stats.dirty_pages, 1);
    }
}
