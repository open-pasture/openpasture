//! The openpasture desktop app. It runs the server in-process on 127.0.0.1
//! (settings port, 7878 by default, or a free port if that one is taken) and
//! shows one window on the server's URL, so the UI and API are same-origin.
//! Quitting shuts the server down gracefully.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use op_server::{ServeOptions, ServerHandle};
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const MAIN: &str = "main";
/// The UI's `--bg`. Set on the window and webview so there is no white flash.
const BG: tauri::window::Color = tauri::window::Color(0x0B, 0x0C, 0x09, 0xFF);

/// The running server, taken on exit.
struct Server(Mutex<Option<ServerHandle>>);

pub fn run() {
    let data_dir = op_core::default_data_dir();
    init_logging(&data_dir);
    tracing::info!(version = op_server::VERSION, data_dir = %data_dir.display(), "openpasture desktop starting");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window(MAIN) {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .manage(Server(Mutex::new(None)))
        .setup(move |app| {
            let window = WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
                .title("openpasture")
                .inner_size(1440.0, 900.0)
                .min_inner_size(1024.0, 680.0)
                .center()
                .theme(Some(tauri::Theme::Dark))
                .background_color(BG)
                .build()?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match start_server(data_dir).await {
                    Ok(server) => {
                        let url = server.url().to_string();
                        tracing::info!(%url, "server ready");
                        handle.state::<Server>().0.lock().unwrap().replace(server);
                        navigate(&window, &url);
                    }
                    Err(e) => {
                        tracing::error!(error = format!("{e:#}"), "server failed to start");
                        let msg = serde_json::to_string(&format!("{e:#}")).unwrap_or_default();
                        // The page may still be loading; retry until shell.js has run.
                        let _ = window.eval(format!("(function f(){{ if (window.__opFailed) window.__opFailed({msg}); else setTimeout(f, 50); }})()"));
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("building the openpasture app");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            let server = app.state::<Server>().0.lock().unwrap().take();
            if let Some(server) = server {
                tracing::info!("shutting down server");
                match tauri::async_runtime::block_on(server.shutdown()) {
                    Ok(()) => tracing::info!("server stopped"),
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "server shutdown"),
                }
            }
        }
    });
}

/// Bind 127.0.0.1 on the settings port; if that port is taken, a free one.
async fn start_server(data_dir: std::path::PathBuf) -> anyhow::Result<ServerHandle> {
    let opts = ServeOptions { data_dir: Some(data_dir), bind: Some("127.0.0.1".into()), ..Default::default() };
    match op_server::serve(opts.clone()).await {
        Ok(h) => Ok(h),
        Err(e) if addr_in_use(&e) => {
            tracing::warn!(error = format!("{e:#}"), "settings port taken, using a free port");
            op_server::serve(ServeOptions { free_port: true, ..opts }).await.context("starting the server on a free port")
        }
        Err(e) => Err(e),
    }
}

fn addr_in_use(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse))
}

fn navigate(window: &WebviewWindow, url: &str) {
    match url.parse() {
        Ok(u) => {
            if let Err(e) = window.navigate(u) {
                tracing::error!(error = %e, "navigating to the server");
            }
        }
        Err(e) => tracing::error!(error = %e, %url, "bad server url"),
    }
}

/// `<data dir>/logs/desktop.log`, appended. Falls back to stderr.
fn init_logging(data_dir: &Path) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,sqlx=warn,tower_http=info,tantivy=warn,rmcp=warn,tao=warn,wry=warn".into());
    let dir = data_dir.join("logs");
    let file = std::fs::create_dir_all(&dir).and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(dir.join("desktop.log")));
    match file {
        Ok(f) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(Mutex::new(f)).try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}
