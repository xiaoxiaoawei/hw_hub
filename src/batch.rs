//! A bounded device queue. Cancellation and invalidation never interrupt active writes.
use crate::{
    backend::{self, FlashProgress, Port},
    firmware::PreparedFirmware,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum JobState {
    Queued,
    Connecting,
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
        ensure!(!ports.is_empty(), "请选择至少一个串口");
        ensure!((1..=16).contains(&concurrency), "并发数必须为 1–16");
        ensure!([115200, 460800, 921600].contains(&baud), "不支持的波特率");
        ensure!(!firmware.segments.is_empty(), "固件分段为空");
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
        let fw = firmware;
        Ok(start_workers(ports, concurrency, move |port, tx| {
            let chip_label = fw.chip.label();
            let mut last_state = JobState::Connecting;
            let mut last_progress = 0.0f32;
            let mut callback = |event| {
                let (state, message, progress) = match event {
                    FlashProgress::Connecting => {
                        (JobState::Connecting, format!("正在连接 {chip_label}"), 0.0)
                    }
                    FlashProgress::Writing { fraction, address } => (
                        JobState::Writing,
                        format!("正在写入 0x{address:X}"),
                        fraction.clamp(0.0, 0.99).max(last_progress),
                    ),
                    FlashProgress::Verifying => (
                        JobState::Verifying,
                        "正在校验 Flash 内容".into(),
                        last_progress,
                    ),
                    FlashProgress::Message(message) => (last_state.clone(), message, last_progress),
                };
                last_state = state.clone();
                last_progress = progress;
                let _ = tx.send(BatchEvent {
                    port: port.into(),
                    state,
                    message,
                    progress,
                });
            };
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

fn start_workers<F>(ports: Vec<String>, concurrency: usize, operation: F) -> Batch
where
    F: Fn(&str, &Sender<BatchEvent>) -> Result<()> + Send + Sync + 'static,
{
    let (tx, receiver) = mpsc::channel();
    for port in &ports {
        let _ = tx.send(BatchEvent {
            port: port.clone(),
            state: JobState::Queued,
            message: "等待烧录".into(),
            progress: 0.0,
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
                        Some((JobState::Cancelled, "已停止排队，未写入".into()))
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
                    }
                } else {
                    let _ = tx.send(BatchEvent {
                        port: port.clone(),
                        state: JobState::Connecting,
                        message: "正在连接".into(),
                        progress: 0.0,
                    });
                    match catch_unwind(AssertUnwindSafe(|| operation(&port, &tx))) {
                        Ok(Ok(())) => BatchEvent {
                            port,
                            state: JobState::Success,
                            message: "写入与校验完成".into(),
                            progress: 1.0,
                        },
                        Ok(Err(error)) => BatchEvent {
                            port,
                            state: JobState::Failed,
                            message: format!("{error:#}"),
                            progress: 0.0,
                        },
                        Err(_) => BatchEvent {
                            port,
                            state: JobState::Failed,
                            message: "烧录库发生异常；请重连设备后重试".into(),
                            progress: 0.0,
                        },
                    }
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
