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
    /// Snapshot memory file backing guest RAM. In layered mode this is the overlay
    /// (diff) file; pages absent from it are served from `base_path`.
    pub snapshot_path: PathBuf,
    /// Base (template) memory file. When set, the restore is layered: a page is
    /// served from `snapshot_path` if present there, else from this base.
    pub base_path: Option<PathBuf>,
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
    /// Failed to open access-log output file: {0}
    OpenRecorder(std::io::Error),
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

    let overlay = mmap_snapshot(&cfg.snapshot_path).map_err(InternalUffdError::OpenSnapshot)?;
    // Layered restore: mmap the base (template) and scan the overlay's extents so a
    // page absent from the overlay falls through to the base.
    let (base, present) = match cfg.base_path.as_deref() {
        Some(base_path) => {
            let base = mmap_snapshot(base_path).map_err(InternalUffdError::OpenSnapshot)?;
            let present = scan_present_pages(&cfg.snapshot_path, page_size)
                .map_err(InternalUffdError::OpenSnapshot)?;
            (Some(base), Some(present))
        }
        None => (None, None),
    };
    let backing = Backing {
        overlay,
        base,
        present,
    };

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
    let recorder = match cfg.record_to.as_deref() {
        Some(path) => Some(Recorder::create(path).map_err(InternalUffdError::OpenRecorder)?),
        None => None,
    };

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
    let stats = Arc::new(Stats::default());
    let stats_for_thread = Arc::clone(&stats);
    let thread = thread::Builder::new()
        .name("uffd-internal".into())
        .spawn(move || {
            run(
                handler_uffd,
                mappings,
                page_size,
                backing,
                prefetch_offsets,
                recorder,
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

/// One bit per guest page: set ⇒ the page is present in the overlay (diff) file,
/// clear ⇒ it must be served from the base. Built once at setup from the overlay's
/// allocated extents (see `scan_present_pages`).
struct PresenceBitmap {
    bits: Vec<u64>,
}

impl PresenceBitmap {
    fn with_pages(n: usize) -> Self {
        Self {
            bits: vec![0u64; n.div_ceil(64)],
        }
    }
    fn set(&mut self, i: usize) {
        self.bits[i >> 6] |= 1u64 << (i & 63);
    }
    fn is_set(&self, i: usize) -> bool {
        self.bits[i >> 6] & (1u64 << (i & 63)) != 0
    }
}

/// Scan `path`'s allocated extents via `SEEK_DATA`/`SEEK_HOLE` and mark every page
/// that overlaps real data. `dump_dirty` writes dirtied pages as real extents and
/// leaves clean pages as holes (no zero-skip), so a present extent == an overlay
/// page and a hole == "fall through to base".
fn scan_present_pages(path: &Path, page_size: usize) -> std::io::Result<PresenceBitmap> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let npages = (size as usize).div_ceil(page_size);
    let mut pm = PresenceBitmap::with_pages(npages);
    let fd = file.as_raw_fd();
    let mut off: libc::off_t = 0;
    while (off as u64) < size {
        // SAFETY: fd is a valid open file; SEEK_DATA returns the next data offset
        // at or after `off`, or -1/ENXIO once no data remains.
        let data = unsafe { libc::lseek(fd, off, libc::SEEK_DATA) };
        if data < 0 {
            break; // ENXIO: no more data
        }
        // SAFETY: same fd; SEEK_HOLE returns the next hole at or after `data`,
        // or EOF if the extent runs to the end of the file.
        let mut hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            hole = size as libc::off_t;
        }
        let start_pg = data as usize / page_size;
        let end_pg = (hole as usize).div_ceil(page_size).min(npages);
        for p in start_pg..end_pg {
            pm.set(p);
        }
        off = hole;
    }
    Ok(pm)
}

/// The memory backing a layered (or single-file) restore. `overlay` is the file
/// named by `Config::snapshot_path`; in layered mode `base` + `present` resolve
/// pages absent from the overlay to the template.
struct Backing {
    overlay: SnapshotMmap,
    base: Option<SnapshotMmap>,
    present: Option<PresenceBitmap>,
}

impl Backing {
    /// Source pointer for the page at `file_offset`: the overlay if the page is
    /// present there (or there is no base), else the base. The two layers are the
    /// same logical size, so `file_offset` is in bounds for whichever is chosen.
    fn src_ptr(&self, file_offset: u64, page_size: usize) -> *const u8 {
        if let (Some(base), Some(present)) = (&self.base, &self.present) {
            let page_idx = (file_offset / page_size as u64) as usize;
            if !present.is_set(page_idx) {
                // SAFETY: file_offset < total guest mem size <= base mmap size.
                return unsafe { base.addr.add(file_offset as usize) };
            }
        }
        // SAFETY: file_offset < total guest mem size <= overlay mmap size.
        unsafe { self.overlay.addr.add(file_offset as usize) }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    uffd: Uffd,
    mappings: Vec<GuestRegionUffdMapping>,
    page_size: usize,
    backing: Backing,
    prefetch_offsets: Vec<u64>,
    mut recorder: Option<Recorder>,
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
            // The recorder (when active) writes its trace line-by-line during record(),
            // so no shutdown-time flush is needed for it.
            drain_to_completion(
                &uffd,
                &mappings,
                &backing,
                page_size,
                &mut deferred,
                recorder.as_mut(),
                &stats,
            );
            return;
        }

        if let Ok(ack) = drain_rx.try_recv() {
            drain_to_completion(
                &uffd,
                &mappings,
                &backing,
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
            &backing,
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
                    &backing,
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
                    &backing,
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
    backing: &Backing,
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
    backing: &Backing,
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
    backing: &Backing,
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
    backing: &Backing,
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
    // Layered: resolve the page to the overlay or base. `region.offset +
    // page_offset_in_region` is bounded by the region's file extent.
    let src = backing.src_ptr(region.offset + page_offset_in_region, page_size)
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
    backing: &Backing,
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
    // Layered: resolve the page to the overlay or base. `region.offset + offset` is
    // bounded above by `region.size`, so it is in bounds for either mmap.
    let src = backing.src_ptr(region.offset + offset, page_size) as *const libc::c_void;
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

/// Records each unique page-fault offset in first-touch order by appending a line to
/// the target file as each new offset is observed.
///
/// The file is opened up front and held for the recorder's lifetime; the kernel page
/// cache absorbs the per-fault writes and the periodic dirty-pages writeback eventually
/// reaches disk without requiring an explicit flush at shutdown. A SIGKILL'd Firecracker
/// loses at most the last few unwritten-back entries; [`load_prefetch_offsets`] is
/// robust to truncation, so the next restore replays a valid prefix with degraded
/// prefetch coverage and no incorrect behavior.
#[derive(Debug)]
struct Recorder {
    seen: HashSet<u64>,
    file: std::fs::File,
}

impl Recorder {
    fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            seen: HashSet::new(),
            file: std::fs::File::create(path)?,
        })
    }

    /// Returns true when `offset` was newly inserted (and a line was appended), false
    /// when it was already present.
    fn record(&mut self, offset: u64) -> bool {
        if self.seen.insert(offset) {
            // Best-effort write: a failed append degrades prefetch coverage on the
            // next restore but is not a VM-correctness concern.
            let _ = writeln!(self.file, "{offset}");
            true
        } else {
            false
        }
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
    base_path: Option<&Path>,
    access_log_path: Option<&Path>,
    record_to: Option<&Path>,
) -> Config {
    Config {
        snapshot_path: snapshot_path.to_path_buf(),
        base_path: base_path.map(Path::to_path_buf),
        access_log_path: access_log_path.map(Path::to_path_buf),
        record_to: record_to.map(Path::to_path_buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_bitmap_set_and_query() {
        let mut pm = PresenceBitmap::with_pages(130);
        for i in 0..130 {
            assert!(!pm.is_set(i));
        }
        pm.set(0);
        pm.set(65);
        pm.set(129);
        assert!(pm.is_set(0) && pm.is_set(65) && pm.is_set(129));
        assert!(!pm.is_set(1) && !pm.is_set(64) && !pm.is_set(128));
    }

    #[test]
    fn scan_present_pages_marks_data_pages_not_holes() {
        use std::io::{Seek, SeekFrom, Write};
        let ps = 4096usize;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.diff");
        let mut f = std::fs::File::create(&path).unwrap();
        f.set_len((4 * ps) as u64).unwrap(); // 4 pages, all holes
        // Real (non-zero) data into pages 0 and 2; leave 1 and 3 as holes — exactly
        // how dump_dirty lays out a diff (dirty = extent, clean = hole).
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&vec![1u8; ps]).unwrap();
        f.seek(SeekFrom::Start((2 * ps) as u64)).unwrap();
        f.write_all(&vec![2u8; ps]).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let pm = scan_present_pages(&path, ps).unwrap();
        assert!(pm.is_set(0), "page 0 has data");
        assert!(!pm.is_set(1), "page 1 is a hole → must fall through to base");
        assert!(pm.is_set(2), "page 2 has data");
        assert!(!pm.is_set(3), "page 3 is a hole → must fall through to base");
    }

    #[test]
    fn recorder_writes_one_line_per_unique_offset_in_first_touch_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        let mut r = Recorder::create(&path).unwrap();
        assert!(r.record(4096));
        assert!(r.record(8192));
        assert!(!r.record(4096));
        assert!(r.record(12288));
        assert!(!r.record(8192));
        // Drop the recorder so the writes are visible (the File's buffer is flushed
        // by Drop and the kernel exposes the writes to subsequent reads).
        drop(r);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "4096\n8192\n12288\n");
    }

    #[test]
    fn recorder_create_fails_on_unwritable_path() {
        Recorder::create(Path::new("/nonexistent/dir/access.log")).unwrap_err();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        let mut rec = Recorder::create(&path).unwrap();
        let stats = Stats::default();
        // Fault at virt 0x1000_1000 → page-aligned to itself → in-region offset 0x1000
        // → file offset 0x2000 + 0x1000 = 0x3000.
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_1000, &stats);
        drop(rec);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "12288\n"); // 0x3000
        assert_eq!(stats.recorded_offsets.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_fault_drops_addr_outside_any_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        let mut rec = Recorder::create(&path).unwrap();
        let stats = Stats::default();
        record_fault(Some(&mut rec), &[], 4096, 0x1000, &stats);
        drop(rec);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        let mut rec = Recorder::create(&path).unwrap();
        let stats = Stats::default();
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_0000, &stats);
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_0000, &stats);
        record_fault(Some(&mut rec), &mappings, 4096, 0x1000_1000, &stats);
        assert_eq!(stats.recorded_offsets.load(Ordering::Relaxed), 2);
    }
}
