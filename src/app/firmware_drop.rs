use std::path::PathBuf;

use eframe::egui;

pub(super) struct FirmwareFileDrop {
    pointer: Option<egui::Pos2>,
    hovering: bool,
    files: Vec<Option<PathBuf>>,
}

impl FirmwareFileDrop {
    pub(super) fn read(ctx: &egui::Context) -> Self {
        let (hovering, files): (bool, Vec<_>) = ctx.input(|input| {
            (
                !input.raw.hovered_files.is_empty(),
                input
                    .raw
                    .dropped_files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect(),
            )
        });
        if hovering {
            // Windows OLE drag-over can move without delivering CursorMoved.
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
        let pointer = if hovering || !files.is_empty() {
            file_drag_pointer(ctx)
        } else {
            None
        };
        Self {
            pointer,
            hovering,
            files,
        }
    }

    pub(super) fn is_hovering(&self, ui: &egui::Ui, rect: egui::Rect) -> bool {
        self.hovering && self.matches_target(ui, rect)
    }

    pub(super) fn take_for_target(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
    ) -> Option<Result<PathBuf, String>> {
        if self.files.is_empty() || !self.matches_target(ui, rect) {
            return None;
        }
        let files = std::mem::take(&mut self.files);
        let [Some(path)] = files.as_slice() else {
            return Some(Err("请每次拖入一个 .bin 固件文件。".into()));
        };
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
            || !path.is_file()
        {
            return Some(Err("请拖入一个有效的 .bin 固件文件。".into()));
        }
        Some(Ok(path.clone()))
    }

    fn matches_target(&self, ui: &egui::Ui, rect: egui::Rect) -> bool {
        ui.is_enabled()
            && self.pointer.is_some_and(|pos| {
                rect.intersect(ui.clip_rect()).contains(pos)
                    && ui.ctx().layer_id_at(pos) == Some(ui.layer_id())
            })
    }
}

#[cfg(windows)]
fn file_drag_pointer(ctx: &egui::Context) -> Option<egui::Pos2> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    // Winit exposes the paths but discards OLE's screen position. Its viewport
    // inner rect already describes the native client area in egui points.
    let origin = ctx.input(|input| input.viewport().inner_rect.map(|rect| rect.min))?;
    let mut cursor = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return None;
    }
    let scale = ctx.pixels_per_point();
    Some(egui::pos2(
        cursor.x as f32 / scale - origin.x,
        cursor.y as f32 / scale - origin.y,
    ))
}

#[cfg(not(windows))]
fn file_drag_pointer(ctx: &egui::Context) -> Option<egui::Pos2> {
    ctx.input(|input| input.pointer.hover_pos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_drop_is_consumed_only_by_the_visible_enabled_row_under_the_cursor() {
        let bin = tempfile::Builder::new().suffix(".BIN").tempfile().unwrap();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let first =
                    egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(80.0, 26.0));
                let second = first.translate(egui::vec2(0.0, 36.0));
                let mut drop = FirmwareFileDrop {
                    pointer: Some(second.center()),
                    hovering: false,
                    files: vec![Some(bin.path().to_owned())],
                };
                assert!(drop.take_for_target(ui, first).is_none());
                ui.add_enabled_ui(false, |ui| {
                    assert!(drop.take_for_target(ui, second).is_none());
                });
                assert_eq!(
                    drop.take_for_target(ui, second).unwrap().unwrap(),
                    bin.path()
                );
                assert!(drop.take_for_target(ui, second).is_none());
            });
        });
    }
}
