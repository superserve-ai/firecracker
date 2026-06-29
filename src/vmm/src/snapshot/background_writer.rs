// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Background memory-snapshot writer.
//!
//! Runs a diff snapshot's dirty-page dump on a dedicated thread so the create call
//! can return at the snapshot point while the write + fsync happen off the critical
//! path. A generic harness: the caller supplies the dump closure and is responsible
//! for keeping the source memory stable while it runs.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The write is running.
pub const STATUS_WRITING: u8 = 1;
/// The write finished successfully.
pub const STATUS_COMPLETE: u8 = 2;
/// The write failed.
pub const STATUS_ERROR: u8 = 3;

/// Errors from the background snapshot writer.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BackgroundWriteError {
    /// failed to spawn the background snapshot writer: {0}
    SpawnThread(String),
    /// background memory write failed: {0}
    Write(String),
    /// background snapshot writer thread panicked
    ThreadPanicked,
    /// background snapshot writer did not finish within the timeout
    Timeout,
}

/// Handle to a background thread writing a diff snapshot's dirty pages.
#[derive(Debug)]
pub struct BackgroundMemoryWriter {
    handle: Option<JoinHandle<Result<(), BackgroundWriteError>>>,
    status: Arc<AtomicU8>,
}

impl BackgroundMemoryWriter {
    /// Spawn a writer thread running `dump`. `dump` must own everything it needs
    /// (a cloned, Arc-backed guest-memory handle, the frozen dirty bitmap, and the
    /// already-sized output file) and perform the write + fsync itself.
    pub fn start<F>(dump: F) -> Result<Self, BackgroundWriteError>
    where
        F: FnOnce() -> Result<(), BackgroundWriteError> + Send + 'static,
    {
        let status = Arc::new(AtomicU8::new(STATUS_WRITING));
        let status_thread = Arc::clone(&status);
        let handle = std::thread::Builder::new()
            .name("fc_snapshot_writer".to_string())
            .spawn(move || {
                let result = dump();
                status_thread.store(
                    if result.is_ok() { STATUS_COMPLETE } else { STATUS_ERROR },
                    Ordering::Release,
                );
                result
            })
            .map_err(|e| BackgroundWriteError::SpawnThread(e.to_string()))?;
        Ok(Self {
            handle: Some(handle),
            status,
        })
    }

    /// Current write status (one of the `STATUS_*` constants).
    pub fn status(&self) -> u8 {
        self.status.load(Ordering::Acquire)
    }

    /// Block until the writer finishes or `timeout` elapses. On success the thread
    /// is joined and its result propagated. On timeout the thread is left running
    /// (it cannot be safely killed mid-write); the caller treats this as a failure.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Result<(), BackgroundWriteError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.status.load(Ordering::Acquire) {
                STATUS_COMPLETE | STATUS_ERROR => {
                    return match self.handle.take() {
                        Some(h) => h.join().map_err(|_| BackgroundWriteError::ThreadPanicked)?,
                        None => Err(BackgroundWriteError::ThreadPanicked),
                    };
                }
                _ => {}
            }
            if self.handle.is_none() {
                return Err(BackgroundWriteError::ThreadPanicked);
            }
            if Instant::now() >= deadline {
                return Err(BackgroundWriteError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
