//! Skills from the repo's `skills/`, embedded at build time. A copy in
//! `<data_dir>/skills/<name>/SKILL.md` wins, so a farmer can tune the
//! instructions without rebuilding.

use op_core::Ctx;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../skills"]
struct Skills;

pub fn load(ctx: &Ctx, name: &str) -> Option<String> {
    let rel = format!("{name}/SKILL.md");
    if let Ok(s) = std::fs::read_to_string(ctx.data_dir().join("skills").join(&rel)) {
        return Some(s);
    }
    Skills::get(&rel).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

/// Instructions for the brain's daily decision.
pub fn daily_grazing_decision(ctx: &Ctx) -> String {
    load(ctx, "daily-grazing-decision").unwrap_or_default()
}
