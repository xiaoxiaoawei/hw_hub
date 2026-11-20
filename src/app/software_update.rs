use super::*;

impl FlashApp {
    pub(super) fn open_update_dialog(&mut self) {
        self.resume_settings_after_update |= self.settings_open;
        self.settings_open = false;
        self.chip_picker_open = false;
        self.pending_driver_license = None;
        self.update_dialog_open = true;
        self.update_prompt_pending = false;
        self.update_focus_later = true;
    }

    pub(super) fn start_update_check(&mut self, manual: bool, ctx: &egui::Context) {
        if self.update_checking || self.update_in_progress() {
            return;
        }
        if !manual {
            let now = now_unix();
            if !updater::should_auto_check(&self.update_settings, now) {
                return;
            }
            let previous_attempt = self.update_settings.last_auto_attempt_unix;
            self.update_settings.last_auto_attempt_unix = Some(now);
            if let Err(error) = updater::save_settings(&self.update_settings) {
                self.update_settings.last_auto_attempt_unix = previous_attempt;
                self.update_settings_error = Some(format!("无法保存自动检查记录：{error:#}"));
                self.update_status = "自动检查未执行".into();
                self.update_prompt_pending = true;
                return;
            }
        }
        self.update_checking = true;
        self.update_status = "正在检查更新…".into();
        self.update_error = None;
        self.self_update_error = None;
        self.update_available = None;
        self.update_prompt_pending = false;
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        self.update_result = Some(receiver);
        if let Err(error) = thread::Builder::new()
            .name("flashtool-update-check".into())
            .spawn(move || {
                let result = updater::check_latest().map_err(|error| format!("{error:#}"));
                let _ = sender.send(result);
                ctx.request_repaint();
            })
        {
            self.update_result = None;
            self.update_checking = false;
            self.update_status = "检查更新失败".into();
            self.update_error = Some(format!("无法启动检查任务：{error}"));
            self.update_prompt_pending = true;
        }
    }

    pub(super) fn collect_update_events(&mut self, ctx: &egui::Context) {
        let result = match self.update_result.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Err("更新检查任务意外结束，请重试。".into())
            }
            _ => return,
        };
        self.update_checking = false;
        self.update_result = None;
        self.update_prompt_pending = true;
        match result {
            Ok(result) => {
                self.update_settings.last_checked_unix = Some(now_unix());
                self.update_settings_error = updater::save_settings(&self.update_settings)
                    .err()
                    .map(|error| format!("无法保存检查时间：{error:#}"));
                match result {
                    CheckResult::UpToDate { .. } => {
                        self.update_status =
                            format!("当前已是最新版本（v{}）", env!("CARGO_PKG_VERSION"));
                    }
                    CheckResult::Available(release) => {
                        self.update_status = format!("发现新版本 v{}", release.version);
                        self.update_available = Some(release);
                    }
                    CheckResult::NoPublishedRelease => {
                        self.update_status = "暂无正式发布版本".into();
                    }
                }
            }
            Err(error) => {
                self.update_status = "检查更新失败".into();
                self.log(format!("检查更新失败：{error}"));
                self.update_error = Some(error);
            }
        }
        ctx.request_repaint();
    }

    pub(super) fn update_in_progress(&self) -> bool {
        self.update_service.is_busy() || self.prepared_update.is_some() || self.installer_preparing
    }

    fn start_download_update(&mut self) {
        let Some(release) = self.update_available.clone() else {
            return;
        };
        if self.update_checking || self.update_in_progress() {
            return;
        }
        self.self_update_error = None;
        self.update_downloaded = 0;
        self.update_total = None;
        match self.update_service.start_download(release) {
            Ok(()) => self.self_update_phase = "正在下载更新".into(),
            Err(error) => self.report_install_error(format!("无法开始下载更新：{error:#}")),
        }
    }

    pub(super) fn poll_self_update(&mut self, ctx: &egui::Context) {
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
                    self.self_update_phase = "更新包已校验".into();
                }
                SelfUpdateEvent::Failed(error) => self.report_install_error(error),
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
                    // Starting the helper locks driver/flash actions and settings.
                    // All prior tasks have already settled.
                    self.allow_exit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Err(error) => self.report_install_error(error),
            }
            return;
        }
        if !self.installer_preparing
            && self.prepared_update.is_some()
            && self.update_wait_reason().is_none()
        {
            self.start_update_installer(ctx);
        }
    }

    fn report_install_error(&mut self, error: String) {
        self.prepared_update = None;
        self.installer_preparing = false;
        self.self_update_phase = "更新未完成".into();
        self.log(format!("更新未完成：{error}"));
        self.self_update_error = Some(error);
        self.update_prompt_pending = true;
    }

    fn update_wait_reason(&self) -> Option<&'static str> {
        restart_safety_reason(
            self.running() || self.startup_log.is_some(),
            self.driver_busy,
            self.record_service.pending_local_count(),
            self.pending_records.len(),
            self.failed_records.len(),
        )
        .or_else(|| {
            ((self.settings_open || self.resume_settings_after_update)
                && self.has_unsaved_settings())
            .then_some("请先返回设置保存改动，或关闭设置放弃改动。")
        })
        .or_else(|| (self.record_service.pending_count() > 0).then_some("等待日志上传结束…"))
        .or_else(|| {
            (self.close_confirm_open
                || self.chip_picker_open
                || self.pending_driver_license.is_some())
            .then_some("等待当前窗口操作结束…")
        })
    }

    fn start_update_installer(&mut self, ctx: &egui::Context) {
        let Some(prepared) = self.prepared_update.take() else {
            return;
        };
        self.installer_preparing = true;
        self.resume_settings_after_update |= self.settings_open;
        self.settings_open = false;
        self.self_update_phase = "正在准备安装与重启…".into();
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        self.installer_result = Some(receiver);
        if let Err(error) = thread::Builder::new()
            .name("flashtool-update-install".into())
            .spawn(move || {
                let result =
                    self_update::launch_installer(&prepared).map_err(|error| format!("{error:#}"));
                let _ = sender.send(result);
                ctx.request_repaint();
            })
        {
            self.installer_result = None;
            self.report_install_error(format!("无法启动安装任务：{error}"));
        }
    }

    pub(super) fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.update_dialog_open {
            return;
        }
        let busy = self.update_in_progress();
        let mut close = false;
        let mut download = false;
        let mut retry_check = false;
        let mut cancel_restart = false;
        let response = egui::Modal::new(egui::Id::new("application_updates"))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .corner_radius(8.0)
                    .inner_margin(18.0),
            )
            .show(ctx, |ui| {
                ui.set_width(450.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("软件更新").size(17.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close |= text_button(ui, "×").on_hover_text("关闭提示").clicked();
                    });
                });
                ui.add_space(9.0);
                if busy {
                    if let Some(reason) =
                        self.prepared_update.as_ref().and(self.update_wait_reason())
                    {
                        ui.label(RichText::new("更新包已校验，等待自动重启").strong());
                        ui.label(RichText::new(reason).color(MUTED));
                    } else {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(&self.self_update_phase);
                        });
                    }
                    if self.update_service.is_busy() {
                        self.update_download_progress(ui);
                    }
                } else if self.update_checking {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在检查更新…");
                    });
                } else if let Some(error) = &self.self_update_error {
                    update_error_panel(ui, ctx, &self.self_update_phase, error);
                } else if let Some(error) = &self.update_error {
                    update_error_panel(ui, ctx, &self.update_status, error);
                } else {
                    ui.label(RichText::new(&self.update_status).size(15.0).strong());
                    if let Some(release) = &self.update_available {
                        ui.label(
                            RichText::new(format!("当前版本 v{}", env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(MUTED),
                        );
                        self.update_release_notes(ui, ctx, release);
                    }
                }
                if let Some(error) = &self.update_settings_error {
                    banner(ui, error, true);
                }
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if busy {
                        close |= secondary_button(ui, "隐藏", [76.0, 32.0]).clicked();
                        if self.prepared_update.is_some() {
                            cancel_restart = text_button(ui, "取消本次安装").clicked();
                        }
                    } else if let Some(release) = &self.update_available
                        && release.download_url.is_some()
                    {
                        let action = if self.self_update_error.is_some() {
                            "重试更新"
                        } else {
                            "立即更新"
                        };
                        download = primary_button(ui, action, [102.0, 32.0]).clicked();
                        let later = secondary_button(ui, "稍后", [76.0, 32.0]);
                        if self.update_focus_later {
                            later.request_focus();
                            self.update_focus_later = false;
                        }
                        close |= later.clicked();
                    } else {
                        close |= secondary_button(ui, "确定", [76.0, 32.0]).clicked();
                        if self.update_error.is_some() && !self.update_checking {
                            retry_check = text_button(ui, "重新检查").clicked();
                        }
                    }
                });
            });
        if cancel_restart {
            self.prepared_update = None;
            close = true;
        }
        if close || response.should_close() {
            self.update_dialog_open = false;
            if self.resume_settings_after_update {
                self.settings_open = true;
                self.resume_settings_after_update = false;
            }
        }
        if retry_check {
            self.start_update_check(true, ctx);
        }
        if download {
            self.start_download_update();
        }
    }

    fn update_release_notes(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        release: &ReleaseUpdate,
    ) {
        ui.add_space(8.0);
        ui.label(RichText::new("更新内容").strong());
        if release.notes.trim().is_empty() {
            ui.label(RichText::new("暂无法获取发布说明，可查看官方发布页。").color(MUTED));
        } else {
            egui::ScrollArea::vertical()
                .id_salt("update_release_notes")
                .max_height(160.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let notes: String = release.notes.chars().take(6000).collect();
                    ui.add(egui::Label::new(RichText::new(notes).color(MUTED)).wrap());
                });
        }
        if text_button(ui, "查看官方发布页").clicked() {
            ctx.open_url(egui::OpenUrl::new_tab(&release.release_url));
        }
        if release.download_url.is_none() {
            ui.label("此版本的 Windows 发布包尚未上传。");
        } else {
            ui.label("是否更新？下载校验完成后会自动安装并重启。");
            if self.update_wait_reason().is_some() {
                ui.label(RichText::new("更新会等待当前任务和在途日志处理完成。").color(MUTED));
            }
            if !self.failed_uploads.is_empty() {
                ui.label(
                    RichText::new("上传失败的记录仍保存在本地 CSV，重启将结束本次重试队列。")
                        .color(MUTED),
                );
            }
        }
    }

    fn update_download_progress(&self, ui: &mut egui::Ui) {
        if self.update_downloaded == 0 && self.update_total.is_none() {
            return;
        }
        const MIB: f64 = 1_048_576.0;
        let received = self.update_downloaded as f64 / MIB;
        let total = self.update_total.filter(|total| *total > 0);
        let fraction = total.map_or(0.0, |total| {
            (self.update_downloaded as f32 / total as f32).clamp(0.0, 1.0)
        });
        let text = total.map_or_else(
            || format!("已下载 {received:.1} MiB"),
            |total| format!("{received:.1} / {:.1} MiB", total as f64 / MIB),
        );
        ui.add(
            egui::ProgressBar::new(fraction)
                .desired_width(ui.available_width())
                .fill(SUCCESS)
                .text(text)
                .animate(total.is_none()),
        );
    }
}
