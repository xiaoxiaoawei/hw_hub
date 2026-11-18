//! A bounded, independent-device queue. Cancellation never interrupts an active write.
use crate::{
    backend::{self, FlashProgress},
    firmware::PreparedFirmware,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::{HashSet, VecDeque},
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

pub struct Batch {
    pub receiver: Receiver<BatchEvent>,
    cancelled: Arc<AtomicBool>,
    remaining: Arc<AtomicUsize>,
}

impl Batch {
    pub fn start(
        ports: Vec<String>,
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
                .map(|p| {
                    if cfg!(windows) {
                        p.strip_prefix("\\\\.\\").unwrap_or(p).to_ascii_uppercase()
                    } else {
                        p.clone()
                    }
                })
                .collect::<HashSet<_>>()
                .len()
                == ports.len(),
            "同一串口不能重复排队"
        );
        let fw = firmware.clone();
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
            backend::flash(port, baud, &fw, &mut callback)
        }))
    }

    pub fn cancel_queued(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
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
    let cancelled = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(Mutex::new(VecDeque::from(ports)));
    let operation = Arc::new(operation);
    for _ in 0..worker_count {
        let (queue, remaining, cancelled, tx, operation) = (
            queue.clone(),
            remaining.clone(),
            cancelled.clone(),
            tx.clone(),
            operation.clone(),
        );
        thread::spawn(move || {
            loop {
                let port = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let Some(port) = port else { break };
                let result = if cancelled.load(Ordering::SeqCst) {
                    BatchEvent {
                        port,
                        state: JobState::Cancelled,
                        message: "已停止排队，未写入".into(),
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
        cancelled,
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
}
