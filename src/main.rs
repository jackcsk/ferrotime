mod app;
mod data;
mod expr;
mod format;
mod grid;
mod search;
mod session;
mod tle_session;
mod value;
mod view;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{} v{}", app::APP_NAME, env!("CARGO_PKG_VERSION")))
            .with_app_id("ferrotime")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(app::APP_NAME, options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, files)))))
}
