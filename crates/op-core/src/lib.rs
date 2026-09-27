//! openpasture core: the shared [`Ctx`], domain types, SQLite store and
//! migrations, the live event bus, secrets and settings, and the op-core
//! routes of `docs/API.md`.

pub mod ctx;
pub mod domain;
pub mod error;
pub mod event;
pub mod id;
pub mod keys;
pub mod patch;
mod routes;
pub mod secrets;
pub mod settings;
pub mod store;
pub mod time;
pub mod tz;
// @HUB
pub mod alert;
pub mod brief;
pub mod check;
pub mod features;
pub mod identity;
pub mod messages;
pub mod notify_config;
pub mod place;
pub mod severity;
pub mod tools;
pub mod units;
pub mod users;
// @HUB-UI
// @E-lib
// @E-srv
// @J
pub mod people;
// @A-engine
// @A-notify
// @D
pub mod features_api;
// @K-animals
pub mod animals;
// @K-files
// @I
// @B
// @G
// @P
pub mod live;
// @Q
// @C
// @F
// @S
// @A3
// @H
// @L
// @M
// @Z

pub use ctx::{BrainToken, Ctx, default_data_dir};
pub use domain::*;
pub use error::{ApiError, ApiJson, ApiResult};
pub use event::Event;
pub use identity::{Actor, Identity, Role, Via, with_identity};
pub use routes::DEFAULT_TIMER_MINUTES;
pub use severity::Severity;
pub use store::Store;

/// Routes for state, farm, paddocks, herds, animals, settings, secrets and `/api/me`.
pub fn router() -> axum::Router<Ctx> {
    let mut app = axum::Router::new();
    for part in [
        routes::router(),
        // @HUB
        identity::router(),
        // @HUB-UI
        // @E-lib
        // @E-srv
        // @J
        people::router(),
        // @A-engine
        // @A-notify
        // @D
        features_api::router(),
        // @K-animals
        // @K-files
        // @I
        // @B
        // @G
        // @P
        // @Q
        // @C
        // @F
        // @S
        // @A3
        // @H
        // @L
        // @M
        // @Z
    ] {
        app = app.merge(part);
    }
    app
}

/// This crate's MCP tools, in registration order. Idempotent: a second call
/// registers nothing.
pub fn register_tools(ctx: &Ctx) {
    ctx.tools().register_once(env!("CARGO_PKG_NAME"), tool_specs);
}

fn tool_specs() -> Vec<tools::ToolSpec> {
    vec![
        // @HUB
        // @HUB-UI
        // @E-lib
        // @E-srv
        // @J
        // @A-engine
        // @A-notify
        // @D
        features_api::tool(),
        // @K-animals
        animals::tool(),
        // @K-files
        // @I
        // @B
        // @G
        // @P
        // @Q
        // @C
        // @F
        // @S
        // @A3
        // @H
        // @L
        // @M
        // @Z
    ]
}
