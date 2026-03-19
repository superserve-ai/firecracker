// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Benchmarks for the overlay block engine and dirty bitmap:
//
//   DirtyBitmap
//     bitmap_set_sequential   - set every bit in order (write-heavy workload)
//     bitmap_set_random       - set bits at random offsets
//     bitmap_clear_all        - zero the whole bitmap (reset critical path)
//     bitmap_dirty_count      - count set bits (snapshot size estimation)
//     bitmap_iter_dirty       - iterate over dirty blocks (snapshot serialisation)
//
//   OverlayFileEngine
//     overlay_write_sequential - sequential 4 KiB block writes
//     overlay_write_random     - random 4 KiB block writes
//     overlay_read_clean       - read 4 KiB blocks that are still in base
//     overlay_read_dirty       - read 4 KiB blocks that are in upper
//     overlay_reset            - truncate upper + clear bitmap (the killer metric)
//     overlay_snapshot_scan    - iterate dirty bitmap to build a snapshot manifest

use std::fs::File;
use std::hint::black_box;
use std::io::Write;
use std::os::unix::fs::FileExt;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use vmm::devices::virtio::block::virtio::io::dirty_bitmap::DirtyBitmap;
use vmm::devices::virtio::block::virtio::io::overlay_io::{
    DEFAULT_BLOCK_SIZE, OverlayFileEngine,
};

// ── Constants ──────────────────────────────────────────────────────────────

/// Number of blocks used in overlay benchmarks. 256 MiB @ 4 KiB/block.
const NUM_BLOCKS: usize = 65_536;
/// All benchmarks use the production block size.
const BLOCK_SIZE: u64 = DEFAULT_BLOCK_SIZE;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Create a named temporary file that is deleted when the returned `File` is
/// dropped.  We use `std::fs` directly so that benches stay independent.
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
    // Unlink now — the file stays alive through the File handle.
    let _ = std::fs::remove_file(&path);
    file
}

/// Very cheap LCG-based pseudo-random u64, seeded from the clock.
fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)
}

/// Shuffle a slice in-place with a deterministic LCG so that random-access
/// benchmarks visit every block exactly once in unpredictable order.
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

/// Create an overlay engine backed by two anonymous temp files.
/// `base_size_bytes` bytes are written to base so reads don't hit EOF.
fn make_engine(base_size_bytes: u64) -> OverlayFileEngine {
    let mut base = temp_file();
    let upper = temp_file();

    if base_size_bytes > 0 {
        // Write a sparse base: just seek and write one byte at the end so the
        // OS knows the file size without allocating real blocks.
        base.write_at(&[0u8], base_size_bytes - 1).unwrap();
    }

    OverlayFileEngine::with_block_size(
        base,
        upper,
        NUM_BLOCKS,
        BLOCK_SIZE,
    )
}

// ── DirtyBitmap benchmarks ─────────────────────────────────────────────────

fn bench_bitmap(c: &mut Criterion) {
    let mut group = c.benchmark_group("DirtyBitmap");
    group.throughput(Throughput::Elements(NUM_BLOCKS as u64));

    // Sequential set — simulates a workload that writes every block once.
    group.bench_function("set_sequential", |b| {
        b.iter_batched(
            || DirtyBitmap::new(NUM_BLOCKS),
            |mut bm| {
                for i in 0..NUM_BLOCKS {
                    bm.set(black_box(i));
                }
                black_box(bm)
            },
            BatchSize::SmallInput,
        )
    });

    // Random set — simulates fragmented, real-world agent writes.
    let mut random_order: Vec<usize> = (0..NUM_BLOCKS).collect();
    shuffle(&mut random_order);
    let random_order = random_order; // freeze
    group.bench_function("set_random", |b| {
        b.iter_batched(
            || DirtyBitmap::new(NUM_BLOCKS),
            |mut bm| {
                for &i in &random_order {
                    bm.set(black_box(i));
                }
                black_box(bm)
            },
            BatchSize::SmallInput,
        )
    });

    // clear_all — this is the reset hot path: how fast can we zero the bitmap?
    group.bench_function("clear_all", |b| {
        b.iter_batched(
            || {
                let mut bm = DirtyBitmap::new(NUM_BLOCKS);
                for i in 0..NUM_BLOCKS {
                    bm.set(i);
                }
                bm
            },
            |mut bm| {
                bm.clear_all();
                black_box(bm)
            },
            BatchSize::SmallInput,
        )
    });

    // dirty_block_count — called before every snapshot to estimate size.
    group.bench_function("dirty_block_count", |b| {
        let mut bm = DirtyBitmap::new(NUM_BLOCKS);
        for i in (0..NUM_BLOCKS).step_by(2) {
            bm.set(i); // 50 % dirty
        }
        b.iter(|| black_box(bm.dirty_block_count()))
    });

    // iter_dirty — drives snapshot serialisation: visit each dirty block once.
    group.bench_function("iter_dirty", |b| {
        let mut bm = DirtyBitmap::new(NUM_BLOCKS);
        for i in (0..NUM_BLOCKS).step_by(2) {
            bm.set(i); // 50 % dirty
        }
        b.iter(|| {
            let mut count = 0usize;
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
    let base_size = NUM_BLOCKS as u64 * BLOCK_SIZE;
    let buf = vec![0xABu8; BLOCK_SIZE as usize];

    // Sequential write — represents a streaming workload (compiling, unpacking).
    group
        .throughput(Throughput::Bytes(base_size))
        .bench_function("sequential", |b| {
            b.iter_batched(
                || make_engine(base_size),
                |mut engine| {
                    for block in 0..NUM_BLOCKS {
                        engine
                            .write(black_box(block as u64 * BLOCK_SIZE), &buf)
                            .unwrap();
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
        b.iter_batched(
            || make_engine(base_size),
            |mut engine| {
                for &block in &random_order {
                    engine
                        .write(black_box(block as u64 * BLOCK_SIZE), &buf)
                        .unwrap();
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
    let base_size = NUM_BLOCKS as u64 * BLOCK_SIZE;
    let mut buf = vec![0u8; BLOCK_SIZE as usize];

    // Read clean blocks (from base) — the common case right after boot.
    group
        .throughput(Throughput::Bytes(base_size))
        .bench_function("clean_from_base", |b| {
            b.iter_batched(
                || make_engine(base_size),
                |engine| {
                    for block in 0..NUM_BLOCKS {
                        engine
                            .read(black_box(block as u64 * BLOCK_SIZE), &mut buf)
                            .unwrap();
                    }
                    black_box(&buf);
                    black_box(engine)
                },
                BatchSize::PerIteration,
            )
        });

    // Read dirty blocks (from upper) — after writes, reads must hit upper.
    group.bench_function("dirty_from_upper", |b| {
        b.iter_batched(
            || {
                let mut engine = make_engine(base_size);
                let write_buf = vec![0xBBu8; BLOCK_SIZE as usize];
                for block in 0..NUM_BLOCKS {
                    engine
                        .write(block as u64 * BLOCK_SIZE, &write_buf)
                        .unwrap();
                }
                engine
            },
            |engine| {
                for block in 0..NUM_BLOCKS {
                    engine
                        .read(black_box(block as u64 * BLOCK_SIZE), &mut buf)
                        .unwrap();
                }
                black_box(&buf);
                black_box(engine)
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

fn bench_overlay_reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("OverlayFileEngine/reset");
    let base_size = NUM_BLOCKS as u64 * BLOCK_SIZE;

    // This is THE key metric: how fast can we reclaim a sandbox for the next
    // agent run?  Compare against the baseline of copying a full disk image.
    for dirty_fraction in [0.01, 0.10, 0.50, 1.00] {
        let dirty_blocks = ((NUM_BLOCKS as f64) * dirty_fraction) as usize;
        group
            .throughput(Throughput::Elements(dirty_blocks as u64))
            .bench_with_input(
                BenchmarkId::new(
                    "reset",
                    format!("{:.0}%_dirty", dirty_fraction * 100.0),
                ),
                &dirty_blocks,
                |b, &dirty_blocks| {
                    b.iter_batched(
                        || {
                            let mut engine = make_engine(base_size);
                            let buf = vec![0u8; BLOCK_SIZE as usize];
                            for block in 0..dirty_blocks {
                                engine.write(block as u64 * BLOCK_SIZE, &buf).unwrap();
                            }
                            engine
                        },
                        |mut engine| {
                            engine.reset().unwrap();
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
    let base_size = NUM_BLOCKS as u64 * BLOCK_SIZE;

    // How fast can we build the list of dirty blocks to include in a
    // snapshot?  Measured at different dirty fractions so we can see whether
    // it scales with dirty count or total block count.
    for dirty_fraction in [0.01, 0.10, 0.50, 1.00] {
        let dirty_blocks = ((NUM_BLOCKS as f64) * dirty_fraction) as usize;
        group
            .throughput(Throughput::Elements(dirty_blocks as u64))
            .bench_with_input(
                BenchmarkId::new(
                    "scan",
                    format!("{:.0}%_dirty", dirty_fraction * 100.0),
                ),
                &dirty_blocks,
                |b, &dirty_blocks| {
                    let mut engine = make_engine(base_size);
                    let buf = vec![0u8; BLOCK_SIZE as usize];
                    for block in 0..dirty_blocks {
                        engine.write(block as u64 * BLOCK_SIZE, &buf).unwrap();
                    }
                    b.iter(|| {
                        let count: usize = engine.bitmap().iter_dirty().count();
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
