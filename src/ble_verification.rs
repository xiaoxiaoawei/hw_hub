//! Passive S3 verifier protocol v2 and atomic BLE verification history.
//! Actual BLE addresses and raw packets come from the serial log. There is no
//! target configuration, host command, or invented association with a device ID.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::production::{PRODUCTION_HEADERS, ProductionBatch};

pub const BAUD_RATE: u32 = 115_200;
pub const EVENT_PREFIX: &str = "BLE_VERIFY ";
pub const PASS_MARKER: &str = "PASS！";
pub const EXPECTED_FIRMWARE: &str = "1.188";
pub const PROTOCOL_VERSION: u32 = 2;
pub const NAME_TOKEN: &str = "Lumysi";
const SERVICE_LE: [u8; 16] = [
    0x4b, 0x91, 0x31, 0xc3, 0xc9, 0xc5, 0xcc, 0x8f, 0x9e, 0x45, 0xb5, 0x1f, 0x01, 0xc2, 0xaf, 0x4f,
];
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
    pub protocol: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartEvent {
    pub session: String,
    pub attempt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvent {
    pub session: String,
    pub attempt: u32,
    pub status: Status,
    /// Actual over-the-air address reported by the scanner, never a target.
    #[serde(default)]
    pub mac: Option<String>,
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
    #[serde(default)]
    pub adv_hex: Option<String>,
    #[serde(default)]
    pub scan_rsp_hex: Option<String>,
    /// Only an explicitly supplied trusted log value; current S3 firmware
    /// has no UID source and therefore omits this field.
    #[serde(default)]
    pub uid: Option<String>,
}

impl ResultEvent {
    pub fn passed(&self) -> bool {
        self.status == Status::Pass && self.validate().is_ok()
    }

    pub fn validate(&self) -> Result<()> {
        valid_session(&self.session)?;
        ensure!(self.attempt >= 1, "蓝牙验证 attempt 必须大于 0");
        if let Some(mac) = &self.mac {
            wire_mac(mac)?;
        }
        bounded_optional(&self.name, 128, "广播名称")?;
        bounded_optional(&self.firmware, 32, "固件版本")?;
        bounded_optional(&self.reason, 1024, "失败原因")?;
        if let Some(uid) = &self.uid {
            normalize_uid(uid)?;
        }
        let adv = self.adv_hex.as_deref().map(decode_packet).transpose()?;
        let scan_rsp = self
            .scan_rsp_hex
            .as_deref()
            .map(decode_packet)
            .transpose()?;
        if self.status == Status::Pass {
            ensure!(self.mac.is_some(), "PASS 缺少实际 BLE 地址");
            let adv = adv.as_deref().context("PASS 缺少实际 ADV 原始数据")?;
            let scan_rsp = scan_rsp
                .as_deref()
                .context("PASS 缺少实际扫描响应原始数据")?;
            let evidence = packet_evidence(adv, scan_rsp)?;
            ensure!(
                self.name.as_deref() == Some(evidence.name.as_str()),
                "PASS 广播名称与原始数据不一致"
            );
            ensure!(
                self.firmware.as_deref() == Some(EXPECTED_FIRMWARE),
                "PASS 固件版本不匹配"
            );
            ensure!(
                self.fault_bits == Some(evidence.fault_bits),
                "PASS 故障位与原始数据不一致"
            );
            ensure!(
                self.battery_mv == Some(evidence.battery_mv),
                "PASS 电池毫伏值与原始数据不一致"
            );
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
    pub fn mac(&self) -> Option<&str> {
        match self {
            Self::Result(event) => event.mac.as_deref(),
            _ => None,
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
        match self {
            Self::Start(event) => ensure!(event.attempt >= 1, "蓝牙验证 attempt 必须大于 0"),
            Self::Result(event) => event.validate()?,
            Self::Ready(event) => ensure!(
                event.protocol == PROTOCOL_VERSION,
                "需要 BLE 验证协议 v2 固件"
            ),
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
    let mut event: Event = serde_json::from_str(json.trim_end_matches(['\r', '\n']))
        .map_err(|_| anyhow::anyhow!("蓝牙验证 JSON 格式、事件字段或结果标记无效"))?;
    if let Event::Result(result) = &mut event
        && let Some(uid) = &mut result.uid
    {
        *uid = normalize_uid(uid)?;
    }
    event.validate()?;
    Ok(Some(event))
}

/// Bind events after a protocol-v2 ready establishes the passive session.
/// Attempt sequencing and stale-result rejection belong to the worker.
pub fn parse_line_for_session(line: &str, session: &str) -> Result<Option<Event>> {
    valid_session(session)?;
    let event = parse_line(line)?;
    if let Some(event) = &event {
        ensure!(event.session() == session, "蓝牙验证事件不属于当前会话");
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

fn wire_mac(value: &str) -> Result<()> {
    ensure!(
        value.len() == 12
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)),
        "协议 BLE 地址必须为 12 位大写十六进制值"
    );
    ensure!(
        !value.bytes().all(|byte| byte == b'0') && !value.bytes().all(|byte| byte == b'F'),
        "实际 BLE 地址不能为全 0 或全 FF"
    );
    Ok(())
}

fn normalize_uid(value: &str) -> Result<String> {
    ensure!(
        value.len() == 32
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            && !value.bytes().all(|byte| byte == b'0'),
        "日志设备 ID 必须为非全 0 的 32 位十六进制值"
    );
    Ok(value.to_ascii_uppercase())
}

fn decode_packet(text: &str) -> Result<Vec<u8>> {
    ensure!(
        text.len() <= 62
            && text.len().is_multiple_of(2)
            && text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "BLE 原始包必须为不超过 31 字节的完整十六进制值"
    );
    let pairs = text.as_bytes().as_chunks::<2>().0;
    Ok(pairs
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                if byte.is_ascii_digit() {
                    byte - b'0'
                } else {
                    byte.to_ascii_uppercase() - b'A' + 10
                }
            };
            (digit(pair[0]) << 4) | digit(pair[1])
        })
        .collect())
}

fn ad_fields(mut packet: &[u8]) -> Result<Vec<(u8, &[u8])>> {
    let mut fields = Vec::new();
    while let Some((&length, remaining)) = packet.split_first() {
        if length == 0 {
            ensure!(
                remaining.iter().all(|byte| *byte == 0),
                "BLE AD 零填充后包含非零数据"
            );
            break;
        }
        ensure!(length as usize <= remaining.len(), "BLE AD 长度截断");
        fields.push((remaining[0], &remaining[1..length as usize]));
        packet = &remaining[length as usize..];
    }
    Ok(fields)
}

struct PacketEvidence {
    name: String,
    fault_bits: u32,
    battery_mv: u16,
}

fn packet_evidence(adv: &[u8], scan_rsp: &[u8]) -> Result<PacketEvidence> {
    let mut service_seen = false;
    for (kind, value) in ad_fields(adv)? {
        if matches!(kind, 0x06 | 0x07) {
            ensure!(
                !value.is_empty() && value.len().is_multiple_of(16),
                "ADV 服务 UUID 字段长度无效"
            );
            service_seen |= value
                .as_chunks::<16>()
                .0
                .iter()
                .any(|uuid| uuid == &SERVICE_LE);
        }
    }
    ensure!(service_seen, "ADV 缺少指定的完整服务 UUID");
    let mut name: Option<&[u8]> = None;
    let mut diagnostic: Option<&[u8]> = None;
    for (kind, value) in ad_fields(scan_rsp)? {
        match kind {
            0x09 => {
                ensure!(
                    !value.is_empty() && value.iter().all(|byte| (0x20..=0x7e).contains(byte)),
                    "扫描响应完整名称不是非空的可打印 ASCII"
                );
                ensure!(
                    value
                        .windows(NAME_TOKEN.len())
                        .any(|part| part.eq_ignore_ascii_case(NAME_TOKEN.as_bytes())),
                    "扫描响应完整名称不包含 Lumysi"
                );
                ensure!(
                    name.is_none_or(|previous| previous == value),
                    "扫描响应包含相互冲突的完整名称"
                );
                name = Some(value);
            }
            0xff => {
                ensure!(
                    value.len() == 12 && value[..3] == [0xff, 0xff, 0x4c],
                    "扫描响应厂商数据的长度、公司标识或魔术字节无效"
                );
                ensure!(
                    diagnostic.is_none_or(|previous| previous == value),
                    "扫描响应包含相互冲突的诊断数据"
                );
                diagnostic = Some(value);
            }
            _ => {}
        }
    }
    let name = name.context("扫描响应缺少完整名称字段")?;
    let diagnostic = diagnostic.context("扫描响应缺少诊断厂商数据")?;
    let fault_bits = u32::from_be_bytes(diagnostic[3..7].try_into().expect("length checked"));
    let minor = u16::from_be_bytes([diagnostic[8], diagnostic[9]]);
    ensure!(fault_bits == 0, "原始诊断包包含非零故障位");
    ensure!(
        diagnostic[7] == 1 && minor == 188,
        "原始诊断包固件版本不是 1.188"
    );
    Ok(PacketEvidence {
        name: String::from_utf8(name.to_vec()).expect("ASCII checked"),
        fault_bits,
        battery_mv: u16::from_be_bytes([diagnostic[10], diagnostic[11]]),
    })
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
    production: Option<&ProductionBatch>,
) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "蓝牙验证记录根目录必须为绝对路径");
    ensure!(!records.is_empty(), "没有待保存的蓝牙验证结果");
    let production = production
        .map(|production| {
            ProductionBatch::parse(
                &production.work_order,
                &production.board_week,
                &production.board_date.to_string(),
            )
        })
        .transpose()?;
    let (directory, name) = if let Some(production) = &production {
        (
            root.join(&production.board_week).join("蓝牙验证"),
            format!(
                "ble-validation-{:x}.csv",
                Sha256::digest(production.work_order.as_bytes())
            ),
        )
    } else {
        (root.join("蓝牙验证"), "ble-validation.csv".to_owned())
    };
    let rows = records
        .iter()
        .map(|record| record_cells(record, production.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    crate::verification_csv::append_rows(&directory, &name, &HEADER, &rows, &[0, 11, 1], None)
}

fn record_cells(record: &Record, production: Option<&ProductionBatch>) -> Result<Vec<String>> {
    record.result.validate()?;
    ensure!(
        record.run_id == record.result.session,
        "蓝牙结果与冻结的会话不一致"
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
        String::new(), // Legacy configured_mac column: passive v2 has no target.
        record.result.mac.clone().unwrap_or_default(),
        record
            .result
            .uid
            .as_deref()
            .map(normalize_uid)
            .transpose()?
            .unwrap_or_default(),
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
        evidence_log(record),
        production.map_or_else(String::new, |value| value.work_order.clone()),
        production.map_or_else(String::new, |value| value.board_week.clone()),
        production.map_or_else(String::new, |value| value.board_date.to_string()),
    ]
    .into_iter()
    .collect::<Vec<_>>();
    Ok(cells)
}

fn evidence_log(record: &Record) -> String {
    let mut log = record.log.clone();
    if record.result.adv_hex.is_some() || record.result.scan_rsp_hex.is_some() {
        if !log.is_empty() && !log.ends_with('\n') {
            log.push('\n');
        }
        log.push_str("[BLE protocol v2 raw evidence]\n");
        if let Some(value) = &record.result.adv_hex {
            log.push_str(&format!("adv_hex={}\n", value.to_ascii_uppercase()));
        }
        if let Some(value) = &record.result.scan_rsp_hex {
            log.push_str(&format!("scan_rsp_hex={}\n", value.to_ascii_uppercase()));
        }
    }
    log
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
    const NAME: &str = "Z-LuMySi-Q";
    fn session() -> String {
        "ab".repeat(32)
    }
    fn line(value: &Value) -> String {
        format!("{EVENT_PREFIX}{value}")
    }
    fn field(kind: u8, value: &[u8]) -> Vec<u8> {
        let mut field = vec![(value.len() + 1) as u8, kind];
        field.extend_from_slice(value);
        field
    }
    fn adv() -> Vec<u8> {
        [field(0x01, &[0x06]), field(0x07, &SERVICE_LE)].concat()
    }
    fn diagnostic(faults: u32, major: u8, minor: u16, battery: u16) -> Vec<u8> {
        [
            &[0xff, 0xff, 0x4c][..],
            &faults.to_be_bytes(),
            &[major],
            &minor.to_be_bytes(),
            &battery.to_be_bytes(),
        ]
        .concat()
    }
    fn scan(name: &[u8], diagnostic: &[u8]) -> Vec<u8> {
        [field(0x09, name), field(0xff, diagnostic)].concat()
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02X}")).collect()
    }
    fn pass() -> Value {
        json!({"event":"result", "session":session(), "attempt":1, "mac":MAC, "status":PASS_MARKER,
            "name":NAME, "firmware":EXPECTED_FIRMWARE,"fault_bits":0,"battery_mv":2000,
            "adv_hex":hex(&adv()),"scan_rsp_hex":hex(&scan(NAME.as_bytes(),&diagnostic(0,1,188,2000)))})
    }
    fn result(value: &Value) -> ResultEvent {
        let Some(Event::Result(result)) = parse_line(&line(value)).unwrap() else {
            panic!("expected result");
        };
        result
    }
    fn record(attempt: u32) -> Record {
        let mut value = pass();
        value["attempt"] = attempt.into();
        Record {
            run_id: session(),
            timestamp: "2026-11-26T18:01:02+08:00".into(),
            port: "COM7".into(),
            result: result(&value),
            log: "Synthetic serial capture\n".into(),
        }
    }
    fn production(order: &str) -> ProductionBatch {
        ProductionBatch::parse(order, "2648", "2026-11-26").unwrap()
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
    fn passive_protocol_v2_has_no_target_and_legacy_ready_cannot_establish_a_session() {
        let ready = json!({"event":"ready","session":session(),"protocol":2});
        let start = json!({"event":"start","session":session(),"attempt":1});
        let event = parse_line(&line(&ready)).unwrap().unwrap();
        assert_eq!(event.mac(), None);
        assert_eq!(event.attempt(), None);
        assert_eq!(event.session(), session());
        assert!(matches!(
            parse_line(&line(&start)).unwrap(),
            Some(Event::Start(_))
        ));
        for legacy in [
            json!({"event":"ready","session":session(),"mac":MAC}),
            json!({"event":"ready","session":session(),"protocol":2,"mac":MAC}),
            json!({"event":"ready","session":session(),"protocol":1}),
            json!({"event":"start","session":session(),"attempt":1,"mac":MAC}),
        ] {
            assert!(parse_line(&line(&legacy)).is_err());
        }
        assert!(parse_line_for_session(&line(&pass()), &"cd".repeat(32)).is_err());
        assert!(parse_line("PASS！").unwrap().is_none());
        assert!(
            parse_line(&format!("[INFO] {}", line(&pass())))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pass_is_accepted_only_from_complete_consistent_raw_packet_evidence() {
        let value = result(&pass());
        assert!(value.passed());
        assert_eq!(value.mac.as_deref(), Some(MAC));
        assert!(value.uid.is_none());
        let mut incomplete = adv();
        incomplete[4] = 0x06;
        let mut alternate = pass();
        alternate["adv_hex"] = hex(&incomplete).into();
        assert!(
            result(&alternate).passed(),
            "incomplete 128-bit UUID list type is supported"
        );
        for key in [
            "mac",
            "name",
            "firmware",
            "fault_bits",
            "battery_mv",
            "adv_hex",
            "scan_rsp_hex",
        ] {
            let mut candidate = pass();
            candidate.as_object_mut().unwrap().remove(key);
            assert!(parse_line(&line(&candidate)).is_err(), "missing {key}");
        }
        for (key, value) in [
            ("status", json!("PASS")),
            ("status", json!("PASS!")),
            ("name", json!("Lumysi-other")),
            ("firmware", json!("1.1880")),
            ("fault_bits", json!(1)),
            ("battery_mv", json!(2001)),
            ("mac", json!("00:11:22:AA:BB:CC")),
            ("mac", json!("001122aabbcc")),
            ("reason", json!("fault")),
        ] {
            let mut candidate = pass();
            candidate[key] = value;
            assert!(parse_line(&line(&candidate)).is_err(), "invalid {key}");
        }
        let mut mutable = value;
        mutable.fault_bits = Some(1);
        assert!(!mutable.passed());
    }

    #[test]
    fn raw_company_magic_service_version_and_every_fault_bit_are_authoritative() {
        let mut wrong_uuid = SERVICE_LE;
        wrong_uuid[0] ^= 1;
        let mut candidate = pass();
        candidate["adv_hex"] = hex(&field(0x07, &wrong_uuid)).into();
        assert!(parse_line(&line(&candidate)).is_err());
        for (index, value) in [
            (0, 0x01),
            (1, 0x00),
            (2, 0x00),
            (7, 0x00),
            (8, 0x01),
            (9, 0xbb),
        ] {
            let mut payload = diagnostic(0, 1, 188, 2000);
            payload[index] = value;
            let mut candidate = pass();
            candidate["scan_rsp_hex"] = hex(&scan(NAME.as_bytes(), &payload)).into();
            assert!(
                parse_line(&line(&candidate)).is_err(),
                "invalid diagnostic byte {index}"
            );
        }
        for bit in 0..32 {
            let mut candidate = pass();
            candidate["scan_rsp_hex"] =
                hex(&scan(NAME.as_bytes(), &diagnostic(1 << bit, 1, 188, 2000))).into();
            assert!(
                parse_line(&line(&candidate)).is_err(),
                "missed nonzero fault bit {bit}"
            );
        }
        for battery in [0, u16::MAX] {
            let mut candidate = pass();
            candidate["battery_mv"] = battery.into();
            candidate["scan_rsp_hex"] =
                hex(&scan(NAME.as_bytes(), &diagnostic(0, 1, 188, battery))).into();
            assert!(
                result(&candidate).passed(),
                "BLE battery is logged without applying the wired-test voltage threshold"
            );
        }
    }

    #[test]
    fn ad_truncation_conflicting_names_zero_padding_junk_and_shortened_names_never_pass() {
        let mut truncated = adv();
        truncated.push(5);
        let mut junk = adv();
        junk.extend([0, 1]);
        for packet in [truncated, junk, vec![1, 7], field(0x07, &SERVICE_LE[..15])] {
            let mut candidate = pass();
            candidate["adv_hex"] = hex(&packet).into();
            assert!(parse_line(&line(&candidate)).is_err());
        }
        let diag = field(0xff, &diagnostic(0, 1, 188, 2000));
        let conflicting = [field(0x09, b"Lumysi"), field(0x09, b"LUMYSI"), diag.clone()].concat();
        assert!(conflicting.len() <= 31);
        let identical = [field(0x09, b"Lumysi"), field(0x09, b"Lumysi"), diag.clone()].concat();
        for packet in [
            conflicting,
            [field(0x08, NAME.as_bytes()), diag.clone()].concat(),
            scan(b"NotTheDevice", &diagnostic(0, 1, 188, 2000)),
            scan(b"Lumysi\x01", &diagnostic(0, 1, 188, 2000)),
            [field(0x09, NAME.as_bytes()), vec![13, 0xff, 0xff]].concat(),
        ] {
            let mut candidate = pass();
            candidate["scan_rsp_hex"] = hex(&packet).into();
            assert!(parse_line(&line(&candidate)).is_err());
        }
        let mut candidate = pass();
        candidate["name"] = "Lumysi".into();
        candidate["scan_rsp_hex"] = hex(&identical).into();
        assert!(
            result(&candidate).passed(),
            "identical duplicate fields are not ambiguous"
        );
        let mut padded = adv();
        padded.extend([0, 0]);
        let mut candidate = pass();
        candidate["adv_hex"] = hex(&padded).into();
        assert!(result(&candidate).passed());
    }

    #[test]
    fn strict_json_numbers_session_and_optional_uid_are_not_inferred_from_a_mac() {
        for (key, value) in [
            ("attempt", json!(0)),
            ("attempt", json!(-1)),
            ("attempt", json!(4294967296_u64)),
            ("fault_bits", json!(4294967296_u64)),
            ("battery_mv", json!(65536)),
            ("battery_mv", json!(1.5)),
            ("session", json!("A".repeat(64))),
            ("uid", json!("0".repeat(32))),
            ("uid", json!("A".repeat(31))),
            ("observed_mac", json!(MAC)),
            ("target", json!(MAC)),
            ("extra", json!(true)),
            ("adv_hex", json!("0G")),
            ("scan_rsp_hex", json!("A")),
            ("adv_hex", json!("00".repeat(32))),
        ] {
            let mut candidate = pass();
            candidate[key] = value;
            assert!(parse_line(&line(&candidate)).is_err(), "invalid {key}");
        }
        let duplicate = format!(
            "BLE_VERIFY {{\"event\":\"ready\",\"protocol\":2,\"session\":\"{}\",\"session\":\"{}\"}}",
            session(),
            session()
        );
        assert!(parse_line(&duplicate).is_err());
        assert!(parse_line("BLE_VERIFY {broken}").is_err());
        assert!(parse_line(&format!("BLE_VERIFY {} {{}}", pass())).is_err());
        let mut candidate = pass();
        candidate["uid"] = UID.to_ascii_lowercase().into();
        assert_eq!(result(&candidate).uid.as_deref(), Some(UID));
        assert!(result(&pass()).uid.is_none());
    }

    #[test]
    fn failure_keeps_raw_diagnostics_even_when_they_could_not_establish_pass() {
        let value = json!({"event":"result","session":session(),"attempt":1,"status":"FAIL",
            "reason":"malformed_advertising_data","adv_hex":"1107","scan_rsp_hex":""});
        let failed = result(&value);
        assert!(!failed.passed());
        assert!(failed.mac.is_none());
        assert!(failed.uid.is_none());
        let mut with_bad_version = value;
        with_bad_version["firmware"] = "1.187".into();
        assert!(parse_line(&line(&with_bad_version)).is_ok());
    }

    #[test]
    fn ungrouped_csv_logs_actual_mac_blank_uid_and_canonical_raw_evidence_in_the_old_seventeen_columns()
     {
        let root = tempfile::tempdir().unwrap();
        let mut first = record(1);
        first.result = result(&json!({"event":"result","session":session(),"attempt":1,
            "status":"FAIL","reason":"diagnostic_scan_response_missing","mac":MAC,"scan_rsp_hex":hex(&field(0x09,NAME.as_bytes()))}));
        let path = append_batch(root.path(), std::slice::from_ref(&first), None).unwrap();
        assert_eq!(path, root.path().join("蓝牙验证/ble-validation.csv"));
        let before = fs::read(&path).unwrap();
        let second = record(2);
        append_batch(root.path(), std::slice::from_ref(&second), None).unwrap();
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&before));
        let parsed = rows(&after);
        assert_eq!(parsed[0], HEADER);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1][6], "FAIL");
        assert_eq!(parsed[2][6], PASS_MARKER);
        assert_eq!(&parsed[2][3..6], ["", MAC, ""]);
        assert!(parsed[2][14..].iter().all(String::is_empty));
        assert!(parsed[2][13].contains(&format!("adv_hex={}", hex(&adv()))));
        assert!(parsed[2][13].contains("scan_rsp_hex="));
        assert_eq!(parsed[2][2], second.timestamp);
        append_batch(root.path(), &[second.clone(), first], None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), after);
        let mut changed = second;
        changed.result.uid = Some(UID.into());
        assert!(append_batch(root.path(), &[changed], None).is_err());
        assert_eq!(fs::read(path).unwrap(), after);
    }

    #[test]
    fn grouped_v2_records_can_append_to_legacy_v060_history_without_changing_old_headers_or_rows() {
        let root = tempfile::tempdir().unwrap();
        let production = production("synthetic-order");
        let directory = root.path().join("2648/蓝牙验证");
        let name = format!(
            "ble-validation-{:x}.csv",
            Sha256::digest(production.work_order.as_bytes())
        );
        let mut legacy = vec![String::new(); HEADER.len()];
        legacy[0] = "cd".repeat(32);
        legacy[1] = "1".into();
        legacy[2] = "2026-11-26T17:00:00+08:00".into();
        legacy[3] = MAC.into();
        legacy[4] = MAC.into();
        legacy[5] = UID.into();
        legacy[6] = PASS_MARKER.into();
        legacy[11] = "COM7".into();
        let path = crate::verification_csv::append_rows(
            &directory,
            &name,
            &HEADER,
            &[legacy],
            &[0, 11, 1],
            None,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let current = record(1);
        assert_eq!(
            append_batch(
                root.path(),
                std::slice::from_ref(&current),
                Some(&production)
            )
            .unwrap(),
            path
        );
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&before));
        let parsed = rows(&after);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1][3], MAC);
        assert_eq!(parsed[1][5], UID);
        assert_eq!(&parsed[2][3..6], ["", MAC, ""]);
        assert_eq!(&parsed[2][14..], ["synthetic-order", "2648", "2026-11-26"]);
        append_batch(root.path(), &[current], Some(&production)).unwrap();
        assert_eq!(fs::read(path).unwrap(), after);
    }

    #[test]
    fn csv_bounds_formula_safety_locks_and_binding_failures_remain_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let mut first = record(1);
        first.result.status = Status::Fail;
        first.port = "+COM7".into();
        first.result.reason = Some("\t=1+1\nquoted \"detail\", 中文".into());
        first.log = "\"".repeat(MAX_LOG_BYTES);
        let path = append_batch(root.path(), std::slice::from_ref(&first), None).unwrap();
        let before = fs::read(&path).unwrap();
        let parsed = rows(&before);
        assert_eq!(parsed[1][11], "'+COM7");
        assert!(parsed[1][12].starts_with("'\t=1+1"));
        assert!(parsed[1][13].starts_with(&first.log));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_file_name("ble-validation.csv.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(append_batch(root.path(), &[record(2)], None).is_err());
        drop(lock);
        first.log.push('x');
        assert!(append_batch(root.path(), &[first], None).is_err());
        let mut wrong = record(2);
        wrong.run_id = "cd".repeat(32);
        assert!(append_batch(root.path(), &[wrong], None).is_err());
        let mut utc = record(2);
        utc.timestamp = "2026-11-26T10:00:00Z".into();
        assert!(append_batch(root.path(), &[utc], None).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let corrupted = [before.as_slice(), b"\"unterminated\n"].concat();
        fs::write(&path, &corrupted).unwrap();
        assert!(append_batch(root.path(), &[record(2)], None).is_err());
        assert_eq!(fs::read(path).unwrap(), corrupted);
    }
}
