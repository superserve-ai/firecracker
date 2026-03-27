#!/bin/bash

FC=~/firecracker/build/cargo_target/release/firecracker
BENCH=~/benchmark
KERNEL=$BENCH/vmlinux
ROOTFS=$BENCH/rootfs.ext4

BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off"

sudo chmod 666 /dev/kvm 2>/dev/null || true
sudo killall -9 firecracker 2>/dev/null || true
rm -f /tmp/fc-*.sock

echo "========================================="
echo "  Snapshot/Clone/Restore Benchmark"
echo "========================================="

run_snapshot_test() {
    local ENGINE=$1
    local LABEL=$2
    local DISK=$3
    local BASE=$4

    echo ""
    echo ">>> $LABEL"
    echo ""

    SOCK=/tmp/fc-snap.sock
    rm -f $SOCK

    # Boot VM
    $FC --api-sock $SOCK > /dev/null 2>&1 &
    FC_PID=$!
    sleep 0.5

    curl -s --unix-socket $SOCK -X PUT http://localhost/boot-source \
      -d "{\"kernel_image_path\": \"$KERNEL\", \"boot_args\": \"$BOOT_ARGS\"}" > /dev/null

    if [ "$ENGINE" = "Overlay" ]; then
        curl -s --unix-socket $SOCK -X PUT http://localhost/drives/rootfs \
          -d "{\"drive_id\": \"rootfs\", \"path_on_host\": \"$DISK\", \"base_path\": \"$BASE\", \"is_root_device\": true, \"is_read_only\": false, \"io_engine\": \"Overlay\"}" > /dev/null
    else
        curl -s --unix-socket $SOCK -X PUT http://localhost/drives/rootfs \
          -d "{\"drive_id\": \"rootfs\", \"path_on_host\": \"$DISK\", \"is_root_device\": true, \"is_read_only\": false, \"io_engine\": \"Sync\"}" > /dev/null
    fi

    curl -s --unix-socket $SOCK -X PUT http://localhost/machine-config \
      -d "{\"vcpu_count\": 2, \"mem_size_mib\": 256}" > /dev/null

    curl -s --unix-socket $SOCK -X PUT http://localhost/actions \
      -d "{\"action_type\": \"InstanceStart\"}" > /dev/null

    # Wait for VM to boot
    sleep 5

    # --- Measure Snapshot ---
    curl -s --unix-socket $SOCK -X PATCH http://localhost/vm \
      -d "{\"state\": \"Paused\"}" > /dev/null
    sleep 0.3

    mkdir -p $BENCH/snap_test

    if [ "$ENGINE" = "Overlay" ]; then
        mkdir -p $BENCH/snap_test/deltas
        S=$(date +%s%N)
        curl -s --unix-socket $SOCK -X PUT http://localhost/snapshot/create \
          -d "{\"snapshot_path\": \"$BENCH/snap_test/snap.bin\", \"mem_file_path\": \"$BENCH/snap_test/snap.mem\", \"block_delta_dir\": \"$BENCH/snap_test/deltas\"}" > /dev/null
        E=$(date +%s%N)
    else
        S=$(date +%s%N)
        curl -s --unix-socket $SOCK -X PUT http://localhost/snapshot/create \
          -d "{\"snapshot_path\": \"$BENCH/snap_test/snap.bin\", \"mem_file_path\": \"$BENCH/snap_test/snap.mem\"}" > /dev/null
        E=$(date +%s%N)
    fi
    SNAP_MS=$(( (E - S) / 1000000 ))

    kill $FC_PID 2>/dev/null; wait $FC_PID 2>/dev/null
    rm -f $SOCK

    # --- Measure full disk copy (what sync cloning needs) ---
    if [ "$ENGINE" = "Sync" ]; then
        S=$(date +%s%N)
        cp "$DISK" "$BENCH/snap_test/clone_disk.ext4"
        E=$(date +%s%N)
        CLONE_DISK_MS=$(( (E - S) / 1000000 ))
    else
        CLONE_DISK_MS=0
    fi

    # --- Measure Restore ---
    SOCK=/tmp/fc-restore.sock
    rm -f $SOCK

    $FC --api-sock $SOCK > /dev/null 2>&1 &
    FC_PID=$!
    sleep 0.5

    if [ "$ENGINE" = "Overlay" ]; then
        S=$(date +%s%N)
        curl -s --unix-socket $SOCK -X PUT http://localhost/snapshot/load \
          -d "{\"snapshot_path\": \"$BENCH/snap_test/snap.bin\", \"mem_backend\": {\"backend_path\": \"$BENCH/snap_test/snap.mem\", \"backend_type\": \"File\"}, \"block_delta_dir\": \"$BENCH/snap_test/deltas\", \"resume_vm\": true}" > /dev/null
        E=$(date +%s%N)
    else
        S=$(date +%s%N)
        curl -s --unix-socket $SOCK -X PUT http://localhost/snapshot/load \
          -d "{\"snapshot_path\": \"$BENCH/snap_test/snap.bin\", \"mem_backend\": {\"backend_path\": \"$BENCH/snap_test/snap.mem\", \"backend_type\": \"File\"}, \"resume_vm\": true}" > /dev/null
        E=$(date +%s%N)
    fi
    RESTORE_MS=$(( (E - S) / 1000000 ))

    kill $FC_PID 2>/dev/null; wait $FC_PID 2>/dev/null
    rm -f $SOCK

    # --- Report ---
    echo "  Snapshot time:  ${SNAP_MS}ms"
    if [ "$ENGINE" = "Sync" ]; then
        echo "  Disk copy time: ${CLONE_DISK_MS}ms (needed for cloning)"
        echo "  Total clone:    $(( SNAP_MS + CLONE_DISK_MS ))ms (snapshot + disk copy)"
    else
        DELTA_SIZE=$(du -h $BENCH/snap_test/deltas/rootfs.delta 2>/dev/null | cut -f1 || echo "N/A")
        echo "  Delta file:     ${DELTA_SIZE}"
        echo "  No disk copy needed (delta only)"
    fi
    echo "  Restore time:   ${RESTORE_MS}ms"
    echo ""
    echo "  Snapshot state:  $(du -h $BENCH/snap_test/snap.bin | cut -f1)"
    echo "  Guest memory:    $(du -h $BENCH/snap_test/snap.mem | cut -f1)"
    if [ "$ENGINE" = "Sync" ]; then
        echo "  Disk image:      $(du -h $DISK | cut -f1)"
    else
        echo "  Overlay file:    $(du -h $DISK | cut -f1)"
        echo "  Base image:      $(du -h $BASE | cut -f1) (shared)"
    fi

    # Cleanup
    rm -rf $BENCH/snap_test
}

# ---- Run 3 iterations of each ----

echo ""
echo "=========== SYNC ENGINE ==========="
for run in 1 2 3; do
    echo ""
    echo "--- Sync Run $run/3 ---"
    cp $ROOTFS $BENCH/sync_snap_disk.ext4
    run_snapshot_test "Sync" "Sync Engine (Run $run)" "$BENCH/sync_snap_disk.ext4" ""
done

echo ""
echo "=========== OVERLAY ENGINE ==========="
cp $ROOTFS $BENCH/base_snap.ext4
for run in 1 2 3; do
    echo ""
    echo "--- Overlay Run $run/3 ---"
    rm -f $BENCH/overlay_snap_disk.ext4
    run_snapshot_test "Overlay" "Overlay Engine (Run $run)" "$BENCH/overlay_snap_disk.ext4" "$BENCH/base_snap.ext4"
done

echo ""
echo "=========== RESET BENCHMARK ==========="
echo ""

# Sync reset: full disk copy
echo "--- Sync Reset (full disk copy) ---"
cp $ROOTFS $BENCH/reset_source.ext4
for run in 1 2 3; do
    S=$(date +%s%N)
    cp $BENCH/reset_source.ext4 $BENCH/reset_dest.ext4
    E=$(date +%s%N)
    MS=$(( (E - S) / 1000000 ))
    echo "  Run $run: ${MS}ms ($(du -h $BENCH/reset_source.ext4 | cut -f1) copied)"
    rm -f $BENCH/reset_dest.ext4
done

echo ""
echo "--- Overlay Reset (truncate + clear) ---"
# Create a dirty overlay first
cp $ROOTFS $BENCH/reset_overlay.ext4
DISK_SIZE=$(stat -c%s $BENCH/reset_overlay.ext4 2>/dev/null || stat -f%z $BENCH/reset_overlay.ext4)
for run in 1 2 3; do
    S=$(date +%s%N)
    truncate -s 0 $BENCH/reset_overlay.ext4
    truncate -s $DISK_SIZE $BENCH/reset_overlay.ext4
    E=$(date +%s%N)
    MS=$(( (E - S) / 1000000 ))
    echo "  Run $run: ${MS}ms (truncate + extend to $(du -h --apparent-size $BENCH/reset_overlay.ext4 2>/dev/null | cut -f1 || echo "${DISK_SIZE} bytes"))"
done

rm -f $BENCH/reset_source.ext4 $BENCH/reset_overlay.ext4 $BENCH/reset_dest.ext4

echo ""
echo "========================================="
echo "  Benchmark Complete"
echo "========================================="
