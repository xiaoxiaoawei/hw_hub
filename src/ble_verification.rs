//! Strict S3 verifier serial protocol and local, atomic BLE verification history.
//! The configured MAC is an over-the-air BLE address, not a chip factory MAC.
//! The separately entered device ID never becomes part of a BLE wire command.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::production::{PRODUCTION_HEADERS, ProductionBatch};

pub const BAUD_RATE: u32 = 115_200;
pub const EVENT_PREFIX: &str = "BLE_VERIFY ";
pub const PASS_MARKER: &str = "PASS！";
pub const EXPECTED_FIRMWARE: &str = "1.188";
pub const NAME_PREFIX: &str = "Lumysi-";
const MAX_PROTOCOL_LINE: usize = 4096;
const MAX_LOG_BYTES: usize = 512 * 1024;
const HEADER: [&str; 17] = [
    "run_id",
    "attempt",
    "time",
    "configured_mac",
    "observed_mac",
    "unique_device_id",
    "result",
    "name",
    "firmware",
    "fault_bits",
    "battery_mv",
    "COM",
    "details",
    "attempt_log",
    PRODUCTION_HEADERS[0],
    PRODUCTION_HEADERS[1],
    PRODUCTION_HEADERS[2],
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub mac: String,
    pub unique_device_id: String,
}

impl Target {
    pub fn new(mac: &str, unique_device_id: &str) -> Result<Self> {
        let mac = normalized_mac(mac.trim())?;
        let id = unique_device_id.trim();
        ensure!(
            id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "设备 ID 必须包含 32 个十六进制字符"
        );
        ensure!(!id.bytes().all(|byte| byte == b'0'), "设备 ID 不能为全 0");
        Ok(Self {
            mac,
            unique_device_id: id.to_ascii_uppercase(),
        })
    }

    pub fn command(&self, session: &str) -> Result<String> {
        target_command(session, self)
    }
}

/// Only the nonce and configured over-the-air address are transmitted.
pub fn target_command(session: &str, target: &Target) -> Result<String> {
    valid_session(session)?;
    let target = Target::new(&target.mac, &target.unique_device_id)?;
    Ok(format!("BLE_TARGET {session} {}\n", target.mac))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "PASS！")]
    Pass,
    #[serde(rename = "FAIL")]
    Fail,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => PASS_MARKER,
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyEvent {
    pub session: String,
    pub mac: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartEvent {
    pub session: String,
    pub attempt: u32,
    pub mac: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvent {
    pub session: String,
    pub attempt: u32,
    /// The configured target address, not inferred from a device name.
    pub mac: String,
    pub status: Status,
    #[serde(default)]
    pub observed_mac: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub fault_bits: Option<u32>,
    #[serde(default)]
    pub battery_mv: Option<u16>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ResultEvent {
    pub fn passed(&self) -> bool {
        self.status == Status::Pass && self.validate().is_ok()
    }

    pub fn validate(&self) -> Result<()> {
        valid_session(&self.session)?;
        wire_mac(&self.mac)?;
        ensure!(self.attempt >= 1, "蓝牙验证 attempt 必须大于 0");
        if let Some(mac) = &self.observed_mac {
            wire_mac(mac)?;
        }
        bounded_optional(&self.name, 128, "广播名称")?;
        bounded_optional(&self.firmware, 32, "固件版本")?;
        bounded_optional(&self.reason, 1024, "失败原因")?;
        if self.status == Status::Pass {
            ensure!(
                self.observed_mac.as_deref() == Some(self.mac.as_str()),
                "PASS 缺少一致的实际 BLE 地址"
            );
            let name = self.name.as_deref().context("PASS 缺少广播名称")?;
            let suffix = name
                .strip_prefix(NAME_PREFIX)
                .context("PASS 广播名称前缀不匹配")?;
            ensure!(
                !suffix.trim().is_empty() && name.bytes().all(|byte| (0x20..=0x7e).contains(&byte)),
                "PASS 广播名称必须有非空的可打印 ASCII 后缀"
            );
            ensure!(
                self.firmware.as_deref() == Some(EXPECTED_FIRMWARE),
                "PASS 固件版本不匹配"
            );
            ensure!(self.fault_bits == Some(0), "PASS 缺少零故障位");
            ensure!(self.battery_mv.is_some(), "PASS 缺少完整载荷中的电池毫伏值");
            ensure!(
                self.reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty()),
                "PASS 同时带有失败原因"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Ready(ReadyEvent),
    Start(StartEvent),
    Result(ResultEvent),
}

impl Event {
    pub fn session(&self) -> &str {
        match self {
            Self::Ready(event) => &event.session,
            Self::Start(event) => &event.session,
            Self::Result(event) => &event.session,
        }
    }
    pub fn mac(&self) -> &str {
        match self {
            Self::Ready(event) => &event.mac,
            Self::Start(event) => &event.mac,
            Self::Result(event) => &event.mac,
        }
    }
    pub fn attempt(&self) -> Option<u32> {
        match self {
            Self::Ready(_) => None,
            Self::Start(event) => Some(event.attempt),
            Self::Result(event) => Some(event.attempt),
        }
    }
    fn validate(&self) -> Result<()> {
        valid_session(self.session())?;
        wire_mac(self.mac())?;
        match self {
            Self::Start(event) => ensure!(event.attempt >= 1, "蓝牙验证 attempt 必须大于 0"),
            Self::Result(event) => event.validate()?,
            Self::Ready(_) => {}
        }
        Ok(())
    }
}

/// Non-protocol console lines are ignored. A malformed protocol line is an
/// error, never a successful or partially accepted verification result.
pub fn parse_line(line: &str) -> Result<Option<Event>> {
    let Some(json) = line.strip_prefix(EVENT_PREFIX) else {
        return Ok(None);
    };
    ensure!(
        line.len() <= MAX_PROTOCOL_LINE,
        "蓝牙验证协议行超过 4096 字节"
    );
    let event: Event = serde_json::from_str(json.trim_end_matches(['\r', '\n']))
        .map_err(|_| anyhow::anyhow!("蓝牙验证 JSON 格式、事件字段或结果标记无效"))?;
    event.validate()?;
    Ok(Some(event))
}

/// The UI/worker still sequences ready -> start -> result and rejects replayed
/// attempts; this helper enforces the frozen session and target address.
pub fn parse_line_for_target(line: &str, session: &str, target: &Target) -> Result<Option<Event>> {
    valid_session(session)?;
    let target = Target::new(&target.mac, &target.unique_device_id)?;
    let event = parse_line(line)?;
    if let Some(event) = &event {
        ensure!(event.session() == session, "蓝牙验证事件不属于当前会话");
        ensure!(
            event.mac() == target.mac,
            "蓝牙验证事件不属于已配置目标地址"
        );
    }
    Ok(event)
}

fn valid_session(session: &str) -> Result<()> {
    ensure!(
        session.len() == 64
            && session
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "蓝牙验证 session 必须为 64 位小写十六进制值"
    );
    Ok(())
}

fn normalized_mac(value: &str) -> Result<String> {
    let compact = if value.contains(':') || value.contains('-') {
        let separator = if value.contains(':') { ':' } else { '-' };
        let parts: Vec<_> = value.split(separator).collect();
        ensure!(
            parts.len() == 6 && parts.iter().all(|part| part.len() == 2),
            "BLE 地址格式无效"
        );
        parts.concat()
    } else {
        value.to_owned()
    };
    ensure!(
        compact.len() == 12 && compact.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "BLE 地址必须包含 12 个十六进制字符"
    );
    ensure!(
        !compact.bytes().all(|byte| byte == b'0')
            && !compact.bytes().all(|byte| byte.eq_ignore_ascii_case(&b'f')),
        "BLE 地址不能为全 0 或全 FF"
    );
    Ok(compact.to_ascii_uppercase())
}

fn wire_mac(value: &str) -> Result<()> {
    ensure!(
        normalized_mac(value)? == value && value.len() == 12,
        "协议 BLE 地址必须为 12 位大写十六进制值"
    );
    Ok(())
}

fn bounded_optional(value: &Option<String>, limit: usize, label: &str) -> Result<()> {
    if let Some(value) = value {
        ensure!(
            value.len() <= limit && !value.contains('\0'),
            "{label} 过长或包含 NUL"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub run_id: String,
    pub target: Target,
    /// The app supplies its frozen RFC3339 timestamp with Beijing +08:00 offset.
    pub timestamp: String,
    pub port: String,
    pub result: ResultEvent,
    pub log: String,
}

/// Route beneath the configured root. Work orders never become path segments.
/// Existing rows are immutable: identical retries are no-ops, any changed
/// run/COM/attempt record is rejected before the atomic replacement.
pub fn append_batch(
    root: &Path,
    records: &[Record],
    production: &ProductionBatch,
) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "蓝牙验证记录根目录必须为绝对路径");
    ensure!(!records.is_empty(), "没有待保存的蓝牙验证结果");
    let production = ProductionBatch::parse(
        &production.work_order,
        &production.board_week,
        &production.board_date.to_string(),
    )?;
    let directory = root.join(&production.board_week).join("蓝牙验证");
    let name = format!(
        "ble-validation-{:x}.csv",
        Sha256::digest(production.work_order.as_bytes())
    );
    let rows = records
        .iter()
        .map(|record| record_cells(record, &production))
        .collect::<Result<Vec<_>>>()?;
    crate::verification_csv::append_rows(&directory, &name, &HEADER, &rows, &[0, 11, 1], None)
}

fn record_cells(record: &Record, production: &ProductionBatch) -> Result<Vec<String>> {
    record.result.validate()?;
    let target = Target::new(&record.target.mac, &record.target.unique_device_id)?;
    ensure!(
        record.run_id == record.result.session && target.mac == record.result.mac,
        "蓝牙结果与冻结的会话或目标地址不一致"
    );
    ensure!(
        !record.port.trim().is_empty() && record.port.len() <= 256 && !record.port.contains('\0'),
        "S3 串口标识无效"
    );
    let time = chrono::DateTime::parse_from_rfc3339(&record.timestamp)
        .context("蓝牙验证时间必须为 RFC3339")?;
    ensure!(
        time.offset().local_minus_utc() == 8 * 60 * 60,
        "蓝牙验证时间必须使用北京时间 +08:00"
    );
    ensure!(
        record.log.len() <= MAX_LOG_BYTES,
        "蓝牙验证尝试日志超过 512 KiB"
    );
    let cells = [
        record.run_id.clone(),
        record.result.attempt.to_string(),
        record.timestamp.clone(),
        target.mac,
        record.result.observed_mac.clone().unwrap_or_default(),
        target.unique_device_id,
        record.result.status.as_str().to_owned(),
        record.result.name.clone().unwrap_or_default(),
        record.result.firmware.clone().unwrap_or_default(),
        record
            .result
            .fault_bits
            .map(|value| value.to_string())
            .unwrap_or_default(),
        record
            .result
            .battery_mv
            .map(|value| value.to_string())
            .unwrap_or_default(),
        record.port.clone(),
        record.result.reason.clone().unwrap_or_default(),
        record.log.clone(),
        production.work_order.clone(),
        production.board_week.clone(),
        production.board_date.to_string(),
    ]
    .into_iter()
    .collect::<Vec<_>>();
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        fs::{self, OpenOptions},
        io::BufReader,
    };

    const MAC: &str = "001122AABBCC";
    const UID: &str = "00112233445566778899AABBCCDDEEFF";

    fn session() -> String {
        "ab".repeat(32)
    }
    fn target() -> Target {
        Target::new(MAC, UID).unwrap()
    }
    fn line(value: &Value) -> String {
        format!("{EVENT_PREFIX}{value}")
    }
    fn pass() -> Value {
        json!({"event":"result", "session":session(), "attempt":1, "mac":MAC,
            "observed_mac":MAC, "status":PASS_MARKER, "name":"Lumysi-synthetic",
            "firmware":EXPECTED_FIRMWARE, "fault_bits":0, "battery_mv":2000})
    }
    fn result(value: &Value) -> ResultEvent {
        let Some(Event::Result(result)) = parse_line(&line(value)).unwrap() else {
            panic!("expected result");
        };
        result
    }
    fn production(order: &str) -> ProductionBatch {
        ProductionBatch::parse(order, "2648", "2026-11-26").unwrap()
    }
    fn record(attempt: u32) -> Record {
        let mut value = pass();
        value["attempt"] = attempt.into();
        Record {
            run_id: session(),
            target: target(),
            timestamp: "2026-11-26T18:01:02+08:00".into(),
            port: "COM7".into(),
            result: result(&value),
            log: format!("Synthetic boot\n{}\n", line(&value)),
        }
    }
    fn rows(bytes: &[u8]) -> Vec<Vec<String>> {
        assert!(bytes.starts_with(b"\xef\xbb\xbf"));
        let mut reader = BufReader::new(&bytes[3..]);
        let mut rows = Vec::new();
        while let Some(row) =
            crate::verification_csv::read_row_with_limit(&mut reader, HEADER.len()).unwrap()
        {
            rows.push(row);
        }
        rows
    }

    #[test]
    fn target_normalizes_explicit_inputs_and_never_sends_the_separately_entered_uid() {
        for mac in ["00:11:22:aa:bb:cc", "00-11-22-aa-bb-cc", MAC] {
            let target = Target::new(mac, &UID.to_ascii_lowercase()).unwrap();
            assert_eq!(target.mac, MAC);
            assert_eq!(target.unique_device_id, UID);
            let command = target.command(&session()).unwrap();
            assert_eq!(command, format!("BLE_TARGET {} {MAC}\n", session()));
            assert!(!command.contains(UID));
        }
        for mac in [
            "000000000000",
            "FFFFFFFFFFFF",
            "001122AABBC",
            "00:1122:AA:BB:CC",
            "001122AABBCC trailing",
        ] {
            assert!(Target::new(mac, UID).is_err());
        }
        for uid in ["0".repeat(32), "A".repeat(31), "G".repeat(32)] {
            assert!(Target::new(MAC, &uid).is_err());
        }
        assert!(Target::new(MAC, &"F".repeat(32)).is_ok());
        for nonce in [
            "a".repeat(63),
            "A".repeat(64),
            "g".repeat(64),
            format!("{}\n", session()),
        ] {
            assert!(target_command(&nonce, &target()).is_err());
        }
    }

    #[test]
    fn ready_start_and_result_are_strictly_typed_and_bound_to_the_explicit_session_and_mac() {
        let ready = json!({"event":"ready", "session":session(), "mac":MAC});
        let start = json!({"event":"start", "session":session(), "mac":MAC, "attempt":1});
        assert!(matches!(
            parse_line(&line(&ready)).unwrap(),
            Some(Event::Ready(_))
        ));
        let event = parse_line_for_target(&line(&start), &session(), &target())
            .unwrap()
            .unwrap();
        assert_eq!(event.attempt(), Some(1));
        assert_eq!(event.session(), session());
        assert_eq!(event.mac(), MAC);
        assert!(result(&pass()).passed());
        assert!(parse_line_for_target(&line(&pass()), &"cd".repeat(32), &target()).is_err());
        let other = Target::new("AABBCCDDEE00", UID).unwrap();
        assert!(parse_line_for_target(&line(&pass()), &session(), &other).is_err());
        assert!(
            parse_line(&format!("console {}", line(&pass())))
                .unwrap()
                .is_none()
        );
        assert!(parse_line("PASS！").unwrap().is_none());
    }

    #[test]
    fn malformed_json_unknown_or_duplicate_fields_and_invalid_numbers_never_become_events() {
        for text in [
            format!(
                "BLE_VERIFY {{\"event\":\"ready\",\"session\":\"{}\",\"session\":\"{}\",\"mac\":\"{MAC}\"}}",
                session(),
                session()
            ),
            "BLE_VERIFY {not JSON}".into(),
            format!("BLE_VERIFY {} {{}}", pass()),
            format!("BLE_VERIFY {}", "x".repeat(MAX_PROTOCOL_LINE)),
        ] {
            assert!(parse_line(&text).is_err());
        }
        for (key, value) in [
            ("attempt", json!(0)),
            ("attempt", json!(-1)),
            ("attempt", json!(4294967296_u64)),
            ("fault_bits", json!(-1)),
            ("fault_bits", json!(4294967296_u64)),
            ("battery_mv", json!(65536)),
            ("battery_mv", json!(1.5)),
            ("mac", json!("00:11:22:AA:BB:CC")),
            ("mac", json!("001122aabbcc")),
            ("uid", json!(UID)),
            ("extra", json!(true)),
        ] {
            let mut value_json = pass();
            value_json[key] = value;
            assert!(
                parse_line(&line(&value_json)).is_err(),
                "accepted invalid {key}"
            );
        }
    }

    #[test]
    fn pass_requires_exact_marker_and_complete_consistent_metadata_without_name_suffix_guessing() {
        for key in [
            "observed_mac",
            "name",
            "firmware",
            "fault_bits",
            "battery_mv",
        ] {
            let mut value = pass();
            value.as_object_mut().unwrap().remove(key);
            assert!(parse_line(&line(&value)).is_err(), "missing {key}");
        }
        for (key, value) in [
            ("status", json!("PASS")),
            ("status", json!("PASS!")),
            ("status", json!("pass！")),
            ("observed_mac", json!("AABBCCDDEE00")),
            ("name", json!("Other-synthetic")),
            ("name", json!("Lumysi-")),
            ("name", json!("Lumysi-  ")),
            ("name", json!("Lumysi-设备")),
            ("firmware", json!("1.1880")),
            ("firmware", json!("1.187")),
            ("fault_bits", json!(1)),
            ("reason", json!("fault")),
        ] {
            let mut candidate = pass();
            candidate[key] = value;
            assert!(parse_line(&line(&candidate)).is_err(), "invalid {key}");
        }
        let mut candidate = pass();
        candidate["name"] = json!("Lumysi-unrelated-readable-suffix");
        candidate["battery_mv"] = json!(65535);
        assert!(
            result(&candidate).passed(),
            "battery is evidence presence, not the UART voltage acceptance threshold"
        );
        let mut malformed_public_value = result(&pass());
        malformed_public_value.fault_bits = Some(2);
        assert!(!malformed_public_value.passed());
    }

    #[test]
    fn failure_preserves_optional_diagnostics_and_allows_no_observed_device() {
        let value = json!({"event":"result","session":session(),"attempt":2,"mac":MAC,"status":"FAIL","reason":"synthetic timeout"});
        let failed = result(&value);
        assert!(!failed.passed());
        assert!(failed.observed_mac.is_none());
        let mut detailed = value;
        detailed["observed_mac"] = json!("AABBCCDDEE00");
        detailed["name"] = json!("wrong name");
        detailed["firmware"] = json!("1.187");
        detailed["fault_bits"] = json!(u32::MAX);
        assert!(parse_line(&line(&detailed)).is_ok());
        detailed["reason"] = json!("x".repeat(1025));
        assert!(parse_line(&line(&detailed)).is_err());
    }

    #[test]
    fn csv_routes_by_week_and_work_order_and_appends_attempts_without_replacing_prior_bytes() {
        let root = tempfile::tempdir().unwrap();
        let production = production("../synthetic/work:*?order");
        let first = record(1);
        let path = append_batch(root.path(), std::slice::from_ref(&first), &production).unwrap();
        assert_eq!(
            path.parent(),
            Some(root.path().join("2648").join("蓝牙验证").as_path())
        );
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!(
                "ble-validation-{:x}.csv",
                Sha256::digest(production.work_order.as_bytes())
            )
        );
        let original = fs::read(&path).unwrap();
        let parsed = rows(&original);
        assert_eq!(parsed[0], HEADER);
        assert_eq!(&parsed[1][3..7], [MAC, MAC, UID, PASS_MARKER]);
        assert_eq!(parsed[1][2], first.timestamp);
        assert_eq!(parsed[1][13], first.log);
        append_batch(root.path(), std::slice::from_ref(&first), &production).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut second = record(2);
        second.result.status = Status::Fail;
        second.result.observed_mac = None;
        second.result.reason = Some("synthetic fail".into());
        append_batch(root.path(), &[second.clone()], &production).unwrap();
        let appended = fs::read(&path).unwrap();
        assert!(appended.starts_with(&original));
        assert_eq!(rows(&appended).len(), 3);
        assert_eq!(rows(&appended)[2][4], "");
        append_batch(root.path(), &[second, first.clone()], &production).unwrap();
        assert_eq!(fs::read(&path).unwrap(), appended);
        let mut changed = first;
        changed.target.unique_device_id = "FFEEDDCCBBAA99887766554433221100".into();
        assert!(append_batch(root.path(), &[changed], &production).is_err());
        assert_eq!(fs::read(&path).unwrap(), appended);
    }

    #[test]
    fn csv_escapes_full_attempt_logs_and_formula_inputs_without_losing_record_identity() {
        let root = tempfile::tempdir().unwrap();
        let production = production("=synthetic-order");
        let mut first = record(1);
        first.port = "+COM7".into();
        first.result.status = Status::Fail;
        first.result.reason = Some("\t=1+1\nquoted \"reason\", 中文".into());
        first.log = "\"".repeat(MAX_LOG_BYTES);
        let path = append_batch(root.path(), std::slice::from_ref(&first), &production).unwrap();
        let before = fs::read(&path).unwrap();
        let parsed = rows(&before);
        assert_eq!(parsed[1][11], "'+COM7");
        assert!(parsed[1][12].starts_with("'\t=1+1"));
        assert_eq!(parsed[1][13], first.log);
        assert_eq!(parsed[1][14], "'=synthetic-order");
        first.log.push('x');
        assert!(append_batch(root.path(), &[first], &production).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn csv_rejects_wrong_bindings_time_or_existing_corruption_and_respects_writer_lock() {
        let root = tempfile::tempdir().unwrap();
        let production = production("synthetic-lock");
        let first = record(1);
        let path = append_batch(root.path(), std::slice::from_ref(&first), &production).unwrap();
        let original = fs::read(&path).unwrap();
        let lock_path = path.with_file_name(format!(
            "{}.lock",
            path.file_name().unwrap().to_str().unwrap()
        ));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        lock.try_lock().unwrap();
        assert!(append_batch(root.path(), &[record(2)], &production).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        drop(lock);
        let mut wrong_session = record(2);
        wrong_session.run_id = "cd".repeat(32);
        let mut wrong_target = record(2);
        wrong_target.target.mac = "AABBCCDDEE00".into();
        for bad in [wrong_session, wrong_target] {
            assert!(append_batch(root.path(), &[bad], &production).is_err());
        }
        let mut utc = record(2);
        utc.timestamp = "2026-11-26T10:00:00Z".into();
        assert!(append_batch(root.path(), &[utc], &production).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        let corrupted = [original.as_slice(), b"\"unterminated\n"].concat();
        fs::write(&path, &corrupted).unwrap();
        assert!(append_batch(root.path(), &[record(2)], &production).is_err());
        assert_eq!(fs::read(path).unwrap(), corrupted);
    }
}
