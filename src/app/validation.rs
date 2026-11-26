use super::*;
use esp32c3_flashtool::{
    verification::{self, VerificationChecks},
    verification_csv::{self, VerificationRecord},
};

pub(super) struct ValidationWorkspace {
    pub(super) baud_input: String,
    pub(super) rows: Vec<ValidationRow>,
    pub(super) report: Option<PathBuf>,
    pub(super) save_error: Option<String>,
    pub(super) result_open: bool,
    pub(super) production: Option<ProductionBatch>,
    batch: Option<Batch>,
    batch_id: String,
    report_directory: Option<PathBuf>,
    save_result: Option<mpsc::Receiver<Result<PathBuf, String>>>,
}

impl Default for ValidationWorkspace {
    fn default() -> Self {
        Self {
            baud_input: "115200".into(),
            rows: Vec::new(),
            report: None,
            save_error: None,
            result_open: false,
            production: None,
            batch: None,
            batch_id: String::new(),
            report_directory: None,
            save_result: None,
        }
    }
}

pub(super) struct ValidationRow {
    pub(super) port: Port,
    pub(super) state: JobState,
    pub(super) checks: Option<VerificationChecks>,
    pub(super) message: String,
    pub(super) key: DeviceLogKey,
    lines: Vec<String>,
    startup_log: String,
    finished_at: String,
}

impl ValidationRow {
    fn new(port: Port, batch_id: &str) -> Self {
        Self {
            key: DeviceLogKey::from_port(&port, batch_id),
            port,
            state: JobState::Queued,
            checks: None,
            message: "等待验证".into(),
            lines: Vec::new(),
            startup_log: String::new(),
            finished_at: String::new(),
        }
    }

    fn finish(&mut self) {
        let lines = std::mem::take(&mut self.lines);
        let mut checks = verification::evaluate(&lines);
        if self.state != JobState::Success {
            checks.failures.insert(0, self.message.clone());
        }
        self.startup_log = lines.join("\n");
        self.finished_at = beijing_now().to_rfc3339();
        self.checks = Some(checks);
    }
}

impl FlashApp {
    pub(super) fn validation_running(&self) -> bool {
        self.validation.batch.is_some() || self.validation.save_result.is_some()
    }

    pub(super) fn start_validation(&mut self) {
        if self.busy() || self.validation.save_error.is_some() {
            return;
        }
        let baud = match parse_baud_input(&self.validation.baud_input, 1) {
            Ok(baud) => baud,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let Some(setup) = self.prepare_record_setup(RecordOperation::ReadOnly) else {
            return;
        };
        let ports = self.selected_online_ports();
        let batch = match Batch::start_validation(ports.clone(), baud, self.concurrency) {
            Ok(batch) => batch,
            Err(error) => {
                self.error = Some(format!("无法启动批量验证：{error:#}"));
                return;
            }
        };

        self.error = None;
        self.notice = None;
        self.validation.batch_id = telemetry::new_batch_id();
        self.validation.production = setup.production;
        self.validation.report_directory = setup.settings.csv_directory;
        self.validation.rows = ports
            .into_iter()
            .map(|port| ValidationRow::new(port, &self.validation.batch_id))
            .collect();
        self.validation.report = None;
        self.validation.result_open = false;
        self.validation.batch = Some(batch);
        for row in &self.validation.rows {
            self.device_logs.append_history_for(
                &row.key,
                format!("{}  === 批量验证 · {baud} bps ===", timestamp()),
            );
        }
    }

    pub(super) fn cancel_validation_queue(&self) {
        if let Some(batch) = &self.validation.batch {
            batch.cancel_queued();
        }
    }

    pub(super) fn invalidate_validation_ports(&self, observed: &[Port]) {
        if let Some(batch) = &self.validation.batch {
            for row in &self.validation.rows {
                if !observed
                    .iter()
                    .any(|port| port.name == row.port.name && port.identity == row.port.identity)
                {
                    batch.invalidate_queued_port(&row.port.name, "排队设备已离线或身份改变");
                }
            }
        }
    }

    pub(super) fn poll_validation(&mut self) {
        if let Some(batch) = &self.validation.batch {
            let mut disconnected = false;
            loop {
                match batch.receiver.try_recv() {
                    Ok(event) => {
                        let Some(row) = self
                            .validation
                            .rows
                            .iter_mut()
                            .find(|row| row.port.name == event.port)
                        else {
                            continue;
                        };
                        if event.kind == BatchEventKind::DeviceLog {
                            self.device_logs
                                .append_history_for(&row.key, event.message.clone());
                            row.lines.push(event.message);
                        } else {
                            row.state = event.state;
                            row.message = event.message;
                            self.device_logs.append_history_for(
                                &row.key,
                                format!("{}  {}", timestamp(), row.message),
                            );
                            if is_terminal(&row.state) {
                                row.finish();
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                for row in self
                    .validation
                    .rows
                    .iter_mut()
                    .filter(|row| row.checks.is_none())
                {
                    row.state = JobState::Failed;
                    row.message = "启动日志采集意外结束".into();
                    self.device_logs
                        .append_history_for(&row.key, row.message.clone());
                    row.finish();
                }
            }
            if self.validation.rows.iter().all(|row| row.checks.is_some()) {
                self.validation.batch = None;
                self.save_validation_report();
            }
        }

        if let Some(receiver) = &self.validation.save_result {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err("验证结果保存任务意外结束".into()),
            };
            self.validation.save_result = None;
            self.validation.result_open = true;
            match result {
                Ok(path) => self.validation.report = Some(path),
                Err(error) => self.validation.save_error = Some(error),
            }
        }
    }

    pub(super) fn retry_validation_save(&mut self) {
        if self.validation.save_error.is_some() && !self.busy() {
            self.save_validation_report();
        }
    }

    fn save_validation_report(&mut self) {
        let Some(directory) = self.validation.report_directory.clone() else {
            self.validation.save_error = Some("本批验证缺少保存目录".into());
            self.validation.result_open = true;
            return;
        };
        let production = self.validation.production.clone();
        let records: Vec<_> = self
            .validation
            .rows
            .iter()
            .map(|row| VerificationRecord {
                batch_id: self.validation.batch_id.clone(),
                port: row.port.name.clone(),
                finished_at: row.finished_at.clone(),
                checks: row.checks.clone().expect("finished validation row"),
                startup_log: row.startup_log.clone(),
            })
            .collect();
        let (sender, receiver) = mpsc::channel();
        self.validation.save_error = None;
        self.validation.result_open = false;
        match thread::Builder::new()
            .name("validation-csv".into())
            .spawn(move || {
                let result = verification_csv::append_batch_with_optional_production(
                    &directory,
                    &records,
                    production.as_ref(),
                )
                .map_err(|error| format!("无法保存验证 CSV：{error:#}"));
                let _ = sender.send(result);
            }) {
            Ok(_) => self.validation.save_result = Some(receiver),
            Err(error) => {
                self.validation.save_error = Some(format!("无法启动 CSV 保存任务：{error}"));
                self.validation.result_open = true;
            }
        }
    }

    pub(super) fn open_validation_report(&mut self) {
        if let Some(path) = &self.validation.report
            && let Err(error) = open_records_directory(path)
        {
            self.error = Some(format!("无法打开验证 CSV：{error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_error_cannot_pass_even_with_all_expected_lines_received() {
        let mut row = ValidationRow::new(
            Port {
                name: "COM8".into(),
                description: "USB UART".into(),
                usb: true,
                suggested: true,
                identity: "board-a".into(),
            },
            "batch-a",
        );
        row.lines = [
            "MAX30105 detected & working!",
            "BMI270 detected & working!",
            "MAC ADDRESS: 001122334455",
            "UNIQUE DEVICE ID: 00112233445566778899AABBCCDDEEFF",
            "Analog Read in millivolts at battery pin: 1000",
        ]
        .map(str::to_owned)
        .into();
        assert!(verification::evaluate(&row.lines).passes());

        row.state = JobState::Failed;
        row.message = "设备已断开".into();
        row.finish();
        let checks = row.checks.unwrap();
        assert!(!checks.passes());
        assert_eq!(checks.failures, ["设备已断开"]);
        assert!(row.finished_at.ends_with("+08:00"));
        assert_eq!(row.startup_log.lines().count(), 5);
    }
}
