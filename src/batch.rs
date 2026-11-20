//! A bounded device queue. Cancellation and invalidation never interrupt active writes.
use crate::{
    backend::{self, DeviceIdentity, FlashProgress, Port},
    firmware::PreparedFirmware,
    serial_capture::{self, CaptureEnd, CaptureEvent, CaptureOptions},
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
}

impl BatchKind {
    fn label(self) -> &'static str {
        match self {
            Self::Flash => "烧录",
            Self::Read => "读取日志",
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
