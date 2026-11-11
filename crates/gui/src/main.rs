//! Spike (task 3.1): opens a window with the renderer named on the command line and reports which
//! graphics backend and device it really got. Its only job is to show, on a Windows machine with no
//! GPU (an RDP session, a server VM), which renderer can open a window at all.
//!
//! ```text
//! scriptrunner-gui            # wgpu (eframe's default)
//! scriptrunner-gui wgpu
//! scriptrunner-gui glow       # OpenGL
//! ```

use eframe::egui;
use eframe::glow::{self, HasContext};

struct Spike {
    requested: &'static str,
    report: String,
    frames: u64,
}

impl Spike {
    fn new(requested: &'static str, cc: &eframe::CreationContext<'_>) -> Self {
        let report = describe_backend(cc);
        // Printed as well, so the answer survives even if the window is unreadable.
        println!("requested renderer: {requested}");
        println!("{report}");
        Self {
            requested,
            report,
            frames: 0,
        }
    }
}

/// What the renderer actually got, as text.
fn describe_backend(cc: &eframe::CreationContext<'_>) -> String {
    if let Some(state) = &cc.wgpu_render_state {
        let info = state.adapter.get_info();
        return format!(
            "wgpu\nadapter: {}\nbackend: {:?}\ndevice type: {:?}\ndriver: {} {}",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        );
    }
    if let Some(gl) = &cc.gl {
        // SAFETY: plain queries on the context eframe just created and made current.
        let (version, renderer) = unsafe {
            (
                gl.get_parameter_string(glow::VERSION),
                gl.get_parameter_string(glow::RENDERER),
            )
        };
        return format!("glow (OpenGL)\nversion: {version}\nrenderer: {renderer}");
    }
    "no renderer state available".to_owned()
}

impl eframe::App for Spike {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames += 1;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("ScriptRunner: graphics spike");
            ui.label(format!("requested: {}", self.requested));
            ui.separator();
            ui.monospace(&self.report);
            ui.separator();
            ui.label(format!("frames drawn: {}", self.frames));
            if ui.button("Copy report").clicked() {
                ui.ctx().copy_text(self.report.clone());
            }
        });
    }
}

fn main() -> eframe::Result {
    let (name, renderer): (&'static str, eframe::Renderer) = match std::env::args().nth(1).as_deref() {
        None | Some("wgpu") => ("wgpu", eframe::Renderer::Wgpu),
        Some("glow") => ("glow", eframe::Renderer::Glow),
        Some(other) => {
            eprintln!("unknown renderer {other:?}; use `wgpu` or `glow`");
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title("ScriptRunner (graphics spike)")
            .with_inner_size([560.0, 340.0]),
        ..Default::default()
    };
    eframe::run_native(
        "scriptrunner-gui-spike",
        options,
        Box::new(move |cc| Ok(Box::new(Spike::new(name, cc)))),
    )
}
