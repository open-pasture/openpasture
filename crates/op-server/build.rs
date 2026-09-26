// Embed ui/dist. If it hasn't been built yet, build it with bun first.
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ui = manifest.join("../../ui");
    let dist = ui.join("dist");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !dist.join("index.html").exists() {
        build_ui(&ui);
    }
    assert!(dist.join("index.html").exists(), "ui/dist/index.html is missing after `bun run build`");
    println!("cargo:rustc-env=OP_UI_DIR={}", dist.canonicalize().unwrap().display());
}

fn build_ui(ui: &Path) {
    println!("cargo:warning=ui/dist is missing; building the UI with bun");
    for args in [&["install"][..], &["run", "build"][..]] {
        let status = Command::new("bun").args(args).current_dir(ui).status().unwrap_or_else(|e| {
            panic!(
                "ui/dist is missing and bun could not be run ({e}). Install bun (https://bun.sh), \
                 then run `bun install && bun run build` in ui/."
            )
        });
        assert!(status.success(), "`bun {}` failed in ui/ ({status})", args.join(" "));
    }
}
