# Block-Level COW Overlay — Benchmark Results

**Date:** March 25, 2026
**Branch:** `feat/block-overlay-cow`
**PR:** [superserve-ai/firecracker#5](https://github.com/superserve-ai/firecracker/pull/5)

---

## Hardware

**Bare metal** — Hetzner dedicated server
- CPU: AMD Ryzen 9 7950X3D 16-Core (32 threads)
- RAM: 128GB
- Kernel: 6.8.0
- Storage: NVMe
- Guest VM: 2 vCPUs, 256MB RAM, ~396MB rootfs

---

## Snapshot / Clone / Restore

| Metric | Sync (vanilla) | Overlay (ours) | Improvement |
|---|---:|---:|---|
| Snapshot time | 225ms | 223ms | same (memory-dominated) |
| Clone disk cost | 205ms (full 396MB copy) | 0ms (560KB delta) | **no copy needed** |
| **Total clone cost** | **430ms** | **223ms** | **~2x faster** |
| Restore time | 7ms | 8ms | same |
| Clone disk size | 396MB | 560KB | **700x smaller** |

Snapshot time is identical because it's dominated by the 256MB guest memory dump. The overlay advantage is in cloning — no full disk copy needed, just a 560KB delta file.

---

## Reset (return to clean state)

| Method | Time | Speedup |
|---|---:|---|
| Sync (full disk copy, 396MB) | 190ms | baseline |
| Overlay (truncate + clear bitmap) | 1-2ms | **~100x faster** |

Overlay reset is two syscalls: `truncate(0)` + `truncate(disk_size)`. No data copied. The bitmap clear is sub-microsecond.

---

## Per-Clone Cost at Scale

| Clones | Sync | Overlay |
|---|---:|---:|
| 1 | 205ms | ~0ms |
| 10 | 2,050ms | ~0ms |
| 100 | 20,500ms | ~0ms |
| 1000 | 205,000ms (~3.4 min) | ~0ms |

Overlay cloning is O(1) — create an empty sparse file pointing at the shared base. Sync cloning is O(disk_size) per clone.

---

## Disk Usage at Scale

| VMs | Sync | Overlay |
|---|---:|---:|
| 1 | 396MB | 396MB (shared base) + ~0MB overlay |
| 10 | 3.96GB | 396MB + ~5MB overlays |
| 100 | 39.6GB | 396MB + ~50MB overlays |

All overlay VMs share one read-only base image. Each VM's overlay is sparse — only written blocks take space.

