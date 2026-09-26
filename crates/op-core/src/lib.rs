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

pub use ctx::{BrainToken, Ctx, default_data_dir};
pub use domain::*;
pub use error::{ApiError, ApiJson, ApiResult};
pub use event::Event;
pub use routes::DEFAULT_TIMER_MINUTES;
pub use store::Store;

/// Routes for state, farm, paddocks, herds, animals, settings and secrets.
pub fn router() -> axum::Router<Ctx> {
    routes::router()
}
