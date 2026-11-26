use super::*;

fn ble_identity_value(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.push_id(("ble_identity", label), |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(label).size(12.0).color(MUTED));
            ui.add(egui::Label::new(RichText::new(value).monospace().size(13.0)).wrap());
            if !value.is_empty() {
                let response = text_button(ui, "复制");
                clipboard::copy_with_feedback(ui, response, || value.to_owned());
            }
        });
    });
}

impl FlashApp {
    pub(super) fn ble_panel(&mut self, ui: &mut egui::Ui) {
        let connected = self.ble.is_connected();
        let editable = !connected && !self.busy();
        let online_ports: Vec<_> = self
            .ports
            .iter()
            .filter(|row| row.online)
            .map(|row| row.port.name.clone())
            .collect();
        card(ui, |ui| {
            panel_title(ui, Icon::Usb, "ESP32-S3 BLE 验证工装");
            ui.add_space(7.0);
            ui.add_enabled_ui(editable, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("工装串口").size(12.0).color(MUTED));
                        egui::ComboBox::from_id_salt("ble_station_port")
                            .width(270.0)
                            .selected_text(if self.ble.port_name.is_empty() {
                                "请选择 ESP32-S3 工装串口"
                            } else {
                                &self.ble.port_name
                            })
                            .show_ui(ui, |ui| {
                                for name in &online_ports {
                                    ui.selectable_value(
                                        &mut self.ble.port_name,
                                        name.clone(),
                                        name,
                                    );
                                }
                                if online_ports.is_empty() {
                                    ui.label("未发现在线串口");
                                }
                            });
                    });
                    ui.vertical(|ui| {
                        ui.label(RichText::new("串口波特率").size(12.0).color(MUTED));
                        ui.label(RichText::new("115200 bps").monospace().size(13.0));
                    });
                    if secondary_button(ui, "刷新串口", [88.0, 31.0]).clicked() {
                        self.scan_ports();
                    }
                });
            });
            ui.add_space(9.0);
            if let Some(error) = &self.scan_error {
                banner(ui, error, true);
            }
            ui.add(
                egui::Label::new(
                    RichText::new("连接工装并等待就绪，短按并松开 GPIO7 开始扫描。")
                        .size(12.0)
                        .color(MUTED),
                )
                .wrap(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new("根据原始主广播与扫描响应校验名称、服务 UUID 和载荷；MAC 来自实际广播，UID 仅在取得时显示。")
                        .size(12.0)
                        .color(MUTED),
                )
                .wrap(),
            );
        });

        if let Some(result) = &self.ble.latest {
            // Freeze the displayed result before actions can mutate the workspace.
            let observed_mac = result.observed_mac.clone();
            let uid = result.uid.clone();
            let passed = result.passed;
            let detail = result.detail.clone();
            let time = result.time.clone();
            let report = result.report.clone();
            let save_error = result.save_error.clone();
            let saved = report.is_some() && save_error.is_none();
            ui.add_space(12.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    panel_title(ui, Icon::Shield, "最近一次验证");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (label, color) = match (passed, saved) {
                            (true, true) => ("PASS", SUCCESS),
                            (true, false) => ("待保存", MUTED),
                            (false, _) => ("FAIL", DANGER),
                        };
                        ui.label(
                            RichText::new(label)
                                .monospace()
                                .size(19.0)
                                .strong()
                                .color(color),
                        );
                    });
                });
                if !time.is_empty() {
                    ui.label(RichText::new(&time).size(11.0).color(MUTED));
                }
                ui.add_space(5.0);
                if let Some(observed_mac) = observed_mac.as_deref().filter(|mac| !mac.is_empty()) {
                    ble_identity_value(ui, "观察到的 BLE MAC", observed_mac);
                } else if !passed {
                    ui.label(
                        RichText::new("本次没有记录到广播地址")
                            .size(11.0)
                            .color(MUTED),
                    );
                }
                if let Some(uid) = uid.as_deref().filter(|uid| !uid.is_empty()) {
                    ble_identity_value(ui, "设备 UID", uid);
                }
                if !detail.is_empty() {
                    ui.add_space(5.0);
                    if passed {
                        ui.add(egui::Label::new(RichText::new(&detail).size(12.0)).wrap());
                    } else {
                        banner(ui, &detail, true);
                    }
                }
                if let Some(error) = &save_error {
                    ui.add_space(7.0);
                    banner(ui, &format!("报告保存失败。{error}"), true);
                    if ui
                        .add_enabled_ui(!self.ble.capturing(), |ui| {
                            secondary_button(ui, "重试保存报告", [128.0, 32.0])
                        })
                        .inner
                        .clicked()
                    {
                        self.retry_ble_save();
                    }
                }
                if let Some(path) = &report {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if secondary_button(ui, "打开 CSV", [94.0, 31.0]).clicked() {
                            self.open_ble_report();
                        }
                        let response = text_button(ui, "复制路径");
                        clipboard::copy_with_feedback(ui, response, || path.display().to_string());
                    });
                    ui.add(
                        egui::Label::new(
                            RichText::new(path.display().to_string())
                                .size(11.0)
                                .color(MUTED),
                        )
                        .wrap(),
                    );
                } else if save_error.is_none() {
                    ui.label(RichText::new("报告待保存").size(12.0).color(MUTED));
                }
            });
        }

        ui.add_space(12.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                panel_title(ui, Icon::Grid, "工装实时串口日志");
                ui.label(
                    RichText::new(format!("{} 行", self.ble.logs.len()))
                        .size(11.0)
                        .color(MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let response = ui
                        .add_enabled_ui(!self.ble.logs.is_empty(), |ui| {
                            secondary_button(ui, "复制日志", [88.0, 29.0])
                        })
                        .inner;
                    clipboard::copy_with_feedback(ui, response, || {
                        self.ble.logs.iter().cloned().collect::<Vec<_>>().join("\n")
                    });
                });
            });
            ui.add_space(5.0);
            egui::ScrollArea::both()
                .id_salt("ble_station_live_serial_log")
                .auto_shrink([false, false])
                .min_scrolled_height(120.0)
                .max_height(260.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.ble.logs.is_empty() {
                        ui.label(
                            RichText::new("连接工装后，串口输出将显示在这里。")
                                .size(12.0)
                                .color(MUTED),
                        );
                    }
                    for line in &self.ble.logs {
                        ui.add(
                            egui::Label::new(RichText::new(line).monospace().size(12.0))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
        });
    }

    pub(super) fn ble_actions(&mut self, ui: &mut egui::Ui) {
        let connected = self.ble.is_connected();
        let online = self
            .ports
            .iter()
            .any(|row| row.online && row.port.name == self.ble.port_name);
        let ready = !self.busy() && !self.ble.has_unsaved() && online;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(if self.ble.capturing() {
                        "正在验证广播"
                    } else if connected {
                        "工装会话进行中"
                    } else {
                        "选择 ESP32-S3 工装串口"
                    })
                    .size(15.0)
                    .strong(),
                );
                if !self.ble.status.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(&self.ble.status).size(11.0).color(MUTED))
                            .wrap(),
                    );
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if connected {
                    if secondary_button(ui, "断开工装", [150.0, 47.0]).clicked() {
                        self.disconnect_ble();
                    }
                } else if ui
                    .add_enabled_ui(ready, |ui| primary_button(ui, "连接工装", [205.0, 47.0]))
                    .inner
                    .clicked()
                {
                    self.connect_ble(ui.ctx());
                }
            });
        });
    }

    pub(super) fn ble_result_dialog(&mut self, ctx: &egui::Context) {
        if !self.ble.result_open {
            return;
        }
        let Some(result) = &self.ble.latest else {
            self.ble.result_open = false;
            return;
        };
        let timed_out = result.timed_out;
        let saved = result.report.is_some() && result.save_error.is_none();
        if !timed_out && (!result.passed || !saved) {
            self.ble.result_open = false;
            return;
        }
        let observed_mac = result.observed_mac.clone();
        let uid = result.uid.clone();
        let time = result.time.clone();
        let detail = result.detail.clone();
        let report = result.report.clone();
        let save_error = result.save_error.clone();
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("ble_verification_result"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .corner_radius(5.0)
                    .inner_margin(20.0),
            )
            .show(ctx, |ui| {
                ui.set_width((ctx.screen_rect().width() - 64.0).clamp(240.0, 620.0));
                ui.label(RichText::new("BLE 验证结果").heading().size(20.0).strong());
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(if timed_out { "FAIL！" } else { "PASS！" })
                            .size(56.0)
                            .strong()
                            .color(if timed_out { DANGER } else { SUCCESS }),
                    );
                    ui.label(
                        RichText::new(if !timed_out {
                            "本次验证通过，报告已保存"
                        } else if saved {
                            "扫描超时。报告已保存"
                        } else if save_error.is_some() {
                            "扫描超时。报告未保存"
                        } else if self.ble.saving() {
                            "扫描超时。报告正在保存"
                        } else {
                            "扫描超时。报告待保存"
                        })
                        .size(13.0)
                        .color(MUTED),
                    );
                });
                ui.add_space(14.0);
                if let Some(mac) = observed_mac.as_deref().filter(|mac| !mac.is_empty()) {
                    ble_identity_value(ui, "BLE MAC", mac);
                }
                if let Some(uid) = uid.as_deref().filter(|uid| !uid.is_empty()) {
                    ble_identity_value(ui, "设备 UID", uid);
                }
                if !time.is_empty() {
                    ui.label(RichText::new(&time).size(11.0).color(MUTED));
                }
                if timed_out && (!detail.is_empty() || save_error.is_some()) {
                    ui.add_space(7.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("ble_timeout_details", &time))
                        .max_height((ctx.screen_rect().height() - 430.0).clamp(90.0, 180.0))
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            if !detail.is_empty() {
                                ui.add(egui::Label::new(RichText::new(&detail).size(12.0)).wrap());
                            }
                            if let Some(error) = &save_error {
                                banner(ui, &format!("报告保存失败。{error}"), true);
                            }
                        });
                }
                ui.add_space(12.0);
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if secondary_button(ui, "关闭", [78.0, 33.0]).clicked() {
                        close = true;
                    }
                    if save_error.is_some()
                        && ui
                            .add_enabled_ui(!self.ble.capturing() && !self.ble.saving(), |ui| {
                                secondary_button(ui, "重试保存报告", [128.0, 33.0])
                            })
                            .inner
                            .clicked()
                    {
                        self.retry_ble_save();
                    }
                    if saved {
                        if primary_button(ui, "打开 CSV", [104.0, 33.0]).clicked() {
                            self.open_ble_report();
                        }
                        if let Some(path) = &report {
                            let response = text_button(ui, "复制路径");
                            clipboard::copy_with_feedback(ui, response, || {
                                path.display().to_string()
                            });
                        }
                    }
                });
            });
        if close || response.should_close() {
            self.ble.result_open = false;
        }
    }
}
