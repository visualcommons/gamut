//! The interactive ICC sandbox's front end: an egui/eframe window natively, a canvas in the
//! browser on wasm32. See the library docs for what each tab demonstrates.

mod app;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size(app::WINDOW),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "gamut ICC sandbox",
        options,
        Box::new(|_cc| Ok(Box::new(app::SandboxApp::new()?))),
    )
}

/// The id of the `<canvas>` in `index.html` the app draws on.
#[cfg(target_arch = "wasm32")]
const CANVAS_ID: &str = "icc-sandbox";

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast as _;

    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(CANVAS_ID))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok());
        let Some(canvas) = canvas else {
            web_sys::console::error_1(
                &format!("no <canvas id=\"{CANVAS_ID}\"> in the page").into(),
            );
            return;
        };
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|_cc| Ok(Box::new(app::SandboxApp::new()?))),
            )
            .await;
        if let Err(e) = started {
            web_sys::console::error_1(&format!("the sandbox failed to start: {e:?}").into());
        }
    });
}
