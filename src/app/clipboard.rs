use super::*;

#[derive(Clone)]
struct CopyFeedback {
    button: egui::Id,
    expires_at: f64,
}

pub(super) fn copy_with_feedback(
    ui: &mut egui::Ui,
    response: egui::Response,
    text: impl FnOnce() -> String,
) -> egui::Response {
    let ctx = ui.ctx();
    let now = ctx.input(|input| input.time);
    let state_id = egui::Id::new("clipboard_copied_feedback");
    if response.clicked() {
        ctx.copy_text(text());
        ctx.data_mut(|data| {
            data.insert_temp(
                state_id,
                CopyFeedback {
                    button: response.id,
                    expires_at: now + 1.2,
                },
            );
        });
        ctx.request_repaint();
    }
    if let Some(feedback) = ctx.data_mut(|data| data.get_temp::<CopyFeedback>(state_id)) {
        if now >= feedback.expires_at {
            ctx.data_mut(|data| data.remove::<CopyFeedback>(state_id));
        } else if feedback.button == response.id && response.rect.intersects(ui.clip_rect()) {
            egui::Area::new(response.id.with("copied_feedback"))
                .order(egui::Order::Tooltip)
                .interactable(false)
                .movable(false)
                .pivot(egui::Align2::CENTER_BOTTOM)
                .fixed_pos(response.rect.center_top() - egui::vec2(0.0, 5.0))
                .constrain_to(ctx.screen_rect().shrink(6.0))
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(SURFACE_ALT)
                        .stroke(egui::Stroke::new(1.0_f32, SUCCESS))
                        .corner_radius(4.0)
                        .inner_margin(egui::vec2(9.0, 5.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new("已复制").size(12.0).color(INK));
                        });
                });
            ctx.request_repaint_after(Duration::from_secs_f64(feedback.expires_at - now));
        }
    }
    response
}
