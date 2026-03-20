// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Benchmarks for the overlay block engine and dirty bitmap:
//
//   DirtyBitmap
//     set_sequential    - set every block in order (write-heavy workload)
//     set_random        - set blocks at random offsets
//     clear_all         - zero the whole bitmap (reset critical path)
//     dirty_block_count - count set bits (snapshot size estimation)
//     iter_dirty        - iterate over dirty blocks (snapshot serialisation)
//
//   OverlayFileEngine
//     write/sequential  - sequential 4 KiB block writes
//     write/random      - random 4 KiB block writes
//     read/clean        - read 4 KiB blocks that are still in base
//     read/dirty        - read 4 KiB blocks that are in upper
//     reset             - replace overlay + clear bitmap (the killer metric)
//     snapshot_scan     - iterate dirty bitmap to build a snapshot manifest

use std::fs::File;
use std::hint::black_box;
use std::io::Write;
use std::os::unix::fs::FileExt;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use vmm::devices::virtio::block::virtio::io::dirty_bitmap::{DEFAULT_BLOCK_SIZE, DirtyBitmap};
use vmm::devices::virtio::block::virtio::io::overlay_io::OverlayFileEngine;
use vmm::vmm_config::machine_config::HugePageConfig;
use vmm::vstate::memory::{self, GuestAddress, GuestMemoryMmap, GuestRegionMmapExt};

// ── Constants ──────────────────────────────────────────────────────────────

/// Number of blocks used in overlay benchmarks. 256 MiB @ 4 KiB/block.
const NUM_BLOCKS: usize = 65_536;
/// Production block size.
const BLOCK_SIZE: u32 = DEFAULT_BLOCK_SIZE;
/// Disk size in bytes.
const DISK_SIZE: u64 = NUM_BLOCKS as u64 * BLOCK_SIZE as u64;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Create an anonymous temp file that is deleted when the `File` is dropped.
fn temp_file() -> File {
    let path = std::env::temp_dir().join(format!(
        "overlay_bench_{}_{}",
        std::process::id(),
        rand_u64()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("failed to create temp file");
    let _ = std::fs::remove_file(&path);
    file
}

/// Cheap LCG-based pseudo-random u64, seeded from the clock.
fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    seed.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

/// Shuffle a slice in-place with a deterministic LCG.
fn shuffle(v: &mut Vec<usize>) {
    let mut state = rand_u64();
    let n = v.len();
    for i in (1..n).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state as usize) % (i + 1);
        v.swap(i, j);
    }
}

/// Create a `GuestMemoryMmap` backed by anonymous memory of `size` bytes.
fn create_mem(size: usize) -> GuestMemoryMmap {
    let regions = memory::anonymous(
        std::iter::once((GuestAddress(0), size)),
        false,
        HugePageConfig::None,
    )
    .expect("failed to allocate guest memory");
    GuestRegionMmapExt::into_region_ext(regions)
}

/// Create an overlay engine backed by two anonymous temp files.
/// The base file is written sparse so reads don't hit EOF.
fn make_engine() -> OverlayFileEngine {
    let mut base = temp_file();
    let upper = temp_file();
    // Sparse base: write one byte at the end to set file size.
    base.write_at(&[0u8], DISK_SIZE - 1).unwrap();
    // Overlay must be same size (sparse is fine).
    upper.set_len(DISK_SIZE).unwrap();
    OverlayFileEngine::from_files(base, upper, DISK_SIZE, BLOCK_SIZE, None).unwrap()
}

/// Write `dirty_blocks` sequential blocks into an engine via the overlay write path.
fn dirty_engine(engine: &mut OverlayFileEngine, dirty_blocks: usize) {
    let mem = create_mem(BLOCK_SIZE as usize);
    for block in 0..dirty_blocks {
        let offset = block as u64 * u64::from(BLOCK_SIZE);
        engine.write(offset, &mem, GuestAddress(0), BLOCK_SIZE).unwrap();
    }
}

// ── DirtyBitmap benchmarks ─────────────────────────────────────────────────

fn bench_bitmap(c: &mut Criterion) {
    let mut group = c.benchmark_group("DirtyBitmap");
    group.throughput(Throughput::Elements(NUM_BLOCKS as u64));

    // Sequential set — simulates a workload that writes every block once.
    group.bench_function("set_sequential", |b| {
        b.iter_batched(
            || DirtyBitmap::new(DISK_SIZE, BLOCK_SIZE).unwrap(),
            |mut bm| {
                for i in 0..NUM_BLOCKS {
                    bm.set(black_box(i as u64 * u64::from(BLOCK_SIZE)), BLOCK_SIZE);
                }
                black_box(bm)
            },
            BatchSize::SmallInput,
        )
    });

    // Random set — simulates fragmented, real-world agent writes.
    let mut random_order: Vec<usize> = (0..NUM_BLOCKS).collect();
    shuffle(&mut random_order);
    let random_order = random_order;
    group.bench_function("set_random", |b| {
        b.iter_batched(
            || DirtyBitmap::new(DISK_SIZE, BLOCK_SIZE).unwrap(),
            |mut bm| {
                for &i in &random_order {
                    bm.set(
                        black_box(i as u64 * u64::from(BLOCK_SIZE)),
                        BLOCK_SIZE,
                    );
                }
                black_box(bm)
            },
            BatchSize::SmallInput,
        )
    });

    // clear — this is the reset hot path: how fast can we zero the bitmap?
    group.bench_function("clear_all", |b| {
        b.iter_batched(
            || {
                let mut bm = DirtyBitmap::new(DISK_SIZE, BLOCK_SIZE).unwrap();
                for i in 0..NUM_BLOCKS {
                    bm.set(i as u64 * u64::from(BLOCK_SIZE), BLOCK_SIZE);
                }
                bm
            },
            |mut bm| {
                bm.clear();
                black_box(bm)
            },
            BatchSize::SmallInput,
        )
    });

    // dirty_count — called before every snapshot to estimate size.
    group.bench_function("dirty_block_count", |b| {
        let mut bm = DirtyBitmap::new(DISK_SIZE, BLOCK_SIZE).unwrap();
        for i in (0..NUM_BLOCKS).step_by(2) {
            bm.set(i as u64 * u64::from(BLOCK_SIZE), BLOCK_SIZE); // 50% dirty
        }
        b.iter(|| black_box(bm.dirty_count()))
    });

    // iter_dirty — drives snapshot serialisation: visit each dirty block once.
    group.bench_function("iter_dirty", |b| {
        let mut bm = DirtyBitmap::new(DISK_SIZE, BLOCK_SIZE).unwrap();
        for i in (0..NUM_BLOCKS).step_by(2) {
            bm.set(i as u64 * u64::from(BLOCK_SIZE), BLOCK_SIZE); // 50% dirty
        }
        b.iter(|| {
            let mut count = 0u64;
            for idx in bm.iter_dirty() {
                count += black_box(idx);
            }
            black_box(count)
        })
    });

    group.finish();
}

// ── OverlayFileEngine benchmarks ───────────────────────────────────────────

fn bench_overlay_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("OverlayFileEngine/write");

    // Sequential write — represents a streaming workload (compiling, unpacking).
    group
        .throughput(Throughput::Bytes(DISK_SIZE))
        .bench_function("sequential", |b| {
            let mem = create_mem(BLOCK_SIZE as usize);
            b.iter_batched(
                make_engine,
                |mut engine| {
                    for block in 0..NUM_BLOCKS {
                        let offset = black_box(block as u64 * u64::from(BLOCK_SIZE));
                        engine.write(offset, &mem, GuestAddress(0), BLOCK_SIZE).unwrap();
                    }
                    black_box(engine)
                },
                BatchSize::PerIteration,
            )
        });

    // Random write — represents an agent doing non-sequential file updates.
    let mut random_order: Vec<usize> = (0..NUM_BLOCKS).collect();
    shuffle(&mut random_order);
    let random_order = random_order;
    group.bench_function("random", |b| {
        let mem = create_mem(BLOCK_SIZE as usize);
        b.iter_batched(
            make_engine,
            |mut engine| {
                for &block in &random_order {
                    let offset = black_box(block as u64 * u64::from(BLOCK_SIZE));
                    engine.write(offset, &mem, GuestAddress(0), BLOCK_SIZE).unwrap();
                }
                black_box(engine)
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

fn bench_overlay_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("OverlayFileEngine/read");

    // Read clean blocks (from base) — the common case right after boot.
    group
        .throughput(Throughput::Bytes(DISK_SIZE))
        .bench_function("clean_from_base", |b| {
            let mem = create_mem(BLOCK_SIZE as usize);
            b.iter_batched(
                make_engine,
                |mut engine| {
                    for block in 0..NUM_BLOCKS {
                        let offset = black_box(block as u64 * u64::from(BLOCK_SIZE));
                        engine.read(offset, &mem, GuestAddress(0), BLOCK_SIZE).unwrap();
                    }
                    black_box(engine)
                },
                BatchSize::PerIteration,
            )
        });

    // Read dirty blocks (from overlay) — after writes, reads must hit upper.
    group.bench_function("dirty_from_upper", |b| {
        let mem = create_mem(BLOCK_SIZE as usize);
        b.iter_batched(
            || {
                let mut engine = make_engine();
                dirty_engine(&mut engine, NUM_BLOCKS);
                engine
            },
            |mut engine| {
                for block in 0..NUM_BLOCKS {
                    let offset = black_box(block as u64 * u64::from(BLOCK_SIZE));
                    engine.read(offset, &mem, GuestAddress(0), BLOCK_SIZE).unwrap();
                }
                black_box(engine)
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

fn bench_overlay_reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("OverlayFileEngine/reset");

    // This is THE key metric: how fast can we reclaim a sandbox for the next
    // agent run?  Compare against baseline/reset/full_file_copy.
    //
    // Reset = swap in a fresh overlay file + create a clean engine (empty bitmap).
    // The base file is shared and never touched.
    for dirty_fraction in [0.01, 0.10, 0.50, 1.00] {
        let dirty_blocks = ((NUM_BLOCKS as f64) * dirty_fraction) as usize;
        group
            .throughput(Throughput::Elements(dirty_blocks as u64))
            .bench_with_input(
                BenchmarkId::new("reset", format!("{:.0}%_dirty", dirty_fraction * 100.0)),
                &dirty_blocks,
                |b, &dirty_blocks| {
                    b.iter_batched(
                        || {
                            // Setup: engine with N dirty blocks already written.
                            let mut engine = make_engine();
                            dirty_engine(&mut engine, dirty_blocks);
                            engine
                        },
                        |mut engine| {
                            // Reset: swap in a fresh overlay, which implicitly
                            // creates a new engine state. We also replace the
                            // overlay file to reclaim disk space.
                            let new_overlay = temp_file();
                            new_overlay.set_len(DISK_SIZE).unwrap();
                            engine.update_overlay(new_overlay);
                            black_box(engine)
                        },
                        BatchSize::PerIteration,
                    )
                },
            );
    }

    group.finish();
}

fn bench_overlay_snapshot_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("OverlayFileEngine/snapshot_scan");

    // How fast can we build the list of dirty blocks to include in a snapshot?
    for dirty_fraction in [0.01, 0.10, 0.50, 1.00] {
        let dirty_blocks = ((NUM_BLOCKS as f64) * dirty_fraction) as usize;
        group
            .throughput(Throughput::Elements(dirty_blocks as u64))
            .bench_with_input(
                BenchmarkId::new("scan", format!("{:.0}%_dirty", dirty_fraction * 100.0)),
                &dirty_blocks,
                |b, &dirty_blocks| {
                    let mut engine = make_engine();
                    dirty_engine(&mut engine, dirty_blocks);
                    b.iter(|| {
                        let count: u64 = engine.bitmap().iter_dirty().count() as u64;
                        black_box(count)
                    })
                },
            );
    }

    group.finish();
}

// ── Criterion entrypoint ───────────────────────────────────────────────────

criterion_group! {
    name = overlay_benches;
    config = Criterion::default()
        .sample_size(50)
        .noise_threshold(0.05);
    targets =
        bench_bitmap,
        bench_overlay_write,
        bench_overlay_read,
        bench_overlay_reset,
        bench_overlay_snapshot_scan
}

criterion_main!(overlay_benches);
