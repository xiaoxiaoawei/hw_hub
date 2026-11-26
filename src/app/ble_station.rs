use super::*;
use anyhow::{Context, ensure};
use esp32c3_flashtool::{
    backend,
    ble_verification::{self, Event, Record, ResultEvent, Status},
};
use std::{
    io::{self, Read},
    sync::atomic::{AtomicBool, Ordering},
};

const STATION_LOG_WAIT: Duration = Duration::from_secs(10);
// Give the station time to deliver its result after the scan deadline.
const SCAN_RESULT_GRACE: Duration = Duration::from_secs(3);
const SCAN_TIMEOUT_REASON: &str = "scan_timeout";
const MISSING_RESULT_REASON: &str = "scan_timeout:station_result_missing";
const MAX_LINE_BYTES: usize = 4_096;
const MAX_ATTEMPT_LOG_BYTES: usize = 512 * 1024;
const MAX_PENDING_RECORDS: usize = 32;

#[derive(Default)]
pub(super) struct BleWorkspace {
    pub(super) port_name: String,
    pub(super) status: String,
    pub(super) logs: VecDeque<String>,
    pub(super) latest: Option<BleResultView>,
    pub(super) result_open: bool,
    task: Option<BleTask>,
    queued: VecDeque<PendingRecord>,
    pending: Option<PendingRecord>,
    save_result: Option<mpsc::Receiver<Result<PathBuf, String>>>,
}

pub(super) struct BleResultView {
    pub(super) observed_mac: Option<String>,
    pub(super) uid: Option<String>,
    pub(super) passed: bool,
    pub(super) timed_out: bool,
    pub(super) detail: String,
    pub(super) time: String,
    pub(super) report: Option<PathBuf>,
    pub(super) save_error: Option<String>,
}

#[derive(Clone)]
struct PendingRecord {
    record: Record,
    production: Option<ProductionBatch>,
    root: PathBuf,
    scan_timeout_ms: Option<u32>,
}

struct ScanAttempt {
    number: u32,
    started: Instant,
    timeout_ms: Option<u32>,
    log: String,
}

impl ScanAttempt {
    fn result_overdue(&self, elapsed: Duration) -> bool {
        let scan_time = Duration::from_millis(u64::from(
            self.timeout_ms.unwrap_or(ble_verification::SCAN_TIMEOUT_MS),
        ));
        elapsed >= scan_time + SCAN_RESULT_GRACE
    }
}

struct BleTask {
    port: Port,
    session: Option<String>,
    scan_timeout_ms: Option<u32>,
    last_attempt: u32,
    opened: Option<Instant>,
    warned_awaiting: bool,
    production: Option<ProductionBatch>,
    root: PathBuf,
    receiver: mpsc::Receiver<StationMessage>,
    stop: Arc<AtomicBool>,
    attempt: Option<ScanAttempt>,
}

enum StationMessage {
    Opened,
    Line { line: String, received_at: Instant },
    Closed(Result<(), String>),
}

impl Drop for BleTask {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl BleWorkspace {
    pub(super) fn is_connected(&self) -> bool {
        self.task.is_some()
    }

    pub(super) fn capturing(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(|task| task.attempt.is_some())
    }

    pub(super) fn saving(&self) -> bool {
        self.save_result.is_some()
    }

    pub(super) fn has_unsaved(&self) -> bool {
        self.pending.is_some() || !self.queued.is_empty()
    }

    fn push_log(&mut self, line: String) {
        self.logs.push_back(line);
        if self.logs.len() > 512 {
            self.logs.pop_front();
        }
    }

    fn begin_result(&mut self, task: &mut BleTask, result: ResultEvent) {
        let Some(attempt) = task.attempt.take() else {
            return;
        };
        self.queued.push_back(PendingRecord {
            record: Record {
                run_id: result.session.clone(),
                timestamp: beijing_now().to_rfc3339(),
                port: task.port.name.clone(),
                result,
                log: attempt.log,
            },
            production: task.production.clone(),
            root: task.root.clone(),
            scan_timeout_ms: attempt.timeout_ms,
        });
        self.save_next();
    }

    fn save_next(&mut self) {
        if self.pending.is_some() || self.result_open {
            return;
        }
        let Some(pending) = self.queued.pop_front() else {
            return;
        };
        let result = &pending.record.result;
        let timed_out = is_scan_timeout(result);
        self.latest = Some(BleResultView {
            observed_mac: result.mac.clone(),
            uid: result.uid.clone(),
            passed: result.status == Status::Pass,
            timed_out,
            detail: result_detail(result, pending.scan_timeout_ms),
            time: pending.record.timestamp.clone(),
            report: None,
            save_error: None,
        });
        self.result_open = timed_out;
        self.pending = Some(pending);
        self.save();
    }

    fn fail_attempt(&mut self, task: &mut BleTask, message: String) {
        if let (Some(session), Some(attempt)) = (&task.session, task.attempt.as_mut()) {
            attempt
                .log
                .push_str(&format!("{}  {message}\n", timestamp()));
            let result = ResultEvent {
                session: session.clone(),
                attempt: attempt.number,
                mac: None,
                status: Status::Fail,
                name: None,
                firmware: None,
                fault_bits: None,
                battery_mv: None,
                reason: Some(message),
                adv_hex: None,
                scan_rsp_hex: None,
                uid: None,
            };
            self.begin_result(task, result);
        }
    }

    fn disconnect(&mut self, task: &mut BleTask, message: String) {
        self.status = message.clone();
        self.fail_attempt(task, message);
        task.stop.store(true, Ordering::Release);
    }

    fn ingest_line_at(&mut self, task: &mut BleTask, line: String, received_at: Instant) {
        self.push_log(line.clone());
        if task.stop.load(Ordering::Acquire) {
            return;
        }
        if task.attempt.as_ref().is_some_and(|attempt| {
            attempt.result_overdue(received_at.saturating_duration_since(attempt.started))
        }) {
            self.fail_attempt(task, MISSING_RESULT_REASON.into());
        }
        if let Some(attempt) = &mut task.attempt {
            if attempt.log.len() + line.len() + 1 > MAX_ATTEMPT_LOG_BYTES - MAX_LINE_BYTES {
                self.fail_attempt(task, "本次工装日志超出保存上限".into());
                return;
            }
            attempt.log.push_str(&line);
            attempt.log.push('\n');
        }
        match ble_verification::parse_line(&line) {
            Ok(Some(Event::Ready(event))) => {
                if task.session.as_deref() != Some(event.session.as_str()) {
                    self.fail_attempt(task, "工装已重启，本次扫描中断".into());
                    task.session = Some(event.session);
                    task.last_attempt = 0;
                    if !self.has_unsaved() {
                        self.status = "工装已就绪，请短按 IO7 按键".into();
                    }
                }
                task.scan_timeout_ms = event.scan_timeout_ms;
            }
            Ok(Some(Event::Start(event)))
                if task.attempt.is_none()
                    && task
                        .session
                        .as_ref()
                        .is_none_or(|session| session == &event.session)
                    && event.attempt > task.last_attempt =>
            {
                task.last_attempt = event.attempt;
                if self.queued.len() >= MAX_PENDING_RECORDS {
                    self.status = "待保存结果已满，请先关闭通过提示或重试保存".into();
                    return;
                }
                task.session = Some(event.session);
                if self
                    .latest
                    .as_ref()
                    .is_some_and(|latest| latest.report.is_some())
                {
                    self.result_open = false;
                }
                self.status = "正在扫描并核对 Lumysi 广播…".into();
                task.attempt = Some(ScanAttempt {
                    number: event.attempt,
                    started: received_at,
                    timeout_ms: event.scan_timeout_ms.or(task.scan_timeout_ms),
                    log: format!("{line}\n"),
                });
            }
            Ok(Some(Event::Result(result)))
                if task.session.as_ref() == Some(&result.session)
                    && task
                        .attempt
                        .as_ref()
                        .is_some_and(|attempt| attempt.number == result.attempt) =>
            {
                self.begin_result(task, result);
            }
            Err(error) => self.push_log(format!("{}  忽略不匹配的工装消息：{error}", timestamp())),
            _ => {}
        }
    }

    fn save(&mut self) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.status = "正在保存蓝牙验证记录…".into();
        if let Some(latest) = &mut self.latest {
            latest.save_error = None;
        }
        match thread::Builder::new()
            .name("ble-verification-csv".into())
            .spawn(move || {
                let result = ble_verification::append_batch(
                    &pending.root,
                    &[pending.record],
                    pending.production.as_ref(),
                )
                .map_err(|err| format!("{err:#}"));
                let _ = sender.send(result);
            }) {
            Ok(_) => self.save_result = Some(receiver),
            Err(error) => self.finish_save(Err(error.to_string())),
        }
    }

    fn finish_save(&mut self, result: Result<PathBuf, String>) {
        let connected = self.is_connected();
        let capturing = self.capturing();
        let Some(latest) = &mut self.latest else {
            return;
        };
        match result {
            Ok(path) => {
                self.pending = None;
                latest.report = Some(path);
                self.status = if capturing {
                    "正在扫描，上一条记录已保存"
                } else if connected {
                    "记录已保存，可短按 IO7 验证下一台"
                } else {
                    "记录已保存，工装已断开"
                }
                .into();
                self.result_open |= latest.passed;
            }
            Err(error) => {
                latest.save_error = Some(error);
                self.status = "蓝牙验证记录未保存，请重试".into();
            }
        }
    }

    fn poll_save(&mut self) {
        if let Some(receiver) = &self.save_result {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err("记录保存任务意外结束".into()),
            };
            self.save_result = None;
            self.finish_save(result);
        }
    }
}

impl FlashApp {
    pub(super) fn connect_ble(&mut self, ctx: &egui::Context) {
        if self.busy() || self.ble.has_unsaved() {
            return;
        }
        match self.launch_ble(ctx) {
            Ok(task) => {
                self.error = None;
                self.notice = None;
                self.ble.result_open = false;
                self.ble.logs.clear();
                self.ble.latest = None;
                self.ble.status = format!("正在连接 {}…", task.port.name);
                self.ble.task = Some(task);
            }
            Err(error) => self.ble.status = format!("无法连接工装：{error:#}"),
        }
    }

    fn launch_ble(&self, ctx: &egui::Context) -> anyhow::Result<BleTask> {
        let production = if self.production_inputs.is_blank() {
            None
        } else {
            Some(self.production_inputs.parse()?)
        };
        let root = record_root(&self.telemetry_settings)?;
        let port = self
            .ports
            .iter()
            .find(|row| {
                row.online && row.port.name == self.ble.port_name && !row.port.identity.is_empty()
            })
            .context("请选择在线的 ESP32-S3 工装串口")?
            .port
            .clone();
        let (sender, receiver) = mpsc::sync_channel(128);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_port = port.clone();
        let ctx = ctx.clone();
        thread::Builder::new()
            .name("ble-verifier-serial".into())
            .spawn(move || {
                let send = |message| {
                    let delivered = sender.send(message).is_ok();
                    ctx.request_repaint();
                    delivered
                };
                let result = listen(
                    &worker_port,
                    &worker_stop,
                    || send(StationMessage::Opened),
                    |line| {
                        send(StationMessage::Line {
                            line,
                            received_at: Instant::now(),
                        })
                    },
                )
                .map_err(|error| format!("{error:#}"));
                send(StationMessage::Closed(result));
            })?;
        Ok(BleTask {
            port,
            session: None,
            scan_timeout_ms: None,
            last_attempt: 0,
            opened: None,
            warned_awaiting: false,
            production,
            root,
            receiver,
            stop,
            attempt: None,
        })
    }

    pub(super) fn disconnect_ble(&mut self) {
        if let Some(mut task) = self.ble.task.take() {
            if !task.stop.load(Ordering::Acquire) {
                self.ble.disconnect(&mut task, "已断开工装".into());
            }
            self.ble.task = Some(task);
        }
    }

    pub(super) fn invalidate_ble_port(&mut self, observed: &[Port]) {
        if let Some(mut task) = self.ble.task.take() {
            if !task.stop.load(Ordering::Acquire)
                && !observed
                    .iter()
                    .any(|port| port.name == task.port.name && port.identity == task.port.identity)
            {
                self.ble
                    .disconnect(&mut task, "工装已拔出或串口身份改变".into());
            }
            self.ble.task = Some(task);
        }
    }

    pub(super) fn poll_ble(&mut self) {
        if let Some(mut task) = self.ble.task.take() {
            let mut closed = false;
            for _ in 0..256 {
                let message = match task.receiver.try_recv() {
                    Ok(message) => message,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        StationMessage::Closed(Err("工装串口读取意外结束".into()))
                    }
                };
                match message {
                    StationMessage::Opened => {
                        task.opened = Some(Instant::now());
                        self.ble.status = "串口已打开，正在读取工装日志…".into();
                    }
                    StationMessage::Line { line, received_at } => {
                        self.ble.ingest_line_at(&mut task, line, received_at);
                    }
                    StationMessage::Closed(result) => {
                        if !task.stop.load(Ordering::Acquire) {
                            let error = result.err().unwrap_or_else(|| "工装已断开".into());
                            self.ble.disconnect(&mut task, error);
                        }
                        closed = true;
                        break;
                    }
                }
            }
            if !closed {
                if task.session.is_none()
                    && !task.warned_awaiting
                    && task
                        .opened
                        .is_some_and(|time| time.elapsed() >= STATION_LOG_WAIT)
                {
                    task.warned_awaiting = true;
                    self.ble.status =
                        "仍在监听，尚未收到工装状态。请核对板载 USB 转 TTL 的 COM 口和工装固件。"
                            .into();
                }
                if task
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.result_overdue(attempt.started.elapsed()))
                {
                    self.ble
                        .fail_attempt(&mut task, MISSING_RESULT_REASON.into());
                }
                self.ble.task = Some(task);
            }
        }
        self.ble.poll_save();
        self.ble.save_next();
    }

    pub(super) fn retry_ble_save(&mut self) {
        if self.ble.has_unsaved() && !self.ble.saving() {
            self.ble.save();
        }
    }

    pub(super) fn open_ble_report(&mut self) {
        if let Some(path) = self
            .ble
            .latest
            .as_ref()
            .and_then(|latest| latest.report.as_ref())
            && let Err(error) = open_records_directory(path)
        {
            self.ble.status = format!("无法打开蓝牙验证 CSV：{error}");
        }
    }
}

fn is_scan_timeout(result: &ResultEvent) -> bool {
    result.status == Status::Fail
        && result.reason.as_deref().is_some_and(|reason| {
            reason == SCAN_TIMEOUT_REASON
                || reason
                    .split_once(':')
                    .is_some_and(|(code, _)| code == SCAN_TIMEOUT_REASON)
        })
}

fn result_detail(result: &ResultEvent, scan_timeout_ms: Option<u32>) -> String {
    let mut details = Vec::new();
    if let Some(reason) = &result.reason {
        let timed_out = is_scan_timeout(result);
        let reason = if timed_out {
            details.push(match scan_timeout_ms {
                Some(ms) => format!("{} 秒内未通过蓝牙验证", f64::from(ms) / 1000.0),
                None => "蓝牙验证扫描超时".into(),
            });
            reason.split_once(':').map_or("", |(_, cause)| cause)
        } else {
            reason.as_str()
        };
        if !reason.is_empty() {
            details.push(
                match reason {
                    "station_result_missing" => "等待工装结果超时",
                    "expected_target_not_seen" => "未发现目标 BLE 地址",
                    "no_lumysi_candidate" => "未发现名称含 Lumysi 的设备",
                    "multiple_lumysi_candidates" => "同时发现多台 Lumysi 设备",
                    "settle_window_incomplete" => "扫描结束前未完成候选设备复核",
                    "candidate_capacity_exceeded" | "scan_report_overflow" => {
                        "本轮广播过多，请重试"
                    }
                    "conflicting_packets" => "目标设备的广播内容互相冲突",
                    "malformed_advertising_data" => "广播数据格式无效",
                    "invalid_complete_name" => "完整广播名称格式不符",
                    "required_service_missing" => "主广播缺少要求的服务 UUID",
                    "complete_name_missing" => "扫描响应缺少完整名称",
                    "diagnostic_scan_response_missing" => "扫描响应缺少有效的自检数据",
                    "nonzero_fault_bits" => "设备自检报告故障",
                    "firmware_version_mismatch" => "设备固件版本与要求的 1.188 不符",
                    "ambiguous_address_type" => "同一地址出现多种 BLE 地址类型",
                    "incomplete_scan" => "工装未读到完整广播",
                    other => other,
                }
                .into(),
            );
        }
    }
    if let Some(name) = &result.name {
        details.push(name.clone());
    }
    if let Some(version) = &result.firmware {
        details.push(format!("固件 {version}"));
    }
    if let Some(faults) = result.fault_bits {
        details.push(format!("故障位 0x{faults:08X}"));
    }
    if let Some(battery) = result.battery_mv {
        details.push(format!("电池采样 {battery} mV"));
    }
    details.join(" · ")
}

fn listen(
    expected: &Port,
    stop: &AtomicBool,
    on_open: impl FnOnce() -> bool,
    mut on_line: impl FnMut(String) -> bool,
) -> anyhow::Result<()> {
    verify_station_port(expected)?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut serial = serialport::new(&expected.name, ble_verification::BAUD_RATE)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .preserve_dtr_on_open()
        .timeout(Duration::from_millis(100))
        .open()
        .with_context(|| format!("无法打开 {}，请检查连接和串口占用", expected.name))?;
    verify_station_port(expected)?;
    if stop.load(Ordering::Acquire) || !on_open() {
        return Ok(());
    }
    let mut frame = Vec::new();
    let mut line_truncated = false;
    let mut chunk = [0_u8; 1_024];
    while !stop.load(Ordering::Acquire) {
        let count = match serial.read(&mut chunk) {
            Ok(0) => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("无法读取工装串口"),
        };
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                let decoded = String::from_utf8_lossy(&frame);
                let line = if line_truncated {
                    format!("[日志行过长] {decoded}")
                } else {
                    decoded.into_owned()
                };
                frame.clear();
                line_truncated = false;
                if !on_line(line) {
                    return Ok(());
                }
            } else if frame.len() < MAX_LINE_BYTES {
                frame.push(*byte);
            } else {
                line_truncated = true;
            }
        }
    }
    Ok(())
}

fn verify_station_port(expected: &Port) -> anyhow::Result<()> {
    ensure!(
        backend::list_ports()?
            .iter()
            .any(|port| port.name == expected.name && port.identity == expected.identity),
        "工装串口已离线或设备身份改变，请重新选择"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    impl BleWorkspace {
        fn ingest_line(&mut self, task: &mut BleTask, line: String) {
            self.ingest_line_at(task, line, Instant::now());
        }
    }

    fn sample_task(root: &Path) -> (BleTask, mpsc::Sender<StationMessage>) {
        let (sender, receiver) = mpsc::channel();
        let task = BleTask {
            port: Port {
                name: "COM7".into(),
                description: "ESP32-S3".into(),
                usb: true,
                suggested: false,
                identity: "sample-fixture".into(),
            },
            session: None,
            scan_timeout_ms: None,
            last_attempt: 0,
            opened: None,
            warned_awaiting: false,
            production: Some(ProductionBatch::parse("QA-WO", "2642", "2026-10-17").unwrap()),
            root: root.to_path_buf(),
            receiver,
            stop: Arc::new(AtomicBool::new(false)),
            attempt: None,
        };
        (task, sender)
    }

    fn line(mut payload: Value) -> String {
        payload["session"] = json!("a".repeat(64));
        format!("BLE_VERIFY {payload}")
    }

    fn pass_payload(attempt: u32) -> Value {
        json!({
            "event": "result", "attempt": attempt, "status": "PASS！",
            "mac": "123456789ABC", "name": "Lumysi-1234",
            "firmware": "1.188", "fault_bits": 0, "battery_mv": 661,
            "adv_hex": "02010611064B9131C3C9C5CC8F9E45B51F01C2AF4F",
            "scan_rsp_hex": "0DFFFFFF4C000000000100BC02950C094C756D7973692D31323334",
        })
    }

    #[test]
    fn pass_requires_current_scan_start_raw_evidence_and_durable_record() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        let mut workspace = BleWorkspace::default();

        let start = line(json!({"event":"start", "attempt":1}));
        let ready = line(json!({"event":"ready", "protocol":2}));
        let result = line(pass_payload(1));
        workspace.ingest_line(&mut task, result.clone());
        assert!(workspace.latest.is_none() && task.attempt.is_none());
        workspace.ingest_line(&mut task, ready);
        workspace.ingest_line(&mut task, start);
        assert!(task.attempt.is_some());

        workspace.ingest_line(&mut task, "PASS！".into());
        workspace.ingest_line(&mut task, result.replace(&"a".repeat(64), &"b".repeat(64)));
        let wrong_attempt = line(pass_payload(2));
        workspace.ingest_line(&mut task, wrong_attempt);
        let mut no_diagnostics = pass_payload(1);
        no_diagnostics["scan_rsp_hex"] = json!("0C094C756D7973692D31323334");
        workspace.ingest_line(&mut task, line(no_diagnostics));
        assert!(workspace.latest.is_none() && !workspace.result_open);

        workspace.ingest_line(&mut task, result);
        assert!(workspace.latest.as_ref().unwrap().passed);
        assert!(!workspace.result_open);
        let saved = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        workspace.finish_save(saved);
        let view = workspace.latest.as_ref().unwrap();
        let saved = fs::read_to_string(view.report.as_ref().unwrap()).unwrap();
        assert!(workspace.result_open);
        assert!(!workspace.has_unsaved());
        assert!(view.time.ends_with("+08:00"));
        assert!(saved.contains("QA-WO") && saved.contains("Lumysi-1234"));
        assert!(saved.contains("PASS！") && saved.contains("123456789ABC"));
        assert!(saved.contains("\"4fafc2011fb5459e8fccc5c9c331914b\""));
        assert!(saved.contains("unique_device_id_source=ble_service_uuid"));
        assert!(view.uid.is_none());
        assert!(!task.stop.load(Ordering::Acquire));
    }

    #[test]
    fn a_disconnected_attempt_is_saved_as_failure_and_never_prompts() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        let mut workspace = BleWorkspace::default();
        let ready = line(json!({"event":"ready", "protocol":2}));
        let start = line(json!({"event":"start", "attempt":1}));
        workspace.ingest_line(&mut task, ready);
        workspace.ingest_line(&mut task, start);
        workspace.ingest_line(&mut task, "raw fixture log entry".into());
        workspace.disconnect(&mut task, "工装已拔出".into());
        let saved = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        workspace.finish_save(saved);
        let view = workspace.latest.as_ref().unwrap();
        let saved = fs::read_to_string(view.report.as_ref().unwrap()).unwrap();
        assert!(!view.passed && !workspace.result_open);
        assert!(saved.contains("工装已拔出") && saved.contains("raw fixture log entry"));
    }

    #[test]
    fn scan_timeout_prompts_before_csv_and_does_not_prompt_again_after_close() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        let mut workspace = BleWorkspace::default();
        workspace.ingest_line(
            &mut task,
            line(json!({"event":"ready", "protocol":2, "scan_timeout_ms":60000})),
        );
        workspace.ingest_line(
            &mut task,
            line(json!({"event":"start", "attempt":1, "scan_timeout_ms":60000})),
        );
        let attempt = task.attempt.as_ref().unwrap();
        for millis in [28_000, 60_000, 62_999] {
            assert!(!attempt.result_overdue(Duration::from_millis(millis)));
        }
        assert!(attempt.result_overdue(Duration::from_secs(63)));
        workspace.ingest_line(
            &mut task,
            line(json!({
                "event": "result", "attempt": 1, "status": "FAIL",
                "reason": "scan_timeout:no_lumysi_candidate",
            })),
        );
        let latest = workspace.latest.as_ref().unwrap();
        assert!(workspace.result_open && latest.timed_out && !latest.passed);
        assert!(latest.report.is_none());
        assert!(latest.detail.contains("60 秒") && latest.detail.contains("Lumysi"));

        workspace.result_open = false;
        let saved = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        workspace.finish_save(saved);
        assert!(!workspace.result_open);
        let report = workspace.latest.as_ref().unwrap().report.as_ref().unwrap();
        let data = fs::read_to_string(report).unwrap();
        assert!(data.contains("scan_timeout:no_lumysi_candidate"));
    }

    #[test]
    fn legacy_timeout_is_still_reported_without_claiming_the_new_duration() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        let mut workspace = BleWorkspace::default();
        workspace.ingest_line(&mut task, line(json!({"event":"start", "attempt":1})));
        let attempt = task.attempt.as_ref().unwrap();
        assert_eq!(attempt.timeout_ms, None);
        assert!(!attempt.result_overdue(Duration::from_secs(60)));
        assert!(attempt.result_overdue(Duration::from_secs(63)));

        workspace.ingest_line(
            &mut task,
            line(json!({
                "event": "result", "attempt": 1, "status": "FAIL",
                "reason": "scan_timeout:no_lumysi_candidate",
            })),
        );
        let latest = workspace.latest.as_ref().unwrap();
        assert!(workspace.result_open && latest.timed_out);
        assert!(latest.detail.contains("扫描超时") && !latest.detail.contains("60 秒"));
        workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn queued_results_use_serial_arrival_time_for_the_watchdog() {
        for (arrival_ms, passed) in [(62_999, true), (63_000, false)] {
            let root = tempfile::tempdir().unwrap();
            let (mut task, _sender) = sample_task(root.path());
            let mut workspace = BleWorkspace::default();
            let started = Instant::now();
            workspace.ingest_line_at(
                &mut task,
                line(json!({"event":"start", "attempt":1, "scan_timeout_ms":60000})),
                started,
            );
            workspace.ingest_line_at(
                &mut task,
                line(pass_payload(1)),
                started + Duration::from_millis(arrival_ms),
            );
            let latest = workspace.latest.as_ref().unwrap();
            assert_eq!(latest.passed, passed);
            assert_eq!(workspace.result_open, !passed);
            let report = workspace
                .save_result
                .take()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(
                fs::read_to_string(report)
                    .unwrap()
                    .contains(MISSING_RESULT_REASON),
                !passed
            );
        }
    }

    #[test]
    fn pass_with_failed_storage_prompts_only_after_successful_retry() {
        let root = tempfile::tempdir().unwrap();
        let unavailable = root.path().join("records");
        fs::write(&unavailable, "not a directory").unwrap();
        let (mut task, _sender) = sample_task(&unavailable);
        let mut workspace = BleWorkspace::default();
        let ready = line(json!({"event":"ready", "protocol":2}));
        let start = line(json!({"event":"start", "attempt":1}));
        let result = line(pass_payload(1));
        for line in [ready, start, result] {
            workspace.ingest_line(&mut task, line);
        }
        let failure = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(failure.is_err());
        workspace.finish_save(failure);
        assert!(!workspace.result_open && workspace.has_unsaved());
        assert!(workspace.latest.as_ref().unwrap().save_error.is_some());

        fs::remove_file(&unavailable).unwrap();
        workspace.save();
        let retry = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        workspace.finish_save(retry);
        assert!(workspace.result_open && !workspace.has_unsaved());
        let report = workspace.latest.as_ref().unwrap().report.as_ref().unwrap();
        assert!(report.starts_with(unavailable.join("2642").join("蓝牙验证")));
    }

    #[test]
    fn two_attempts_share_the_open_port_and_need_no_production_metadata() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        task.production = None;
        let mut workspace = BleWorkspace::default();
        workspace.ingest_line(&mut task, "UART0 ready".into());
        workspace.ingest_line(&mut task, line(json!({"event":"ready", "protocol":2})));
        workspace.ingest_line(&mut task, line(json!({"event":"start", "attempt":1})));
        workspace.ingest_line(
            &mut task,
            line(json!({
                "event":"result", "attempt":1, "status":"FAIL", "mac":"123456789ABC",
                "name":"Lumysi-3D38", "reason":"diagnostic_scan_response_missing",
                "adv_hex":"02010611064B9131C3C9C5CC8F9E45B51F01C2AF4F051206000600",
                "scan_rsp_hex":"0C094C756D7973692D33443338",
            })),
        );
        workspace.ingest_line(&mut task, line(json!({"event":"start", "attempt":2})));
        workspace.ingest_line(&mut task, line(pass_payload(2)));
        assert_eq!(workspace.queued.len(), 1);

        let wait_for_save = |workspace: &mut BleWorkspace| {
            let saved = workspace
                .save_result
                .take()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            workspace.finish_save(saved);
        };
        wait_for_save(&mut workspace);
        assert!(!workspace.result_open);
        workspace.save_next();
        wait_for_save(&mut workspace);
        assert!(workspace.result_open && !workspace.has_unsaved());
        let report = workspace.latest.as_ref().unwrap().report.as_ref().unwrap();
        assert_eq!(
            report,
            &root.path().join("蓝牙验证").join("ble-validation.csv")
        );
        let contents = fs::read_to_string(report).unwrap();
        assert!(contents.contains("diagnostic_scan_response_missing"));
        assert!(contents.contains("0DFFFFFF4C000000000100BC0295"));
        assert!(contents.contains("PASS！") && contents.contains("Lumysi-3D38"));
        assert!(!task.stop.load(Ordering::Acquire));

        workspace.ingest_line(&mut task, line(json!({"event":"start", "attempt":2})));
        assert!(task.attempt.is_none());
    }
}
