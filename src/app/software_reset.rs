use super::*;
use std::{ffi::OsString, process::Command};

#[derive(Default)]
pub(super) struct SoftwareReset {
    pub(super) open: bool,
    confirmed: bool,
    launch_result: Option<mpsc::Receiver<Result<(), String>>>,
    error: Option<String>,
}

impl SoftwareReset {
    pub(super) fn in_progress(&self) -> bool {
        self.confirmed || self.launch_result.is_some()
    }
}

fn restart_command(executable: &Path, arguments: &[OsString], directory: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .current_dir(directory)
        .env_remove("FLASHTOOL_UPDATE_MANIFEST")
        .env_remove("FLASHTOOL_UPDATE_SESSION");
    command
}

impl FlashApp {
    pub(super) fn open_software_reset(&mut self) {
        if self.installer_preparing || self.software_reset.in_progress() {
            return;
        }
        self.software_reset.open = true;
        self.software_reset.error = None;
    }

    fn software_reset_block_reason(&self) -> Option<&'static str> {
        restart_safety_reason(
            self.running() || self.validation_running() || self.startup_log.is_some(),
            self.driver_busy,
            self.record_service.pending_local_count(),
            self.pending_records.len(),
            self.failed_records.len(),
        )
        .or_else(|| {
            self.installer_preparing
                .then_some("正在安装更新，请等待更新完成。")
        })
        .or_else(|| {
            self.update_in_progress()
                .then_some("软件更新尚未完成，请先完成或取消更新。")
        })
        .or_else(|| {
            self.validation
                .save_error
                .is_some()
                .then_some("验证结果尚未保存，请先重试保存报告。")
        })
        .or_else(|| {
            (self.ble.saving() || self.ble.has_unsaved())
                .then_some("蓝牙验证记录尚未保存，请等待保存完成；保存失败时请重试。")
        })
    }

    fn confirm_software_reset(&mut self) {
        if self.software_reset_block_reason().is_some() {
            return;
        }
        if self.has_unsaved_settings() && !self.save_settings_from_ui() {
            self.software_reset.error = self.telemetry_settings_error.clone();
            return;
        }
        self.software_reset.error = None;
        self.software_reset.confirmed = true;
        // Disconnect synthesizes a failed record for an interrupted scan. The
        // serial worker need not reply: only its durable record must finish.
        self.ble.result_open = false;
        if self.ble.is_connected() {
            self.disconnect_ble();
        }
    }

    pub(super) fn poll_software_reset(&mut self, ctx: &egui::Context) {
        if self.allow_exit {
            return;
        }
        let result = match self
            .software_reset
            .launch_result
            .as_ref()
            .map(mpsc::Receiver::try_recv)
        {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Some(Err("软件重启任务意外结束，当前窗口保持运行。".into()))
            }
            _ => None,
        };
        if let Some(result) = result {
            self.software_reset.launch_result = None;
            match result {
                Ok(()) => {
                    // Keep actions locked until the old process actually exits.
                    self.allow_exit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Err(error) => {
                    self.software_reset.confirmed = false;
                    self.software_reset.error = Some(error);
                }
            }
            ctx.request_repaint();
            return;
        }
        if !self.software_reset.confirmed || self.software_reset.launch_result.is_some() {
            return;
        }
        // A saved PASS can reopen this modal and pause queued BLE records.
        // Keep draining records while the reset confirmation is in control.
        self.ble.result_open = false;
        if self.software_reset_block_reason().is_some() {
            return;
        }
        if self.has_unsaved_settings() {
            self.software_reset.confirmed = false;
            self.software_reset.error = Some("设置发生了改动，请确认保存后再重启。".into());
            return;
        }
        let command = (|| -> std::io::Result<Command> {
            Ok(restart_command(
                &std::env::current_exe()?,
                &std::env::args_os().skip(1).collect::<Vec<_>>(),
                &std::env::current_dir()?,
            ))
        })();
        let mut command = match command {
            Ok(command) => command,
            Err(error) => {
                self.software_reset.confirmed = false;
                self.software_reset.error = Some(format!("无法准备重启：{error}"));
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        match thread::Builder::new()
            .name("flashtool-reset".into())
            .spawn(move || {
                let result = command
                    .spawn()
                    .map(|_child| ())
                    .map_err(|error| format!("无法重新启动软件，当前窗口保持运行：{error}"));
                let _ = sender.send(result);
                repaint.request_repaint();
            }) {
            Ok(_) => self.software_reset.launch_result = Some(receiver),
            Err(error) => {
                self.software_reset.confirmed = false;
                self.software_reset.error = Some(format!("无法启动重启任务：{error}"));
            }
        }
    }

    pub(super) fn software_reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.software_reset.open {
            return;
        }
        let launching = self.software_reset.launch_result.is_some()
            || (self.software_reset.confirmed && self.allow_exit);
        let waiting = self.software_reset.confirmed;
        let block_reason = self.software_reset_block_reason();
        let unsaved_settings = self.has_unsaved_settings();
        let uploads = self
            .record_service
            .pending_count()
            .saturating_sub(self.record_service.pending_local_count())
            + self.failed_uploads.len();
        let mut cancel = false;
        let mut confirm = false;
        let mut settings = false;
        let mut validation = false;
        let response = egui::Modal::new(egui::Id::new("software_reset"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .corner_radius(5.0)
                    .inner_margin(20.0),
            )
            .show(ctx, |ui| {
                ui.set_width((ctx.screen_rect().width() - 64.0).clamp(260.0, 510.0));
                ui.label(RichText::new("重新启动当前版本").heading().size(20.0).strong());
                ui.add(
                    egui::Label::new(
                        RichText::new("保留设置与已保存的记录，当前连接和临时日志将重新初始化。")
                            .size(13.0)
                            .color(MUTED),
                    )
                    .wrap(),
                );
                ui.add_space(9.0);
                egui::ScrollArea::vertical()
                    .id_salt("software_reset_details")
                    .max_height((ctx.screen_rect().height() - 280.0).clamp(140.0, 300.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if launching {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("正在重新启动…");
                            });
                        } else if waiting {
                            ui.label("正在等待记录安全保存，完成后自动重启。");
                        }
                        if let Some(reason) = block_reason {
                            banner(ui, reason, true);
                        }
                        if self.ble.is_connected() && !waiting {
                            ui.add(
                                egui::Label::new("重启将断开蓝牙工装；正在进行的扫描会中断，并先保存本次中断记录。")
                                    .wrap(),
                            );
                        }
                        if unsaved_settings {
                            ui.label("未保存的设置将在重启前保存；保存失败时保持当前软件运行。");
                        }
                        if uploads > 0 {
                            ui.add(
                                egui::Label::new(format!(
                                    "{uploads} 条上传未完成或失败。重启会中断上传，已保存 CSV 保留，重启后不会自动重试。"
                                ))
                                .wrap(),
                            );
                        }
                        if let Some(error) = &self.software_reset.error {
                            banner(ui, error, true);
                        }
                        if !self.failed_records.is_empty() {
                            settings = text_button(ui, "返回设置重试保存记录").clicked();
                        }
                        if self.validation.save_error.is_some() {
                            validation = text_button(ui, "返回验证结果重试保存").clicked();
                        }
                        if self.ble.has_unsaved()
                            && !self.ble.saving()
                            && secondary_button(ui, "重试保存蓝牙记录", [156.0, 32.0]).clicked()
                        {
                            self.ble.result_open = false;
                            self.retry_ble_save();
                        }
                    });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled_ui(!launching, |ui| {
                            secondary_button(ui, if waiting { "取消重启" } else { "取消" }, [90.0, 34.0])
                        })
                        .inner
                        .clicked()
                    {
                        cancel = true;
                    }
                    let label = if uploads > 0 {
                        "中断上传并重启"
                    } else if unsaved_settings {
                        "保存设置并重启"
                    } else {
                        "重新启动"
                    };
                    confirm = ui
                        .add_enabled_ui(!waiting && !launching && block_reason.is_none(), |ui| {
                            primary_button(ui, label, [170.0, 34.0])
                        })
                        .inner
                        .clicked();
                });
            });
        if !launching && (cancel || response.should_close() || settings || validation) {
            self.software_reset.confirmed = false;
            self.software_reset.open = false;
        }
        if settings {
            self.open_settings();
            settings::select_page(ctx, settings::SettingsPage::General);
        }
        if validation {
            self.workspace = Workspace::Validation;
            self.validation.result_open = true;
        }
        if confirm {
            self.confirm_software_reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_preserves_executable_arguments_and_directory_without_a_shell() {
        let executable = Path::new("FlashTool current.exe");
        let arguments = [
            OsString::from("argument with spaces"),
            OsString::from("$literal"),
        ];
        let directory = Path::new("working directory");
        let command = restart_command(executable, &arguments, directory);
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            arguments
                .iter()
                .map(|arg| arg.as_os_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(command.get_current_dir(), Some(directory));
    }

    #[test]
    fn restart_does_not_replay_the_updater_handshake() {
        let command = restart_command(Path::new("FlashTool.exe"), &[], Path::new("."));
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key)
            .collect();
        assert!(removed.contains(&std::ffi::OsStr::new("FLASHTOOL_UPDATE_MANIFEST")));
        assert!(removed.contains(&std::ffi::OsStr::new("FLASHTOOL_UPDATE_SESSION")));
    }
}
