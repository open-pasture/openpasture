//! The openpasture desktop app. It runs the server in-process on 127.0.0.1
//! and shows one window on the server's URL, so the UI and API are
//! same-origin. When a server already runs from the same data dir (an
//! `openpasture serve`, say), the window opens that one instead of starting a
//! second on the same database.
//!
//! The server is the fence engine, so the app keeps it running: closing the
//! window hides it (the Dock icon brings it back), quitting during a move or
//! escape asks first, the Mac stays awake while there's work (`power`), and an
//! update waits for a move to finish before it restarts (`updater`). Updates
//! come from the GitHub releases.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use op_core::instance::AlreadyRunning;
use op_server::{ServeOptions, ServerHandle};
use tauri::ipc::CapabilityBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

mod power;
mod updater;

const MAIN: &str = "main";
/// The UI's `--bg`. Set on the window and webview so there is no white flash.
const BG: tauri::window::Color = tauri::window::Color(0x0B, 0x0B, 0x0A, 0xFF);
const LOG_FILE: &str = "desktop.log";
const LOG_FILTER: &str = "info,sqlx=warn,tower_http=info,tantivy=warn,rmcp=warn,tao=warn,wry=warn";
pub const REVEAL_LOGS: &str = "reveal-logs";

/// The server the window shows: this app's own, or one already running from
/// the same data dir in another process.
enum Server {
    Own(ServerHandle),
    Other { url: String },
}

struct Running(Mutex<Option<Server>>);

/// Set once quitting is settled (nothing running, or the farmer said quit).
struct Quitting(AtomicBool);

struct DataDir(PathBuf);

pub fn run() {
    let data_dir = op_core::default_data_dir();
    op_server::logging::init_file(&data_dir.join("logs"), LOG_FILE, LOG_FILTER);
    tracing::info!(version = op_server::VERSION, data_dir = %data_dir.display(), "openpasture desktop starting");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show(app)))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .manage(Running(Mutex::new(None)))
        .manage(Quitting(AtomicBool::new(false)))
        .manage(DataDir(data_dir.clone()))
        .manage(updater::Updates::default())
        .invoke_handler(tauri::generate_handler![updater::update_status, updater::update_check])
        .menu(updater::menu)
        .on_menu_event(|app, event| {
            if event.id() == updater::MENU_ID {
                updater::check_now(app);
            } else if event.id() == REVEAL_LOGS {
                reveal_logs(app);
            }
        })
        .on_window_event(|window, event| {
            // Closing the window hides it; the server keeps running the fence.
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == MAIN
                && !window.app_handle().state::<Quitting>().0.load(Ordering::SeqCst)
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            let window = WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
                .title("openpasture")
                .inner_size(1440.0, 900.0)
                .min_inner_size(1024.0, 680.0)
                .center()
                .theme(Some(tauri::Theme::Dark))
                .background_color(BG)
                .build()?;

            // Checked even when the server fails to start: an update may fix it.
            updater::start(app.handle());

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match open_server(data_dir.clone()).await {
                    Ok(server) => {
                        let (url, note) = match &server {
                            Server::Own(h) => (h.url().to_string(), None),
                            Server::Other { url } => (url.clone(), Some("another openpasture already running from this folder")),
                        };
                        tracing::info!(%url, note, "server ready");
                        if let Server::Own(h) = &server {
                            tauri::async_runtime::spawn(power::watch(h.ctx().clone()));
                        }
                        // Being on this machine grants nothing: the window opens with a one-time sign-in code.
                        let signed = match &server {
                            Server::Own(h) => op_server::session::sign_in_url(h.ctx(), &url, "desktop").await,
                            Server::Other { .. } => op_server::session::sign_in_url_in(&data_dir, &url, "desktop").await,
                        };
                        let open = signed.unwrap_or_else(|e| {
                            tracing::warn!(error = format!("{e:#}"), "no sign-in code; the window will ask for a token");
                            url.clone()
                        });
                        handle.state::<Running>().0.lock().unwrap().replace(server);
                        allow_ipc(&handle, &url);
                        navigate(&window, &open);
                    }
                    Err(e) => {
                        tracing::error!(error = format!("{e:#}"), "server failed to start");
                        failed(&window, &format!("{e:#}"));
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("building the openpasture app");

    app.run(|app, event| match event {
        // Quit from the menu or Cmd-Q (code None). Exits the app asks for itself
        // (restart for an update, a confirmed quit) have a code.
        RunEvent::ExitRequested { api, code: None, .. } if !app.state::<Quitting>().0.load(Ordering::SeqCst) => {
            api.prevent_exit();
            let app = app.clone();
            tauri::async_runtime::spawn(async move { confirm_quit(&app).await });
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show(app),
        RunEvent::Exit => {
            let server = app.state::<Running>().0.lock().unwrap().take();
            if let Some(Server::Own(server)) = server {
                tracing::info!("shutting down server");
                match tauri::async_runtime::block_on(server.shutdown()) {
                    Ok(()) => tracing::info!("server stopped"),
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "server shutdown"),
                }
            }
        }
        _ => {}
    });
}

fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// What this app's own server is doing; nothing for a server in another process.
pub async fn work(app: &AppHandle) -> op_server::work::Work {
    let ctx = match &*app.state::<Running>().0.lock().unwrap() {
        Some(Server::Own(h)) => h.ctx().clone(),
        _ => return Default::default(),
    };
    op_server::work::current(&ctx).await.unwrap_or_default()
}

/// Quit at once when nothing is moving; during a move or escape, ask.
async fn confirm_quit(app: &AppHandle) {
    let w = work(app).await;
    if w.moving() {
        let what = match (w.moves, w.escapes) {
            (0, 1) => "An animal is being brought back".to_owned(),
            (0, n) => format!("{n} animals are being brought back"),
            (1, 0) => "A move is running".to_owned(),
            _ => "Moves are running".to_owned(),
        };
        show(app);
        let dialog = app
            .dialog()
            .message(format!("{what}. If openpasture quits, the fence stops where it is until you open it again."))
            .title("Quit openpasture?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Quit".into(), "Keep running".into()));
        if !tauri::async_runtime::spawn_blocking(move || dialog.blocking_show()).await.unwrap_or(false) {
            return;
        }
    }
    app.state::<Quitting>().0.store(true, Ordering::SeqCst);
    app.exit(0);
}

/// Open the server running from this data dir, else start one.
async fn open_server(data_dir: PathBuf) -> anyhow::Result<Server> {
    if let Some(url) = running_url(&data_dir) {
        return Ok(Server::Other { url });
    }
    match start_server(&data_dir).await {
        Ok(h) => Ok(Server::Own(h)),
        // Started between the look above and the lock: open that one.
        Err(e) if e.downcast_ref::<AlreadyRunning>().is_some() => {
            for _ in 0..50 {
                if let Some(url) = running_url(&data_dir) {
                    return Ok(Server::Other { url });
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// The URL of a server another process runs from `data_dir`, once it answers.
fn running_url(data_dir: &Path) -> Option<String> {
    let s = op_core::instance::running(data_dir)?;
    let url = s.url?;
    let addr = url.strip_prefix("http://")?.parse::<std::net::SocketAddr>().ok()?;
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok().map(|_| url)
}

/// Bind 127.0.0.1 on the settings port. If another program has that port,
/// a free one, but only when nothing outside points at the port: with a
/// public URL or collar address set, a tunnel is aimed at it, so moving
/// would cut collars and phones off without a word.
async fn start_server(data_dir: &Path) -> anyhow::Result<ServerHandle> {
    let opts = ServeOptions { data_dir: Some(data_dir.to_path_buf()), bind: Some("127.0.0.1".into()), ..Default::default() };
    match op_server::serve(opts.clone()).await {
        Ok(h) => Ok(h),
        Err(e) if addr_in_use(&e) => {
            let settings = read_settings(data_dir).await;
            let port = settings.as_ref().map_or(op_core::settings::DEFAULT_PORT, |s| s.server.port);
            if settings.as_ref().is_some_and(|s| s.server.public_url.is_some() || s.server.collar_url.is_some()) {
                anyhow::bail!(
                    "Another program is using port {port}. Your public address points at that port, so openpasture won't move to another one. Quit the other program and open openpasture again."
                );
            }
            tracing::warn!(error = format!("{e:#}"), "settings port taken, using a free port");
            op_server::serve(ServeOptions { free_port: true, ..opts }).await.context("starting the server on a free port")
        }
        Err(e) => Err(e),
    }
}

/// The settings, read after a failed start (the lock is free again by then).
async fn read_settings(data_dir: &Path) -> Option<op_core::Settings> {
    let ctx = op_core::Ctx::open(data_dir).await.ok()?;
    let s = ctx.settings().await.ok();
    ctx.shutdown();
    s
}

fn addr_in_use(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse))
}

/// Let the page the server serves call the update commands, and only from
/// its own origin (not any local port).
fn allow_ipc(app: &AppHandle, url: &str) {
    let cap = CapabilityBuilder::new("server-ui")
        .remote(format!("{}/*", url.trim_end_matches('/')))
        .window(MAIN)
        .permission("allow-update-status")
        .permission("allow-update-check");
    if let Err(e) = app.add_capability(cap) {
        tracing::error!(error = %e, "granting the UI its update commands");
    }
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

/// Show the start-up error on the bundled page.
fn failed(window: &WebviewWindow, msg: &str) {
    let msg = serde_json::to_string(msg).unwrap_or_default();
    // The page may still be loading; retry until shell.js has run.
    let _ = window.eval(format!("(function f(){{ if (window.__opFailed) window.__opFailed({msg}); else setTimeout(f, 50); }})()"));
}

/// openpasture > Reveal Logs: the log file in Finder.
fn reveal_logs(app: &AppHandle) {
    let file = app.state::<DataDir>().0.join("logs").join(LOG_FILE);
    if let Err(e) = std::process::Command::new("open").arg("-R").arg(&file).spawn() {
        tracing::warn!(error = %e, "revealing the logs");
        let dialog = app.dialog().message(format!("The log is at {}.", file.display())).title("Logs");
        std::thread::spawn(move || dialog.blocking_show());
    }
}
