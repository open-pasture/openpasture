//! Simulated collars: collars run by `simulation/collar-sim`, never on an
//! animal. A collar is simulated once it is linked as one or reports the
//! `sim` cap ([`CAP`]), and stays so. The app marks them; reports leave
//! them out and alerts about them reach no one (`simulation/README.md`).

use std::collections::BTreeSet;

use sqlx::SqlitePool;

/// The cap a simulated collar reports in `device.caps`.
pub const CAP: &str = op_protocol::caps::SIM;

/// True when `caps` names the simulator.
pub fn in_caps(caps: &[String]) -> bool {
    caps.iter().any(|c| c == CAP)
}

/// Every simulated collar.
pub async fn collars(db: &SqlitePool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM collars WHERE simulated = 1").fetch_all(db).await?.into_iter().collect())
}

/// Animals wearing a simulated collar.
pub async fn animals(db: &SqlitePool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar(
        "SELECT a.id FROM animals a JOIN collars c ON c.id = a.collar_id WHERE c.simulated = 1
         UNION SELECT c.animal_id FROM collars c WHERE c.simulated = 1 AND c.animal_id IS NOT NULL",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .collect())
}

/// Herds whose collars are all simulated (at least one). A herd with no
/// collars, or any real one, isn't.
pub async fn herds(db: &SqlitePool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar("SELECT herd_id FROM collars GROUP BY herd_id HAVING MIN(simulated) = 1").fetch_all(db).await?.into_iter().collect())
}

/// Herds with both real and simulated collars: (herd, simulated collars).
pub async fn mixed_herds(db: &SqlitePool) -> anyhow::Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as("SELECT herd_id, SUM(simulated) FROM collars GROUP BY herd_id HAVING MIN(simulated) = 0 AND MAX(simulated) = 1 ORDER BY herd_id")
        .fetch_all(db)
        .await?)
}

/// True when the farm has collars and every one is simulated.
pub async fn all_simulated(db: &SqlitePool) -> anyhow::Result<bool> {
    let (n, sim): (i64, Option<i64>) = sqlx::query_as("SELECT COUNT(*), SUM(simulated) FROM collars").fetch_one(db).await?;
    Ok(n > 0 && sim == Some(n))
}
