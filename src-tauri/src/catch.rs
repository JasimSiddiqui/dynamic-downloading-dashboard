use crate::app::Clip;
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

pub const W: f64 = 360.0;
pub const H: f64 = 76.0;

pub fn create(app: &AppHandle) {
    let _ = WebviewWindowBuilder::new(app, "catch", WebviewUrl::App("catch.html".into()))
        .title("DDD")
        .inner_size(W, H)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build();
}

pub fn show(app: &AppHandle, clip: &Clip) {
    let Some(w) = app.get_webview_window("catch") else { return };
    if let Ok(Some(m)) = w.primary_monitor() {
        let scale = m.scale_factor();
        let wa = m.work_area();
        let _ = w.set_size(LogicalSize::new(W, H));
        let x = wa.position.x + wa.size.width as i32 - ((W + 16.0) * scale) as i32;
        let y = wa.position.y + wa.size.height as i32 - ((H + 12.0) * scale) as i32;
        let _ = w.set_position(PhysicalPosition::new(x, y));
    }
    let _ = app.emit_to("catch", "catch", clip);
    let _ = w.show();
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("catch") {
        let _ = w.hide();
    }
}
