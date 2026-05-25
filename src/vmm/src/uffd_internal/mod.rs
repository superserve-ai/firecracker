// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-process userfaultfd handler for snapshot restore.
//!
//! A handler thread, spawned by [`setup`] and joined when the returned [`Handler`] is
//! dropped, serves guest page faults via `UFFDIO_COPY` from a memory-mapped snapshot
//! file. The snapshot is mapped `MAP_PRIVATE` without `MAP_POPULATE` so pages stay
//! demand-paged from disk.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;

use userfaultfd::{Error as UffdCrateError, Event, FeatureFlags, Uffd, UffdBuilder};

use crate::persist::GuestRegionUffdMapping;
use crate::seccomp::{BpfProgram, apply_filter};
use crate::vmm_config::machine_config::HugePageConfig;
use crate::vstate::memory::{self, GuestMemoryState, GuestRegionMmap, MemoryError};

/// Poll timeout between shutdown-channel checks. Bounds how long a handler thread takes
/// to notice that the VM is going away.
const POLL_TIMEOUT_MS: i32 = 100;

/// Atomic counters maintained by the handler thread. Read via [`Handler::stats`] for
/// observability; not used for synchronization, hence `Ordering::Relaxed` throughout.
#[derive(Default, Debug)]
struct Stats {
    faults_served: AtomicU64,
    faults_deferred: AtomicU64,
    faults_failed_transient: AtomicU64,
    prefetch_served: AtomicU64,
    prefetch_eexist: AtomicU64,
    prefetch_eagain: AtomicU64,
    prefetch_failed: AtomicU64,
    recorded_offsets: AtomicU64,
}

/// Snapshot of [`Stats`] returned to external callers. Each field is a monotonic counter
/// of events since the handler started, except `recorded_offsets`, which is the count of
/// unique offsets currently held by the in-memory recorder (template-build mode only).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Count of guest page faults the handler resolved by copying a page from the
    /// snapshot into guest memory.
    pub faults_served: u64,
    /// Count of EAGAIN-deferred page-fault attempts. A single faulting address can
    /// contribute multiple increments if its `UFFDIO_COPY` is deferred more than once.
    pub faults_deferred: u64,
    /// Count of page-fault servicing attempts that hit an unexpected ioctl error and
    /// could not be completed. Each increment is paired with an `error!` log entry.
    pub faults_failed_transient: u64,
    /// Count of prefetcher `UFFDIO_COPY` calls that completed successfully.
    pub prefetch_served: u64,
    /// Count of prefetcher copies skipped because the page had already been faulted in
    /// by an on-demand handler call (an expected race; benign).
    pub prefetch_eexist: u64,
    /// Count of prefetcher copies that returned EAGAIN because a REMOVE event was
    /// queued ahead of them.
    pub prefetch_eagain: u64,
    /// Count of prefetcher copies that hit an unexpected error; each increment is paired
    /// with a `warn!` log entry.
    pub prefetch_failed: u64,
    /// Number of unique page offsets the in-memory recorder is currently holding
    /// (template-build mode only; zero otherwise).
    pub recorded_offsets: u64,
}

impl Stats {
    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            faults_served: self.faults_served.load(Ordering::Relaxed),
            faults_deferred: self.faults_deferred.load(Ordering::Relaxed),
            faults_failed_transient: self.faults_failed_transient.load(Ordering::Relaxed),
            prefetch_served: self.prefetch_served.load(Ordering::Relaxed),
            prefetch_eexist: self.prefetch_eexist.load(Ordering::Relaxed),
            prefetch_eagain: self.prefetch_eagain.load(Ordering::Relaxed),
            prefetch_failed: self.prefetch_failed.load(Ordering::Relaxed),
            recorded_offsets: self.recorded_offsets.load(Ordering::Relaxed),
        }
    }
}

/// Configuration for an internal-UFFD-backed restore.
#[derive(Clone, Debug)]
pub struct Config {
    /// Snapshot memory file backing guest RAM.
    pub snapshot_path: PathBuf,
    /// Recorded page-access trace replayed as prefetch when present.
    pub access_log_path: Option<PathBuf>,
    /// When set, the handler records each served page offset and suppresses prefetch.
    pub record_to: Option<PathBuf>,
}

/// Owning handle for a handler thread. Drop signals shutdown and joins the thread.
pub struct Handler {
    shutdown_tx: mpsc::Sender<()>,
    drain_tx: mpsc::SyncSender<mpsc::SyncSender<()>>,
    stats: Arc<Stats>,
    thread: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler")
            .field("running", &self.thread.is_some())
            .finish()
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Handler {
    /// Block until the handler thread has drained every UFFD event currently queued by the
    /// kernel. Callers must hold the VM paused so no new faults can arrive between drain
    /// and the operation that requires a stable memory view (e.g. snapshot dump).
    pub fn drain_pending(&self) -> Result<(), DrainError> {
        let (ack_tx, ack_rx) = mpsc::sync_channel::<()>(0);
        self.drain_tx
            .send(ack_tx)
            .map_err(|_| DrainError::HandlerExited)?;
        ack_rx.recv().map_err(|_| DrainError::NoAck)
    }

    /// Snapshot of the handler's counters at the time of the call.
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
}

/// Failure modes for [`Handler::drain_pending`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum DrainError {
    /// Handler thread has exited; the drain request was not delivered.
    HandlerExited,
    /// Drain request was delivered but the handler did not acknowledge completion (thread likely died mid-drain).
    NoAck,
}

/// Errors returned during setup of the in-process handler.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum InternalUffdError {
    /// Failed to allocate guest memory: {0}
    Memory(#[from] MemoryError),
    /// Failed to create userfaultfd: {0}
    Create(UffdCrateError),
    /// Failed to register memory region with userfaultfd: {0}
    Register(UffdCrateError),
    /// Failed to open or mmap snapshot file: {0}
    OpenSnapshot(std::io::Error),
    /// Failed to duplicate userfaultfd descriptor: {0}
    DupFd(std::io::Error),
    /// Failed to spawn handler thread: {0}
    SpawnThread(std::io::Error),
}

/// Allocate anonymous guest memory, create + register a userfaultfd, and start a handler
/// thread that serves page faults from `cfg.snapshot_path`.
///
/// All file I/O (opening + mmap of the snapshot) happens on the calling thread before the
/// handler is spawned, so the runtime filesystem syscalls do not need to be present in the
/// VMM seccomp allowlist that gates the handler thread.
pub fn setup(
    cfg: Config,
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
    huge_pages: HugePageConfig,
    vmm_filter: Arc<BpfProgram>,
) -> Result<(Vec<GuestRegionMmap>, Uffd, Handler), InternalUffdError> {
    let guest_memory = memory::anonymous(mem_state.regions(), track_dirty_pages, huge_pages)?;
    let page_size = huge_pages.page_size();

    let mut builder = UffdBuilder::new();
    builder.require_features(FeatureFlags::EVENT_REMOVE);
    let uffd = builder
        .close_on_exec(true)
        .non_blocking(true)
        .user_mode_only(false)
        .create()
        .map_err(InternalUffdError::Create)?;

    let mut mappings = Vec::with_capacity(guest_memory.len());
    let mut offset = 0u64;
    for region in guest_memory.iter() {
        uffd.register(region.as_ptr().cast(), region.size())
            .map_err(InternalUffdError::Register)?;
        #[allow(deprecated)]
        mappings.push(GuestRegionUffdMapping {
            base_host_virt_addr: region.as_ptr() as u64,
            size: region.size(),
            offset,
            page_size,
            page_size_kib: page_size,
        });
        offset += region.size() as u64;
    }

    let snapshot = mmap_snapshot(&cfg.snapshot_path).map_err(InternalUffdError::OpenSnapshot)?;

    // Recording disables prefetch so the captured trace reflects guest-driven access
    // order instead of pages pulled in by the prefetcher.
    let prefetch_offsets = if cfg.record_to.is_some() {
        Vec::new()
    } else {
        cfg.access_log_path
            .as_deref()
            .map(|p| load_prefetch_offsets(p, page_size))
            .unwrap_or_default()
    };
    let recorder = cfg.record_to.as_ref().map(|_| Recorder::default());

    // Duplicate the fd so the handler thread holds an independent owner. The kernel
    // UFFD registration stays alive until every refcount on the fd is closed.
    // SAFETY: `uffd` is alive for the duration of this call and exposes a valid open fd.
    let dup_fd = unsafe { libc::dup(uffd.as_raw_fd()) };
    if dup_fd < 0 {
        return Err(InternalUffdError::DupFd(std::io::Error::last_os_error()));
    }
    // SAFETY: `dup_fd` was just returned by `dup()` and is owned exclusively by the new
    // `Uffd` from this point on. No other code retains the raw value.
    let handler_uffd = unsafe { Uffd::from_raw_fd(dup_fd) };

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let (drain_tx, drain_rx) = mpsc::sync_channel::<mpsc::SyncSender<()>>(0);
    let record_to = cfg.record_to.clone();
    let stats = Arc::new(Stats::default());
    let stats_for_thread = Arc::clone(&stats);
    let thread = thread::Builder::new()
        .name("uffd-internal".into())
        .spawn(move || {
            run(
                handler_uffd,
                mappings,
                page_size,
                snapshot,
                prefetch_offsets,
                recorder,
                record_to,
                vmm_filter,
                stats_for_thread,
                shutdown_rx,
                drain_rx,
            )
        })
        .map_err(InternalUffdError::SpawnThread)?;

    Ok((
        guest_memory,
        uffd,
        Handler {
            shutdown_tx,
            drain_tx,
            stats,
            thread: Some(thread),
        },
    ))
}

struct SnapshotMmap {
    addr: *const u8,
    size: usize,
}

// SAFETY: the mapping is read-only and owned exclusively by the handler thread; no
// concurrent mutation is possible across threads.
unsafe impl Send for SnapshotMmap {}

impl Drop for SnapshotMmap {
    fn drop(&mut self) {
        if !self.addr.is_null() && self.size > 0 {
            // SAFETY: `addr` and `size` are the exact arguments returned by `mmap()` in
            // `mmap_snapshot`, this is the only owner of the mapping, and no live pointer
            // into it survives past this `drop`.
            unsafe {
                libc::munmap(self.addr as *mut _, self.size);
            }
        }
    }
}

fn mmap_snapshot(path: &Path) -> std::io::Result<SnapshotMmap> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len() as usize;
    // MAP_POPULATE intentionally omitted so pages stay demand-paged from the file.
    // SAFETY: `file` is open and its fd is valid for the duration of the mmap call;
    // `size` is the file size in bytes; PROT_READ requires no special alignment of the
    // returned address.
    let addr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(SnapshotMmap {
        addr: addr.cast(),
        size,
    })
}

#[allow(clippy::too_many_arguments)]
fn run(
    uffd: Uffd,
    mappings: Vec<GuestRegionUffdMapping>,
    page_size: usize,
    snapshot: SnapshotMmap,
    prefetch_offsets: Vec<u64>,
    mut recorder: Option<Recorder>,
    record_to: Option<PathBuf>,
    vmm_filter: Arc<BpfProgram>,
    stats: Arc<Stats>,
    shutdown_rx: mpsc::Receiver<()>,
    drain_rx: mpsc::Receiver<mpsc::SyncSender<()>>,
) {
    // Apply the same seccomp filter as the VMM thread before serving any events.
    if let Err(e) = apply_filter(vmm_filter.as_slice()) {
        log::error!("uffd-internal: failed to apply seccomp filter, exiting: {e:?}");
        return;
    }

    // Pagefault addresses that returned EAGAIN because a REMOVE event was queued ahead
    // of them; retried at the top of each iteration so the REMOVE drains first.
    let mut deferred: Vec<u64> = Vec::new();
    let mut prefetch_cursor = 0usize;
    let pollfd = libc::pollfd {
        fd: uffd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        if shutdown_rx.try_recv().is_ok() {
            // Drain anything the kernel has queued, then exit. The VM is paused before
            // snapshot save and before VM destroy, so no new faults arrive after this.
            drain_to_completion(
                &uffd,
                &mappings,
                snapshot.addr,
                page_size,
                &mut deferred,
                recorder.as_mut(),
                &stats,
            );
            if let (Some(rec), Some(path)) = (recorder, record_to.as_deref()) {
                if let Err(e) = rec.flush(path) {
                    log::error!("uffd-internal: failed to flush access log to {path:?}: {e}");
                }
            }
            return;
        }

        if let Ok(ack) = drain_rx.try_recv() {
            drain_to_completion(
                &uffd,
                &mappings,
                snapshot.addr,
                page_size,
                &mut deferred,
                recorder.as_mut(),
                &stats,
            );
            let _ = ack.send(());
        }

        // While prefetch entries remain, poll non-blocking so the loop can advance the
        // prefetcher when the kernel queue is empty. Incoming faults always preempt
        // prefetch because each iteration re-enters `poll`.
        let poll_timeout = if prefetch_cursor < prefetch_offsets.len() {
            0
        } else {
            POLL_TIMEOUT_MS
        };

        let mut pfds = [pollfd];
        // SAFETY: pfds is a single-element array on this stack frame.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, poll_timeout) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log::error!("uffd-internal: poll failed: {err}");
            return;
        }

        retry_deferred(
            &uffd,
            &mappings,
            snapshot.addr,
            page_size,
            &mut deferred,
            recorder.as_mut(),
            &stats,
        );

        if n == 0 {
            if prefetch_cursor < prefetch_offsets.len() {
                prefetch_one(
                    &uffd,
                    &mappings,
                    snapshot.addr,
                    page_size,
                    prefetch_offsets[prefetch_cursor],
                    &stats,
                );
                prefetch_cursor += 1;
            }
            continue;
        }

        loop {
            match uffd.read_event() {
                Ok(Some(ev)) => handle_event(
                    &uffd,
                    &mappings,
                    snapshot.addr,
                    page_size,
                    ev,
                    recorder.as_mut(),
                    &mut deferred,
                    &stats,
                ),
                Ok(None) => break,
                Err(UffdCrateError::SystemError(e))
                    if std::io::Error::from(e).raw_os_error() == Some(libc::EAGAIN) =>
                {
                    break;
                }
                Err(UffdCrateError::SystemError(e))
                    if std::io::Error::from(e).raw_os_error() == Some(libc::EINVAL) =>
                {
                    // EINVAL on read means firecracker has already unmapped the registered
                    // memory regions; the VM is going away. Exit cleanly.
                    return;
                }
                Err(e) => {
                    log::error!("uffd-internal: read_event failed: {e:?}");
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn retry_deferred(
    uffd: &Uffd,
    mappings: &[GuestRegionUffdMapping],
    backing: *const u8,
    page_size: usize,
    deferred: &mut Vec<u64>,
    mut recorder: Option<&mut Recorder>,
    stats: &Stats,
) {
    if deferred.is_empty() {
        return;
    }
    let mut still_deferred = Vec::with_capacity(deferred.len());
    for addr in deferred.drain(..) {
        match serve_pagefault(uffd, mappings, backing, page_size, addr) {
            ServeOutcome::Served => {
                stats.faults_served.fetch_add(1, Ordering::Relaxed);
                record_fault(recorder.as_deref_mut(), mappings, page_size, addr, stats);
            }
            ServeOutcome::Deferred => {
                stats.faults_deferred.fetch_add(1, Ordering::Relaxed);
                still_deferred.push(addr);
            }
            ServeOutcome::FailedTransient => {
                stats.faults_failed_transient.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    *deferred = still_deferred;
}

/// Drain every UFFD event the kernel currently has queued, retrying any deferred
/// entries until both the queue is empty and the deferred list has stopped shrinking.
///
/// **Pre-condition: the caller must pause guest vCPUs before invoking this.** Without
/// that invariant a guest that keeps page-faulting will keep `read_event` returning new
/// events and this loop will never terminate. The two existing call sites (shutdown and
/// snapshot-save drain) both pair this function with an external pause.
#[allow(clippy::too_many_arguments)]
fn drain_to_completion(
    uffd: &Uffd,
    mappings: &[GuestRegionUffdMapping],
    backing: *const u8,
    page_size: usize,
    deferred: &mut Vec<u64>,
    mut recorder: Option<&mut Recorder>,
    stats: &Stats,
) {
    loop {
        let prev_deferred = deferred.len();
        retry_deferred(
            uffd,
            mappings,
            backing,
            page_size,
            deferred,
            recorder.as_deref_mut(),
            stats,
        );

        let mut got_new = false;
        while let Ok(Some(ev)) = uffd.read_event() {
            handle_event(
                uffd,
                mappings,
                backing,
                page_size,
                ev,
                recorder.as_deref_mut(),
                deferred,
                stats,
            );
            got_new = true;
        }

        // Termination: stop only when both the kernel queue is empty (no new events)
        // and the deferred list has not shrunk (no retry progress was made). If either
        // condition fails, continue: a successful retry may have unblocked the kernel
        // queue, and a freshly-read REMOVE may have unblocked a deferred entry.
        if !got_new && deferred.len() >= prev_deferred {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    uffd: &Uffd,
    mappings: &[GuestRegionUffdMapping],
    backing: *const u8,
    page_size: usize,
    ev: Event,
    recorder: Option<&mut Recorder>,
    deferred: &mut Vec<u64>,
    stats: &Stats,
) {
    match ev {
        Event::Pagefault { addr, .. } => {
            let addr_u64 = addr as u64;
            match serve_pagefault(uffd, mappings, backing, page_size, addr_u64) {
                ServeOutcome::Served => {
                    stats.faults_served.fetch_add(1, Ordering::Relaxed);
                    record_fault(recorder, mappings, page_size, addr_u64, stats);
                }
                ServeOutcome::Deferred => {
                    stats.faults_deferred.fetch_add(1, Ordering::Relaxed);
                    deferred.push(addr_u64);
                }
                ServeOutcome::FailedTransient => {
                    stats.faults_failed_transient.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Event::Remove { start, end } => unregister_range(uffd, start, end, page_size),
        _ => {
            log::debug!("uffd-internal: unexpected event: {ev:?}");
        }
    }
}

/// Record the page-aligned file offset corresponding to `addr` if a recorder is active.
/// Called only after `serve_pagefault` confirms the page is resident in guest memory.
fn record_fault(
    recorder: Option<&mut Recorder>,
    mappings: &[GuestRegionUffdMapping],
    page_size: usize,
    addr: u64,
    stats: &Stats,
) {
    let Some(rec) = recorder else { return };
    let page_addr = addr & !((page_size as u64) - 1);
    if let Some(region) = mappings.iter().find(|r| {
        page_addr >= r.base_host_virt_addr && page_addr < r.base_host_virt_addr + r.size as u64
    }) {
        let offset = region.offset + (page_addr - region.base_host_virt_addr);
        if rec.record(offset) {
            stats.recorded_offsets.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Replay one entry of the recorded access trace. Outcomes are counted on `stats` so
/// EAGAIN bursts and unexpected errors are visible even though the prefetch path has no
/// caller to surface them to.
fn prefetch_one(
    uffd: &Uffd,
    mappings: &[GuestRegionUffdMapping],
    backing: *const u8,
    page_size: usize,
    offset: u64,
    stats: &Stats,
) {
    let region = match mappings
        .iter()
        .find(|r| offset >= r.offset && offset < r.offset + r.size as u64)
    {
        Some(r) => r,
        None => return,
    };
    let page_offset_in_region = (offset - region.offset) & !((page_size as u64) - 1);
    let dst = (region.base_host_virt_addr + page_offset_in_region) as *mut libc::c_void;
    // SAFETY: `region.offset + page_offset_in_region` is bounded above by the region's
    // file extent, which is at most the snapshot file's length used to build the mmap.
    let src = unsafe { backing.add((region.offset + page_offset_in_region) as usize) }
        as *const libc::c_void;
    // SAFETY: same constraints as in `serve_pagefault` — src within snapshot mmap, dst
    // within a region registered with this UFFD.
    let res = unsafe { uffd.copy(src, dst, page_size, true) };
    match res {
        Ok(_) => {
            stats.prefetch_served.fetch_add(1, Ordering::Relaxed);
        }
        Err(UffdCrateError::PartiallyCopied(bytes))
            if bytes == 0 || bytes == (-libc::EAGAIN) as usize =>
        {
            // REMOVE event queued ahead; on-demand fault path will retry, prefetch
            // does not attempt to recover.
            stats.prefetch_eagain.fetch_add(1, Ordering::Relaxed);
        }
        Err(UffdCrateError::CopyFailed(errno))
            if std::io::Error::from(errno).raw_os_error() == Some(libc::EEXIST) =>
        {
            // Guest already faulted this page in; expected race with on-demand path.
            stats.prefetch_eexist.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            stats.prefetch_failed.fetch_add(1, Ordering::Relaxed);
            log::warn!("uffd-internal: prefetch UFFDIO_COPY failed: {e:?}");
        }
    }
}

/// Outcome of a single page-fault servicing attempt. Drives whether the caller defers,
/// records, or moves on.
#[derive(Debug, PartialEq, Eq)]
enum ServeOutcome {
    /// Page is now resident in guest memory (either freshly copied or already present).
    Served,
    /// `UFFDIO_COPY` returned EAGAIN because a REMOVE event is queued ahead; the caller
    /// should retry this address after draining subsequent events.
    Deferred,
    /// Servicing failed in an unexpected way (already logged); the page is not resident
    /// and there is no immediate path to fix it.
    FailedTransient,
}

fn serve_pagefault(
    uffd: &Uffd,
    mappings: &[GuestRegionUffdMapping],
    backing: *const u8,
    page_size: usize,
    addr: u64,
) -> ServeOutcome {
    let page_addr = addr & !((page_size as u64) - 1);
    let region = match mappings
        .iter()
        .find(|r| page_addr >= r.base_host_virt_addr && page_addr < r.base_host_virt_addr + r.size as u64)
    {
        Some(r) => r,
        None => {
            log::warn!("uffd-internal: page fault {page_addr:#x} outside known regions");
            return ServeOutcome::FailedTransient;
        }
    };
    let offset = page_addr - region.base_host_virt_addr;
    // SAFETY: `region.offset + offset` is bounded above by `region.size`, which `setup`
    // computed from the snapshot's region length; the snapshot mmap is at least that big.
    let src = unsafe { backing.add(region.offset as usize + offset as usize) }
        as *const libc::c_void;
    let dst = page_addr as *mut libc::c_void;

    // SAFETY: `src` is within the snapshot mmap; `dst` is within a region registered with
    // this UFFD; both ranges are exactly `page_size` bytes long. Setting the wake bit
    // lets the faulting vCPU resume after the kernel installs the page.
    let res = unsafe { uffd.copy(src, dst, page_size, true) };
    match res {
        Ok(_) => ServeOutcome::Served,
        Err(UffdCrateError::PartiallyCopied(bytes))
            if bytes == 0 || bytes == (-libc::EAGAIN) as usize =>
        {
            ServeOutcome::Deferred
        }
        Err(UffdCrateError::CopyFailed(errno))
            if std::io::Error::from(errno).raw_os_error() == Some(libc::EEXIST) =>
        {
            // Page already populated by another fault on the same address.
            ServeOutcome::Served
        }
        Err(e) => {
            log::error!("uffd-internal: UFFDIO_COPY failed at {page_addr:#x}: {e:?}");
            ServeOutcome::FailedTransient
        }
    }
}

fn unregister_range(uffd: &Uffd, start: *mut libc::c_void, end: *mut libc::c_void, page_size: usize) {
    let start_usz = start as usize;
    let end_usz = end as usize;
    if end_usz <= start_usz {
        return;
    }
    if !start_usz.is_multiple_of(page_size) || !end_usz.is_multiple_of(page_size) {
        log::warn!(
            "uffd-internal: REMOVE range not page-aligned start={start_usz:#x} end={end_usz:#x}"
        );
        return;
    }
    let len = end_usz - start_usz;
    if let Err(e) = uffd.unregister(start, len) {
        log::warn!("uffd-internal: UFFDIO_UNREGISTER failed start={start:?} len={len}: {e:?}");
    }
}

/// Records each unique page-fault offset in first-touch order.
///
/// Flushed on handler shutdown by truncating the target path and writing the trace in
/// one pass + fsync. There is no atomic temp-then-rename: `rename` is intentionally
/// absent from the VMM seccomp filter that gates the handler thread.
///
/// A torn write (firecracker dies mid-flush) is safe because the consumer is robust to
/// truncation: [`load_prefetch_offsets`] uses `lines().map_while(Result::ok)`, which
/// silently drops unparseable trailing bytes. The next restore therefore replays a
/// shorter — but valid — prefix of the recorded trace, with degraded prefetch coverage
/// and no incorrect behavior.
#[derive(Default)]
struct Recorder {
    seen: HashSet<u64>,
    order: Vec<u64>,
}

impl Recorder {
    /// Returns true when `offset` was newly inserted, false when it was already present.
    fn record(&mut self, offset: u64) -> bool {
        if self.seen.insert(offset) {
            self.order.push(offset);
            true
        } else {
            false
        }
    }

    fn flush(&self, path: &Path) -> std::io::Result<()> {
        let mut buf = String::with_capacity(self.order.len() * 12);
        for off in &self.order {
            use std::fmt::Write as _;
            writeln!(buf, "{off}").ok();
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(buf.as_bytes())?;
        f.sync_all()
    }
}

/// Parse an access log: one decimal u64 per line, blank lines and lines starting with
/// `#` skipped, misaligned offsets dropped with a warning. Returns the offsets in file
/// order so prefetch replays first-touch sequence.
fn load_prefetch_offsets(path: &Path, page_size: usize) -> Vec<u64> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("uffd-internal: cannot open access log {path:?}: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let page_mask = (page_size as u64) - 1;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let off: u64 = match trimmed.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if off & page_mask != 0 {
            log::warn!("uffd-internal: skipping misaligned access-log offset {off}");
            continue;
        }
        if seen.insert(off) {
            out.push(off);
        }
    }
    out
}

/// Build a [`Config`] from raw path references.
pub fn config_from_paths(
    snapshot_path: &Path,
    access_log_path: Option<&Path>,
    record_to: Option<&Path>,
) -> Config {
    Config {
        snapshot_path: snapshot_path.to_path_buf(),
        access_log_path: access_log_path.map(Path::to_path_buf),
        record_to: record_to.map(Path::to_path_buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_dedups_and_preserves_first_touch_order() {
        let mut r = Recorder::default();
        assert!(r.record(4096));
        assert!(r.record(8192));
        assert!(!r.record(4096));
        assert!(r.record(12288));
        assert!(!r.record(8192));
        assert_eq!(r.order, vec![4096, 8192, 12288]);
    }

    #[test]
    fn recorder_flush_writes_one_offset_per_line() {
        let mut r = Recorder::default();
        r.record(0);
        r.record(4096);
        r.record(8192);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        r.flush(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "0\n4096\n8192\n");
    }

    #[test]
    fn load_prefetch_offsets_skips_blank_comment_and_misaligned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        std::fs::write(
            &path,
            "# header comment\n\n0\n4096\n4097\n8192\n4096\nnot-a-number\n12288\n",
        )
        .unwrap();
        let offsets = load_prefetch_offsets(&path, 4096);
        assert_eq!(offsets, vec![0, 4096, 8192, 12288]);
    }

    #[test]
    fn load_prefetch_offsets_returns_empty_when_file_missing() {
        let path = Path::new("/nonexistent/path/that/does/not/exist.log");
        assert!(load_prefetch_offsets(path, 4096).is_empty());
    }

    #[test]
    fn record_fault_skips_when_recorder_is_none() {
        // The "no recorder" branch is the hot path on normal restores; a no-op call must
        // not panic even when the mappings table is empty.
        let stats = Stats::default();
        record_fault(None, &[], 4096, 0x1000, &stats);
        assert_eq!(stats.recorded_offsets.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_fault_writes_file_offset_for_in_range_addr() {
        #[allow(deprecated)]
        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000_0000,
            size: 0x4000,
            offset: 0x2000,
            page_size: 4096,
            page_size_kib: 4096,
        }];
        let mut rec = Recorder::default();
        let stats = Stats::default();
        // Fault at virt 0x1000_1000 → page-aligned to itself → in-region offset 0x1000
        // → file offset 0x2000 + 0x1000 = 0x3000.
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_1000, &stats);
        assert_eq!(rec.order, vec![0x3000]);
        assert_eq!(stats.recorded_offsets.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_fault_drops_addr_outside_any_region() {
        let mut rec = Recorder::default();
        let stats = Stats::default();
        record_fault(Some(&mut rec), &[], 4096, 0x1000, &stats);
        assert!(rec.order.is_empty());
        assert_eq!(stats.recorded_offsets.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_fault_counter_only_increments_on_new_offsets() {
        #[allow(deprecated)]
        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000_0000,
            size: 0x4000,
            offset: 0,
            page_size: 4096,
            page_size_kib: 4096,
        }];
        let mut rec = Recorder::default();
        let stats = Stats::default();
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_0000, &stats);
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_0000, &stats);
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_1000, &stats);
        assert_eq!(stats.recorded_offsets.load(Ordering::Relaxed), 2);
    }
}
