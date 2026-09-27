//! Paddock areas stored before op-geo measured each ring on its own
//! (GEO-AREA): a clockwise outer ring stored about +51 billion ha, a hole wound
//! like its outer ring about -51 billion ha. SQL can't compute a geodesic
//! area, so this runs in Rust when the data dir opens: every stored area is
//! measured again from its own geometry, and the rows that disagree are
//! rewritten, in `paddocks` and in the history reports read
//! (`paddock_geometry_history`). A row that already agrees is left alone, so
//! it is idempotent and costs one read of the paddocks once they are right.

use sqlx::{Row, SqlitePool};

use crate::domain::Polygon;

/// Areas are stored to 3 decimals (a square metre is 0.0001 ha); anything off by
/// more than half of that last decimal disagrees.
const TOLERANCE_HA: f64 = 0.0005;

/// What one pass rewrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Repaired {
    pub paddocks: usize,
    pub history: usize,
}

fn stored_area(geometry: &str) -> Option<f64> {
    let p: Polygon = serde_json::from_str(geometry).ok()?;
    Some((p.area_ha() * 1000.0).round() / 1000.0)
}

/// Measure every stored paddock area again and rewrite the ones that disagree.
pub async fn repair_paddock_areas(pool: &SqlitePool) -> anyhow::Result<Repaired> {
    let mut out = Repaired::default();
    for (table, count) in [("paddocks", &mut out.paddocks), ("paddock_geometry_history", &mut out.history)] {
        let rows = sqlx::query(&format!("SELECT rowid, geometry, area_ha FROM {table}")).fetch_all(pool).await?;
        let wrong: Vec<(i64, f64)> = rows
            .iter()
            .filter_map(|r| {
                let (rowid, geometry, stored): (i64, String, f64) = (r.get(0), r.get(1), r.get(2));
                let fresh = stored_area(&geometry)?;
                ((stored - fresh).abs() > TOLERANCE_HA || !stored.is_finite()).then_some((rowid, fresh))
            })
            .collect();
        if wrong.is_empty() {
            continue;
        }
        let mut tx = pool.begin().await?;
        for (rowid, fresh) in &wrong {
            sqlx::query(&format!("UPDATE {table} SET area_ha = ? WHERE rowid = ?")).bind(fresh).bind(rowid).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        *count = wrong.len();
    }
    if out != Repaired::default() {
        tracing::info!(paddocks = out.paddocks, history = out.history, "paddock areas measured again from their geometry");
    }
    Ok(out)
}
