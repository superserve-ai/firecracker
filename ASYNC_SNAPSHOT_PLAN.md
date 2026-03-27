# Async Background Memory Snapshot — Zero-Pause VM Snapshots

## Context

After implementing the block-level overlay (PR #5), the remaining bottleneck in the agent tool-call loop is **memory snapshot time**. Currently, the VM is paused for the entire duration of the memory dump — the pause time scales linearly with guest memory size. The larger the guest, the longer the agent waits.

The goal: reduce VM pause time to **microseconds**, regardless of guest memory size.

## Approach: userfaultfd Write-Protect COW

Use Linux's `UFFDIO_WRITEPROTECT` (kernel 5.7+) to write-protect dirty guest pages, resume the VM immediately, and write pages to disk in the background. If the VM writes to a protected page, the uffd handler saves the old data before allowing the write. No signals involved — uffd handles it cleanly.

This is the same technique QEMU uses for live VM snapshots. CRIU uses it for live container checkpointing. Firecracker already has uffd infrastructure for snapshot restore — we extend it to snapshot creation.

```
CURRENT:
  Pause VM → write ALL memory to disk → sync → Resume VM
  (pause time scales with memory size)

PROPOSED:
  Pause VM
    → get dirty bitmap from KVM (microseconds)
    → register dirty pages with uffd write-protect (microseconds)
    → Resume VM (microseconds)
      ↓
  Background thread writes dirty pages to disk
      ↓
  If VM writes to a protected page:
    → uffd event fires
    → handler saves old page data to snapshot buffer
    → handler unprotects the page
    → VM write proceeds
      ↓
  Background thread finishes → snapshot complete
```

### Why this approach

- **Pause time is O(1)**: microseconds regardless of guest memory size (1GB, 4GB, 16GB)
- **No extra memory proportional to guest size**: only pages the VM modifies during snapshot need buffering (typically very few during a short snapshot window)
- **Battle-tested pattern**: QEMU live snapshots, CRIU live checkpointing
- **Firecracker already has uffd**: used for snapshot restore (`guest_memory_from_uffd()` in `persist.rs:561-593`). We reuse the same infrastructure for snapshot creation.
- **No signal handler changes**: `UFFDIO_WRITEPROTECT` delivers events via uffd file descriptor, not via SIGSEGV/SIGBUS (which are fatal in Firecracker)

---

## Phase 1: uffd Write-Protect Infrastructure

### What we build

A `SnapshotWriteProtect` component that:
1. Creates a userfaultfd and registers guest memory for write-protect tracking
2. Write-protects dirty pages via `UFFDIO_WRITEPROTECT`
3. Runs a handler thread that catches write-protect faults
4. Saves old page data before allowing writes to proceed

### New file: `src/vmm/src/snapshot/write_protect.rs`

```rust
pub struct SnapshotWriteProtect {
    uffd: Uffd,
    /// Pages that the VM tried to write during snapshot.
    /// These need to be saved before unprotecting.
    cow_pages: Arc<Mutex<HashMap<u64, Vec<u8>>>>,
    handler_thread: Option<JoinHandle<()>>,
}

impl SnapshotWriteProtect {
    /// Register guest memory regions for write-protect fault tracking.
    pub fn new(guest_memory: &GuestMemoryMmap) -> Result<Self, SnapshotError>;

    /// Write-protect all dirty pages. Called during pause.
    pub fn protect_dirty_pages(
        &self,
        dirty_bitmap: &DirtyBitmap,
        guest_memory: &GuestMemoryMmap,
    ) -> Result<(), SnapshotError>;

    /// Start the handler thread that catches write-protect faults.
    pub fn start_handler(&mut self, guest_memory: Arc<GuestMemoryMmap>) -> Result<(), SnapshotError>;

    /// Unprotect all pages and stop handler. Called when snapshot write completes.
    pub fn finish(&mut self) -> Result<HashMap<u64, Vec<u8>>, SnapshotError>;
}
```

### Handler thread logic

```
loop {
    event = uffd.read_event()  // blocks until VM writes to protected page

    if event.is_write_protect_fault():
        page_addr = event.address & ~(PAGE_SIZE - 1)

        // Save the old page data (this is what goes into the snapshot)
        old_data = read_page(guest_memory, page_addr)
        cow_pages.insert(page_addr, old_data)

        // Unprotect the page so the VM can write
        uffd.write_protect(page_addr, PAGE_SIZE, false)
}
```

### Files to create/modify

| File | Change |
|------|--------|
| `src/vmm/src/snapshot/write_protect.rs` | New — uffd write-protect COW handler |
| `src/vmm/src/snapshot/mod.rs` | Add `pub mod write_protect;` |
| `src/vmm/src/lib.rs` | Add `write_protect: Option<SnapshotWriteProtect>` to Vmm |

---

## Phase 2: Background Memory Writer

### What we build

A background thread that writes dirty pages to the snapshot file while the VM runs.

### New file: `src/vmm/src/snapshot/background_writer.rs`

```rust
pub struct BackgroundMemoryWriter {
    sender: mpsc::Sender<WriteRequest>,
    handle: JoinHandle<Result<WriteStats, SnapshotError>>,
    status: Arc<AtomicU8>,
}

pub struct WriteRequest {
    /// Guest memory reference for reading pages
    guest_memory: Arc<GuestMemoryMmap>,
    /// Which pages are dirty (from KVM)
    dirty_bitmap: DirtyBitmap,
    /// Where to write
    mem_file_path: PathBuf,
    /// Total memory size
    mem_size: u64,
    /// Reference to COW pages (pages modified by VM during snapshot)
    cow_pages: Arc<Mutex<HashMap<u64, Vec<u8>>>>,
    /// Whether to sync to disk
    sync: bool,
}

pub struct WriteStats {
    pub pages_written: u64,
    pub cow_pages_saved: u64,
    pub write_time_us: u64,
}
```

### Writer thread logic

```
1. Open/create snapshot memory file
2. Set file length to guest memory size
3. For each dirty page in bitmap:
     a. Check cow_pages — if VM already modified this page, use the saved copy
     b. Otherwise read directly from guest memory (page is still write-protected)
     c. Write to file at correct offset
4. After all dirty pages written:
     a. Signal write_protect to finish (unprotect all remaining pages)
     b. Optional: fsync
     c. Update status to "complete"
```

### Files to create/modify

| File | Change |
|------|--------|
| `src/vmm/src/snapshot/background_writer.rs` | New — background memory write thread |
| `src/vmm/src/snapshot/mod.rs` | Add `pub mod background_writer;` |

---

## Phase 3: Integration with Snapshot Flow

### Modified `create_snapshot()`

```rust
pub fn create_snapshot(vmm, vm_info, params) -> Result<(), CreateSnapshotError> {
    // Save VM state (CPU, devices) — same as before, fast
    let microvm_state = vmm.save_state(vm_info)?;
    snapshot_state_to_file(&microvm_state, &params.snapshot_path)?;

    if params.async_snapshot {
        // ---- ASYNC PATH (new) ----

        // 1. Get dirty bitmap (microseconds)
        let dirty_bitmap = vmm.vm.get_dirty_bitmap()?;

        // 2. Write-protect dirty pages (microseconds)
        let mut wp = SnapshotWriteProtect::new(vmm.vm.guest_memory())?;
        wp.protect_dirty_pages(&dirty_bitmap, vmm.vm.guest_memory())?;
        wp.start_handler(vmm.vm.guest_memory_arc())?;

        // 3. Start background writer
        let writer = BackgroundMemoryWriter::start(WriteRequest {
            guest_memory: vmm.vm.guest_memory_arc(),
            dirty_bitmap,
            mem_file_path: params.mem_file_path,
            cow_pages: wp.cow_pages(),
            sync: !params.skip_sync,
        })?;

        // 4. Store references for later cleanup
        vmm.active_snapshot = Some(ActiveSnapshot { wp, writer });

        // VM will be resumed by the caller — pause time was microseconds

    } else {
        // ---- SYNC PATH (existing, unchanged) ----
        vmm.vm.snapshot_memory_to_file(&params.mem_file_path, params.snapshot_type)?;
    }

    // Write block deltas (already fast from PR #5)
    if let Some(ref delta_dir) = params.block_delta_dir {
        vmm.device_manager.write_block_deltas(delta_dir)?;
    }

    Ok(())
}
```

### New API fields

**`CreateSnapshotParams`:**
```rust
pub async_snapshot: bool,  // Use background snapshot (default: false for backward compat)
pub skip_sync: bool,       // Skip fsync (default: false)
```

### Snapshot completion handling

The VM resumes immediately after `create_snapshot()` returns. The background writer continues. When it finishes:
1. All remaining write-protected pages are unprotected
2. Status is set to "complete"
3. An eventfd fires (if registered) to notify the API layer

**New API endpoint: `GET /snapshot/status`**
```json
{
    "state": "writing",  // "idle" | "protecting" | "writing" | "complete" | "error"
    "dirty_pages": 65536,
    "pages_written": 32000,
    "cow_faults": 12,
    "elapsed_us": 150000
}
```

### Files to modify

| File | Change |
|------|--------|
| `src/vmm/src/persist.rs` | Modified `create_snapshot()` with async path |
| `src/vmm/src/lib.rs` | Add `active_snapshot: Option<ActiveSnapshot>` to Vmm |
| `src/vmm/src/vmm_config/snapshot.rs` | Add `async_snapshot`, `skip_sync` fields |
| `src/vmm/src/rpc_interface.rs` | Add snapshot status endpoint |
| `src/firecracker/swagger/firecracker.yaml` | API schema updates |
| `src/vmm/src/vstate/vm.rs` | Expose guest memory Arc, make `get_dirty_bitmap()` public |

---

## Phase 4: Diff Snapshot Optimization

For agent tool-call loops, only a few pages change between tool calls. KVM dirty tracking already gives us the minimal dirty set.

### Flow for agent tool calls

```
1. Agent starts → VM boots (all pages dirty)
2. First snapshot: protect all pages, write all in background
3. Agent runs tool call → modifies ~1% of pages
4. Second snapshot: protect only 1% of pages, write only those
5. Repeat 3-4 for each tool call
```

### Key: reset dirty tracking after snapshot

After the background writer finishes and all pages are unprotected:
```rust
vmm.vm.reset_dirty_bitmap();
vmm.vm.guest_memory().reset_dirty();
```

This ensures the next snapshot's dirty bitmap only contains pages modified since the last snapshot.

### Expected behavior

Pause time becomes constant (microseconds) regardless of guest memory size or dirty page count. The background write time scales with the number of dirty pages but happens while the VM is running. Exact numbers to be determined by benchmarking after implementation.

---

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| uffd write-protect not available (kernel < 5.7) | Feature-gate: fall back to sync snapshot on old kernels. Check `UFFD_FEATURE_PAGEFAULT_FLAG_WP` at init. |
| High COW fault rate (VM writes many pages during snapshot) | Monitor cow_faults in stats. If > 50% of pages, the async snapshot degrades toward sync performance. For agent workloads this shouldn't happen — tool calls are short. |
| Background writer crashes | Store snapshot status as "error". The sync path is always available as fallback. Snapshot file is incomplete — don't use it. |
| Memory ordering between uffd handler and writer | uffd handler acquires lock before saving page. Writer checks cow_pages before reading guest memory. Standard mutex synchronization. |
| Guest memory remapping during snapshot | Block memory hotplug/balloon operations while async snapshot is in progress. |

---

## Verification

### Unit Tests
- Write-protect a memory region → write to it → verify uffd event fires
- COW page save → verify old data preserved correctly
- Background writer → verify snapshot file matches expected dirty pages
- COW pages merged into snapshot file correctly
- Fallback to sync when uffd write-protect unavailable

### Integration Tests (GCE VM)
- Async snapshot of running VM → restore → verify state correct
- Async snapshot during I/O load → verify no data corruption
- Multiple sequential async snapshots (diff mode) → verify each captures only new changes
- Benchmark: pause time measurement (should be < 1ms)

### Benchmark
```bash
# On amit-firecracker GCE VM
# Compare pause time: sync vs async
# Compare total snapshot time
# Measure COW fault rate under various workloads
```

---

## Key files to read before implementing

| File | What to understand |
|------|-------------------|
| `src/vmm/src/vstate/vm.rs:334-394` | Current sync snapshot path |
| `src/vmm/src/vstate/memory.rs:137-213` | `dump_dirty()` — page iteration logic |
| `src/vmm/src/vstate/memory.rs:215-235` | `mprotect()` — existing page protection |
| `src/vmm/src/vstate/vm.rs:308-326` | `get_dirty_bitmap()` — KVM bitmap |
| `src/vmm/src/persist.rs:561-593` | `guest_memory_from_uffd()` — existing uffd usage |
| `src/vmm/src/persist.rs:618-669` | `send_uffd_handshake()` — uffd socket protocol |
| `src/vmm/src/lib.rs:504-547` | `pause_vm()` / `resume_vm()` |
| `src/vmm/src/vstate/vcpu.rs:169-197` | Thread spawning pattern |
| `src/vmm/src/signal_handler.rs:157-165` | Signal handlers — SIGSEGV is fatal |
