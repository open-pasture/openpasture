//! Simulated herds and collars: run by `simulation/collar-sim`, never on an
//! animal. The server decides it when the herd is made or the collar linked,
//! never from what a collar reports, and it never changes:
//!
//! - a herd is simulated when the simulator makes it (`simulated: true` on
//!   `POST /api/herds`), and only on a server with no real herds or collars
//!   ([`may_add_simulated_herd`]);
//! - a collar is simulated when it is linked into a simulated herd, and
//!   simulated collars go only there (real ones only on real herds);
//! - fixes, cues and episodes of a simulated collar, and the herd history of
//!   a simulated herd, carry the mark (migration 0985), so deleting collars
//!   or herds later never turns what the simulator did into farm records.
//!
//! The app marks them; reports leave them out and alerts about them reach no
//! one (`simulation/README.md`). The `sim` cap a collar reports ([`CAP`]) is
//! informational only: it never makes a collar simulated.

use std::collections::BTreeSet;

use sqlx::SqlitePool;

/// The cap a simulated collar reports in `device.caps`. Never trusted.
pub const CAP: &str = op_protocol::caps::SIM;

/// Why a simulated herd can't be made here.
pub const REAL_DATA: &str = "This server has real herds or collars. Simulated herds only go on a server of their own (collar-sim farm).";

/// Why a collar can't go on a herd of the other kind.
pub const WRONG_SIDE: &str = "Simulated collars go on simulated herds and real collars on real herds.";

/// Every simulated collar.
pub async fn collars(db: &SqlitePool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM collars WHERE simulated = 1").fetch_all(db).await?.into_iter().collect())
}

/// True when the herd was made as a simulated herd.
pub async fn herd_is_simulated(db: &SqlitePool, herd_id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>("SELECT simulated FROM herds WHERE id = ?").bind(herd_id).fetch_optional(db).await?.unwrap_or(false))
}

/// A simulated herd may be made only while the server has no real herd and
/// no real collar.
pub async fn may_add_simulated_herd(db: &SqlitePool) -> anyhow::Result<bool> {
    let real: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM herds WHERE simulated = 0) + (SELECT COUNT(*) FROM collars WHERE simulated = 0)")
        .fetch_one(db)
        .await?;
    Ok(real == 0)
}

/// Animals of simulated herds, on simulated collars, or with anything a
/// simulated collar recorded.
pub async fn animals(db: &SqlitePool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar(
        "SELECT a.id FROM animals a JOIN herds h ON h.id = a.herd_id WHERE h.simulated = 1
         UNION SELECT a.id FROM animals a JOIN collars c ON c.id = a.collar_id WHERE c.simulated = 1
         UNION SELECT c.animal_id FROM collars c WHERE c.simulated = 1 AND c.animal_id IS NOT NULL
         UNION SELECT animal_id FROM cues WHERE simulated = 1 AND animal_id IS NOT NULL
         UNION SELECT animal_id FROM episodes WHERE simulated = 1 AND animal_id IS NOT NULL",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .collect())
}

/// Simulated herds, including deleted ones (from their history).
pub async fn herds(db: &SqlitePool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM herds WHERE simulated = 1 UNION SELECT herd_id FROM herd_history WHERE simulated = 1")
        .fetch_all(db)
        .await?
        .into_iter()
        .collect())
}

/// Real herds with simulated collars: (herd, simulated collars). Only data
/// from before simulated collars were kept to simulated herds can have them.
pub async fn mixed_herds(db: &SqlitePool) -> anyhow::Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT c.herd_id, SUM(c.simulated) FROM collars c JOIN herds h ON h.id = c.herd_id WHERE h.simulated = 0
         GROUP BY c.herd_id HAVING MAX(c.simulated) = 1 ORDER BY c.herd_id",
    )
    .fetch_all(db)
    .await?)
}

/// True when the farm has collars and every one is simulated.
pub async fn all_simulated(db: &SqlitePool) -> anyhow::Result<bool> {
    let (n, sim): (i64, Option<i64>) = sqlx::query_as("SELECT COUNT(*), SUM(simulated) FROM collars").fetch_one(db).await?;
    Ok(n > 0 && sim == Some(n))
}
