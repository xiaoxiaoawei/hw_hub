use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use eframe::egui::{self, Color32, RichText};
use esp32c3_flashtool::{
    backend::{self, Port},
    batch::{Batch, BatchEvent, JobState},
    chip::ChipModel,
    drivers::{self, DriverKind},
    firmware::{FirmwareInput, prepare_firmware},
    updater::{self, CheckResult, ReleaseUpdate, UpdateSettings},
};

const ACCENT: Color32 = Color32::from_rgb(0, 133, 120);
const ACCENT_LIGHT: Color32 = Color32::from_rgb(230, 246, 242);
const INK: Color32 = Color32::from_rgb(27, 42, 63);
const MUTED: Color32 = Color32::from_rgb(119, 132, 151);
const BORDER: Color32 = Color32::from_rgb(227, 233, 240);
const BACKGROUND: Color32 = Color32::from_rgb(245, 247, 251);
const NAVY: Color32 = Color32::from_rgb(17, 29, 49);
const DANGER: Color32 = Color32::from_rgb(191, 65, 75);

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
}

struct JobRow {
    state: JobState,
    progress: f32,
    message: String,
    started: String,
    finished: String,
    elapsed: Option<Instant>,
    duration_secs: Option<f64>,
}

pub struct FlashApp {
    selected_chip: ChipModel,
    pending_chip: ChipModel,
    batch_chip: ChipModel,
    chip_picker_open: bool,
    driver_dialog_open: bool,
    layout: FirmwareLayout,
    firmware: Vec<FirmwareRow>,
    ports: Vec<PortRow>,
    baud: u32,
    concurrency: usize,
    batch: Option<Batch>,
    expected_jobs: usize,
    jobs: BTreeMap<String, JobRow>,
    logs: VecDeque<String>,
    error: Option<String>,
    notice: Option<String>,
    driver_result: Option<mpsc::Receiver<Result<String, String>>>,
    driver_busy: bool,
    last_scan: String,
    firmware_hash: String,
    batch_firmware: String,
    batch_started: String,
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
}

impl FlashApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = Color32::WHITE;
        visuals.override_text_color = Some(INK);
        visuals.selection.bg_fill = ACCENT_LIGHT;
        visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
        ] {
            widget.bg_fill = Color32::WHITE;
            widget.weak_bg_fill = Color32::WHITE;
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
            style.spacing.item_spacing = egui::vec2(10.0, 10.0);
            style.spacing.button_padding = egui::vec2(12.0, 9.0);
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
        let mut app = Self {
            selected_chip: ChipModel::default(),
            pending_chip: ChipModel::default(),
            batch_chip: ChipModel::default(),
            chip_picker_open: false,
            driver_dialog_open: false,
            layout: FirmwareLayout::Merged,
            firmware: vec![FirmwareRow::empty("合并固件", "0x0")],
            ports: Vec::new(),
            baud: 460_800,
            concurrency: 4,
            batch: None,
            expected_jobs: 0,
            jobs: BTreeMap::new(),
            logs: VecDeque::new(),
            error: None,
            notice: None,
            driver_result: None,
            driver_busy: false,
            last_scan: String::new(),
            firmware_hash: String::new(),
            batch_firmware: String::new(),
            batch_started: String::new(),
            logs_visible: false,
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
        self.running() || self.driver_busy
    }

    fn open_update_dialog(&mut self) {
        self.chip_picker_open = false;
        self.driver_dialog_open = false;
        self.update_dialog_open = true;
    }

    fn start_update_check(&mut self, manual: bool, ctx: &egui::Context) {
        if self.update_checking {
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

    fn collect_update_events(&mut self) {
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
                        self.update_latest_version = Some(release.version.clone());
                        self.update_status = format!("发现新版本 {}", release.version);
                        self.update_available = Some(release);
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
                self.update_error = Some(error);
            }
        }
    }

    fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.update_dialog_open {
            return;
        }
        let mut close = false;
        let mut manual_check = false;
        let mut enable_auto_check = false;
        let response = egui::Modal::new(egui::Id::new("application_updates"))
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .corner_radius(16.0)
                    .inner_margin(26.0),
            )
            .show(ctx, |ui| {
                ui.set_width(620.0);
                ui.horizontal(|ui| {
                    icon(ui, Icon::Chip, 25.0, ACCENT);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("FlashTool 更新").size(23.0).strong());
                        ui.label(
                            RichText::new(format!("当前版本 {}", env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if text_button(ui, "关闭").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(14.0);
                // Keep the subscription controls outside the scrolling content.
                // Reserve their full height even in the minimum 720px window.
                egui::ScrollArea::vertical()
                    .id_salt("update_dialog_content")
                    .max_height((ctx.screen_rect().height() - 420.0).max(180.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .fill(BACKGROUND)
                            .corner_radius(10.0)
                            .inner_margin(16.0)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    if self.update_checking {
                                        ui.spinner();
                                    }
                                    ui.label(
                                        RichText::new(&self.update_status)
                                            .size(16.0)
                                            .strong()
                                            .color(if self.update_available.is_some() {
                                                ACCENT
                                            } else {
                                                INK
                                            }),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled_ui(!self.update_checking, |ui| {
                                                    secondary_button(ui, "手动检查", [93.0, 34.0])
                                                })
                                                .inner
                                                .clicked()
                                            {
                                                manual_check = true;
                                            }
                                        },
                                    );
                                });
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(format!(
                                        "上次成功检查：{}",
                                        self.update_settings
                                            .last_checked_unix
                                            .map(format_update_time)
                                            .unwrap_or_else(|| "尚未检查".into())
                                    ))
                                    .size(11.0)
                                    .color(MUTED),
                                );
                                if let Some(version) = &self.update_latest_version {
                                    ui.label(
                                        RichText::new(format!("最新正式版本：{version}"))
                                            .size(12.0)
                                            .color(MUTED),
                                    );
                                }
                            });
                        ui.add_space(8.0);
                        let was_auto = self.update_settings.auto_check;
                        if ui
                            .checkbox(
                                &mut self.update_settings.auto_check,
                                "启动时自动检查更新（每天最多一次）",
                            )
                            .changed()
                        {
                            if let Err(error) = updater::save_settings(&self.update_settings) {
                                self.update_settings.auto_check = was_auto;
                                self.update_settings_error =
                                    Some(format!("无法保存自动检查设置：{error:#}"));
                            } else {
                                self.update_settings_error = None;
                                enable_auto_check = self.update_settings.auto_check;
                            }
                        }
                        if let Some(message) = &self.update_settings_error {
                            banner(ui, message, true);
                        }
                        if let Some(message) = &self.update_error {
                            ui.add_space(4.0);
                            banner(ui, &format!("更新服务暂不可用：{message}"), true);
                        }
                        if let Some(release) = &self.update_available {
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new(format!("版本 {}", release.version))
                                    .size(16.0)
                                    .strong()
                                    .color(ACCENT),
                            );
                            if !release.notes.trim().is_empty() {
                                egui::ScrollArea::vertical()
                                    .id_salt("release_notes")
                                    .max_height(120.0)
                                    .show(ui, |ui| {
                                        let mut notes =
                                            release.notes.chars().take(2_000).collect::<String>();
                                        if release.notes.chars().count() > 2_000 {
                                            notes.push_str("\n…完整说明请查看发布页面。");
                                        }
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(notes).size(12.0).color(MUTED),
                                            )
                                            .wrap(),
                                        );
                                    });
                            }
                            ui.add_space(5.0);
                            if release.download_url.is_none() {
                                ui.label(
                                    RichText::new("发布包尚未上传，可先查看发布页面。")
                                        .size(12.0)
                                        .color(MUTED),
                                );
                            }
                            ui.horizontal(|ui| {
                                if let Some(url) = &release.download_url
                                    && primary_button(ui, "下载新版", [124.0, 38.0]).clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab(url));
                                }
                                if secondary_button(ui, "查看发布", [105.0, 38.0]).clicked() {
                                    ctx.open_url(egui::OpenUrl::new_tab(&release.release_url));
                                }
                            });
                        }
                    });
                ui.add_space(14.0);
                mail_subscription_footer(ui, ctx);
            });
        if close || response.should_close() {
            self.update_dialog_open = false;
        }
        if manual_check {
            self.start_update_check(true, ctx);
        } else if enable_auto_check {
            self.start_update_check(false, ctx);
        }
    }

    fn scan_ports(&mut self) {
        // A newly seen/replaced port must be explicitly selected before flashing.
        let previous: HashMap<_, _> = self
            .ports
            .iter()
            .map(|r| {
                (
                    (r.port.name.clone(), r.port.description.clone()),
                    r.selected,
                )
            })
            .collect();
        match backend::list_ports() {
            Ok(ports) => {
                self.ports = ports
                    .into_iter()
                    .map(|port| {
                        let selected = previous
                            .get(&(port.name.clone(), port.description.clone()))
                            .copied()
                            .unwrap_or(false);
                        PortRow { port, selected }
                    })
                    .collect();
                self.last_scan = timestamp();
                self.error = None;
            }
            Err(err) => self.error = Some(format!("读取串口失败：{err:#}")),
        }
    }

    fn start_batch(&mut self) {
        self.error = None;
        self.notice = None;
        let ports: Vec<_> = self
            .ports
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.port.name.clone())
            .collect();
        if ports.is_empty() {
            self.error = Some("请先勾选需要烧录的串口。".into());
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
        match Batch::start(ports, self.baud, self.concurrency, Arc::new(firmware)) {
            Ok(batch) => {
                self.jobs.clear();
                self.expected_jobs = expected_jobs;
                self.logs.clear();
                self.firmware_hash = firmware_hash;
                self.batch_firmware = batch_firmware;
                self.batch_chip = self.selected_chip;
                self.batch_started = timestamp();
                self.batch = Some(batch);
                self.log(
                    "批次开始。正在写入的设备会完成写入与校验；“停止排队”只取消尚未开始的任务。"
                        .into(),
                );
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
            });
            let changed = row.state != state || row.message != message;
            if state == JobState::Connecting && row.started.is_empty() {
                row.started = now.clone();
                row.elapsed = Some(Instant::now());
            }
            if is_terminal(&state) && row.finished.is_empty() {
                row.finished = now;
                row.duration_secs = row.elapsed.map(|start| start.elapsed().as_secs_f64());
            }
            row.state = state.clone();
            row.progress = progress.clamp(0.0, 1.0);
            row.message = message.clone();
            if changed && state != JobState::Writing {
                self.log(format!("[{port}] {}  {message}", state_label(&state)));
            }
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

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            icon(ui, Icon::Chip, 24.0, Color32::from_rgb(79, 215, 191));
            ui.label(
                RichText::new("FlashTool")
                    .size(23.0)
                    .strong()
                    .color(Color32::WHITE),
            );
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.label(
                RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(10.0)
                    .color(Color32::from_rgb(123, 145, 169)),
            );
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("检查更新")
                            .size(11.0)
                            .color(Color32::from_rgb(133, 209, 193)),
                    )
                    .frame(false)
                    .min_size(egui::vec2(0.0, 23.0)),
                )
                .clicked()
            {
                self.open_update_dialog();
                self.start_update_check(true, ui.ctx());
            }
        });
        ui.add_space(30.0);
        ui.label(
            RichText::new("工作空间")
                .size(11.0)
                .color(Color32::from_rgb(103, 124, 150)),
        );
        ui.add_space(8.0);
        egui::Frame::new()
            .fill(Color32::from_rgb(27, 52, 70))
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 13))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    icon(ui, Icon::Grid, 17.0, Color32::from_rgb(96, 222, 197));
                    ui.label(
                        RichText::new("批量烧录")
                            .strong()
                            .color(Color32::from_rgb(187, 244, 231)),
                    );
                });
            });
        ui.add_space(24.0);
        ui.label(
            RichText::new("当前配置")
                .size(11.0)
                .color(Color32::from_rgb(103, 124, 150)),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(self.selected_chip.label())
                .size(16.0)
                .strong()
                .color(Color32::from_rgb(220, 230, 241)),
        );
        ui.label(
            RichText::new(format!("{} · {} 台并发", self.baud, self.concurrency))
                .size(11.0)
                .color(Color32::from_rgb(133, 153, 177)),
        );
        if self.update_checking {
            ui.add_space(15.0);
            ui.label(
                RichText::new("正在检查更新…")
                    .size(11.0)
                    .color(Color32::from_rgb(133, 153, 177)),
            );
        } else if let Some(version) = self
            .update_available
            .as_ref()
            .map(|release| release.version.clone())
        {
            ui.add_space(20.0);
            egui::Frame::new()
                .fill(Color32::from_rgb(24, 63, 66))
                .corner_radius(8.0)
                .inner_margin(11.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(
                        RichText::new("发现新版本")
                            .size(11.0)
                            .color(Color32::from_rgb(130, 218, 196)),
                    );
                    ui.label(
                        RichText::new(version)
                            .size(15.0)
                            .strong()
                            .color(Color32::from_rgb(206, 247, 234)),
                    );
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("查看更新  >")
                                    .size(11.0)
                                    .color(Color32::from_rgb(130, 218, 196)),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        self.open_update_dialog();
                    }
                });
        } else if self.update_settings_error.is_some() || self.update_error.is_some() {
            ui.add_space(15.0);
            let text = if self.update_settings_error.is_some() {
                "更新设置需要检查"
            } else {
                "更新检查暂不可用"
            };
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(text)
                            .size(10.0)
                            .color(Color32::from_rgb(149, 163, 182)),
                    )
                    .frame(false),
                )
                .clicked()
            {
                self.open_update_dialog();
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(10.0);
            ui.label(
                RichText::new("写入校验 / 自动复位")
                    .size(11.0)
                    .color(Color32::from_rgb(103, 124, 150)),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let color = if self.ports.is_empty() {
                    Color32::from_rgb(105, 123, 145)
                } else {
                    Color32::from_rgb(76, 204, 173)
                };
                dot(ui, color, 6.0);
                ui.label(
                    RichText::new(format!("{} 个串口在线", self.ports.len()))
                        .size(12.0)
                        .color(Color32::from_rgb(168, 188, 207)),
                );
            });
        });
    }

    fn heading(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("批量烧录工作台")
                        .size(27.0)
                        .strong()
                        .color(INK),
                );
                ui.add_space(3.0);
                ui.label(
                    RichText::new("连接设备，选择固件，一次完成整批烧录。")
                        .size(13.0)
                        .color(MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled_ui(!self.busy(), |ui| {
                        secondary_button(ui, "安装驱动", [94.0, 35.0])
                    })
                    .inner
                    .clicked()
                {
                    self.chip_picker_open = false;
                    self.update_dialog_open = false;
                    self.driver_dialog_open = true;
                }
                let (text, color) = if self.running() {
                    ("批次运行中", ACCENT)
                } else if self.driver_busy {
                    ("驱动安装中", ACCENT)
                } else if self.ports.is_empty() {
                    ("等待连接设备", MUTED)
                } else {
                    ("设备已就绪", ACCENT)
                };
                status_pill(ui, text, color);
            });
        });
    }

    fn chip_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(ACCENT_LIGHT)
                .corner_radius(12.0)
                .inner_margin(15.0)
                .show(ui, |ui| {
                    icon(ui, Icon::Chip, 30.0, ACCENT);
                });
            ui.vertical(|ui| {
                ui.label(RichText::new("目标芯片").size(12.0).color(MUTED));
                ui.label(
                    RichText::new(self.selected_chip.label())
                        .size(23.0)
                        .strong()
                        .color(INK),
                );
                ui.label(
                    RichText::new(format!(
                        "分段 Bootloader 默认地址 0x{:X}",
                        self.selected_chip.bootloader_address()
                    ))
                    .size(12.0)
                    .color(MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, "选择芯片  >", [112.0, 38.0]).clicked() {
                    self.pending_chip = self.selected_chip;
                    self.driver_dialog_open = false;
                    self.update_dialog_open = false;
                    self.chip_picker_open = true;
                }
            });
        });
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
                    .fill(Color32::WHITE)
                    .corner_radius(16.0)
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
        panel_title(ui, Icon::File, "固件配置", "01");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            for (layout, label) in [
                (FirmwareLayout::Merged, "合并固件"),
                (FirmwareLayout::Segments, "分段固件"),
            ] {
                let selected = self.layout == layout;
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).color(if selected {
                            ACCENT
                        } else {
                            MUTED
                        }))
                        .fill(if selected {
                            ACCENT_LIGHT
                        } else {
                            Color32::WHITE
                        })
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(7.0)
                        .min_size(egui::vec2(106.0, 33.0)),
                    )
                    .clicked()
                {
                    self.set_layout(layout);
                }
            }
        });
        ui.add_space(5.0);
        let mut remove = None;
        let single = self.firmware.len() == 1 && self.layout == FirmwareLayout::Merged;
        for (index, row) in self.firmware.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                if !single {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut row.enabled, row.label);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if text_button(ui, "移除").clicked() {
                                remove = Some(index);
                            }
                        });
                    });
                }
                if file_picker(ui, &row.path, single).clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("固件 BIN", &["bin"])
                        .add_filter("所有文件", &["*"])
                        .pick_file()
                {
                    row.path = path;
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("写入地址").size(12.0).color(MUTED));
                    ui.add_sized(
                        [100.0, 30.0],
                        egui::TextEdit::singleline(&mut row.address)
                            .font(egui::TextStyle::Monospace)
                            .margin(egui::vec2(9.0, 6.0)),
                    );
                    ui.label(RichText::new("HEX").monospace().size(10.0).color(MUTED));
                    if single {
                        ui.label(RichText::new("合并 BIN 通常为 0x0").size(11.0).color(MUTED));
                    }
                });
                if !single {
                    ui.add_space(8.0);
                }
            });
        }
        if let Some(index) = remove {
            self.firmware.remove(index);
        }
        if self.layout == FirmwareLayout::Segments && text_button(ui, "+ 添加固件段").clicked()
        {
            self.firmware.push(FirmwareRow::empty("自定义固件", "0x0"));
        }
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.columns(2, |columns| {
            columns[0].label(RichText::new("波特率").size(12.0).color(MUTED));
            egui::ComboBox::from_id_salt("baud")
                .selected_text(self.baud.to_string())
                .width(columns[0].available_width() - 10.0)
                .show_ui(&mut columns[0], |ui| {
                    for baud in [115_200, 460_800, 921_600] {
                        ui.selectable_value(&mut self.baud, baud, baud.to_string());
                    }
                });
            columns[1].label(RichText::new("并发设备数").size(12.0).color(MUTED));
            let width = columns[1].available_width();
            columns[1].add_sized(
                [width, 32.0],
                egui::DragValue::new(&mut self.concurrency)
                    .range(1..=16)
                    .suffix(" 台"),
            );
        });
        ui.add_space(5.0);
        ui.label(
            RichText::new("USB Hub 建议从 4 台并发开始。")
                .size(11.0)
                .color(MUTED),
        );
    }

    fn ports_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            panel_title(ui, Icon::Usb, "设备连接", "02");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, "刷新", [62.0, 30.0]).clicked() {
                    self.scan_ports();
                }
            });
        });
        if self.ports.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 68.0),
                    egui::Sense::hover(),
                );
                ui.painter().circle_filled(rect.center(), 33.0, BACKGROUND);
                paint_icon(
                    ui.painter(),
                    rect.center(),
                    30.0,
                    Icon::Usb,
                    Color32::from_rgb(144, 161, 180),
                );
                ui.add_space(5.0);
                ui.label(RichText::new("等待设备连接").size(18.0).strong().color(INK));
                ui.add_space(3.0);
                ui.label(
                    RichText::new("连接 USB Hub 与待烧录设备，\n然后刷新串口列表。")
                        .size(13.0)
                        .color(MUTED),
                );
                ui.add_space(20.0);
                status_pill(ui, "尚未发现串口", MUTED);
            });
            ui.add_space(20.0);
        } else {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if text_button(ui, "选择已识别 USB 串口").clicked() {
                    for row in &mut self.ports {
                        row.selected = row.port.usb && row.port.suggested;
                    }
                }
                if text_button(ui, "清空").clicked() {
                    for row in &mut self.ports {
                        row.selected = false;
                    }
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("ports_scroll")
                .max_height(255.0)
                .show(ui, |ui| {
                    for row in &mut self.ports {
                        egui::Frame::new()
                            .fill(if row.selected {
                                ACCENT_LIGHT
                            } else {
                                BACKGROUND
                            })
                            .corner_radius(8.0)
                            .inner_margin(11.0)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.checkbox(
                                        &mut row.selected,
                                        RichText::new(&row.port.name).strong().size(15.0),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if row.port.usb {
                                                ui.label(
                                                    RichText::new("USB")
                                                        .monospace()
                                                        .size(10.0)
                                                        .color(ACCENT),
                                                );
                                            }
                                        },
                                    );
                                });
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&row.port.description)
                                            .size(11.0)
                                            .color(MUTED),
                                    )
                                    .wrap(),
                                );
                            });
                        ui.add_space(5.0);
                    }
                });
        }
        ui.separator();
        ui.add_space(3.0);
        ui.label(
            RichText::new("仅勾选本批待烧录设备，新接入串口不会自动选中。")
                .size(11.0)
                .color(MUTED),
        );
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
                    .corner_radius(8.0)
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
                                            self.install_driver(kind, None);
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
                    .fill(Color32::WHITE)
                    .corner_radius(16.0)
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
        let selected = self.ports.iter().filter(|row| row.selected).count();
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
                        RichText::new(format!("已选择 {selected} 台设备"))
                            .size(16.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new("写入后校验，通过后报告成功")
                            .size(11.0)
                            .color(MUTED),
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
                        self.log("用户停止排队。当前任务继续完成写入与校验。".into());
                    }
                } else if !self.jobs.is_empty() && text_button(ui, "导出 CSV").clicked() {
                    self.export_csv();
                }
            });
        });
    }

    fn results_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            panel_title(ui, Icon::Grid, "批次记录", "04");
            ui.label(
                RichText::new(format!(
                    "{} · {}",
                    self.batch_chip.label(),
                    self.batch_started
                ))
                .size(11.0)
                .color(MUTED),
            );
        });
        ui.add_space(10.0);
        egui::ScrollArea::vertical()
            .id_salt("results_scroll")
            .max_height(310.0)
            .show(ui, |ui| {
                for (port, row) in &self.jobs {
                    let color = match row.state {
                        JobState::Success => ACCENT,
                        JobState::Failed => DANGER,
                        _ => MUTED,
                    };
                    egui::Frame::new()
                        .fill(BACKGROUND)
                        .corner_radius(8.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(port).monospace().size(15.0).strong());
                                status_pill(ui, state_label(&row.state), color);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let elapsed = row.duration_secs.or_else(|| {
                                            row.elapsed.map(|i| i.elapsed().as_secs_f64())
                                        });
                                        if let Some(elapsed) = elapsed {
                                            ui.label(
                                                RichText::new(format!("{elapsed:.1} s"))
                                                    .size(11.0)
                                                    .color(MUTED),
                                            );
                                        }
                                    },
                                );
                            });
                            ui.add(
                                egui::ProgressBar::new(row.progress)
                                    .desired_width(ui.available_width())
                                    .desired_height(5.0)
                                    .fill(color),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(&row.message).size(12.0).color(
                                    if row.state == JobState::Failed {
                                        DANGER
                                    } else {
                                        MUTED
                                    },
                                ))
                                .wrap(),
                            );
                        });
                    ui.add_space(5.0);
                }
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if text_button(
                ui,
                if self.logs_visible {
                    "收起运行日志"
                } else {
                    "查看运行日志"
                },
            )
            .clicked()
            {
                self.logs_visible = !self.logs_visible;
            }
            ui.label(
                RichText::new(format!("{} 条", self.logs.len()))
                    .size(11.0)
                    .color(MUTED),
            );
        });
        if self.logs_visible {
            egui::Frame::new()
                .fill(NAVY)
                .corner_radius(8.0)
                .inner_margin(13.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("logs_scroll")
                        .max_height(165.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.logs {
                                ui.label(
                                    RichText::new(line)
                                        .monospace()
                                        .size(11.0)
                                        .color(Color32::from_rgb(186, 210, 217)),
                                );
                            }
                        });
                });
        }
        if !self.firmware_hash.is_empty() {
            ui.add_space(5.0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!("SHA256  {}", self.firmware_hash))
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                )
                .wrap(),
            );
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
        self.collect_update_events();
        let busy = self.busy();
        if ctx.input(|input| input.viewport().close_requested()) && busy {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.error = Some(
                "任务正在进行，暂时不能关闭。可先停止排队，等待当前烧录或驱动安装完成。".into(),
            );
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
                    .fill(Color32::WHITE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .inner_margin(egui::Margin::symmetric(24, 17)),
            )
            .show(ctx, |ui| self.actions_panel(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(25, 23)),
            )
            .show(ctx, |ui| {
                self.heading(ui);
                ui.add_space(21.0);
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
                            ui.add_enabled_ui(!busy, |ui| self.chip_panel(ui));
                        });
                        ui.add_space(16.0);
                        if ui.available_width() >= 800.0 {
                            ui.columns(2, |columns| {
                                card(&mut columns[0], |ui| {
                                    ui.add_enabled_ui(!busy, |ui| self.firmware_panel(ui));
                                });
                                card(&mut columns[1], |ui| {
                                    ui.add_enabled_ui(!busy, |ui| self.ports_panel(ui));
                                });
                            });
                        } else {
                            card(ui, |ui| {
                                ui.add_enabled_ui(!busy, |ui| self.firmware_panel(ui));
                            });
                            ui.add_space(16.0);
                            card(ui, |ui| {
                                ui.add_enabled_ui(!busy, |ui| self.ports_panel(ui));
                            });
                        }
                        if !self.jobs.is_empty() {
                            ui.add_space(16.0);
                            card(ui, |ui| self.results_panel(ui));
                        }
                        ui.add_space(8.0);
                    });
            });
        if !busy {
            self.chip_dialog(ctx);
            self.driver_dialog(ctx);
        }
        self.update_dialog(ctx);
        if self.busy() || self.update_checking {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Chip,
    Usb,
    File,
    Grid,
    Shield,
}

fn paint_icon(painter: &egui::Painter, center: egui::Pos2, size: f32, kind: Icon, color: Color32) {
    let p = |x: f32, y: f32| center + egui::vec2(x * size, y * size);
    let stroke = egui::Stroke::new(1.65_f32, color);
    match kind {
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

fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
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
                ui.label(RichText::new(text).size(11.0).color(color));
            });
        });
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
        .corner_radius(8.0)
        .min_size(egui::vec2(size[0], size[1])),
    )
}

fn secondary_button(ui: &mut egui::Ui, text: &str, size: [f32; 2]) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(12.0).color(INK))
            .fill(Color32::WHITE)
            .stroke(egui::Stroke::new(1.0_f32, BORDER))
            .corner_radius(7.0)
            .min_size(egui::vec2(size[0], size[1])),
    )
}

fn text_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(12.0).color(ACCENT)).frame(false))
}

fn file_picker(ui: &mut egui::Ui, path: &Path, large: bool) -> egui::Response {
    let empty = path.as_os_str().is_empty();
    let height = if large { 111.0 } else { 63.0 };
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
        .fill(Color32::WHITE)
        .stroke(egui::Stroke::new(1.0_f32, BORDER))
        .corner_radius(12.0)
        .inner_margin(19.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            content(ui)
        });
}

fn banner(ui: &mut egui::Ui, message: &str, error: bool) {
    egui::Frame::new()
        .fill(if error {
            Color32::from_rgb(255, 239, 241)
        } else {
            ACCENT_LIGHT
        })
        .corner_radius(9.0)
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

fn mail_subscription_footer(ui: &mut egui::Ui, ctx: &egui::Context) {
    egui::Frame::new()
        .fill(ACCENT_LIGHT)
        .corner_radius(10.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height(94.0);
            ui.label(
                RichText::new("新版本邮件提醒")
                    .size(14.0)
                    .strong()
                    .color(INK),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "在 GitHub 仓库选择 Watch → Custom → Releases，并在通知设置中启用邮件。",
                    )
                    .size(11.0)
                    .color(MUTED),
                )
                .wrap(),
            );
            if secondary_button(ui, "订阅版本邮件", [137.0, 36.0]).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab(
                    "https://github.com/xiaoxiaoawei/hw_hub",
                ));
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
    use super::append_csv;

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
