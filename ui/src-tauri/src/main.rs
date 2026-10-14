//! OpenProfiler desktop: starts an in-process OpenProfiler daemon on a random loopback port and points the
//! webview at it by injecting `window.__OPENPROFILER__ = { url, token }` before the UI loads.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use openprofiler_daemon::{DaemonConfig, RunningDaemon};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn init_script(daemon: &RunningDaemon) -> String {
    let cfg = serde_json::json!({ "url": daemon.url(), "token": daemon.token });
    format!("window.__OPENPROFILER__ = {cfg};")
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let cfg = DaemonConfig { bind: "127.0.0.1:0".parse()?, token: None, ui_dir: None, web: false };
            let daemon = tauri::async_runtime::block_on(openprofiler_daemon::start(cfg))?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("OpenProfiler")
                .inner_size(1440.0, 900.0)
                .min_inner_size(960.0, 600.0)
                .initialization_script(init_script(&daemon))
                .build()?;
            app.manage(daemon);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run OpenProfiler");
}
