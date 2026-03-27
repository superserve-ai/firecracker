#!/bin/bash
set -e

mkdir -p ~/benchmark
cd ~/benchmark
rm -f vmlinux rootfs.squashfs

ARCH=x86_64
CI_VERSION=v1.15

# Download kernel
latest_kernel=$(curl -s "http://spec.ccfc.min.s3.amazonaws.com/?prefix=firecracker-ci/${CI_VERSION}/${ARCH}/vmlinux-&list-type=2" | grep -oP "(?<=<Key>)(firecracker-ci/${CI_VERSION}/${ARCH}/vmlinux-[0-9]+\.[0-9]+\.[0-9]{1,3})(?=</Key>)" | sort -V | tail -1)
echo "Kernel: $latest_kernel"
wget -q -O vmlinux "https://s3.amazonaws.com/spec.ccfc.min/${latest_kernel}"

# Download rootfs
latest_rootfs=$(curl -s "http://spec.ccfc.min.s3.amazonaws.com/?prefix=firecracker-ci/${CI_VERSION}/${ARCH}/ubuntu-&list-type=2" | grep -oP "(?<=<Key>)(firecracker-ci/${CI_VERSION}/${ARCH}/ubuntu-[0-9]+\.[0-9]+\.squashfs)(?=</Key>)" | sort -V | tail -1)
echo "Rootfs: $latest_rootfs"
wget -q -O rootfs.squashfs "https://s3.amazonaws.com/spec.ccfc.min/${latest_rootfs}"

# Convert squashfs to writable ext4 (1GB to have room for SQLite)
echo "Converting to ext4..."
sudo apt-get install -y -qq squashfs-tools
sudo unsquashfs -d /tmp/rootfs_extract rootfs.squashfs
dd if=/dev/zero of=rootfs.ext4 bs=1M count=1024 status=none
mkfs.ext4 -q rootfs.ext4
sudo mkdir -p /tmp/rootfs_mount
sudo mount rootfs.ext4 /tmp/rootfs_mount
sudo cp -a /tmp/rootfs_extract/* /tmp/rootfs_mount/

# Install SQLite benchmark script inside the rootfs
sudo tee /tmp/rootfs_mount/root/sqlite_bench.py > /dev/null << 'PYSCRIPT'
import sqlite3
import time
import random
import os

random.seed(42)
DB_PATH = "/tmp/bench.db"
ROWS = 60000

def timed(name, fn):
    start = time.monotonic()
    result = fn()
    elapsed = time.monotonic() - start
    print(f"  {name}: {elapsed*1000:.1f}ms")
    return elapsed

def run_benchmark():
    if os.path.exists(DB_PATH):
        os.remove(DB_PATH)

    conn = sqlite3.connect(DB_PATH)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA cache_size=-65536")

    conn.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT, value REAL)")
    conn.execute("CREATE INDEX idx_value ON test(value)")
    conn.execute("CREATE TABLE aux (id INTEGER PRIMARY KEY, test_id INTEGER, label TEXT)")

    total = 0.0

    # Batch insert
    def batch_insert():
        data = [(f"name_{i}", random.random() * 1000) for i in range(ROWS)]
        conn.executemany("INSERT INTO test (name, value) VALUES (?, ?)", data)
        conn.commit()
    total += timed("batch_insert (60k rows)", batch_insert)

    # SELECT COUNT
    def count_rows():
        return conn.execute("SELECT COUNT(*) FROM test").fetchone()
    total += timed("SELECT COUNT(*)", count_rows)

    # Range query
    def range_query():
        return conn.execute("SELECT * FROM test WHERE value BETWEEN 100 AND 200").fetchall()
    total += timed("range_query", range_query)

    # LIKE query (full scan)
    def like_query():
        return conn.execute("SELECT * FROM test WHERE name LIKE '%999%'").fetchall()
    total += timed("LIKE query (full scan)", like_query)

    # Update
    def update_rows():
        conn.execute("UPDATE test SET value = value + 1 WHERE name = 'name_100'")
        conn.commit()
    total += timed("update", update_rows)

    # Delete
    def delete_rows():
        conn.execute("DELETE FROM test WHERE name = 'name_200'")
        conn.commit()
    total += timed("delete", delete_rows)

    # Transaction insert
    def txn_insert():
        conn.execute("BEGIN")
        for i in range(1000):
            conn.execute("INSERT INTO test (name, value) VALUES (?, ?)", (f"txn_{i}", random.random()))
        conn.execute("COMMIT")
    total += timed("transaction_insert (1k rows)", txn_insert)

    # Aggregates
    def aggregates():
        return conn.execute("SELECT AVG(value), MIN(value), MAX(value), SUM(value) FROM test").fetchone()
    total += timed("aggregates", aggregates)

    # Join
    def join_query():
        conn.executemany("INSERT INTO aux (test_id, label) VALUES (?, ?)",
                        [(i, f"label_{i}") for i in range(1, 1001)])
        conn.commit()
        return conn.execute("SELECT t.name, a.label FROM test t JOIN aux a ON t.id = a.test_id WHERE t.value > 500").fetchall()
    total += timed("join_query", join_query)

    # Concurrent reads (sequential, single-threaded for simplicity)
    def concurrent_reads():
        for _ in range(2000):
            conn.execute("SELECT * FROM test WHERE value BETWEEN ? AND ?",
                        (random.random() * 500, random.random() * 500 + 500)).fetchall()
    total += timed("2000 range reads", concurrent_reads)

    conn.close()
    print(f"\n  TOTAL: {total*1000:.1f}ms")

if __name__ == "__main__":
    print("=== SQLite Benchmark ===")
    for run in range(3):
        print(f"\nRun {run+1}/3:")
        run_benchmark()
PYSCRIPT

sudo umount /tmp/rootfs_mount
sudo rm -rf /tmp/rootfs_extract /tmp/rootfs_mount

ls -lh ~/benchmark/
echo "=== Setup done ==="
