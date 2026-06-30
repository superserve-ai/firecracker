// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configurations used in the snapshotting context.

use std::path::PathBuf;

/// For crates that depend on `vmm` we export.
pub use semver::Version;
use serde::{Deserialize, Serialize};

/// The snapshot type options that are available when
/// creating a new snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum SnapshotType {
    /// Diff snapshot.
    Diff,
    /// Full snapshot.
    #[default]
    Full,
}

/// Specifies the method through which guest memory will get populated when
/// resuming from a snapshot:
/// 1) A file that contains the guest memory to be loaded,
/// 2) An UDS where a custom page-fault handler process is listening for the UFFD set up by
///    Firecracker to handle its guest memory page faults,
/// 3) An in-process UFFD handler that lives inside firecracker — UFFD lifecycle equals VM
///    lifecycle, no external coordination required.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub enum MemBackendType {
    /// Guest memory contents will be loaded from a file.
    File,
    /// Guest memory will be served through UFFD by a separate process.
    Uffd,
    /// Guest memory will be served through UFFD by a handler thread inside firecracker.
    /// `backend_path` points to the snapshot memory file (mem.snap).
    UffdInternal,
}

/// Stores the configuration that will be used for creating a snapshot.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSnapshotParams {
    /// This marks the type of snapshot we want to create.
    /// The default value is `Full`, which means a full snapshot.
    #[serde(default = "SnapshotType::default")]
    pub snapshot_type: SnapshotType,
    /// Path to the file that will contain the microVM state.
    pub snapshot_path: PathBuf,
    /// Path to the file that will contain the guest memory.
    pub mem_file_path: PathBuf,
    /// Optional directory for block device delta files. When set, overlay block
    /// devices will write delta files (containing only dirty blocks) into this
    /// directory, named `{drive_id}.delta`.
    #[serde(default)]
    pub block_delta_dir: Option<PathBuf>,
    /// If true, bake each overlay's dirty blocks into its base.ext4 in place
    /// and zero the side-car bitmap. Restores skip apply_delta. Only safe at
    /// template-creation time — corrupts bases shared with other live VMs.
    /// Requires `block_delta_dir`.
    #[serde(default)]
    pub flatten: bool,
    /// If true (and `snapshot_type` is `Diff`), write the memory diff on a background
    /// thread and return at the snapshot point; confirm durability with
    /// `PUT /snapshot/complete`. The vCPUs must stay paused until then. Ignored for
    /// `Full` snapshots.
    #[serde(default)]
    pub async_snapshot: bool,
}

/// Allows for changing the mapping between tap devices and host devices
/// during snapshot restore
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct NetworkOverride {
    /// The index of the interface to modify
    pub iface_id: String,
    /// The new name of the interface to be assigned
    pub host_dev_name: String,
}

/// Stores the configuration that will be used for loading a snapshot.
#[derive(Debug, PartialEq, Eq)]
pub struct LoadSnapshotParams {
    /// Path to the file that contains the microVM state to be loaded.
    pub snapshot_path: PathBuf,
    /// Specifies guest memory backend configuration.
    pub mem_backend: MemBackendConfig,
    /// Whether KVM dirty page tracking should be enabled, to space optimization
    /// of differential snapshots.
    pub track_dirty_pages: bool,
    /// When set to true, the vm is also resumed if the snapshot load
    /// is successful.
    pub resume_vm: bool,
    /// The network devices to override on load.
    pub network_overrides: Vec<NetworkOverride>,
    /// Optional directory containing block device delta files for cloning.
    /// Each overlay device will look for `{drive_id}.delta` in this directory
    /// and apply it to a fresh overlay, enabling fast VM cloning.
    pub block_delta_dir: Option<PathBuf>,
    /// Back the restored guest memory with a memfd (MAP_SHARED) instead of an
    /// anonymous mapping. See `MachineConfig::shared_mem`.
    pub shared_mem: bool,
}

/// Stores the configuration for loading a snapshot that is provided by the user.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSnapshotConfig {
    /// Path to the file that contains the microVM state to be loaded.
    pub snapshot_path: PathBuf,
    /// Path to the file that contains the guest memory to be loaded. To be used only if
    /// `mem_backend` is not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_file_path: Option<PathBuf>,
    /// Guest memory backend configuration. Is not to be used in conjunction with `mem_file_path`.
    /// None value is allowed only if `mem_file_path` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_backend: Option<MemBackendConfig>,
    /// Whether or not to enable KVM dirty page tracking.
    #[serde(default)]
    #[deprecated]
    pub enable_diff_snapshots: bool,
    /// Whether KVM dirty page tracking should be enabled.
    #[serde(default)]
    pub track_dirty_pages: bool,
    /// Whether or not to resume the vm post snapshot load.
    #[serde(default)]
    pub resume_vm: bool,
    /// The network devices to override on load.
    #[serde(default)]
    pub network_overrides: Vec<NetworkOverride>,
    /// Optional directory containing block device delta files for cloning.
    #[serde(default)]
    pub block_delta_dir: Option<PathBuf>,
    /// Back the restored guest memory with a memfd (MAP_SHARED). See
    /// `MachineConfig::shared_mem`.
    #[serde(default)]
    pub shared_mem: bool,
}

/// Stores the configuration used for managing snapshot memory.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemBackendConfig {
    /// Path to the backend used to handle the guest memory. For `Uffd` this is the UDS
    /// where the external handler is listening; for `File` and `UffdInternal` this is
    /// the path to the snapshot memory file. In layered `UffdInternal` restore this is
    /// the overlay (diff) file; pages absent from it are served from `base_path`.
    pub backend_path: PathBuf,
    /// Specifies the guest memory backend type.
    pub backend_type: MemBackendType,
    /// Base (template) memory file for a layered `UffdInternal` restore. When set, a page
    /// is served from `backend_path` if present there, else from this base. Only
    /// meaningful for `UffdInternal`; ignored otherwise.
    #[serde(default)]
    pub base_path: Option<PathBuf>,
    /// Intermediate overlay (diff) files between `base_path` and `backend_path`, ordered
    /// oldest → newest, for a multi-layer `UffdInternal` restore. Empty for a single-overlay
    /// or monolithic restore. Only meaningful for `UffdInternal` with `base_path` set;
    /// ignored otherwise.
    #[serde(default)]
    pub lower_overlay_paths: Vec<PathBuf>,
    /// When true, an unexpected `UffdInternal` handler death aborts Firecracker instead
    /// of leaving the guest to freeze on its next page fault. Only meaningful for
    /// `UffdInternal`; ignored otherwise. Defaults to false (opt-in).
    #[serde(default)]
    pub abort_on_handler_death: bool,
    /// Path to a recorded page-access trace to replay as prefetch. Only meaningful for
    /// `UffdInternal`; ignored otherwise. Absent or unreadable → fall back to sequential
    /// prefetch.
    #[serde(default)]
    pub access_log_path: Option<PathBuf>,
    /// If set, the internal UFFD handler will write each served page offset to this path
    /// (template-build recording mode). Only meaningful for `UffdInternal`; ignored
    /// otherwise. When present, prefetch is disabled so the captured trace reflects the
    /// guest's natural access pattern.
    #[serde(default)]
    pub record_to: Option<PathBuf>,
}

/// The microVM state options.
#[derive(Debug, Deserialize, Serialize)]
pub enum VmState {
    /// The microVM is paused, which means that we can create a snapshot of it.
    Paused,
    /// The microVM is resumed; this state should be set after we load a snapshot.
    Resumed,
}

/// Keeps the microVM state necessary in the snapshotting context.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    /// The microVM state, which can be `paused` or `resumed`.
    pub state: VmState,
}
