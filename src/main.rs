#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;

fn main() -> eframe::Result<()> {
    if let Some(code) = esp32c3_flashtool::cli::run_if_requested() {
        std::process::exit(code);
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("FlashTool")
            .with_decorations(true)
            .with_resizable(true)
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/app.png"))
                    .expect("embedded application icon"),
            )
            .with_inner_size([1120.0, 850.0])
            .with_min_inner_size([960.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FlashTool",
        options,
        Box::new(|cc| Ok(Box::new(app::FlashApp::new(cc)))),
    )
}
