use super::*;
use esp32c3_flashtool::serial_capture::{self, CaptureEnd, CaptureEvent, CaptureSummary};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct StartupLogTask {
    key: DeviceLogKey,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<StartupLogEvent>,
}

enum StartupLogEvent {
    Serial(CaptureEvent),
    Finished(Result<CaptureSummary, String>),
}

impl StartupLogTask {
    fn launch(key: DeviceLogKey, port: Port, baud: u32) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::Builder::new()
            .name("startup-log".into())
            .spawn(move || {
                let result = serial_capture::capture_startup_log(
                    &port,
                    baud,
                    &worker_cancel,
                    &mut |event| sender.send(StartupLogEvent::Serial(event)).is_ok(),
                )
                .map_err(|error| format!("{error:#}"));
                let _ = sender.send(StartupLogEvent::Finished(result));
            })?;
        Ok(Self {
            key,
            cancel,
            receiver,
        })
    }
}

impl Drop for StartupLogTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn startup_log_port<'a>(
    ports: &'a [PortRow],
    logs: &DeviceLogBook,
    key: &DeviceLogKey,
) -> Option<&'a Port> {
    ports.iter().find_map(|row| {
        (row.online
            && !key.identity.is_empty()
            && row.port.name == key.port
            && row.port.identity == key.identity
            && logs.key_for_port(&row.port) == *key)
            .then_some(&row.port)
    })
}

impl FlashApp {
    pub(super) fn startup_log_available(&self, key: &DeviceLogKey) -> bool {
        startup_log_port(&self.ports, &self.device_logs, key).is_some()
    }

    pub(super) fn startup_log_running(&self, key: &DeviceLogKey) -> bool {
        self.startup_log
            .as_ref()
            .is_some_and(|task| task.key == *key)
    }

    pub(super) fn start_startup_log(&mut self, key: &DeviceLogKey, baud: u32) {
        if self.busy() || baud == 0 {
            return;
        }
        let Some(port) = startup_log_port(&self.ports, &self.device_logs, key).cloned() else {
            return;
        };
        self.device_logs.append_history_for(
            key,
            format!("{}  === 启动日志 · {baud} bps ===", timestamp()),
        );
        match StartupLogTask::launch(key.clone(), port, baud) {
            Ok(task) => self.startup_log = Some(task),
            Err(error) => self
                .device_logs
                .append_history_for(key, format!("无法启动日志读取：{error}")),
        }
    }

    pub(super) fn cancel_startup_log(&self, key: &DeviceLogKey) {
        if let Some(task) = self.startup_log.as_ref().filter(|task| task.key == *key) {
            task.cancel.store(true, Ordering::Release);
        }
    }

    pub(super) fn poll_startup_log(&mut self) {
        let Some(task) = self.startup_log.take() else {
            return;
        };
        loop {
            let (line, complete) = match task.receiver.try_recv() {
                Ok(StartupLogEvent::Serial(CaptureEvent::Opened)) => continue,
                Ok(StartupLogEvent::Serial(CaptureEvent::Resetting)) => {
                    (format!("{}  正在复位设备", timestamp()), false)
                }
                Ok(StartupLogEvent::Serial(CaptureEvent::Line(line))) => (line, false),
                Ok(StartupLogEvent::Finished(result)) => (
                    format!("{}  {}", timestamp(), startup_log_outcome(result)),
                    true,
                ),
                Err(mpsc::TryRecvError::Empty) => {
                    self.startup_log = Some(task);
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    (format!("{}  启动日志读取意外结束", timestamp()), true)
                }
            };
            // Console output belongs to the selected window only. Flash job
            // records and custom-field extraction use a separate log buffer.
            self.device_logs.append_history_for(&task.key, line);
            if complete {
                return;
            }
        }
    }
}

fn startup_log_outcome(result: Result<CaptureSummary, String>) -> String {
    match result {
        Ok(summary) => match summary.end {
            CaptureEnd::TimedOut if summary.bytes_read == 0 => {
                "复位后未收到串口输出，请检查波特率、复位接线和固件的输出端口".into()
            }
            CaptureEnd::TimedOut => format!("读取结束，共 {} 行", summary.lines),
            CaptureEnd::LimitReached => format!("已达到日志采集上限，显示 {} 行", summary.lines),
            CaptureEnd::Cancelled | CaptureEnd::Stopped => "已停止读取".into(),
        },
        Err(error) => format!("启动日志读取失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_startup_task_requests_cancellation() {
        let (_, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let task = StartupLogTask {
            key: DeviceLogKey {
                batch: String::new(),
                port: "COM7".into(),
                identity: "board".into(),
            },
            cancel: Arc::clone(&cancel),
            receiver,
        };
        drop(task);
        assert!(cancel.load(Ordering::Acquire));
    }

    #[test]
    fn startup_targets_the_current_window_for_the_same_online_port_identity() {
        let mut ports = [PortRow {
            port: Port {
                name: "COM6".into(),
                description: "USB UART".into(),
                usb: true,
                suggested: true,
                identity: "board-a".into(),
            },
            selected: false,
            online: true,
        }];
        let mut logs = DeviceLogBook::default();
        let idle = DeviceLogKey::from_port(&ports[0].port, "");
        assert!(startup_log_port(&ports, &logs, &idle).is_some());

        logs.begin_read(std::slice::from_ref(&ports[0].port), "current");
        let live = logs.key_for_port(&ports[0].port);
        assert!(startup_log_port(&ports, &logs, &live).is_some());
        assert!(startup_log_port(&ports, &logs, &idle).is_none());

        ports[0].online = false;
        assert!(startup_log_port(&ports, &logs, &live).is_none());
        ports[0].online = true;
        ports[0].port.identity = "board-b".into();
        assert!(startup_log_port(&ports, &logs, &live).is_none());
        let replacement = logs.key_for_port(&ports[0].port);
        assert!(startup_log_port(&ports, &logs, &replacement).is_some());
    }

    #[test]
    fn startup_log_is_visible_without_adding_to_record_or_other_windows() {
        let port = Port {
            name: "COM7".into(),
            description: "USB UART".into(),
            usb: true,
            suggested: true,
            identity: "board-a".into(),
        };
        let mut logs = DeviceLogBook::default();
        logs.begin_batch(std::slice::from_ref(&port), "flash", "ESP32-C3", "hash");
        let key = logs.key_for_port(&port);
        let record_lines = logs.current_lines.clone();
        logs.append_history_for(&key, "boot: startup line".into());
        assert_eq!(logs.current_lines, record_lines);
        assert_eq!(
            logs.history[&key].back().map(String::as_str),
            Some("boot: startup line")
        );
        assert!(
            !logs
                .history
                .contains_key(&DeviceLogKey::from_port(&port, ""))
        );
    }
}
