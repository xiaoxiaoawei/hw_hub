mod ble_station;
mod ble_verification_ui;
mod clipboard;
mod device_read;
mod firmware_drop;
mod production_setup;
mod settings;
mod software_reset;
mod software_update;
mod startup_log;
mod validation;
mod validation_ui;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ble_station::BleWorkspace;
use device_read::DeviceReadSetup;
use eframe::egui::{self, Color32, RichText};
use esp32c3_flashtool::{
    backend::{DeviceIdentity, Port},
    batch::{Batch, BatchEvent, BatchEventKind, JobState},
    chip::ChipModel,
    device_scan::DeviceScanner,
    device_watch::DeviceWatcher,
    drivers::{self, DriverKind},
    firmware::{FirmwareInput, prepare_firmware},
    production::{PRODUCTION_HEADERS, ProductionBatch, beijing_datetime, beijing_now},
    self_update::{self, PreparedUpdate, SelfUpdateEvent, SelfUpdateService},
    telemetry::{
        self, RecordDraft, RecordOperation, RecordService, RecordStatus, TelemetrySettings,
        UploadState,
    },
    updater::{self, CheckResult, ReleaseUpdate, UpdateSettings},
};
use firmware_drop::FirmwareFileDrop;
use production_setup::{ProductionInputs, RecordSetup, effective_record_directory, record_root};
use software_reset::SoftwareReset;
use startup_log::StartupLogTask;
use validation::ValidationWorkspace;

const ACCENT: Color32 = Color32::from_rgb(22, 139, 245);
const ACCENT_LIGHT: Color32 = Color32::from_rgb(29, 52, 74);
const SUCCESS: Color32 = Color32::from_rgb(32, 198, 87);
const INK: Color32 = Color32::from_rgb(239, 239, 239);
const MUTED: Color32 = Color32::from_rgb(166, 166, 166);
const BORDER: Color32 = Color32::from_rgb(59, 59, 59);
const BACKGROUND: Color32 = Color32::from_rgb(34, 34, 34);
const SURFACE: Color32 = Color32::from_rgb(40, 40, 40);
const SURFACE_ALT: Color32 = Color32::from_rgb(48, 48, 48);
const NAVY: Color32 = Color32::from_rgb(27, 27, 27);
const DANGER: Color32 = Color32::from_rgb(245, 114, 127);

#[derive(Clone, Copy, PartialEq)]
enum FirmwareLayout {
    Merged,
    Segments,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Workspace {
    Flash,
    Validation,
    Bluetooth,
}

struct FirmwareRow {
    enabled: bool,
    path: PathBuf,
    address: String,
    label: &'static str,
}

impl FirmwareRow {
    fn empty(label: &'static str, address: &str) -> Self {
        Self {
            enabled: true,
            path: PathBuf::new(),
            address: address.to_owned(),
            label,
        }
    }
}

struct PortRow {
    port: Port,
    selected: bool,
    online: bool,
}

struct JobRow {
    state: JobState,
    progress: f32,
    message: String,
    started: String,
    finished: String,
    elapsed: Option<Instant>,
    duration_secs: Option<f64>,
    started_at: String,
    finished_at: String,
    chip_identity: Option<DeviceIdentity>,
    unique_device_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DeviceLogKey {
    batch: String,
    port: String,
    identity: String,
}

impl DeviceLogKey {
    fn from_port(port: &Port, batch: &str) -> Self {
        Self {
            batch: batch.into(),
            port: port.name.clone(),
            identity: port.identity.clone(),
        }
    }
}

#[derive(Default)]
struct DeviceLogBook {
    history: BTreeMap<DeviceLogKey, VecDeque<String>>,
    current_keys: BTreeMap<String, DeviceLogKey>,
    current_lines: BTreeMap<String, Vec<String>>,
    current_bytes: BTreeMap<String, usize>,
    truncated: BTreeSet<String>,
    identities: BTreeMap<DeviceLogKey, DeviceIdentity>,
}

impl DeviceLogBook {
    fn key_for_port(&self, port: &Port) -> DeviceLogKey {
        self.current_keys
            .get(&port.name)
            .filter(|key| key.identity == port.identity)
            .cloned()
            .unwrap_or_else(|| DeviceLogKey::from_port(port, ""))
    }

    fn begin_batch(&mut self, ports: &[Port], batch: &str, chip: &str, firmware: &str) {
        self.begin_run(ports, batch, &format!("{chip} · SHA256 {firmware}"), true);
    }

    fn begin_read(&mut self, ports: &[Port], batch: &str) {
        self.begin_run(ports, batch, "读取串口日志", false);
    }

    fn begin_run(&mut self, ports: &[Port], batch: &str, operation: &str, record_header: bool) {
        self.current_keys.clear();
        self.current_lines.clear();
        self.current_bytes.clear();
        self.truncated.clear();
        for port in ports {
            let key = DeviceLogKey::from_port(port, batch);
            self.current_keys.insert(port.name.clone(), key.clone());
            let line = format!("{}  === 批次 {batch} · {operation} ===", timestamp());
            if record_header {
                self.append_line(&port.name, line);
            } else {
                self.append_history(&port.name, line);
            }
        }
    }

    fn append(&mut self, port: &str, state: &JobState, kind: BatchEventKind, message: &str) {
        if kind == BatchEventKind::Progress {
            return;
        }
        let line = format!("{}  [{}] {message}", timestamp(), state_label(state));
        self.append_line(port, line);
    }

    fn append_history(&mut self, port: &str, line: String) {
        let Some(key) = self.current_keys.get(port).cloned() else {
            return;
        };
        self.append_history_for(&key, line);
    }

    fn append_history_for(&mut self, key: &DeviceLogKey, line: String) {
        let history = self.history.entry(key.clone()).or_default();
        history.push_back(line);
        while history.len() > 10_000 {
            history.pop_front();
        }
    }

    fn append_line(&mut self, port: &str, line: String) {
        if !self.current_keys.contains_key(port) {
            return;
        }
        self.append_history(port, line.clone());
        if self.truncated.contains(port) {
            return;
        }
        let bytes = self.current_bytes.entry(port.into()).or_default();
        let current = self.current_lines.entry(port.into()).or_default();
        if *bytes + line.len() + 1 > 512 * 1024 - 128 {
            current.push("[日志已截断：本批设备日志超过 512 KiB，已保留最早身份信息；芯片结构化身份仍完整保留。]".into());
            self.truncated.insert(port.into());
        } else {
            *bytes += line.len() + 1;
            current.push(line);
        }
    }

    fn remember_identity(&mut self, port: &str, identity: &DeviceIdentity) {
        if let Some(key) = self.current_keys.get(port) {
            let known = self
                .identities
                .entry(key.clone())
                .or_insert_with(|| identity.clone());
            if known.efuse_id.is_none() {
                known.efuse_id = identity.efuse_id.clone();
            }
            if known.mac.is_none() {
                known.mac = identity.mac.clone();
            }
        }
    }
}

struct PendingRecord {
    settings: TelemetrySettings,
    draft: RecordDraft,
    device: DeviceLogKey,
    operation: RecordOperation,
    unique_device_id: Option<String>,
    production: Option<ProductionBatch>,
}

impl PendingRecord {
    fn refresh_upload_settings(&mut self, settings: &TelemetrySettings) {
        self.settings.upload_enabled = settings.upload_enabled;
        self.settings.endpoint.clone_from(&settings.endpoint);
        self.settings.timeout_seconds = settings.timeout_seconds;
        self.settings
            .custom_headers
            .clone_from(&settings.custom_headers);
    }
}

#[derive(Default)]
struct ScanSchedule {
    manual_pending: bool,
    event_deadlines: [Option<Instant>; 5],
    last_scan: Option<Instant>,
}

impl ScanSchedule {
    fn request_manual(&mut self) {
        self.manual_pending = true;
    }

    fn device_event(&mut self, now: Instant) {
        // Fixed slots coalesce successive responses from the hub and driver.
        // Preserve earlier work and make one final pass after the last event.
        for (slot, millis) in self.event_deadlines[..4].iter_mut().zip([0, 100, 300, 750]) {
            let deadline = now + Duration::from_millis(millis);
            *slot = Some(slot.map_or(deadline, |pending| pending.min(deadline)));
        }
        self.event_deadlines[4] = Some(now + Duration::from_millis(1_500));
    }

    fn disable_auto(&mut self) {
        self.event_deadlines.fill(None);
    }

    fn is_due(&self, now: Instant) -> bool {
        self.manual_pending || self.auto_deadline().is_some_and(|deadline| deadline <= now)
    }

    fn scan_started(&mut self, now: Instant) {
        self.manual_pending = false;
        self.last_scan = Some(now);
        for deadline in &mut self.event_deadlines {
            if deadline.is_some_and(|time| time <= now) {
                *deadline = None;
            }
        }
    }

    fn repaint_delay(&self, now: Instant) -> Option<Duration> {
        if self.manual_pending {
            Some(Duration::ZERO)
        } else {
            self.auto_deadline()
                .map(|deadline| deadline.saturating_duration_since(now))
        }
    }

    fn auto_deadline(&self) -> Option<Instant> {
        let deadline = self.event_deadlines.iter().flatten().copied().min()?;
        Some(self.last_scan.map_or(deadline, |last| {
            deadline.max(last + Duration::from_millis(50))
        }))
    }
}

pub struct FlashApp {
    workspace: Workspace,
    validation: ValidationWorkspace,
    ble: BleWorkspace,
    production_inputs: ProductionInputs,
    selected_chip: ChipModel,
    pending_chip: ChipModel,
    batch_chip: ChipModel,
    chip_picker_open: bool,
    pending_driver_license: Option<DriverKind>,
    layout: FirmwareLayout,
    firmware: Vec<FirmwareRow>,
    ports: Vec<PortRow>,
    scanner: DeviceScanner,
    watcher: Option<DeviceWatcher>,
    watcher_error: Option<String>,
    auto_refresh: bool,
    scan_schedule: ScanSchedule,
    scan_error: Option<String>,
    baud: u32,
    baud_input: String,
    concurrency: usize,
    batch: Option<Batch>,
    batch_operation: RecordOperation,
    device_read_setup: DeviceReadSetup,
    expected_jobs: usize,
    jobs: BTreeMap<String, JobRow>,
    logs: VecDeque<String>,
    device_logs: DeviceLogBook,
    log_windows: BTreeSet<DeviceLogKey>,
    startup_log: Option<StartupLogTask>,
    startup_log_baud: String,
    batch_summary_logged: bool,
    batch_id: String,
    batch_started_at: String,
    settings_open: bool,
    telemetry_settings: TelemetrySettings,
    telemetry_draft: TelemetrySettings,
    batch_telemetry_settings: TelemetrySettings,
    batch_production: Option<ProductionBatch>,
    telemetry_settings_error: Option<String>,
    settings_headers: Vec<(String, String)>,
    settings_extraction_rules_json: String,
    settings_auto_update: bool,
    settings_saved: bool,
    record_service: RecordService,
    pending_records: VecDeque<PendingRecord>,
    submitted_records: BTreeMap<String, PendingRecord>,
    failed_records: VecDeque<PendingRecord>,
    failed_uploads: VecDeque<PendingRecord>,
    record_notes: BTreeMap<DeviceLogKey, String>,
    record_submit_error: Option<String>,
    close_confirm_open: bool,
    software_reset: SoftwareReset,
    allow_exit: bool,
    error: Option<String>,
    notice: Option<String>,
    driver_result: Option<mpsc::Receiver<Result<String, String>>>,
    driver_busy: bool,
    last_scan: String,
    firmware_hash: String,
    batch_firmware: String,
    batch_started: String,
    batch_port_identities: BTreeMap<String, String>,
    batch_port_snapshots: Vec<Port>,
    logs_visible: bool,
    update_dialog_open: bool,
    update_settings: UpdateSettings,
    update_settings_error: Option<String>,
    update_result: Option<mpsc::Receiver<Result<CheckResult, String>>>,
    update_checking: bool,
    update_status: String,
    update_error: Option<String>,
    update_available: Option<ReleaseUpdate>,
    update_service: SelfUpdateService,
    prepared_update: Option<PreparedUpdate>,
    update_downloaded: u64,
    update_total: Option<u64>,
    self_update_phase: String,
    self_update_error: Option<String>,
    update_prompt_pending: bool,
    update_focus_later: bool,
    resume_settings_after_update: bool,
    installer_preparing: bool,
    installer_result: Option<mpsc::Receiver<Result<(), String>>>,
}

impl FlashApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = SURFACE;
        visuals.override_text_color = Some(INK);
        visuals.selection.bg_fill = ACCENT_LIGHT;
        visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
        ] {
            widget.bg_fill = SURFACE_ALT;
            widget.weak_bg_fill = SURFACE_ALT;
            widget.bg_stroke = egui::Stroke::new(1.0_f32, BORDER);
            widget.corner_radius = 6.0.into();
        }
        visuals.widgets.hovered.bg_fill = ACCENT_LIGHT;
        visuals.widgets.hovered.weak_bg_fill = ACCENT_LIGHT;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);
        visuals.widgets.hovered.corner_radius = 6.0.into();
        visuals.widgets.active.bg_fill = ACCENT_LIGHT;
        visuals.widgets.active.weak_bg_fill = ACCENT_LIGHT;
        visuals.widgets.active.corner_radius = 6.0.into();
        visuals.extreme_bg_color = BACKGROUND;
        cc.egui_ctx.set_visuals(visuals);
        cc.egui_ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 7.0);
            style.spacing.button_padding = egui::vec2(10.0, 7.0);
            style.spacing.interact_size.y = 30.0;
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
            style.text_styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::new(18.0, egui::FontFamily::Name("ui-heading".into())),
            );
        });
        let (update_settings, update_settings_error) = match updater::load_settings() {
            Ok(settings) => (settings, None),
            Err(error) => (
                UpdateSettings::default(),
                Some(format!("无法读取更新设置：{error:#}")),
            ),
        };
        let repaint = cc.egui_ctx.clone();
        let records_repaint = cc.egui_ctx.clone();
        let update_repaint = cc.egui_ctx.clone();
        let watcher_repaint = cc.egui_ctx.clone();
        let (watcher, watcher_error) =
            match DeviceWatcher::new(move || watcher_repaint.request_repaint()) {
                Ok(watcher) => (Some(watcher), None),
                Err(error) => (None, Some(format!("设备插拔监听不可用：{error:#}"))),
            };
        let auto_refresh = watcher.is_some();
        let (telemetry_settings, telemetry_settings_error) = match telemetry::load_settings() {
            Ok(settings) => (settings, None),
            Err(error) => (
                TelemetrySettings::default(),
                Some(format!("无法读取记录设置，当前使用本地默认配置：{error:#}")),
            ),
        };
        let mut app = Self {
            workspace: Workspace::Flash,
            validation: ValidationWorkspace::default(),
            ble: BleWorkspace::default(),
            production_inputs: ProductionInputs::default(),
            selected_chip: ChipModel::default(),
            pending_chip: ChipModel::default(),
            batch_chip: ChipModel::default(),
            chip_picker_open: false,
            pending_driver_license: None,
            layout: FirmwareLayout::Merged,
            firmware: vec![FirmwareRow::empty("合并固件", "0x0")],
            ports: Vec::new(),
            scanner: DeviceScanner::new(move || repaint.request_repaint()),
            watcher,
            watcher_error,
            auto_refresh,
            scan_schedule: ScanSchedule::default(),
            scan_error: None,
            baud: 460_800,
            baud_input: "460800".into(),
            concurrency: 4,
            batch: None,
            batch_operation: RecordOperation::Flash,
            device_read_setup: DeviceReadSetup::default(),
            expected_jobs: 0,
            jobs: BTreeMap::new(),
            logs: VecDeque::new(),
            device_logs: DeviceLogBook::default(),
            log_windows: BTreeSet::new(),
            startup_log: None,
            startup_log_baud: "115200".into(),
            batch_summary_logged: true,
            batch_id: String::new(),
            batch_started_at: String::new(),
            settings_open: false,
            telemetry_draft: telemetry_settings.clone(),
            batch_telemetry_settings: telemetry_settings.clone(),
            batch_production: None,
            settings_headers: telemetry_settings
                .custom_headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            settings_extraction_rules_json: serde_json::to_string_pretty(
                &telemetry_settings.extraction_rules,
            )
            .expect("string map serializes to JSON"),
            settings_auto_update: update_settings.auto_check,
            settings_saved: false,
            telemetry_settings,
            telemetry_settings_error,
            record_service: RecordService::new(move || records_repaint.request_repaint()),
            pending_records: VecDeque::new(),
            submitted_records: BTreeMap::new(),
            failed_records: VecDeque::new(),
            failed_uploads: VecDeque::new(),
            record_notes: BTreeMap::new(),
            record_submit_error: None,
            close_confirm_open: false,
            software_reset: SoftwareReset::default(),
            allow_exit: false,
            error: None,
            notice: None,
            driver_result: None,
            driver_busy: false,
            last_scan: String::new(),
            firmware_hash: String::new(),
            batch_firmware: String::new(),
            batch_started: String::new(),
            batch_port_identities: BTreeMap::new(),
            batch_port_snapshots: Vec::new(),
            logs_visible: true,
            update_dialog_open: false,
            update_settings,
            update_settings_error,
            update_result: None,
            update_checking: false,
            update_status: "尚未检查更新".into(),
            update_error: None,
            update_available: None,
            update_service: SelfUpdateService::new(move || update_repaint.request_repaint()),
            prepared_update: None,
            update_downloaded: 0,
            update_total: None,
            self_update_phase: String::new(),
            self_update_error: None,
            update_prompt_pending: false,
            update_focus_later: false,
            resume_settings_after_update: false,
            installer_preparing: false,
            installer_result: None,
        };
        app.scan_ports();
        if app.update_settings_error.is_none()
            && updater::should_auto_check(&app.update_settings, now_unix())
        {
            app.start_update_check(false, &cc.egui_ctx);
        }
        app
    }

    fn running(&self) -> bool {
        self.batch.as_ref().is_some_and(|batch| {
            batch.is_running()
                || self.jobs.len() < self.expected_jobs
                || self.jobs.values().any(|row| !is_terminal(&row.state))
        })
    }

    fn busy(&self) -> bool {
        self.running()
            || self.validation_running()
            || self.ble.is_connected()
            || self.ble.saving()
            || self.startup_log.is_some()
            || self.driver_busy
            || self.installer_preparing
            || self.software_reset.in_progress()
    }

    fn scan_ports(&mut self) {
        self.scan_schedule.request_manual();
        self.start_scheduled_scan();
    }

    fn start_scheduled_scan(&mut self) {
        let now = Instant::now();
        if self.scan_schedule.is_due(now) && self.scanner.request_scan() {
            self.scan_schedule.scan_started(now);
        }
    }

    fn poll_ports(&mut self) {
        if let Some(Err(error)) = self
            .watcher
            .as_mut()
            .and_then(DeviceWatcher::poll_registration)
        {
            self.watcher = None;
            self.auto_refresh = false;
            self.scan_schedule.disable_auto();
            self.watcher_error = Some(format!("设备插拔监听不可用：{error:#}"));
        }
        if self
            .watcher
            .as_ref()
            .is_some_and(DeviceWatcher::take_pending)
            && self.auto_refresh
        {
            self.scan_schedule.device_event(Instant::now());
        }
        if let Some(result) = self.scanner.poll() {
            if let Ok(observed) = &result {
                self.invalidate_validation_ports(observed);
                self.invalidate_ble_port(observed);
            }
            if let (Ok(observed), Some(batch)) = (&result, &self.batch) {
                for (name, identity) in &self.batch_port_identities {
                    if !observed
                        .iter()
                        .any(|port| &port.name == name && &port.identity == identity)
                    {
                        batch.invalidate_queued_port(
                            name,
                            "原设备已离线或串口身份改变，取消本批排队任务；请重新勾选后启动新批次",
                        );
                    }
                }
            }
            let success = result.is_ok();
            let (ports, error) = reconcile_scan_result(
                std::mem::take(&mut self.ports),
                result.map_err(|error| format!("串口扫描失败：{error:#}")),
            );
            self.ports = ports;
            self.scan_error = error;
            if success {
                self.last_scan = beijing_now().format("%H:%M:%S").to_string();
            }
        }
        self.start_scheduled_scan();
    }

    fn start_batch(&mut self) {
        self.error = None;
        self.notice = None;
        self.baud = match parse_baud_input(&self.baud_input, 115_200) {
            Ok(baud) => baud,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let ports = self.selected_online_ports();
        if ports.is_empty() {
            self.error = Some("请先勾选在线的待烧录串口。".into());
            return;
        }
        let Some(setup) = self.prepare_record_setup(RecordOperation::Flash) else {
            return;
        };
        let mut inputs = Vec::new();
        for (index, row) in self.firmware.iter().enumerate().filter(|(_, r)| r.enabled) {
            if row.path.as_os_str().is_empty() {
                self.error = Some(format!(
                    "请选择第 {} 行的固件文件，或取消启用该行。",
                    index + 1
                ));
                return;
            }
            let raw = row.address.trim();
            let raw = raw
                .strip_prefix("0x")
                .or_else(|| raw.strip_prefix("0X"))
                .unwrap_or(raw);
            let Ok(address) = u32::from_str_radix(raw, 16) else {
                self.error = Some(format!("第 {} 行地址无效，请输入十六进制地址。", index + 1));
                return;
            };
            inputs.push(FirmwareInput {
                path: row.path.clone(),
                address,
            });
        }
        let firmware = match prepare_firmware(&inputs, self.selected_chip) {
            Ok(firmware) => firmware,
            Err(err) => {
                self.error = Some(format!("固件预检未通过：{err:#}"));
                return;
            }
        };
        let firmware_hash = firmware.sha256.clone();
        let batch_firmware = inputs
            .iter()
            .map(|input| format!("{}@0x{:X}", input.path.display(), input.address))
            .collect::<Vec<_>>()
            .join("; ");
        match Batch::start_guarded(
            ports.clone(),
            self.baud,
            self.concurrency,
            Arc::new(firmware),
        ) {
            Ok(batch) => {
                self.firmware_hash = firmware_hash;
                self.batch_firmware = batch_firmware;
                self.batch_chip = self.selected_chip;
                self.attach_batch(batch, ports, RecordOperation::Flash, setup);
            }
            Err(err) => self.error = Some(format!("无法启动批次：{err:#}")),
        }
    }

    fn selected_online_ports(&self) -> Vec<Port> {
        self.ports
            .iter()
            .filter(|row| row.selected && row.online)
            .map(|row| row.port.clone())
            .collect()
    }

    fn attach_batch(
        &mut self,
        batch: Batch,
        ports: Vec<Port>,
        operation: RecordOperation,
        setup: RecordSetup,
    ) {
        self.jobs.clear();
        self.expected_jobs = ports.len();
        self.logs.clear();
        self.batch_operation = operation;
        self.batch_port_identities = ports
            .iter()
            .map(|port| (port.name.clone(), port.identity.clone()))
            .collect();
        self.batch_port_snapshots = ports;
        self.batch_started = timestamp();
        self.batch_started_at = beijing_now().to_rfc3339();
        self.batch_id = telemetry::new_batch_id();
        self.batch_telemetry_settings = setup.settings;
        self.batch_production = setup.production;
        self.batch_summary_logged = false;
        match operation {
            RecordOperation::Flash => self.device_logs.begin_batch(
                &self.batch_port_snapshots,
                &self.batch_id,
                self.batch_chip.label(),
                &self.firmware_hash,
            ),
            RecordOperation::ReadOnly => self
                .device_logs
                .begin_read(&self.batch_port_snapshots, &self.batch_id),
        }
        for port in &self.batch_port_snapshots {
            let idle = DeviceLogKey::from_port(port, "");
            if self.log_windows.remove(&idle) {
                self.log_windows
                    .insert(DeviceLogKey::from_port(port, &self.batch_id));
            }
        }
        self.batch = Some(batch);
    }

    fn log(&mut self, message: String) {
        self.logs
            .push_back(format!("{}  {message}", beijing_now().format("%H:%M:%S")));
        while self.logs.len() > 1_000 {
            self.logs.pop_front();
        }
    }

    fn collect_events(&mut self) {
        let events = self
            .batch
            .as_ref()
            .map(|batch| batch.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for BatchEvent {
            port,
            state,
            message,
            progress,
            kind,
            identity,
        } in events
        {
            if kind == BatchEventKind::DeviceLog {
                self.device_logs.append_line(&port, message);
                continue;
            }
            let now = timestamp();
            let row = self.jobs.entry(port.clone()).or_insert_with(|| JobRow {
                state: JobState::Queued,
                progress: 0.0,
                message: String::new(),
                started: String::new(),
                finished: String::new(),
                elapsed: None,
                duration_secs: None,
                started_at: String::new(),
                finished_at: String::new(),
                chip_identity: None,
                unique_device_id: None,
            });
            if kind == BatchEventKind::UniqueDeviceId {
                self.device_logs
                    .append_line(&port, format!("{now}  UNIQUE DEVICE ID: {message}"));
                row.unique_device_id = Some(message);
                continue;
            }
            let first_terminal =
                kind == BatchEventKind::State && is_terminal(&state) && row.finished_at.is_empty();
            if state == JobState::Connecting && row.started.is_empty() {
                row.started = now.clone();
                row.started_at = beijing_now().to_rfc3339();
                row.elapsed = Some(Instant::now());
            }
            if first_terminal {
                row.finished = now;
                row.finished_at = beijing_now().to_rfc3339();
                row.duration_secs = row.elapsed.map(|start| start.elapsed().as_secs_f64());
            }
            if let Some(reported) = &identity {
                let known = row.chip_identity.get_or_insert(DeviceIdentity {
                    efuse_id: None,
                    mac: None,
                });
                if known.efuse_id.is_none() {
                    known.efuse_id = reported.efuse_id.clone();
                }
                if known.mac.is_none() {
                    known.mac = reported.mac.clone();
                }
            }
            row.state = state.clone();
            let progress = progress.clamp(0.0, 1.0);
            row.progress = if state == JobState::Failed {
                row.progress.max(progress)
            } else {
                progress
            };
            row.message = message.clone();
            if self.batch_operation == RecordOperation::Flash {
                self.device_logs.append(&port, &state, kind, &message);
            } else if kind != BatchEventKind::Progress {
                self.device_logs.append_history(
                    &port,
                    format!("{}  [{}] {message}", timestamp(), state_label(&state)),
                );
            }
            if kind == BatchEventKind::Identity
                && let Some(identity) = &identity
            {
                self.device_logs.remember_identity(&port, identity);
                self.device_logs.append_line(
                    &port,
                    format!(
                        "{}  EFUSE_ID={} MAC={}",
                        timestamp(),
                        identity.efuse_id.as_deref().unwrap_or("未知"),
                        identity.mac.as_deref().unwrap_or("未知")
                    ),
                );
            }
            if first_terminal {
                if let Some(summary) = terminal_summary(&port, &state, self.batch_operation) {
                    self.log(summary);
                }
                self.queue_terminal_record(&port);
            }
        }
        if !self.batch_summary_logged
            && self.jobs.len() == self.expected_jobs
            && self.jobs.values().all(|row| is_terminal(&row.state))
        {
            self.batch_summary_logged = true;
            let count = |state| self.jobs.values().filter(|row| row.state == state).count();
            self.log(format!(
                "批次完成：成功 {}，失败 {}，取消 {}，共 {} 台",
                count(JobState::Success),
                count(JobState::Failed),
                count(JobState::Cancelled),
                self.expected_jobs
            ));
        }
        if let Some(receiver) = &self.driver_result {
            match receiver.try_recv() {
                Ok(Ok(message)) => {
                    self.driver_busy = false;
                    self.driver_result = None;
                    self.notice = Some(message);
                    self.scan_ports();
                }
                Ok(Err(message)) => {
                    self.driver_busy = false;
                    self.driver_result = None;
                    self.error = Some(message);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.driver_busy = false;
                    self.driver_result = None;
                    self.error = Some("驱动安装任务意外结束，请检查 Windows 驱动安装记录。".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn queue_terminal_record(&mut self, port: &str) {
        let Some(row) = self.jobs.get(port) else {
            return;
        };
        let Some(device) = self.device_logs.current_keys.get(port).cloned() else {
            return;
        };
        let status = match row.state {
            JobState::Success => RecordStatus::Success,
            JobState::Failed => RecordStatus::Failed,
            JobState::Cancelled => RecordStatus::Cancelled,
            _ => return,
        };
        let draft = RecordDraft {
            batch_id: self.batch_id.clone(),
            port: port.into(),
            chip: if self.batch_operation == RecordOperation::Flash {
                self.batch_chip.slug().into()
            } else {
                String::new()
            },
            identity: row.chip_identity.clone(),
            status,
            started_at: if row.started_at.is_empty() {
                self.batch_started_at.clone()
            } else {
                row.started_at.clone()
            },
            finished_at: row.finished_at.clone(),
            duration_ms: row
                .duration_secs
                .map_or(0, |seconds| (seconds * 1_000.0).round() as u64),
            firmware_sha256: self.firmware_hash.clone(),
            logs: self
                .device_logs
                .current_lines
                .get(port)
                .cloned()
                .unwrap_or_default(),
        };
        self.pending_records.push_back(PendingRecord {
            settings: self.batch_telemetry_settings.clone(),
            draft,
            device,
            operation: self.batch_operation,
            unique_device_id: row.unique_device_id.clone(),
            production: self.batch_production.clone(),
        });
        self.retry_record_submissions();
    }

    fn retry_record_submissions(&mut self) {
        while let Some(pending) = self.pending_records.front() {
            match self.record_service.submit_with_production(
                &pending.settings,
                pending.draft.clone(),
                pending.operation,
                pending.unique_device_id.as_deref(),
                pending.production.as_ref(),
            ) {
                Ok(id) => {
                    let pending = self
                        .pending_records
                        .pop_front()
                        .expect("front record exists");
                    self.submitted_records.insert(id, pending);
                    self.record_submit_error = None;
                }
                Err(error) => {
                    let error = format!("设备记录等待保存：{error:#}");
                    if self.record_submit_error.as_deref() != Some(error.as_str()) {
                        self.log(error.clone());
                    }
                    self.record_submit_error = Some(error);
                    break;
                }
            }
        }
    }

    fn poll_records(&mut self) {
        let mut completed = false;
        while let Some(result) = self.record_service.poll() {
            completed = true;
            let Some(pending) = self.submitted_records.remove(&result.record_id) else {
                continue;
            };
            if pending.draft.batch_id != result.batch_id {
                self.log(format!(
                    "{} 记录响应批次不匹配，未关联到当前设备",
                    result.port
                ));
                self.failed_records.push_back(pending);
                continue;
            }
            let device = pending.device.clone();
            let storage = result
                .csv_path
                .as_ref()
                .map(|path| format!("CSV：{}", path.display()))
                .unwrap_or_else(|| "本地 CSV 未保存".into());
            let incomplete_read = pending.operation == RecordOperation::ReadOnly
                && pending.draft.status != RecordStatus::Success;
            let retry_upload = !incomplete_read
                && matches!(
                    &result.upload_state,
                    UploadState::Failed | UploadState::Deferred
                );
            let upload = match result.upload_state {
                UploadState::Disabled => "上报关闭",
                UploadState::SkippedIdentity => "未上报：设备身份未确认",
                UploadState::Uploaded => "上报完成",
                UploadState::Failed if incomplete_read => "未上报：读取未完成",
                UploadState::Failed => "上报失败",
                UploadState::Deferred => "未上报：上传队列暂不可用",
            };
            let note = if result.custom_fields.is_empty() {
                format!("{storage} · {upload}")
            } else {
                format!(
                    "{storage} · 提取字段 {} 个 · {upload}",
                    result.custom_fields.len()
                )
            };
            self.device_logs
                .history
                .entry(device.clone())
                .or_default()
                .push_back(format!(
                    "{}  [记录 {}] {note}",
                    timestamp(),
                    result.batch_id
                ));
            self.record_notes.insert(device, note);
            if let Some(error) = result.error {
                self.log(format!("{} 记录处理失败：{error}", result.port));
            }
            if result.csv_path.is_none() {
                self.failed_records.push_back(pending);
            } else if retry_upload {
                self.failed_uploads.push_back(pending);
            }
        }
        if completed || !self.pending_records.is_empty() {
            self.retry_record_submissions();
        }
    }

    fn device_log_windows(&mut self, ctx: &egui::Context) {
        let viewport = ctx.screen_rect().shrink(12.0);
        let windows: Vec<_> = self.log_windows.iter().cloned().collect();
        for key in windows {
            let mut open = true;
            let mut close = false;
            let frame = egui::Frame::new()
                .fill(NAVY)
                .stroke(egui::Stroke::new(1.0_f32, BORDER))
                .corner_radius(4.0)
                .inner_margin(12.0);
            let max_size =
                (viewport.size() - frame.total_margin().sum()).max(egui::Vec2::splat(1.0));
            egui::Window::new(format!("{} LOG", key.port))
                .id(egui::Id::new((
                    "device_log",
                    &key.batch,
                    &key.port,
                    &key.identity,
                )))
                .open(&mut open)
                .title_bar(false)
                .movable(true)
                .resizable([true, true])
                .constrain_to(viewport)
                .frame(frame)
                .default_size(egui::vec2(900.0, 560.0).min(max_size))
                .min_size(egui::vec2(600.0, 360.0).min(max_size))
                .max_size(max_size)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{} LOG", key.port))
                                .size(16.0)
                                .strong(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if secondary_button(ui, "关闭", [64.0, 29.0]).clicked() {
                                close = true;
                            }
                            let copy = secondary_button(ui, "复制", [64.0, 29.0]);
                            clipboard::copy_with_feedback(ui, copy, || {
                                self.device_logs
                                    .history
                                    .get(&key)
                                    .map(|lines| {
                                        lines.iter().cloned().collect::<Vec<_>>().join("\n")
                                    })
                                    .unwrap_or_default()
                            });
                        });
                    });
                    ui.separator();
                    let reading_startup = self.startup_log_running(&key);
                    if reading_startup || self.startup_log_available(&key) {
                        ui.push_id(("startup_log_toolbar", &key), |ui| {
                            ui.horizontal_wrapped(|ui| {
                                if reading_startup {
                                    ui.spinner();
                                    ui.label(
                                        RichText::new("正在读取启动日志").size(12.0).color(MUTED),
                                    );
                                    if secondary_button(ui, "停止读取", [88.0, 30.0]).clicked()
                                    {
                                        self.cancel_startup_log(&key);
                                    }
                                } else {
                                    let locked = self.busy();
                                    ui.label(RichText::new("波特率").size(12.0).color(MUTED));
                                    ui.add_enabled_ui(!locked, |ui| {
                                        baud_editor(
                                            ui,
                                            &mut self.startup_log_baud,
                                            "startup_log_baud",
                                            1,
                                        );
                                    });
                                    let baud = parse_baud_input(&self.startup_log_baud, 1).ok();
                                    if ui
                                        .add_enabled_ui(!locked && baud.is_some(), |ui| {
                                            primary_button(ui, "复位并读取启动日志", [184.0, 32.0])
                                        })
                                        .inner
                                        .clicked()
                                        && let Some(baud) = baud
                                    {
                                        self.start_startup_log(&key, baud);
                                    }
                                }
                            });
                        });
                        ui.separator();
                    }
                    let body_size = ui.available_size().max(egui::Vec2::splat(1.0));
                    egui::ScrollArea::both()
                        .id_salt(("detailed_log", &key))
                        .auto_shrink([false, false])
                        .min_scrolled_width(0.0)
                        .min_scrolled_height(0.0)
                        .max_width(body_size.x)
                        .max_height(body_size.y)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if let Some(lines) = self
                                .device_logs
                                .history
                                .get(&key)
                                .filter(|lines| !lines.is_empty())
                            {
                                for line in lines {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(line).monospace().size(12.0).color(INK),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                }
                            } else {
                                ui.label(RichText::new("暂无日志").size(12.0).color(MUTED));
                            }
                        });
                });
            if close || !open {
                self.cancel_startup_log(&key);
                self.log_windows.remove(&key);
            }
        }
    }

    fn save_settings_from_ui(&mut self) -> bool {
        self.telemetry_draft.extraction_rules =
            match telemetry::parse_extraction_rules_json(&self.settings_extraction_rules_json) {
                Ok(rules) => rules,
                Err(error) => {
                    self.telemetry_settings_error = Some(format!("提取规则未保存：{error:#}"));
                    return false;
                }
            };
        let mut headers = BTreeMap::new();
        let mut names = BTreeSet::new();
        for (name, value) in &self.settings_headers {
            let name = name.trim();
            if name.is_empty() && value.is_empty() {
                continue;
            }
            if name.is_empty() || !names.insert(name.to_ascii_lowercase()) {
                self.telemetry_settings_error = Some("请求头名称不能为空或重复。".into());
                return false;
            }
            headers.insert(name.into(), value.clone());
        }
        self.telemetry_draft.custom_headers = headers;
        if let Err(error) = telemetry::validate_settings(&self.telemetry_draft)
            .and_then(|_| telemetry::save_settings(&self.telemetry_draft))
        {
            self.telemetry_settings_error = Some(format!("设置未保存：{error:#}"));
            return false;
        }
        self.telemetry_settings = self.telemetry_draft.clone();
        self.settings_extraction_rules_json =
            serde_json::to_string_pretty(&self.telemetry_settings.extraction_rules)
                .expect("string map serializes to JSON");
        let previous = self.update_settings.auto_check;
        self.update_settings.auto_check = self.settings_auto_update;
        if let Err(error) = updater::save_settings(&self.update_settings) {
            self.update_settings.auto_check = previous;
            self.telemetry_settings_error =
                Some(format!("记录设置已保存，更新偏好未保存：{error:#}"));
            return false;
        }
        self.telemetry_settings_error = None;
        self.settings_saved = true;
        self.record_submit_error = None;
        self.retry_record_submissions();
        true
    }

    fn retry_failed_local_records(&mut self) {
        while let Some(mut pending) = self.failed_records.pop_front() {
            if let Ok(directory) = effective_record_directory(
                &self.telemetry_settings,
                pending.operation,
                pending.production.as_ref(),
            ) {
                pending.settings.csv_directory = Some(directory);
            }
            pending.refresh_upload_settings(&self.telemetry_settings);
            self.pending_records.push_back(pending);
        }
        self.record_submit_error = None;
        self.retry_record_submissions();
    }

    fn retry_failed_uploads(&mut self) {
        while let Some(mut pending) = self.failed_uploads.pop_front() {
            pending.refresh_upload_settings(&self.telemetry_settings);
            self.pending_records.push_back(pending);
        }
        self.record_submit_error = None;
        self.retry_record_submissions();
    }

    fn confirm_record_exit(&mut self, ctx: &egui::Context) {
        if !self.close_confirm_open {
            return;
        }
        let mut wait = false;
        let mut exit = false;
        let mut retry = false;
        let mut show_validation = false;
        let mut show_ble = false;
        let response = egui::Modal::new(egui::Id::new("unfinished_record_exit"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(5.0)
                    .inner_margin(24.0),
            )
            .show(ctx, |ui| {
                ui.set_width(480.0);
                ui.label(RichText::new("仍有记录未处理完成").size(22.0).strong());
                if !self.failed_records.is_empty() || !self.pending_records.is_empty() {
                    ui.label(format!(
                        "{} 条本地记录尚未保存。",
                        self.failed_records.len() + self.pending_records.len()
                    ));
                    if secondary_button(ui, "返回设置重试保存", [164.0, 36.0]).clicked() {
                        retry = true;
                    }
                } else if self.record_service.pending_count() > 0 {
                    ui.label(format!(
                        "本地已保存，仍有 {} 条上传未完成。",
                        self.record_service.pending_count()
                    ));
                }
                if self.validation.save_error.is_some() {
                    ui.label("本批验证结果尚未保存。");
                    show_validation = secondary_button(ui, "返回验证结果", [128.0, 36.0]).clicked();
                }
                if self.ble.has_unsaved() {
                    ui.label("蓝牙验证结果尚未保存。");
                    show_ble = secondary_button(ui, "返回蓝牙验证", [128.0, 36.0]).clicked();
                }
                ui.horizontal(|ui| {
                    if primary_button(ui, "继续等待", [116.0, 38.0]).clicked() {
                        wait = true;
                    }
                    if secondary_button(ui, "仍然退出", [116.0, 38.0]).clicked() {
                        exit = true;
                    }
                });
            });
        if exit {
            self.allow_exit = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if retry {
            self.close_confirm_open = false;
            self.open_settings();
        }
        if show_validation {
            self.close_confirm_open = false;
            self.workspace = Workspace::Validation;
            self.validation.result_open = true;
        }
        if show_ble {
            self.close_confirm_open = false;
            self.workspace = Workspace::Bluetooth;
        }
        if wait || response.should_close() {
            self.close_confirm_open = false;
        }
    }

    fn install_driver(&mut self, kind: DriverKind, selected_zip: Option<PathBuf>) {
        self.error = None;
        self.notice = None;
        self.driver_busy = true;
        let (sender, receiver) = mpsc::channel();
        self.driver_result = Some(receiver);
        thread::spawn(move || {
            let result = if let Some(path) = selected_zip {
                drivers::install_from_zip(kind, &path)
            } else {
                drivers::install(kind)
            };
            let _ = sender.send(result.map_err(|err| format!("驱动安装失败：{err:#}")));
        });
    }

    fn request_bundled_driver(&mut self, kind: DriverKind) {
        if drivers::bundled_license_text(kind).is_some() {
            self.pending_driver_license = Some(kind);
        } else {
            self.install_driver(kind, None);
        }
    }

    fn driver_license_dialog(&mut self, ctx: &egui::Context) {
        let Some(kind) = self.pending_driver_license else {
            return;
        };
        let Some(license) = drivers::bundled_license_text(kind) else {
            self.pending_driver_license = None;
            self.error = Some("内置驱动许可不可用，请重新打开驱动管理。".into());
            return;
        };
        let mut accept = false;
        let mut cancel = false;
        let response = egui::Modal::new(egui::Id::new("driver_license"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(6.0)
                    .inner_margin(24.0),
            )
            .show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(RichText::new("CP210x 驱动许可").size(22.0).strong());
                ui.label(
                    RichText::new("安装前请阅读驱动包随附的原厂许可。")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .id_salt("driver_license_text")
                    .max_height((ctx.screen_rect().height() - 300.0).max(200.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(license).monospace().size(11.0)).wrap(),
                        );
                    });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, "接受许可并安装", [166.0, 39.0]).clicked() {
                        accept = true;
                    }
                    if secondary_button(ui, "取消", [84.0, 39.0]).clicked() {
                        cancel = true;
                    }
                });
            });
        if accept {
            self.pending_driver_license = None;
            self.install_driver(kind, None);
        } else if cancel || response.should_close() {
            self.pending_driver_license = None;
        }
    }

    fn apply_chip(&mut self, chip: ChipModel) {
        if chip == self.selected_chip {
            return;
        }
        let old_address = self.selected_chip.bootloader_address();
        for row in &mut self.firmware {
            if row.label == "Bootloader" {
                let address = row
                    .address
                    .trim()
                    .trim_start_matches("0x")
                    .trim_start_matches("0X");
                if u32::from_str_radix(address, 16).ok() == Some(old_address) {
                    row.address = format!("0x{:X}", chip.bootloader_address());
                }
            }
        }
        self.selected_chip = chip;
        self.notice = Some(format!(
            "已切换为 {}，保留已选文件。分段模板的 Bootloader 默认地址为 0x{:X}；请确认固件及写入地址与目标芯片一致。",
            chip.label(),
            chip.bootloader_address()
        ));
    }

    fn set_layout(&mut self, layout: FirmwareLayout) {
        if self.layout == layout {
            return;
        }
        self.layout = layout;
        self.firmware = match layout {
            FirmwareLayout::Merged => vec![FirmwareRow::empty("合并固件", "0x0")],
            FirmwareLayout::Segments => vec![
                FirmwareRow::empty(
                    "Bootloader",
                    &format!("0x{:X}", self.selected_chip.bootloader_address()),
                ),
                FirmwareRow::empty("分区表", "0x8000"),
                FirmwareRow::empty("应用固件", "0x10000"),
            ],
        };
    }

    fn open_settings(&mut self) {
        if self.settings_open || self.installer_preparing {
            return;
        }
        self.update_dialog_open = false;
        self.chip_picker_open = false;
        self.pending_driver_license = None;
        self.discard_settings_draft();
        self.settings_open = true;
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let meta = Color32::from_rgb(144, 163, 184);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            icon(ui, Icon::Chip, 23.0, ACCENT);
            ui.label(
                RichText::new("FlashTool")
                    .heading()
                    .size(22.0)
                    .strong()
                    .color(Color32::WHITE),
            );
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.label(
                RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(12.0)
                    .color(meta),
            );
            if ui
                .add(
                    egui::Button::new(RichText::new("检查更新").size(12.0).color(ACCENT))
                        .frame(false),
                )
                .clicked()
            {
                self.open_update_dialog();
                self.start_update_check(true, ui.ctx());
            }
        });
        ui.add_space(22.0);
        ui.label(
            RichText::new("工作空间")
                .heading()
                .size(13.0)
                .strong()
                .color(INK),
        );
        for (workspace, icon, label) in [
            (Workspace::Flash, Icon::Grid, "批量烧录"),
            (Workspace::Validation, Icon::Shield, "批量验证"),
            (Workspace::Bluetooth, Icon::Bluetooth, "蓝牙验证"),
        ] {
            if nav_item(
                ui,
                icon,
                label,
                self.workspace == workspace && !self.settings_open,
            )
            .clicked()
            {
                self.workspace = workspace;
                self.settings_open = false;
                self.discard_settings_draft();
            }
        }
        ui.add_space(20.0);
        ui.label(
            RichText::new("当前配置")
                .heading()
                .size(13.0)
                .strong()
                .color(INK),
        );
        let (chip, baud) = match self.workspace {
            Workspace::Flash => (self.selected_chip, self.baud_input.as_str()),
            Workspace::Validation => (ChipModel::Esp32c3, self.validation.baud_input.as_str()),
            Workspace::Bluetooth => (ChipModel::Esp32s3, "115200"),
        };
        ui.label(
            RichText::new(chip.label())
                .heading()
                .size(14.0)
                .strong()
                .color(Color32::from_rgb(220, 230, 241)),
        );
        let connection = if self.workspace == Workspace::Bluetooth {
            "115200 · 串口工装".into()
        } else {
            format!(
                "{} · {} 台并发",
                parse_baud_input(
                    baud,
                    if self.workspace == Workspace::Flash {
                        115_200
                    } else {
                        1
                    }
                )
                .map_or_else(|_| "波特率无效".into(), |rate| rate.to_string()),
                self.concurrency
            )
        };
        ui.label(RichText::new(connection).size(12.0).color(meta));
        if self.update_in_progress() {
            ui.add_space(12.0);
            let text = if self.installer_preparing {
                "正在准备更新"
            } else if self.update_service.is_busy() {
                "正在下载更新"
            } else {
                "更新已就绪"
            };
            if ui
                .add(egui::Button::new(RichText::new(text).size(12.0).color(ACCENT)).frame(false))
                .clicked()
            {
                self.open_update_dialog();
            }
        } else if self.update_checking {
            ui.add_space(12.0);
            ui.label(RichText::new("正在检查更新…").size(12.0).color(meta));
        } else if let Some(version) = self
            .update_available
            .as_ref()
            .map(|release| release.version.clone())
        {
            ui.add_space(14.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(format!("发现新版本 {version}"))
                            .size(12.0)
                            .color(Color32::from_rgb(130, 218, 196)),
                    )
                    .fill(Color32::from_rgb(24, 63, 66))
                    .corner_radius(4.0),
                )
                .clicked()
            {
                self.open_update_dialog();
            }
        } else if self.update_error.is_some()
            || self.update_settings_error.is_some()
            || self.self_update_error.is_some()
        {
            ui.add_space(12.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("更新检查详情").size(12.0).color(meta))
                        .frame(false),
                )
                .clicked()
            {
                self.open_update_dialog();
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled_ui(!self.installer_preparing, settings_icon_button)
                    .inner
                    .on_hover_text("软件设置")
                    .clicked()
                {
                    self.open_settings();
                }
                if ui
                    .add_enabled_ui(
                        !self.installer_preparing && !self.software_reset.in_progress(),
                        |ui| secondary_button(ui, "重置软件", [90.0, 34.0]),
                    )
                    .inner
                    .on_hover_text("重新启动当前版本，保留设置与已保存记录")
                    .clicked()
                {
                    self.open_software_reset();
                }
            });
            ui.label(
                RichText::new(format!(
                    "{} 个串口在线",
                    self.ports.iter().filter(|row| row.online).count()
                ))
                .size(12.0)
                .color(meta),
            );
            if !self.failed_records.is_empty() {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("{} 条记录未保存", self.failed_records.len()))
                                .size(12.0)
                                .color(Color32::from_rgb(245, 161, 153)),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    self.open_settings();
                }
            } else if self.record_service.pending_local_count() + self.pending_records.len() > 0 {
                ui.label(RichText::new("正在保存本地记录").size(12.0).color(meta));
            }
        });
    }

    fn heading(&mut self, ui: &mut egui::Ui) {
        let mut title_rect = egui::Rect::NOTHING;
        let mut controls_left = ui.max_rect().right();
        let row = ui.horizontal(|ui| {
            title_rect = ui
                .label(
                    RichText::new(match self.workspace {
                        Workspace::Flash => "设备烧录工作台",
                        Workspace::Validation => "ESP32-C3 批量验证",
                        Workspace::Bluetooth => "ESP32-S3 蓝牙验证",
                    })
                    .heading()
                    .size(22.0)
                    .strong()
                    .color(INK),
                )
                .rect;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let driver = ui
                    .add_enabled_ui(!self.busy(), |ui| {
                        secondary_button(ui, "安装驱动", [94.0, 35.0])
                    })
                    .inner;
                controls_left = controls_left.min(driver.rect.left());
                if driver.clicked() {
                    self.open_settings();
                    settings::select_page(ui.ctx(), settings::SettingsPage::Drivers);
                }
            });
        });
        let modal_open = self.settings_open
            || self.device_read_setup.open
            || self.validation.result_open
            || self.update_dialog_open
            || self.chip_picker_open
            || self.pending_driver_license.is_some()
            || self.close_confirm_open
            || self.software_reset.open;
        if !modal_open
            && let Some(blank) = header_drag_region(row.response.rect, title_rect, controls_left)
        {
            let drag = ui.interact(
                blank,
                ui.id().with("blank_header_drag"),
                egui::Sense::drag(),
            );
            if drag.drag_started_by(egui::PointerButton::Primary)
                && ui
                    .ctx()
                    .pointer_interact_pos()
                    .is_some_and(|position| ui.ctx().layer_id_at(position) == Some(ui.layer_id()))
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        }
    }

    fn chip_panel(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("目标芯片").size(11.0).color(MUTED));
        if secondary_button(
            ui,
            &format!("{}  >", self.selected_chip.label()),
            [132.0, 32.0],
        )
        .clicked()
        {
            self.pending_chip = self.selected_chip;
            self.settings_open = false;
            self.update_dialog_open = false;
            self.chip_picker_open = true;
        }
    }

    fn chip_dialog(&mut self, ctx: &egui::Context) {
        if !self.chip_picker_open {
            return;
        }
        let mut apply = false;
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("chip_selector"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(6.0)
                    .inner_margin(26.0),
            )
            .show(ctx, |ui| {
                ui.set_width(570.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("选择目标芯片").size(23.0).strong());
                        ui.label(
                            RichText::new("本批所有设备均使用同一芯片型号。")
                                .color(MUTED)
                                .size(13.0),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if text_button(ui, "关闭").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(16.0);
                egui::Grid::new("chip_choices")
                    .num_columns(3)
                    .spacing([12.0, 12.0])
                    .show(ui, |ui| {
                        for (index, chip) in ChipModel::all().iter().copied().enumerate() {
                            let selected = self.pending_chip == chip;
                            let (rect, response) = ui
                                .allocate_exact_size(egui::vec2(182.0, 81.0), egui::Sense::click());
                            let fill = if selected || response.hovered() {
                                ACCENT_LIGHT
                            } else {
                                BACKGROUND
                            };
                            ui.painter().rect_filled(rect, 9.0, fill);
                            ui.painter().rect_stroke(
                                rect,
                                9.0,
                                egui::Stroke::new(1.0_f32, if selected { ACCENT } else { BORDER }),
                                egui::StrokeKind::Inside,
                            );
                            ui.painter().text(
                                rect.left_top() + egui::vec2(15.0, 18.0),
                                egui::Align2::LEFT_TOP,
                                chip.label(),
                                egui::FontId::proportional(17.0),
                                if selected { ACCENT } else { INK },
                            );
                            ui.painter().text(
                                rect.left_top() + egui::vec2(15.0, 47.0),
                                egui::Align2::LEFT_TOP,
                                format!("Bootloader 0x{:X}", chip.bootloader_address()),
                                egui::FontId::proportional(11.0),
                                MUTED,
                            );
                            if selected {
                                ui.painter().circle_filled(
                                    rect.right_top() + egui::vec2(-16.0, 18.0),
                                    4.0,
                                    ACCENT,
                                );
                            }
                            if response.clicked() {
                                self.pending_chip = chip;
                            }
                            if (index + 1) % 3 == 0 {
                                ui.end_row();
                            }
                        }
                    });
                ui.add_space(16.0);
                ui.label(
                    RichText::new(
                        "切换后保留已选文件，并更新默认 Bootloader 地址。请核对固件说明。",
                    )
                    .color(MUTED)
                    .size(12.0),
                );
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, "应用芯片选择", [152.0, 40.0]).clicked() {
                        apply = true;
                    }
                    if secondary_button(ui, "取消", [84.0, 40.0]).clicked() {
                        close = true;
                    }
                });
            });
        if apply {
            self.apply_chip(self.pending_chip);
        }
        if apply || close || response.should_close() {
            self.chip_picker_open = false;
        }
    }

    fn firmware_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            panel_title(ui, Icon::File, "本批烧录配置");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.chip_panel(ui)
            });
        });
        ui.add_space(5.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (layout, label) in [
                (FirmwareLayout::Merged, "合并固件"),
                (FirmwareLayout::Segments, "分段固件"),
            ] {
                let selected = self.layout == layout;
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).size(12.0).color(if selected {
                            ACCENT
                        } else {
                            MUTED
                        }))
                        .fill(if selected { ACCENT_LIGHT } else { SURFACE })
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(6.0)
                        .min_size(egui::vec2(84.0, 30.0)),
                    )
                    .clicked()
                {
                    self.set_layout(layout);
                }
            }
            ui.separator();
            ui.label(RichText::new("波特率").size(11.0).color(MUTED));
            if baud_editor(ui, &mut self.baud_input, "flash_baud", 115_200).changed()
                && let Ok(baud) = parse_baud_input(&self.baud_input, 115_200)
            {
                self.baud = baud;
            }
            ui.label(RichText::new("并发").size(11.0).color(MUTED));
            ui.add_sized(
                [76.0, 30.0],
                egui::DragValue::new(&mut self.concurrency)
                    .range(1..=16)
                    .suffix(" 台"),
            );
            if self.layout == FirmwareLayout::Segments && text_button(ui, "+ 添加固件段").clicked()
            {
                self.firmware.push(FirmwareRow::empty("自定义固件", "0x0"));
            }
        });
        ui.add_space(3.0);
        let mut remove = None;
        let segmented = self.layout == FirmwareLayout::Segments;
        let address_font = egui::FontId::monospace(13.0);
        let field_height = ui.fonts(|fonts| fonts.row_height(&address_font)).max(18.0) + 12.0;
        let label_width = self
            .firmware
            .iter()
            .map(|row| {
                ui.fonts(|fonts| {
                    fonts
                        .layout_no_wrap(row.label.to_owned(), egui::FontId::proportional(15.0), INK)
                        .size()
                        .x
                })
            })
            .fold(94.0_f32, f32::max)
            .ceil();
        let mut file_drop = FirmwareFileDrop::read(ui.ctx());
        for (index, row) in self.firmware.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                ui.horizontal(|ui| {
                    green_checkbox(ui, &mut row.enabled, "");
                    if segmented {
                        ui.add_sized(
                            [label_width, field_height],
                            egui::Label::new(
                                RichText::new(row.label).heading().size(15.0).strong(),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                    let width =
                        (ui.available_width() - if segmented { 190.0 } else { 160.0 }).max(160.0);
                    let picker = ui
                        .allocate_ui_with_layout(
                            egui::vec2(width, field_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| file_picker(ui, &row.path, field_height),
                        )
                        .inner;
                    if file_drop.is_hovering(ui, picker.rect) {
                        ui.painter().rect_stroke(
                            picker.rect,
                            4.0,
                            egui::Stroke::new(2.0_f32, ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if let Some(result) = file_drop.take_for_target(ui, picker.rect) {
                        match result {
                            Ok(path) => row.path = path,
                            Err(error) => self.error = Some(error),
                        }
                        ui.ctx().request_repaint();
                    }
                    if picker.clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("固件 BIN", &["bin"])
                            .add_filter("所有文件", &["*"])
                            .pick_file()
                    {
                        row.path = path;
                        ui.ctx().request_repaint();
                    }
                    ui.label(RichText::new("地址").size(11.0).color(MUTED));
                    ui.add_sized(
                        [89.0, field_height],
                        egui::TextEdit::singleline(&mut row.address)
                            .font(address_font.clone())
                            .margin(egui::vec2(7.0, 6.0))
                            .min_size(egui::vec2(89.0, field_height)),
                    );
                    if segmented && text_button(ui, "移除").clicked() {
                        remove = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove {
            self.firmware.remove(index);
        }
    }

    fn ports_panel(&mut self, ui: &mut egui::Ui) {
        let mut requested_logs = Vec::new();
        let locked = self.busy();
        let online = self.ports.iter().filter(|row| row.online).count();
        let selected = self
            .ports
            .iter()
            .filter(|row| row.selected && row.online)
            .count();
        ui.horizontal(|ui| {
            panel_title(ui, Icon::Usb, "在线 COM 设备");
            ui.label(
                RichText::new(format!("在线 {online}  /  已选 {selected}"))
                    .size(11.0)
                    .color(MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, "刷新串口", [88.0, 31.0]).clicked() {
                    self.scan_ports();
                }
                if ui
                    .add_enabled_ui(self.watcher.is_some(), |ui| {
                        green_checkbox(
                            ui,
                            &mut self.auto_refresh,
                            RichText::new("设备插拔自动识别").size(11.0),
                        )
                    })
                    .inner
                    .changed()
                {
                    self.scan_schedule.disable_auto();
                    if self.auto_refresh {
                        if let Some(watcher) = &self.watcher {
                            watcher.take_pending();
                        }
                        self.scan_ports();
                    }
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!locked, |ui| {
                if text_button(ui, "选择已识别 USB").clicked() {
                    for row in &mut self.ports {
                        row.selected = row.online && row.port.usb && row.port.suggested;
                    }
                }
                if text_button(ui, "清空选择").clicked() {
                    for row in &mut self.ports {
                        row.selected = false;
                    }
                }
            });
            if self.scanner.is_scanning() {
                ui.spinner();
                ui.label(RichText::new("正在后台扫描").size(10.0).color(MUTED));
            } else if !self.last_scan.is_empty() {
                ui.label(
                    RichText::new(format!("上次识别 {}", self.last_scan))
                        .size(10.0)
                        .color(MUTED),
                );
            }
        });
        if let Some(error) = &self.watcher_error {
            ui.add(
                egui::Label::new(
                    RichText::new("手动刷新模式：设备插拔监听不可用，请使用“刷新串口”。")
                        .size(10.0)
                        .color(MUTED),
                )
                .wrap(),
            )
            .on_hover_text(error);
        }
        if let Some(error) = &self.scan_error {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("{error}；保留上次设备状态。"))
                        .size(11.0)
                        .color(DANGER),
                )
                .wrap(),
            );
        }
        if self.ports.is_empty() {
            egui::Frame::new()
                .fill(BACKGROUND)
                .corner_radius(4.0)
                .inner_margin(20.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        icon(ui, Icon::Usb, 29.0, Color32::from_rgb(137, 158, 180));
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(if self.scanner.is_scanning() {
                                    "正在识别 COM 设备"
                                } else {
                                    "尚未发现 COM 设备"
                                })
                                .size(16.0)
                                .strong(),
                            );
                            ui.label(
                                RichText::new(if self.auto_refresh {
                                    "连接 USB Hub 与设备后即可识别；首次使用可先安装串口驱动。"
                                } else {
                                    "连接设备后点击“刷新串口”；首次使用可先安装串口驱动。"
                                })
                                .size(11.0)
                                .color(MUTED),
                            );
                        });
                    });
                });
        } else {
            let columns = if ui.available_width() >= 780.0 { 3 } else { 2 };
            let jobs = &self.jobs;
            let batch_ports = &self.batch_port_identities;
            let device_logs = &self.device_logs;
            egui::ScrollArea::vertical()
                .id_salt("device_workstations")
                .max_height(360.0)
                .show(ui, |ui| {
                    for chunk in self.ports.chunks_mut(columns) {
                        ui.columns(columns, |uis| {
                            for (index, row) in chunk.iter_mut().enumerate() {
                                let in_batch =
                                    batch_ports.get(&row.port.name) == Some(&row.port.identity);
                                let job = if in_batch {
                                    jobs.get(&row.port.name)
                                } else {
                                    None
                                };
                                if device_tile(&mut uis[index], row, job, in_batch, locked, false) {
                                    requested_logs.push(device_logs.key_for_port(&row.port));
                                }
                            }
                        });
                        ui.add_space(7.0);
                    }
                });
        }
        let missing_records =
            missing_batch_jobs(&self.ports, &self.batch_port_snapshots, &self.jobs);
        if !missing_records.is_empty() {
            ui.add_space(9.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("本批记录 · 已离线")
                        .size(12.0)
                        .strong()
                        .color(MUTED),
                );
                ui.label(
                    RichText::new(format!("{} 台", missing_records.len()))
                        .size(10.0)
                        .color(MUTED),
                );
            });
            ui.label(
                RichText::new("以下为本批设备的只读任务记录；这些设备已从在线 COM 列表移除。")
                    .size(10.0)
                    .color(MUTED),
            );
            let columns = if ui.available_width() >= 780.0 { 3 } else { 2 };
            egui::ScrollArea::vertical()
                .id_salt("offline_batch_records")
                .max_height(270.0)
                .show(ui, |ui| {
                    for chunk in missing_records.chunks(columns) {
                        ui.columns(columns, |uis| {
                            for (index, record) in chunk.iter().enumerate() {
                                let mut history = PortRow {
                                    port: record.port.clone(),
                                    selected: false,
                                    online: false,
                                };
                                if device_tile(
                                    &mut uis[index],
                                    &mut history,
                                    record.job,
                                    true,
                                    true,
                                    true,
                                ) {
                                    requested_logs
                                        .push(DeviceLogKey::from_port(record.port, &self.batch_id));
                                }
                            }
                        });
                        ui.add_space(7.0);
                    }
                });
        }
        self.log_windows.extend(requested_logs);
        if self.running() {
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "本批端口已锁定。新接入设备不会加入当前批次；离线任务在只读记录中继续显示。",
                )
                .size(10.0)
                .color(MUTED),
            );
        }
    }

    fn actions_panel(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
        let reading = self.batch_operation == RecordOperation::ReadOnly;
        let selected = self
            .ports
            .iter()
            .filter(|row| row.selected && row.online)
            .count();
        let success = self
            .jobs
            .values()
            .filter(|row| row.state == JobState::Success)
            .count();
        let failed = self
            .jobs
            .values()
            .filter(|row| row.state == JobState::Failed)
            .count();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                if self.jobs.is_empty() {
                    ui.label(
                        RichText::new(format!("已选择 {selected} 台在线设备"))
                            .size(16.0)
                            .strong(),
                    );
                } else {
                    ui.label(
                        RichText::new(format!(
                            "成功 {success}   /   失败 {failed}   /   共 {} 台",
                            self.jobs.len()
                        ))
                        .size(14.0)
                        .strong(),
                    );
                    ui.label(
                        RichText::new(match (running, reading) {
                            (true, true) => "正在读取所选设备的日志，请保持连接。",
                            (true, false) => "请保持设备连接，当前任务会完成校验。",
                            (false, true) => "本批读取已结束。",
                            (false, false) => "本批已结束，可调整配置继续烧录。",
                        })
                        .size(11.0)
                        .color(MUTED),
                    );
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if running && reading {
                    "正在读取日志…"
                } else if running {
                    "批次正在运行…"
                } else if self.startup_log.is_some() {
                    "正在读取启动日志…"
                } else if self.validation_running() {
                    "正在批量验证…"
                } else if self.driver_busy {
                    "等待驱动安装…"
                } else {
                    "开始批量烧录  >"
                };
                if ui
                    .add_enabled_ui(!self.busy() && selected > 0, |ui| {
                        primary_button(ui, label, [205.0, 47.0])
                    })
                    .inner
                    .clicked()
                {
                    self.start_batch();
                }
                if running {
                    if secondary_button(ui, "停止排队", [100.0, 47.0]).clicked()
                        && let Some(batch) = &self.batch
                    {
                        batch.cancel_queued();
                        self.notice = Some(if reading {
                            "已停止排队。当前设备继续读取，请保持连接。".into()
                        } else {
                            "已停止排队；正在烧录的设备将完成写入与校验，请保持连接。".into()
                        });
                    }
                } else {
                    if ui
                        .add_enabled_ui(!self.busy() && selected > 0, |ui| {
                            secondary_button(ui, "批量读取日志", [112.0, 47.0])
                        })
                        .inner
                        .clicked()
                    {
                        self.device_read_setup.open = true;
                    }
                    if !self.jobs.is_empty() && text_button(ui, "导出 CSV").clicked() {
                        self.export_csv();
                    }
                }
            });
        });
    }

    fn logs_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            panel_title(
                ui,
                Icon::Grid,
                if self.batch_operation == RecordOperation::ReadOnly {
                    "读取汇总"
                } else {
                    "烧录汇总"
                },
            );
            ui.label(
                RichText::new(format!("{} 条", self.logs.len()))
                    .size(10.0)
                    .color(MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled_ui(!self.logs.is_empty(), |ui| {
                        secondary_button(ui, "清空汇总", [88.0, 29.0])
                    })
                    .inner
                    .clicked()
                {
                    self.logs.clear();
                }
                if text_button(
                    ui,
                    if self.logs_visible {
                        "收起"
                    } else {
                        "展开"
                    },
                )
                .clicked()
                {
                    self.logs_visible = !self.logs_visible;
                }
                if !self.jobs.is_empty()
                    && ui
                        .add_enabled_ui(!self.running(), |ui| {
                            text_button(ui, "导出本批 CSV")
                                .on_disabled_hover_text("批次结束后可导出完整结果")
                        })
                        .inner
                        .clicked()
                {
                    self.export_csv();
                }
            });
        });
        if !self.jobs.is_empty() && self.batch_operation == RecordOperation::Flash {
            ui.label(
                RichText::new(format!(
                    "本批 {} · {} · 固件 SHA256 {}…",
                    self.batch_chip.label(),
                    self.batch_started,
                    self.firmware_hash.chars().take(12).collect::<String>()
                ))
                .size(10.0)
                .color(MUTED),
            )
            .on_hover_text(&self.firmware_hash);
        }
        if self.logs_visible {
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(NAVY)
                .corner_radius(4.0)
                .inner_margin(11.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.set_min_height(52.0);
                    egui::ScrollArea::vertical()
                        .id_salt("logs_scroll")
                        .max_height(145.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if self.logs.is_empty() {
                                ui.label(
                                    RichText::new(
                                        if self.batch_operation == RecordOperation::ReadOnly {
                                            "暂无读取结果"
                                        } else {
                                            "暂无烧录结果"
                                        },
                                    )
                                    .size(11.0)
                                    .color(Color32::from_rgb(126, 149, 172)),
                                );
                            }
                            for line in &self.logs {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(line)
                                            .monospace()
                                            .size(11.0)
                                            .color(Color32::from_rgb(190, 215, 221)),
                                    )
                                    .wrap(),
                                );
                            }
                        });
                });
        }
    }

    fn export_csv(&mut self) {
        if self.jobs.is_empty() || self.running() {
            return;
        }
        let settings = &self.batch_telemetry_settings;
        let rules = match telemetry::CompiledExtractionRules::new(&settings.extraction_rules) {
            Ok(rules) => rules,
            Err(error) => {
                self.error = Some(format!("无法导出字段：{error:#}"));
                return;
            }
        };
        let reading = self.batch_operation == RecordOperation::ReadOnly;
        let mut dialog = rfd::FileDialog::new()
            .set_title("导出本批设备结果")
            .set_file_name(format!(
                "flashtool-{}.csv",
                beijing_now().format("%Y%m%d-%H%M%S")
            ))
            .add_filter("CSV 表格", &["csv"]);
        if let Some(directory) = &settings.csv_directory {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.save_file() else {
            return;
        };
        let mut output = String::from("\u{feff}");
        let header = [
            "批次开始",
            "串口",
            "芯片",
            "批次ID",
            "eFuse出厂MAC",
            "MAC",
            "状态",
            "进度",
            "开始时间",
            "结束时间",
            "耗时秒",
            "详情",
            "固件",
            "固件SHA256",
        ];
        let mut custom_keys: BTreeSet<_> = settings
            .extraction_rules
            .keys()
            .map(String::as_str)
            .collect();
        custom_keys.insert(telemetry::UNIQUE_DEVICE_ID_KEY);
        let production_values = self.batch_production.as_ref().map(|production| {
            [
                production.work_order.clone(),
                production.board_week.clone(),
                production.board_date_text(),
            ]
        });
        append_csv(
            &mut output,
            header.into_iter().chain(custom_keys.iter().copied()).chain(
                production_values
                    .as_ref()
                    .map(|_| PRODUCTION_HEADERS)
                    .into_iter()
                    .flatten(),
            ),
        );
        for (port, row) in &self.jobs {
            let progress = format!("{:.1}%", row.progress * 100.0);
            let duration = row
                .duration_secs
                .map(|v| format!("{v:.3}"))
                .unwrap_or_default();
            let logs = self
                .device_logs
                .current_lines
                .get(port)
                .map_or(&[][..], Vec::as_slice);
            let mut fields = rules.extract_record(logs, &settings.custom_headers);
            if let Some(unique_device_id) = &row.unique_device_id
                && let Err(error) =
                    telemetry::apply_hardware_unique_id(&mut fields, unique_device_id)
            {
                self.error = Some(format!("无法导出 {port} 的设备 ID：{error:#}"));
                return;
            }
            append_csv(
                &mut output,
                [
                    self.batch_started.as_str(),
                    port.as_str(),
                    if reading { "" } else { self.batch_chip.label() },
                    self.batch_id.as_str(),
                    row.chip_identity
                        .as_ref()
                        .and_then(|identity| identity.efuse_id.as_deref())
                        .unwrap_or(""),
                    row.chip_identity
                        .as_ref()
                        .and_then(|identity| identity.mac.as_deref())
                        .unwrap_or(""),
                    state_label(&row.state),
                    progress.as_str(),
                    row.started.as_str(),
                    row.finished.as_str(),
                    duration.as_str(),
                    row.message.as_str(),
                    self.batch_firmware.as_str(),
                    self.firmware_hash.as_str(),
                ]
                .into_iter()
                .chain(
                    custom_keys
                        .iter()
                        .map(|key| fields.custom_fields.get(*key).map_or("", String::as_str)),
                )
                .chain(production_values.iter().flatten().map(String::as_str)),
            );
        }
        match fs::write(&path, output) {
            Ok(()) => self.notice = Some(format!("结果已导出：{}", path.display())),
            Err(err) => self.error = Some(format!("导出失败：{err}")),
        }
    }
}

impl eframe::App for FlashApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.collect_events();
        self.poll_startup_log();
        self.poll_validation();
        self.poll_ble();
        self.poll_records();
        self.poll_software_reset(ctx);
        self.collect_update_events(ctx);
        self.poll_self_update(ctx);
        self.poll_ports();
        let busy = self.busy();
        if ctx.input(|input| input.viewport().close_requested()) && !self.allow_exit {
            if self.installer_preparing {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.notice = Some("正在准备更新并检查写入权限，请稍候。".into());
            } else if self.ble.is_connected() && !self.ble.capturing() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.disconnect_ble();
                self.notice = Some("正在断开蓝牙工装，请稍候再关闭。".into());
            } else if busy {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.error = Some("任务正在进行，请等待当前任务完成后关闭。".into());
            } else if self.record_service.pending_local_count() > 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.notice = Some("正在保存本地设备记录，请稍候再关闭。".into());
            } else if self.record_service.pending_count() > 0
                || !self.pending_records.is_empty()
                || !self.failed_records.is_empty()
                || self.validation.save_error.is_some()
                || self.ble.has_unsaved()
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_confirm_open = true;
            }
        }
        egui::SidePanel::left("workspace_sidebar")
            .exact_width(174.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(NAVY)
                    .inner_margin(egui::Margin::symmetric(17, 19)),
            )
            .show(ctx, |ui| self.sidebar(ui));
        egui::TopBottomPanel::bottom("batch_actionbar")
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .inner_margin(egui::Margin::symmetric(16, 11)),
            )
            .show(ctx, |ui| match self.workspace {
                Workspace::Flash => self.actions_panel(ui),
                Workspace::Validation => self.validation_actions(ui),
                Workspace::Bluetooth => self.ble_actions(ui),
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(18, 15)),
            )
            .show(ctx, |ui| {
                self.heading(ui);
                ui.add_space(15.0);
                egui::ScrollArea::vertical()
                    .id_salt(match self.workspace {
                        Workspace::Flash => "workspace_flash",
                        Workspace::Validation => "workspace_validation",
                        Workspace::Bluetooth => "workspace_bluetooth",
                    })
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(message) = &self.error {
                            banner(ui, message, true);
                            ui.add_space(12.0);
                        }
                        if let Some(message) = &self.notice {
                            banner(ui, message, false);
                            ui.add_space(12.0);
                        }
                        self.production_input_panel(ui, !busy);
                        ui.add_space(12.0);
                        match self.workspace {
                            Workspace::Flash => {
                                card(ui, |ui| {
                                    ui.add_enabled_ui(!busy, |ui| self.firmware_panel(ui));
                                });
                                ui.add_space(12.0);
                                card(ui, |ui| self.ports_panel(ui));
                                ui.add_space(12.0);
                                card(ui, |ui| self.logs_panel(ui));
                            }
                            Workspace::Validation => self.validation_panel(ui),
                            Workspace::Bluetooth => self.ble_panel(ui),
                        }
                        ui.add_space(8.0);
                    });
            });
        if !busy && !self.software_reset.open {
            self.chip_dialog(ctx);
            self.driver_license_dialog(ctx);
            self.device_read_dialog(ctx);
        }
        if self.update_prompt_pending
            && !self.software_reset.open
            && !self.close_confirm_open
            && self.pending_driver_license.is_none()
            && !self.chip_picker_open
            && !self.device_read_setup.open
            && !self.validation.result_open
            && !self.ble.result_open
        {
            self.open_update_dialog();
        }
        if !self.software_reset.open {
            self.update_dialog(ctx);
            self.settings_window(ctx);
            self.device_log_windows(ctx);
            if !self.close_confirm_open {
                self.validation_result_dialog(ctx);
                if !self.validation.result_open && !self.update_dialog_open && !self.settings_open {
                    self.ble_result_dialog(ctx);
                }
            }
        }
        self.confirm_record_exit(ctx);
        self.software_reset_dialog(ctx);
        if self.busy() || self.update_checking || self.scanner.is_scanning() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if !self.scanner.is_scanning()
            && let Some(delay) = self.scan_schedule.repaint_delay(Instant::now())
        {
            ctx.request_repaint_after(delay);
        }
    }
}

fn restart_safety_reason(
    running: bool,
    driver_busy: bool,
    local_pending: usize,
    queued_records: usize,
    failed_records: usize,
) -> Option<&'static str> {
    if running || driver_busy {
        Some("当前任务未结束，完成后可重启软件。")
    } else if local_pending > 0 || queued_records > 0 {
        Some("本地 CSV 正在保存，完成后可重启软件。")
    } else if failed_records > 0 {
        Some("仍有本地记录未保存，请先到设置中重试保存。")
    } else {
        None
    }
}

fn header_drag_region(
    header: egui::Rect,
    title: egui::Rect,
    controls_left: f32,
) -> Option<egui::Rect> {
    let left = title.right() + 12.0;
    let right = controls_left - 12.0;
    (right > left).then(|| {
        egui::Rect::from_min_max(
            egui::pos2(left, header.top()),
            egui::pos2(right, header.bottom()),
        )
    })
}

fn terminal_summary(port: &str, state: &JobState, operation: RecordOperation) -> Option<String> {
    let action = match operation {
        RecordOperation::Flash => "烧录",
        RecordOperation::ReadOnly => "读取日志",
    };
    match state {
        JobState::Success => Some(format!("{port}{action}成功")),
        JobState::Failed => Some(format!("{port}{action}失败")),
        JobState::Cancelled => Some(format!("{port}已取消{action}")),
        _ => None,
    }
}

fn nav_item(ui: &mut egui::Ui, kind: Icon, title: &str, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 43.0), egui::Sense::click());
    if selected || response.hovered() {
        ui.painter()
            .rect_filled(rect, 8.0, if selected { SURFACE_ALT } else { SURFACE });
    }
    let color = if selected { INK } else { MUTED };
    paint_icon(
        ui.painter(),
        rect.left_center() + egui::vec2(20.0, 0.0),
        17.0,
        kind,
        color,
    );
    ui.painter().text(
        rect.left_center() + egui::vec2(39.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(16.0),
        color,
    );
    response
}

fn terminal_icon_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(30.0, 25.0), egui::Sense::click());
    ui.painter().rect_filled(
        rect,
        5.0,
        if response.hovered() {
            ACCENT_LIGHT
        } else {
            SURFACE
        },
    );
    ui.painter().rect_stroke(
        rect,
        5.0,
        egui::Stroke::new(1.0_f32, BORDER),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ">_",
        egui::FontId::monospace(12.0),
        ACCENT,
    );
    response
}

fn settings_icon_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(36.0, 36.0), egui::Sense::click());
    ui.painter().rect_filled(
        rect,
        4.0,
        if response.hovered() {
            SURFACE_ALT
        } else {
            SURFACE
        },
    );
    paint_icon(
        ui.painter(),
        rect.center(),
        21.0,
        Icon::Settings,
        if response.hovered() { ACCENT } else { MUTED },
    );
    response
}

fn open_records_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let command = "explorer.exe";
    #[cfg(not(windows))]
    let command = "xdg-open";
    std::process::Command::new(command)
        .arg(path)
        .spawn()
        .map(|_| ())
}

fn reconcile_scan_result(
    previous: Vec<PortRow>,
    observed: Result<Vec<Port>, String>,
) -> (Vec<PortRow>, Option<String>) {
    match observed {
        Ok(ports) => (reconcile_ports(previous, ports), None),
        Err(error) => (previous, Some(error)),
    }
}

fn reconcile_ports(previous: Vec<PortRow>, observed: Vec<Port>) -> Vec<PortRow> {
    let mut found: HashMap<_, _> = observed
        .into_iter()
        .map(|port| ((port.name.clone(), port.identity.clone()), port))
        .collect();
    let mut rows = Vec::new();
    for mut row in previous {
        let identity = (row.port.name.clone(), row.port.identity.clone());
        if let Some(port) = found.remove(&identity) {
            if !row.online {
                row.selected = false;
            }
            row.port = port;
            row.online = true;
            rows.push(row);
        }
    }
    rows.extend(found.into_values().map(|port| PortRow {
        port,
        selected: false,
        online: true,
    }));
    rows.sort_by(|left, right| {
        let number = |name: &str| {
            name.strip_prefix("COM")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        };
        number(&left.port.name)
            .cmp(&number(&right.port.name))
            .then_with(|| left.port.name.cmp(&right.port.name))
            .then_with(|| left.port.description.cmp(&right.port.description))
    });
    rows
}

struct MissingJob<'a> {
    port: &'a Port,
    job: Option<&'a JobRow>,
}

fn missing_batch_jobs<'a>(
    online: &[PortRow],
    snapshots: &'a [Port],
    jobs: &'a BTreeMap<String, JobRow>,
) -> Vec<MissingJob<'a>> {
    snapshots
        .iter()
        .filter(|expected| {
            !online
                .iter()
                .any(|row| row.port.name == expected.name && row.port.identity == expected.identity)
        })
        .map(|port| MissingJob {
            port,
            job: jobs.get(&port.name),
        })
        .collect()
}

fn device_tile(
    ui: &mut egui::Ui,
    row: &mut PortRow,
    job: Option<&JobRow>,
    in_batch: bool,
    locked: bool,
    history: bool,
) -> bool {
    let mut open_log = false;
    let progress = job.map_or(0.0, |job| job.progress);
    let stage = job.map_or_else(
        || {
            if in_batch && locked {
                "等待中"
            } else if !row.online {
                "设备已离线"
            } else if row.selected {
                "准备就绪"
            } else {
                "未选择"
            }
        },
        |job| state_label(&job.state),
    );
    let color = match job.map(|job| &job.state) {
        Some(JobState::Failed) => DANGER,
        Some(JobState::Success | JobState::Writing | JobState::Verifying) => SUCCESS,
        _ => MUTED,
    };
    egui::Frame::new()
        .fill(if row.selected || in_batch {
            Color32::from_rgb(35, 48, 60)
        } else {
            BACKGROUND
        })
        .stroke(egui::Stroke::new(
            1.0_f32,
            if row.selected || in_batch {
                Color32::from_rgb(36, 88, 132)
            } else {
                BORDER
            },
        ))
        .corner_radius(4.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                if history {
                    ui.label(
                        RichText::new(&row.port.name)
                            .monospace()
                            .size(15.0)
                            .strong(),
                    );
                } else {
                    ui.add_enabled_ui(!locked && row.online, |ui| {
                        green_checkbox(
                            ui,
                            &mut row.selected,
                            RichText::new(&row.port.name)
                                .monospace()
                                .size(15.0)
                                .strong(),
                        )
                    });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if terminal_icon_button(ui)
                        .on_hover_text(format!("打开 {} 详细日志", row.port.name))
                        .clicked()
                    {
                        open_log = true;
                    }
                    ui.label(
                        RichText::new(if row.online { "在线" } else { "已离线" })
                            .size(10.0)
                            .color(if row.online { ACCENT } else { DANGER }),
                    );
                    dot(ui, if row.online { ACCENT } else { DANGER }, 4.0);
                });
            });
            ui.add(
                egui::Label::new(RichText::new(&row.port.description).size(10.0).color(MUTED))
                    .truncate(),
            )
            .on_hover_text(&row.port.description);
            ui.horizontal(|ui| {
                ui.label(RichText::new(stage).size(12.0).strong().color(color));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.0}%", progress * 100.0))
                            .monospace()
                            .size(13.0)
                            .strong()
                            .color(color),
                    );
                });
            });
            ui.add(
                egui::ProgressBar::new(progress)
                    .desired_width(ui.available_width())
                    .desired_height(7.0)
                    .fill(color),
            );
            ui.horizontal(|ui| {
                let elapsed = job.and_then(|job| {
                    job.duration_secs
                        .or_else(|| job.elapsed.map(|start| start.elapsed().as_secs_f64()))
                });
                ui.label(
                    RichText::new(
                        elapsed
                            .map(|seconds| format!("耗时 {seconds:.1} s"))
                            .unwrap_or_else(|| "耗时 —".into()),
                    )
                    .size(10.0)
                    .color(MUTED),
                );
                if in_batch {
                    ui.label(
                        RichText::new(if history {
                            "本批记录 · 只读"
                        } else {
                            "本批任务"
                        })
                        .size(10.0)
                        .color(ACCENT),
                    );
                }
            });
            let message = job
                .map(|job| job.message.as_str())
                .unwrap_or(if !row.online {
                    "本批设备已离线，等待任务状态更新"
                } else if row.selected {
                    "等待开始本批烧录"
                } else {
                    "勾选后加入下一批"
                });
            ui.add(
                egui::Label::new(RichText::new(message).size(10.0).color(
                    if job.is_some_and(|job| job.state == JobState::Failed) {
                        DANGER
                    } else {
                        MUTED
                    },
                ))
                .truncate(),
            )
            .on_hover_text(message);
        });
    open_log
}

#[derive(Clone, Copy)]
enum Icon {
    Bluetooth,
    Chip,
    Usb,
    File,
    Grid,
    Shield,
    Settings,
}

fn paint_icon(painter: &egui::Painter, center: egui::Pos2, size: f32, kind: Icon, color: Color32) {
    let p = |x: f32, y: f32| center + egui::vec2(x * size, y * size);
    let stroke = egui::Stroke::new(1.65_f32, color);
    match kind {
        Icon::Bluetooth => {
            painter.add(egui::Shape::line(
                vec![
                    p(-0.28, -0.24),
                    p(0.28, 0.25),
                    p(0.0, 0.47),
                    p(0.0, -0.47),
                    p(0.28, -0.25),
                    p(-0.28, 0.24),
                ],
                stroke,
            ));
        }
        Icon::Settings => {
            painter.circle_stroke(center, size * 0.29, stroke);
            painter.circle_stroke(center, size * 0.10, stroke);
            for step in 0..8 {
                let angle = step as f32 * std::f32::consts::TAU / 8.0;
                let direction = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [
                        center + direction * size * 0.30,
                        center + direction * size * 0.45,
                    ],
                    stroke,
                );
            }
        }
        Icon::Chip => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(size * 0.58, size * 0.58)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(size * 0.22, size * 0.22)),
                1.5,
                color,
            );
            for coordinate in [-0.16, 0.0, 0.16] {
                for sign in [-1.0, 1.0] {
                    painter.line_segment(
                        [p(coordinate, sign * 0.3), p(coordinate, sign * 0.47)],
                        stroke,
                    );
                    painter.line_segment(
                        [p(sign * 0.3, coordinate), p(sign * 0.47, coordinate)],
                        stroke,
                    );
                }
            }
        }
        Icon::Usb => {
            painter.line_segment([p(0.0, 0.39), p(0.0, -0.39)], stroke);
            painter.circle_stroke(p(0.0, 0.39), size * 0.08, stroke);
            painter.line_segment([p(0.0, 0.1), p(-0.3, -0.08)], stroke);
            painter.line_segment([p(-0.3, -0.08), p(-0.3, -0.3)], stroke);
            painter.line_segment([p(0.0, -0.05), p(0.3, -0.22)], stroke);
            painter.line_segment([p(0.3, -0.22), p(0.3, -0.39)], stroke);
            painter.circle_filled(p(-0.3, -0.31), size * 0.065, color);
            painter.rect_filled(
                egui::Rect::from_center_size(p(0.3, -0.4), egui::vec2(size * 0.13, size * 0.13)),
                0.5,
                color,
            );
            painter.line_segment([p(-0.09, -0.3), p(0.0, -0.42)], stroke);
            painter.line_segment([p(0.09, -0.3), p(0.0, -0.42)], stroke);
        }
        Icon::File => {
            painter.rect_stroke(
                egui::Rect::from_min_max(p(-0.29, -0.41), p(0.29, 0.41)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            for y in [-0.12, 0.04, 0.2] {
                painter.line_segment([p(-0.14, y), p(0.14, y)], stroke);
            }
        }
        Icon::Grid => {
            for x in [-0.23, 0.23] {
                for y in [-0.23, 0.23] {
                    painter.rect_stroke(
                        egui::Rect::from_center_size(p(x, y), egui::vec2(size * 0.3, size * 0.3)),
                        1.5,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
        Icon::Shield => {
            let points = vec![
                p(0.0, -0.44),
                p(0.35, -0.28),
                p(0.29, 0.18),
                p(0.0, 0.44),
                p(-0.29, 0.18),
                p(-0.35, -0.28),
            ];
            painter.add(egui::Shape::closed_line(points, stroke));
            painter.line_segment([p(-0.14, 0.0), p(-0.02, 0.13)], stroke);
            painter.line_segment([p(-0.02, 0.13), p(0.19, -0.13)], stroke);
        }
    }
}

fn icon(ui: &mut egui::Ui, kind: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    paint_icon(ui.painter(), rect.center(), size, kind, color);
}

fn dot(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
}

fn panel_title(ui: &mut egui::Ui, kind: Icon, title: &str) {
    ui.horizontal(|ui| {
        icon(ui, kind, 17.0, ACCENT);
        ui.label(
            RichText::new(title)
                .heading()
                .size(16.0)
                .strong()
                .color(INK),
        );
    });
}

fn green_checkbox(
    ui: &mut egui::Ui,
    value: &mut bool,
    label: impl Into<RichText>,
) -> egui::Response {
    ui.scope(|ui| {
        let label = label.into().color(ui.visuals().text_color());
        let widgets = &mut ui.visuals_mut().widgets;
        for state in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            state.fg_stroke = egui::Stroke::new(1.7_f32, SUCCESS);
        }
        ui.add(egui::Checkbox::new(value, label))
    })
    .inner
}

fn primary_button(ui: &mut egui::Ui, text: &str, size: [f32; 2]) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .size(14.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(ACCENT)
        .stroke(egui::Stroke::NONE)
        .corner_radius(4.0)
        .min_size(egui::vec2(size[0], size[1])),
    )
}

fn secondary_button(ui: &mut egui::Ui, text: &str, size: [f32; 2]) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(12.0).color(INK))
            .fill(SURFACE)
            .stroke(egui::Stroke::new(1.0_f32, BORDER))
            .corner_radius(4.0)
            .min_size(egui::vec2(size[0], size[1])),
    )
}

fn text_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(12.0).color(ACCENT)).frame(false))
}

fn parse_baud_input(input: &str, minimum: u32) -> Result<u32, String> {
    let minimum = minimum.max(1);
    let baud = input.trim().parse::<u32>().map_err(|_| {
        format!(
            "波特率无效，请输入 {minimum} 至 {} 的十进制整数。",
            u32::MAX
        )
    })?;
    if baud < minimum {
        return Err(format!("波特率不能低于 {minimum}。"));
    }
    Ok(baud)
}

fn baud_editor(ui: &mut egui::Ui, input: &mut String, id: &str, minimum: u32) -> egui::Response {
    let previous = input.clone();
    let mut response = ui.add_sized(
        [101.0, 30.0],
        egui::TextEdit::singleline(input)
            .id_salt((id, "input"))
            .font(egui::FontId::monospace(13.0))
            .margin(egui::vec2(7.0, 6.0))
            .hint_text("自定义"),
    );
    if let Err(error) = parse_baud_input(input, minimum) {
        ui.painter().rect_stroke(
            response.rect,
            4.0,
            egui::Stroke::new(1.0_f32, DANGER),
            egui::StrokeKind::Inside,
        );
        response = response.on_hover_text(error);
    } else {
        response = response.on_hover_text("可直接输入十进制波特率，或从右侧选择常用值。");
    }
    let presets = egui::ComboBox::from_id_salt((id, "presets"))
        .selected_text("常用")
        .width(62.0)
        .show_ui(ui, |ui| {
            for baud in [
                9_600_u32, 19_200, 57_600, 115_200, 230_400, 460_800, 921_600, 1_000_000,
                1_500_000, 2_000_000, 3_000_000,
            ] {
                if baud >= minimum
                    && ui
                        .selectable_label(input.trim() == baud.to_string(), baud.to_string())
                        .clicked()
                {
                    *input = baud.to_string();
                }
            }
        })
        .response;
    response = response.union(presets);
    if *input != previous {
        response.mark_changed();
    }
    response
}

fn file_picker(ui: &mut egui::Ui, path: &Path, height: f32) -> egui::Response {
    let empty = path.as_os_str().is_empty();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    ui.painter().rect_filled(
        rect,
        4.0,
        if response.hovered() {
            ACCENT_LIGHT
        } else {
            BACKGROUND
        },
    );
    ui.painter().rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0_f32, if response.hovered() { ACCENT } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    let filename = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "选择固件".into());
    let text_height = ui.fonts(|fonts| fonts.row_height(&egui::FontId::proportional(13.0)));
    paint_text_ellipsis(
        ui,
        &filename,
        rect.left_center() + egui::vec2(10.0, -text_height / 2.0),
        (rect.width() - 20.0).max(0.0),
        13.0,
        INK,
        false,
    );
    if empty {
        response
    } else {
        response.on_hover_text(path.display().to_string())
    }
}

fn paint_text_ellipsis(
    ui: &egui::Ui,
    text: &str,
    position: egui::Pos2,
    width: f32,
    size: f32,
    color: Color32,
    centered: bool,
) {
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.into(),
        egui::FontId::proportional(size),
        color,
    );
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    let position = if centered {
        position + egui::vec2((width - galley.size().x) / 2.0, 0.0)
    } else {
        position
    };
    ui.painter().galley(position, galley, color);
}

fn card<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0_f32, BORDER))
        .corner_radius(5.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            content(ui)
        });
}

fn banner(ui: &mut egui::Ui, message: &str, error: bool) {
    egui::Frame::new()
        .fill(if error {
            Color32::from_rgb(66, 35, 41)
        } else {
            ACCENT_LIGHT
        })
        .corner_radius(4.0)
        .inner_margin(13.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(message).size(12.0).color(if error {
                    DANGER
                } else {
                    ACCENT
                }))
                .wrap(),
            );
        });
}

fn update_error_panel(ui: &mut egui::Ui, ctx: &egui::Context, status: &str, message: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(66, 35, 41))
        .corner_radius(4.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(
                RichText::new("更新未完成")
                    .size(13.0)
                    .strong()
                    .color(DANGER),
            );
            egui::ScrollArea::vertical()
                .id_salt("update_error_details")
                .max_height(92.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(message).size(12.0).color(DANGER)).wrap(),
                    );
                });
        });
    ui.add_space(7.0);
    // Keep these actions outside every error/release ScrollArea.
    ui.horizontal_wrapped(|ui| {
        let copy = secondary_button(ui, "复制错误详情", [124.0, 34.0]);
        clipboard::copy_with_feedback(ui, copy, || {
            format!(
                "FlashTool {}\n{}\n{}",
                env!("CARGO_PKG_VERSION"),
                status,
                message
            )
        });
        if secondary_button(ui, "打开官方发布页", [140.0, 34.0]).clicked() {
            ctx.open_url(egui::OpenUrl::new_tab(updater::RELEASES_URL));
        }
    });
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn format_update_time(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0))
        .map(|time| beijing_datetime(time).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "时间无效".into())
}

fn timestamp() -> String {
    beijing_now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn state_label(state: &JobState) -> &'static str {
    match state {
        JobState::Queued => "等待中",
        JobState::Connecting => "连接设备",
        JobState::Reading => "正在读取",
        JobState::Writing => "正在写入",
        JobState::Verifying => "正在校验",
        JobState::Success => "成功",
        JobState::Failed => "失败",
        JobState::Cancelled => "已取消",
    }
}

fn is_terminal(state: &JobState) -> bool {
    matches!(
        state,
        JobState::Success | JobState::Failed | JobState::Cancelled
    )
}

fn append_csv<'a>(output: &mut String, values: impl IntoIterator<Item = &'a str>) {
    let mut first = true;
    for value in values {
        if !first {
            output.push(',');
        }
        first = false;
        output.push('"');
        // Neutralize spreadsheet formulas in file paths, device labels and error text.
        if value
            .trim_start_matches(|character: char| {
                character.is_whitespace() || character.is_control() || character == '\u{feff}'
            })
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@'))
            || value.starts_with(['\t', '\r', '\n'])
        {
            output.push('\'');
        }
        output.push_str(&value.replace('"', "\"\""));
        output.push('"');
    }
    output.push_str("\r\n");
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut load = |name: &str, path: PathBuf| -> bool {
        match fs::read(path) {
            Ok(bytes) => {
                fonts
                    .font_data
                    .insert(name.into(), egui::FontData::from_owned(bytes).into());
                true
            }
            Err(_) => false,
        }
    };
    let windows_fonts = std::env::var_os("WINDIR")
        .or_else(|| std::env::var_os("SystemRoot"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Windows"))
        .join("Fonts");
    let system_ui = cfg!(windows) && load("system-ui", windows_fonts.join("segoeui.ttf"));
    let system_bold = cfg!(windows) && load("system-ui-bold", windows_fonts.join("segoeuib.ttf"));
    let system_mono = cfg!(windows) && load("system-mono", windows_fonts.join("consola.ttf"));
    let mut cjk = cfg!(windows) && load("system-cjk", windows_fonts.join("msyh.ttc"));
    let cjk_bold = cfg!(windows) && load("system-cjk-bold", windows_fonts.join("msyhbd.ttc"));
    if !cjk {
        for path in [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        ] {
            if load("system-cjk", PathBuf::from(path)) {
                cjk = true;
                break;
            }
        }
    }
    let fallback = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut proportional = Vec::new();
    if system_ui {
        proportional.push("system-ui".into());
    }
    if cjk {
        proportional.push("system-cjk".into());
    }
    proportional.extend(fallback.clone());
    fonts
        .families
        .insert(egui::FontFamily::Proportional, proportional);
    let mut heading = Vec::new();
    if system_bold {
        heading.push("system-ui-bold".into());
    } else if system_ui {
        heading.push("system-ui".into());
    }
    if cjk_bold {
        heading.push("system-cjk-bold".into());
    } else if cjk {
        heading.push("system-cjk".into());
    }
    heading.extend(fallback);
    fonts
        .families
        .insert(egui::FontFamily::Name("ui-heading".into()), heading);
    let mut monospace = Vec::new();
    if system_mono {
        monospace.push("system-mono".into());
    }
    if cjk {
        monospace.push("system-cjk".into());
    }
    monospace.extend(
        fonts
            .families
            .get(&egui::FontFamily::Monospace)
            .cloned()
            .unwrap_or_default(),
    );
    fonts
        .families
        .insert(egui::FontFamily::Monospace, monospace);
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::{
        BatchEventKind, DeviceIdentity, DeviceLogBook, DeviceLogKey, JobRow, JobState, Port,
        PortRow, RecordOperation, ScanSchedule, append_csv, header_drag_region, missing_batch_jobs,
        parse_baud_input, reconcile_ports, reconcile_scan_result, restart_safety_reason,
        terminal_summary,
    };
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    #[test]
    fn custom_baud_accepts_decimal_values_and_respects_operation_minimum() {
        assert_eq!(parse_baud_input(" 256000 ", 115_200), Ok(256_000));
        assert_eq!(parse_baud_input("115200", 115_200), Ok(115_200));
        assert_eq!(parse_baud_input("9600", 1), Ok(9_600));
        assert_eq!(parse_baud_input("4294967295", 1), Ok(u32::MAX));
        assert!(parse_baud_input("115199", 115_200).is_err());
    }

    #[test]
    fn custom_baud_rejects_invalid_zero_negative_and_overflow_values() {
        for input in ["", " ", "0", "-1", "4294967296", "1.5", "1e6", "0x70800"] {
            assert!(parse_baud_input(input, 1).is_err(), "{input:?}");
        }
    }

    #[test]
    fn update_restart_waits_for_flash_driver_and_every_unsaved_local_record() {
        assert!(restart_safety_reason(true, false, 0, 0, 0).is_some());
        assert!(restart_safety_reason(false, true, 0, 0, 0).is_some());
        assert!(restart_safety_reason(false, false, 1, 0, 0).is_some());
        assert!(restart_safety_reason(false, false, 0, 1, 0).is_some());
        assert!(restart_safety_reason(false, false, 0, 0, 1).is_some());
        assert!(restart_safety_reason(false, false, 0, 0, 0).is_none());
    }

    #[test]
    fn device_logs_are_isolated_between_ports() {
        let a = port("COM11", "adapter A");
        let b = port("COM12", "adapter B");
        let mut logs = DeviceLogBook::default();
        logs.begin_batch(&[a.clone(), b.clone()], "batch-a", "esp32c3", "hash");
        logs.append(
            "COM11",
            &JobState::Connecting,
            BatchEventKind::Log,
            "only-device-a",
        );
        logs.append(
            "COM12",
            &JobState::Failed,
            BatchEventKind::State,
            "only-device-b",
        );
        let text = |key| {
            logs.history[&key]
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(text(DeviceLogKey::from_port(&a, "batch-a")).contains("only-device-a"));
        assert!(!text(DeviceLogKey::from_port(&a, "batch-a")).contains("only-device-b"));
        assert!(!text(DeviceLogKey::from_port(&b, "batch-a")).contains("only-device-a"));
    }

    #[test]
    fn new_batch_resets_extraction_logs_without_reassigning_old_device_history() {
        let old = port("COM3", "old device");
        let new = port("COM3", "new device");
        let mut logs = DeviceLogBook::default();
        logs.begin_batch(std::slice::from_ref(&old), "batch-old", "esp32c3", "hash");
        logs.append_line("COM3", "EFUSE_ID=102030405060 old-only".into());
        logs.remember_identity(
            "COM3",
            &DeviceIdentity {
                efuse_id: Some("102030405060".into()),
                mac: None,
            },
        );
        logs.begin_batch(std::slice::from_ref(&new), "batch-new", "esp32c3", "hash");
        assert!(!logs.current_lines["COM3"].join("\n").contains("old-only"));
        assert_eq!(
            logs.current_keys["COM3"],
            DeviceLogKey::from_port(&new, "batch-new")
        );
        assert!(
            logs.history[&DeviceLogKey::from_port(&old, "batch-old")]
                .iter()
                .any(|line| line.contains("old-only"))
        );
        assert!(
            !logs
                .identities
                .contains_key(&DeviceLogKey::from_port(&new, "batch-new"))
        );
    }

    #[test]
    fn read_only_records_contain_only_raw_serial_lines_for_each_port() {
        let first = port("COM3", "adapter A");
        let second = port("COM4", "adapter B");
        let mut logs = DeviceLogBook::default();
        logs.begin_read(&[first.clone(), second.clone()], "read-batch");
        logs.append_history("COM3", "正在读取串口日志".into());
        logs.append_line("COM4", "UNIQUE DEVICE ID: B".into());
        logs.append_line("COM3", "UNIQUE DEVICE ID: A".into());
        assert_eq!(logs.current_lines["COM3"], ["UNIQUE DEVICE ID: A"]);
        assert_eq!(logs.current_lines["COM4"], ["UNIQUE DEVICE ID: B"]);
        assert!(
            logs.history[&DeviceLogKey::from_port(&first, "read-batch")]
                .iter()
                .any(|line| line == "正在读取串口日志")
        );
    }

    #[test]
    fn same_adapter_new_batch_keeps_old_terminal_bound_to_old_run() {
        let device = port("COM3", "same adapter");
        let mut logs = DeviceLogBook::default();
        logs.begin_batch(
            std::slice::from_ref(&device),
            "batch-one",
            "esp32c3",
            "hash",
        );
        logs.append_line("COM3", "first chip".into());
        let old_window = DeviceLogKey::from_port(&device, "batch-one");
        logs.begin_batch(
            std::slice::from_ref(&device),
            "batch-two",
            "esp32c3",
            "hash",
        );
        logs.append_line("COM3", "second chip".into());
        assert_ne!(old_window, logs.current_keys["COM3"]);
        assert!(
            logs.history[&old_window]
                .iter()
                .all(|line| !line.contains("second chip"))
        );
    }

    #[test]
    fn progress_updates_do_not_spam_device_logs_or_terminal_summary() {
        let mut logs = DeviceLogBook::default();
        logs.begin_batch(&[port("COM11", "adapter")], "batch", "esp32c3", "hash");
        let initial = logs.current_lines["COM11"].len();
        for _ in 0..1_000 {
            logs.append(
                "COM11",
                &JobState::Writing,
                BatchEventKind::Progress,
                "progress",
            );
        }
        assert_eq!(logs.current_lines["COM11"].len(), initial);
        for state in [
            JobState::Queued,
            JobState::Connecting,
            JobState::Writing,
            JobState::Verifying,
        ] {
            assert!(terminal_summary("COM11", &state, RecordOperation::Flash).is_none());
        }
        assert_eq!(
            terminal_summary("COM11", &JobState::Success, RecordOperation::Flash).as_deref(),
            Some("COM11烧录成功")
        );
        assert_eq!(
            terminal_summary("COM12", &JobState::Failed, RecordOperation::Flash).as_deref(),
            Some("COM12烧录失败")
        );
    }

    #[test]
    fn raw_log_limit_is_explicit_and_preserves_structured_identity() {
        let device = port("COM3", "adapter");
        let mut logs = DeviceLogBook::default();
        logs.begin_batch(std::slice::from_ref(&device), "batch", "esp32c3", "hash");
        logs.remember_identity(
            "COM3",
            &DeviceIdentity {
                efuse_id: Some("102030405060".into()),
                mac: None,
            },
        );
        logs.append_line("COM3", "字".repeat(200_000));
        let text = logs.current_lines["COM3"].join("\n");
        assert!(text.contains("日志已截断") && text.len() <= 512 * 1024);
        assert_eq!(
            logs.identities[&DeviceLogKey::from_port(&device, "batch")]
                .efuse_id
                .as_deref(),
            Some("102030405060")
        );
    }

    #[test]
    fn header_drag_region_excludes_title_and_action_buttons() {
        let header = eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(900.0, 40.0),
        );
        let title = eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(200.0, 40.0),
        );
        let blank = header_drag_region(header, title, 700.0).unwrap();
        assert!(blank.left() > title.right() && blank.right() < 700.0);
        assert!(header_drag_region(header, title, 210.0).is_none());
    }

    #[test]
    fn idle_device_schedule_never_requests_periodic_enumeration() {
        let schedule = ScanSchedule::default();
        let now = Instant::now();
        for seconds in [0, 2, 60, 86_400] {
            assert!(!schedule.is_due(now + Duration::from_secs(seconds)));
            assert!(
                schedule
                    .repaint_delay(now + Duration::from_secs(seconds))
                    .is_none()
            );
        }
    }

    #[test]
    fn real_device_event_has_only_a_bounded_settling_burst() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        schedule.device_event(now);
        assert!(schedule.is_due(now));
        for millis in [0, 100, 300, 750, 1_500] {
            let time = now + Duration::from_millis(millis);
            assert!(schedule.is_due(time));
            schedule.scan_started(time);
            assert!(!schedule.is_due(time));
        }
        assert!(
            schedule
                .repaint_delay(now + Duration::from_secs(60))
                .is_none()
        );
        assert!(!schedule.is_due(now + Duration::from_secs(86_400)));
    }

    #[test]
    fn event_during_active_scan_remains_dirty_until_follow_up_can_start() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        schedule.request_manual();
        schedule.scan_started(now);
        schedule.device_event(now + Duration::from_millis(10));
        let active_finishes = now + Duration::from_millis(600);
        assert!(schedule.is_due(active_finishes));
        // A busy scanner does not call scan_started, so another UI frame cannot lose the event.
        assert!(schedule.is_due(active_finishes + Duration::from_millis(5)));
        schedule.scan_started(active_finishes);
        assert!(!schedule.is_due(active_finishes));
        assert!(schedule.is_due(now + Duration::from_millis(760)));
    }

    #[test]
    fn manual_refresh_does_not_drop_later_device_settling() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        schedule.device_event(now);
        schedule.request_manual();
        assert!(schedule.is_due(now));
        schedule.scan_started(now);
        assert!(schedule.is_due(now + Duration::from_millis(200)));
    }

    #[test]
    fn disabling_event_recognition_preserves_explicit_manual_refresh() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        schedule.device_event(now);
        schedule.request_manual();
        schedule.disable_auto();
        assert!(schedule.is_due(now));
        schedule.scan_started(now);
        assert!(
            schedule
                .repaint_delay(now + Duration::from_secs(60))
                .is_none()
        );
    }

    #[test]
    fn large_device_burst_keeps_earliest_scan_and_only_one_final_settling_pass() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        for index in 0..=150 {
            schedule.device_event(now + Duration::from_millis(index));
        }
        assert_eq!(schedule.auto_deadline(), Some(now));
        let first_scan = now + Duration::from_millis(150);
        schedule.scan_started(first_scan);
        for millis in [300, 750, 1_650] {
            let next = now + Duration::from_millis(millis);
            assert_eq!(schedule.auto_deadline(), Some(next));
            schedule.scan_started(next);
        }
        assert!(schedule.auto_deadline().is_none());
    }

    #[test]
    fn auto_event_bursts_are_coalesced_for_fifty_ms_but_manual_refresh_is_immediate() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        schedule.device_event(now);
        schedule.scan_started(now);
        schedule.device_event(now + Duration::from_millis(1));
        assert!(!schedule.is_due(now + Duration::from_millis(49)));
        assert!(schedule.is_due(now + Duration::from_millis(50)));
        schedule.request_manual();
        assert!(schedule.is_due(now + Duration::from_millis(2)));
    }

    fn port(name: &str, description: &str) -> Port {
        Port {
            name: name.into(),
            description: description.into(),
            identity: format!("{name}|{description}"),
            usb: true,
            suggested: true,
        }
    }

    fn selected(name: &str, description: &str) -> PortRow {
        PortRow {
            port: port(name, description),
            selected: true,
            online: true,
        }
    }

    #[test]
    fn failed_scan_preserves_known_devices_and_selection() {
        let previous = vec![selected("COM3", "CP210x A")];
        let (rows, error) = reconcile_scan_result(previous, Err("enumeration failed".into()));
        assert_eq!(error.as_deref(), Some("enumeration failed"));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].online && rows[0].selected);
    }

    #[test]
    fn selected_device_is_removed_from_live_list_as_soon_as_scan_confirms_unplug() {
        let previous = vec![selected("COM3", "CP210x A")];
        let (rows, error) = reconcile_scan_result(previous, Ok(vec![]));
        assert!(rows.is_empty());
        assert!(error.is_none());
    }

    fn job(state: JobState, progress: f32) -> JobRow {
        JobRow {
            state,
            progress,
            message: "original task status".into(),
            started: "started".into(),
            finished: String::new(),
            elapsed: None,
            duration_secs: Some(1.25),
            started_at: "2026-11-19T00:00:00Z".into(),
            finished_at: "2026-11-19T00:00:01.250Z".into(),
            chip_identity: None,
            unique_device_id: None,
        }
    }

    #[test]
    fn completed_batch_unplug_disappears_from_live_com_list_but_retains_read_only_record() {
        let snapshots = vec![port("COM3", "CP210x A")];
        let jobs = BTreeMap::from([("COM3".into(), job(JobState::Success, 1.0))]);
        let rows = reconcile_ports(vec![selected("COM3", "CP210x A")], vec![]);
        assert!(rows.is_empty());
        let records = missing_batch_jobs(&rows, &snapshots, &jobs);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].port.name, "COM3");
        let record = records[0].job.unwrap();
        assert_eq!(record.state, JobState::Success);
        assert_eq!(record.progress, 1.0);
        assert_eq!(record.duration_secs, Some(1.25));
    }

    #[test]
    fn missing_active_task_keeps_its_progress_independently_of_live_device_rows() {
        let snapshots = vec![port("COM7", "ESP native USB")];
        let jobs = BTreeMap::from([("COM7".into(), job(JobState::Writing, 0.42))]);
        let rows = reconcile_ports(vec![selected("COM7", "ESP native USB")], vec![]);
        assert!(rows.is_empty());
        let records = missing_batch_jobs(&rows, &snapshots, &jobs);
        let record = records[0].job.unwrap();
        assert_eq!(record.state, JobState::Writing);
        assert_eq!(record.progress, 0.42);
        assert_eq!(record.message, "original task status");
        assert_eq!(jobs.len(), 1);
    }

    #[test]
    fn reused_com_contains_only_new_device_and_does_not_inherit_selection_or_old_task() {
        let snapshots = vec![port("COM3", "CP210x A")];
        let jobs = BTreeMap::from([("COM3".into(), job(JobState::Failed, 0.42))]);
        let rows = reconcile_ports(
            vec![selected("COM3", "CP210x A")],
            vec![port("COM3", "CP210x B")],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port.description, "CP210x B");
        assert!(rows[0].online && !rows[0].selected);
        let records = missing_batch_jobs(&rows, &snapshots, &jobs);
        assert_eq!(records.len(), 1);
        assert_ne!(records[0].port.identity, rows[0].port.identity);
        assert_eq!(records[0].job.unwrap().state, JobState::Failed);
        assert_eq!(records[0].job.unwrap().progress, 0.42);
    }

    #[test]
    fn disconnect_then_same_identity_reconnect_requires_new_selection() {
        let disconnected = reconcile_ports(vec![selected("COM3", "CP210x A")], vec![]);
        assert!(disconnected.is_empty());
        let reconnected = reconcile_ports(disconnected, vec![port("COM3", "CP210x A")]);
        assert_eq!(reconnected.len(), 1);
        assert!(reconnected[0].online && !reconnected[0].selected);
    }

    #[test]
    fn repeated_scan_deduplicates_observations_without_losing_continuous_selection() {
        let observed = vec![port("COM3", "CP210x A"), port("COM3", "CP210x A")];
        let rows = reconcile_ports(vec![selected("COM3", "CP210x A")], observed);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].selected && rows[0].online);
    }

    #[test]
    fn csv_preserves_quotes_newlines_and_prevents_formula_execution() {
        let mut text = String::new();
        append_csv(&mut text, ["COM3", "a,\"b\"\nnext", "=HYPERLINK(\"x\")"]);
        assert_eq!(
            text,
            "\"COM3\",\"a,\"\"b\"\"\nnext\",\"'=HYPERLINK(\"\"x\"\")\"\r\n"
        );
    }
}
