// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::de::Error as DeserializeError;
use vmm::logger::{IncMetric, METRICS};
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotConfig, LoadSnapshotParams, MemBackendConfig, MemBackendType,
    Vm, VmState,
};

use super::super::parsed_request::{ParsedRequest, RequestError};
use super::super::request::{Body, Method, StatusCode};

/// Deprecation message for the `mem_file_path` field.
const LOAD_DEPRECATION_MESSAGE: &str =
    "PUT /snapshot/load: mem_file_path and enable_diff_snapshots fields are deprecated.";
/// None of the `mem_backend` or `mem_file_path` fields has been specified.
pub const MISSING_FIELD: &str =
    "missing field: either `mem_backend` or `mem_file_path` is required";
/// Both the `mem_backend` and `mem_file_path` fields have been specified.
/// Only specifying one of them is allowed.
pub const TOO_MANY_FIELDS: &str =
    "too many fields: either `mem_backend` or `mem_file_path` exclusively is required";
/// Upper bound on a dirty-tracking session id: room for a UUID or a hex
/// nonce, bounded so the token is never a caller-sized allocation.
pub const TRACKING_SESSION_ID_MAX_LEN: usize = 128;

/// A session id must be a non-empty, bounded token of `[A-Za-z0-9_-]`. The
/// shape cannot prove the id is fresh per load, but it rejects the empty and
/// structured values that make reuse likely, and the same rule on both
/// endpoints makes a malformed expected token a 400 rather than a silent
/// mismatch.
fn validate_session_id(field: &str, id: &str) -> Result<(), RequestError> {
    let well_formed = !id.is_empty()
        && id.len() <= TRACKING_SESSION_ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if well_formed {
        return Ok(());
    }
    Err(RequestError::Generic(
        StatusCode::BadRequest,
        format!(
            "{field} must be 1..={TRACKING_SESSION_ID_MAX_LEN} characters of [A-Za-z0-9_-], \
             unique per snapshot load"
        ),
    ))
}

pub(crate) fn parse_put_snapshot(
    body: &Body,
    request_type_from_path: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    match request_type_from_path {
        Some(request_type) => match request_type {
            "create" => parse_put_snapshot_create(body),
            "load" => parse_put_snapshot_load(body),
            _ => Err(RequestError::InvalidPathMethod(
                format!("/snapshot/{}", request_type),
                Method::Put,
            )),
        },
        None => Err(RequestError::Generic(
            StatusCode::BadRequest,
            "Missing snapshot operation type.".to_string(),
        )),
    }
}

pub(crate) fn parse_patch_vm_state(body: &Body) -> Result<ParsedRequest, RequestError> {
    let vm = serde_json::from_slice::<Vm>(body.raw())?;

    match vm.state {
        VmState::Paused => Ok(ParsedRequest::new_sync(VmmAction::Pause)),
        VmState::Resumed => Ok(ParsedRequest::new_sync(VmmAction::Resume)),
    }
}

fn parse_put_snapshot_create(body: &Body) -> Result<ParsedRequest, RequestError> {
    let snapshot_config = serde_json::from_slice::<CreateSnapshotParams>(body.raw())?;
    if let Some(id) = &snapshot_config.expected_session_id {
        validate_session_id("expected_session_id", id)?;
    }
    Ok(ParsedRequest::new_sync(VmmAction::CreateSnapshot(
        snapshot_config,
    )))
}

fn parse_put_snapshot_load(body: &Body) -> Result<ParsedRequest, RequestError> {
    let snapshot_config = serde_json::from_slice::<LoadSnapshotConfig>(body.raw())?;
    if let Some(id) = &snapshot_config.tracking_session_id {
        validate_session_id("tracking_session_id", id)?;
    }

    match (&snapshot_config.mem_backend, &snapshot_config.mem_file_path) {
        // Ensure `mem_file_path` and `mem_backend` fields are not present at the same time.
        (Some(_), Some(_)) => {
            return Err(RequestError::SerdeJson(serde_json::Error::custom(
                TOO_MANY_FIELDS,
            )));
        }
        // Ensure that one of `mem_file_path` or `mem_backend` fields is always specified.
        (None, None) => {
            return Err(RequestError::SerdeJson(serde_json::Error::custom(
                MISSING_FIELD,
            )));
        }
        _ => {}
    }

    // Check for the presence of deprecated `mem_file_path` field and create
    // deprecation message if found.
    let mut deprecation_message = None;
    #[allow(deprecated)]
    if snapshot_config.mem_file_path.is_some() || snapshot_config.enable_diff_snapshots {
        // `mem_file_path` field in request is deprecated.
        METRICS.deprecated_api.deprecated_http_api_calls.inc();
        deprecation_message = Some(LOAD_DEPRECATION_MESSAGE);
    }

    // If `mem_file_path` is specified instead of `mem_backend`, we construct the
    // `MemBackendConfig` object from the path specified, with `File` as backend type.
    let mem_backend = match snapshot_config.mem_backend {
        Some(backend_cfg) => backend_cfg,
        None => {
            MemBackendConfig {
                // This is safe to unwrap() because we ensure above that one of the two:
                // either `mem_file_path` or `mem_backend` field is always specified.
                backend_path: snapshot_config.mem_file_path.unwrap(),
                backend_type: MemBackendType::File,
                abort_on_handler_death: false,
                base_path: None,
                access_log_path: None,
                record_to: None,
            }
        }
    };

    let snapshot_params = LoadSnapshotParams {
        snapshot_path: snapshot_config.snapshot_path,
        mem_backend,
        #[allow(deprecated)]
        track_dirty_pages: snapshot_config.enable_diff_snapshots
            || snapshot_config.track_dirty_pages,
        resume_vm: snapshot_config.resume_vm,
        network_overrides: snapshot_config.network_overrides,
        block_delta_dir: snapshot_config.block_delta_dir,
        clock_realtime: snapshot_config.clock_realtime,
        tracking_session_id: snapshot_config.tracking_session_id,
    };

    // Construct the `ParsedRequest` object.
    let mut parsed_req = ParsedRequest::new_sync(VmmAction::LoadSnapshot(snapshot_params));

    // If `mem_file_path` was present, set the deprecation message in `parsing_info`.
    if let Some(msg) = deprecation_message {
        parsed_req.parsing_info().append_deprecation_message(msg);
    }

    Ok(parsed_req)
}

#[cfg(test)]
mod tests {
    use vmm::vmm_config::snapshot::{MemBackendConfig, MemBackendType, NetworkOverride};

    use super::*;
    use crate::api_server::parsed_request::tests::{depr_action_from_req, vmm_action_from_request};

    #[test]
    fn test_parse_put_snapshot_clock_realtime_tristate() {
        // Omitted, `false` and `true` are three distinct requests: omission restores the
        // snapshot's own clock flags, which is what callers got before the field existed.
        fn parse(extra: &str) -> Option<bool> {
            let body = format!(
                r#"{{
                    "snapshot_path": "foo",
                    "mem_backend": {{
                        "backend_path": "bar",
                        "backend_type": "File"
                    }}{extra}
                }}"#
            );
            let parsed = parse_put_snapshot(&Body::new(body), Some("load")).unwrap();
            match vmm_action_from_request(parsed) {
                VmmAction::LoadSnapshot(cfg) => cfg.clock_realtime,
                _ => panic!("expected LoadSnapshot"),
            }
        }

        assert_eq!(parse(""), None);
        assert_eq!(parse(r#", "clock_realtime": false"#), Some(false));
        assert_eq!(parse(r#", "clock_realtime": true"#), Some(true));
    }

    #[test]
    fn test_parse_put_snapshot_session_fields() {
        // Both endpoints accept a well-formed token and reject the same
        // malformed shapes.
        fn load(extra: &str) -> Result<Option<String>, RequestError> {
            let body = format!(
                r#"{{
                    "snapshot_path": "foo",
                    "mem_backend": {{"backend_path": "bar", "backend_type": "File"}},
                    "track_dirty_pages": true{extra}
                }}"#
            );
            parse_put_snapshot(&Body::new(body), Some("load")).map(|parsed| {
                match vmm_action_from_request(parsed) {
                    VmmAction::LoadSnapshot(cfg) => cfg.tracking_session_id,
                    _ => panic!("expected LoadSnapshot"),
                }
            })
        }
        let hex = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            load(&format!(r#", "tracking_session_id": "{hex}""#)).unwrap(),
            Some(hex.to_string())
        );
        assert_eq!(load("").unwrap(), None);
        load(r#", "tracking_session_id": """#).unwrap_err();
        load(r#", "tracking_session_id": "has space""#).unwrap_err();
        let max = "a".repeat(TRACKING_SESSION_ID_MAX_LEN);
        assert!(load(&format!(r#", "tracking_session_id": "{max}""#)).is_ok());
        load(&format!(r#", "tracking_session_id": "{max}a""#)).unwrap_err();

        fn create(extra: &str) -> Result<(Option<String>, Option<u64>), RequestError> {
            let body = format!(
                r#"{{"snapshot_path": "foo", "mem_file_path": "bar", "snapshot_type": "Diff"{extra}}}"#
            );
            parse_put_snapshot(&Body::new(body), Some("create")).map(|parsed| {
                match vmm_action_from_request(parsed) {
                    VmmAction::CreateSnapshot(cfg) => {
                        (cfg.expected_session_id, cfg.expected_generation)
                    }
                    _ => panic!("expected CreateSnapshot"),
                }
            })
        }
        assert_eq!(
            create(&format!(
                r#", "expected_session_id": "{hex}", "expected_generation": 3"#
            ))
            .unwrap(),
            (Some(hex.to_string()), Some(3))
        );
        assert_eq!(create("").unwrap(), (None, None));
        create(r#", "expected_session_id": """#).unwrap_err();
        create(r#", "expected_session_id": "bad/slash""#).unwrap_err();
    }

    #[test]
    fn test_parse_put_snapshot() {
        use std::path::PathBuf;

        use vmm::vmm_config::snapshot::SnapshotType;

        let body = r#"{
            "snapshot_type": "Diff",
            "snapshot_path": "foo",
            "mem_file_path": "bar"
        }"#;
        let expected_config = CreateSnapshotParams {
            snapshot_type: SnapshotType::Diff,
            snapshot_path: PathBuf::from("foo"),
            mem_file_path: PathBuf::from("bar"),
            block_delta_dir: None,
            flatten: false,
            expected_session_id: None,
            expected_generation: None,
        };
        assert_eq!(
            vmm_action_from_request(parse_put_snapshot(&Body::new(body), Some("create")).unwrap()),
            VmmAction::CreateSnapshot(expected_config)
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_file_path": "bar"
        }"#;
        let expected_config = CreateSnapshotParams {
            snapshot_type: SnapshotType::Full,
            snapshot_path: PathBuf::from("foo"),
            mem_file_path: PathBuf::from("bar"),
            block_delta_dir: None,
            flatten: false,
            expected_session_id: None,
            expected_generation: None,
        };
        assert_eq!(
            vmm_action_from_request(parse_put_snapshot(&Body::new(body), Some("create")).unwrap()),
            VmmAction::CreateSnapshot(expected_config)
        );

        let invalid_body = r#"{
            "invalid_field": "foo",
            "mem_file_path": "bar"
        }"#;
        parse_put_snapshot(&Body::new(invalid_body), Some("create")).unwrap_err();

        let body = r#"{
            "snapshot_path": "foo",
            "mem_backend": {
                "backend_path": "bar",
                "backend_type": "File"
            }
        }"#;
        let expected_config = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                base_path: None,
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::File,
                abort_on_handler_death: false,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: false,
            network_overrides: vec![],
            block_delta_dir: None,
            clock_realtime: None,
            tracking_session_id: None,
        };
        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some("load")).unwrap();
        assert!(
            parsed_request
                .parsing_info()
                .take_deprecation_message()
                .is_none()
        );
        assert_eq!(
            vmm_action_from_request(parsed_request),
            VmmAction::LoadSnapshot(expected_config)
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_backend": {
                "backend_path": "bar",
                "backend_type": "File"
            },
            "track_dirty_pages": true
        }"#;
        let expected_config = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                base_path: None,
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::File,
                abort_on_handler_death: false,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: true,
            resume_vm: false,
            network_overrides: vec![],
            block_delta_dir: None,
            clock_realtime: None,
            tracking_session_id: None,
        };
        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some("load")).unwrap();
        assert!(
            parsed_request
                .parsing_info()
                .take_deprecation_message()
                .is_none()
        );
        assert_eq!(
            vmm_action_from_request(parsed_request),
            VmmAction::LoadSnapshot(expected_config)
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_backend": {
                "backend_path": "bar",
                "backend_type": "Uffd"
            },
            "resume_vm": true
        }"#;
        let expected_config = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                base_path: None,
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::Uffd,
                abort_on_handler_death: false,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: true,
            network_overrides: vec![],
            block_delta_dir: None,
            clock_realtime: None,
            tracking_session_id: None,
        };
        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some("load")).unwrap();
        assert!(
            parsed_request
                .parsing_info()
                .take_deprecation_message()
                .is_none()
        );
        assert_eq!(
            vmm_action_from_request(parsed_request),
            VmmAction::LoadSnapshot(expected_config)
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_backend": {
                "backend_path": "bar",
                "backend_type": "Uffd"
            },
            "resume_vm": true,
            "network_overrides": [
                {
                    "iface_id": "eth0",
                    "host_dev_name": "vmtap2"
                }
            ]
        }"#;
        let expected_config = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                base_path: None,
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::Uffd,
                abort_on_handler_death: false,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: true,
            network_overrides: vec![NetworkOverride {
                iface_id: String::from("eth0"),
                host_dev_name: String::from("vmtap2"),
            }],
            block_delta_dir: None,
            clock_realtime: None,
            tracking_session_id: None,
        };
        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some("load")).unwrap();
        assert!(
            parsed_request
                .parsing_info()
                .take_deprecation_message()
                .is_none()
        );
        assert_eq!(
            vmm_action_from_request(parsed_request),
            VmmAction::LoadSnapshot(expected_config)
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_file_path": "bar",
            "resume_vm": true
        }"#;
        let expected_config = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                base_path: None,
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::File,
                abort_on_handler_death: false,
                access_log_path: None,
                record_to: None,
            },
            track_dirty_pages: false,
            resume_vm: true,
            network_overrides: vec![],
            block_delta_dir: None,
            clock_realtime: None,
            tracking_session_id: None,
        };
        let parsed_request = parse_put_snapshot(&Body::new(body), Some("load")).unwrap();
        assert_eq!(
            depr_action_from_req(parsed_request, Some(LOAD_DEPRECATION_MESSAGE.to_string())),
            VmmAction::LoadSnapshot(expected_config)
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_backend": {
                "backend_path": "bar"
            }
        }"#;
        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some("load"))
                .err()
                .unwrap()
                .to_string(),
            "An error occurred when deserializing the json body of a request: missing field \
             `backend_type` at line 5 column 13."
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_backend": {
                "backend_type": "File",
            }
        }"#;
        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some("load"))
                .err()
                .unwrap()
                .to_string(),
            "An error occurred when deserializing the json body of a request: trailing comma at \
             line 5 column 13."
        );

        let body = r#"{
            "snapshot_path": "foo",
            "mem_file_path": "bar",
            "mem_backend": {
                "backend_path": "bar",
                "backend_type": "Uffd"
            }
        }"#;
        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some("load"))
                .err()
                .unwrap()
                .to_string(),
            RequestError::SerdeJson(serde_json::Error::custom(TOO_MANY_FIELDS.to_string()))
                .to_string()
        );

        let body = r#"{
            "snapshot_path": "foo"
        }"#;
        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some("load"))
                .err()
                .unwrap()
                .to_string(),
            RequestError::SerdeJson(serde_json::Error::custom(MISSING_FIELD.to_string()))
                .to_string()
        );

        let body = r#"{
            "mem_backend": {
                "backend_path": "bar",
                "backend_type": "Uffd"
            }
        }"#;
        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some("load"))
                .err()
                .unwrap()
                .to_string(),
            "An error occurred when deserializing the json body of a request: missing field \
             `snapshot_path` at line 6 column 9."
        );
        parse_put_snapshot(&Body::new(body), Some("invalid")).unwrap_err();
        parse_put_snapshot(&Body::new(body), None).unwrap_err();
    }

    #[test]
    fn test_parse_patch_vm_state() {
        let body = r#"{
            "state": "Paused"
        }"#;
        assert!(
            parse_patch_vm_state(&Body::new(body))
                .unwrap()
                .eq(&ParsedRequest::new_sync(VmmAction::Pause))
        );

        let body = r#"{
            "state": "Resumed"
        }"#;
        assert!(
            parse_patch_vm_state(&Body::new(body))
                .unwrap()
                .eq(&ParsedRequest::new_sync(VmmAction::Resume))
        );

        let invalid_body = r#"{
            "invalid": "Paused"
        }"#;
        parse_patch_vm_state(&Body::new(invalid_body)).unwrap_err();
    }
}
