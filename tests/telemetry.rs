use esp32c3_flashtool::{
    backend::DeviceIdentity,
    production::ProductionBatch,
    telemetry::{
        CompiledExtractionRules, ExtractionOutcome, RecordDraft, RecordOperation, RecordResult,
        RecordService, RecordStatus, TelemetrySettings, UNIQUE_DEVICE_ID_KEY, UploadState,
        apply_hardware_unique_id, load_settings_from, parse_extraction_rules_json,
        save_settings_at, validate_settings,
    },
    telemetry_http::{post_json, validate_custom_headers, validate_endpoint},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const EFUSE: &str = "AABBCCDDEEFF";
const MAC: &str = "AA:BB:CC:DD:EE:FF";
const PRIVATE_TEST_HEADER: &str = "Bearer telemetry-local-test-secret";
const CUSTOM_ID: &str = "00112233445566778899AABBCCDDEEFF";

fn custom_rules() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "unique_device_id".into(),
        r"UNIQUE DEVICE ID:\s*([0-9A-Fa-f]{32})\b".into(),
    )])
}

#[test]
fn extraction_rules_json_is_bounded_strict_and_old_settings_remain_loadable() {
    let rules = parse_extraction_rules_json(
        r#"{"unique_device_id":"UNIQUE DEVICE ID:\\s*([0-9A-Fa-f]{32})\\b"}"#,
    )
    .unwrap();
    assert_eq!(rules, custom_rules());
    assert!(parse_extraction_rules_json("  ").unwrap().is_empty());
    for text in [
        r#"{"id":"(a)","id":"(b)"}"#,
        r#"{"id":true}"#,
        "[]",
        "null",
        r#"{"id":"(a)"} {}"#,
        r#"{"bad-key":"(a)"}"#,
        r#"{"1id":"(a)"}"#,
        r#"{"id":"no capture"}"#,
        r#"{"id":"("}"#,
    ] {
        assert!(parse_extraction_rules_json(text).is_err(), "{text}");
    }
    assert!(parse_extraction_rules_json(&" ".repeat(64 * 1024 + 1)).is_err());
    let too_many: BTreeMap<_, _> = (0..33)
        .map(|index| (format!("key{index}"), "(a)".into()))
        .collect();
    assert!(CompiledExtractionRules::new(&too_many).is_err());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("telemetry.json");
    fs::write(&path, "{\"upload_enabled\":false}").unwrap();
    assert!(
        load_settings_from(&path)
            .unwrap()
            .extraction_rules
            .is_empty()
    );
    let mut settings = settings(root.path());
    settings.extraction_rules = rules;
    save_settings_at(&path, &settings).unwrap();
    assert_eq!(load_settings_from(&path).unwrap(), settings);
    fs::write(&path, r#"{"extraction_rules":{"id":"(a)","id":"(b)"}}"#).unwrap();
    assert!(load_settings_from(&path).is_err());
}

#[test]
fn reusable_matcher_keeps_ports_independent_and_reports_missing_or_ambiguous_fields() {
    let rules = BTreeMap::from([
        ("id".into(), r"ID=(\w+)".into()),
        ("board".into(), r"BOARD=(\w+)".into()),
    ]);
    let matcher = CompiledExtractionRules::new(&rules).unwrap();
    let a = matcher.extract(&["ID=AAA ID=AAA BOARD=c3".into()]);
    assert_eq!(a.custom_fields["id"], "AAA");
    assert_eq!(a.custom_fields["board"], "c3");
    assert!(a.errors.is_empty());
    let b = matcher.extract(&["ID=BBB ID=CCC".into()]);
    assert!(b.custom_fields.is_empty());
    assert!(b.errors["id"].contains("多个不同"));
    assert!(b.errors["board"].contains("未找到"));
    let c = matcher.extract(&["ID=DDD".into()]);
    assert_eq!(c.custom_fields["id"], "DDD");
    assert_eq!(c.errors.len(), 1);
}

#[test]
fn custom_capture_limits_preserve_finite_work_and_reject_empty_values() {
    let matcher =
        CompiledExtractionRules::new(&BTreeMap::from([("id".into(), r"ID=([^\n]*)".into())]))
            .unwrap();
    for text in [
        "ID=".into(),
        "ID=   ".into(),
        format!("ID={}", "a".repeat(4097)),
        "ID=a\0b".into(),
        "ID=a\n".repeat(4097),
    ] {
        let result = matcher.extract(&[text]);
        assert!(result.custom_fields.is_empty());
        assert_eq!(result.errors.len(), 1);
    }
    assert!(matcher.extract(&["a".repeat(1024 * 1024)]).errors["id"].contains("1 MiB"));
}

#[test]
fn flash_custom_fields_are_saved_in_stable_json_columns_and_posted_with_factory_identity() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![Reply::status(201)], Some(directory.path().to_owned()));
    let mut settings = settings(directory.path());
    settings.upload_enabled = true;
    settings.endpoint = server.endpoint.clone();
    settings.extraction_rules = custom_rules();
    let mut input = draft();
    input.logs.push(format!("UNIQUE DEVICE ID: {CUSTOM_ID}"));
    let mut service = RecordService::new(|| {});
    service.submit(&settings, input).unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.operation, RecordOperation::Flash);
    assert_eq!(result.upload_state, UploadState::Uploaded);
    assert_eq!(result.custom_fields["unique_device_id"], CUSTOM_ID);
    let fields = csv_fields(result.csv_path.as_ref().unwrap());
    assert_eq!(fields["operation"], "flash");
    assert_eq!(fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert_eq!(
        serde_json::from_str::<Value>(&fields["custom_fields"]).unwrap()["unique_device_id"],
        CUSTOM_ID
    );
    let header = &csv_rows(result.csv_path.as_ref().unwrap())[0];
    assert_eq!(header[15], "logs");
    assert_eq!(
        &header[16..20],
        &[
            "operation",
            "custom_fields",
            "custom_field_errors",
            UNIQUE_DEVICE_ID_KEY
        ]
    );
    assert_eq!(
        &header[20..],
        &["生产批次/工单号", "主板年周", "主板生产日期"]
    );
    let posted: Value = serde_json::from_slice(&server.request().body).unwrap();
    assert_eq!(posted["efuse_id"], EFUSE);
    assert_eq!(posted["custom_fields"]["unique_device_id"], CUSTOM_ID);
    assert_eq!(posted["operation"], "flash");
    assert!(
        posted.get(UNIQUE_DEVICE_ID_KEY).is_none(),
        "CSV column does not change the HTTP schema"
    );
    server.assert_no_request();
}

#[test]
fn readonly_custom_identity_can_upload_without_claiming_a_flash_or_factory_mac() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![Reply::status(201)], Some(directory.path().to_owned()));
    let mut settings = settings(directory.path());
    settings.upload_enabled = true;
    settings.endpoint = server.endpoint.clone();
    settings.extraction_rules = custom_rules();
    settings
        .extraction_rules
        .insert("optional_board".into(), r"BOARD=(\w+)".into());
    let input = readonly_draft();
    let mut service = RecordService::new(|| {});
    let readonly_id = service
        .submit_with_operation(&settings, input.clone(), RecordOperation::ReadOnly)
        .unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.operation, RecordOperation::ReadOnly);
    assert_eq!(result.upload_state, UploadState::Uploaded);
    assert_eq!(result.custom_fields["unique_device_id"], CUSTOM_ID);
    assert!(result.custom_field_errors.contains_key("optional_board"));
    let fields = csv_fields(result.csv_path.as_ref().unwrap());
    assert_eq!(fields["operation"], "read_only");
    assert_eq!(fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert!(fields["firmware_sha256"].is_empty());
    let posted: Value = serde_json::from_slice(&server.request().body).unwrap();
    assert_eq!(posted["operation"], "read_only");
    assert_eq!(posted["status"], "success");
    assert!(posted["efuse_id"].is_null() && posted["mac"].is_null());
    assert_eq!(posted["chip"], "");
    assert!(posted["identity_error"].is_null());
    // A custom field cannot bypass the original Flash operation's typed guard.
    let flash_id = service.submit(&settings, input).unwrap();
    assert_ne!(readonly_id, flash_id);
    let flash = receive_record(&mut service);
    assert_eq!(flash.upload_state, UploadState::SkippedIdentity);
    server.assert_no_request();
}

#[test]
fn unsuccessful_or_missing_readonly_capture_retains_csv_fields_and_never_posts() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![], None);
    let mut settings = settings(directory.path());
    settings.upload_enabled = true;
    settings.endpoint = server.endpoint.clone();
    settings.extraction_rules = custom_rules();
    let mut service = RecordService::new(|| {});
    for (index, status, logs) in [
        (
            0,
            RecordStatus::Failed,
            vec![format!("UNIQUE DEVICE ID: {CUSTOM_ID}")],
        ),
        (
            1,
            RecordStatus::Cancelled,
            vec![format!("UNIQUE DEVICE ID: {CUSTOM_ID}")],
        ),
        (2, RecordStatus::Success, vec!["no ID".into()]),
        (
            3,
            RecordStatus::Success,
            vec![
                format!("UNIQUE DEVICE ID: {CUSTOM_ID}"),
                "UNIQUE DEVICE ID: FFEEDDCCBBAA99887766554433221100".into(),
            ],
        ),
    ] {
        let mut input = readonly_draft();
        input.batch_id = format!("readonly-failure-{index}");
        input.status = status;
        input.logs = logs;
        service
            .submit_with_operation(&settings, input, RecordOperation::ReadOnly)
            .unwrap();
        let result = receive_record(&mut service);
        assert!(result.error.is_some());
        assert_ne!(result.upload_state, UploadState::Uploaded);
        assert!(result.csv_path.as_ref().unwrap().is_file());
        if index < 2 {
            assert_eq!(result.custom_fields["unique_device_id"], CUSTOM_ID);
        } else {
            assert!(result.custom_fields.is_empty());
        }
    }
    server.assert_no_request();
}

#[test]
fn custom_extraction_cannot_reintroduce_redacted_header_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let mut settings = settings(directory.path());
    settings
        .custom_headers
        .insert("Authorization".into(), PRIVATE_TEST_HEADER.into());
    settings.extraction_rules = BTreeMap::from([("token".into(), r"TOKEN=(.+)".into())]);
    let mut input = draft();
    input.logs.push(format!("TOKEN={PRIVATE_TEST_HEADER}"));
    let matcher = CompiledExtractionRules::new(&settings.extraction_rules).unwrap();
    let preview = matcher.extract_record(&input.logs, &settings.custom_headers);
    assert!(preview.custom_fields.is_empty());
    assert!(preview.errors.contains_key("token"));
    let mut service = RecordService::new(|| {});
    service.submit(&settings, input).unwrap();
    let result = receive_record(&mut service);
    assert!(result.custom_fields.is_empty());
    assert!(result.custom_field_errors.contains_key("token"));
    let content = fs::read_to_string(result.csv_path.unwrap()).unwrap();
    assert!(!content.contains("telemetry-local-test-secret"));
}

#[test]
fn hardware_unique_id_is_automatically_saved_and_posted_without_a_log_regex() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![Reply::status(201)], Some(directory.path().to_owned()));
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut service = RecordService::new(|| {});
    service
        .submit_with_identity(
            &configuration,
            draft(),
            RecordOperation::Flash,
            Some(&CUSTOM_ID.to_ascii_lowercase()),
        )
        .unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.upload_state, UploadState::Uploaded);
    assert_eq!(result.custom_fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert!(result.custom_field_errors.is_empty());
    let csv = csv_fields(result.csv_path.as_ref().unwrap());
    assert_eq!(csv[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert_eq!(
        serde_json::from_str::<Value>(&csv["custom_fields"]).unwrap()[UNIQUE_DEVICE_ID_KEY],
        CUSTOM_ID
    );
    assert_eq!(csv["efuse_id"], EFUSE);
    assert_eq!(csv["mac"], MAC);
    let posted: Value = serde_json::from_slice(&server.request().body).unwrap();
    assert_eq!(posted["schema_version"], 1);
    assert_eq!(posted["custom_fields"][UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert_eq!(posted["efuse_id"], EFUSE);
    assert_eq!(posted["efuse_field"], "factory_mac");
    assert_eq!(posted["mac"], MAC);
    assert!(
        posted.get("unique_device_id").is_none(),
        "keep the existing custom field namespace"
    );
    server.assert_no_request();
}

#[test]
fn hardware_unique_id_merge_normalizes_and_preserves_conflict_provenance() {
    let matcher = CompiledExtractionRules::new(&custom_rules()).unwrap();
    for logs in [
        vec![],
        vec![format!(
            "UNIQUE DEVICE ID: {}",
            CUSTOM_ID.to_ascii_lowercase()
        )],
    ] {
        let mut fields = matcher.extract(&logs);
        apply_hardware_unique_id(&mut fields, CUSTOM_ID).unwrap();
        assert_eq!(fields.custom_fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
        assert!(
            fields.errors.is_empty(),
            "missing log captures are fulfilled by the hardware field"
        );
    }
    let conflicting = "FFEEDDCCBBAA99887766554433221100";
    let mut fields = matcher.extract(&[
        format!("UNIQUE DEVICE ID: {CUSTOM_ID}"),
        format!("UNIQUE DEVICE ID: {conflicting}"),
    ]);
    fields
        .custom_fields
        .insert("board".into(), "board-a".into());
    fields
        .errors
        .insert("missing_other".into(), "not found".into());
    apply_hardware_unique_id(&mut fields, CUSTOM_ID).unwrap();
    assert_eq!(fields.custom_fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert_eq!(fields.custom_fields["board"], "board-a");
    assert_eq!(fields.errors["missing_other"], "not found");
    assert!(fields.errors[UNIQUE_DEVICE_ID_KEY].contains("已采用硬件"));
    assert!(!fields.errors[UNIQUE_DEVICE_ID_KEY].contains(CUSTOM_ID));
    let mut same_error = matcher.extract(&[format!("UNIQUE DEVICE ID: {conflicting}")]);
    apply_hardware_unique_id(&mut same_error, CUSTOM_ID).unwrap();
    assert_eq!(
        fields.errors[UNIQUE_DEVICE_ID_KEY],
        same_error.errors[UNIQUE_DEVICE_ID_KEY]
    );
}

#[test]
fn hardware_unique_id_rejects_invalid_or_zero_but_accepts_exact_nonzero_ff_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let mut service = RecordService::new(|| {});
    for invalid in [
        "0".repeat(32),
        "A".repeat(31),
        "G".repeat(32),
        format!(" {}", "A".repeat(32)),
        EFUSE.into(),
    ] {
        assert!(
            service
                .submit_with_identity(
                    &settings(directory.path()),
                    draft(),
                    RecordOperation::Flash,
                    Some(&invalid)
                )
                .is_err()
        );
        assert_eq!(service.pending_count(), 0);
        assert_eq!(service.pending_local_count(), 0);
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    let mut fields = ExtractionOutcome::default();
    apply_hardware_unique_id(&mut fields, &"f".repeat(32)).unwrap();
    assert_eq!(fields.custom_fields[UNIQUE_DEVICE_ID_KEY], "F".repeat(32));
    service
        .submit_with_identity(
            &settings(directory.path()),
            draft(),
            RecordOperation::Flash,
            Some(&"F".repeat(32)),
        )
        .unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.custom_fields[UNIQUE_DEVICE_ID_KEY], "F".repeat(32));
}

#[test]
fn posted_hardware_id_wins_over_configured_uart_conflict_and_retry_stays_immutable() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(
        vec![Reply::status(400), Reply::status(201)],
        Some(directory.path().to_owned()),
    );
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    configuration.extraction_rules = custom_rules();
    let mut input = draft();
    input
        .logs
        .push("UNIQUE DEVICE ID: FFEEDDCCBBAA99887766554433221100".into());
    let mut service = RecordService::new(|| {});
    let id = service
        .submit_with_identity(
            &configuration,
            input.clone(),
            RecordOperation::Flash,
            Some(CUSTOM_ID),
        )
        .unwrap();
    let failed = receive_record(&mut service);
    assert_eq!(failed.upload_state, UploadState::Failed);
    let path = failed.csv_path.clone().unwrap();
    let old_csv = fs::read(&path).unwrap();
    assert_eq!(failed.custom_fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert_eq!(csv_fields(&path)[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert!(failed.custom_field_errors[UNIQUE_DEVICE_ID_KEY].contains("已采用硬件"));
    let first = server.request();
    let posted: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(posted["custom_fields"][UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert!(
        posted["custom_field_errors"][UNIQUE_DEVICE_ID_KEY]
            .as_str()
            .unwrap()
            .contains("已采用硬件")
    );
    assert_eq!(
        service
            .submit_with_identity(
                &configuration,
                input,
                RecordOperation::Flash,
                Some(CUSTOM_ID)
            )
            .unwrap(),
        id
    );
    let retry = receive_record(&mut service);
    assert_eq!(retry.upload_state, UploadState::Uploaded);
    assert_eq!(fs::read(path).unwrap(), old_csv);
    let second = server.request();
    assert_eq!(first.body, second.body);
    assert_eq!(second.headers["idempotency-key"], id);
    server.assert_no_request();
}

#[test]
fn previous_csv_format_is_reused_without_rewriting_or_accepting_a_changed_identity() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![Reply::status(201)], Some(directory.path().to_owned()));
    let mut configuration = settings(directory.path());
    let mut old_session = RecordService::new(|| {});
    old_session
        .submit_with_identity(
            &configuration,
            draft(),
            RecordOperation::Flash,
            Some(CUSTOM_ID),
        )
        .unwrap();
    let saved = receive_record(&mut old_session);
    let path = saved.csv_path.unwrap();
    let mut rows = csv_rows(&path);
    assert_eq!(rows[0].len(), 23);
    rows[0].truncate(20);
    rows[1].truncate(20);
    assert_eq!(rows[0].pop().as_deref(), Some(UNIQUE_DEVICE_ID_KEY));
    assert_eq!(rows[1].pop().as_deref(), Some(CUSTOM_ID));
    // Model the exact pre-column release format independently of the writer.
    let mut previous = vec![0xef, 0xbb, 0xbf];
    for row in rows {
        let line = row
            .iter()
            .map(|cell| format!("\"{}\"", cell.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",");
        previous.extend_from_slice(line.as_bytes());
        previous.extend_from_slice(b"\r\n");
    }
    fs::write(&path, &previous).unwrap();
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut retry_session = RecordService::new(|| {});
    assert_eq!(
        retry_session
            .submit_with_identity(
                &configuration,
                draft(),
                RecordOperation::Flash,
                Some(CUSTOM_ID)
            )
            .unwrap(),
        saved.record_id
    );
    let retry = receive_record(&mut retry_session);
    assert_eq!(retry.upload_state, UploadState::Uploaded);
    assert_eq!(retry.csv_path.as_ref(), Some(&path));
    assert_eq!(
        fs::read(&path).unwrap(),
        previous,
        "historical files stay immutable"
    );
    assert!(!csv_fields(&path).contains_key(UNIQUE_DEVICE_ID_KEY));
    let posted: Value = serde_json::from_slice(&server.request().body).unwrap();
    assert_eq!(posted["custom_fields"][UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    let mut changed_session = RecordService::new(|| {});
    changed_session
        .submit_with_identity(
            &configuration,
            draft(),
            RecordOperation::Flash,
            Some("FFEEDDCCBBAA99887766554433221100"),
        )
        .unwrap();
    let rejected = receive_record(&mut changed_session);
    assert!(rejected.csv_path.is_none());
    assert!(rejected.error.unwrap().contains("已拒绝覆盖和上传"));
    assert_eq!(fs::read(&path).unwrap(), previous);
    server.assert_no_request();
}

#[test]
fn absent_hardware_identity_preserves_user_uart_rules_without_fabricating_factory_data() {
    let directory = tempfile::tempdir().unwrap();
    let mut configuration = settings(directory.path());
    configuration.extraction_rules = custom_rules();
    let mut input = readonly_draft();
    input.logs = vec![format!("UNIQUE DEVICE ID: {}", "0".repeat(32))];
    let mut service = RecordService::new(|| {});
    service
        .submit_with_identity(&configuration, input, RecordOperation::ReadOnly, None)
        .unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.custom_fields[UNIQUE_DEVICE_ID_KEY], "0".repeat(32));
    let fields = csv_fields(result.csv_path.as_ref().unwrap());
    assert!(fields["efuse_id"].is_empty() && fields["mac"].is_empty());
    assert_eq!(fields[UNIQUE_DEVICE_ID_KEY], "0".repeat(32));
    let input = draft();
    configuration.extraction_rules.clear();
    service
        .submit_with_identity(&configuration, input, RecordOperation::Flash, None)
        .unwrap();
    let result = receive_record(&mut service);
    assert!(!result.custom_fields.contains_key(UNIQUE_DEVICE_ID_KEY));
    assert_eq!(
        csv_fields(result.csv_path.as_ref().unwrap())[UNIQUE_DEVICE_ID_KEY],
        ""
    );
}

#[test]
fn production_snapshot_uses_the_frozen_destination_and_keeps_http_schema_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let mut production = ProductionBatch::parse("synthetic-order", "2647", "2026-11-20").unwrap();
    let destination = production.record_directory(root.path(), RecordOperation::Flash);
    let server = LocalServer::new(vec![Reply::status(201)], Some(destination.clone()));
    let mut configuration = settings(&destination);
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut input = draft();
    input.started_at = "2026-11-20T15:01:02+08:00".into();
    input.finished_at = "2026-11-20T15:01:03+08:00".into();
    let mut service = RecordService::new(|| {});
    service
        .submit_with_production(
            &configuration,
            input,
            RecordOperation::Flash,
            Some(CUSTOM_ID),
            Some(&production),
        )
        .unwrap();
    production.work_order = "later-order".into();
    production.board_week = "2648".into();
    configuration.csv_directory = Some(root.path().join("later-path"));
    let result = receive_record(&mut service);
    assert_eq!(result.upload_state, UploadState::Uploaded);
    let path = result.csv_path.as_ref().unwrap();
    assert_eq!(path.parent(), Some(destination.as_path()));
    let fields = csv_fields(path);
    assert_eq!(fields["生产批次/工单号"], "synthetic-order");
    assert_eq!(fields["主板年周"], "2647");
    assert_eq!(fields["主板生产日期"], "2026-11-20");
    assert_eq!(fields[UNIQUE_DEVICE_ID_KEY], CUSTOM_ID);
    assert_eq!(fields["started_at"], "2026-11-20T15:01:02+08:00");
    assert!(!root.path().join("later-path").exists());
    let posted: Value = serde_json::from_slice(&server.request().body).unwrap();
    assert_eq!(posted["started_at"], fields["started_at"]);
    assert_eq!(posted["finished_at"], fields["finished_at"]);
    for key in [
        "production",
        "work_order",
        "board_week",
        "board_date",
        "生产批次/工单号",
        "主板年周",
        "主板生产日期",
    ] {
        assert!(
            posted.get(key).is_none(),
            "production metadata must remain CSV-only"
        );
    }
    server.assert_no_request();
}

#[test]
fn readonly_records_append_the_same_production_columns_in_the_resolved_verification_directory() {
    let root = tempfile::tempdir().unwrap();
    let production = ProductionBatch::parse("read-order", "2647", "2026-11-20").unwrap();
    let destination = production.record_directory(root.path(), RecordOperation::ReadOnly);
    let mut configuration = settings(&destination);
    configuration.extraction_rules = custom_rules();
    let mut service = RecordService::new(|| {});
    service
        .submit_with_production(
            &configuration,
            readonly_draft(),
            RecordOperation::ReadOnly,
            None,
            Some(&production),
        )
        .unwrap();
    let saved = receive_record(&mut service);
    assert_eq!(
        saved.csv_path.as_ref().unwrap().parent(),
        Some(destination.as_path())
    );
    let fields = csv_fields(saved.csv_path.as_ref().unwrap());
    assert_eq!(fields["operation"], "read_only");
    assert_eq!(fields["生产批次/工单号"], "read-order");
    assert_eq!(fields["主板年周"], "2647");
    assert_eq!(fields["主板生产日期"], "2026-11-20");
    let header = &csv_rows(saved.csv_path.as_ref().unwrap())[0];
    assert_eq!(header.len(), 23);
    assert_eq!(header[19], UNIQUE_DEVICE_ID_KEY);
    assert_eq!(
        &header[20..],
        &["生产批次/工单号", "主板年周", "主板生产日期"]
    );
}

#[test]
fn production_retry_preserves_files_and_distinguishes_formula_safe_work_order_values() {
    let root = tempfile::tempdir().unwrap();
    let production = ProductionBatch::parse("=ORDER", "2647", "2026-11-20").unwrap();
    let configuration = settings(root.path());
    let mut initial = RecordService::new(|| {});
    let id = initial
        .submit_with_production(
            &configuration,
            draft(),
            RecordOperation::Flash,
            None,
            Some(&production),
        )
        .unwrap();
    let saved = receive_record(&mut initial);
    let path = saved.csv_path.unwrap();
    let original = fs::read(&path).unwrap();
    assert_eq!(csv_fields(&path)["生产批次/工单号"], "'=ORDER");
    let mut retry = RecordService::new(|| {});
    assert_eq!(
        retry
            .submit_with_production(
                &configuration,
                draft(),
                RecordOperation::Flash,
                None,
                Some(&production)
            )
            .unwrap(),
        id
    );
    assert_eq!(receive_record(&mut retry).csv_path.as_ref(), Some(&path));
    assert_eq!(fs::read(&path).unwrap(), original);
    let changed = ProductionBatch::parse("'=ORDER", "2647", "2026-11-20").unwrap();
    let mut changed_session = RecordService::new(|| {});
    changed_session
        .submit_with_production(
            &configuration,
            draft(),
            RecordOperation::Flash,
            None,
            Some(&changed),
        )
        .unwrap();
    let rejected = receive_record(&mut changed_session);
    assert!(rejected.csv_path.is_none());
    assert!(rejected.error.unwrap().contains("已拒绝覆盖和上传"));
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn legacy_twenty_column_records_are_reused_only_without_production_metadata() {
    let root = tempfile::tempdir().unwrap();
    let configuration = settings(root.path());
    let mut initial = RecordService::new(|| {});
    initial.submit(&configuration, draft()).unwrap();
    let path = receive_record(&mut initial).csv_path.unwrap();
    let mut previous = vec![0xef, 0xbb, 0xbf];
    for row in csv_rows(&path) {
        let line = row
            .iter()
            .take(20)
            .map(|cell| format!("\"{}\"", cell.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",");
        previous.extend_from_slice(line.as_bytes());
        previous.extend_from_slice(b"\r\n");
    }
    fs::write(&path, &previous).unwrap();
    let mut legacy_retry = RecordService::new(|| {});
    legacy_retry.submit(&configuration, draft()).unwrap();
    assert_eq!(
        receive_record(&mut legacy_retry).csv_path.as_ref(),
        Some(&path)
    );
    assert_eq!(fs::read(&path).unwrap(), previous);
    let production = ProductionBatch::parse("new-order", "2647", "2026-11-20").unwrap();
    let mut enriched_retry = RecordService::new(|| {});
    enriched_retry
        .submit_with_production(
            &configuration,
            draft(),
            RecordOperation::Flash,
            None,
            Some(&production),
        )
        .unwrap();
    let rejected = receive_record(&mut enriched_retry);
    assert!(
        rejected.csv_path.is_none(),
        "an old file cannot silently omit a new production snapshot"
    );
    assert!(rejected.error.unwrap().contains("已拒绝覆盖和上传"));
    assert_eq!(fs::read(&path).unwrap(), previous);
}

fn readonly_draft() -> RecordDraft {
    RecordDraft {
        chip: String::new(),
        identity: None,
        firmware_sha256: String::new(),
        logs: vec![format!("UNIQUE DEVICE ID: {CUSTOM_ID}")],
        ..draft()
    }
}

fn draft() -> RecordDraft {
    RecordDraft {
        batch_id: "batch-test-20261119".into(),
        port: "COM3".into(),
        chip: "esp32c3".into(),
        identity: Some(DeviceIdentity {
            efuse_id: Some(EFUSE.into()),
            mac: Some(MAC.into()),
        }),
        status: RecordStatus::Success,
        started_at: "2026-11-19T03:00:00Z".into(),
        finished_at: "2026-11-19T03:00:01Z".into(),
        duration_ms: 1000,
        firmware_sha256: "a".repeat(64),
        logs: vec![
            "Connecting to ESP32-C3".into(),
            format!("EFUSE_ID={EFUSE} MAC={MAC}"),
            "Flash verified".into(),
        ],
    }
}

fn settings(directory: &Path) -> TelemetrySettings {
    TelemetrySettings {
        csv_directory: Some(directory.to_owned()),
        ..TelemetrySettings::default()
    }
}

fn receive_record(service: &mut RecordService) -> RecordResult {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(result) = service.poll() {
            return result;
        }
        assert!(Instant::now() < deadline, "record worker did not complete");
        thread::sleep(Duration::from_millis(10));
    }
}

/// CSV reader used to verify quoting/newlines independently of the writer.
fn csv_rows(path: &Path) -> Vec<Vec<String>> {
    let bytes = fs::read(path).unwrap();
    assert!(
        bytes.starts_with(&[0xef, 0xbb, 0xbf]),
        "CSV must use a UTF-8 BOM"
    );
    let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut cell)),
            '\r' if !quoted && chars.peek() == Some(&'\n') => {}
            '\n' if !quoted => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            value => cell.push(value),
        }
    }
    assert!(!quoted, "CSV contains an unclosed quote");
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

fn csv_fields(path: &Path) -> BTreeMap<String, String> {
    let rows = csv_rows(path);
    assert_eq!(
        rows.len(),
        2,
        "one immutable CSV record must contain one header and one row"
    );
    assert_eq!(rows[0].len(), rows[1].len());
    rows[0]
        .iter()
        .cloned()
        .zip(rows[1].iter().cloned())
        .collect()
}

#[derive(Debug)]
struct CapturedRequest {
    request_line: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    csv_saved_before_post: bool,
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    redirect: bool,
    delay: Duration,
}

impl Reply {
    fn status(status: u16) -> Self {
        Self {
            status,
            body: b"{}".to_vec(),
            redirect: false,
            delay: Duration::ZERO,
        }
    }
}

struct LocalServer {
    endpoint: String,
    requests: Receiver<CapturedRequest>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LocalServer {
    fn new(replies: Vec<Reply>, csv_directory: Option<PathBuf>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/records", listener.local_addr().unwrap());
        let redirect = format!("http://{}/must-not-follow", listener.local_addr().unwrap());
        let (sender, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(12);
            let mut replies = replies.into_iter();
            while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let Some(mut request) = read_request(&mut socket) else {
                            continue;
                        };
                        request.csv_saved_before_post =
                            csv_directory.as_ref().is_some_and(|directory| {
                                fs::read_dir(directory).ok().is_some_and(|entries| {
                                    entries.flatten().any(|entry| {
                                        entry
                                            .path()
                                            .extension()
                                            .is_some_and(|extension| extension == "csv")
                                            && fs::read(entry.path()).is_ok_and(|bytes| {
                                                bytes.starts_with(&[0xef, 0xbb, 0xbf])
                                            })
                                    })
                                })
                            });
                        if sender.send(request).is_err() {
                            break;
                        }
                        let reply = replies.next().unwrap_or_else(|| Reply::status(500));
                        if !reply.delay.is_zero() {
                            thread::sleep(reply.delay);
                        }
                        let location = if reply.redirect {
                            format!("Location: {redirect}\r\n")
                        } else {
                            String::new()
                        };
                        let header = format!(
                            "HTTP/1.1 {} Test\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n",
                            reply.status,
                            reply.body.len()
                        );
                        if socket.write_all(header.as_bytes()).is_ok() {
                            let _ = socket.write_all(&reply.body);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn request(&self) -> CapturedRequest {
        self.requests
            .recv_timeout(Duration::from_secs(3))
            .expect("localhost POST was not received")
    }

    fn assert_no_request(&self) {
        assert!(
            self.requests
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "unexpected additional POST or redirect request"
        );
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Option<CapturedRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0; 1024];
        let length = socket.read(&mut buffer).ok()?;
        if length == 0 || bytes.len() + length > 1024 * 1024 + 65536 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..length]);
    };
    assert!(
        !bytes[..header_end]
            .windows(3)
            .any(|window| window == b"\r\r\n"),
        "request sent an extra carriage return in a header delimiter"
    );
    let header = String::from_utf8(bytes[..header_end].to_vec()).ok()?;
    let mut lines = header.lines();
    let request_line = lines.next()?.to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let expected: usize = headers.get("content-length")?.parse().ok()?;
    if expected > 1024 * 1024 {
        return None;
    }
    while bytes.len() - header_end < expected {
        let mut buffer = [0; 4096];
        let length = socket.read(&mut buffer).ok()?;
        if length == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..length]);
    }
    Some(CapturedRequest {
        request_line,
        headers,
        body: bytes[header_end..header_end + expected].to_vec(),
        csv_saved_before_post: false,
    })
}

#[test]
fn settings_are_off_by_default_and_atomic_updates_do_not_touch_user_preferences() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings/telemetry.json");
    let initial = load_settings_from(&path).unwrap();
    assert!(!initial.upload_enabled);
    assert!(initial.endpoint.is_empty());
    assert_eq!(initial.timeout_seconds, 10);
    assert!(initial.custom_headers.is_empty());
    validate_settings(&initial).unwrap();
    let mut saved = settings(directory.path());
    saved
        .custom_headers
        .insert("Authorization".into(), PRIVATE_TEST_HEADER.into());
    save_settings_at(&path, &saved).unwrap();
    assert_eq!(
        serde_json::to_value(load_settings_from(&path).unwrap()).unwrap(),
        serde_json::to_value(&saved).unwrap()
    );
    saved.timeout_seconds = 5;
    save_settings_at(&path, &saved).unwrap();
    assert_eq!(load_settings_from(&path).unwrap().timeout_seconds, 5);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    }
    let previous = fs::read(&path).unwrap();
    saved
        .custom_headers
        .insert("Host".into(), "untrusted.example".into());
    assert!(save_settings_at(&path, &saved).is_err());
    assert_eq!(fs::read(&path).unwrap(), previous);
    fs::write(&path, "broken settings").unwrap();
    assert!(load_settings_from(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"broken settings");
    fs::write(&path, vec![b' '; 65537]).unwrap();
    assert!(load_settings_from(&path).is_err());
}

#[test]
fn invalid_endpoint_headers_regex_and_timeouts_are_rejected_without_posting() {
    for endpoint in [
        "ftp://localhost/records",
        "https://user:password@example.com/records",
        "https://example.com/records#fragment",
        "not a URL",
    ] {
        assert!(validate_endpoint(endpoint).is_err());
    }
    for endpoint in [
        "http://127.0.0.1:9876/records",
        "http://intranet.example/records",
        "https://example.com/api?token=test",
    ] {
        validate_endpoint(endpoint).unwrap();
    }
    for name in [
        "Host",
        "Content-Length",
        "Content-Type",
        "Idempotency-Key",
        "Connection",
        "Transfer-Encoding",
        "Proxy-Authorization",
    ] {
        assert!(
            validate_custom_headers(&BTreeMap::from([(name.into(), "forbidden".into())])).is_err()
        );
    }
    for (name, value) in [
        ("Authorization", "token\r\nInjected: true"),
        ("bad header", "value"),
        ("X-Token", "bad\0value"),
    ] {
        assert!(validate_custom_headers(&BTreeMap::from([(name.into(), value.into())])).is_err());
    }
    let directory = tempfile::tempdir().unwrap();
    let mut value = settings(directory.path());
    value.upload_enabled = true;
    assert!(validate_settings(&value).is_err());
    value.endpoint = "http://127.0.0.1:9876/records".into();
    for timeout in [0, 4, 31, u64::MAX] {
        value.timeout_seconds = timeout;
        assert!(validate_settings(&value).is_err());
    }
    value.timeout_seconds = 10;
    for pattern in ["[", "EFUSE_ID=[0-9A-F]+"] {
        value.efuse_pattern = pattern.into();
        assert!(validate_settings(&value).is_err());
    }
}

#[test]
fn disabled_upload_still_writes_formula_safe_unicode_csv_and_rejects_duplicate_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let mut service = RecordService::new(|| {});
    let mut input = draft();
    input.batch_id = "=batch-formula".into();
    input.port = "+COM3".into();
    input.chip = "@ESP32-C3".into();
    input.logs.push("中文, \"引号\"\n第二行".into());
    let record_id = service
        .submit(&settings(directory.path()), input.clone())
        .unwrap();
    assert!(service.submit(&settings(directory.path()), input).is_err());
    let result = receive_record(&mut service);
    assert_eq!(result.record_id, record_id);
    assert_eq!(result.upload_state, UploadState::Disabled);
    assert_eq!(service.pending_count(), 0);
    let fields = csv_fields(result.csv_path.as_ref().unwrap());
    assert_eq!(fields.get("efuse_id").map(String::as_str), Some(EFUSE));
    assert_eq!(fields.get("mac").map(String::as_str), Some(MAC));
    for name in ["batch_id", "port", "chip"] {
        assert!(
            fields[name].starts_with('\''),
            "spreadsheet formula input must be escaped"
        );
    }
    assert!(fields["logs"].contains("中文") && fields["logs"].contains("引号"));
}

#[test]
fn post_contains_bound_device_record_and_csv_is_durable_before_network_activity() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![Reply::status(201)], Some(directory.path().to_owned()));
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    configuration
        .custom_headers
        .insert("Authorization".into(), PRIVATE_TEST_HEADER.into());
    configuration
        .custom_headers
        .insert("X-Workstation".into(), "line-1".into());
    let mut service = RecordService::new(|| {});
    let record_id = service.submit(&configuration, draft()).unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.upload_state, UploadState::Uploaded);
    assert!(result.error.is_none());
    let request = server.request();
    assert!(request.request_line.starts_with("POST /records HTTP/"));
    assert!(
        request.csv_saved_before_post,
        "CSV must exist before POST starts"
    );
    assert!(request.headers["content-type"].starts_with("application/json"));
    assert_eq!(request.headers["authorization"], PRIVATE_TEST_HEADER);
    assert_eq!(request.headers["x-workstation"], "line-1");
    assert_eq!(request.headers["idempotency-key"], record_id);
    let record: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["record_id"], record_id);
    assert_eq!(record["batch_id"], "batch-test-20261119");
    assert_eq!(record["port"], "COM3");
    assert_eq!(record["chip"], "esp32c3");
    assert_eq!(record["efuse_id"], EFUSE);
    assert_eq!(record["efuse_field"], "factory_mac");
    assert_eq!(record["mac"], MAC);
    assert_eq!(record["status"], "success");
    assert_eq!(record["duration_ms"], 1000);
    assert_eq!(record["firmware_sha256"], "a".repeat(64));
    assert!(record["logs"].is_array());
    assert!(
        !String::from_utf8(request.body)
            .unwrap()
            .contains(PRIVATE_TEST_HEADER)
    );
    assert!(
        !fs::read_to_string(result.csv_path.unwrap())
            .unwrap()
            .contains(PRIVATE_TEST_HEADER)
    );
    server.assert_no_request();
}

#[test]
fn missing_conflicting_or_wrong_device_ids_never_upload_but_leave_csv_records() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(vec![], Some(directory.path().to_owned()));
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut inputs = Vec::new();
    let mut missing = draft();
    missing.identity = None;
    inputs.push(missing);
    let mut mismatch = draft();
    mismatch.logs = vec!["EFUSE_ID=102030405060 MAC=10:20:30:40:50:60".into()];
    inputs.push(mismatch);
    let mut conflicting = draft();
    conflicting
        .logs
        .push("EFUSE_ID=102030405060 MAC=10:20:30:40:50:60".into());
    inputs.push(conflicting);
    let mut missing_log_id = draft();
    missing_log_id.logs = vec!["connection failed before identity".into()];
    missing_log_id.status = RecordStatus::Failed;
    inputs.push(missing_log_id);
    for (index, mut input) in inputs.into_iter().enumerate() {
        input.batch_id = format!("identity-case-{index}");
        let mut service = RecordService::new(|| {});
        service.submit(&configuration, input).unwrap();
        let result = receive_record(&mut service);
        assert_eq!(result.upload_state, UploadState::SkippedIdentity);
        assert!(result.error.is_some());
        let fields = csv_fields(result.csv_path.as_ref().unwrap());
        assert!(fields["efuse_id"].is_empty());
        assert!(fields["mac"].is_empty());
    }
    server.assert_no_request();
}

#[test]
fn transient_upload_retries_use_the_same_record_and_idempotency_key() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(
        vec![Reply::status(503), Reply::status(200)],
        Some(directory.path().to_owned()),
    );
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut service = RecordService::new(|| {});
    let record_id = service.submit(&configuration, draft()).unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.upload_state, UploadState::Uploaded);
    let first = server.request();
    let second = server.request();
    assert_eq!(first.headers["idempotency-key"], record_id);
    assert_eq!(second.headers["idempotency-key"], record_id);
    assert_eq!(first.body, second.body);
    assert!(first.csv_saved_before_post && second.csv_saved_before_post);
    server.assert_no_request();
}

#[test]
fn permanent_failure_and_redirect_do_not_retry_or_leak_headers_into_diagnostics() {
    for redirect in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let reply = Reply {
            status: if redirect { 302 } else { 400 },
            body: PRIVATE_TEST_HEADER.as_bytes().to_vec(),
            redirect,
            delay: Duration::ZERO,
        };
        let server = LocalServer::new(
            vec![reply, Reply::status(200)],
            Some(directory.path().to_owned()),
        );
        let mut configuration = settings(directory.path());
        configuration.upload_enabled = true;
        configuration.endpoint = format!("{}?token=private-query-value", server.endpoint);
        configuration
            .custom_headers
            .insert("Authorization".into(), PRIVATE_TEST_HEADER.into());
        let mut service = RecordService::new(|| {});
        service.submit(&configuration, draft()).unwrap();
        let result = receive_record(&mut service);
        assert_eq!(result.upload_state, UploadState::Failed);
        let error = result.error.unwrap();
        assert!(!error.contains(PRIVATE_TEST_HEADER));
        assert!(!error.contains("private-query-value"));
        assert!(result.csv_path.is_some());
        server.request();
        server.assert_no_request();
    }
}

#[test]
fn local_write_failure_is_visible_and_prevents_even_an_enabled_upload() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("not-a-directory");
    fs::write(&blocked, "preserve this file").unwrap();
    let server = LocalServer::new(vec![], None);
    let mut configuration = settings(&blocked);
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut service = RecordService::new(|| {});
    service.submit(&configuration, draft()).unwrap();
    let result = receive_record(&mut service);
    assert!(result.csv_path.is_none());
    assert!(result.error.is_some());
    assert_eq!(result.upload_state, UploadState::Failed);
    assert_eq!(fs::read_to_string(blocked).unwrap(), "preserve this file");
    server.assert_no_request();
}

#[test]
fn http_response_size_and_timeout_are_bounded_without_external_requests() {
    let oversized = LocalServer::new(
        vec![Reply {
            body: vec![b'x'; 65537],
            ..Reply::status(200)
        }],
        None,
    );
    let error = post_json(
        &oversized.endpoint,
        5,
        &BTreeMap::new(),
        "record-limit-test",
        b"{}",
    )
    .unwrap_err();
    assert!(!error.retryable);
    oversized.request();
    let slow = LocalServer::new(
        vec![Reply {
            delay: Duration::from_secs(6),
            ..Reply::status(200)
        }],
        None,
    );
    let started = Instant::now();
    let error = post_json(
        &slow.endpoint,
        5,
        &BTreeMap::new(),
        "record-timeout-test",
        b"{}",
    )
    .unwrap_err();
    assert!(error.retryable);
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "transport ignored its configured timeout"
    );
    slow.request();
}

#[test]
fn transient_failures_stop_after_three_attempts_and_keep_one_local_record() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(
        vec![
            Reply::status(503),
            Reply::status(503),
            Reply::status(503),
            Reply::status(200),
        ],
        Some(directory.path().to_owned()),
    );
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut service = RecordService::new(|| {});
    let id = service.submit(&configuration, draft()).unwrap();
    let result = receive_record(&mut service);
    assert_eq!(result.upload_state, UploadState::Failed);
    assert!(result.csv_path.is_some());
    for _ in 0..3 {
        assert_eq!(server.request().headers["idempotency-key"], id);
    }
    server.assert_no_request();
    assert_eq!(
        fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "csv"))
            .count(),
        1
    );
}

#[test]
fn bounded_service_queue_rejects_excess_without_losing_accepted_local_records() {
    let directory = tempfile::tempdir().unwrap();
    let configuration = settings(directory.path());
    let mut service = RecordService::new(|| {});
    let mut accepted = Vec::new();
    let mut refused = false;
    for index in 0..256 {
        let mut input = draft();
        input.batch_id = format!("queue-case-{index}");
        match service.submit(&configuration, input) {
            Ok(id) => accepted.push(id),
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    assert!(
        refused,
        "the queue and unconsumed results must remain bounded"
    );
    assert!(!accepted.is_empty() && accepted.len() <= 65);
    for _ in 0..accepted.len() {
        let result = receive_record(&mut service);
        assert!(accepted.contains(&result.record_id));
        assert_eq!(result.upload_state, UploadState::Disabled);
        assert!(result.csv_path.is_some());
    }
    assert_eq!(service.pending_count(), 0);
    let mut next = draft();
    next.batch_id = "after-queue-drained".into();
    service.submit(&configuration, next).unwrap();
    assert_eq!(
        receive_record(&mut service).upload_state,
        UploadState::Disabled
    );
}

#[test]
fn delayed_upload_does_not_block_durable_csv_for_the_same_board_in_a_later_batch() {
    let directory = tempfile::tempdir().unwrap();
    let server = LocalServer::new(
        vec![Reply {
            delay: Duration::from_secs(2),
            ..Reply::status(200)
        }],
        Some(directory.path().to_owned()),
    );
    let mut configuration = settings(directory.path());
    configuration.upload_enabled = true;
    configuration.endpoint = server.endpoint.clone();
    let mut service = RecordService::new(|| {});
    let mut first = draft();
    first.batch_id = "two-phase-online-batch".into();
    let first_id = service.submit(&configuration, first).unwrap();
    // The server has read the first POST, but intentionally has not answered it.
    let request = server.request();
    assert!(request.csv_saved_before_post);
    assert_eq!(request.headers["idempotency-key"], first_id);
    let mut later = draft();
    later.batch_id = "two-phase-offline-later-batch".into();
    configuration.upload_enabled = false;
    let started = Instant::now();
    let later_id = service.submit(&configuration, later).unwrap();
    assert_ne!(
        later_id, first_id,
        "the same physical board is permitted in another batch"
    );
    let mut later_result = None;
    loop {
        if let Some(result) = service.poll() {
            assert_eq!(
                result.record_id, later_id,
                "the disabled local record must finish before the delayed HTTP response"
            );
            later_result = Some(result);
        }
        if later_result.is_some() && service.pending_local_count() == 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "local CSV persistence waited behind a network upload"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let later_result = later_result.unwrap();
    assert_eq!(later_result.batch_id, "two-phase-offline-later-batch");
    assert_eq!(later_result.upload_state, UploadState::Disabled);
    assert!(later_result.error.is_none());
    let later_path = later_result.csv_path.as_ref().unwrap();
    assert!(later_path.is_file());
    assert_eq!(csv_fields(later_path)["efuse_id"], EFUSE);
    assert_eq!(csv_fields(later_path)["mac"], MAC);
    assert_eq!(
        service.pending_count(),
        1,
        "only the delayed first upload should remain pending"
    );
    let first_result = receive_record(&mut service);
    assert_eq!(first_result.record_id, first_id);
    assert_eq!(first_result.batch_id, "two-phase-online-batch");
    assert_eq!(first_result.upload_state, UploadState::Uploaded);
    assert_ne!(
        first_result.csv_path.as_ref(),
        later_result.csv_path.as_ref()
    );
    assert_eq!(service.pending_local_count(), 0);
    assert_eq!(service.pending_count(), 0);
    server.assert_no_request();
}
