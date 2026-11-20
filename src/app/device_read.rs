use super::*;
use esp32c3_flashtool::serial_capture::CaptureOptions;

pub(super) struct DeviceReadSetup {
    pub(super) open: bool,
    baud: String,
    reset: bool,
}

impl Default for DeviceReadSetup {
    fn default() -> Self {
        Self {
            open: false,
            baud: "115200".into(),
            reset: false,
        }
    }
}

impl FlashApp {
    pub(super) fn device_read_dialog(&mut self, ctx: &egui::Context) {
        if !self.device_read_setup.open {
            return;
        }
        let selected = self.selected_online_ports();
        let has_rules = !self.telemetry_settings.extraction_rules.is_empty();
        let busy = self.busy();
        let mut start = None;
        let mut configure = false;
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("device_read_setup"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .corner_radius(8.0)
                    .inner_margin(18.0),
            )
            .show(ctx, |ui| {
                ui.set_width(385.0);
                ui.label(RichText::new("批量读取日志").size(17.0).strong());
                ui.add_space(8.0);
                ui.label(format!(
                    "已选 {} 台设备 · 每台读取最多 10 秒",
                    selected.len()
                ));
                ui.horizontal(|ui| {
                    ui.label("串口波特率");
                    baud_editor(ui, &mut self.device_read_setup.baud, "read_baud", 1);
                });
                green_checkbox(ui, &mut self.device_read_setup.reset, "读取前复位设备");
                if self.device_read_setup.reset {
                    ui.label(
                        RichText::new("适用于支持串口自动复位的设备，用于采集启动时输出的字段。")
                            .size(12.0)
                            .color(MUTED),
                    );
                }
                if !has_rules {
                    ui.label(RichText::new("请先在高级配置保存要提取的字段。").color(MUTED));
                    configure = text_button(ui, "配置提取规则").clicked();
                }
                ui.add_space(10.0);
                let baud = parse_baud_input(&self.device_read_setup.baud, 1).ok();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled_ui(
                            !busy && !selected.is_empty() && has_rules && baud.is_some(),
                            |ui| primary_button(ui, "开始读取", [112.0, 37.0]),
                        )
                        .inner
                        .clicked()
                    {
                        start = baud;
                    }
                    close = secondary_button(ui, "取消", [78.0, 37.0]).clicked();
                });
            });
        if close || response.should_close() || start.is_some() || configure {
            self.device_read_setup.open = false;
        }
        if configure {
            self.open_settings();
            settings::select_page(ctx, settings::SettingsPage::Advanced);
        }
        if let Some(baud) = start {
            self.start_read(selected, baud);
        }
    }

    fn start_read(&mut self, ports: Vec<Port>, baud: u32) {
        self.error = None;
        self.notice = None;
        let Some(setup) = self.prepare_record_setup(RecordOperation::ReadOnly, false) else {
            return;
        };
        let options = CaptureOptions {
            baud,
            reset: self.device_read_setup.reset,
            ..CaptureOptions::default()
        };
        match Batch::start_reading(ports.clone(), self.concurrency, options, &setup.settings) {
            Ok(batch) => {
                self.firmware_hash.clear();
                self.batch_firmware.clear();
                self.attach_batch(batch, ports, RecordOperation::ReadOnly, setup);
            }
            Err(error) => self.error = Some(format!("无法开始读取日志：{error:#}")),
        }
    }
}
