use super::*;
use esp32c3_flashtool::verification::MAX_BATTERY_MV;

fn validation_status(
    state: &JobState,
    checks_pass: bool,
    report_saved: bool,
) -> (&'static str, Color32) {
    match state {
        JobState::Success if checks_pass && report_saved => ("PASS", SUCCESS),
        JobState::Success if checks_pass => ("待保存", MUTED),
        JobState::Success | JobState::Failed => ("FAIL", DANGER),
        JobState::Cancelled => ("已取消", MUTED),
        _ => (state_label(state), ACCENT),
    }
}

fn verification_metric(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(MUTED));
        ui.label(
            RichText::new(value)
                .monospace()
                .size(13.0)
                .strong()
                .color(color),
        );
    });
}

impl FlashApp {
    pub(super) fn validation_panel(&mut self, ui: &mut egui::Ui) {
        let locked = self.busy();
        card(ui, |ui| {
            panel_title(ui, Icon::Shield, "验证配置");
            ui.add_space(5.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!locked, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("波特率").size(12.0).color(MUTED));
                        baud_editor(ui, &mut self.validation.baud_input, "validation_baud", 1);
                        ui.label(RichText::new("并发").size(12.0).color(MUTED));
                        ui.add_sized(
                            [76.0, 30.0],
                            egui::DragValue::new(&mut self.concurrency)
                                .range(1..=16)
                                .suffix(" 台"),
                        );
                    });
                });
                ui.label(
                    RichText::new(format!("电池 0–{MAX_BATTERY_MV} mV"))
                        .size(12.0)
                        .color(MUTED),
                );
            });
        });
        ui.add_space(12.0);
        let mut requested_logs = Vec::new();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                panel_title(ui, Icon::Usb, "在线 COM 设备");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, "刷新串口", [88.0, 31.0]).clicked() {
                        self.scan_ports();
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
            });
            if let Some(error) = &self.scan_error {
                ui.add(egui::Label::new(RichText::new(error).size(12.0).color(DANGER)).wrap());
            }
            if self.ports.iter().all(|row| !row.online) {
                ui.add_space(8.0);
                ui.label(RichText::new("尚未发现 COM 设备").size(14.0).color(MUTED));
                ui.add_space(8.0);
            } else {
                let validation_rows = &self.validation.rows;
                let device_logs = &self.device_logs;
                egui::ScrollArea::vertical()
                    .id_salt("validation_online_ports")
                    .max_height(230.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for row in self.ports.iter_mut().filter(|row| row.online) {
                            ui.push_id(("validation_port", &row.port.identity), |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_enabled_ui(!locked, |ui| {
                                        green_checkbox(
                                            ui,
                                            &mut row.selected,
                                            RichText::new(&row.port.name)
                                                .monospace()
                                                .size(15.0)
                                                .strong(),
                                        );
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if terminal_icon_button(ui)
                                                .on_hover_text("打开设备日志")
                                                .clicked()
                                            {
                                                let key = validation_rows
                                                    .iter()
                                                    .find(|result| {
                                                        result.port.name == row.port.name
                                                            && result.port.identity
                                                                == row.port.identity
                                                    })
                                                    .map(|result| result.key.clone())
                                                    .unwrap_or_else(|| {
                                                        device_logs.key_for_port(&row.port)
                                                    });
                                                requested_logs.push(key);
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
                                    .truncate(),
                                )
                                .on_hover_text(&row.port.description);
                                ui.add_space(4.0);
                            });
                        }
                    });
            }
        });
        self.log_windows.extend(requested_logs);
        if !self.validation.rows.is_empty() {
            ui.add_space(12.0);
            card(ui, |ui| {
                panel_title(ui, Icon::Grid, "本批验证结果");
                ui.add_space(6.0);
                self.validation_device_results(ui);
                self.validation_report_controls(ui);
            });
        }
    }

    pub(super) fn validation_actions(&mut self, ui: &mut egui::Ui) {
        let running = self.validation_running();
        let capturing = self
            .validation
            .rows
            .iter()
            .any(|row| !is_terminal(&row.state));
        let selected = self
            .ports
            .iter()
            .filter(|row| row.online && row.selected)
            .count();
        let saved = self.validation.report.is_some();
        let passed = self
            .validation
            .rows
            .iter()
            .filter(|row| {
                validation_status(
                    &row.state,
                    row.checks.as_ref().is_some_and(|checks| checks.passes()),
                    saved,
                )
                .0 == "PASS"
            })
            .count();
        let failed = self
            .validation
            .rows
            .iter()
            .filter(|row| {
                validation_status(
                    &row.state,
                    row.checks.as_ref().is_some_and(|checks| checks.passes()),
                    saved,
                )
                .0 == "FAIL"
            })
            .count();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(format!("已选择 {selected} 台在线设备"))
                        .size(15.0)
                        .strong(),
                );
                if !self.validation.rows.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "PASS {passed}  /  FAIL {failed}  /  共 {} 台",
                            self.validation.rows.len()
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if running && capturing {
                    "正在批量验证…"
                } else if running {
                    "正在保存报告…"
                } else {
                    "开始批量验证  >"
                };
                if ui
                    .add_enabled_ui(
                        !self.busy()
                            && !running
                            && self.validation.save_error.is_none()
                            && selected > 0
                            && parse_baud_input(&self.validation.baud_input, 1).is_ok(),
                        |ui| primary_button(ui, label, [205.0, 47.0]),
                    )
                    .inner
                    .clicked()
                {
                    self.start_validation();
                }
                if running && capturing {
                    if secondary_button(ui, "停止排队", [100.0, 47.0]).clicked() {
                        self.cancel_validation_queue();
                    }
                } else if !running
                    && !self.validation.rows.is_empty()
                    && secondary_button(ui, "查看结果", [94.0, 47.0]).clicked()
                {
                    self.validation.result_open = true;
                }
            });
        });
    }

    fn validation_device_results(&mut self, ui: &mut egui::Ui) -> bool {
        let saved = self.validation.report.is_some();
        let mut requested_logs = Vec::new();
        for row in &self.validation.rows {
            let online = self.ports.iter().any(|online| {
                online.online
                    && online.port.name == row.port.name
                    && online.port.identity == row.port.identity
            });
            let (status, color) = validation_status(
                &row.state,
                row.checks.as_ref().is_some_and(|checks| checks.passes()),
                saved,
            );
            egui::Frame::new()
                .fill(BACKGROUND)
                .stroke(egui::Stroke::new(1.0_f32, BORDER))
                .corner_radius(4.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&row.port.name)
                                .monospace()
                                .size(16.0)
                                .strong(),
                        );
                        if !online {
                            ui.label(RichText::new("已离线").size(11.0).color(MUTED));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if terminal_icon_button(ui)
                                .on_hover_text(format!("打开 {} 验证日志", row.port.name))
                                .clicked()
                            {
                                requested_logs.push(row.key.clone());
                            }
                            ui.label(
                                RichText::new(status)
                                    .monospace()
                                    .size(18.0)
                                    .strong()
                                    .color(color),
                            );
                        });
                    });
                    if let Some(checks) = &row.checks {
                        ui.horizontal_wrapped(|ui| {
                            verification_metric(
                                ui,
                                "MAX30105",
                                if checks.max30105_ok { "OK" } else { "FAIL" },
                                if checks.max30105_ok { SUCCESS } else { DANGER },
                            );
                            verification_metric(
                                ui,
                                "BMI270",
                                if checks.bmi270_ok { "OK" } else { "FAIL" },
                                if checks.bmi270_ok { SUCCESS } else { DANGER },
                            );
                            let battery = checks
                                .battery_mv
                                .map_or_else(|| "未读取".into(), |value| format!("{value} mV"));
                            verification_metric(
                                ui,
                                "电池",
                                &battery,
                                if checks
                                    .battery_mv
                                    .is_some_and(|value| value <= MAX_BATTERY_MV)
                                {
                                    SUCCESS
                                } else {
                                    DANGER
                                },
                            );
                        });
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "MAC  {}",
                                    checks.mac.as_deref().unwrap_or("未读取")
                                ))
                                .monospace()
                                .size(12.0)
                                .color(INK),
                            )
                            .wrap(),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "UNIQUE DEVICE ID  {}",
                                    checks.unique_device_id.as_deref().unwrap_or("未读取")
                                ))
                                .monospace()
                                .size(12.0)
                                .color(INK),
                            )
                            .wrap(),
                        );
                        for failure in &checks.failures {
                            ui.add(
                                egui::Label::new(RichText::new(failure).size(12.0).color(DANGER))
                                    .wrap(),
                            );
                        }
                    }
                    if row.checks.is_none() && !row.message.is_empty() {
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
                    }
                });
            ui.add_space(7.0);
        }
        let opened = !requested_logs.is_empty();
        self.log_windows.extend(requested_logs);
        opened
    }

    fn validation_report_controls(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.validation.save_error {
            banner(ui, &format!("报告保存失败：{error}"), true);
            if ui
                .add_enabled_ui(!self.busy(), |ui| {
                    secondary_button(ui, "重试保存报告", [128.0, 32.0])
                })
                .inner
                .clicked()
            {
                self.retry_validation_save();
            }
        } else if let Some(path) = self.validation.report.clone() {
            ui.horizontal_wrapped(|ui| {
                if secondary_button(ui, "打开 CSV", [94.0, 31.0]).clicked() {
                    self.open_validation_report();
                }
                if text_button(ui, "复制路径").clicked() {
                    ui.ctx().copy_text(path.display().to_string());
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new(path.display().to_string())
                        .size(11.0)
                        .color(MUTED),
                )
                .truncate(),
            )
            .on_hover_text(path.display().to_string());
        } else if self
            .validation
            .rows
            .iter()
            .all(|row| is_terminal(&row.state))
        {
            ui.label(RichText::new("报告待保存").size(12.0).color(MUTED));
        }
    }

    pub(super) fn validation_result_dialog(&mut self, ctx: &egui::Context) {
        if !self.validation.result_open {
            return;
        }
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("validation_result"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .corner_radius(5.0)
                    .inner_margin(18.0),
            )
            .show(ctx, |ui| {
                ui.set_width((ctx.screen_rect().width() - 64.0).clamp(260.0, 760.0));
                ui.label(RichText::new("本批验证结果").heading().size(20.0).strong());
                if self.validation.report.is_none() {
                    ui.label(RichText::new("报告尚未保存").size(12.0).color(DANGER));
                }
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_salt("validation_result_content")
                    .max_height((ctx.screen_rect().height() - 190.0).clamp(160.0, 510.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.validation_device_results(ui) {
                            close = true;
                        }
                        if let Some(error) = &self.validation.save_error {
                            banner(ui, &format!("报告保存失败：{error}"), true);
                        }
                        if let Some(path) = &self.validation.report {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(path.display().to_string())
                                        .size(11.0)
                                        .color(MUTED),
                                )
                                .wrap(),
                            );
                        }
                    });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if secondary_button(ui, "关闭", [78.0, 33.0]).clicked() {
                        close = true;
                    }
                    if self.validation.save_error.is_some()
                        && ui
                            .add_enabled_ui(!self.busy(), |ui| {
                                primary_button(ui, "重试保存报告", [128.0, 33.0])
                            })
                            .inner
                            .clicked()
                    {
                        self.retry_validation_save();
                    }
                    if let Some(path) = self.validation.report.clone() {
                        if primary_button(ui, "打开 CSV", [104.0, 33.0]).clicked() {
                            self.open_validation_report();
                        }
                        if text_button(ui, "复制路径").clicked() {
                            ctx.copy_text(path.display().to_string());
                        }
                    }
                });
            });
        if close || response.should_close() {
            self.validation.result_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_requires_finished_checks_and_a_saved_report() {
        assert_eq!(validation_status(&JobState::Success, true, true).0, "PASS");
        assert_eq!(
            validation_status(&JobState::Success, true, false).0,
            "待保存"
        );
        assert_eq!(validation_status(&JobState::Success, false, true).0, "FAIL");
        assert_eq!(validation_status(&JobState::Failed, true, true).0, "FAIL");
        for state in [
            JobState::Queued,
            JobState::Connecting,
            JobState::Reading,
            JobState::Cancelled,
        ] {
            assert_ne!(validation_status(&state, true, true).0, "PASS");
        }
    }
}
