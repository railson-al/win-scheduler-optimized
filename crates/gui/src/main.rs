//! ScriptRunner's window: opens it and hands everything else to [`App`](scriptrunner_gui::app::App).
//!
//! On Windows this program asks for elevation (see `build.rs`), because the daemon's pipe only admits
//! SYSTEM and Administrators.

use eframe::egui;
use scriptrunner_core::paths;
use scriptrunner_gui::app::App;
use scriptrunner_gui::backend::{Backend, BackendConfig};
use scriptrunner_gui::i18n::{Text, t};
use scriptrunner_ipc::transport::Endpoint;

fn main() -> eframe::Result {
    let endpoint = Endpoint::for_data_dir(&paths::data_dir(None));
    let options = eframe::NativeOptions {
        // wgpu is the provisional choice (D14), pending the spike on a Windows machine without a GPU.
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(t(Text::AppTitle))
            .with_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        t(Text::AppTitle),
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let backend = Backend::spawn(endpoint, BackendConfig::default(), move || ctx.request_repaint())?;
            Ok(Box::new(App::new(backend)))
        }),
    )
}
