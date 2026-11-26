use super::*;
use esp32c3_flashtool::production::ProductionBatch;

#[derive(Default)]
pub(super) struct ProductionInputs {
    work_order: String,
    board_week: String,
    board_date: String,
}

impl ProductionInputs {
    pub(super) fn is_blank(&self) -> bool {
        [&self.work_order, &self.board_week, &self.board_date]
            .iter()
            .all(|value| value.trim().is_empty())
    }

    pub(super) fn parse(&self) -> anyhow::Result<ProductionBatch> {
        ProductionBatch::parse(&self.work_order, &self.board_week, &self.board_date)
    }
}

#[derive(Clone)]
pub(super) struct RecordSetup {
    pub(super) settings: TelemetrySettings,
    pub(super) production: Option<ProductionBatch>,
}

impl FlashApp {
    pub(super) fn production_input_panel(&mut self, ui: &mut egui::Ui, editable: bool) {
        card(ui, |ui| {
            panel_title(ui, Icon::File, "生产信息（均为选填）");
            ui.add_enabled_ui(editable, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (label, value, width, hint) in [
                        (
                            "生产批次/工单号",
                            &mut self.production_inputs.work_order,
                            210.0,
                            "选填：批次号或工单号",
                        ),
                        (
                            "主板年周",
                            &mut self.production_inputs.board_week,
                            100.0,
                            "选填 YYWW",
                        ),
                        (
                            "主板生产日期",
                            &mut self.production_inputs.board_date,
                            146.0,
                            "选填 YYYY-MM-DD",
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
    ) -> Option<RecordSetup> {
        let inputs = &self.production_inputs;
        let production = if !inputs.is_blank() {
            match inputs.parse() {
                Ok(production) => Some(production),
                Err(error) => {
                    self.error = Some(format!("已填写的生产信息无效：{error}"));
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
    let root = record_root(settings)?;
    Ok(match production {
        Some(production) => production.record_directory(&root, operation),
        None => root.join(match operation {
            RecordOperation::Flash => "烧录",
            RecordOperation::ReadOnly => "验证",
        }),
    })
}

pub(super) fn record_root(settings: &TelemetrySettings) -> anyhow::Result<PathBuf> {
    settings
        .csv_directory
        .clone()
        .map_or_else(telemetry::default_records_directory, Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_metadata_routes_each_operation_without_changing_the_selected_root() {
        let root = tempfile::tempdir().unwrap();
        let settings = TelemetrySettings {
            csv_directory: Some(root.path().to_owned()),
            ..TelemetrySettings::default()
        };
        assert_eq!(
            effective_record_directory(&settings, RecordOperation::Flash, None).unwrap(),
            root.path().join("烧录")
        );
        assert_eq!(
            effective_record_directory(&settings, RecordOperation::ReadOnly, None).unwrap(),
            root.path().join("验证")
        );
        let order_only = ProductionBatch::parse("only-order", "", "").unwrap();
        assert_eq!(
            effective_record_directory(&settings, RecordOperation::Flash, Some(&order_only))
                .unwrap(),
            root.path().join("烧录")
        );
        let week_only = ProductionBatch::parse("", "2642", "").unwrap();
        assert_eq!(
            effective_record_directory(&settings, RecordOperation::ReadOnly, Some(&week_only))
                .unwrap(),
            root.path().join("2642").join("验证")
        );
    }

    #[test]
    fn partial_inputs_validate_only_fields_that_are_present() {
        let inputs = ProductionInputs {
            work_order: "QA".into(),
            ..ProductionInputs::default()
        };
        assert_eq!(inputs.parse().unwrap().board_date, None);
        assert!(
            ProductionInputs {
                board_week: "2600".into(),
                ..inputs
            }
            .parse()
            .is_err()
        );
    }
}
