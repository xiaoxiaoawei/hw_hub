use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use eframe::egui::{self, Color32, RichText};
use esp32c3_flashtool::{
    backend::{self, DeviceIdentity, Port},
    batch::{Batch, BatchEvent, BatchEventKind, JobState},
    chip::ChipModel,
    device_scan::DeviceScanner,
    device_watch::DeviceWatcher,
    drivers::{self, DriverKind},
    firmware::{FirmwareInput, prepare_firmware},
    self_update::{self, PreparedUpdate, SelfUpdateEvent, SelfUpdateService},
    telemetry::{self, RecordDraft, RecordService, RecordStatus, TelemetrySettings, UploadState},
    updater::{self, CheckResult, ReleaseUpdate, UpdateSettings},
};

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
    fn begin_batch(&mut self, ports: &[Port], batch: &str, chip: &str, firmware: &str) {
        self.current_keys.clear();
        self.current_lines.clear();
        self.current_bytes.clear();
        self.truncated.clear();
        for port in ports {
            let key = DeviceLogKey::from_port(port, batch);
            self.current_keys.insert(port.name.clone(), key.clone());
            let line = format!(
                "{}  === 批次 {batch} · {chip} · SHA256 {firmware} ===",
                timestamp()
            );
            self.history.entry(key).or_default().push_back(line.clone());
            self.current_bytes.insert(port.name.clone(), line.len());
            self.current_lines.insert(port.name.clone(), vec![line]);
        }
    }

    fn append(&mut self, port: &str, state: &JobState, kind: BatchEventKind, message: &str) {
        if kind == BatchEventKind::Progress {
            return;
        }
        let line = format!("{}  [{}] {message}", timestamp(), state_label(state));
        self.append_line(port, line);
    }

    fn append_line(&mut self, port: &str, line: String) {
        let Some(key) = self.current_keys.get(port) else {
            return;
        };
        let history = self.history.entry(key.clone()).or_default();
        history.push_back(line.clone());
        while history.len() > 10_000 {
            history.pop_front();
        }
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
}

#[derive(Default)]
struct ScanSchedule {
    manual_pending: bool,
    event_deadlines: VecDeque<Instant>,
    last_scan: Option<Instant>,
}

impl ScanSchedule {
    fn request_manual(&mut self) {
        self.manual_pending = true;
    }

    fn device_event(&mut self, now: Instant) {
        // Discover immediately, then use finite event-only settling scans.
        // Unioning pending deadlines prevents a USB/COM burst from postponing
        // an earlier request while the current enumeration is still active.
        let mut pending: BTreeSet<_> = self.event_deadlines.iter().copied().collect();
        pending.extend(
            [0, 100, 300, 750, 1_500]
                .into_iter()
                .map(|millis| now + Duration::from_millis(millis)),
        );
        self.event_deadlines = pending.into_iter().collect();
    }

    fn disable_auto(&mut self) {
        self.event_deadlines.clear();
    }

    fn is_due(&self, now: Instant) -> bool {
        self.manual_pending || self.auto_deadline().is_some_and(|deadline| deadline <= now)
    }

    fn scan_started(&mut self, now: Instant) {
        self.manual_pending = false;
        self.last_scan = Some(now);
        while self
            .event_deadlines
            .front()
            .is_some_and(|deadline| *deadline <= now)
        {
            self.event_deadlines.pop_front();
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
        self.event_deadlines.front().map(|deadline| {
            self.last_scan.map_or(*deadline, |last| {
                (*deadline).max(last + Duration::from_millis(50))
            })
        })
    }
}

pub struct FlashApp {
    selected_chip: ChipModel,
    pending_chip: ChipModel,
    batch_chip: ChipModel,
    chip_picker_open: bool,
    driver_dialog_open: bool,
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
    concurrency: usize,
    batch: Option<Batch>,
    expected_jobs: usize,
    jobs: BTreeMap<String, JobRow>,
    logs: VecDeque<String>,
    device_logs: DeviceLogBook,
    log_windows: BTreeSet<DeviceLogKey>,
    batch_summary_logged: bool,
    batch_id: String,
    batch_started_at: String,
    settings_open: bool,
    telemetry_settings: TelemetrySettings,
    telemetry_draft: TelemetrySettings,
    batch_telemetry_settings: TelemetrySettings,
    telemetry_settings_error: Option<String>,
    settings_headers: Vec<(String, String)>,
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
    allow_exit: bool,
    diagnostics_running: bool,
    diagnostics_result: Option<mpsc::Receiver<Result<String, String>>>,
    diagnostics_text: String,
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
    update_was_manual: bool,
    update_status: String,
    update_error: Option<String>,
    update_available: Option<ReleaseUpdate>,
    update_latest_version: Option<String>,
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
    installer_ready: bool,
    install_allow_upload_interruption: bool,
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
            style.spacing.interact_size.y = 32.0;
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
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
            selected_chip: ChipModel::default(),
            pending_chip: ChipModel::default(),
            batch_chip: ChipModel::default(),
            chip_picker_open: false,
            driver_dialog_open: false,
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
            concurrency: 4,
            batch: None,
            expected_jobs: 0,
            jobs: BTreeMap::new(),
            logs: VecDeque::new(),
            device_logs: DeviceLogBook::default(),
            log_windows: BTreeSet::new(),
            batch_summary_logged: true,
            batch_id: String::new(),
            batch_started_at: String::new(),
            settings_open: false,
            telemetry_draft: telemetry_settings.clone(),
            batch_telemetry_settings: telemetry_settings.clone(),
            settings_headers: telemetry_settings
                .custom_headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
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
            allow_exit: false,
            diagnostics_running: false,
            diagnostics_result: None,
            diagnostics_text: String::new(),
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
            update_was_manual: false,
            update_status: "尚未检查更新".into(),
            update_error: None,
            update_available: None,
            update_latest_version: None,
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
            installer_ready: false,
            install_allow_upload_interruption: false,
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
        self.running() || self.driver_busy || self.installer_preparing
    }

    fn open_update_dialog(&mut self) {
        self.resume_settings_after_update |= self.settings_open;
        self.settings_open = false;
        self.chip_picker_open = false;
        self.driver_dialog_open = false;
        self.pending_driver_license = None;
        self.update_dialog_open = true;
        self.update_prompt_pending = false;
        self.update_focus_later = true;
    }

    fn start_update_check(&mut self, manual: bool, ctx: &egui::Context) {
        if self.update_checking || self.update_service.is_busy() || self.installer_preparing {
            return;
        }
        if !manual {
            if !updater::should_auto_check(&self.update_settings, now_unix()) {
                return;
            }
            let previous_attempt = self.update_settings.last_auto_attempt_unix;
            self.update_settings.last_auto_attempt_unix = Some(now_unix());
            if let Err(error) = updater::save_settings(&self.update_settings) {
                self.update_settings.last_auto_attempt_unix = previous_attempt;
                self.update_settings_error = Some(format!("无法保存自动检查记录：{error:#}"));
                self.update_status = "自动检查未执行：更新设置无法保存".into();
                return;
            }
        }
        self.update_checking = true;
        self.update_was_manual = manual;
        self.update_status = "正在检查正式发布版本…".into();
        self.update_error = None;
        let (sender, receiver) = mpsc::channel();
        self.update_result = Some(receiver);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = updater::check_latest().map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    fn collect_update_events(&mut self, ctx: &egui::Context) {
        let result = match self.update_result.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Err("更新检查任务意外结束，请手动重试。".into())
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => return,
        };
        self.update_checking = false;
        self.update_result = None;
        match result {
            Ok(result) => {
                self.update_error = None;
                self.update_settings.last_checked_unix = Some(now_unix());
                if let Err(error) = updater::save_settings(&self.update_settings) {
                    self.update_settings_error =
                        Some(format!("检查已完成，但无法保存检查时间：{error:#}"));
                } else {
                    self.update_settings_error = None;
                }
                match result {
                    CheckResult::UpToDate { latest_version } => {
                        self.update_latest_version = Some(latest_version);
                        self.update_available = None;
                        self.update_status = "当前已是最新版本".into();
                    }
                    CheckResult::Available(release) => {
                        if self
                            .prepared_update
                            .as_ref()
                            .is_some_and(|ready| ready.version != release.version)
                        {
                            self.prepared_update = None;
                        }
                        self.update_latest_version = Some(release.version.clone());
                        self.update_status = format!("发现新版本 {}", release.version);
                        self.update_available = Some(release);
                        self.update_prompt_pending = true;
                    }
                    CheckResult::NoPublishedRelease => {
                        self.update_latest_version = None;
                        self.update_available = None;
                        self.update_status = "暂未发布正式版本".into();
                    }
                }
            }
            Err(error) => {
                self.update_status = if self.update_was_manual {
                    "检查更新失败，请稍后重试".into()
                } else {
                    "自动检查暂不可用".into()
                };
                self.log(format!("更新检查失败：{error}"));
                self.update_error = Some(error);
            }
        }
        // The result can change modal geometry after its cached sizing pass.
        // Request one settling frame only when a result was consumed.
        ctx.request_repaint();
    }

    fn start_download_update(&mut self) {
        let Some(release) = self.update_available.clone() else {
            return;
        };
        if self.update_service.is_busy() || self.installer_preparing {
            return;
        }
        self.self_update_error = None;
        self.update_downloaded = 0;
        self.update_total = None;
        match self.update_service.start_download(release) {
            Ok(()) => self.self_update_phase = "正在下载更新".into(),
            Err(error) => self.self_update_error = Some(format!("无法开始下载更新：{error:#}")),
        }
    }

    fn poll_self_update(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.update_service.poll() {
            match event {
                SelfUpdateEvent::Progress {
                    downloaded,
                    total,
                    phase,
                } => {
                    self.update_downloaded = downloaded;
                    self.update_total = total;
                    self.self_update_phase = phase;
                }
                SelfUpdateEvent::Ready(prepared) => {
                    self.prepared_update = Some(prepared);
                    self.self_update_phase = "更新已下载并校验".into();
                    self.self_update_error = None;
                    self.update_prompt_pending = true;
                }
                SelfUpdateEvent::Failed(error) => {
                    self.self_update_phase = "更新未完成".into();
                    self.log(format!("更新未完成：{error}"));
                    self.self_update_error = Some(error);
                }
            }
            ctx.request_repaint();
        }
        let result = match self.installer_result.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Some(Err("更新准备任务意外结束。".into()))
            }
            _ => None,
        };
        if let Some(result) = result {
            self.installer_result = None;
            match result {
                Ok(()) => {
                    self.installer_ready = true;
                    self.self_update_phase = "更新程序已就绪".into();
                }
                Err(error) => {
                    self.installer_preparing = false;
                    self.installer_ready = false;
                    self.prepared_update = None;
                    self.self_update_phase = "无法准备更新".into();
                    self.log(format!("无法准备更新：{error}"));
                    self.self_update_error = Some(error);
                }
            }
            ctx.request_repaint();
        }
        if self.installer_ready && self.restart_block_reason().is_none() {
            if self.record_service.pending_count() > 0 && !self.install_allow_upload_interruption {
                self.self_update_phase = "本地已保存，请确认未完成上传后重启".into();
                if !self.update_dialog_open {
                    self.update_prompt_pending = true;
                }
            } else {
                self.allow_exit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn restart_block_reason(&self) -> Option<&'static str> {
        restart_safety_reason(
            self.running(),
            self.driver_busy,
            self.record_service.pending_local_count(),
            self.pending_records.len(),
            self.failed_records.len(),
        )
    }

    fn start_update_installer(&mut self, ctx: &egui::Context) {
        if let Some(reason) = self.restart_block_reason() {
            self.self_update_phase = reason.into();
            return;
        }
        if !cfg!(windows) {
            self.self_update_error =
                Some("当前平台不支持安装 Windows 更新，请在 Windows 电脑使用。".into());
            return;
        }
        self.install_allow_upload_interruption = self.record_service.pending_count() > 0;
        if self.installer_ready {
            self.poll_self_update(ctx);
            return;
        }
        if self.installer_preparing {
            return;
        }
        let Some(prepared) = self.prepared_update.clone() else {
            return;
        };
        self.installer_preparing = true;
        self.self_update_error = None;
        self.self_update_phase = "正在准备重启并检查写入权限".into();
        self.settings_open = false;
        self.resume_settings_after_update = false;
        self.driver_dialog_open = false;
        self.pending_driver_license = None;
        let (sender, receiver) = mpsc::channel();
        self.installer_result = Some(receiver);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result =
                self_update::launch_installer(&prepared).map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.update_dialog_open {
            return;
        }
        let mut later = false;
        let mut manual_check = false;
        let mut download = false;
        let mut restart = false;
        let response = egui::Modal::new(egui::Id::new("application_updates"))
            .frame(egui::Frame::new().fill(SURFACE).corner_radius(6.0).inner_margin(20.0))
            .show(ctx, |ui| {
                ui.set_width(600.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FlashTool 更新").size(20.0).strong());
                    ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).size(12.0).color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { if text_button(ui, "关闭").clicked() { later = true; } });
                });
                ui.add_space(8.0);
                if let Some(message) = &self.self_update_error { update_error_panel(ui, ctx, &self.self_update_phase, message); }
                else if let Some(message) = &self.update_error { update_error_panel(ui, ctx, &self.update_status, message); }
                let max_height = (ctx.screen_rect().height() - 240.0 - if self.self_update_error.is_some() || self.update_error.is_some() { 190.0 } else { 0.0 }).max(170.0);
                egui::ScrollArea::vertical().id_salt("update_dialog_content").min_scrolled_height(170.0).max_height(max_height).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let status = if self.installer_preparing || self.update_service.is_busy() || self.prepared_update.is_some() { &self.self_update_phase } else { &self.update_status };
                        if self.update_checking || self.update_service.is_busy() || self.installer_preparing { ui.spinner(); }
                        ui.add(egui::Label::new(RichText::new(status).size(14.0).strong()).wrap());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled_ui(!self.update_checking && !self.update_service.is_busy() && !self.installer_preparing, |ui| secondary_button(ui, "手动检查", [91.0, 31.0])).inner.clicked() { manual_check = true; }
                        });
                    });
                    ui.label(RichText::new(format!("上次成功检查：{}", self.update_settings.last_checked_unix.map(format_update_time).unwrap_or_else(|| "尚未检查".into()))).size(12.0).color(MUTED));
                    let was_auto = self.update_settings.auto_check;
                    if ui.checkbox(&mut self.update_settings.auto_check, "启动时自动检查更新").changed() {
                        if let Err(error) = updater::save_settings(&self.update_settings) {
                            self.update_settings.auto_check = was_auto;
                            self.update_settings_error = Some(format!("无法保存更新设置：{error:#}"));
                        } else { self.update_settings_error = None; }
                    }
                    if let Some(error) = &self.update_settings_error { banner(ui, error, true); }
                    ui.separator();
                    if let Some(prepared) = &self.prepared_update {
                        ui.label(RichText::new(format!("v{} 已下载并校验，是否重启安装？", prepared.version)).size(15.0).strong());
                        ui.label(RichText::new("重启前请先保存软件设置。").size(12.0).color(MUTED));
                        if let Some(reason) = self.restart_block_reason() { ui.label(RichText::new(reason).size(12.0).color(MUTED)); }
                        let uploads = self.record_service.pending_count().saturating_sub(self.record_service.pending_local_count());
                        if uploads > 0 { ui.label(RichText::new(format!("本地已保存，仍有 {uploads} 条上传未完成；重启会中断这些上传。")).size(12.0).color(DANGER)); }
                        ui.horizontal(|ui| {
                            let postpone = secondary_button(ui, if self.installer_preparing { "隐藏" } else { "稍后" }, [88.0, 36.0]);
                            if self.update_focus_later { postpone.request_focus(); self.update_focus_later = false; }
                            if postpone.clicked() { later = true; }
                            if ui.add_enabled_ui(self.restart_block_reason().is_none() && (!self.installer_preparing || self.installer_ready), |ui| primary_button(ui, "重启更新", [120.0, 36.0])).inner.clicked() { restart = true; }
                        });
                    } else if self.update_service.is_busy() {
                        let progress = self.update_total.filter(|total| *total > 0).map(|total| (self.update_downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32);
                        let text = match self.update_total { Some(total) => format!("{:.1} / {:.1} MiB", self.update_downloaded as f64 / 1_048_576.0, total as f64 / 1_048_576.0), None => format!("已下载 {:.1} MiB", self.update_downloaded as f64 / 1_048_576.0) };
                        ui.add(egui::ProgressBar::new(progress.unwrap_or(0.0)).desired_width(ui.available_width()).fill(SUCCESS).text(text).animate(progress.is_none()));
                    } else if let Some(release) = &self.update_available {
                        ui.label(RichText::new(format!("发现新版本 v{}", release.version)).size(15.0).strong());
                        if !release.notes.trim().is_empty() { ui.add(egui::Label::new(RichText::new(release.notes.chars().take(1_500).collect::<String>()).size(12.0).color(MUTED)).wrap()); }
                        if release.download_url.is_some() {
                            ui.label(RichText::new("是否下载更新？下载期间可继续使用。").size(12.0).color(MUTED));
                            ui.horizontal(|ui| {
                                let postpone = secondary_button(ui, "稍后", [88.0, 36.0]);
                                if self.update_focus_later { postpone.request_focus(); self.update_focus_later = false; }
                                if postpone.clicked() { later = true; }
                                if primary_button(ui, "下载更新", [120.0, 36.0]).clicked() { download = true; }
                            });
                        } else { ui.label(RichText::new("发布包尚未上传。").size(12.0).color(MUTED)); }
                        if text_button(ui, "查看官方发布页").clicked() { ctx.open_url(egui::OpenUrl::new_tab(&release.release_url)); }
                    }
                });
            });
        if later || response.should_close() {
            self.update_dialog_open = false;
            self.update_prompt_pending = false;
            if self.resume_settings_after_update {
                self.settings_open = true;
                self.resume_settings_after_update = false;
            }
        }
        if manual_check {
            self.start_update_check(true, ctx);
        }
        if download {
            self.start_download_update();
        }
        if restart {
            self.start_update_installer(ctx);
        }
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
        if self
            .watcher
            .as_ref()
            .is_some_and(DeviceWatcher::take_pending)
            && self.auto_refresh
        {
            self.scan_schedule.device_event(Instant::now());
        }
        if let Some(result) = self.scanner.poll() {
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
                self.last_scan = Local::now().format("%H:%M:%S").to_string();
            }
        }
        self.start_scheduled_scan();
    }

    fn start_batch(&mut self) {
        self.error = None;
        self.notice = None;
        let ports: Vec<_> = self
            .ports
            .iter()
            .filter(|r| r.selected && r.online)
            .map(|r| r.port.clone())
            .collect();
        if ports.is_empty() {
            self.error = Some("请先勾选在线的待烧录串口。".into());
            return;
        }
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
        let expected_jobs = ports.len();
        let batch_port_snapshots = ports.clone();
        let batch_port_identities = self
            .ports
            .iter()
            .filter(|row| row.selected && row.online)
            .map(|row| (row.port.name.clone(), row.port.identity.clone()))
            .collect();
        match Batch::start_guarded(ports, self.baud, self.concurrency, Arc::new(firmware)) {
            Ok(batch) => {
                self.jobs.clear();
                self.expected_jobs = expected_jobs;
                self.logs.clear();
                self.firmware_hash = firmware_hash;
                self.batch_firmware = batch_firmware;
                self.batch_chip = self.selected_chip;
                self.batch_port_identities = batch_port_identities;
                self.batch_port_snapshots = batch_port_snapshots;
                self.batch_started = timestamp();
                self.batch_started_at = chrono::Utc::now().to_rfc3339();
                self.batch_id = telemetry::new_batch_id();
                self.batch_telemetry_settings = self.telemetry_settings.clone();
                self.batch_summary_logged = false;
                self.device_logs.begin_batch(
                    &self.batch_port_snapshots,
                    &self.batch_id,
                    self.batch_chip.label(),
                    &self.firmware_hash,
                );
                for port in &self.batch_port_snapshots {
                    let idle = DeviceLogKey::from_port(port, "");
                    if self.log_windows.remove(&idle) {
                        self.log_windows
                            .insert(DeviceLogKey::from_port(port, &self.batch_id));
                    }
                }
                self.batch = Some(batch);
            }
            Err(err) => self.error = Some(format!("无法启动批次：{err:#}")),
        }
    }

    fn log(&mut self, message: String) {
        self.logs
            .push_back(format!("{}  {message}", Local::now().format("%H:%M:%S")));
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
            });
            let first_terminal =
                kind == BatchEventKind::State && is_terminal(&state) && row.finished_at.is_empty();
            if state == JobState::Connecting && row.started.is_empty() {
                row.started = now.clone();
                row.started_at = chrono::Utc::now().to_rfc3339();
                row.elapsed = Some(Instant::now());
            }
            if first_terminal {
                row.finished = now;
                row.finished_at = chrono::Utc::now().to_rfc3339();
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
            self.device_logs.append(&port, &state, kind, &message);
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
                if let Some(summary) = terminal_summary(&port, &state) {
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
            chip: self.batch_chip.slug().into(),
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
        });
        self.retry_record_submissions();
    }

    fn retry_record_submissions(&mut self) {
        while let Some(pending) = self.pending_records.front() {
            match self
                .record_service
                .submit(&pending.settings, pending.draft.clone())
            {
                Ok(id) => {
                    let pending = self
                        .pending_records
                        .pop_front()
                        .expect("front record exists");
                    self.submitted_records.insert(id, pending);
                    self.record_submit_error = None;
                }
                Err(error) => {
                    let error = format!("烧录记录等待保存，尚未丢弃：{error:#}");
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
            let retry_upload = matches!(
                &result.upload_state,
                UploadState::Failed | UploadState::Deferred
            );
            let upload = match result.upload_state {
                UploadState::Disabled => "上报关闭",
                UploadState::SkippedIdentity => "未上报：设备身份未确认",
                UploadState::Uploaded => "上报完成",
                UploadState::Failed => "上报失败",
                UploadState::Deferred => "未上报：上传队列暂不可用",
            };
            let note = format!("{storage} · {upload}");
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
        let windows: Vec<_> = self.log_windows.iter().cloned().collect();
        for key in windows {
            let mut open = true;
            let mut close = false;
            egui::Window::new(format!("{} log", key.port))
                .id(egui::Id::new((
                    "device_log",
                    &key.batch,
                    &key.port,
                    &key.identity,
                )))
                .open(&mut open)
                .title_bar(false)
                .resizable(true)
                .frame(
                    egui::Frame::new()
                        .fill(NAVY)
                        .stroke(egui::Stroke::new(1.0_f32, BORDER))
                        .corner_radius(4.0)
                        .inner_margin(12.0),
                )
                .default_size([700.0, 430.0])
                .min_size([420.0, 230.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{} log", key.port))
                                .size(16.0)
                                .strong(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if secondary_button(ui, "关闭", [64.0, 29.0]).clicked() {
                                close = true;
                            }
                            if secondary_button(ui, "复制", [64.0, 29.0]).clicked() {
                                let text = self
                                    .device_logs
                                    .history
                                    .get(&key)
                                    .map(|lines| {
                                        lines.iter().cloned().collect::<Vec<_>>().join("\n")
                                    })
                                    .unwrap_or_default();
                                ctx.copy_text(text);
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::both()
                        .id_salt(("detailed_log", &key))
                        .min_scrolled_height(160.0)
                        .stick_to_bottom(true)
                        .max_height(ui.available_height().max(120.0))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            if let Some(lines) = self.device_logs.history.get(&key) {
                                for line in lines {
                                    ui.label(RichText::new(line).monospace().size(12.0).color(INK));
                                }
                            } else {
                                ui.label(RichText::new("暂无日志").size(12.0).color(MUTED));
                            }
                        });
                });
            if close || !open {
                self.log_windows.remove(&key);
            }
        }
    }

    fn save_settings_from_ui(&mut self) -> bool {
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
            pending.settings = self.telemetry_settings.clone();
            self.pending_records.push_back(pending);
        }
        self.record_submit_error = None;
        self.retry_record_submissions();
    }

    fn retry_failed_uploads(&mut self) {
        while let Some(mut pending) = self.failed_uploads.pop_front() {
            let original_directory = pending.settings.csv_directory.clone();
            pending.settings = self.telemetry_settings.clone();
            pending.settings.csv_directory = original_directory;
            self.pending_records.push_back(pending);
        }
        self.record_submit_error = None;
        self.retry_record_submissions();
    }

    fn start_diagnostics(&mut self, ctx: &egui::Context) {
        if self.diagnostics_running {
            return;
        }
        self.diagnostics_running = true;
        let (sender, receiver) = mpsc::channel();
        self.diagnostics_result = Some(receiver);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result =
                backend::port_diagnostics().map_err(|error| format!("串口诊断失败：{error:#}"));
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    fn poll_diagnostics(&mut self) {
        let result = match self
            .diagnostics_result
            .as_ref()
            .map(mpsc::Receiver::try_recv)
        {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => Err("串口诊断任务意外结束。".into()),
            _ => return,
        };
        self.diagnostics_running = false;
        self.diagnostics_result = None;
        self.diagnostics_text = result.unwrap_or_else(|error| error);
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut check_updates = false;
        egui::Window::new("软件设置")
            .id(egui::Id::new("software_settings"))
            .open(&mut open)
            .default_size([720.0, 640.0])
            .min_size([570.0, 420.0])
            .max_height(ctx.screen_rect().height() - 50.0)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings_content")
                    .min_scrolled_height(300.0)
                    .max_height((ui.available_height() - 65.0).max(250.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("FlashTool {}", env!("CARGO_PKG_VERSION")))
                                    .size(22.0)
                                    .strong(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if secondary_button(ui, "检查软件更新", [119.0, 33.0]).clicked()
                                    {
                                        check_updates = true;
                                    }
                                },
                            );
                        });
                        ui.checkbox(&mut self.settings_auto_update, "启动时自动检查软件更新");
                        ui.add_space(8.0);
                        ui.separator();
                        ui.label(RichText::new("本地记录").size(16.0).strong());
                        let directory = self
                            .telemetry_draft
                            .csv_directory
                            .clone()
                            .map(Ok)
                            .unwrap_or_else(telemetry::default_records_directory);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("CSV 目录").size(12.0).color(MUTED));
                            if secondary_button(ui, "选择目录", [89.0, 31.0]).clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .set_title("选择烧录记录 CSV 目录")
                                    .pick_folder()
                            {
                                self.telemetry_draft.csv_directory = Some(path);
                                self.settings_saved = false;
                            }
                            if text_button(ui, "默认目录").clicked() {
                                self.telemetry_draft.csv_directory = None;
                                self.settings_saved = false;
                            }
                            if let Ok(path) = &directory
                                && ui
                                    .add_enabled_ui(path.is_dir(), |ui| text_button(ui, "打开目录"))
                                    .inner
                                    .clicked()
                                && let Err(error) = open_records_directory(path)
                            {
                                self.telemetry_settings_error =
                                    Some(format!("无法打开记录目录：{error}"));
                            }
                        });
                        match directory {
                            Ok(path) => {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(path.display().to_string())
                                            .monospace()
                                            .size(12.0)
                                            .color(MUTED),
                                    )
                                    .wrap(),
                                );
                            }
                            Err(error) => {
                                ui.label(
                                    RichText::new(format!("默认目录不可用：{error}"))
                                        .size(12.0)
                                        .color(DANGER),
                                );
                            }
                        }
                        ui.label(
                            RichText::new("每台设备任务结束后自动保存 CSV，上报关闭时仍会保存。")
                                .size(12.0)
                                .color(MUTED),
                        );
                        if !self.failed_records.is_empty() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} 条本地记录保存失败",
                                        self.failed_records.len()
                                    ))
                                    .size(12.0)
                                    .color(DANGER),
                                );
                                if secondary_button(ui, "保存设置并重试", [140.0, 32.0]).clicked()
                                    && self.save_settings_from_ui()
                                {
                                    self.retry_failed_local_records();
                                }
                            });
                        }
                        ui.add_space(9.0);
                        ui.separator();
                        ui.label(RichText::new("日志上报").size(16.0).strong());
                        ui.checkbox(&mut self.telemetry_draft.upload_enabled, "启用烧录日志上报");
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("接口地址").size(12.0).color(MUTED));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.telemetry_draft.endpoint)
                                    .desired_width(ui.available_width())
                                    .hint_text("http:// 或 https://"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("请求超时").size(12.0).color(MUTED));
                            ui.add(
                                egui::DragValue::new(&mut self.telemetry_draft.timeout_seconds)
                                    .range(5..=30)
                                    .suffix(" 秒"),
                            );
                        });
                        if !self.failed_uploads.is_empty() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} 条记录已保存，上传未完成",
                                        self.failed_uploads.len()
                                    ))
                                    .size(12.0)
                                    .color(MUTED),
                                );
                                if ui
                                    .add_enabled_ui(self.telemetry_draft.upload_enabled, |ui| {
                                        secondary_button(ui, "按当前设置重试上报", [172.0, 32.0])
                                    })
                                    .inner
                                    .clicked()
                                    && self.save_settings_from_ui()
                                {
                                    self.retry_failed_uploads();
                                }
                            });
                        }
                        egui::CollapsingHeader::new("高级设置 · 身份提取与请求头")
                            .id_salt("advanced_record_settings")
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("eFuse 出厂 MAC 提取规则")
                                        .size(12.0)
                                        .color(MUTED),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut self.telemetry_draft.efuse_pattern,
                                    )
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(ui.available_width()),
                                );
                                ui.label(RichText::new("MAC 提取规则").size(12.0).color(MUTED));
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut self.telemetry_draft.mac_pattern,
                                    )
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(ui.available_width()),
                                );
                                ui.add_space(5.0);
                                ui.label(RichText::new("自定义请求头").size(12.0).color(MUTED));
                                let mut remove = None;
                                for (index, (name, value)) in
                                    self.settings_headers.iter_mut().enumerate()
                                {
                                    ui.push_id(index, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.add(
                                                egui::TextEdit::singleline(name)
                                                    .desired_width(160.0)
                                                    .hint_text("Header-Name"),
                                            );
                                            ui.add(
                                                egui::TextEdit::singleline(value)
                                                    .password(true)
                                                    .desired_width(
                                                        (ui.available_width() - 63.0).max(120.0),
                                                    )
                                                    .hint_text("Header value"),
                                            );
                                            if text_button(ui, "移除").clicked() {
                                                remove = Some(index);
                                            }
                                        });
                                    });
                                }
                                if let Some(index) = remove {
                                    self.settings_headers.remove(index);
                                }
                                if text_button(ui, "+ 添加请求头").clicked() {
                                    self.settings_headers.push((String::new(), String::new()));
                                }
                            });
                        egui::CollapsingHeader::new("设备诊断")
                            .id_salt("serial_diagnostics")
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled_ui(!self.diagnostics_running, |ui| {
                                            secondary_button(ui, "生成串口诊断", [124.0, 32.0])
                                        })
                                        .inner
                                        .clicked()
                                    {
                                        self.start_diagnostics(ctx);
                                    }
                                    if ui
                                        .add_enabled_ui(!self.diagnostics_text.is_empty(), |ui| {
                                            text_button(ui, "复制诊断")
                                        })
                                        .inner
                                        .clicked()
                                    {
                                        ctx.copy_text(self.diagnostics_text.clone());
                                    }
                                    if self.diagnostics_running {
                                        ui.spinner();
                                    }
                                });
                                if !self.diagnostics_text.is_empty() {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut self.diagnostics_text)
                                            .font(egui::TextStyle::Monospace)
                                            .interactive(false)
                                            .desired_width(ui.available_width())
                                            .desired_rows(8),
                                    );
                                }
                            });
                        if let Some(error) = &self.telemetry_settings_error {
                            banner(ui, error, true);
                        }
                        if let Some(error) = &self.record_submit_error {
                            banner(ui, error, true);
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if primary_button(ui, "保存设置", [124.0, 38.0]).clicked() {
                        self.save_settings_from_ui();
                    }
                    if self.settings_saved {
                        ui.label(RichText::new("设置已保存").size(12.0).color(ACCENT));
                    }
                    let local =
                        self.record_service.pending_local_count() + self.pending_records.len();
                    if local > 0 {
                        ui.label(
                            RichText::new(format!("{local} 条记录等待本地保存"))
                                .size(12.0)
                                .color(MUTED),
                        );
                    }
                });
            });
        self.settings_open = open;
        if check_updates {
            self.resume_settings_after_update = true;
            self.open_update_dialog();
            self.start_update_check(true, ctx);
        }
    }

    fn confirm_record_exit(&mut self, ctx: &egui::Context) {
        if !self.close_confirm_open {
            return;
        }
        let mut wait = false;
        let mut exit = false;
        let mut retry = false;
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
                } else {
                    ui.label(format!(
                        "本地已保存，仍有 {} 条上传未完成。",
                        self.record_service.pending_count()
                    ));
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
        if wait || response.should_close() {
            self.close_confirm_open = false;
        }
    }

    fn install_driver(&mut self, kind: DriverKind, selected_zip: Option<PathBuf>) {
        self.error = None;
        self.notice = None;
        self.driver_busy = true;
        self.driver_dialog_open = false;
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
            self.driver_dialog_open = false;
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
        if self.settings_open {
            return;
        }
        self.update_dialog_open = false;
        self.chip_picker_open = false;
        self.driver_dialog_open = false;
        self.pending_driver_license = None;
        self.telemetry_draft = self.telemetry_settings.clone();
        self.settings_headers = self
            .telemetry_settings
            .custom_headers
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        self.settings_auto_update = self.update_settings.auto_check;
        self.settings_saved = false;
        self.settings_open = true;
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let meta = Color32::from_rgb(144, 163, 184);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            icon(ui, Icon::Chip, 23.0, ACCENT);
            ui.label(
                RichText::new("FlashTool")
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
                if !self.update_service.is_busy()
                    && self.prepared_update.is_none()
                    && !self.installer_preparing
                {
                    self.start_update_check(true, ui.ctx());
                }
            }
        });
        ui.add_space(22.0);
        ui.label(RichText::new("工作空间").size(12.0).color(meta));
        if nav_item(ui, Icon::Grid, "批量烧录", !self.settings_open).clicked() {
            self.settings_open = false;
        }
        ui.add_space(20.0);
        ui.label(RichText::new("当前配置").size(12.0).color(meta));
        ui.label(
            RichText::new(self.selected_chip.label())
                .size(12.0)
                .strong()
                .color(Color32::from_rgb(220, 230, 241)),
        );
        ui.label(
            RichText::new(format!("{} · {} 台并发", self.baud, self.concurrency))
                .size(12.0)
                .color(meta),
        );
        if self.installer_preparing
            || self.update_service.is_busy()
            || self.prepared_update.is_some()
        {
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
                    RichText::new("设备烧录工作台")
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
                    self.chip_picker_open = false;
                    self.update_dialog_open = false;
                    self.settings_open = false;
                    self.driver_dialog_open = true;
                }
                let (text, color) = if self.running() {
                    ("批次运行中", ACCENT)
                } else if self.driver_busy {
                    ("驱动安装中", ACCENT)
                } else if self.ports.iter().all(|row| !row.online) {
                    ("等待连接设备", MUTED)
                } else {
                    ("设备已就绪", ACCENT)
                };
                controls_left = controls_left.min(status_pill(ui, text, color).left());
            });
        });
        let modal_open = self.settings_open
            || self.update_dialog_open
            || self.chip_picker_open
            || self.driver_dialog_open
            || self.pending_driver_license.is_some()
            || self.close_confirm_open;
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
            self.driver_dialog_open = false;
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
            panel_title(ui, Icon::File, "本批烧录配置", "01");
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
            egui::ComboBox::from_id_salt("baud")
                .selected_text(self.baud.to_string())
                .width(101.0)
                .show_ui(ui, |ui| {
                    for baud in [115_200, 460_800, 921_600] {
                        ui.selectable_value(&mut self.baud, baud, baud.to_string());
                    }
                });
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
        for (index, row) in self.firmware.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut row.enabled, "");
                    if segmented {
                        ui.add_sized(
                            [75.0, 22.0],
                            egui::Label::new(RichText::new(row.label).size(11.0)),
                        );
                    }
                    let width =
                        (ui.available_width() - if segmented { 190.0 } else { 160.0 }).max(160.0);
                    if ui
                        .allocate_ui_with_layout(
                            egui::vec2(width, 48.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| file_picker(ui, &row.path, false),
                        )
                        .inner
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("固件 BIN", &["bin"])
                            .add_filter("所有文件", &["*"])
                            .pick_file()
                    {
                        row.path = path;
                    }
                    ui.label(RichText::new("地址").size(11.0).color(MUTED));
                    ui.add_sized(
                        [89.0, 30.0],
                        egui::TextEdit::singleline(&mut row.address)
                            .font(egui::TextStyle::Monospace)
                            .margin(egui::vec2(7.0, 6.0)),
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
            panel_title(ui, Icon::Usb, "在线 COM 设备", "02");
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
                    .add_enabled(
                        self.watcher.is_some(),
                        egui::Checkbox::new(
                            &mut self.auto_refresh,
                            RichText::new("设备插拔自动识别").size(11.0),
                        ),
                    )
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
            let log_keys = &self.device_logs.current_keys;
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
                                    let key = log_keys
                                        .get(&row.port.name)
                                        .filter(|key| key.identity == row.port.identity)
                                        .cloned()
                                        .unwrap_or_else(|| DeviceLogKey::from_port(&row.port, ""));
                                    requested_logs.push(key);
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

    fn driver_panel(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |columns| {
            for (index, kind, name) in [
                (0, DriverKind::Ch343, "CH343 / CH9102"),
                (1, DriverKind::Cp210x, "CP2102 / CP210x"),
            ] {
                let column = &mut columns[index];
                let available = drivers::package_available(kind);
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .corner_radius(4.0)
                    .inner_margin(12.0)
                    .show(column, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(name).strong().size(13.0));
                                ui.label(
                                    RichText::new(
                                        drivers::package_source(kind).unwrap_or("官方驱动包未内置"),
                                    )
                                    .size(11.0)
                                    .color(MUTED),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if available {
                                        if secondary_button(ui, "安装驱动", [86.0, 34.0]).clicked()
                                        {
                                            self.request_bundled_driver(kind);
                                        }
                                    } else if secondary_button(ui, "选择 ZIP", [86.0, 34.0])
                                        .on_hover_text(
                                            "选择原厂完整驱动 ZIP 后手动安装；不会自动下载或安装。",
                                        )
                                        .clicked()
                                        && let Some(path) = rfd::FileDialog::new()
                                            .set_title(format!("选择 {name} 原厂完整驱动 ZIP"))
                                            .add_filter("驱动压缩包", &["zip"])
                                            .pick_file()
                                    {
                                        self.install_driver(kind, Some(path));
                                    }
                                },
                            );
                        });
                    });
            }
        });
    }

    fn driver_dialog(&mut self, ctx: &egui::Context) {
        if !self.driver_dialog_open {
            return;
        }
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("driver_manager"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(6.0)
                    .inner_margin(25.0),
            )
            .show(ctx, |ui| {
                ui.set_width(660.0);
                ui.horizontal(|ui| {
                    icon(ui, Icon::Shield, 22.0, ACCENT);
                    ui.label(RichText::new("安装 USB 串口驱动").size(22.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if text_button(ui, "关闭").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("选择设备对应的原厂驱动，安装时 Windows 将请求管理员授权。")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.add_space(15.0);
                self.driver_panel(ui);
                ui.add_space(10.0);
                ui.label(
                    RichText::new("驱动安装完成后会刷新串口。若尚未发现设备，请重新连接 USB Hub。")
                        .size(12.0)
                        .color(MUTED),
                );
            });
        if close || response.should_close() {
            self.driver_dialog_open = false;
        }
    }

    fn actions_panel(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
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
                        RichText::new(if running {
                            "请保持设备连接，当前任务会完成校验。"
                        } else {
                            "本批已结束，可调整配置继续烧录。"
                        })
                        .size(11.0)
                        .color(MUTED),
                    );
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if settings_icon_button(ui).on_hover_text("软件设置").clicked() {
                    self.open_settings();
                }
                let label = if running {
                    "批次正在运行…"
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
                        self.notice =
                            Some("已停止排队；正在烧录的设备将完成写入与校验，请保持连接。".into());
                    }
                } else if !self.jobs.is_empty() && text_button(ui, "导出 CSV").clicked() {
                    self.export_csv();
                }
            });
        });
    }

    fn logs_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            panel_title(ui, Icon::Grid, "烧录汇总", "03");
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
                if !self.jobs.is_empty() && text_button(ui, "导出本批 CSV").clicked() {
                    self.export_csv();
                }
            });
        });
        if !self.jobs.is_empty() {
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
                                    RichText::new("暂无烧录结果")
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
        let Some(path) = rfd::FileDialog::new()
            .set_title("导出本批烧录结果")
            .set_file_name(format!(
                "flashtool-{}.csv",
                Local::now().format("%Y%m%d-%H%M%S")
            ))
            .add_filter("CSV 表格", &["csv"])
            .save_file()
        else {
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
        append_csv(&mut output, header.iter().copied());
        for (port, row) in &self.jobs {
            let progress = format!("{:.1}%", row.progress * 100.0);
            let duration = row
                .duration_secs
                .map(|v| format!("{v:.3}"))
                .unwrap_or_default();
            append_csv(
                &mut output,
                [
                    self.batch_started.as_str(),
                    port.as_str(),
                    self.batch_chip.label(),
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
                ],
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
        self.poll_records();
        self.poll_diagnostics();
        self.collect_update_events(ctx);
        self.poll_self_update(ctx);
        self.poll_ports();
        let busy = self.busy();
        if ctx.input(|input| input.viewport().close_requested()) && !self.allow_exit {
            if self.installer_preparing {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.notice = Some("正在准备更新并检查写入权限，请稍候。".into());
            } else if busy {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.error = Some(
                    "任务正在进行，暂时不能关闭。可先停止排队，等待当前烧录或驱动安装完成。".into(),
                );
            } else if self.record_service.pending_local_count() > 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.notice = Some("正在保存本地烧录记录，请稍候再关闭。".into());
            } else if self.record_service.pending_count() > 0
                || !self.pending_records.is_empty()
                || !self.failed_records.is_empty()
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
            .show(ctx, |ui| self.actions_panel(ui));
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
                    .id_salt("workspace_content")
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
                        card(ui, |ui| {
                            ui.add_enabled_ui(!busy, |ui| self.firmware_panel(ui));
                        });
                        ui.add_space(12.0);
                        card(ui, |ui| self.ports_panel(ui));
                        ui.add_space(12.0);
                        card(ui, |ui| self.logs_panel(ui));
                        ui.add_space(8.0);
                    });
            });
        if !busy {
            self.chip_dialog(ctx);
            self.driver_dialog(ctx);
            self.driver_license_dialog(ctx);
        }
        if self.update_prompt_pending
            && !self.close_confirm_open
            && self.pending_driver_license.is_none()
            && !self.chip_picker_open
            && !self.driver_dialog_open
        {
            self.open_update_dialog();
        }
        self.update_dialog(ctx);
        self.settings_window(ctx);
        self.device_log_windows(ctx);
        self.confirm_record_exit(ctx);
        if self.busy()
            || self.update_checking
            || self.scanner.is_scanning()
            || self.diagnostics_running
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if let Some(delay) = self.scan_schedule.repaint_delay(Instant::now()) {
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
        Some("当前任务未结束，完成后可重启更新。")
    } else if local_pending > 0 || queued_records > 0 {
        Some("本地 CSV 正在保存，完成后可重启更新。")
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

fn terminal_summary(port: &str, state: &JobState) -> Option<String> {
    match state {
        JobState::Success => Some(format!("{port}烧录成功")),
        JobState::Failed => Some(format!("{port}烧录失败")),
        JobState::Cancelled => Some(format!("{port}已取消烧录")),
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
                    ui.add_enabled(
                        !locked && row.online,
                        egui::Checkbox::new(
                            &mut row.selected,
                            RichText::new(&row.port.name)
                                .monospace()
                                .size(15.0)
                                .strong(),
                        ),
                    );
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

fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Rect {
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            14,
        ))
        .corner_radius(20.0)
        .inner_margin(egui::Margin::symmetric(11, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                dot(ui, color, 5.0);
                ui.label(RichText::new(text).size(12.0).color(color));
            });
        })
        .response
        .rect
}

fn panel_title(ui: &mut egui::Ui, kind: Icon, title: &str, index: &str) {
    ui.horizontal(|ui| {
        icon(ui, kind, 17.0, ACCENT);
        ui.label(RichText::new(title).size(16.0).strong().color(INK));
        ui.label(
            RichText::new(index)
                .monospace()
                .size(10.0)
                .color(Color32::from_rgb(166, 178, 193)),
        );
    });
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

fn file_picker(ui: &mut egui::Ui, path: &Path, large: bool) -> egui::Response {
    let empty = path.as_os_str().is_empty();
    let height = if large { 111.0 } else { 48.0 };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    ui.painter().rect_filled(
        rect,
        8.0,
        if response.hovered() {
            ACCENT_LIGHT
        } else {
            BACKGROUND
        },
    );
    ui.painter().rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(1.0_f32, if response.hovered() { ACCENT } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    let filename = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "点击选择固件文件".into());
    if large {
        paint_icon(
            ui.painter(),
            rect.center_top() + egui::vec2(0.0, 27.0),
            24.0,
            Icon::File,
            ACCENT,
        );
        paint_text_ellipsis(
            ui,
            &filename,
            egui::pos2(rect.left() + 15.0, rect.top() + 51.0),
            rect.width() - 30.0,
            14.0,
            if empty { INK } else { ACCENT },
            true,
        );
        ui.painter().text(
            rect.center_bottom() - egui::vec2(0.0, 22.0),
            egui::Align2::CENTER_CENTER,
            if empty {
                "支持 .bin 格式 · 本地文件"
            } else {
                "点击更换文件"
            },
            egui::FontId::proportional(11.0),
            MUTED,
        );
    } else {
        paint_icon(
            ui.painter(),
            rect.left_center() + egui::vec2(22.0, 0.0),
            20.0,
            Icon::File,
            ACCENT,
        );
        paint_text_ellipsis(
            ui,
            &filename,
            rect.left_top() + egui::vec2(42.0, 12.0),
            rect.width() - 55.0,
            13.0,
            INK,
            false,
        );
        ui.painter().text(
            rect.left_bottom() + egui::vec2(42.0, -15.0),
            egui::Align2::LEFT_CENTER,
            if empty {
                "选择本地 .bin 文件"
            } else {
                "点击更换文件"
            },
            egui::FontId::proportional(10.0),
            MUTED,
        );
    }
    response.on_hover_text(if empty {
        "选择本地固件文件".into()
    } else {
        path.display().to_string()
    })
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
        if secondary_button(ui, "复制错误详情", [124.0, 34.0]).clicked() {
            ctx.copy_text(format!(
                "FlashTool {}\n{}\n{}",
                env!("CARGO_PKG_VERSION"),
                status,
                message
            ));
        }
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
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "时间无效".into())
}

fn timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn state_label(state: &JobState) -> &'static str {
    match state {
        JobState::Queued => "等待中",
        JobState::Connecting => "连接设备",
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
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r' | '\n'))
        {
            output.push('\'');
        }
        output.push_str(&value.replace('"', "\"\""));
        output.push('"');
    }
    output.push_str("\r\n");
}

fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = fs::read(Path::new(path)) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("chinese".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("chinese".into());
            }
            ctx.set_fonts(fonts);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BatchEventKind, DeviceIdentity, DeviceLogBook, DeviceLogKey, JobRow, JobState, Port,
        PortRow, ScanSchedule, append_csv, header_drag_region, missing_batch_jobs, reconcile_ports,
        reconcile_scan_result, restart_safety_reason, terminal_summary,
    };
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

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
            assert!(terminal_summary("COM11", &state).is_none());
        }
        assert_eq!(
            terminal_summary("COM11", &JobState::Success).as_deref(),
            Some("COM11烧录成功")
        );
        assert_eq!(
            terminal_summary("COM12", &JobState::Failed).as_deref(),
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
    fn successive_device_events_do_not_postpone_an_earlier_due_scan() {
        let mut schedule = ScanSchedule::default();
        let now = Instant::now();
        schedule.device_event(now);
        schedule.device_event(now + Duration::from_millis(150));
        assert_eq!(schedule.event_deadlines.front(), Some(&now));
        assert!(schedule.is_due(now + Duration::from_millis(150)));
        schedule.scan_started(now + Duration::from_millis(150));
        assert!(!schedule.is_due(now + Duration::from_millis(249)));
        assert!(schedule.is_due(now + Duration::from_millis(250)));
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
