//! Updates from the GitHub releases. Every release publishes a signed
//! `latest.json` next to the app; the app checks it a little after launch,
//! every six hours after that, and from openpasture > Check for Updates…
//! Installing swaps the .app in place, then the app restarts through the
//! normal exit, so the server shuts down cleanly first.
//!
//! The UI's sidebar has an update button too. It calls [`update_status`] and
//! [`update_check`] over IPC; `capabilities/server-ui.json` lets the pages the
//! embedded server serves call those two commands and nothing else.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem, MenuItemKind, PredefinedMenuItem};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Update, UpdaterExt};

pub const MENU_ID: &str = "check-for-updates";
const FIRST_CHECK: Duration = Duration::from_secs(10);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// One check or install at a time. A version put off with Later isn't offered
/// again by the background check; Check for Updates… still offers it.
#[derive(Default)]
pub struct Updates {
    busy: AtomicBool,
    later: Mutex<Option<String>>,
    /// The newer version the last check found, until one finds none.
    available: Mutex<Option<String>>,
}

/// What the sidebar's update button shows.
#[derive(serde::Serialize)]
pub struct Status {
    version: String,
    available: Option<String>,
    busy: bool,
}

#[tauri::command]
pub fn update_status(app: AppHandle) -> Status {
    let state = app.state::<Updates>();
    Status { version: app.package_info().version.to_string(), available: state.available.lock().unwrap().clone(), busy: state.busy.load(Ordering::SeqCst) }
}

/// The sidebar's update button: the same as Check for Updates….
#[tauri::command]
pub fn update_check(app: AppHandle) {
    check_now(&app);
}

/// The default menu with Check for Updates… under About in the app menu.
pub fn menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::default(app)?;
    if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.first() {
        let check = MenuItem::with_id(app, MENU_ID, "Check for Updates…", true, None::<&str>)?;
        app_menu.insert_items(&[&check, &PredefinedMenuItem::separator(app)?], 1)?;
    }
    Ok(menu)
}

/// Background checks. Debug builds don't check: their version isn't a release.
pub fn start(app: &AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            check(&app, false).await;
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Check for Updates…: always answers, even when there's nothing new.
pub fn check_now(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move { check(&app, true).await });
}

async fn check(app: &AppHandle, asked: bool) {
    let state = app.state::<Updates>();
    if state.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let found = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    match found {
        Ok(Some(update)) => {
            state.available.lock().unwrap().replace(update.version.clone());
            let put_off = state.later.lock().unwrap().as_deref() == Some(update.version.as_str());
            if asked || !put_off {
                offer(app, update).await;
            }
        }
        Ok(None) => {
            tracing::info!("no update");
            state.available.lock().unwrap().take();
            if asked {
                let version = app.package_info().version.to_string();
                say(app, "You're up to date", format!("openpasture {version} is the newest version."), MessageDialogKind::Info).await;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            if asked {
                say(app, "Couldn't check for updates", e.to_string(), MessageDialogKind::Warning).await;
            }
        }
    }
    state.busy.store(false, Ordering::SeqCst);
}

async fn offer(app: &AppHandle, update: Update) {
    tracing::info!(version = %update.version, "update available");
    let dialog = app
        .dialog()
        .message(format!("openpasture {} is ready. You have {}.\n\nopenpasture restarts once it's installed.", update.version, update.current_version))
        .title("Update available")
        .buttons(MessageDialogButtons::OkCancelCustom("Update".into(), "Later".into()));
    if !tauri::async_runtime::spawn_blocking(move || dialog.blocking_show()).await.unwrap_or(false) {
        app.state::<Updates>().later.lock().unwrap().replace(update.version.clone());
        return;
    }

    // Progress shows in the window title.
    let window = app.get_webview_window(crate::MAIN);
    let title = |t: &str| {
        if let Some(w) = &window {
            let _ = w.set_title(t);
        }
    };
    let (mut got, mut shown) = (0u64, None);
    let result = update
        .download_and_install(
            |chunk, total| {
                got += chunk as u64;
                let pct = total.filter(|t| *t > 0).map(|t| got * 100 / t);
                if pct != shown {
                    shown = pct;
                    title(&match pct {
                        Some(p) => format!("openpasture — updating {p}%"),
                        None => "openpasture — updating".into(),
                    });
                }
            },
            || title("openpasture — restarting"),
        )
        .await;
    match result {
        Ok(()) => {
            tracing::info!(version = %update.version, "update installed, restarting");
            app.request_restart();
        }
        Err(e) => {
            tracing::error!(error = %e, "update failed");
            title("openpasture");
            say(app, "Update failed", format!("openpasture {} couldn't be installed: {e}", update.version), MessageDialogKind::Error).await;
        }
    }
}

async fn say(app: &AppHandle, title: &str, message: String, kind: MessageDialogKind) {
    let dialog = app.dialog().message(message).title(title).kind(kind);
    let _ = tauri::async_runtime::spawn_blocking(move || dialog.blocking_show()).await;
}
