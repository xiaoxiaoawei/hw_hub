//! Non-blocking serial discovery shared by manual and automatic UI refreshes.
//!
//! The UI schedules requests, consumes snapshots and reconciles selection.
//! A failed discovery remains an error; it is never an empty port snapshot.
use crate::backend::{self, Port};
use anyhow::{Result, anyhow};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
};

type ScanResult = Result<Vec<Port>>;
type ScanOperation = dyn Fn() -> ScanResult + Send + Sync;
type Wake = dyn Fn() + Send + Sync;

pub struct DeviceScanner {
    sender: Sender<ScanResult>,
    receiver: Receiver<ScanResult>,
    operation: Arc<ScanOperation>,
    wake: Arc<Wake>,
    scanning: bool,
}

impl DeviceScanner {
    /// `wake` is called on the worker after a result is available, e.g. to
    /// request an egui repaint. Construction does not enumerate any devices.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::with_operation(wake, backend::list_ports)
    }

    fn with_operation(
        wake: impl Fn() + Send + Sync + 'static,
        operation: impl Fn() -> ScanResult + Send + Sync + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            operation: Arc::new(operation),
            wake: Arc::new(wake),
            scanning: false,
        }
    }

    /// Starts a background scan and returns immediately. Automatic and manual
    /// refreshes use this same entry point. Returns false while a prior request
    /// is running OR its result has not yet been consumed with `poll`.
    pub fn request_scan(&mut self) -> bool {
        if self.scanning {
            return false;
        }
        self.scanning = true;
        let sender = self.sender.clone();
        let operation = self.operation.clone();
        let wake = self.wake.clone();
        let worker = thread::Builder::new()
            .name("flashtool-port-scan".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| operation()))
                    .unwrap_or_else(|_| Err(anyhow!("串口扫描出现异常，请稍后重试")));
                // A closed receiver means the window/scanner has been dropped.
                // Do not wake a disposed UI, and do not wait for it to close.
                if sender.send(result).is_ok() {
                    wake();
                }
            });
        if let Err(error) = worker {
            // Keep failure reporting identical to an ordinary worker result.
            // The pending result also prevents another overlapping request.
            let _ = self
                .sender
                .send(Err(anyhow!("无法启动串口扫描线程：{error}")));
            (self.wake)();
        }
        // Dropping JoinHandle detaches the worker. No UI operation joins it;
        // dropping DeviceScanner never waits for a slow OS enumeration.
        true
    }

    /// Non-blocking receipt. Preserve the UI's previous list on Err; only an
    /// Ok snapshot (including a genuinely empty one) replaces live discovery.
    pub fn poll(&mut self) -> Option<ScanResult> {
        match self.receiver.try_recv() {
            Ok(result) => {
                self.scanning = false;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) if self.scanning => {
                self.scanning = false;
                Some(Err(anyhow!("串口扫描线程已断开，请重试")))
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Includes a completed result that the UI has not consumed yet.
    pub fn is_scanning(&self) -> bool {
        self.scanning
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    const DEADLINE: Duration = Duration::from_secs(3);

    fn port(name: &str) -> Port {
        Port {
            name: name.into(),
            identity: format!("test-usb:{name}:CH343"),
            description: "CH343 · USB 1A86:55D4".into(),
            usb: true,
            suggested: true,
        }
    }

    #[test]
    fn requests_are_non_blocking_and_coalesced_until_result_is_polled() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let (owner_tx, owner_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (call_count, active_count, peak_count) = (calls.clone(), active.clone(), peak.clone());
        // A separate owner lets the timeout detect a regression that executes
        // the blocking operation inside request_scan instead of a worker.
        thread::spawn(move || {
            let mut scanner = DeviceScanner::with_operation(
                move || {
                    let _ = wake_tx.send(());
                },
                move || {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    let count = active_count.fetch_add(1, Ordering::SeqCst) + 1;
                    peak_count.fetch_max(count, Ordering::SeqCst);
                    let _ = entered_tx.send(());
                    release_rx.lock().unwrap().recv()?;
                    active_count.fetch_sub(1, Ordering::SeqCst);
                    Ok(vec![port("COM3")])
                },
            );
            let accepted = scanner.request_scan();
            let _ = owner_tx.send((scanner, accepted));
        });
        let (mut scanner, accepted) = owner_rx
            .recv_timeout(DEADLINE)
            .expect("request must return before discovery finishes");
        assert!(accepted);
        entered_rx.recv_timeout(DEADLINE).unwrap();
        assert!(scanner.is_scanning());
        assert!(scanner.poll().is_none());
        assert!(!scanner.request_scan());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        wake_rx.recv_timeout(DEADLINE).unwrap();
        // Wake occurs after sending the snapshot, but a completed unconsumed
        // request still coalesces another manual/automatic refresh.
        assert!(!scanner.request_scan());
        let ports = scanner.poll().unwrap().unwrap();
        assert_eq!(ports[0].name, "COM3");
        assert!(!scanner.is_scanning());
        assert!(scanner.poll().is_none());
        assert!(scanner.request_scan());
        entered_rx.recv_timeout(DEADLINE).unwrap();
        assert!(!scanner.request_scan());
        release_tx.send(()).unwrap();
        wake_rx.recv_timeout(DEADLINE).unwrap();
        scanner.poll().unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn discovery_failure_remains_an_error_and_does_not_poison_the_next_request() {
        let calls = AtomicUsize::new(0);
        let (wake_tx, wake_rx) = mpsc::channel();
        let mut scanner = DeviceScanner::with_operation(
            move || {
                let _ = wake_tx.send(());
            },
            move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("mock OS enumeration failure")
                }
                Ok(vec![port("COM10")])
            },
        );
        assert!(scanner.request_scan());
        wake_rx.recv_timeout(DEADLINE).unwrap();
        let error = scanner.poll().unwrap().unwrap_err();
        assert!(error.to_string().contains("mock OS enumeration failure"));
        assert!(!scanner.is_scanning());
        assert!(scanner.request_scan());
        wake_rx.recv_timeout(DEADLINE).unwrap();
        assert_eq!(scanner.poll().unwrap().unwrap()[0].name, "COM10");
    }

    #[test]
    fn a_panicking_discovery_reports_failure_instead_of_leaving_refresh_stuck() {
        let (wake_tx, wake_rx) = mpsc::channel();
        let mut scanner = DeviceScanner::with_operation(
            move || {
                let _ = wake_tx.send(());
            },
            || -> ScanResult { panic!("mock driver library panic") },
        );
        scanner.request_scan();
        wake_rx.recv_timeout(DEADLINE).unwrap();
        assert!(scanner.poll().unwrap().is_err());
        assert!(!scanner.is_scanning());
    }

    #[test]
    fn dropping_scanner_does_not_wait_for_os_enumeration_or_wake_a_disposed_ui() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let mut scanner = DeviceScanner::with_operation(
            move || {
                let _ = wake_tx.send(());
            },
            move || {
                entered_tx.send(())?;
                release_rx.lock().unwrap().recv()?;
                Ok(Vec::new())
            },
        );
        scanner.request_scan();
        entered_rx.recv_timeout(DEADLINE).unwrap();
        thread::spawn(move || {
            drop(scanner);
            let _ = done_tx.send(());
        });
        let dropped = done_rx.recv_timeout(DEADLINE);
        // Always release the worker even if this assertion will fail.
        let _ = release_tx.send(());
        dropped.expect("scanner drop must not join a blocked discovery");
        // Worker completion drops the last wake sender without invoking it.
        assert!(matches!(
            wake_rx.recv_timeout(DEADLINE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
