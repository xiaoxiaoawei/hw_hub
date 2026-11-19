use esp32c3_flashtool::{
    backend::DeviceIdentity,
    telemetry::{
        RecordDraft, RecordResult, RecordService, RecordStatus, TelemetrySettings, UploadState,
        load_settings_from, save_settings_at, validate_settings,
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
