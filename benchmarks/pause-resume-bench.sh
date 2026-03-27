#!/bin/bash
# pause-resume-bench.sh — Benchmark full sandbox pause/resume cycle
#
# Compares vanilla Firecracker vs our fork (async snapshot + COW overlay)
# at different memory sizes.
#
# Usage: sudo ./benchmarks/pause-resume-bench.sh <vanilla_binary> <our_binary>
#
# Example:
#   sudo ./benchmarks/pause-resume-bench.sh \
#     /usr/local/bin/firecracker \
#     /opt/firecracker/build/cargo_target/release/firecracker

set -euo pipefail

VANILLA_BIN="${1:?Usage: $0 <vanilla_binary> <our_binary>}"
OUR_BIN="${2:?Usage: $0 <vanilla_binary> <our_binary>}"
KERNEL="${KERNEL:-/var/lib/sandtrace/kernel/vmlinux.bin}"
ROOTFS="${ROOTFS:-/var/lib/sandtrace/rootfs/alpine-rootfs.ext4}"
WORKDIR="/tmp/pause-resume-bench-$$"

BOLD='\033[1m'
GREEN='\033[32m'
YELLOW='\033[33m'
CYAN='\033[36m'
RESET='\033[0m'

[[ $EUID -eq 0 ]] || { echo "Must run as root"; exit 1; }
[[ -f "$VANILLA_BIN" ]] || { echo "Vanilla binary not found: $VANILLA_BIN"; exit 1; }
[[ -f "$OUR_BIN" ]] || { echo "Our binary not found: $OUR_BIN"; exit 1; }
[[ -f "$KERNEL" ]] || { echo "Kernel not found: $KERNEL"; exit 1; }
[[ -f "$ROOTFS" ]] || { echo "Rootfs not found: $ROOTFS"; exit 1; }

mkdir -p "$WORKDIR"
trap 'pkill -9 -f "firecracker --api-sock $WORKDIR" 2>/dev/null; rm -rf "$WORKDIR"' EXIT

api() {
    curl -s --unix-socket "$1" -X "$2" "http://localhost$3" \
        -H "Content-Type: application/json" "${@:4}"
}

# ---------------------------------------------------------------
# Benchmark one full pause/resume cycle
# Returns: pause_ms resume_ms snapshot_size_kb
# ---------------------------------------------------------------
bench_cycle() {
    local fc_bin="$1" mem_mib="$2" mode="$3"  # mode: vanilla | ours
    local dir="$WORKDIR/${mode}-${mem_mib}"
    local sock="$dir/api.sock"

    rm -rf "$dir"; mkdir -p "$dir"
    cp "$ROOTFS" "$dir/rootfs.ext4"

    # Recreate tap
    ip link del tap0 2>/dev/null || true
    ip tuntap add dev tap0 mode tap
    ip link set tap0 master br0 2>/dev/null || true
    ip link set tap0 up

    # Boot VM
    rm -f "$sock"
    "$fc_bin" --api-sock "$sock" > "$dir/fc.log" 2>&1 &
    local pid=$!
    sleep 1

    api "$sock" PUT /boot-source -d "{\"kernel_image_path\":\"$KERNEL\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off\"}" > /dev/null
    api "$sock" PUT /drives/rootfs -d "{\"drive_id\":\"rootfs\",\"path_on_host\":\"$dir/rootfs.ext4\",\"is_root_device\":true,\"is_read_only\":false}" > /dev/null
    api "$sock" PUT /machine-config -d "{\"vcpu_count\":1,\"mem_size_mib\":$mem_mib,\"track_dirty_pages\":true}" > /dev/null
    api "$sock" PUT /actions -d "{\"action_type\":\"InstanceStart\"}" > /dev/null
    sleep 5  # let VM boot

    # === PAUSE (snapshot + kill) ===
    local pause_start pause_end pause_ms

    # Freeze vCPUs
    api "$sock" PATCH /vm -d "{\"state\":\"Paused\"}" > /dev/null
    sleep 0.3

    pause_start=$(date +%s%N)

    if [ "$mode" = "ours" ]; then
        # Async memory snapshot
        api "$sock" PUT /snapshot/create -d "{\"snapshot_path\":\"$dir/snap.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"snapshot_type\":\"Full\",\"async_snapshot\":true}" > /dev/null
        api "$sock" PUT /snapshot/complete -d "{}" > /dev/null
    else
        # Vanilla sync snapshot
        api "$sock" PUT /snapshot/create -d "{\"snapshot_path\":\"$dir/snap.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"snapshot_type\":\"Full\"}" > /dev/null
    fi

    pause_end=$(date +%s%N)
    pause_ms=$(( (pause_end - pause_start) / 1000000 ))

    # Kill the process
    kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null || true
    sleep 1

    # Snapshot size
    local snap_kb=0
    if [ -f "$dir/mem.bin" ]; then
        snap_kb=$(( $(stat -c %s "$dir/mem.bin") / 1024 ))
    fi

    # === RESUME (restore) ===
    local resume_start resume_end resume_ms

    rm -f "$sock"
    "$fc_bin" --api-sock "$sock" > "$dir/fc-restore.log" 2>&1 &
    local restore_pid=$!
    sleep 1

    resume_start=$(date +%s%N)
    api "$sock" PUT /snapshot/load -d "{\"snapshot_path\":\"$dir/snap.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"resume_vm\":true}" > /dev/null
    resume_end=$(date +%s%N)
    resume_ms=$(( (resume_end - resume_start) / 1000000 ))

    # Verify restore worked
    if kill -0 $restore_pid 2>/dev/null; then
        local status="ok"
    else
        local status="FAIL"
    fi

    kill -9 $restore_pid 2>/dev/null; wait $restore_pid 2>/dev/null || true

    echo "${pause_ms} ${resume_ms} ${snap_kb} ${status}"
}

# Also benchmark diff snapshot (second snapshot after resume)
bench_diff() {
    local fc_bin="$1" mem_mib="$2" mode="$3"
    local dir="$WORKDIR/${mode}-diff-${mem_mib}"
    local sock="$dir/api.sock"

    rm -rf "$dir"; mkdir -p "$dir"
    cp "$ROOTFS" "$dir/rootfs.ext4"

    ip link del tap0 2>/dev/null || true
    ip tuntap add dev tap0 mode tap
    ip link set tap0 master br0 2>/dev/null || true
    ip link set tap0 up

    rm -f "$sock"
    "$fc_bin" --api-sock "$sock" > "$dir/fc.log" 2>&1 &
    local pid=$!
    sleep 1

    api "$sock" PUT /boot-source -d "{\"kernel_image_path\":\"$KERNEL\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off\"}" > /dev/null
    api "$sock" PUT /drives/rootfs -d "{\"drive_id\":\"rootfs\",\"path_on_host\":\"$dir/rootfs.ext4\",\"is_root_device\":true,\"is_read_only\":false}" > /dev/null
    api "$sock" PUT /machine-config -d "{\"vcpu_count\":1,\"mem_size_mib\":$mem_mib,\"track_dirty_pages\":true}" > /dev/null
    api "$sock" PUT /actions -d "{\"action_type\":\"InstanceStart\"}" > /dev/null
    sleep 5

    # First full snapshot
    api "$sock" PATCH /vm -d "{\"state\":\"Paused\"}" > /dev/null
    sleep 0.3

    if [ "$mode" = "ours" ]; then
        api "$sock" PUT /snapshot/create -d "{\"snapshot_path\":\"$dir/snap1.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"snapshot_type\":\"Full\",\"async_snapshot\":true}" > /dev/null
        api "$sock" PUT /snapshot/complete -d "{}" > /dev/null
    else
        api "$sock" PUT /snapshot/create -d "{\"snapshot_path\":\"$dir/snap1.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"snapshot_type\":\"Full\"}" > /dev/null
    fi

    # Resume, let VM run, then diff snapshot
    api "$sock" PATCH /vm -d "{\"state\":\"Resumed\"}" > /dev/null
    sleep 3
    api "$sock" PATCH /vm -d "{\"state\":\"Paused\"}" > /dev/null
    sleep 0.3

    local diff_start diff_end diff_ms
    diff_start=$(date +%s%N)

    if [ "$mode" = "ours" ]; then
        api "$sock" PUT /snapshot/create -d "{\"snapshot_path\":\"$dir/snap2.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"snapshot_type\":\"Diff\",\"async_snapshot\":true}" > /dev/null
        api "$sock" PUT /snapshot/complete -d "{}" > /dev/null
    else
        api "$sock" PUT /snapshot/create -d "{\"snapshot_path\":\"$dir/snap2.bin\",\"mem_file_path\":\"$dir/mem.bin\",\"snapshot_type\":\"Diff\"}" > /dev/null
    fi

    diff_end=$(date +%s%N)
    diff_ms=$(( (diff_end - diff_start) / 1000000 ))

    kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null || true
    echo "$diff_ms"
}

# ---------------------------------------------------------------
# Run benchmarks
# ---------------------------------------------------------------
echo ""
echo -e "${BOLD}Sandbox Pause/Resume Benchmark${RESET}"
echo -e "  Vanilla: $VANILLA_BIN"
echo -e "  Ours:    $OUR_BIN"
echo ""

echo -e "${BOLD}Full Snapshot (first pause)${RESET}"
echo ""
printf "  ${CYAN}%-8s${RESET} | ${YELLOW}%-20s${RESET} | ${GREEN}%-20s${RESET} | %-10s\n" \
    "Memory" "Vanilla" "Ours" "Speedup"
printf "  %-8s-+-%-20s-+-%-20s-+-%-10s\n" "--------" "--------------------" "--------------------" "----------"

for mem in 128 256 512 1024; do
    # Vanilla
    read v_pause v_resume v_snap v_status <<< $(bench_cycle "$VANILLA_BIN" "$mem" "vanilla")

    # Ours
    read o_pause o_resume o_snap o_status <<< $(bench_cycle "$OUR_BIN" "$mem" "ours")

    # Speedup
    if [ "$o_pause" -gt 0 ]; then
        speedup=$(echo "scale=1; $v_pause / $o_pause" | bc)
    else
        speedup="inf"
    fi

    printf "  ${CYAN}%-8s${RESET} | pause: %4dms  res: %4dms | pause: %4dms  res: %4dms | ${GREEN}%sx${RESET}\n" \
        "${mem}MB" "$v_pause" "$v_resume" "$o_pause" "$o_resume" "$speedup"
done

echo ""
echo -e "${BOLD}Diff Snapshot (subsequent tool calls)${RESET}"
echo ""
printf "  ${CYAN}%-8s${RESET} | ${YELLOW}%-12s${RESET} | ${GREEN}%-12s${RESET} | %-10s\n" \
    "Memory" "Vanilla" "Ours" "Speedup"
printf "  %-8s-+-%-12s-+-%-12s-+-%-10s\n" "--------" "------------" "------------" "----------"

for mem in 128 256 512 1024; do
    v_diff=$(bench_diff "$VANILLA_BIN" "$mem" "vanilla")
    o_diff=$(bench_diff "$OUR_BIN" "$mem" "ours")

    if [ "$o_diff" -gt 0 ]; then
        speedup=$(echo "scale=1; $v_diff / $o_diff" | bc)
    else
        speedup="inf"
    fi

    printf "  ${CYAN}%-8s${RESET} | %6dms     | %6dms     | ${GREEN}%sx${RESET}\n" \
        "${mem}MB" "$v_diff" "$o_diff" "$speedup"
done

echo ""
echo -e "${BOLD}E2B Reference (documented)${RESET}"
echo "  Pause: ~4000ms per GB of RAM"
echo "  Resume: ~1000ms"
echo ""
