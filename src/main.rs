#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod app;
mod config;
mod forge;
mod git;
// The encoders are only used by build.rs, which compiles this file too.
#[allow(dead_code)]
mod icon;
mod makefile;
mod pipeline;
mod proc;
mod update;
mod version;
mod wsl;

/// Large enough for the taskbar on a high-DPI display.
const WINDOW_ICON_SIZE: u32 = 128;

fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData {
        rgba: icon::render(WINDOW_ICON_SIZE),
        width: WINDOW_ICON_SIZE,
        height: WINDOW_ICON_SIZE,
    }
}

fn main() -> eframe::Result {
    // First, because changing the environment is only sound before other threads exist.
    proc::adopt_login_path();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_icon(window_icon())
            // Lets Linux desktops match the window to packaging/factory.desktop.
            .with_app_id("factory"),
        ..Default::default()
    };
    eframe::run_native("Factory", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_dimensions_match_its_pixels() {
        let icon = window_icon();
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }
}
