fn main() {
    // The app's own commands, so capabilities can grant them by name
    // (allow-update-status, allow-update-check).
    let app = tauri_build::AppManifest::new().commands(&["update_status", "update_check"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(app)).expect("tauri build");
}
