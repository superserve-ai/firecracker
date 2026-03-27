# Block-Level Copy-on-Write Overlay with Dirty Bitmap Tracking

## Context

We want to build a block-level overlay filesystem inside Firecracker's VMM, similar to what TensorLake built for their agent sandbox platform. The goal is fast snapshot/restore for agent tool-call loops: each tool call modifies a small fraction of the disk, so snapshots should only persist dirty blocks (O(dirty) instead of O(disk size)).

**Key insight**: Firecracker's snapshot already excludes disk data — it saves VMM state + guest memory only. The disk image is managed externally. This means our overlay + dirty bitmap gives us the ability to efficiently capture and replay only the disk changes between snapshots, enabling fast VM cloning and restore.

**Insertion point**: The `FileEngine` enum in `io/mod.rs` — all block I/O flows through `read()`/`write()`/`flush()` methods on this enum. We add an `Overlay` variant that wraps a read-only base file + writable sparse overlay file + dirty bitmap.

---

## Phase 0: Dirty Bitmap Data Structure

### New file: `src/vmm/src/devices/virtio/block/virtio/io/dirty_bitmap.rs`

**Struct**:
```rust
pub struct DirtyBitmap {
    bits: BitVec,       // from bitvec crate (already a dependency)
    block_size: u32,    // granularity in bytes (default 4096)
    total_blocks: u64,
}
```

**Design decisions**:
- **Granularity: 4KB blocks, not 512-byte sectors.** Rationale: (a) 4KB matches host page size and ext4/xfs block size — sparse file allocation is 4KB-aligned anyway; (b) 8x smaller bitmap (32KB per GB vs 256KB); (c) sub-block writes need read-modify-write regardless since the overlay is block-aligned. Sector-level tracking gains nothing because the host filesystem can't allocate less than 4KB. (d) `bitvec` is already used in the codebase for memory slot tracking (`vstate/memory.rs:104`, `devices/virtio/mem/device.rs:100`).
- **No sub-block partial write concern**: Virtio requests are sector-aligned (enforced in `Request::parse()`, `request.rs:300`), and a 4KB block always contains whole sectors. The bitmap marks a block dirty on any write touching it.

**API**:
```rust
impl DirtyBitmap {
    pub fn new(disk_size_bytes: u64, block_size: u32) -> Self;
    pub fn set(&mut self, offset: u64, len: u32);           // mark blocks covering [offset, offset+len) dirty
    pub fn is_set(&self, block_idx: u64) -> bool;
    pub fn is_range_uniform(&self, offset: u64, len: u32) -> Option<bool>; // Some(true)=all dirty, Some(false)=all clean, None=mixed
    pub fn clear(&mut self);
    pub fn dirty_count(&self) -> u64;
    pub fn iter_dirty(&self) -> impl Iterator<Item = u64>;  // yields dirty block indices
    pub fn serialize(&self) -> Vec<u8>;                      // for snapshot
    pub fn deserialize(bytes: &[u8], block_size: u32, total_blocks: u64) -> Result<Self, DirtyBitmapError>;
}
```

**Hardening**:
- `deserialize()` validates: byte length matches expected `ceil(total_blocks / 8)`, block_size is power of 2 and >= 512, total_blocks > 0.
- All index calculations use checked arithmetic to prevent overflow.
- `#[derive(Debug, Clone)]` for testability. No `Serialize/Deserialize` derive — manual `serialize()`/`deserialize()` with validation.

**Error type**:
```rust
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum DirtyBitmapError {
    /// Invalid block size: {0} (must be power of 2, >= 512)
    InvalidBlockSize(u32),
    /// Bitmap data length mismatch: expected {expected}, got {actual}
    LengthMismatch { expected: usize, actual: usize },
    /// Disk size is zero
    ZeroDiskSize,
}
```

**Tests**: Unit tests for set/get/clear, boundary conditions (first block, last block, offset not block-aligned), serialize/deserialize round-trip, deserialize with corrupted data, `is_range_uniform` for all-dirty/all-clean/mixed ranges.

---

## Phase 1: Sync Overlay Engine

### New file: `src/vmm/src/devices/virtio/block/virtio/io/overlay_io.rs`

**Struct**:
```rust
pub struct OverlayFileEngine {
    base: File,                // read-only, shared across VMs
    overlay: File,             // read-write, sparse, same logical size as base
    bitmap: DirtyBitmap,
    nsectors: u64,             // for validation
}
```

**Core operations** (same signature as `SyncFileEngine`):

- **`read(offset, mem, addr, count)`**:
  1. Call `bitmap.is_range_uniform(offset, count)`
  2. Fast path (common case): all clean → single read from `base`; all dirty → single read from `overlay`
  3. Slow path (rare): mixed → split into contiguous runs of same source, issue sequential reads. Log a metric for mixed reads.

- **`write(offset, mem, addr, count)`**:
  1. Write to `overlay` at `offset` (same offset as base — overlay is a sparse file of matching size)
  2. Call `bitmap.set(offset, count)` to mark blocks dirty
  3. Return count

- **`flush()`**: Flush + sync_all on overlay only. Base is read-only.

- **`from_files(base, overlay, nsectors, bitmap) -> Result<Self, OverlayIoError>`**: Constructor. Validates base is readable, overlay is writable, sizes match.

- **`update_file(overlay: File)`**: Replace overlay file handle (for drive hot-update).

- **`bitmap(&self) -> &DirtyBitmap`**: Accessor for snapshot serialization.

- **`into_bitmap(self) -> DirtyBitmap`**: Consume engine for delta export.

**Error type** (follows `SyncIoError` pattern from `sync_io.rs:11-21`):
```rust
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum OverlayIoError {
    /// Base read seek: {0}
    BaseSeek(std::io::Error),
    /// Base read transfer: {0}
    BaseTransfer(GuestMemoryError),
    /// Overlay seek: {0}
    OverlaySeek(std::io::Error),
    /// Overlay read transfer: {0}
    OverlayReadTransfer(GuestMemoryError),
    /// Overlay write transfer: {0}
    OverlayWriteTransfer(GuestMemoryError),
    /// Overlay flush: {0}
    OverlayFlush(std::io::Error),
    /// Overlay sync: {0}
    OverlaySync(std::io::Error),
    /// Size mismatch: base={base_size}, overlay={overlay_size}
    SizeMismatch { base_size: u64, overlay_size: u64 },
    /// Bitmap error: {0}
    Bitmap(DirtyBitmapError),
}
```

### Modify: `src/vmm/src/devices/virtio/block/virtio/io/mod.rs`

- Add `pub mod dirty_bitmap;` and `pub mod overlay_io;`
- Add `pub use self::overlay_io::{OverlayFileEngine, OverlayIoError};`
- Add `Overlay(OverlayIoError)` to `BlockIoError` enum
- Add `FileEngine::Overlay(OverlayFileEngine)` variant
- Extend all match arms: `read()`, `write()`, `flush()`, `drain()`, `drain_and_flush()`, `from_file()`, `update_file_path()`, `file()`

**Crash consistency** (fsync ordering in `write()`):
- Write data to overlay → `overlay.sync_data()` if `CacheType::Writeback` → then bitmap is updated in memory
- If host crashes after overlay write but before bitmap update: sector appears clean but overlay has data. On next read, data comes from base (stale but consistent — no corruption, just lost write, same as if crash happened before the write)
- If host crashes after bitmap update: bitmap says dirty, overlay has data. Correct.
- **This is safe**: bitmap is always a conservative subset of what's actually in the overlay. Reading from base when overlay has data is the same as the write never happening — acceptable crash semantics matching Firecracker's existing behavior.

**Hardening**:
- Base file opened with `O_RDONLY`. Validate at construction: attempt a no-op write to base should fail (or just check file mode).
- Overlay file created with `File::set_len(base_size)` to match base. Validate sizes match at construction.
- All `seek()` calls use checked offset arithmetic.
- Mixed reads: cap split count to prevent pathological cases (e.g., alternating dirty/clean blocks). If split count > 32, fall back to reading entire range from overlay (works because overlay's clean blocks are zeros, but we need to pre-populate from base — so instead, just read block-by-block from the appropriate source). In practice this won't happen for typical 4KB guest I/O.

**Tests**: Reuse existing `test_sync` pattern from `io/mod.rs:254-336`. Add:
- Write-then-read correctness
- Read from base (never-written sectors)
- Mixed reads across dirty/clean boundary
- Verify base file is never written to (check mtime before/after)
- Partial block writes (512B write within a 4KB block)
- Concurrent access safety (not needed for sync — single-threaded I/O)

---

## Phase 2: Configuration & API

### Modify: `src/vmm/src/devices/virtio/block/virtio/device.rs`

- Add `Overlay` variant to `FileEngineType` enum (line ~46)
- Extend `DiskProperties::new()` to accept optional `base_path`:
  ```rust
  pub fn new_overlay(
      base_image_path: String,
      overlay_path: String,
      file_engine_type: FileEngineType,  // must be Overlay
  ) -> Result<Self, VirtioBlockError>
  ```
  - Open base with `OpenOptions::new().read(true)` (read-only, no write)
  - Open/create overlay with `OpenOptions::new().read(true).write(true).create(true)`
  - If overlay is new, set length to match base: `overlay_file.set_len(base_size)`
  - If overlay exists, validate `overlay_size == base_size`
  - Construct `OverlayFileEngine` with empty or provided bitmap
  - `nsectors` derived from base image size
  - `image_id` derived from base image metadata (shared identity)

### Modify: `src/vmm/src/vmm_config/drive.rs`

- Add to `BlockDeviceConfig`:
  ```rust
  pub base_path: Option<String>,      // read-only base image path (for overlay mode)
  pub overlay_block_size: Option<u32>, // bitmap granularity, default 4096
  ```
- Validation in config parsing:
  - If `io_engine == "Overlay"`, `base_path` is required and `path_on_host` becomes the overlay path
  - If `io_engine != "Overlay"`, `base_path` must be None
  - `overlay_block_size` must be power of 2 and >= 512 if provided
  - `is_read_only` must be false for overlay mode (the overlay is writable)

### Modify: `src/vmm/src/devices/virtio/block/virtio/device.rs` — `VirtioBlockConfig`

- Add `base_path: Option<String>`
- Wire through from `BlockDeviceConfig` to `VirtioBlockConfig` in the `TryFrom` impl

### Modify: `src/firecracker/swagger/firecracker.yaml`

- Add to `Drive` schema:
  ```yaml
  base_path:
    type: string
    description: "Read-only base image path for overlay mode. Required when io_engine is Overlay."
  overlay_block_size:
    type: integer
    description: "Block size for dirty tracking bitmap in bytes. Default 4096. Must be power of 2, >= 512."
    default: 4096
  ```
- Add `"Overlay"` to `io_engine` enum

### Modify: `src/vmm/src/devices/virtio/block/virtio/metrics.rs`

- Add overlay-specific metrics to `BlockDeviceMetrics` (line ~143):
  ```rust
  pub overlay_base_reads: SharedIncMetric,      // reads served from base
  pub overlay_dirty_reads: SharedIncMetric,     // reads served from overlay
  pub overlay_mixed_reads: SharedIncMetric,     // reads that crossed dirty/clean boundary
  pub overlay_dirty_blocks: SharedIncMetric,    // current dirty block count (gauge-like, reset on snapshot)
  ```
- Initialize in `BlockDeviceMetrics::new()`, add to `aggregate()`

**Tests**: Config validation tests — overlay without base_path fails, overlay with read_only fails, non-overlay with base_path fails. End-to-end: create VirtioBlock in overlay mode, write, read back.

---

## Phase 3: Snapshot Integration

### Modify: `src/vmm/src/devices/virtio/block/virtio/persist.rs`

- Add `OverlayState`:
  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct OverlayState {
      pub base_path: String,
      pub overlay_path: String,
      pub dirty_bitmap: Vec<u8>,    // serialized DirtyBitmap
      pub block_size: u32,
      pub total_blocks: u64,
      pub bitmap_checksum: u64,     // CRC64 of dirty_bitmap bytes
  }
  ```

- Extend `VirtioBlockState`:
  ```rust
  pub overlay_state: Option<OverlayState>,  // None for non-overlay disks
  ```

- Add `Overlay` to `FileEngineTypeState` enum with From/Into conversions

- **`save()`**: When file engine is overlay, populate `overlay_state` with base_path, overlay_path, serialized bitmap, and CRC64 checksum of bitmap bytes.

- **`restore()`**: When `overlay_state` is present:
  1. Validate `bitmap_checksum` matches CRC64 of `dirty_bitmap` bytes
  2. Deserialize bitmap with validation (size, block_size, total_blocks)
  3. Open base read-only, open overlay read-write
  4. Construct `OverlayFileEngine` with restored bitmap
  5. If checksum fails → return `VirtioBlockError` (don't silently proceed with corrupted bitmap)

### Modify: `src/vmm/src/devices/virtio/block/persist.rs`

- Extend `BlockState::set_host_path()` → this sets the overlay_path for overlay devices
- Add `BlockState::set_base_path(path: &str)` → sets base_path in overlay_state
- Add `BlockState::set_overlay_path(path: &str)` → sets overlay_path in overlay_state

### Modify: `src/vmm/src/vmm_config/snapshot.rs`

- Extend `DriveOverride`:
  ```rust
  pub struct DriveOverride {
      pub drive_id: String,
      pub path_on_host: String,
      pub base_path: Option<String>,  // NEW: override base image path on restore
  }
  ```

### Modify: `src/vmm/src/persist.rs`

- In `restore_from_snapshot()` drive override loop (line ~411-429):
  - Apply `base_path` override if present: `device_state.set_base_path(&path)`
  - Apply `path_on_host` override as overlay path for overlay devices

### Bump: Snapshot version

- Increment `SNAPSHOT_VERSION` minor version in `src/vmm/src/persist.rs:163`
- Old snapshots (without `overlay_state`) restore fine — `Option<OverlayState>` defaults to `None`

### Modify: `src/vmm/src/devices/virtio/block/virtio/device.rs`

- **`prepare_save()`** (line ~571): When overlay engine, call `overlay.flush()` to ensure overlay data is on disk before bitmap is serialized. The bitmap is in memory and serialized during `save()` — since the VM is paused (vCPUs stopped, no new I/O), this is safe.

**Hardening**:
- Checksum validation on restore prevents corrupted bitmaps from causing silent data loss
- Base path validation: file must exist and be readable at restore time
- Overlay path validation: file must exist at restore time (or create fresh if doing clone)
- Version compatibility: `Option<OverlayState>` makes this backward-compatible with older snapshots

**Tests**: Save/restore round-trip — create overlay VM, write data, snapshot, restore, verify bitmap and data integrity. Restore with corrupted checksum should fail. Restore old snapshot (no overlay_state) should work.

---

## Phase 4: Delta Snapshots

### New file: `src/vmm/src/devices/virtio/block/virtio/io/delta.rs`

**Delta file format**:
```
[Header: 32 bytes]
  magic:          u64   = 0x4F564C5944454C54 ("OVLYDELT")
  version:        u32   = 1
  block_size:     u32
  total_blocks:   u64
  dirty_count:    u64
[Bitmap section]
  bitmap_len:     u32
  bitmap_data:    [u8; bitmap_len]
  bitmap_crc64:   u64
[Data section]
  For each dirty block (in index order):
    block_data:   [u8; block_size]
  data_crc64:     u64   // CRC64 of entire data section
```

**API**:
```rust
pub fn write_delta(
    overlay: &mut File,
    bitmap: &DirtyBitmap,
    delta_path: &Path,
) -> Result<DeltaStats, DeltaError>;

pub fn apply_delta(
    overlay: &mut File,
    bitmap: &mut DirtyBitmap,
    delta_path: &Path,
) -> Result<DeltaStats, DeltaError>;

pub struct DeltaStats {
    pub dirty_blocks: u64,
    pub bytes_written: u64,
    pub duration_us: u64,
}
```

**Hardening**:
- Magic number validation on read
- Version check (reject unknown versions)
- CRC64 on bitmap section and data section separately — detect corruption early
- `dirty_count` cross-checked against bitmap popcount
- `total_blocks` cross-checked against overlay file size
- Bounded allocation: `dirty_count * block_size` must not exceed a configurable max (e.g., disk size) to prevent OOM from malicious delta files
- Sequential I/O on delta file write (dirty blocks written in index order) for optimal disk throughput

**Integration with snapshot flow**:
- Add optional `block_delta_path: Option<String>` to `CreateSnapshotParams`
- In `create_snapshot()` (persist.rs:166): after saving VM state, if overlay engine and delta_path provided, call `write_delta()`
- In `restore_from_snapshot()`: if delta_path provided, create fresh overlay and apply delta before constructing devices

**Tests**: Write delta → apply to fresh overlay → verify data matches original. Corrupted delta (bad magic, bad CRC, wrong version) should fail with clear errors. Empty delta (no dirty blocks). Full-disk delta (all blocks dirty).

---

## Phase 5: Fast VM Cloning

This is primarily a **usage pattern** enabled by phases 1-4:

**Clone flow**:
1. Source VM: snapshot with delta → produces delta file + VM state snapshot
2. Clone VM: create with same `base_path`, fresh empty overlay, apply delta from source
3. Result: clone has identical disk state to source at snapshot time

**API addition** — Add `clone_from_delta` field to `LoadSnapshotConfig`:
```rust
pub struct DriveOverride {
    pub drive_id: String,
    pub path_on_host: String,          // overlay path for the clone
    pub base_path: Option<String>,     // base image (usually same as source)
    pub delta_path: Option<String>,    // NEW: apply this delta to create clone's overlay
}
```

**On restore with delta_path**:
1. Create fresh sparse overlay at `path_on_host`
2. Apply delta to overlay
3. Construct `OverlayFileEngine` with the bitmap from the delta

**Hardening**:
- Base image integrity: compute SHA256 of base image at VM creation, store in `OverlayState`, verify on clone restore. This catches cases where someone modified the shared base between source snapshot and clone restore.
- Base image immutability: open with `O_RDONLY`. Log warning if base image mtime changed between construction and snapshot.

**Tests**: Snapshot source → clone 3 VMs → verify each has correct data. Clone with different base_path should fail integrity check.

---

## Phase 6: Async Overlay (Future — NOT in initial implementation)

Deferred. Sync overlay is sufficient for agent workloads where I/O is typically light (tool calls do small reads/writes, not sustained throughput). The sync path adds ~microseconds per I/O which is negligible compared to the milliseconds saved on snapshot/restore.

Revisit if benchmarks show sync overlay is a bottleneck.

---

## Files to Create

| File | Phase | Description |
|------|-------|-------------|
| `src/vmm/src/devices/virtio/block/virtio/io/dirty_bitmap.rs` | 0 | DirtyBitmap struct |
| `src/vmm/src/devices/virtio/block/virtio/io/overlay_io.rs` | 1 | OverlayFileEngine |
| `src/vmm/src/devices/virtio/block/virtio/io/delta.rs` | 4 | Delta file read/write |

## Files to Modify

| File | Phase | Changes |
|------|-------|---------|
| `src/vmm/src/devices/virtio/block/virtio/io/mod.rs` | 0,1 | Add modules, `Overlay` variant to `FileEngine` and `BlockIoError`, extend match arms |
| `src/vmm/src/devices/virtio/block/virtio/device.rs` | 2 | `FileEngineType::Overlay`, `DiskProperties::new_overlay()`, `prepare_save()` |
| `src/vmm/src/vmm_config/drive.rs` | 2 | `base_path`, `overlay_block_size` fields on `BlockDeviceConfig` |
| `src/vmm/src/devices/virtio/block/virtio/metrics.rs` | 2 | Overlay-specific metrics |
| `src/firecracker/swagger/firecracker.yaml` | 2 | API schema additions |
| `src/vmm/src/devices/virtio/block/virtio/persist.rs` | 3 | `OverlayState`, `FileEngineTypeState::Overlay`, save/restore with bitmap |
| `src/vmm/src/devices/virtio/block/persist.rs` | 3 | `set_base_path()`, `set_overlay_path()` |
| `src/vmm/src/vmm_config/snapshot.rs` | 3,5 | `DriveOverride` extensions |
| `src/vmm/src/persist.rs` | 3,4 | Snapshot version bump, delta path handling, override application |
| `src/vmm/src/devices/virtio/block/device.rs` | 2 | Wire overlay config through `Block::new()` |
| `src/vmm/Cargo.toml` | 0 | (bitvec already present — no change needed) |

## Existing Code to Reuse

- `bitvec::vec::BitVec` — already used in `vstate/memory.rs:104` and `devices/virtio/mem/device.rs:100`
- `SyncFileEngine` pattern — error types, seek+read/write pattern, flush
- `BlockIoError` enum pattern — wrapping per-engine errors
- `Persist` trait — save/restore pattern from `persist.rs:65-138`
- `DriveOverride` mechanism — path override during restore from `persist.rs:411-429`
- `BlockDeviceMetrics` — per-device metrics pattern from `metrics.rs:89-113`
- `SNAPSHOT_VERSION` versioning — major/minor compatibility from `persist.rs:163`
- CRC64 — already used in snapshot format (`snapshot/mod.rs`)

## Verification

### Unit Tests (each phase)
- `cargo test -p vmm` — run all VMM tests
- Phase 0: `cargo test -p vmm dirty_bitmap` — bitmap correctness
- Phase 1: `cargo test -p vmm overlay` — overlay engine correctness
- Phase 3: `cargo test -p vmm persistence` — save/restore round-trip

### Integration Tests
- Modify existing snapshot tests in `tests/integration_tests/functional/test_snapshot_basic.py`:
  - Add overlay-mode VM creation
  - Snapshot and restore with overlay
  - Verify data integrity after restore
- Add new test: `test_overlay_clone.py`:
  - Create VM with overlay, write data, snapshot with delta
  - Clone from delta, verify clone has correct data
  - Verify base image unchanged

### Manual Smoke Test
1. Create a base rootfs image (read-only)
2. Boot VM with overlay mode pointing to base + fresh overlay
3. Write files inside guest
4. Snapshot (with delta)
5. Restore from snapshot — verify files still present
6. Clone from delta to new VM — verify files present in clone
7. Verify base image byte-identical to original

### Performance Benchmarks
- Compare snapshot size: overlay delta vs full disk copy
- Compare snapshot time: write_delta() vs cp disk_image
- Compare restore time: apply_delta() + boot vs full image + boot
- Run SQLite benchmark inside overlay VM vs standard VM — verify no regression

---

## Deferred & Future Work

### Deferred in the plan

1. **Async/io_uring overlay (Phase 6)** — we only build the sync I/O path. The async path is more complex (two file descriptors in one io_uring instance, split reads across base/overlay with async completions). Deferred because agent tool calls are typically light I/O where sync is fine.
2. **Incremental deltas** — our delta captures all dirty blocks since VM creation (cumulative). A further optimization would be capturing only blocks dirtied *since the last snapshot* — so each successive snapshot gets smaller. This requires bitmap reset after snapshot and delta chaining, which adds complexity.
3. **Overlay chains** — we support one overlay on top of one base. QCOW2-style stacking (overlay on overlay on overlay) isn't in the plan. Not needed for the agent sandbox use case.

### Mentioned but light on detail

4. **Base image integrity verification** — Deferred. The risk is that if someone modifies the shared read-only base image while VMs are running (or between snapshot and restore), overlay VMs will silently read corrupted data — the bitmap says "clean, read from base" but the base content has changed since the overlay was created. The fix would be to compute SHA256 of the base image at overlay creation time, store the hash in the overlay state, and verify it on restore. However, this adds 2-3 seconds of hashing overhead per boot for a 2GB image, which defeats the purpose of fast snapshot/restore. The base image is operator-controlled infrastructure — typically on a read-only mount or built as an immutable artifact. If integrity verification is needed later, the recommended approach is: hash once at image build time, store the hash alongside the image file, and verify only on first use or behind an optional configuration flag.
5. **Overlay space management** — Deferred as an operational concern, not a code change. The risk is that a rogue agent writes excessive data, growing the sparse overlay file until it fills the host disk and affects other VMs. However, this is the same class of problem as any guest filling a regular disk image — Firecracker doesn't manage host disk space for non-overlay devices either. The overlay actually improves the situation since it's sparse and only grows as blocks are written, versus a pre-allocated full image. Operators should handle this at the infrastructure level: use separate filesystems or tmpfs with size limits per VM's overlay directory, apply Firecracker's existing block device rate limiters, and monitor host disk usage with standard tooling.
6. **Overlay garbage collection** — Implemented. Added `VIRTIO_BLK_T_DISCARD` support for overlay devices. When the guest trims/discards blocks, the overlay engine clears the bitmap bits and punches holes in the overlay file with `fallocate(FALLOC_FL_PUNCH_HOLE)` to reclaim host disk space. The `VIRTIO_BLK_F_DISCARD` feature is advertised to the guest for overlay devices. Non-overlay devices treat discard as a no-op.
7. **Hot-update interaction** — Implemented. `update_disk_image()` is rejected for overlay devices with a clear error. Hot-swapping a file would make the bitmap inconsistent with the new file content, leading to silent data corruption. Resetting the bitmap is also unsafe because the guest kernel's page cache would be stale. This matches the pattern used by vhost-user block devices which also reject hot-update. Users should snapshot and restore with new configuration instead.
8. **Performance tuning** — Deferred until benchmarks identify bottlenecks. `fadvise(RANDOM)` is safe and can be added as a one-liner. `O_DIRECT` needs alignment testing. `madvise`/`fadvise(DONTNEED)` need timing analysis. None require interface changes — can be added anytime after benchmarking.

