//! bitwerk — digital logic circuit simulator (desktop app).
//!
//! Threads:
//! * main/UI thread: winit event loop, egui, wgpu submission (eframe);
//! * `bw-sim`: simulation engine (commands in via channel, snapshots out via
//!   `ArcSwap`, never blocks the UI);
//! * `bw-scene`: rebuilds GPU instance arrays + R-tree after edits;
//! * short-lived worker for generating the debug stress circuit;
//! * short-lived workers for Digital Works imports (`bw-dw-import`: one
//!   `.dwm` file; `bw-dw-folder`: a folder of macros into the library,
//!   reporting progress and polling a cancel flag between files).
//!
//! Command line: `bitwerk [file.bitwerk | file.dwm]` opens / imports a file
//! (a `.dwm` import shows the Import report; `--no-report` hides it);
//! `--import-dw-folder DIR` imports a folder of Digital Works macros into
//! the user library; `bitwerk FILE --export-dwm OUT` exports FILE as a
//! Digital Works circuit without opening a window (prints the report);
//! `--save-as-dwm OUT` does the same in the app and shows the Export
//! report; see `app.rs` for the other options.

#![forbid(unsafe_code)]

mod app;
mod canvas;
mod dwexport;
mod dwimport;
mod hier;
mod icons;
mod kind_props;
mod macro_props;
mod memwin;
mod menu;
mod parts;
mod scene_worker;
mod stress;
mod sweep;

/// The app icon (256 px), generated from the symbol geometry by `bw-icon`.
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/AppIcon-256.png");

fn main() -> eframe::Result {
    // `bitwerk FILE --export-dwm OUT`: headless Digital Works export.
    if let Some(out) = arg_value("--export-dwm") {
        let input = std::env::args().skip(1).find(|a| {
            std::path::Path::new(a)
                .extension()
                .is_some_and(|e| e == app::PROJECT_EXT || e.eq_ignore_ascii_case(dwexport::DWM_EXT))
                && *a != out
        });
        std::process::exit(dwexport::cli(input.as_deref(), &out));
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn")),
        )
        .init();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("bitwerk")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([640.0, 400.0]);
    // Window / Dock icon: rendered from code (`make icon`) and embedded.
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(std::sync::Arc::new(icon)),
        Err(e) => tracing::warn!("app icon unavailable: {e}"),
    }
    // Debug: `--window-pos X,Y` places the window (e.g. on a given display for
    // reproducible measurements).
    if let Some(pos) = window_pos_arg() {
        viewport = viewport.with_position(pos);
    }
    // The measurement sweep keeps its window above others: an occluded
    // window is neither painted nor presented, which would void the run.
    // `--on-top` does the same for scripted screenshots.
    if std::env::args().any(|a| a == "--sweep" || a == "--on-top") {
        viewport = viewport.with_always_on_top();
    }
    let mut wgpu_options = egui_wgpu::WgpuConfiguration::default();
    // Debug: `--frame-latency N` overrides how many frames may be queued for
    // presentation (eframe's default is 2).
    if let Some(n) = arg_value("--frame-latency").and_then(|v| v.parse().ok()) {
        wgpu_options.surface.desired_maximum_frame_latency = Some(n);
    }
    let options = eframe::NativeOptions {
        viewport,
        wgpu_options,
        renderer: eframe::Renderer::Wgpu,
        // The canvas pipelines are created for exactly this sample count.
        multisampling: app::MSAA_SAMPLES,
        depth_buffer: 0,
        ..Default::default()
    };
    eframe::run_native("bitwerk", options, Box::new(|cc| Ok(Box::new(app::BitwerkApp::new(cc)?))))
}

fn arg_value(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn window_pos_arg() -> Option<[f32; 2]> {
    let v = arg_value("--window-pos")?;
    let (x, y) = v.split_once(',')?;
    Some([x.trim().parse().ok()?, y.trim().parse().ok()?])
}
