# Copyright 2026 Superserve AI. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for the in-process UFFD handler (MemBackendType=UffdInternal)."""

from pathlib import Path

import pytest


@pytest.fixture(scope="function", name="snapshot")
def snapshot_fxt(microvm_factory, guest_kernel_linux_5_10, rootfs):
    """Boot a microVM, snapshot it, return the snapshot artefacts."""
    basevm = microvm_factory.build(guest_kernel_linux_5_10, rootfs)
    basevm.spawn()
    basevm.basic_config(vcpu_count=2, mem_size_mib=256)
    basevm.add_net_iface()
    basevm.start()

    snap = basevm.snapshot_full()
    basevm.kill()
    yield snap


def _restore_uffd_internal(vm, snapshot, *, access_log_path=None, record_to=None):
    """Restore `snapshot` into `vm` using the UffdInternal backend.

    Mirrors the disk + interface plumbing in `Microvm.restore_from_snapshot` but bypasses
    its external-handler path so we can set our own `mem_backend` fields.
    """
    jailed_snapshot = snapshot.copy_to_chroot(Path(vm.chroot()))
    jailed_mem = Path("/") / jailed_snapshot.mem.name
    jailed_vmstate = Path("/") / jailed_snapshot.vmstate.name

    for disk in jailed_snapshot.disks.values():
        vm.create_jailed_resource(disk)
    vm.disks = jailed_snapshot.disks
    vm.ssh_key = jailed_snapshot.ssh_key

    for iface in jailed_snapshot.net_ifaces:
        vm.add_net_iface(iface, api=False)

    for key, value in jailed_snapshot.meta.items():
        setattr(vm, key, value)
    vm.kernel_file = Path(vm.kernel_file)

    mem_backend = {
        "backend_type": "UffdInternal",
        "backend_path": str(jailed_mem),
    }
    if access_log_path is not None:
        mem_backend["access_log_path"] = str(access_log_path)
    if record_to is not None:
        mem_backend["record_to"] = str(record_to)

    vm.api.snapshot_load.put(
        mem_backend=mem_backend,
        snapshot_path=str(jailed_vmstate),
        enable_diff_snapshots=False,
        resume_vm=True,
    )


def test_restore_with_uffd_internal(uvm_plain, snapshot):
    """Restoring with UffdInternal serves on-demand faults from inside firecracker."""
    vm = uvm_plain
    vm.memory_monitor = None
    vm.spawn()

    _restore_uffd_internal(vm, snapshot)

    # The guest needs to touch new memory to exercise the handler; `true` is enough to
    # force a fork/exec inside the guest, which faults in cold libc/shell pages.
    vm.ssh.check_output("true")
    out = vm.ssh.check_output("echo hello-from-uffd-internal").stdout.strip()
    assert out == "hello-from-uffd-internal"


def test_snapshot_after_faults_restores_cleanly(uvm_plain, microvm_factory, snapshot):
    """End-to-end check that snapshot save observes a consistent memory view.

    Restores into UffdInternal, generates a heavy on-demand fault workload so the
    handler is actively serving when pause arrives, takes a fresh snapshot, then
    restores the fresh snapshot into a second VM and verifies it boots. If the
    snapshot-save drain were broken (events queued at pause time not served
    before `dump()`), the second snapshot would have zero-filled pages where the
    in-flight faults should have landed and this second restore would either fail
    or hang on the first guest operation.
    """
    first = uvm_plain
    first.memory_monitor = None
    first.spawn()
    _restore_uffd_internal(first, snapshot)

    # Touch a large slice of guest memory through a path that has not been
    # exercised since boot; each fresh page becomes a UFFD fault. `dd` on the
    # kernel image is convenient: it sits in the rootfs, is large enough to
    # cause many faults, and the read is sequential so faults arrive in bursts.
    first.ssh.check_output("dd if=/usr/bin/python3 of=/dev/null bs=64K 2>/dev/null || true")
    first.ssh.check_output("cat /etc/os-release > /dev/null")

    # Snapshot the running VM (snapshot_full pauses internally; the drain runs
    # between pause and the memory dump).
    fresh = first.snapshot_full()
    first.kill()

    second = microvm_factory.build()
    second.memory_monitor = None
    second.spawn()
    _restore_uffd_internal(second, fresh)

    # If any cold-page fault was queued at pause time and not drained before
    # the memory dump, the new mem.snap is missing data for those pages. Boxing
    # this VM through a fork/exec exercises the same code paths as the first
    # run, so a hole in the captured memory would surface as a timeout or a
    # crash here, not as a silent corruption.
    second.ssh.check_output("true")
    out = second.ssh.check_output("echo drain-survived-roundtrip").stdout.strip()
    assert out == "drain-survived-roundtrip"


def test_record_and_replay_roundtrip(uvm_plain, microvm_factory, snapshot):
    """Recording mode captures fault offsets; the resulting log replays as prefetch."""
    record_name = "access.log"

    recorder = uvm_plain
    recorder.memory_monitor = None
    recorder.spawn()
    # `record_to` is a path relative to firecracker's cwd (the chroot), so the file is
    # created with jail ownership inside `recorder.chroot()`.
    _restore_uffd_internal(recorder, snapshot, record_to=f"/{record_name}")

    recorder.ssh.check_output("true")
    recorder.ssh.check_output("ls /usr/bin > /dev/null")
    recorder.kill()

    recorded_log = Path(recorder.chroot()) / record_name
    offsets = [int(line) for line in recorded_log.read_text().splitlines() if line.strip()]
    assert offsets, "expected at least one recorded fault offset"
    assert len(offsets) == len(set(offsets)), "recorder must dedup"

    replay = microvm_factory.build()
    replay.memory_monitor = None
    replay.spawn()
    jailed_replay_log = replay.create_jailed_resource(str(recorded_log))
    _restore_uffd_internal(replay, snapshot, access_log_path=jailed_replay_log)
    replay.ssh.check_output("true")
