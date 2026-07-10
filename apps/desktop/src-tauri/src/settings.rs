use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const SETTINGS_LABEL: &str = "settings";

pub fn show_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }

    match WebviewWindowBuilder::new(app, SETTINGS_LABEL, WebviewUrl::App("settings.html".into()))
        .title("VZT Flow Settings")
        .inner_size(480.0, 760.0)
        .resizable(true)
        .minimizable(false)
        .visible(true)
        .center()
        .build()
    {
        // For an `Accessory` app a freshly built window can open *behind* the
        // frontmost app, so focus it explicitly on the create path too (the
        // reuse path above already does). Without this the first-run Settings
        // window that onboarding opens can be invisible under whatever the
        // installer/browser left frontmost.
        Ok(window) => {
            let _ = window.set_focus();
        }
        Err(e) => eprintln!("[vzt-flow] failed to open Settings window: {e}"),
    }
}
