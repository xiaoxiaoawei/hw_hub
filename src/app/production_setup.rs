use super::*;
use esp32c3_flashtool::production::ProductionBatch;

#[derive(Default)]
pub(super) struct ProductionInputs {
    work_order: String,
    board_week: String,
    board_date: String,
}

#[derive(Clone)]
pub(super) struct RecordSetup {
    pub(super) settings: TelemetrySettings,
    pub(super) production: Option<ProductionBatch>,
}

impl FlashApp {
    pub(super) fn production_input_panel(&mut self, ui: &mut egui::Ui, editable: bool) {
        card(ui, |ui| {
            panel_title(ui, Icon::File, "生产信息");
            ui.add_enabled_ui(editable, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (label, value, width, hint) in [
                        (
                            "生产批次/工单号",
                            &mut self.production_inputs.work_order,
                            210.0,
                            "填写批次号或工单号",
                        ),
                        (
                            "主板年周",
                            &mut self.production_inputs.board_week,
                            100.0,
                            "YYWW",
                        ),
                        (
                            "主板生产日期",
                            &mut self.production_inputs.board_date,
                            146.0,
                            "YYYY-MM-DD",
                        ),
                    ] {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(label).size(12.0).color(MUTED));
                            ui.add(
                                egui::TextEdit::singleline(value)
                                    .desired_width(width)
                                    .hint_text(hint),
                            );
                        });
                    }
                });
            });
        });
    }

    pub(super) fn prepare_record_setup(
        &mut self,
        operation: RecordOperation,
        required: bool,
    ) -> Option<RecordSetup> {
        let inputs = &self.production_inputs;
        let blank = [&inputs.work_order, &inputs.board_week, &inputs.board_date]
            .iter()
            .all(|value| value.trim().is_empty());
        let production = if required || !blank {
            match ProductionBatch::parse(&inputs.work_order, &inputs.board_week, &inputs.board_date)
            {
                Ok(production) => Some(production),
                Err(error) => {
                    self.error = Some(format!("请填写生产信息。{error}"));
                    return None;
                }
            }
        } else {
            None
        };
        match effective_record_directory(&self.telemetry_settings, operation, production.as_ref()) {
            Ok(directory) => {
                let mut settings = self.telemetry_settings.clone();
                settings.csv_directory = Some(directory);
                Some(RecordSetup {
                    settings,
                    production,
                })
            }
            Err(error) => {
                self.error = Some(format!("无法确定记录目录：{error:#}"));
                None
            }
        }
    }
}

pub(super) fn effective_record_directory(
    settings: &TelemetrySettings,
    operation: RecordOperation,
    production: Option<&ProductionBatch>,
) -> anyhow::Result<PathBuf> {
    let root = settings
        .csv_directory
        .clone()
        .map_or_else(telemetry::default_records_directory, Ok)?;
    Ok(match production {
        Some(production) => production.record_directory(&root, operation),
        None => root.join("验证"),
    })
}
