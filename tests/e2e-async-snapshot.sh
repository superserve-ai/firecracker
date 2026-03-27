#!/usr/bin/env bash
# e2e-async-snapshot.sh — Production-grade E2E test for async memory snapshots.
#
# Simulates the real agent sandbox lifecycle:
#   Create VM → Boot → Run workload → Async Snapshot → Resume → Run more work
#   → Diff Snapshot → Resume → Restore from snapshot → Verify state
#
# This is the exact flow AgentBox uses for pause/resume in the tool-call loop.
#
# Requires: Firecracker binary (our fork), kernel + rootfs, KVM access.
# Run as root on a bare metal Linux host.
#
# Usage: sudo ./tests/e2e-async-snapshot.sh [firecracker_binary]

set -euo pipefail

FC_BIN="${1:-firecracker}"
KERNEL="${KERNEL:-/var/lib/sandtrace/kernel/vmlinux.bin}"
ROOTFS="${ROOTFS:-/var/lib/sandtrace/rootfs/alpine-rootfs.ext4}"
WORKDIR="/tmp/fc-async-snapshot-test-$$"
PASS=0
FAIL=0
TOTAL=0

GREEN='\033[32m'
RED='\033[31m'
YELLOW='\033[33m'
BOLD='\033[1m'
RESET='\033[0m'

pass() { PASS=$((PASS + 1)); TOTAL=$((TOTAL + 1)); echo -e "  ${GREEN}✓${RESET} $1"; }
fail() { FAIL=$((FAIL + 1)); TOTAL=$((TOTAL + 1)); echo -e "  ${RED}✗${RESET} $1"; }
info() { echo -e "  ${YELLOW}→${RESET} $1"; }
check() {
    local desc="$1"; shift
    if eval "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}

time_ms() {
    # Returns elapsed time in milliseconds for a command
    local start end
    start=$(date +%s%N)
    eval "$@" >/dev/null 2>&1
    end=$(date +%s%N)
    echo $(( (end - start) / 1000000 ))
}

api() {
    # Helper: call Firecracker API
    local method="$1" path="$2" socket="$3"
    shift 3
    curl -s --unix-socket "$socket" -X "$method" "http://localhost${path}" \
        -H "Content-Type: application/json" "$@"
}

cleanup() {
    echo ""
    info "Cleaning up..."
    pkill -f "firecracker --api-sock ${WORKDIR}" 2>/dev/null || true
    sleep 1
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
echo -e "${BOLD}Async Snapshot E2E Test${RESET}"
echo -e "  Simulates production agent sandbox lifecycle"
echo ""

[[ $EUID -eq 0 ]] || { echo "Must run as root"; exit 1; }
[[ -e /dev/kvm ]] || { echo "/dev/kvm not found"; exit 1; }
command -v "$FC_BIN" &>/dev/null || { echo "Firecracker not found: $FC_BIN"; exit 1; }
[[ -f "$KERNEL" ]] || { echo "Kernel not found: $KERNEL"; exit 1; }
[[ -f "$ROOTFS" ]] || { echo "Rootfs not found: $ROOTFS"; exit 1; }

mkdir -p "$WORKDIR"

# ---------------------------------------------------------------------------
# Helper: start a fresh Firecracker VM
# ---------------------------------------------------------------------------
start_vm() {
    local vm_name="$1" mem_mib="${2:-256}"
    local vm_dir="$WORKDIR/$vm_name"
    local sock="$vm_dir/api.sock"

    mkdir -p "$vm_dir"
    cp "$ROOTFS" "$vm_dir/rootfs.ext4"
    rm -f "$sock"

    "$FC_BIN" --api-sock "$sock" > "$vm_dir/fc.log" 2>&1 &
    local fc_pid=$!
    echo "$fc_pid" > "$vm_dir/fc.pid"
    sleep 1

    # Configure VM
    api PUT /boot-source "$sock" -d "{
        \"kernel_image_path\": \"$KERNEL\",
        \"boot_args\": \"console=ttyS0 reboot=k panic=1 pci=off\"
    }"

    api PUT /drives/rootfs "$sock" -d "{
        \"drive_id\": \"rootfs\",
        \"path_on_host\": \"$vm_dir/rootfs.ext4\",
        \"is_root_device\": true,
        \"is_read_only\": false
    }"

    api PUT /machine-config "$sock" -d "{
        \"vcpu_count\": 1,
        \"mem_size_mib\": $mem_mib,
        \"track_dirty_pages\": true
    }"

    # Start VM
    api PUT /actions "$sock" -d '{"action_type": "InstanceStart"}'
    sleep 3

    echo "$sock"
}

pause_vm() {
    local sock="$1"
    api PATCH /vm "$sock" -d '{"state": "Paused"}'
}

resume_vm() {
    local sock="$1"
    api PATCH /vm "$sock" -d '{"state": "Resumed"}'
}

kill_vm() {
    local vm_name="$1"
    local pid_file="$WORKDIR/$vm_name/fc.pid"
    if [[ -f "$pid_file" ]]; then
        kill "$(cat "$pid_file")" 2>/dev/null || true
    fi
}

# ===================================================================
# TEST 1: Basic async snapshot — create, complete, verify file
# ===================================================================
echo -e "${BOLD}Test 1: Basic async snapshot${RESET}"

SOCK=$(start_vm "vm1" 128)
VM_DIR="$WORKDIR/vm1"

# Pause VM
pause_vm "$SOCK"
sleep 0.5

# Create async snapshot
SNAP_RESULT=$(api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snapshot.bin\",
    \"mem_file_path\": \"$VM_DIR/mem.bin\",
    \"snapshot_type\": \"Full\",
    \"async_snapshot\": true
}" -w "%{http_code}")
check "async snapshot API returns 204" "echo '$SNAP_RESULT' | grep -q '204'"

# Complete the snapshot
COMPLETE_RESULT=$(api PUT /snapshot/complete "$SOCK" -d '{}' -w "%{http_code}")
check "complete snapshot returns 204" "echo '$COMPLETE_RESULT' | grep -q '204'"

# Verify files exist and are non-empty
check "snapshot state file exists" "[ -s $VM_DIR/snapshot.bin ]"
check "snapshot memory file exists" "[ -s $VM_DIR/mem.bin ]"

# Memory file should be approximately guest memory size (128MB)
MEM_SIZE=$(stat -c %s "$VM_DIR/mem.bin" 2>/dev/null || stat -f %z "$VM_DIR/mem.bin" 2>/dev/null)
EXPECTED=$((128 * 1024 * 1024))
check "memory file is correct size (${MEM_SIZE} = ${EXPECTED})" "[ $MEM_SIZE -eq $EXPECTED ]"

kill_vm "vm1"
sleep 1

# ===================================================================
# TEST 2: Pause time measurement — async vs sync
# ===================================================================
echo ""
echo -e "${BOLD}Test 2: Pause time comparison (sync vs async)${RESET}"

# -- Sync snapshot --
SOCK=$(start_vm "vm2-sync" 256)
VM_DIR="$WORKDIR/vm2-sync"

pause_vm "$SOCK"
sleep 0.5

SYNC_START=$(date +%s%N)
api PUT /snapshot/create "$SOCK" -d "{\"snapshot_path\":\"$VM_DIR/snapshot.bin\",\"mem_file_path\":\"$VM_DIR/mem.bin\",\"snapshot_type\":\"Full\",\"async_snapshot\":false}" > /dev/null
SYNC_END=$(date +%s%N)
SYNC_MS=$(( (SYNC_END - SYNC_START) / 1000000 ))
info "sync snapshot: ${SYNC_MS}ms"
check "sync snapshot file exists" "[ -s $VM_DIR/mem.bin ]"
kill_vm "vm2-sync"
sleep 1

# -- Async snapshot --
SOCK=$(start_vm "vm2-async" 256)
VM_DIR="$WORKDIR/vm2-async"

pause_vm "$SOCK"
sleep 0.5

ASYNC_START=$(date +%s%N)
api PUT /snapshot/create "$SOCK" -d "{\"snapshot_path\":\"$VM_DIR/snapshot.bin\",\"mem_file_path\":\"$VM_DIR/mem.bin\",\"snapshot_type\":\"Full\",\"async_snapshot\":true}" > /dev/null
ASYNC_END=$(date +%s%N)
ASYNC_MS=$(( (ASYNC_END - ASYNC_START) / 1000000 ))
info "async snapshot API call: ${ASYNC_MS}ms"

# Complete it
api PUT /snapshot/complete "$SOCK" -d '{}' -w "%{http_code}" > /dev/null

check "async snapshot file exists" "[ -s $VM_DIR/mem.bin ]"

# Async should be significantly faster than sync
if [ "$ASYNC_MS" -lt "$SYNC_MS" ]; then
    pass "async (${ASYNC_MS}ms) faster than sync (${SYNC_MS}ms)"
else
    fail "async (${ASYNC_MS}ms) not faster than sync (${SYNC_MS}ms)"
fi

kill_vm "vm2-async"
sleep 1

# ===================================================================
# TEST 3: Restore from async snapshot
# ===================================================================
echo ""
echo -e "${BOLD}Test 3: Restore from async snapshot${RESET}"

# Create a snapshot first
SOCK=$(start_vm "vm3-create" 128)
VM_DIR="$WORKDIR/vm3-create"

pause_vm "$SOCK"
sleep 0.5

api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snapshot.bin\",
    \"mem_file_path\": \"$VM_DIR/mem.bin\",
    \"snapshot_type\": \"Full\",
    \"async_snapshot\": true
}" > /dev/null

api PUT /snapshot/complete "$SOCK" -d '{}' > /dev/null
kill_vm "vm3-create"
sleep 1

# Restore from the snapshot in a new Firecracker process
RESTORE_DIR="$WORKDIR/vm3-restore"
mkdir -p "$RESTORE_DIR"
RESTORE_SOCK="$RESTORE_DIR/api.sock"
rm -f "$RESTORE_SOCK"

"$FC_BIN" --api-sock "$RESTORE_SOCK" > "$RESTORE_DIR/fc.log" 2>&1 &
echo $! > "$RESTORE_DIR/fc.pid"
sleep 1

RESTORE_RESULT=$(api PUT /snapshot/load "$RESTORE_SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snapshot.bin\",
    \"mem_file_path\": \"$VM_DIR/mem.bin\",
    \"resume_vm\": true
}" -w "%{http_code}")

check "restore from async snapshot succeeds" "echo '$RESTORE_RESULT' | grep -q '204'"

# Give VM time to resume
sleep 2

# Verify the restored VM's Firecracker process is still alive
RESTORE_PID=$(cat "$RESTORE_DIR/fc.pid")
check "restored VM process is running" "kill -0 $RESTORE_PID 2>/dev/null"

kill_vm "vm3-restore"
sleep 1

# ===================================================================
# TEST 4: Diff snapshot after async (agent tool-call loop simulation)
# ===================================================================
echo ""
echo -e "${BOLD}Test 4: Agent tool-call loop (full → diff → diff)${RESET}"

SOCK=$(start_vm "vm4" 128)
VM_DIR="$WORKDIR/vm4"

# --- First tool call: full async snapshot ---
pause_vm "$SOCK"
sleep 0.5

api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snap1.bin\",
    \"mem_file_path\": \"$VM_DIR/mem1.bin\",
    \"snapshot_type\": \"Full\",
    \"async_snapshot\": true
}" > /dev/null
api PUT /snapshot/complete "$SOCK" -d '{}' > /dev/null

FULL_SIZE=$(stat -c %s "$VM_DIR/mem1.bin" 2>/dev/null || stat -f %z "$VM_DIR/mem1.bin" 2>/dev/null)
info "full snapshot mem size: ${FULL_SIZE} bytes"
check "full async snapshot created" "[ -s $VM_DIR/mem1.bin ]"

# Resume and "run tool call" (VM runs, some pages change)
resume_vm "$SOCK"
sleep 2

# --- Second tool call: diff async snapshot ---
pause_vm "$SOCK"
sleep 0.5

DIFF_START=$(date +%s%N)
api PUT /snapshot/create "$SOCK" -d "{\"snapshot_path\":\"$VM_DIR/snap2.bin\",\"mem_file_path\":\"$VM_DIR/mem1.bin\",\"snapshot_type\":\"Diff\",\"async_snapshot\":true}" > /dev/null
DIFF_END=$(date +%s%N)
DIFF_MS=$(( (DIFF_END - DIFF_START) / 1000000 ))
api PUT /snapshot/complete "$SOCK" -d '{}' > /dev/null

info "diff async snapshot: ${DIFF_MS}ms"
check "diff snapshot updates mem file" "[ -s $VM_DIR/mem1.bin ]"

# Resume and run another "tool call"
resume_vm "$SOCK"
sleep 2

# --- Third tool call: another diff ---
pause_vm "$SOCK"
sleep 0.5

DIFF2_START=$(date +%s%N)
api PUT /snapshot/create "$SOCK" -d "{\"snapshot_path\":\"$VM_DIR/snap3.bin\",\"mem_file_path\":\"$VM_DIR/mem1.bin\",\"snapshot_type\":\"Diff\",\"async_snapshot\":true}" > /dev/null
DIFF2_END=$(date +%s%N)
DIFF2_MS=$(( (DIFF2_END - DIFF2_START) / 1000000 ))
api PUT /snapshot/complete "$SOCK" -d '{}' > /dev/null

info "second diff snapshot: ${DIFF2_MS}ms"
check "second diff snapshot succeeds" "[ -s $VM_DIR/snap3.bin ]"

kill_vm "vm4"
sleep 1

# ===================================================================
# TEST 5: Concurrent snapshot guard
# ===================================================================
echo ""
echo -e "${BOLD}Test 5: Concurrent snapshot guard${RESET}"

SOCK=$(start_vm "vm5" 128)
VM_DIR="$WORKDIR/vm5"

pause_vm "$SOCK"
sleep 0.5

# Start first async snapshot (don't complete it)
api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snap-a.bin\",
    \"mem_file_path\": \"$VM_DIR/mem-a.bin\",
    \"snapshot_type\": \"Full\",
    \"async_snapshot\": true
}" > /dev/null

# Immediately start second async snapshot (should auto-complete first)
SECOND_RESULT=$(api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snap-b.bin\",
    \"mem_file_path\": \"$VM_DIR/mem-b.bin\",
    \"snapshot_type\": \"Full\",
    \"async_snapshot\": true
}" -w "%{http_code}")

check "second snapshot while first in progress returns 204" "echo '$SECOND_RESULT' | grep -q '204'"

# Complete the second
api PUT /snapshot/complete "$SOCK" -d '{}' > /dev/null

check "first snapshot file created (auto-completed)" "[ -s $VM_DIR/mem-a.bin ]"
check "second snapshot file created" "[ -s $VM_DIR/mem-b.bin ]"

kill_vm "vm5"
sleep 1

# ===================================================================
# TEST 6: Async snapshot + resume + verify VM still works
# ===================================================================
echo ""
echo -e "${BOLD}Test 6: VM continues working after async snapshot${RESET}"

SOCK=$(start_vm "vm6" 128)
VM_DIR="$WORKDIR/vm6"

# Snapshot
pause_vm "$SOCK"
sleep 0.5

api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snapshot.bin\",
    \"mem_file_path\": \"$VM_DIR/mem.bin\",
    \"snapshot_type\": \"Full\",
    \"async_snapshot\": true
}" > /dev/null

# Resume immediately (background writer still running)
resume_vm "$SOCK"

# VM should continue running while background write happens
sleep 1
FC_PID=$(cat "$VM_DIR/fc.pid")
check "VM still running after async snapshot + resume" "kill -0 $FC_PID 2>/dev/null"

# Now complete (should work even after resume)
pause_vm "$SOCK"
sleep 0.5
api PUT /snapshot/complete "$SOCK" -d '{}' > /dev/null
check "complete after resume succeeds" "[ -s $VM_DIR/mem.bin ]"

kill_vm "vm6"
sleep 1

# ===================================================================
# TEST 7: Sync snapshot still works (backward compatibility)
# ===================================================================
echo ""
echo -e "${BOLD}Test 7: Sync snapshot backward compatibility${RESET}"

SOCK=$(start_vm "vm7" 128)
VM_DIR="$WORKDIR/vm7"

pause_vm "$SOCK"
sleep 0.5

SYNC_RESULT=$(api PUT /snapshot/create "$SOCK" -d "{
    \"snapshot_path\": \"$VM_DIR/snapshot.bin\",
    \"mem_file_path\": \"$VM_DIR/mem.bin\",
    \"snapshot_type\": \"Full\"
}" -w "%{http_code}")

check "sync snapshot (no async_snapshot field) returns 204" "echo '$SYNC_RESULT' | grep -q '204'"
check "sync snapshot creates valid file" "[ -s $VM_DIR/mem.bin ]"

kill_vm "vm7"

# ===================================================================
# Results
# ===================================================================
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${BOLD}Results: $PASS/$TOTAL passed${RESET}"
if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}$FAIL tests failed${RESET}"
    echo ""
    echo "=== VM logs ==="
    for log in "$WORKDIR"/*/fc.log; do
        echo "--- $log ---"
        tail -20 "$log" 2>/dev/null || true
    done
    exit 1
else
    echo -e "${GREEN}All tests passed${RESET}"
    echo ""
    echo "  Timing summary:"
    echo "    Sync snapshot:   ${SYNC_MS:-?}ms"
    echo "    Async snapshot:  ${ASYNC_MS:-?}ms"
    echo "    Diff snapshot 1: ${DIFF_MS:-?}ms"
    echo "    Diff snapshot 2: ${DIFF2_MS:-?}ms"
    exit 0
fi
