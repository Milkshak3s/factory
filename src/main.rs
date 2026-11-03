#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod app;
mod config;
mod forge;
mod git;
mod pipeline;
mod proc;
mod update;
mod version;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("Factory", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
