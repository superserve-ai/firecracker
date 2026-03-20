// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Baseline I/O benchmarks — current Firecracker behaviour before the overlay
// engine is introduced.  Run these first on a bare-metal Linux host to
// establish the numbers that overlay_io.rs benchmarks will be compared against.
//
// What we measure and why
// ───────────────────────
// write_seek_and_write     The pattern SyncFileEngine uses: seek() then write().
//                          This is every guest block write today.
//
// write_pwrite             pwrite(2) / write_at() — no separate seek syscall.
//                          Our OverlayFileEngine uses this.  Comparing with
//                          seek+write shows whether eliminating the seek matters.
//
// read_seek_and_read       The pattern SyncFileEngine uses for reads.
//
// read_pread               pread(2) / read_at() — what OverlayFileEngine does.
//
// reset_full_file_copy     std::fs::copy() of an entire disk image.
//                          This is the current "reset a sandbox" cost.
//                          The overlay reset replaces this with ftruncate + bitmap
//                          clear — a completely different cost curve.
//
// reset_file_copy_partial  Copy only N% of the disk image.  Simulates what
//                          a dirty-bitmap snapshot/restore would need if the
//                          baseline tried to do it without the overlay.
//
// snapshot_full_copy       Copying a full disk to object storage staging area.
//                          Baseline for Phase 2 (bitmap-accelerated snapshots).
//
// Run on a bare-metal Linux host (Hetzner AX41-NVMe or similar):
//   cargo bench -p vmm --bench baseline_io

use std::fs::File;
use std::hint::black_box;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

// ── Constants ──────────────────────────────────────────────────────────────

/// Number of 4 KiB blocks — matches overlay_io bench so results are comparable.
const NUM_BLOCKS: usize = 65_536;
const BLOCK_SIZE: usize = 4096;
const DISK_BYTES: u64 = (NUM_BLOCKS * BLOCK_SIZE) as u64; // 256 MiB

// ── Helpers ────────────────────────────────────────────────────────────────

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    ns.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

/// Create an anonymous temp file (unlinked immediately, lives via fd).
fn temp_file() -> File {
    let path = std::env::temp_dir().join(format!(
        "baseline_bench_{}_{}",
        std::process::id(),
        rand_suffix()
    ));
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("temp file");
    let _ = std::fs::remove_file(&path);
    f
}

/// Create a temp file pre-filled with `size` bytes of data.
fn filled_temp_file(size: u64) -> File {
    let f = temp_file();
    // Sparse-allocate: write one byte at the end so the file has the right
    // size without faulting in all pages.  Actual content doesn't matter
    // for throughput benchmarks.
    f.write_at(&[0u8], size - 1).unwrap();
    f
}

fn shuffle(v: &mut Vec<usize>) {
    let mut s = rand_suffix();
    for i in (1..v.len()).rev() {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (s as usize) % (i + 1);
        v.swap(i, j);
    }
}

// ── Write benchmarks ───────────────────────────────────────────────────────

fn bench_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline/write");
    group.throughput(Throughput::Bytes(DISK_BYTES));

    let buf = vec![0xABu8; BLOCK_SIZE];

    // ── seek + write (current SyncFileEngine pattern) ──────────────────────
    group.bench_function("seek_and_write/sequential", |b| {
        b.iter_batched(
            || {
                let mut f = filled_temp_file(DISK_BYTES);
                // Seek to start so the first seek in the loop is a real one.
                f.seek(SeekFrom::Start(0)).unwrap();
                f
            },
            |mut f| {
                for block in 0..NUM_BLOCKS {
                    f.seek(SeekFrom::Start(black_box(block as u64 * BLOCK_SIZE as u64)))
                        .unwrap();
                    f.write_all(&buf).unwrap();
                }
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    let mut random_order: Vec<usize> = (0..NUM_BLOCKS).collect();
    shuffle(&mut random_order);
    let random_order = random_order.clone();
    group.bench_function("seek_and_write/random", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |mut f| {
                for &block in &random_order {
                    f.seek(SeekFrom::Start(black_box(block as u64 * BLOCK_SIZE as u64)))
                        .unwrap();
                    f.write_all(&buf).unwrap();
                }
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    // ── pwrite / write_at (OverlayFileEngine pattern) ─────────────────────
    // Measuring this in the baseline suite shows whether eliminating the
    // seek syscall has any measurable benefit on its own, independent of
    // the overlay logic.
    group.bench_function("pwrite/sequential", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |f| {
                for block in 0..NUM_BLOCKS {
                    f.write_at(&buf, black_box(block as u64 * BLOCK_SIZE as u64))
                        .unwrap();
                }
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    group.bench_function("pwrite/random", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |f| {
                for &block in &random_order {
                    f.write_at(&buf, black_box(block as u64 * BLOCK_SIZE as u64))
                        .unwrap();
                }
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

// ── Read benchmarks ────────────────────────────────────────────────────────

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline/read");
    group.throughput(Throughput::Bytes(DISK_BYTES));

    let mut buf = vec![0u8; BLOCK_SIZE];

    // ── seek + read (current SyncFileEngine pattern) ───────────────────────
    group.bench_function("seek_and_read/sequential", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |mut f| {
                for block in 0..NUM_BLOCKS {
                    f.seek(SeekFrom::Start(black_box(block as u64 * BLOCK_SIZE as u64)))
                        .unwrap();
                    f.read_exact(&mut buf).unwrap();
                }
                black_box(&buf);
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    let mut random_order: Vec<usize> = (0..NUM_BLOCKS).collect();
    shuffle(&mut random_order);
    let random_order = random_order.clone();
    group.bench_function("seek_and_read/random", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |mut f| {
                for &block in &random_order {
                    f.seek(SeekFrom::Start(black_box(block as u64 * BLOCK_SIZE as u64)))
                        .unwrap();
                    f.read_exact(&mut buf).unwrap();
                }
                black_box(&buf);
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    // ── pread / read_at (OverlayFileEngine pattern) ────────────────────────
    group.bench_function("pread/sequential", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |f| {
                for block in 0..NUM_BLOCKS {
                    f.read_at(&mut buf, black_box(block as u64 * BLOCK_SIZE as u64))
                        .unwrap();
                }
                black_box(&buf);
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    group.bench_function("pread/random", |b| {
        b.iter_batched(
            || filled_temp_file(DISK_BYTES),
            |f| {
                for &block in &random_order {
                    f.read_at(&mut buf, black_box(block as u64 * BLOCK_SIZE as u64))
                        .unwrap();
                }
                black_box(&buf);
                black_box(f)
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

// ── Reset benchmarks ───────────────────────────────────────────────────────

fn bench_reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline/reset");

    // Full file copy — this is what "reset a sandbox to clean state" costs
    // today.  Compare this against overlay_io::reset (ftruncate + clear_all).
    group
        .throughput(Throughput::Bytes(DISK_BYTES))
        .bench_function("full_file_copy", |b| {
            b.iter_batched(
                || {
                    // Source: the "golden" base image.
                    let src_path = std::env::temp_dir().join(format!(
                        "baseline_src_{}_{}",
                        std::process::id(),
                        rand_suffix()
                    ));
                    let dst_path = std::env::temp_dir().join(format!(
                        "baseline_dst_{}_{}",
                        std::process::id(),
                        rand_suffix()
                    ));
                    let src = File::options()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&src_path)
                        .unwrap();
                    src.write_at(&[0u8], DISK_BYTES - 1).unwrap();
                    drop(src);
                    (src_path, dst_path)
                },
                |(src_path, dst_path)| {
                    std::fs::copy(&src_path, &dst_path).unwrap();
                    // Cleanup
                    let _ = std::fs::remove_file(&src_path);
                    let _ = std::fs::remove_file(&dst_path);
                },
                BatchSize::PerIteration,
            )
        });

    // Partial copy — simulates a naive "only copy dirty blocks" implementation
    // without the overlay, to show that the approach itself (not just our code)
    // is the win.
    for dirty_fraction in [0.01, 0.10, 0.50, 1.00] {
        let dirty_bytes = ((DISK_BYTES as f64) * dirty_fraction) as u64;
        group
            .throughput(Throughput::Bytes(dirty_bytes))
            .bench_with_input(
                BenchmarkId::new(
                    "partial_copy",
                    format!("{:.0}%_dirty", dirty_fraction * 100.0),
                ),
                &dirty_bytes,
                |b, &dirty_bytes| {
                    b.iter_batched(
                        || {
                            let src_path = std::env::temp_dir().join(format!(
                                "bl_src_{}_{}",
                                std::process::id(),
                                rand_suffix()
                            ));
                            let dst_path = std::env::temp_dir().join(format!(
                                "bl_dst_{}_{}",
                                std::process::id(),
                                rand_suffix()
                            ));
                            let src = File::options()
                                .read(true)
                                .write(true)
                                .create(true)
                                .truncate(true)
                                .open(&src_path)
                                .unwrap();
                            src.write_at(&[0u8], DISK_BYTES - 1).unwrap();
                            let dst = File::options()
                                .read(true)
                                .write(true)
                                .create(true)
                                .truncate(true)
                                .open(&dst_path)
                                .unwrap();
                            (src, dst, src_path, dst_path)
                        },
                        |(src, dst, src_path, dst_path)| {
                            // Copy block-by-block for the dirty portion, as a
                            // naive per-block snapshot would.
                            let dirty_blocks =
                                (dirty_bytes / BLOCK_SIZE as u64) as usize;
                            let mut buf = vec![0u8; BLOCK_SIZE];
                            for block in 0..dirty_blocks {
                                let off = block as u64 * BLOCK_SIZE as u64;
                                src.read_at(&mut buf, black_box(off)).unwrap();
                                dst.write_at(&buf, off).unwrap();
                            }
                            let _ = std::fs::remove_file(&src_path);
                            let _ = std::fs::remove_file(&dst_path);
                        },
                        BatchSize::PerIteration,
                    )
                },
            );
    }

    group.finish();
}

// ── Snapshot baseline ──────────────────────────────────────────────────────

fn bench_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline/snapshot");

    // Full snapshot — serialize the entire disk image.  This is what
    // Firecracker does today on CreateSnapshot for a block device.
    // Compare against overlay_io::snapshot_scan (only dirty blocks).
    group
        .throughput(Throughput::Bytes(DISK_BYTES))
        .bench_function("full_disk_serialize", |b| {
            b.iter_batched(
                || filled_temp_file(DISK_BYTES),
                |src| {
                    let dst = filled_temp_file(DISK_BYTES);
                    let mut buf = vec![0u8; BLOCK_SIZE];
                    for block in 0..NUM_BLOCKS {
                        let off = block as u64 * BLOCK_SIZE as u64;
                        src.read_at(&mut buf, off).unwrap();
                        dst.write_at(&buf, off).unwrap();
                    }
                    black_box(dst)
                },
                BatchSize::PerIteration,
            )
        });

    group.finish();
}

// ── Criterion entrypoint ───────────────────────────────────────────────────

criterion_group! {
    name = baseline_benches;
    config = Criterion::default()
        .sample_size(20)        // fewer samples — full-copy iterations are slow
        .noise_threshold(0.05);
    targets =
        bench_write,
        bench_read,
        bench_reset,
        bench_snapshot
}

criterion_main!(baseline_benches);
