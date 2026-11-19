//! Non-blocking serial discovery shared by manual and automatic UI refreshes.
//!
//! The UI schedules requests, consumes snapshots and reconciles selection.
//! A failed discovery remains an error; it is never an empty port snapshot.
use crate::backend::{self, Port};
use anyhow::{Result, anyhow};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
};

type ScanResult = Result<Vec<Port>>;
pub struct DeviceScanner {
    requests: SyncSender<()>,
    receiver: Receiver<ScanResult>,
    startup_error: Option<String>,
    scanning: bool,
}

impl DeviceScanner {
    /// Starts one persistent background worker; no device is enumerated until
    /// requested. Native enumeration and subsequent thread creation never run
    /// on the UI path when a USB/COM burst is received.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::with_operation(wake, backend::list_ports)
    }

    fn with_operation(
        wake: impl Fn() + Send + Sync + 'static,
        operation: impl Fn() -> ScanResult + Send + Sync + 'static,
    ) -> Self {
        let (requests, request_receiver) = mpsc::sync_channel(1);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("flashtool-port-scan".into())
            .spawn(move || {
                while request_receiver.recv().is_ok() {
                    let result = catch_unwind(AssertUnwindSafe(&operation))
                        .unwrap_or_else(|_| Err(anyhow!("串口扫描出现异常，请稍后重试")));
                    // Closing the UI drops its receiver and request sender.
                    // Never join a possibly slow OS enumeration from Drop.
                    if sender.send(result).is_err() {
                        break;
                    }
                    let _ = catch_unwind(AssertUnwindSafe(&wake));
                }
            });
        Self {
            requests,
            receiver,
            startup_error: worker
                .err()
                .map(|error| format!("无法启动串口扫描线程：{error}")),
            scanning: false,
        }
    }

    /// Only a bounded non-blocking send: automatic and manual refreshes share
    /// the same worker. A result must be consumed before another request is
    /// accepted, so there is never an overlapping scan or a queued scan storm.
    pub fn request_scan(&mut self) -> bool {
        if self.scanning {
            return false;
        }
        self.scanning = true;
        match self.requests.try_send(()) {
            Ok(()) => {}
            Err(TrySendError::Full(())) => return false,
            Err(TrySendError::Disconnected(())) => {
                self.startup_error
                    .get_or_insert_with(|| "串口扫描线程已断开，请重启程序".into());
            }
        }
        true
    }

    /// Non-blocking receipt. Preserve the UI's previous list on Err; only an
    /// Ok snapshot (including a genuinely empty one) replaces live discovery.
    pub fn poll(&mut self) -> Option<ScanResult> {
        if self.scanning
            && let Some(error) = &self.startup_error
        {
            self.scanning = false;
            return Some(Err(anyhow!(error.clone())));
        }
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
            Arc, Mutex,
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
    fn successive_scans_reuse_one_worker_even_after_a_wake_panics() {
        let (thread_tx, thread_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let wakes = AtomicUsize::new(0);
        let mut scanner = DeviceScanner::with_operation(
            move || {
                wake_tx.send(()).unwrap();
                if wakes.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("mock UI wake panic");
                }
            },
            move || {
                thread_tx.send(thread::current().id())?;
                Ok(vec![port("COM7")])
            },
        );
        let mut threads = Vec::new();
        for _ in 0..2 {
            assert!(scanner.request_scan());
            for _ in 0..100 {
                assert!(!scanner.request_scan());
            }
            wake_rx.recv_timeout(DEADLINE).unwrap();
            assert_eq!(scanner.poll().unwrap().unwrap()[0].name, "COM7");
            threads.push(thread_rx.recv_timeout(DEADLINE).unwrap());
        }
        assert_eq!(threads[0], threads[1]);
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
