// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::cast_possible_truncation, clippy::tests_outside_test_module)]

use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use vmm::builder::build_and_boot_microvm;
use vmm::devices::virtio::block::CacheType;
use vmm::devices::virtio::block::virtio::io::overlay_io::OverlayIoError;
use vmm::persist::{CreateSnapshotError, MicrovmState, MicrovmStateError, VmInfo, snapshot_state_sanity_check};
use vmm::resources::VmResources;
use vmm::rpc_interface::{
    LoadSnapshotError, PrebootApiController, RuntimeApiController, VmmAction, VmmActionError,
};
use vmm::seccomp::get_empty_filters;
use vmm::snapshot::Snapshot;
use vmm::test_utils::mock_resources::{MockVmResources, NOISY_KERNEL_IMAGE};
use vmm::test_utils::{create_vmm, default_vmm, default_vmm_no_boot};
use vmm::vmm_config::balloon::BalloonDeviceConfig;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::instance_info::{InstanceInfo, VmState};
use vmm::vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
use vmm::vmm_config::net::NetworkInterfaceConfig;
use vmm::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotParams, MemBackendConfig, MemBackendType, SnapshotType,
};
use vmm::vmm_config::vsock::VsockDeviceConfig;
use vmm::{DumpCpuConfigError, EventManager, FcExitCode, Vmm};
use vmm_sys_util::tempdir::TempDir;
use vmm_sys_util::tempfile::TempFile;

#[allow(unused_mut, unused_variables)]
fn check_booted_microvm(vmm: Arc<Mutex<Vmm>>, mut evmgr: EventManager) {
    // On x86_64, the vmm should exit once its workload completes and signals the exit event.
    // On aarch64, the test kernel doesn't exit, so the vmm is force-stopped.
    #[cfg(target_arch = "x86_64")]
    evmgr.run_with_timeout(500).unwrap();
    #[cfg(target_arch = "aarch64")]
    vmm.lock().unwrap().stop(FcExitCode::Ok);

    assert_eq!(
        vmm.lock().unwrap().shutdown_exit_code(),
        Some(FcExitCode::Ok)
    );
}

#[test]
fn test_build_and_boot_microvm() {
    // Error case: no boot source configured.
    {
        let resources: VmResources = MockVmResources::new().into();
        let mut event_manager = EventManager::new().unwrap();
        let empty_seccomp_filters = get_empty_filters();

        let vmm_ret = build_and_boot_microvm(
            &InstanceInfo::default(),
            &resources,
            &mut event_manager,
            &empty_seccomp_filters,
        );
        assert_eq!(format!("{:?}", vmm_ret.err()), "Some(MissingKernelConfig)");
    }

    for pci_enabled in [false, true] {
        for memory_hotplug in [false, true] {
            let (vmm, evmgr) = create_vmm(None, false, true, pci_enabled, memory_hotplug);
            check_booted_microvm(vmm, evmgr);
        }
    }
}

#[allow(unused_mut, unused_variables)]
fn check_build_microvm(vmm: Arc<Mutex<Vmm>>, mut evmgr: EventManager) {
    // The built microVM should be in the `VmState::Paused` state here.
    assert_eq!(vmm.lock().unwrap().instance_info().state, VmState::Paused);

    // The microVM should be able to resume and exit successfully.
    // On x86_64, the vmm should exit once its workload completes and signals the exit event.
    // On aarch64, the test kernel doesn't exit, so the vmm is force-stopped.
    vmm.lock().unwrap().resume_vm().unwrap();
    #[cfg(target_arch = "x86_64")]
    evmgr.run_with_timeout(500).unwrap();
    #[cfg(target_arch = "aarch64")]
    vmm.lock().unwrap().stop(FcExitCode::Ok);
    assert_eq!(
        vmm.lock().unwrap().shutdown_exit_code(),
        Some(FcExitCode::Ok)
    );
}

#[test]
fn test_build_microvm() {
    for pci_enabled in [false, true] {
        for memory_hotplug in [false, true] {
            let (vmm, evmgr) = create_vmm(None, false, false, pci_enabled, memory_hotplug);
            check_build_microvm(vmm, evmgr);
        }
    }
}

fn pause_resume_microvm(vmm: Arc<Mutex<Vmm>>) {
    let mut api_controller = RuntimeApiController::new(vmm.clone());

    // There's a race between this thread and the vcpu thread, but this thread
    // should be able to pause vcpu thread before it finishes running its test-binary.
    api_controller.handle_request(VmmAction::Pause).unwrap();
    // Pausing again the microVM should not fail (microVM remains in the
    // `Paused` state).
    api_controller.handle_request(VmmAction::Pause).unwrap();
    api_controller.handle_request(VmmAction::Resume).unwrap();

    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

#[test]
fn test_pause_resume_microvm() {
    for pci_enabled in [false, true] {
        for memory_hotplug in [false, true] {
            // Tests that pausing and resuming a microVM work as expected.
            let (vmm, _) = create_vmm(None, false, true, pci_enabled, memory_hotplug);

            pause_resume_microvm(vmm);
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn test_dirty_bitmap_success() {
    let vmms = [
        vmm::test_utils::dirty_tracking_vmm(Some(NOISY_KERNEL_IMAGE)),
        default_vmm(Some(NOISY_KERNEL_IMAGE)),
    ];

    for (vmm, _) in vmms {
        // Let it churn for a while and dirty some pages...
        thread::sleep(Duration::from_millis(100));
        let bitmap = vmm.lock().unwrap().vm.get_dirty_bitmap().unwrap();
        let num_dirty_pages: u32 = bitmap
            .values()
            .map(|bitmap_per_region| {
                // Gently coerce to u32
                let num_dirty_pages_per_region: u32 =
                    bitmap_per_region.iter().map(|n| n.count_ones()).sum();
                num_dirty_pages_per_region
            })
            .sum();
        assert!(num_dirty_pages > 0);
        vmm.lock().unwrap().stop(FcExitCode::Ok);
    }
}

#[test]
fn test_disallow_snapshots_without_pausing() {
    let (vmm, _) = default_vmm(Some(NOISY_KERNEL_IMAGE));
    let vm_info = VmInfo {
        mem_size_mib: 1u64,
        ..Default::default()
    };

    // Verify saving state while running is not allowed.
    assert!(matches!(
        vmm.lock().unwrap().save_state(&vm_info),
        Err(MicrovmStateError::NotAllowed(_))
    ));

    // Pause microVM.
    vmm.lock().unwrap().pause_vm().unwrap();
    // It is now allowed.
    vmm.lock().unwrap().save_state(&vm_info).unwrap();
    // Stop.
    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

#[test]
fn test_disallow_dump_cpu_config_without_pausing() {
    let (vmm, _) = default_vmm_no_boot(Some(NOISY_KERNEL_IMAGE));

    // This call should succeed since the microVM is in the paused state before boot.
    vmm.lock().unwrap().dump_cpu_config().unwrap();

    // Boot the microVM.
    vmm.lock().unwrap().resume_vm().unwrap();

    // Verify this call is not allowed while running.
    assert!(matches!(
        vmm.lock().unwrap().dump_cpu_config(),
        Err(DumpCpuConfigError::NotAllowed(_))
    ));

    // Stop the microVM.
    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

fn verify_create_snapshot(
    is_diff: bool,
    pci_enabled: bool,
    memory_hotplug: bool,
) -> (TempFile, TempFile) {
    let snapshot_file = TempFile::new().unwrap();
    let memory_file = TempFile::new().unwrap();

    let (vmm, _) = create_vmm(
        Some(NOISY_KERNEL_IMAGE),
        is_diff,
        true,
        pci_enabled,
        memory_hotplug,
    );

    let vm_info = VmInfo::from(&*vmm.lock().unwrap());
    let mut controller = RuntimeApiController::new(vmm.clone());

    // Be sure that the microVM is running.
    thread::sleep(Duration::from_millis(200));

    // Pause microVM.
    controller.handle_request(VmmAction::Pause).unwrap();

    // Create snapshot.
    let snapshot_type = match is_diff {
        true => SnapshotType::Diff,
        false => SnapshotType::Full,
    };
    let snapshot_params = CreateSnapshotParams {
        snapshot_type,
        snapshot_path: snapshot_file.as_path().to_path_buf(),
        mem_file_path: memory_file.as_path().to_path_buf(),
        block_delta_dir: None,
        flatten: false,
    };

    controller
        .handle_request(VmmAction::CreateSnapshot(snapshot_params))
        .unwrap();

    vmm.lock().unwrap().stop(FcExitCode::Ok);

    // Check that we can deserialize the microVM state from `snapshot_file`.
    let restored_microvm_state: MicrovmState =
        Snapshot::load(&mut snapshot_file.as_file()).unwrap().data;

    assert_eq!(restored_microvm_state.vm_info, vm_info);

    // Verify deserialized data.
    // The default vmm has no devices and one vCPU.
    assert_eq!(
        restored_microvm_state
            .device_states
            .mmio_state
            .block_devices
            .len(),
        0
    );
    assert_eq!(
        restored_microvm_state
            .device_states
            .mmio_state
            .net_devices
            .len(),
        0
    );
    assert!(
        restored_microvm_state
            .device_states
            .mmio_state
            .vsock_device
            .is_none()
    );
    assert_eq!(restored_microvm_state.vcpu_states.len(), 1);

    (snapshot_file, memory_file)
}

fn verify_load_snapshot(snapshot_file: TempFile, memory_file: TempFile) {
    let mut event_manager = EventManager::new().unwrap();
    let empty_seccomp_filters = get_empty_filters();
    let mut vm_resources = VmResources::default();

    let mut preboot_api_controller = PrebootApiController::new(
        &empty_seccomp_filters,
        InstanceInfo::default(),
        &mut vm_resources,
        &mut event_manager,
    );

    preboot_api_controller
        .handle_preboot_request(VmmAction::LoadSnapshot(LoadSnapshotParams {
            snapshot_path: snapshot_file.as_path().to_path_buf(),
            mem_backend: MemBackendConfig {
                backend_path: memory_file.as_path().to_path_buf(),
                backend_type: MemBackendType::File,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: true,
            network_overrides: vec![],
            block_delta_dir: None,
        }))
        .unwrap();

    let vmm = preboot_api_controller.built_vmm.take().unwrap();

    assert_eq!(vmm.lock().unwrap().instance_info.state, VmState::Running);
    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

// Build a VMM with a single overlay-mode block device backed by `base` /
// `overlay`. Both files are pre-sized to disk_size_bytes and the engine
// validates they match at attach time. Drive is non-root so the test kernel
// (no rootfs configured) still boots.
fn create_vmm_with_overlay_drive(
    base_path: &str,
    overlay_path: &str,
    disk_size_bytes: u64,
) -> Arc<Mutex<Vmm>> {
    use vmm::test_utils::mock_resources::{MockBootSourceConfig, MockVmResources};
    use vmm::vmm_config::drive::FileEngineType;

    // Ensure both files exist at the expected size. NO truncate(true) — that
    // would wipe caller-provided content (the bake test pre-populates base
    // with 0xAA before calling here). set_len is a no-op if size already matches.
    std::fs::OpenOptions::new()
        .write(true).create(true)
        .open(base_path).unwrap()
        .set_len(disk_size_bytes).unwrap();
    std::fs::OpenOptions::new()
        .write(true).create(true)
        .open(overlay_path).unwrap()
        .set_len(disk_size_bytes).unwrap();

    let mut event_manager = EventManager::new().unwrap();
    let empty_seccomp_filters = get_empty_filters();

    // Default kernel from MockBootSourceConfig::new() — keep cross-arch (with_kernel is x86_64-only).
    let boot_source_cfg: BootSourceConfig = MockBootSourceConfig::new()
        .with_default_boot_args()
        .into();

    let mut resources: VmResources = MockVmResources::new()
        .with_boot_source(boot_source_cfg)
        .into();

    resources
        .set_block_device(BlockDeviceConfig {
            drive_id: "data".to_string(),
            partuuid: None,
            is_root_device: false,
            cache_type: CacheType::Unsafe,
            is_read_only: Some(false),
            path_on_host: Some(overlay_path.to_string()),
            rate_limiter: None,
            file_engine_type: Some(FileEngineType::Overlay),
            socket: None,
            base_path: Some(base_path.to_string()),
        })
        .unwrap();

    let vmm = build_and_boot_microvm(
        &InstanceInfo::default(),
        &resources,
        &mut event_manager,
        &empty_seccomp_filters,
    )
    .unwrap();
    vmm.lock().unwrap().resume_vm().unwrap();
    vmm
}

// Wiring test (empty bitmap): CreateSnapshot with flatten=true against a VMM
// with an overlay device produces empty-form artifacts and restores cleanly.
// Bake-with-content is covered by `test_create_snapshot_flatten_bakes_dirty_content_into_base`.
#[test]
fn test_create_snapshot_flatten_wires_through_overlay_drive() {
    let tmp = TempDir::new().unwrap();
    let base_path = format!("{}/base.ext4", tmp.as_path().to_str().unwrap());
    let overlay_path = format!("{}/overlay.ext4", tmp.as_path().to_str().unwrap());
    let delta_dir = format!("{}/deltas", tmp.as_path().to_str().unwrap());
    std::fs::create_dir(&delta_dir).unwrap();

    let disk_size = 1024 * 1024_u64; // 1 MiB, sized for the bitmap
    let vmm = create_vmm_with_overlay_drive(&base_path, &overlay_path, disk_size);

    let mut controller = RuntimeApiController::new(vmm.clone());
    thread::sleep(Duration::from_millis(200));
    controller.handle_request(VmmAction::Pause).unwrap();

    let snapshot_file = TempFile::new().unwrap();
    let memory_file = TempFile::new().unwrap();
    let params = CreateSnapshotParams {
        snapshot_type: SnapshotType::Full,
        snapshot_path: snapshot_file.as_path().to_path_buf(),
        mem_file_path: memory_file.as_path().to_path_buf(),
        block_delta_dir: Some(std::path::PathBuf::from(&delta_dir)),
        flatten: true,
    };
    controller
        .handle_request(VmmAction::CreateSnapshot(params))
        .expect("flatten snapshot must succeed");

    assert_eq!(
        delta_dirty_count(format!("{delta_dir}/data.delta")),
        0,
        "delta dirty_count must be 0 after flatten"
    );

    // Side-car must exist (non-zero — torn-save sentinel is empty file).
    let sidecar_path = format!("{}.overlay", snapshot_file.as_path().to_str().unwrap());
    let sidecar_size = std::fs::metadata(&sidecar_path)
        .expect("sidecar written")
        .len();
    assert!(sidecar_size > 0, "sidecar must be non-empty (would be 0 only on torn save)");

    // Stop the source VMM before restoring — frees the overlay file handle so
    // the restore path can reopen it cleanly.
    vmm.lock().unwrap().stop(FcExitCode::Ok);

    // Restore phase: rebuild a VMM from the flattened snapshot. Validates that
    // born-zero sidecar bitmaps + empty-form delta loaded by the same fork
    // produce a functional VMM. Block content verification is NOT done here
    // (would require dirty bits set on the source VMM — see apply_overlay_to_base
    // unit tests for that coverage).
    let mut restore_event_manager = EventManager::new().unwrap();
    let restore_seccomp = get_empty_filters();
    let mut restore_resources = VmResources::default();
    let mut preboot = PrebootApiController::new(
        &restore_seccomp,
        InstanceInfo::default(),
        &mut restore_resources,
        &mut restore_event_manager,
    );
    preboot
        .handle_preboot_request(VmmAction::LoadSnapshot(LoadSnapshotParams {
            snapshot_path: snapshot_file.as_path().to_path_buf(),
            mem_backend: MemBackendConfig {
                backend_path: memory_file.as_path().to_path_buf(),
                backend_type: MemBackendType::File,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: true,
            network_overrides: vec![],
            block_delta_dir: Some(std::path::PathBuf::from(&delta_dir)),
        }))
        .expect("restore from flattened snapshot must succeed");

    let restored_vmm = preboot.built_vmm.take().unwrap();
    assert_eq!(restored_vmm.lock().unwrap().instance_info.state, VmState::Running);
    restored_vmm.lock().unwrap().stop(FcExitCode::Ok);
}

// Full-cycle bake: inject known dirty bytes → flatten → assert base.ext4
// has those bytes at the right offset → restore. Catches regressions the
// wiring-only test would miss.
#[cfg(feature = "test-fixtures")]
#[test]
fn test_create_snapshot_flatten_bakes_dirty_content_into_base() {
    let tmp = TempDir::new().unwrap();
    let base_path = format!("{}/base.ext4", tmp.as_path().to_str().unwrap());
    let overlay_path = format!("{}/overlay.ext4", tmp.as_path().to_str().unwrap());
    let delta_dir = format!("{}/deltas", tmp.as_path().to_str().unwrap());
    std::fs::create_dir(&delta_dir).unwrap();

    let disk_size = 1024 * 1024_u64;
    let block_size = 4096_usize;
    let target_block_idx = 1_u64;
    let dirty_content = vec![0xCD_u8; block_size];

    // Initialize base with 0xAA — distinguishable from our dirty bytes.
    std::fs::write(&base_path, vec![0xAA_u8; disk_size as usize]).unwrap();
    std::fs::OpenOptions::new()
        .write(true).create(true).truncate(true)
        .open(&overlay_path).unwrap()
        .set_len(disk_size).unwrap();

    let vmm = create_vmm_with_overlay_drive(&base_path, &overlay_path, disk_size);

    // 200ms cushion — without it Pause can race boot on slow runners.
    thread::sleep(Duration::from_millis(200));

    // Pause BEFORE injecting dirty bits — mutating engine state while the
    // guest can read it would create a transient filesystem inconsistency
    // visible to the guest. After pause, no concurrent device I/O.
    let mut controller = RuntimeApiController::new(vmm.clone());
    controller.handle_request(VmmAction::Pause).unwrap();
    vmm.lock()
        .unwrap()
        .force_dirty_block_for_test("data", target_block_idx, &dirty_content)
        .expect("force_dirty_block_for_test");

    let snapshot_file = TempFile::new().unwrap();
    let memory_file = TempFile::new().unwrap();
    controller
        .handle_request(VmmAction::CreateSnapshot(CreateSnapshotParams {
            snapshot_type: SnapshotType::Full,
            snapshot_path: snapshot_file.as_path().to_path_buf(),
            mem_file_path: memory_file.as_path().to_path_buf(),
            block_delta_dir: Some(std::path::PathBuf::from(&delta_dir)),
            flatten: true,
        }))
        .expect("flatten snapshot");

    // Bake verification: read base.ext4 from disk, assert target block now
    // holds the dirty bytes (was 0xAA before flatten).
    let baked = std::fs::read(&base_path).expect("read base");
    let offset = (target_block_idx as usize) * block_size;
    assert!(
        baked[offset..offset + block_size].iter().all(|b| *b == 0xCD),
        "flatten did not bake dirty bytes into base.ext4 at block {target_block_idx}: \
         got first byte 0x{:02X}, want 0xCD",
        baked[offset]
    );
    // Untouched blocks should retain 0xAA.
    assert!(
        baked[..block_size].iter().all(|b| *b == 0xAA),
        "flatten corrupted clean block 0"
    );

    assert_eq!(
        delta_dirty_count(format!("{delta_dir}/data.delta")),
        0,
        "delta dirty_count must be 0 after flatten"
    );

    vmm.lock().unwrap().stop(FcExitCode::Ok);

    // Restore phase: load the flat snapshot and confirm the VMM boots.
    let mut restore_event_manager = EventManager::new().unwrap();
    let restore_seccomp = get_empty_filters();
    let mut restore_resources = VmResources::default();
    let mut preboot = PrebootApiController::new(
        &restore_seccomp,
        InstanceInfo::default(),
        &mut restore_resources,
        &mut restore_event_manager,
    );
    preboot
        .handle_preboot_request(VmmAction::LoadSnapshot(LoadSnapshotParams {
            snapshot_path: snapshot_file.as_path().to_path_buf(),
            mem_backend: MemBackendConfig {
                backend_path: memory_file.as_path().to_path_buf(),
                backend_type: MemBackendType::File,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: true,
            network_overrides: vec![],
            block_delta_dir: Some(std::path::PathBuf::from(&delta_dir)),
        }))
        .expect("restore from flat snapshot");
    let restored_vmm = preboot.built_vmm.take().unwrap();
    assert_eq!(restored_vmm.lock().unwrap().instance_info.state, VmState::Running);
    restored_vmm.lock().unwrap().stop(FcExitCode::Ok);
}

// dirty_count is the 4th u64 in the delta header — see
// src/vmm/src/devices/virtio/block/virtio/io/delta.rs for the full layout.
const DELTA_HEADER_DIRTY_COUNT_OFFSET: usize = 24;

fn delta_dirty_count(path: impl AsRef<std::path::Path>) -> u64 {
    let bytes = std::fs::read(&path).expect("delta file");
    assert!(bytes.len() >= 32, "delta header truncated");
    let end = DELTA_HEADER_DIRTY_COUNT_OFFSET + 8;
    u64::from_le_bytes(bytes[DELTA_HEADER_DIRTY_COUNT_OFFSET..end].try_into().unwrap())
}

// Helper: drive a CreateSnapshot with flatten=true through the controller
// and extract the inner OverlayIoError variant. Panics if the call doesn't
// fail with a CreateSnapshotError::FlattenOverlays.
fn flatten_snapshot_expect_overlay_err(
    controller: &mut RuntimeApiController,
    snapshot_path: std::path::PathBuf,
    mem_path: std::path::PathBuf,
    delta_dir: std::path::PathBuf,
) -> OverlayIoError {
    let err = controller
        .handle_request(VmmAction::CreateSnapshot(CreateSnapshotParams {
            snapshot_type: SnapshotType::Full,
            snapshot_path,
            mem_file_path: mem_path,
            block_delta_dir: Some(delta_dir),
            flatten: true,
        }))
        .expect_err("expected overlay error from flatten");
    match err {
        VmmActionError::CreateSnapshot(CreateSnapshotError::FlattenOverlays(e)) => e,
        other => panic!("unexpected error variant: {other:?}"),
    }
}

// Pre-flight error branch: base.ext4 is missing on disk at flatten time.
// The engine still has its open FD, so the VMM keeps running, but pre-flight
// opens base by path and must fail with FlattenBaseOpen before any mutation.
#[test]
fn test_flatten_pre_flight_rejects_missing_base() {
    let tmp = TempDir::new().unwrap();
    let base_path = format!("{}/base.ext4", tmp.as_path().to_str().unwrap());
    let overlay_path = format!("{}/overlay.ext4", tmp.as_path().to_str().unwrap());
    let delta_dir = format!("{}/deltas", tmp.as_path().to_str().unwrap());
    std::fs::create_dir(&delta_dir).unwrap();

    let disk_size = 1024 * 1024_u64;
    let vmm = create_vmm_with_overlay_drive(&base_path, &overlay_path, disk_size);
    // Drop the base file. Engine's open FD survives via inode pinning, but the
    // path is now gone — pre-flight's open(2) by path will get ENOENT.
    std::fs::remove_file(&base_path).unwrap();

    let mut controller = RuntimeApiController::new(vmm.clone());
    controller.handle_request(VmmAction::Pause).unwrap();

    let snapshot_file = TempFile::new().unwrap();
    let mem_file = TempFile::new().unwrap();
    let err = flatten_snapshot_expect_overlay_err(
        &mut controller,
        snapshot_file.as_path().to_path_buf(),
        mem_file.as_path().to_path_buf(),
        std::path::PathBuf::from(&delta_dir),
    );
    assert!(
        matches!(err, OverlayIoError::FlattenBaseOpen(_)),
        "expected FlattenBaseOpen, got {err:?}"
    );

    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

// Pre-flight error branch: base.ext4 size diverges from the engine's
// (block_size * total_blocks). Catches silent extend/truncate by failing loud.
#[test]
fn test_flatten_pre_flight_rejects_size_mismatch() {
    let tmp = TempDir::new().unwrap();
    let base_path = format!("{}/base.ext4", tmp.as_path().to_str().unwrap());
    let overlay_path = format!("{}/overlay.ext4", tmp.as_path().to_str().unwrap());
    let delta_dir = format!("{}/deltas", tmp.as_path().to_str().unwrap());
    std::fs::create_dir(&delta_dir).unwrap();

    let disk_size = 1024 * 1024_u64;
    let vmm = create_vmm_with_overlay_drive(&base_path, &overlay_path, disk_size);
    // Truncate base to a wrong size. Engine's FD is unaffected, but
    // pre-flight reads metadata().len() and compares against bitmap dims.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&base_path)
        .unwrap()
        .set_len(disk_size - 4096)
        .unwrap();

    let mut controller = RuntimeApiController::new(vmm.clone());
    controller.handle_request(VmmAction::Pause).unwrap();

    let snapshot_file = TempFile::new().unwrap();
    let mem_file = TempFile::new().unwrap();
    let err = flatten_snapshot_expect_overlay_err(
        &mut controller,
        snapshot_file.as_path().to_path_buf(),
        mem_file.as_path().to_path_buf(),
        std::path::PathBuf::from(&delta_dir),
    );
    match err {
        OverlayIoError::FlattenBaseSizeMismatch { expected, actual } => {
            assert_eq!(expected, disk_size);
            assert_eq!(actual, disk_size - 4096);
        }
        other => panic!("expected FlattenBaseSizeMismatch, got {other:?}"),
    }

    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

// Skip non-overlay device branch: a VMM with one overlay drive AND one
// non-overlay drive flattens cleanly (succeeds), touching only the overlay.
#[test]
fn test_flatten_skips_non_overlay_device() {
    use vmm::test_utils::mock_resources::{MockBootSourceConfig, MockVmResources};
    use vmm::vmm_config::drive::FileEngineType;

    let tmp = TempDir::new().unwrap();
    let base_path = format!("{}/base.ext4", tmp.as_path().to_str().unwrap());
    let overlay_path = format!("{}/overlay.ext4", tmp.as_path().to_str().unwrap());
    let non_overlay_path = format!("{}/regular.ext4", tmp.as_path().to_str().unwrap());
    let delta_dir = format!("{}/deltas", tmp.as_path().to_str().unwrap());
    std::fs::create_dir(&delta_dir).unwrap();

    let disk_size = 1024 * 1024_u64;
    for p in [&base_path, &overlay_path, &non_overlay_path] {
        std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).open(p).unwrap()
            .set_len(disk_size).unwrap();
    }

    let mut event_manager = EventManager::new().unwrap();
    let empty_seccomp_filters = get_empty_filters();
    let boot_source_cfg: BootSourceConfig = MockBootSourceConfig::new()
        .with_default_boot_args()
        .into();
    let mut resources: VmResources =
        MockVmResources::new().with_boot_source(boot_source_cfg).into();

    // Overlay drive: gets flattened.
    resources.set_block_device(BlockDeviceConfig {
        drive_id: "overlay_drive".to_string(),
        partuuid: None,
        is_root_device: false,
        cache_type: CacheType::Unsafe,
        is_read_only: Some(false),
        path_on_host: Some(overlay_path),
        rate_limiter: None,
        file_engine_type: Some(FileEngineType::Overlay),
        socket: None,
        base_path: Some(base_path),
    }).unwrap();
    // Non-overlay drive: must be skipped by flatten without error.
    resources.set_block_device(BlockDeviceConfig {
        drive_id: "regular_drive".to_string(),
        partuuid: None,
        is_root_device: false,
        cache_type: CacheType::Unsafe,
        is_read_only: Some(false),
        path_on_host: Some(non_overlay_path),
        rate_limiter: None,
        file_engine_type: None,
        socket: None,
        base_path: None,
    }).unwrap();

    let vmm = build_and_boot_microvm(
        &InstanceInfo::default(),
        &resources,
        &mut event_manager,
        &empty_seccomp_filters,
    ).unwrap();
    vmm.lock().unwrap().resume_vm().unwrap();

    let mut controller = RuntimeApiController::new(vmm.clone());
    controller.handle_request(VmmAction::Pause).unwrap();

    let snapshot_file = TempFile::new().unwrap();
    let mem_file = TempFile::new().unwrap();
    controller
        .handle_request(VmmAction::CreateSnapshot(CreateSnapshotParams {
            snapshot_type: SnapshotType::Full,
            snapshot_path: snapshot_file.as_path().to_path_buf(),
            mem_file_path: mem_file.as_path().to_path_buf(),
            block_delta_dir: Some(std::path::PathBuf::from(&delta_dir)),
            flatten: true,
        }))
        .expect("flatten must succeed even with a non-overlay drive in the mix");

    // Non-overlay drive must NOT have produced a delta file (only overlay devices do).
    assert!(
        !std::path::Path::new(&format!("{delta_dir}/regular_drive.delta")).exists(),
        "non-overlay drive should not emit a delta file"
    );
    // Overlay drive's delta should exist + be empty-form.
    assert_eq!(delta_dirty_count(format!("{delta_dir}/overlay_drive.delta")), 0);

    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

// `flatten: true` without `block_delta_dir` must error before any mutation.
// Snapshot paths are intentionally bogus — if validation didn't fire first,
// the error would be a backing-file write failure, not FlattenRequiresDeltaDir.
#[test]
fn test_create_snapshot_flatten_requires_delta_dir() {
    let (vmm, _) = default_vmm(Some(NOISY_KERNEL_IMAGE));
    let mut controller = RuntimeApiController::new(vmm.clone());
    thread::sleep(Duration::from_millis(200));
    controller.handle_request(VmmAction::Pause).unwrap();

    let params = CreateSnapshotParams {
        snapshot_type: SnapshotType::Full,
        snapshot_path: std::path::PathBuf::from("/this/should/never/be/written"),
        mem_file_path: std::path::PathBuf::from("/this/should/never/be/written.mem"),
        block_delta_dir: None,
        flatten: true,
    };
    let err = controller
        .handle_request(VmmAction::CreateSnapshot(params))
        .expect_err("expected FlattenRequiresDeltaDir");
    assert!(
        matches!(
            err,
            VmmActionError::CreateSnapshot(CreateSnapshotError::FlattenRequiresDeltaDir)
        ),
        "unexpected error: {err:?}"
    );

    vmm.lock().unwrap().stop(FcExitCode::Ok);
}

#[test]
fn test_create_and_load_snapshot() {
    for diff_snap in [false, true] {
        for pci_enabled in [false, true] {
            for memory_hotplug in [false, true] {
                // Create snapshot.
                let (snapshot_file, memory_file) =
                    verify_create_snapshot(diff_snap, pci_enabled, memory_hotplug);
                // Create a new microVm from snapshot. This only tests code-level logic; it verifies
                // that a microVM can be built with no errors from given snapshot.
                // It does _not_ verify that the guest is actually restored properly. We're using
                // python integration tests for that.
                verify_load_snapshot(snapshot_file, memory_file);
            }
        }
    }
}

#[test]
fn test_snapshot_load_sanity_checks() {
    let microvm_state = get_microvm_state_from_snapshot(false);
    check_snapshot(microvm_state);
    let microvm_state = get_microvm_state_from_snapshot(true);
    check_snapshot(microvm_state);
}

fn check_snapshot(mut microvm_state: MicrovmState) {
    use vmm::persist::SnapShotStateSanityCheckError;
    snapshot_state_sanity_check(&microvm_state).unwrap();

    // Remove memory regions.
    microvm_state.vm_state.memory.regions.clear();

    // Validate sanity checks fail because there is no mem region in state.
    assert_eq!(
        snapshot_state_sanity_check(&microvm_state),
        Err(SnapShotStateSanityCheckError::NoMemory)
    );
}

fn get_microvm_state_from_snapshot(pci_enabled: bool) -> MicrovmState {
    // Create a diff snapshot
    let (snapshot_file, _) = verify_create_snapshot(true, pci_enabled, false);

    // Deserialize the microVM state.
    snapshot_file.as_file().seek(SeekFrom::Start(0)).unwrap();
    Snapshot::load(&mut snapshot_file.as_file()).unwrap().data
}

fn verify_load_snap_disallowed_after_boot_resources(res: VmmAction, res_name: &str) {
    let (snapshot_file, memory_file) = verify_create_snapshot(false, false, false);

    let mut event_manager = EventManager::new().unwrap();
    let empty_seccomp_filters = get_empty_filters();
    let mut vm_resources = VmResources::default();

    let mut preboot_api_controller = PrebootApiController::new(
        &empty_seccomp_filters,
        InstanceInfo::default(),
        &mut vm_resources,
        &mut event_manager,
    );

    preboot_api_controller.handle_preboot_request(res).unwrap();

    // Load snapshot should no longer be allowed.
    let req = VmmAction::LoadSnapshot(LoadSnapshotParams {
        snapshot_path: snapshot_file.as_path().to_path_buf(),
        mem_backend: MemBackendConfig {
            backend_path: memory_file.as_path().to_path_buf(),
            backend_type: MemBackendType::File,
            access_log_path: None,
            record_to: None,
        },
        track_dirty_pages: false,
        resume_vm: false,
        network_overrides: vec![],
        block_delta_dir: None,
    });
    let err = preboot_api_controller.handle_preboot_request(req);
    assert!(
        matches!(
            err.unwrap_err(),
            VmmActionError::LoadSnapshot(LoadSnapshotError::LoadSnapshotNotAllowed)
        ),
        "LoadSnapshot should be disallowed after {}",
        res_name
    );
}

#[test]
fn test_preboot_load_snap_disallowed_after_boot_resources() {
    let tmp_file = TempFile::new().unwrap();
    let tmp_file = tmp_file.as_path().to_str().unwrap().to_string();
    // Verify LoadSnapshot not allowed after configuring various boot-specific resources.
    let req = VmmAction::ConfigureBootSource(BootSourceConfig {
        kernel_image_path: tmp_file.clone(),
        ..Default::default()
    });
    verify_load_snap_disallowed_after_boot_resources(req, "ConfigureBootSource");

    let config = BlockDeviceConfig {
        drive_id: String::new(),
        partuuid: None,
        is_root_device: false,
        cache_type: CacheType::Unsafe,

        is_read_only: Some(false),
        path_on_host: Some(tmp_file),
        rate_limiter: None,
        file_engine_type: None,

        socket: None,
        base_path: None,
    };

    let req = VmmAction::InsertBlockDevice(config);
    verify_load_snap_disallowed_after_boot_resources(req, "InsertBlockDevice");

    let req = VmmAction::InsertNetworkDevice(NetworkInterfaceConfig {
        iface_id: String::new(),
        host_dev_name: String::new(),
        guest_mac: None,
        rx_rate_limiter: None,
        tx_rate_limiter: None,
    });
    verify_load_snap_disallowed_after_boot_resources(req, "InsertNetworkDevice");

    let req = VmmAction::SetBalloonDevice(BalloonDeviceConfig::default());
    verify_load_snap_disallowed_after_boot_resources(req, "SetBalloonDevice");

    let req = VmmAction::SetVsockDevice(VsockDeviceConfig {
        vsock_id: Some(String::new()),
        guest_cid: 0,
        uds_path: String::new(),
    });
    verify_load_snap_disallowed_after_boot_resources(req, "SetVsockDevice");

    let req =
        VmmAction::UpdateMachineConfiguration(MachineConfigUpdate::from(MachineConfig::default()));
    verify_load_snap_disallowed_after_boot_resources(req, "SetVmConfiguration");
}
