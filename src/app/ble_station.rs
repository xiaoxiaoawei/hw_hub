use super::*;
use anyhow::{Context, bail, ensure};
use esp32c3_flashtool::{
    backend,
    ble_verification::{self, Event, Record, ResultEvent, Status, Target},
};
use std::{
    io::{self, Read, Write},
    sync::atomic::{AtomicBool, Ordering},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const SCAN_TIMEOUT: Duration = Duration::from_secs(55);
const MAX_LINE_BYTES: usize = 4_096;
const MAX_ATTEMPT_LOG_BYTES: usize = 512 * 1024;

#[derive(Default)]
pub(super) struct BleWorkspace {
    pub(super) port_name: String,
    pub(super) mac_input: String,
    pub(super) uid_input: String,
    pub(super) status: String,
    pub(super) logs: VecDeque<String>,
    pub(super) latest: Option<BleResultView>,
    pub(super) pass_open: bool,
    task: Option<BleTask>,
    pending: Option<PendingRecord>,
    save_result: Option<mpsc::Receiver<Result<PathBuf, String>>>,
}

pub(super) struct BleResultView {
    pub(super) mac: String,
    pub(super) observed_mac: Option<String>,
    pub(super) uid: String,
    pub(super) passed: bool,
    pub(super) detail: String,
    pub(super) time: String,
    pub(super) report: Option<PathBuf>,
    pub(super) save_error: Option<String>,
}

#[derive(Clone)]
struct PendingRecord {
    record: Record,
    production: ProductionBatch,
    root: PathBuf,
}

struct ScanAttempt {
    number: u32,
    started: Instant,
    log: String,
}

struct BleTask {
    port: Port,
    session: String,
    target: Target,
    production: ProductionBatch,
    root: PathBuf,
    receiver: mpsc::Receiver<StationMessage>,
    armed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    attempt: Option<ScanAttempt>,
}

enum StationMessage {
    Line(String),
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
        self.pending.is_some()
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
        task.stop.store(true, Ordering::Release);
        let timestamp = beijing_now().to_rfc3339();
        self.latest = Some(BleResultView {
            mac: task.target.mac.clone(),
            observed_mac: result.observed_mac.clone(),
            uid: task.target.unique_device_id.clone(),
            passed: result.status == Status::Pass,
            detail: result_detail(&result),
            time: timestamp.clone(),
            report: None,
            save_error: None,
        });
        self.pending = Some(PendingRecord {
            record: Record {
                run_id: task.session.clone(),
                target: task.target.clone(),
                timestamp,
                port: task.port.name.clone(),
                result,
                log: attempt.log,
            },
            production: task.production.clone(),
            root: task.root.clone(),
        });
        self.save();
    }

    fn fail_attempt(&mut self, task: &mut BleTask, message: String) {
        if let Some(attempt) = task.attempt.as_mut() {
            attempt
                .log
                .push_str(&format!("{}  {message}\n", timestamp()));
            let result = ResultEvent {
                session: task.session.clone(),
                attempt: attempt.number,
                mac: task.target.mac.clone(),
                observed_mac: None,
                status: Status::Fail,
                name: None,
                firmware: None,
                fault_bits: None,
                battery_mv: None,
                reason: Some(message),
            };
            self.begin_result(task, result);
        } else {
            task.stop.store(true, Ordering::Release);
            self.status = message;
        }
    }

    fn ingest_line(&mut self, task: &mut BleTask, line: String) {
        self.push_log(line.clone());
        if task.stop.load(Ordering::Acquire) {
            return;
        }
        if let Some(attempt) = &mut task.attempt {
            if attempt.log.len() + line.len() + 1 > MAX_ATTEMPT_LOG_BYTES - MAX_LINE_BYTES {
                self.fail_attempt(task, "本次工装日志超出保存上限".into());
                return;
            }
            attempt.log.push_str(&line);
            attempt.log.push('\n');
        }
        match ble_verification::parse_line_for_target(&line, &task.session, &task.target) {
            Ok(Some(Event::Ready(_))) if !task.armed.swap(true, Ordering::AcqRel) => {
                self.status = "工装已就绪，请短按 IO7 按键".into();
            }
            Ok(Some(Event::Start(event)))
                if task.armed.load(Ordering::Acquire) && task.attempt.is_none() =>
            {
                self.status = "正在扫描并核对目标设备…".into();
                task.attempt = Some(ScanAttempt {
                    number: event.attempt,
                    started: Instant::now(),
                    log: format!("{line}\n"),
                });
            }
            Ok(Some(Event::Result(result)))
                if task
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
                    &pending.production,
                )
                .map_err(|err| format!("{err:#}"));
                let _ = sender.send(result);
            }) {
            Ok(_) => self.save_result = Some(receiver),
            Err(error) => self.finish_save(Err(error.to_string())),
        }
    }

    fn finish_save(&mut self, result: Result<PathBuf, String>) {
        let Some(latest) = &mut self.latest else {
            return;
        };
        match result {
            Ok(path) => {
                self.pending = None;
                latest.report = Some(path);
                self.status = "记录已保存，可连接下一台待验证设备".into();
                self.pass_open = latest.passed;
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
                self.ble.pass_open = false;
                self.ble.logs.clear();
                self.ble.latest = None;
                self.ble.status = format!("正在连接 {}…", task.port.name);
                self.ble.task = Some(task);
            }
            Err(error) => self.ble.status = format!("无法连接工装：{error:#}"),
        }
    }

    fn launch_ble(&self, ctx: &egui::Context) -> anyhow::Result<BleTask> {
        let target = Target::new(&self.ble.mac_input, &self.ble.uid_input)?;
        let production = self.production_inputs.parse().context("请填写生产信息")?;
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
        let batch_id = telemetry::new_batch_id();
        let session = batch_id
            .strip_prefix("batch-")
            .context("无法生成工装会话")?
            .to_owned();
        let command = ble_verification::target_command(&session, &target)?;
        let (sender, receiver) = mpsc::sync_channel(128);
        let armed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_armed = Arc::clone(&armed);
        let worker_stop = Arc::clone(&stop);
        let worker_port = port.clone();
        let ctx = ctx.clone();
        thread::Builder::new()
            .name("ble-verifier-serial".into())
            .spawn(move || {
                let result = listen(
                    &worker_port,
                    &command,
                    &worker_armed,
                    &worker_stop,
                    |line| {
                        let delivered = sender.send(StationMessage::Line(line)).is_ok();
                        ctx.request_repaint();
                        delivered
                    },
                )
                .map_err(|error| format!("{error:#}"));
                let _ = sender.send(StationMessage::Closed(result));
                ctx.request_repaint();
            })?;
        Ok(BleTask {
            port,
            session,
            target,
            production,
            root,
            receiver,
            armed,
            stop,
            attempt: None,
        })
    }

    pub(super) fn disconnect_ble(&mut self) {
        if let Some(mut task) = self.ble.task.take() {
            if !task.stop.load(Ordering::Acquire) {
                self.ble.fail_attempt(&mut task, "已断开工装".into());
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
                    .fail_attempt(&mut task, "工装已拔出或串口身份改变".into());
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
                    StationMessage::Line(line) => self.ble.ingest_line(&mut task, line),
                    StationMessage::Closed(result) => {
                        if !task.stop.load(Ordering::Acquire) {
                            let error = result.err().unwrap_or_else(|| "工装已断开".into());
                            self.ble.fail_attempt(&mut task, error);
                        }
                        closed = true;
                        break;
                    }
                }
            }
            if !closed {
                if task
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.started.elapsed() >= SCAN_TIMEOUT)
                {
                    self.ble.fail_attempt(&mut task, "工装扫描结果超时".into());
                }
                self.ble.task = Some(task);
            }
        }
        self.ble.poll_save();
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

fn result_detail(result: &ResultEvent) -> String {
    let mut details = Vec::new();
    if let Some(reason) = &result.reason {
        details.push(
            match reason.as_str() {
                "expected_target_not_seen" => "未发现目标 BLE 地址",
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
    command: &str,
    armed: &AtomicBool,
    stop: &AtomicBool,
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
    let opened = Instant::now();
    let mut next_write = opened;
    let mut frame = Vec::new();
    let mut chunk = [0_u8; 1_024];
    while !stop.load(Ordering::Acquire) {
        if !armed.load(Ordering::Acquire) {
            let now = Instant::now();
            ensure!(
                now.duration_since(opened) < HANDSHAKE_TIMEOUT,
                "工装没有回应，请检查 ESP32-S3 蓝牙验证固件及 115200 串口连接"
            );
            if now >= next_write {
                serial
                    .write_all(command.as_bytes())
                    .context("无法设置工装的目标地址")?;
                next_write = now + Duration::from_secs(1);
            }
        }
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
                let line = String::from_utf8_lossy(&frame).into_owned();
                frame.clear();
                if !on_line(line) {
                    return Ok(());
                }
            } else {
                if frame.len() == MAX_LINE_BYTES {
                    bail!("工装输出超长串口日志行");
                }
                frame.push(*byte);
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
            session: "a".repeat(64),
            target: Target::new("12:34:56:78:9a:bc", "0123456789ABCDEF0123456789ABCDEF").unwrap(),
            production: ProductionBatch::parse("QA-WO", "2642", "2026-10-17").unwrap(),
            root: root.to_path_buf(),
            receiver,
            armed: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            attempt: None,
        };
        (task, sender)
    }

    fn line(mut payload: Value, task: &BleTask) -> String {
        payload["session"] = json!(task.session);
        payload["mac"] = json!(task.target.mac);
        format!("BLE_VERIFY {payload}")
    }

    fn pass_payload(attempt: u32) -> Value {
        json!({
            "event": "result", "attempt": attempt, "status": "PASS！",
            "observed_mac": "123456789ABC", "name": "Lumysi-1234",
            "firmware": "1.188", "fault_bits": 0, "battery_mv": 661,
        })
    }

    #[test]
    fn pass_requires_current_ack_start_attempt_and_durable_record() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        let mut workspace = BleWorkspace::default();

        let start = line(json!({"event":"start", "attempt":1}), &task);
        let ready = line(json!({"event":"ready"}), &task);
        let result = line(pass_payload(1), &task);
        workspace.ingest_line(&mut task, start.clone());
        workspace.ingest_line(&mut task, result.clone());
        assert!(workspace.latest.is_none() && task.attempt.is_none());
        workspace.ingest_line(&mut task, ready);
        workspace.ingest_line(&mut task, start);
        assert!(task.attempt.is_some());

        workspace.ingest_line(&mut task, "PASS！".into());
        workspace.ingest_line(&mut task, result.replace(&"a".repeat(64), &"b".repeat(64)));
        let wrong_attempt = line(pass_payload(2), &task);
        workspace.ingest_line(&mut task, wrong_attempt);
        assert!(workspace.latest.is_none() && !workspace.pass_open);

        workspace.ingest_line(&mut task, result);
        assert!(workspace.latest.as_ref().unwrap().passed);
        assert!(!workspace.pass_open);
        let saved = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        workspace.finish_save(saved);
        let view = workspace.latest.as_ref().unwrap();
        let saved = fs::read_to_string(view.report.as_ref().unwrap()).unwrap();
        assert!(workspace.pass_open);
        assert!(!workspace.has_unsaved());
        assert!(view.time.ends_with("+08:00"));
        assert!(saved.contains("QA-WO") && saved.contains("Lumysi-1234"));
        assert!(saved.contains("PASS！") && saved.contains("0123456789ABCDEF0123456789ABCDEF"));
    }

    #[test]
    fn a_disconnected_attempt_is_saved_as_failure_and_never_prompts() {
        let root = tempfile::tempdir().unwrap();
        let (mut task, _sender) = sample_task(root.path());
        let mut workspace = BleWorkspace::default();
        let ready = line(json!({"event":"ready"}), &task);
        let start = line(json!({"event":"start", "attempt":1}), &task);
        workspace.ingest_line(&mut task, ready);
        workspace.ingest_line(&mut task, start);
        workspace.ingest_line(&mut task, "raw fixture log entry".into());
        workspace.fail_attempt(&mut task, "工装已拔出".into());
        let saved = workspace
            .save_result
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        workspace.finish_save(saved);
        let view = workspace.latest.as_ref().unwrap();
        let saved = fs::read_to_string(view.report.as_ref().unwrap()).unwrap();
        assert!(!view.passed && !workspace.pass_open);
        assert!(saved.contains("工装已拔出") && saved.contains("raw fixture log entry"));
    }

    #[test]
    fn pass_with_failed_storage_prompts_only_after_successful_retry() {
        let root = tempfile::tempdir().unwrap();
        let unavailable = root.path().join("records");
        fs::write(&unavailable, "not a directory").unwrap();
        let (mut task, _sender) = sample_task(&unavailable);
        let mut workspace = BleWorkspace::default();
        let ready = line(json!({"event":"ready"}), &task);
        let start = line(json!({"event":"start", "attempt":1}), &task);
        let result = line(pass_payload(1), &task);
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
        assert!(!workspace.pass_open && workspace.has_unsaved());
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
        assert!(workspace.pass_open && !workspace.has_unsaved());
        let report = workspace.latest.as_ref().unwrap().report.as_ref().unwrap();
        assert!(report.starts_with(unavailable.join("2642").join("蓝牙验证")));
    }
}
