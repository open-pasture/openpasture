//! Animals (K-animals): CSV import, remove and swap, bulk collar linking,
//! rekeying and provisioning cards. Animal CRUD, the head count and
//! `list_animals` live in op-core; park, unpark and the collar functions in
//! op-ingest.

pub mod cards;
pub mod collars;
pub mod import;
pub mod lifecycle;
pub mod mapping;
pub mod table;

use op_core::{ActivityEvent, Ctx, id};

/// `/api/animals/import*`, `/api/animals/{id}/remove`, `/api/animals/{id}/swap`,
/// `/api/collars/bulk`, `/api/collars/{id}/rekey`, `/api/cards`.
pub fn router() -> axum::Router<Ctx> {
    axum::Router::new().merge(import::router()).merge(lifecycle::router()).merge(collars::router()).merge(cards::router())
}

/// A line in the activity log, done by a person.
async fn log(ctx: &Ctx, kind: &str, title: String, payload: serde_json::Value, targets: Vec<(String, String)>) {
    let at = op_core::time::now();
    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: kind.into(),
        source: "farmer".into(),
        occurred_at: at,
        recorded_at: at,
        title,
        body: None,
        payload,
        targets,
    };
    if let Err(err) = ctx.store().record_event(&e).await {
        tracing::warn!("activity log: {err:#}");
    }
}
