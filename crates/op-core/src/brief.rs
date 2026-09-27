//! Lines other crates add to the morning brief, per herd. The brief itself
//! (op-engine) writes the decision part and then these lines, in order.
//!
//! Registered lines: `schedule` (order 20), `attention` (order 50).

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;

use crate::Ctx;

/// `(ctx, herd_id, now)` → the lines to add (none is fine).
pub type BriefFn = Arc<dyn Fn(Ctx, String, DateTime<Utc>) -> BoxFuture<'static, anyhow::Result<Vec<String>>> + Send + Sync>;

#[derive(Clone)]
pub struct BriefLine {
    pub name: &'static str,
    /// Lower comes first.
    pub order: u32,
    pub run: BriefFn,
}

impl BriefLine {
    /// Wrap an async fn as a line body:
    /// `BriefLine::run_fn(|ctx, herd_id, now| async move { Ok(vec![…]) })`.
    pub fn run_fn<F, Fut>(f: F) -> BriefFn
    where
        F: Fn(Ctx, String, DateTime<Utc>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = anyhow::Result<Vec<String>>> + Send + 'static,
    {
        Arc::new(move |ctx, herd_id, now| f(ctx, herd_id, now).boxed())
    }
}

impl std::fmt::Debug for BriefLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BriefLine").field("name", &self.name).field("order", &self.order).finish_non_exhaustive()
    }
}

#[derive(Default)]
pub struct BriefRegistry {
    lines: RwLock<Vec<BriefLine>>,
}

impl BriefRegistry {
    /// Panics on a duplicate name.
    pub fn register(&self, l: BriefLine) {
        let mut lines = self.lines.write().unwrap_or_else(|e| e.into_inner());
        assert!(!lines.iter().any(|x| x.name == l.name), "brief line {} is registered twice", l.name);
        lines.push(l);
    }

    pub fn has(&self, name: &str) -> bool {
        self.lines.read().unwrap_or_else(|e| e.into_inner()).iter().any(|l| l.name == name)
    }

    /// By `order`, then registration order.
    pub fn list(&self) -> Vec<BriefLine> {
        let mut out = self.lines.read().unwrap_or_else(|e| e.into_inner()).clone();
        out.sort_by_key(|l| l.order);
        out
    }

    /// Every registered line's text for one herd, in order. A line that fails
    /// is left out (and logged) so the rest of the brief still goes.
    pub async fn collect(&self, ctx: &Ctx, herd_id: &str, now: DateTime<Utc>) -> Vec<String> {
        let mut out = Vec::new();
        for l in self.list() {
            match (l.run)(ctx.clone(), herd_id.to_owned(), now).await {
                Ok(lines) => out.extend(lines.into_iter().filter(|s| !s.trim().is_empty())),
                Err(e) => tracing::warn!(line = l.name, "brief line failed: {e:#}"),
            }
        }
        out
    }
}
