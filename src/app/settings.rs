use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SettingsPage {
    #[default]
    General,
    Drivers,
    Advanced,
}

pub(super) fn select_page(ctx: &egui::Context, page: SettingsPage) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("settings_page"), page));
}

const EXTRACTION_RULES_EXAMPLE: &str = r#"{
  "unique_device_id": "UNIQUE DEVICE ID:\\s*([0-9A-Fa-f]{32})\\b"
}"#;

fn extraction_rules_json(rules: &BTreeMap<String, String>) -> String {
    serde_json::to_string_pretty(rules).unwrap_or_else(|_| "{}".into())
}

struct ExtractionRulesValidation {
    text: String,
    result: Result<usize, String>,
}

fn extraction_rules_validation(ctx: &egui::Context, text: &str) -> Result<usize, String> {
    let id = egui::Id::new("settings_extraction_rules_validation");
    let cached =
        ctx.data_mut(|data| data.get_temp::<std::sync::Arc<ExtractionRulesValidation>>(id));
    if let Some(cached) = cached
        && cached.text == text
    {
        return cached.result.clone();
    }
    let result = telemetry::parse_extraction_rules_json(text)
        .map(|rules| rules.len())
        .map_err(|error| format!("{error:#}"));
    ctx.data_mut(|data| {
        data.insert_temp(
            id,
            std::sync::Arc::new(ExtractionRulesValidation {
                text: text.into(),
                result: result.clone(),
            }),
        );
    });
    result
}

fn extraction_rules_layout(text: &str, wrap_width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let mut offset = 0;
    while offset < text.len() {
        let start = offset;
        let color = if text.as_bytes()[offset] == b'"' {
            offset += 1;
            let mut escaped = false;
            while offset < text.len() {
                let character = text[offset..].chars().next().unwrap();
                offset += character.len_utf8();
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    break;
                }
            }
            if text[offset..].trim_start().starts_with(':') {
                ACCENT
            } else {
                SUCCESS
            }
        } else {
            offset += text[offset..].find('"').unwrap_or(text.len() - offset);
            MUTED
        };
        job.append(
            &text[start..offset],
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color,
                ..Default::default()
            },
        );
    }
    job
}

fn headers_match_saved(rows: &[(String, String)], saved: &BTreeMap<String, String>) -> bool {
    let mut headers = BTreeMap::new();
    let mut names = BTreeSet::new();
    for (name, value) in rows {
        let name = name.trim();
        if name.is_empty() && value.is_empty() {
            continue;
        }
        if name.is_empty() || !names.insert(name.to_ascii_lowercase()) {
            return false;
        }
        headers.insert(name.to_owned(), value.clone());
    }
    &headers == saved
}

fn settings_are_dirty(
    draft: &TelemetrySettings,
    saved: &TelemetrySettings,
    headers: &[(String, String)],
    auto_check: bool,
    saved_auto_check: bool,
) -> bool {
    draft != saved
        || auto_check != saved_auto_check
        || !headers_match_saved(headers, &saved.custom_headers)
}

fn page_heading(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).heading().size(20.0).strong());
    ui.add_space(6.0);
}

impl FlashApp {
    pub(super) fn has_unsaved_settings(&self) -> bool {
        settings_are_dirty(
            &self.telemetry_draft,
            &self.telemetry_settings,
            &self.settings_headers,
            self.settings_auto_update,
            self.update_settings.auto_check,
        ) || self.settings_extraction_rules_json
            != extraction_rules_json(&self.telemetry_settings.extraction_rules)
    }

    pub(super) fn discard_settings_draft(&mut self) {
        self.telemetry_draft = self.telemetry_settings.clone();
        self.settings_headers = self
            .telemetry_settings
            .custom_headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        self.settings_auto_update = self.update_settings.auto_check;
        self.settings_extraction_rules_json =
            extraction_rules_json(&self.telemetry_settings.extraction_rules);
        self.settings_saved = false;
        self.resume_settings_after_update = false;
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut cancel = false;
        let mut check_updates = false;
        let screen = ctx.screen_rect();
        let body_height = (screen.height() - 144.0).clamp(280.0, 500.0);
        let window_width = (screen.width() - 40.0).min(780.0);
        let mut page = ctx.data_mut(|data| {
            data.get_temp::<SettingsPage>(egui::Id::new("settings_page"))
                .unwrap_or_default()
        });
        egui::Window::new("软件设置")
            .id(egui::Id::new("software_settings"))
            .open(&mut open)
            .default_width(window_width)
            .default_height(body_height + 58.0)
            .min_width(window_width.min(650.0))
            .max_width(screen.width() - 24.0)
            .default_pos(
                screen.center() - egui::vec2(window_width / 2.0, (body_height + 103.0) / 2.0),
            )
            .resizable([true, false])
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(148.0, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(148.0);
                            for (target, label, icon_kind) in [
                                (SettingsPage::General, "通用设置", Icon::Settings),
                                (SettingsPage::Drivers, "驱动安装", Icon::Shield),
                                (SettingsPage::Advanced, "高级配置", Icon::Grid),
                            ] {
                                if nav_item(ui, icon_kind, label, page == target).clicked() {
                                    page = target;
                                }
                            }
                        },
                    );
                    let (separator, _) =
                        ui.allocate_exact_size(egui::vec2(1.0, body_height), egui::Sense::hover());
                    ui.painter().rect_filled(separator, 0.0, BORDER);
                    let width = ui.available_width();
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            egui::ScrollArea::vertical()
                                .id_salt(("settings_page_content", page as u8))
                                .min_scrolled_height(260.0)
                                .max_height(body_height)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    match page {
                                        SettingsPage::General => {
                                            check_updates = self.settings_general_page(ui)
                                        }
                                        SettingsPage::Drivers => self.settings_drivers_page(ui),
                                        SettingsPage::Advanced => self.settings_advanced_page(ui),
                                    }
                                    if let Some(error) = &self.telemetry_settings_error {
                                        banner(ui, error, true);
                                    }
                                    if let Some(error) = &self.record_submit_error {
                                        banner(ui, error, true);
                                    }
                                });
                        },
                    );
                });
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, "保存设置", [116.0, 34.0]).clicked() {
                        self.save_settings_from_ui();
                    }
                    if secondary_button(ui, "取消", [76.0, 34.0]).clicked() {
                        cancel = true;
                    }
                    if self.has_unsaved_settings() {
                        ui.label(RichText::new("尚未保存").size(12.0).color(MUTED));
                    } else if self.settings_saved {
                        ui.label(RichText::new("设置已保存").size(12.0).color(SUCCESS));
                    }
                });
            });
        select_page(ctx, page);
        if !open || cancel {
            self.settings_open = false;
            self.discard_settings_draft();
        } else {
            self.settings_open = true;
        }
        if check_updates {
            self.open_update_dialog();
            self.start_update_check(true, ctx);
        }
    }

    fn settings_general_page(&mut self, ui: &mut egui::Ui) -> bool {
        page_heading(ui, "通用设置");
        let mut check_updates = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("FlashTool v{}", env!("CARGO_PKG_VERSION")))
                    .size(14.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, "检查更新", [92.0, 31.0]).clicked() {
                    check_updates = true;
                }
            });
        });
        green_checkbox(ui, &mut self.settings_auto_update, "启动时自动检查更新");
        ui.label(
            RichText::new(format!(
                "上次成功检查：{}",
                self.update_settings
                    .last_checked_unix
                    .map(format_update_time)
                    .unwrap_or_else(|| "尚未检查".into())
            ))
            .size(12.0)
            .color(MUTED),
        );
        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("本地 CSV 记录").heading().size(16.0).strong());
        ui.horizontal_wrapped(|ui| {
            if secondary_button(ui, "选择目录", [90.0, 31.0]).clicked()
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
            let directory = self
                .telemetry_draft
                .csv_directory
                .clone()
                .map(Ok)
                .unwrap_or_else(telemetry::default_records_directory);
            if let Ok(path) = directory
                && ui
                    .add_enabled_ui(path.is_dir(), |ui| text_button(ui, "打开目录"))
                    .inner
                    .clicked()
                && let Err(error) = open_records_directory(&path)
            {
                self.telemetry_settings_error = Some(format!("无法打开记录目录：{error}"));
            }
        });
        let directory = self
            .telemetry_draft
            .csv_directory
            .clone()
            .map(Ok)
            .unwrap_or_else(telemetry::default_records_directory);
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
            RichText::new("设备任务结束后自动保存本地 CSV。")
                .size(12.0)
                .color(MUTED),
        );
        if !self.failed_records.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("{} 条本地记录保存失败", self.failed_records.len()))
                    .size(12.0)
                    .color(DANGER),
            );
            if secondary_button(ui, "保存设置并重试", [140.0, 32.0]).clicked()
                && self.save_settings_from_ui()
            {
                self.retry_failed_local_records();
            }
        }
        let local = self.record_service.pending_local_count() + self.pending_records.len();
        if local > 0 {
            ui.label(
                RichText::new(format!("{local} 条记录等待本地保存"))
                    .size(12.0)
                    .color(MUTED),
            );
        }
        check_updates
    }

    fn settings_drivers_page(&mut self, ui: &mut egui::Ui) {
        page_heading(ui, "驱动安装");
        ui.label(
            RichText::new("安装时 Windows 将请求管理员授权。")
                .size(12.0)
                .color(MUTED),
        );
        ui.add_enabled_ui(!self.busy(), |ui| {
            for (kind, name) in [
                (DriverKind::Ch343, "CH343 / CH9102"),
                (DriverKind::Cp210x, "CP2102 / CP210x"),
            ] {
                egui::Frame::new()
                    .fill(SURFACE_ALT)
                    .corner_radius(4.0)
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(name).heading().size(15.0).strong());
                                ui.label(
                                    RichText::new(
                                        drivers::package_source(kind).unwrap_or("未内置驱动包"),
                                    )
                                    .size(12.0)
                                    .color(MUTED),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if drivers::package_available(kind) {
                                        if secondary_button(ui, "安装驱动", [94.0, 32.0]).clicked()
                                        {
                                            self.request_bundled_driver(kind);
                                        }
                                    } else if secondary_button(ui, "选择 ZIP", [94.0, 32.0])
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
        if self.driver_busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在安装驱动…");
            });
        }
    }

    fn settings_advanced_page(&mut self, ui: &mut egui::Ui) {
        page_heading(ui, "高级配置");
        ui.label(RichText::new("日志上报").heading().size(16.0).strong());
        green_checkbox(ui, &mut self.telemetry_draft.upload_enabled, "启用日志上报");
        ui.label(RichText::new("POST 接口地址").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.telemetry_draft.endpoint)
                .desired_width(ui.available_width())
                .hint_text("http:// 或 https://"),
        );
        ui.horizontal(|ui| {
            ui.label(RichText::new("请求超时").size(12.0).color(MUTED));
            ui.add(
                egui::DragValue::new(&mut self.telemetry_draft.timeout_seconds)
                    .range(5..=30)
                    .suffix(" 秒"),
            );
        });
        if !self.failed_uploads.is_empty() {
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
        }
        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("身份提取规则").heading().size(16.0).strong());
        ui.label(RichText::new("eFuse 出厂 MAC").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.telemetry_draft.efuse_pattern)
                .font(egui::TextStyle::Monospace)
                .desired_width(ui.available_width()),
        );
        ui.label(RichText::new("MAC").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.telemetry_draft.mac_pattern)
                .font(egui::TextStyle::Monospace)
                .desired_width(ui.available_width()),
        );
        ui.label(
            RichText::new("以上两项继续校验芯片出厂 MAC，自定义字段不会替代它们。")
                .size(12.0)
                .color(MUTED),
        );
        ui.add_space(10.0);
        self.settings_extraction_rules_editor(ui);
        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("自定义请求头").heading().size(16.0).strong());
        let mut remove = None;
        for (index, (name, value)) in self.settings_headers.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .desired_width(142.0)
                            .hint_text("Header-Name"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(value)
                            .password(true)
                            .desired_width((ui.available_width() - 58.0).max(110.0))
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
    }

    fn settings_extraction_rules_editor(&mut self, ui: &mut egui::Ui) {
        let validation =
            extraction_rules_validation(ui.ctx(), &self.settings_extraction_rules_json);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("自定义字段提取（JSON）")
                    .heading()
                    .size(16.0)
                    .strong(),
            );
            if text_button(ui, "复制示例").clicked() {
                ui.ctx().copy_text(EXTRACTION_RULES_EXAMPLE.into());
            }
            if ui
                .add_enabled_ui(matches!(validation, Ok(0)), |ui| {
                    text_button(ui, "填入示例")
                })
                .inner
                .on_hover_text("仅在规则为空时填入，保留已有配置。")
                .clicked()
            {
                self.settings_extraction_rules_json = EXTRACTION_RULES_EXAMPLE.into();
                self.settings_saved = false;
            }
        });
        ui.label(
            RichText::new("JSON 对象：字段名 → 正则表达式；使用第 1 个捕获组，留空或 {} 表示不提取自定义字段。")
                .size(12.0)
                .color(MUTED),
        );
        let mut layouter = |ui: &egui::Ui, text: &str, width: f32| {
            ui.fonts(|fonts| fonts.layout_job(extraction_rules_layout(text, width)))
        };
        if ui
            .add(
                egui::TextEdit::multiline(&mut self.settings_extraction_rules_json)
                    .id_salt("custom_extraction_rules_json")
                    .code_editor()
                    .desired_rows(6)
                    .desired_width(ui.available_width())
                    .char_limit(64 * 1024)
                    .hint_text("{}")
                    .layouter(&mut layouter),
            )
            .changed()
        {
            self.settings_saved = false;
        }
        match extraction_rules_validation(ui.ctx(), &self.settings_extraction_rules_json) {
            Ok(0) => {
                ui.label(RichText::new("未配置自定义字段").size(12.0).color(MUTED));
            }
            Ok(count) => {
                ui.label(
                    RichText::new(format!("JSON 与正则校验通过 · {count} 个字段"))
                        .size(12.0)
                        .color(SUCCESS),
                );
            }
            Err(error) => {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("尚不能保存：{error}"))
                            .size(12.0)
                            .color(DANGER),
                    )
                    .wrap(),
                );
            }
        }
        ui.label(
            RichText::new("JSON 中的正则反斜杠需写为 \\\\；最多 32 个字段。点击“保存设置”后生效。")
                .size(12.0)
                .color(MUTED),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_editor_example_parses_and_captures_the_requested_device_id() {
        let rules = telemetry::parse_extraction_rules_json(EXTRACTION_RULES_EXAMPLE).unwrap();
        let pattern = regex::Regex::new(&rules["unique_device_id"]).unwrap();
        let captures = pattern
            .captures("UNIQUE DEVICE ID: 00112233445566778899AABBCCDDEEFF")
            .unwrap();
        assert_eq!(&captures[1], "00112233445566778899AABBCCDDEEFF");
    }

    #[test]
    fn json_highlighting_preserves_incomplete_and_unicode_editor_input() {
        for text in [
            "",
            "{}",
            EXTRACTION_RULES_EXAMPLE,
            r#"{"device_id":"含中文的日志\\\\\"内容"}"#,
            "{\"device_id\":\"unfinished\\",
            "not JSON 日志",
        ] {
            assert_eq!(extraction_rules_layout(text, 420.0).text, text);
        }
    }

    #[test]
    fn unsaved_settings_include_telemetry_headers_and_auto_check() {
        let saved = TelemetrySettings::default();
        assert!(!settings_are_dirty(&saved, &saved, &[], true, true));
        let mut draft = saved.clone();
        draft.endpoint = "https://example.invalid/logs".into();
        assert!(settings_are_dirty(&draft, &saved, &[], true, true));
        assert!(settings_are_dirty(
            &saved,
            &saved,
            &[("X-Key".into(), "value".into())],
            true,
            true
        ));
        assert!(settings_are_dirty(&saved, &saved, &[], false, true));
    }

    #[test]
    fn saved_header_rows_allow_whitespace_and_empty_editor_rows_without_false_dirty_state() {
        let mut saved = TelemetrySettings::default();
        saved.custom_headers.insert("X-Key".into(), "value".into());
        let rows = vec![
            (" X-Key ".into(), "value".into()),
            (String::new(), String::new()),
        ];
        assert!(!settings_are_dirty(&saved, &saved, &rows, true, true));
        let duplicate = vec![
            ("X-Key".into(), "value".into()),
            ("x-key".into(), "value".into()),
        ];
        assert!(settings_are_dirty(&saved, &saved, &duplicate, true, true));
    }
}
