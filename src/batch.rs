//! A bounded device queue. Cancellation and invalidation never interrupt active writes.
use crate::{
    backend::{self, DeviceIdentity, FlashProgress, Port},
    firmware::PreparedFirmware,
    serial_capture::{self, CaptureEnd, CaptureEvent, CaptureOptions, ValidationCaptureResult},
    telemetry::{CompiledExtractionRules, TelemetrySettings},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum JobState {
    Queued,
    Connecting,
    Reading,
    Writing,
    Verifying,
    Success,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct BatchEvent {
    pub port: String,
    pub state: JobState,
    pub message: String,
    pub progress: f32,
    pub kind: BatchEventKind,
    /// Per-task hardware metadata. Once read, it is carried through subsequent
    /// progress/log events and the terminal event, including failures.
    pub identity: Option<DeviceIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchEventKind {
    State,
    Progress,
    Log,
    DeviceLog,
    Identity,
    UniqueDeviceId,
}

/// Each claimed task has its own accumulator; neither driver VID/PID nor a
/// different port's progress/identity can merge two physical workstations.
struct JobEvents {
    port: String,
    sender: Sender<BatchEvent>,
    state: JobState,
    progress: f32,
    writing_address: Option<u32>,
    identity: Option<DeviceIdentity>,
}

impl JobEvents {
    fn new(port: &str, sender: &Sender<BatchEvent>) -> Self {
        Self {
            port: port.into(),
            sender: sender.clone(),
            state: JobState::Connecting,
            progress: 0.0,
            writing_address: None,
            identity: None,
        }
    }

    fn snapshot(&self, kind: BatchEventKind, message: String) -> BatchEvent {
        BatchEvent {
            port: self.port.clone(),
            state: self.state.clone(),
            message,
            progress: self.progress,
            kind,
            identity: self.identity.clone(),
        }
    }

    fn send(&self, kind: BatchEventKind, message: String) {
        let _ = self.sender.send(self.snapshot(kind, message));
    }

    fn progress(&mut self, event: FlashProgress, chip_label: &str) {
        let (kind, message) = match event {
            FlashProgress::Connecting => {
                self.state = JobState::Connecting;
                (BatchEventKind::State, format!("正在连接 {chip_label}"))
            }
            FlashProgress::Writing { fraction, address } => {
                let kind =
                    if self.state == JobState::Writing && self.writing_address == Some(address) {
                        BatchEventKind::Progress
                    } else {
                        BatchEventKind::State
                    };
                self.state = JobState::Writing;
                self.writing_address = Some(address);
                self.progress = fraction.clamp(0.0, 0.99).max(self.progress);
                (kind, format!("正在写入 0x{address:X}"))
            }
            FlashProgress::Verifying => {
                self.state = JobState::Verifying;
                (BatchEventKind::State, "正在校验 Flash 内容".into())
            }
            FlashProgress::Message(message) => (BatchEventKind::Log, message),
            FlashProgress::UniqueDeviceId(value) => (BatchEventKind::UniqueDeviceId, value),
            FlashProgress::Identity(identity) => {
                if let Some(previous) = &mut self.identity {
                    if identity.mac.is_some() {
                        previous.mac = identity.mac;
                    }
                    if identity.efuse_id.is_some() {
                        previous.efuse_id = identity.efuse_id;
                    }
                } else {
                    self.identity = Some(identity);
                }
                let identity = self.identity.as_ref().unwrap();
                (
                    BatchEventKind::Identity,
                    format!(
                        "出厂 MAC {}（原始 48 位值 {}）",
                        identity.mac.as_deref().unwrap_or("未提供"),
                        identity.efuse_id.as_deref().unwrap_or("未提供"),
                    ),
                )
            }
        };
        self.send(kind, message);
    }

    fn terminal(&mut self, state: JobState, message: String) -> BatchEvent {
        if state == JobState::Success {
            self.progress = 1.0;
        }
        self.state = state;
        self.snapshot(BatchEventKind::State, message)
    }
}

struct QueueState {
    pending: VecDeque<String>,
    invalidated: HashMap<String, String>,
    cancelled: bool,
}

pub struct Batch {
    pub receiver: Receiver<BatchEvent>,
    queue: Arc<Mutex<QueueState>>,
    remaining: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum BatchKind {
    Flash,
    Read,
    Validation,
}

impl BatchKind {
    fn label(self) -> &'static str {
        match self {
            Self::Flash => "烧录",
            Self::Read => "读取日志",
            Self::Validation => "启动验证",
        }
    }
}

impl Batch {
    /// Legacy name-only entry point. User-facing batch workflows should bind
    /// tasks to selected device identities through start_guarded instead.
    pub fn start(
        ports: Vec<String>,
        baud: u32,
        concurrency: usize,
        firmware: Arc<PreparedFirmware>,
    ) -> Result<Self> {
        Self::start_internal(ports, None, baud, concurrency, firmware)
    }

    pub fn start_guarded(
        ports: Vec<Port>,
        baud: u32,
        concurrency: usize,
        firmware: Arc<PreparedFirmware>,
    ) -> Result<Self> {
        ensure!(
            ports.iter().all(|port| !port.identity.is_empty()),
            "设备身份为空，请重新扫描后选择设备"
        );
        let names = ports.iter().map(|port| port.name.clone()).collect();
        let expected = ports
            .into_iter()
            .map(|port| (backend::port_name_key(&port.name), port))
            .collect();
        Self::start_internal(names, Some(expected), baud, concurrency, firmware)
    }

    fn start_internal(
        ports: Vec<String>,
        expected: Option<HashMap<String, Port>>,
        baud: u32,
        concurrency: usize,
        firmware: Arc<PreparedFirmware>,
    ) -> Result<Self> {
        validate_queue(&ports, concurrency)?;
        ensure!(baud >= 115_200, "烧录波特率必须至少为 115200");
        ensure!(!firmware.segments.is_empty(), "固件分段为空");
        let fw = firmware;
        Ok(start_workers(ports, concurrency, move |port, events| {
            let chip_label = fw.chip.label();
            let mut callback = |event| events.progress(event, chip_label);
            if let Some(expected) = &expected {
                let snapshot = expected
                    .get(&backend::port_name_key(port))
                    .context("本批设备身份快照丢失，已停止任务")?;
                backend::flash_expected(snapshot, baud, &fw, &mut callback)
            } else {
                backend::flash(port, baud, &fw, &mut callback)
            }
        }))
    }

    pub fn start_reading(
        ports: Vec<Port>,
        concurrency: usize,
        options: CaptureOptions,
        settings: &TelemetrySettings,
    ) -> Result<Self> {
        let names: Vec<_> = ports.iter().map(|port| port.name.clone()).collect();
        validate_queue(&names, concurrency)?;
        ensure!(
            !settings.extraction_rules.is_empty(),
            "请先配置用于提取的日志字段"
        );
        let rules = CompiledExtractionRules::new(&settings.extraction_rules)?;
        let headers = settings.custom_headers.clone();
        ensure!(
            ports.iter().all(|port| !port.identity.is_empty()),
            "设备身份为空，请重新扫描后选择设备"
        );
        let expected: HashMap<_, _> = ports
            .into_iter()
            .map(|port| (port.name.clone(), port))
            .collect();
        Ok(start_workers_for(
            names,
            concurrency,
            BatchKind::Read,
            move |port, events| {
                let snapshot = expected.get(port).context("本批设备身份快照丢失")?;
                let mut lines = Vec::new();
                let capture = serial_capture::capture(
                    snapshot,
                    &options,
                    &AtomicBool::new(false),
                    &mut |event| {
                        match event {
                            CaptureEvent::Opened => {
                                events.state = JobState::Reading;
                                events.send(BatchEventKind::State, "正在读取串口日志".into());
                            }
                            CaptureEvent::Resetting => {
                                events.send(BatchEventKind::Log, "正在复位设备".into())
                            }
                            CaptureEvent::Line(line) => {
                                events.send(BatchEventKind::DeviceLog, line.clone());
                                lines.push(line);
                            }
                        }
                        true
                    },
                )?;
                ensure!(
                    capture.end != CaptureEnd::LimitReached,
                    "串口输出超过本次读取的日志上限"
                );
                ensure!(!lines.is_empty(), "未收到串口日志，请检查波特率和设备输出");
                ensure!(
                    !rules
                        .extract_record(&lines, &headers)
                        .custom_fields
                        .is_empty(),
                    "日志中没有匹配到唯一的配置字段"
                );
                Ok(())
            },
        ))
    }

    /// Reset each frozen device into normal firmware boot and collect its raw
    /// console for at most ten seconds. Complete passing fresh-boot data plus
    /// 700 ms of quiet may finish earlier. Success means console transport;
    /// the application evaluates its verification criteria independently.
    /// No firmware, extraction regex, eFuse probe or flash operation is used.
    pub fn start_validation(ports: Vec<Port>, baud: u32, concurrency: usize) -> Result<Self> {
        start_validation_with_capture(
            ports,
            baud,
            concurrency,
            serial_capture::capture_validation_startup_log,
        )
    }

    pub fn cancel_queued(&self) {
        self.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelled = true;
    }

    /// Permanently reject a not-yet-started task after a successful scan
    /// observes its selected device disappear or change identity. Reconnection
    /// cannot restore this batch's eligibility. Active writes are untouched.
    pub fn invalidate_queued_port(&self, port: &str, reason: impl Into<String>) {
        let key = backend::port_name_key(port);
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        if queue
            .pending
            .iter()
            .any(|pending| backend::port_name_key(pending) == key)
        {
            queue
                .invalidated
                .entry(key)
                .or_insert_with(|| reason.into());
        }
    }

    pub fn is_running(&self) -> bool {
        self.remaining.load(Ordering::SeqCst) > 0
    }
}

fn start_validation_with_capture(
    ports: Vec<Port>,
    baud: u32,
    concurrency: usize,
    capture: impl Fn(
        &Port,
        u32,
        &AtomicBool,
        &mut dyn FnMut(CaptureEvent) -> bool,
    ) -> Result<ValidationCaptureResult>
    + Send
    + Sync
    + 'static,
) -> Result<Batch> {
    let names: Vec<_> = ports.iter().map(|port| port.name.clone()).collect();
    validate_queue(&names, concurrency)?;
    ensure!(baud > 0, "启动日志波特率必须大于 0");
    ensure!(
        ports.iter().all(|port| !port.identity.trim().is_empty()),
        "设备身份为空，请重新扫描后选择设备"
    );
    let expected: HashMap<_, _> = ports
        .into_iter()
        .map(|port| (backend::port_name_key(&port.name), port))
        .collect();
    Ok(start_workers_for(
        names,
        concurrency,
        BatchKind::Validation,
        move |port, events| {
            let snapshot = expected
                .get(&backend::port_name_key(port))
                .context("本批启动验证设备身份快照丢失")?;
            let mut lines_received = 0usize;
            let ValidationCaptureResult {
                summary: result,
                early_verified,
            } = capture(snapshot, baud, &AtomicBool::new(false), &mut |event| {
                match event {
                    CaptureEvent::Opened => {
                        events.state = JobState::Reading;
                        events.send(BatchEventKind::State, "已连接，准备采集启动日志".into());
                    }
                    CaptureEvent::Resetting => {
                        events.send(BatchEventKind::Log, "正在正常启动复位".into())
                    }
                    CaptureEvent::Line(line) => {
                        lines_received += 1;
                        events.send(BatchEventKind::DeviceLog, line);
                    }
                }
                true
            })?;
            // Only the low-level fresh-marker + complete passing data + quiet
            // path can certify an early stop. Arbitrary callback stops remain
            // incomplete. Already forwarded lines stay with this COM.
            match result.end {
                CaptureEnd::TimedOut => {}
                CaptureEnd::Stopped if early_verified => {
                    events.send(
                        BatchEventKind::Log,
                        "完整启动数据已收齐并静默 700ms，提前结束采集".into(),
                    );
                }
                CaptureEnd::LimitReached => {
                    anyhow::bail!("启动日志超过本次读取上限，采集结果不完整")
                }
                CaptureEnd::Cancelled => anyhow::bail!("启动日志读取已取消，采集结果不完整"),
                CaptureEnd::Stopped => anyhow::bail!("启动日志提前停止，采集结果不完整"),
            }
            ensure!(
                result.bytes_read > 0 && lines_received > 0,
                "未收到启动日志，请检查波特率、复位电路和固件输出通道"
            );
            Ok(())
        },
    ))
}

fn validate_queue(ports: &[String], concurrency: usize) -> Result<()> {
    ensure!(!ports.is_empty(), "请选择至少一个串口");
    ensure!((1..=16).contains(&concurrency), "并发数必须为 1–16");
    ensure!(
        ports.iter().all(|p| !p.trim().is_empty()),
        "串口名称不能为空"
    );
    ensure!(
        ports
            .iter()
            .map(|port| backend::port_name_key(port))
            .collect::<HashSet<_>>()
            .len()
            == ports.len(),
        "同一串口不能重复排队"
    );
    Ok(())
}

fn start_workers<F>(ports: Vec<String>, concurrency: usize, operation: F) -> Batch
where
    F: Fn(&str, &mut JobEvents) -> Result<()> + Send + Sync + 'static,
{
    start_workers_for(ports, concurrency, BatchKind::Flash, operation)
}

fn start_workers_for<F>(
    ports: Vec<String>,
    concurrency: usize,
    kind: BatchKind,
    operation: F,
) -> Batch
where
    F: Fn(&str, &mut JobEvents) -> Result<()> + Send + Sync + 'static,
{
    let (tx, receiver) = mpsc::channel();
    for port in &ports {
        let _ = tx.send(BatchEvent {
            port: port.clone(),
            state: JobState::Queued,
            message: format!("等待{}", kind.label()),
            progress: 0.0,
            kind: BatchEventKind::State,
            identity: None,
        });
    }
    let worker_count = concurrency.min(ports.len());
    let remaining = Arc::new(AtomicUsize::new(ports.len()));
    let queue = Arc::new(Mutex::new(QueueState {
        pending: VecDeque::from(ports),
        invalidated: HashMap::new(),
        cancelled: false,
    }));
    let operation = Arc::new(operation);
    for _ in 0..worker_count {
        let (queue, remaining, tx, operation) = (
            queue.clone(),
            remaining.clone(),
            tx.clone(),
            operation.clone(),
        );
        thread::spawn(move || {
            loop {
                // Claiming a task as active and reading its cancellation/identity
                // guards share one lock with GUI invalidation. No invalidation can
                // race between a pending-task check and its active transition.
                let (port, rejection) = {
                    let mut queue = queue.lock().unwrap_or_else(|error| error.into_inner());
                    let Some(port) = queue.pending.pop_front() else {
                        break;
                    };
                    let key = backend::port_name_key(&port);
                    let rejection = if let Some(reason) = queue.invalidated.get(&key) {
                        Some((JobState::Failed, reason.clone()))
                    } else if queue.cancelled {
                        let message = match kind {
                            BatchKind::Flash => "已停止排队，未写入",
                            BatchKind::Read => "已停止排队，未读取日志",
                            BatchKind::Validation => "已停止排队，未复位或读取启动日志",
                        };
                        Some((JobState::Cancelled, message.into()))
                    } else {
                        None
                    };
                    (port, rejection)
                };
                let result = if let Some((state, message)) = rejection {
                    BatchEvent {
                        port,
                        state,
                        message,
                        progress: 0.0,
                        kind: BatchEventKind::State,
                        identity: None,
                    }
                } else {
                    let mut events = JobEvents::new(&port, &tx);
                    events.send(BatchEventKind::State, "正在连接".into());
                    let (state, message) =
                        match catch_unwind(AssertUnwindSafe(|| operation(&port, &mut events))) {
                            Ok(Ok(())) => (
                                JobState::Success,
                                match kind {
                                    BatchKind::Flash => "写入与校验完成",
                                    BatchKind::Read => "日志读取完成，已提取配置字段",
                                    BatchKind::Validation => "启动日志采集完成，待规则判定",
                                }
                                .into(),
                            ),
                            Ok(Err(error)) => (JobState::Failed, format!("{error:#}")),
                            Err(_) => (
                                JobState::Failed,
                                format!("{}任务发生异常，请重连设备后重试", kind.label()),
                            ),
                        };
                    events.terminal(state, message)
                };
                let _ = tx.send(result);
                remaining.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }
    Batch {
        receiver,
        queue,
        remaining,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, time::Duration};

    fn terminal(state: &JobState) -> bool {
        matches!(
            state,
            JobState::Success | JobState::Failed | JobState::Cancelled
        )
    }

    fn identity(port: &str) -> DeviceIdentity {
        let suffix = if port == "COM1" { "01" } else { "02" };
        DeviceIdentity {
            mac: Some(format!("AA:BB:CC:DD:EE:{suffix}")),
            efuse_id: Some(format!("AABBCCDDEE{suffix}")),
        }
    }

    fn unique_id(port: &str) -> &'static str {
        if port == "COM1" {
            "1234567890ABCDEF0123456789ABCDEF"
        } else {
            "FEDCBA98765432100FEDCBA987654321"
        }
    }

    fn validation_port(name: &str) -> Port {
        Port {
            name: name.into(),
            identity: format!("frozen-physical-{name}"),
            description: "same USB driver".into(),
            usb: true,
            suggested: false,
        }
    }

    fn capture_summary(end: CaptureEnd, bytes: usize, lines: usize) -> ValidationCaptureResult {
        ValidationCaptureResult {
            summary: serial_capture::CaptureSummary {
                bytes_read: bytes,
                decoded_bytes: bytes,
                lines,
                elapsed: Duration::from_secs(10),
                end,
                limit: (end == CaptureEnd::LimitReached)
                    .then_some(serial_capture::CaptureLimit::TotalBytes),
            },
            early_verified: false,
        }
    }

    #[test]
    fn validation_rejects_bad_queue_baud_or_identity_before_invoking_capture() {
        let valid = validation_port("COM1");
        let mut missing_identity = valid.clone();
        missing_identity.identity = " ".into();
        for (ports, baud, concurrency) in [
            (vec![], 115_200, 1),
            (vec![valid.clone(), valid.clone()], 115_200, 1),
            (vec![valid.clone()], 0, 1),
            (vec![valid.clone()], 115_200, 0),
            (vec![valid], 115_200, 17),
            (vec![missing_identity], 115_200, 1),
        ] {
            assert!(
                start_validation_with_capture(ports, baud, concurrency, |_, _, _, _| panic!(
                    "invalid input must never capture/reset"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn validation_accepts_only_certified_quiet_completion_and_preserves_all_raw_lines() {
        let batch = start_validation_with_capture(
            ["COM1", "COM2", "COM3"]
                .into_iter()
                .map(validation_port)
                .collect(),
            115_200,
            2,
            |snapshot, _, _, callback| {
                callback(CaptureEvent::Opened);
                callback(CaptureEvent::Resetting);
                let lines = [
                    "ESP-ROM:esp32c3-api1-20210207",
                    "MAX30105 detected & working!",
                    "BMI270 detected & working!",
                    "MAC ADDRESS: 00:11:22:AA:BB:CC",
                    "UNIQUE DEVICE ID: 00112233445566778899AABBCCDDEEFF",
                    "Analog Read in millivolts at battery pin: 2922",
                    "last diagnostic",
                ];
                for line in lines {
                    assert!(callback(CaptureEvent::Line(line.into())));
                }
                let end = if snapshot.name == "COM3" {
                    CaptureEnd::LimitReached
                } else {
                    CaptureEnd::Stopped
                };
                let mut result = capture_summary(
                    end,
                    lines.iter().map(|line| line.len() + 1).sum(),
                    lines.len(),
                );
                result.summary.elapsed = Duration::from_millis(701);
                result.early_verified = snapshot.name != "COM2";
                Ok(result)
            },
        )
        .unwrap();
        let mut finals = HashMap::new();
        let mut raw: HashMap<String, Vec<String>> = HashMap::new();
        let mut quiet_logs = Vec::new();
        while finals.len() < 3 {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if event.kind == BatchEventKind::DeviceLog {
                raw.entry(event.port.clone())
                    .or_default()
                    .push(event.message.clone());
            }
            if event.kind == BatchEventKind::Log && event.message.contains("700ms") {
                quiet_logs.push(event.port.clone());
            }
            if terminal(&event.state) {
                finals.insert(event.port.clone(), event);
            }
        }
        assert_eq!(finals["COM1"].state, JobState::Success);
        assert_eq!(
            finals["COM2"].state,
            JobState::Failed,
            "uncertified early stops remain incomplete"
        );
        assert_eq!(
            finals["COM3"].state,
            JobState::Failed,
            "a completion flag never waives limits"
        );
        assert_eq!(quiet_logs, ["COM1"]);
        for lines in raw.values() {
            assert_eq!(lines.len(), 7);
            assert_eq!(lines.last().unwrap(), "last diagnostic");
        }
        assert_eq!(raw.len(), 3);
    }

    #[test]
    fn validation_streams_each_com_without_regex_and_only_complete_console_transport_succeeds() {
        let names = ["COM1", "COM2", "COM3", "COM4", "COM5", "COM6"];
        let ports = names.iter().map(|name| validation_port(name)).collect();
        let batch =
            start_validation_with_capture(ports, 115_200, 2, |snapshot, baud, cancel, callback| {
                assert_eq!(
                    snapshot.identity,
                    format!("frozen-physical-{}", snapshot.name)
                );
                assert_eq!(baud, 115_200);
                assert!(!cancel.load(Ordering::Acquire));
                assert!(callback(CaptureEvent::Opened));
                assert!(callback(CaptureEvent::Resetting));
                if snapshot.name == "COM6" {
                    return Ok(capture_summary(CaptureEnd::TimedOut, 0, 0));
                }
                // Deliberately contains FAIL: transport must not apply product
                // verification rules or require configured regex/firmware here.
                assert!(callback(CaptureEvent::Line(format!(
                    "{} SELF_TEST=FAIL",
                    snapshot.name
                ))));
                let end = match snapshot.name.as_str() {
                    "COM1" => CaptureEnd::TimedOut,
                    "COM2" => CaptureEnd::LimitReached,
                    "COM3" => CaptureEnd::Stopped,
                    "COM4" => CaptureEnd::Cancelled,
                    _ => anyhow::bail!("synthetic serial disconnect after raw output"),
                };
                Ok(capture_summary(end, 23, 1))
            })
            .unwrap();
        let mut events = Vec::new();
        let mut finals = HashMap::new();
        while finals.len() < names.len() {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if terminal(&event.state) {
                finals.insert(event.port.clone(), event.clone());
            }
            events.push(event);
        }
        assert_eq!(finals["COM1"].state, JobState::Success);
        assert_eq!(finals["COM1"].message, "启动日志采集完成，待规则判定");
        for name in names {
            let own: Vec<_> = events.iter().filter(|event| event.port == name).collect();
            assert!(own.iter().any(|event| event.state == JobState::Queued && event.message == "等待启动验证"));
            assert!(own.iter().any(
                |event| event.kind == BatchEventKind::State && event.state == JobState::Reading
            ));
            assert!(
                own.iter().any(|event| event.kind == BatchEventKind::Log
                    && event.message == "正在正常启动复位")
            );
            if name != "COM1" {
                assert_eq!(finals[name].state, JobState::Failed);
            }
            let raw: Vec<_> = own
                .iter()
                .filter(|event| event.kind == BatchEventKind::DeviceLog)
                .collect();
            if name == "COM6" {
                assert!(raw.is_empty());
                assert!(finals[name].message.contains("未收到启动日志"));
            } else {
                assert_eq!(raw.len(), 1);
                assert_eq!(raw[0].message, format!("{name} SELF_TEST=FAIL"));
            }
            assert!(
                own.iter().all(|event| event.identity.is_none()
                    && event.kind != BatchEventKind::UniqueDeviceId)
            );
        }
        assert!(
            finals["COM5"]
                .message
                .contains("synthetic serial disconnect")
        );
    }

    #[test]
    fn validation_invalidation_and_queue_stop_never_reset_unclaimed_devices() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let gate = Arc::new(Barrier::new(2));
        let worker_gate = gate.clone();
        let invoked = Arc::new(Mutex::new(Vec::new()));
        let calls = invoked.clone();
        let batch = start_validation_with_capture(
            ["COM1", "COM2", "COM3"]
                .into_iter()
                .map(validation_port)
                .collect(),
            115_200,
            1,
            move |snapshot, _, cancel, callback| {
                calls.lock().unwrap().push(snapshot.name.clone());
                entered_tx.send(()).unwrap();
                worker_gate.wait();
                assert!(
                    !cancel.load(Ordering::Acquire),
                    "queue stop must not interrupt the active capture"
                );
                callback(CaptureEvent::Opened);
                callback(CaptureEvent::Line("boot complete".into()));
                Ok(capture_summary(CaptureEnd::TimedOut, 14, 1))
            },
        )
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        batch.invalidate_queued_port("COM1", "active capture must finish");
        batch.invalidate_queued_port("COM2", "frozen device disappeared before validation");
        batch.cancel_queued();
        gate.wait();
        let mut finals = HashMap::new();
        while finals.len() < 3 {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if terminal(&event.state) {
                finals.insert(event.port.clone(), event);
            }
        }
        assert_eq!(*invoked.lock().unwrap(), ["COM1"]);
        assert_eq!(finals["COM1"].state, JobState::Success);
        assert_eq!(finals["COM2"].state, JobState::Failed);
        assert_eq!(
            finals["COM2"].message,
            "frozen device disappeared before validation"
        );
        assert_eq!(finals["COM3"].state, JobState::Cancelled);
        assert_eq!(finals["COM3"].message, "已停止排队，未复位或读取启动日志");
    }

    #[test]
    fn independent_device_identity_survives_logs_progress_and_terminal_results() {
        let batch = start_workers(vec!["COM1".into(), "COM2".into()], 2, |port, events| {
            events.progress(FlashProgress::Identity(identity(port)), "ESP32-C3");
            events.progress(
                FlashProgress::UniqueDeviceId(unique_id(port).into()),
                "ESP32-C3",
            );
            events.progress(
                FlashProgress::Message("设备连接、芯片识别完成".into()),
                "ESP32-C3",
            );
            events.progress(
                FlashProgress::Writing {
                    fraction: 0.25,
                    address: 0,
                },
                "ESP32-C3",
            );
            events.progress(
                FlashProgress::Writing {
                    fraction: 0.5,
                    address: 0,
                },
                "ESP32-C3",
            );
            events.progress(FlashProgress::Verifying, "ESP32-C3");
            if port == "COM2" {
                anyhow::bail!("mock per-device verify failure")
            }
            Ok(())
        });
        let mut all = Vec::new();
        let mut finished = HashSet::new();
        while finished.len() < 2 {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if terminal(&event.state) {
                finished.insert(event.port.clone());
            }
            all.push(event);
        }
        for port in ["COM1", "COM2"] {
            let events: Vec<_> = all.iter().filter(|event| event.port == port).collect();
            let identity_event = events
                .iter()
                .position(|event| event.kind == BatchEventKind::Identity)
                .unwrap();
            for event in &events[identity_event..] {
                assert_eq!(event.identity, Some(identity(port)));
            }
            let unique_events: Vec<_> = events
                .iter()
                .filter(|event| event.kind == BatchEventKind::UniqueDeviceId)
                .map(|event| event.message.as_str())
                .collect();
            assert_eq!(unique_events, [unique_id(port)]);
            assert!(events.iter().any(|event| event.kind == BatchEventKind::Log));
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == BatchEventKind::Progress && event.progress == 0.5)
            );
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == BatchEventKind::State
                        && event.state == JobState::Verifying)
            );
            let last = events.last().unwrap();
            assert_eq!(last.kind, BatchEventKind::State);
            if port == "COM1" {
                assert_eq!(last.state, JobState::Success);
                assert_eq!(last.progress, 1.0);
            } else {
                assert_eq!(last.state, JobState::Failed);
                assert_eq!(last.progress, 0.5);
                assert!(last.message.contains("mock per-device verify failure"));
            }
        }
    }

    #[test]
    fn one_workers_next_task_never_reuses_the_previous_devices_identity() {
        let batch = start_workers(vec!["COM1".into(), "COM2".into()], 1, |port, events| {
            if port == "COM1" {
                events.progress(FlashProgress::Identity(identity(port)), "ESP32-C3");
            }
            events.progress(FlashProgress::Message("独立设备日志".into()), "ESP32-C3");
            Ok(())
        });
        let mut finals = HashMap::new();
        while finals.len() < 2 {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if event.port == "COM2" {
                assert!(event.identity.is_none());
            }
            if terminal(&event.state) {
                finals.insert(event.port.clone(), event);
            }
        }
        assert_eq!(finals["COM1"].identity, Some(identity("COM1")));
        assert!(finals["COM2"].identity.is_none());
    }

    #[test]
    fn partial_identity_and_failed_progress_are_retained_without_inventing_values() {
        let (sender, receiver) = mpsc::channel();
        let mut events = JobEvents::new("COM1", &sender);
        events.progress(
            FlashProgress::Identity(DeviceIdentity {
                mac: Some("AA:BB:CC:DD:EE:01".into()),
                efuse_id: None,
            }),
            "ESP32-C3",
        );
        events.progress(
            FlashProgress::Identity(DeviceIdentity {
                mac: None,
                efuse_id: Some("AABBCCDDEE01".into()),
            }),
            "ESP32-C3",
        );
        events.progress(
            FlashProgress::Writing {
                fraction: 0.8,
                address: 0x10000,
            },
            "ESP32-C3",
        );
        events.progress(
            FlashProgress::Writing {
                fraction: 0.2,
                address: 0x10000,
            },
            "ESP32-C3",
        );
        let final_event = events.terminal(JobState::Failed, "mock disconnect".into());
        assert_eq!(final_event.identity, Some(identity("COM1")));
        assert_eq!(final_event.progress, 0.8);
        let first = receiver.try_recv().unwrap();
        assert!(first.identity.unwrap().efuse_id.is_none());
    }

    #[test]
    fn writing_segment_changes_are_detailed_states_without_logging_each_packet() {
        let (sender, receiver) = mpsc::channel();
        let mut events = JobEvents::new("COM1", &sender);
        for (fraction, address) in [(0.1, 0), (0.2, 0), (0.3, 0x8000)] {
            events.progress(FlashProgress::Writing { fraction, address }, "ESP32-C3");
        }
        let kinds: Vec<_> = receiver.try_iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            [
                BatchEventKind::State,
                BatchEventKind::Progress,
                BatchEventKind::State
            ]
        );
    }

    #[test]
    fn bounded_parallelism_and_independent_failures() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (a, p) = (active.clone(), peak.clone());
        let barrier = Arc::new(Barrier::new(2));
        let batch = start_workers(
            vec!["COM1".into(), "COM2".into(), "COM3".into(), "COM4".into()],
            2,
            move |port, _| {
                let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(n, Ordering::SeqCst);
                barrier.wait();
                a.fetch_sub(1, Ordering::SeqCst);
                if port == "COM2" {
                    anyhow::bail!("mock device unplugged")
                }
                Ok(())
            },
        );
        let events: Vec<_> = (0..4)
            .map(|_| {
                loop {
                    let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
                    if terminal(&event.state) {
                        break event;
                    }
                }
            })
            .collect();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(
            events
                .iter()
                .filter(|e| e.state == JobState::Success)
                .count(),
            3
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.state == JobState::Failed)
                .count(),
            1
        );
    }

    #[test]
    fn cancellation_finishes_active_and_skips_remaining() {
        let (started_tx, started_rx) = mpsc::channel();
        let barrier = Arc::new(Barrier::new(2));
        let gate = barrier.clone();
        let batch = start_workers(
            vec!["COM1".into(), "COM2".into(), "COM3".into()],
            1,
            move |_, _| {
                started_tx.send(()).unwrap();
                gate.wait();
                Ok(())
            },
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        batch.cancel_queued();
        barrier.wait();
        let mut states = Vec::new();
        while states.len() < 3 {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if terminal(&event.state) {
                states.push(event.state)
            }
        }
        assert_eq!(
            states,
            [JobState::Success, JobState::Cancelled, JobState::Cancelled]
        );
    }

    #[test]
    fn invalidated_queued_device_never_opens_but_active_and_other_jobs_finish() {
        let (started_tx, started_rx) = mpsc::channel();
        let barrier = Arc::new(Barrier::new(2));
        let gate = barrier.clone();
        let invoked = Arc::new(Mutex::new(Vec::new()));
        let calls = invoked.clone();
        let batch = start_workers(
            vec!["COM1".into(), "COM2".into(), "COM3".into()],
            1,
            move |port, _| {
                calls.lock().unwrap().push(port.to_owned());
                if port == "COM1" {
                    started_tx.send(()).unwrap();
                    gate.wait();
                }
                Ok(())
            },
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        batch.invalidate_queued_port("COM1", "active write must not be interrupted");
        batch.invalidate_queued_port("COM2", "selected device disconnected before start");
        // A later observation/reconnection cannot overwrite or restore this
        // batch's first invalidation. A new batch and explicit selection are required.
        batch.invalidate_queued_port("COM2", "later observation");
        barrier.wait();
        let mut results = HashMap::new();
        while results.len() < 3 {
            let event = batch.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if terminal(&event.state) {
                results.insert(event.port.clone(), event);
            }
        }
        assert_eq!(results["COM1"].state, JobState::Success);
        assert_eq!(results["COM2"].state, JobState::Failed);
        assert_eq!(
            results["COM2"].message,
            "selected device disconnected before start"
        );
        assert_eq!(results["COM3"].state, JobState::Success);
        assert_eq!(*invoked.lock().unwrap(), ["COM1", "COM3"]);
    }
}
