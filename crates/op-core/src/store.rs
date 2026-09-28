//! SQLite store (WAL). Record CRUD for farm, paddocks, herds and animals,
//! key/value settings and the activity log. Feature crates run their own SQL
//! against [`Store::pool`] for the tables they own.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::domain::*;
use crate::time::{from_db, opt_from_db, to_db};

pub const DB_FILE: &str = "openpasture.db";

/// Every migration in `crates/op-core/migrations`, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Environment variable for the number of SQLite connections (default [`DEFAULT_POOL`]).
pub const POOL_ENV: &str = "OPENPASTURE_DB_POOL";
pub const DEFAULT_POOL: u32 = 16;
/// Bytes the WAL file is truncated to after a checkpoint (64 MB).
pub const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;
/// WAL pages (4 KB) past which a commit checkpoints by itself (SQLite's
/// default is 1,000). The server checkpoints in the background
/// ([`spawn_checkpointer`]), so a collar report never waits for one; this
/// is the backstop when nothing runs it (tests, tools).
pub const WAL_AUTOCHECKPOINT_PAGES: i64 = 16_384;
/// How often [`spawn_checkpointer`] checkpoints. The WAL starts over from
/// its beginning only when a writer finds every frame in it copied, so a
/// checkpoint has to end before the next commit lands: every second each
/// one has little to copy and usually does. (Every 5 s, 250 collars at the
/// fast cadence grew the WAL to 40–50 MB.)
pub const CHECKPOINT_EVERY: Duration = Duration::from_secs(1);

/// Checkpoint the WAL every `every` (`PRAGMA wal_checkpoint(PASSIVE)`, which
/// never waits for readers or writers), until the server shuts down. A
/// checkpoint copies pages into the database file and syncs it; done inside
/// a commit it made that commit (often a collar's report) wait for the disk.
pub fn spawn_checkpointer(ctx: &crate::Ctx, every: Duration) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    let started = std::time::Instant::now();
                    match sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(PASSIVE)").fetch_one(ctx.db()).await {
                        Ok((busy, log, done)) => {
                            let ms = started.elapsed().as_millis();
                            if ms > 1000 {
                                tracing::warn!(ms, log, done, busy, "wal checkpoint was slow");
                            } else {
                                tracing::debug!(ms, log, done, busy, "wal checkpoint");
                            }
                        }
                        Err(e) => tracing::warn!("wal checkpoint: {e:#}"),
                    }
                }
            }
        }
    });
}

/// Connections in the pool: `OPENPASTURE_DB_POOL` when it is a whole number
/// from 1 to 256, else [`DEFAULT_POOL`]. WAL lets readers run beside the one
/// writer, so 250 collars reporting and a page of analytics don't queue.
pub fn pool_size(env: Option<&str>) -> u32 {
    env.and_then(|v| v.trim().parse::<u32>().ok()).filter(|n| (1..=256).contains(n)).unwrap_or(DEFAULT_POOL)
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if needed) `openpasture.db` in `data_dir` and run migrations.
    pub async fn open(data_dir: &Path) -> anyhow::Result<Self> {
        Self::open_file(&data_dir.join(DB_FILE)).await
    }

    pub async fn open_file(path: &Path) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10))
            // @L: a WAL that grew while a long read held it back is cut to this once a checkpoint catches up.
            .pragma("journal_size_limit", JOURNAL_SIZE_LIMIT.to_string())
            // @L: checkpoints run from `spawn_checkpointer`; a commit only does one past 64 MB of WAL.
            .pragma("wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES.to_string());
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_size(std::env::var(POOL_ENV).ok().as_deref()))
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        MIGRATOR.run(&pool).await.context("running migrations")?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // Farm

    pub async fn get_farm(&self) -> anyhow::Result<Option<Farm>> {
        let row = sqlx::query("SELECT * FROM farm ORDER BY created_at LIMIT 1").fetch_optional(&self.pool).await?;
        row.map(|r| farm_from_row(&r)).transpose()
    }

    pub async fn insert_farm(&self, f: &Farm) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO farm (id, name, timezone, center_lon, center_lat, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&f.id)
            .bind(&f.name)
            .bind(&f.timezone)
            .bind(f.center[0])
            .bind(f.center[1])
            .bind(to_db(&f.created_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_farm(&self, f: &Farm) -> anyhow::Result<()> {
        sqlx::query("UPDATE farm SET name = ?, timezone = ?, center_lon = ?, center_lat = ? WHERE id = ?")
            .bind(&f.name)
            .bind(&f.timezone)
            .bind(f.center[0])
            .bind(f.center[1])
            .bind(&f.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // Paddocks

    pub async fn list_paddocks(&self) -> anyhow::Result<Vec<Paddock>> {
        let rows = sqlx::query("SELECT * FROM paddocks ORDER BY created_at, id").fetch_all(&self.pool).await?;
        rows.iter().map(paddock_from_row).collect()
    }

    pub async fn get_paddock(&self, id: &str) -> anyhow::Result<Option<Paddock>> {
        let row = sqlx::query("SELECT * FROM paddocks WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.map(|r| paddock_from_row(&r)).transpose()
    }

    pub async fn insert_paddock(&self, p: &Paddock) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO paddocks (id, name, geometry, area_ha, status, notes, grazed_until, created_at, props) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&p.id)
            .bind(&p.name)
            .bind(serde_json::to_string(&p.geometry)?)
            .bind(p.area_ha)
            .bind(p.status.as_db())
            .bind(&p.notes)
            .bind(p.grazed_until.as_ref().map(to_db))
            .bind(to_db(&p.created_at))
            .bind(serde_json::to_string(&p.props)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_paddock(&self, p: &Paddock) -> anyhow::Result<()> {
        sqlx::query("UPDATE paddocks SET name = ?, geometry = ?, area_ha = ?, status = ?, notes = ?, grazed_until = ?, props = ? WHERE id = ?")
            .bind(&p.name)
            .bind(serde_json::to_string(&p.geometry)?)
            .bind(p.area_ha)
            .bind(p.status.as_db())
            .bind(&p.notes)
            .bind(p.grazed_until.as_ref().map(to_db))
            .bind(serde_json::to_string(&p.props)?)
            .bind(&p.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Herds in the paddock are left without one.
    pub async fn delete_paddock(&self, id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("DELETE FROM paddocks WHERE id = ?").bind(id).execute(&self.pool).await?.rows_affected() > 0)
    }

    // Herds

    pub async fn list_herds(&self) -> anyhow::Result<Vec<Herd>> {
        let rows = sqlx::query("SELECT * FROM herds ORDER BY created_at, id").fetch_all(&self.pool).await?;
        rows.iter().map(herd_from_row).collect()
    }

    pub async fn get_herd(&self, id: &str) -> anyhow::Result<Option<Herd>> {
        let row = sqlx::query("SELECT * FROM herds WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.map(|r| herd_from_row(&r)).transpose()
    }

    pub async fn insert_herd(&self, h: &Herd) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO herds (id, name, species, count, paddock_id, autonomy, timer_minutes, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&h.id)
            .bind(&h.name)
            .bind(h.species.as_db())
            .bind(h.count as i64)
            .bind(&h.paddock_id)
            .bind(h.autonomy.as_db())
            .bind(h.timer_minutes as i64)
            .bind(to_db(&h.created_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_herd(&self, h: &Herd) -> anyhow::Result<()> {
        sqlx::query("UPDATE herds SET name = ?, species = ?, count = ?, paddock_id = ?, autonomy = ?, timer_minutes = ? WHERE id = ?")
            .bind(&h.name)
            .bind(h.species.as_db())
            .bind(h.count as i64)
            .bind(&h.paddock_id)
            .bind(h.autonomy.as_db())
            .bind(h.timer_minutes as i64)
            .bind(&h.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Deletes the herd's animals and collars too, and takes it out of the
    /// alert prefs that name it (a person limited to it alone then has none).
    pub async fn delete_herd(&self, id: &str) -> anyhow::Result<bool> {
        let gone = sqlx::query("DELETE FROM herds WHERE id = ?").bind(id).execute(&self.pool).await?.rows_affected() > 0;
        if gone {
            sqlx::query(
                "UPDATE alert_prefs SET herds = (SELECT json_group_array(value) FROM json_each(alert_prefs.herds) WHERE value != ?1)
                 WHERE herds IS NOT NULL AND EXISTS (SELECT 1 FROM json_each(alert_prefs.herds) WHERE value = ?1)",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(gone)
    }

    // Animals

    pub async fn list_animals(&self, herd_id: Option<&str>) -> anyhow::Result<Vec<Animal>> {
        let rows = match herd_id {
            Some(h) => sqlx::query("SELECT * FROM animals WHERE herd_id = ? ORDER BY created_at, id").bind(h).fetch_all(&self.pool).await?,
            None => sqlx::query("SELECT * FROM animals ORDER BY created_at, id").fetch_all(&self.pool).await?,
        };
        rows.iter().map(animal_from_row).collect()
    }

    pub async fn get_animal(&self, id: &str) -> anyhow::Result<Option<Animal>> {
        let row = sqlx::query("SELECT * FROM animals WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.map(|r| animal_from_row(&r)).transpose()
    }

    /// Also points the collar (if any) at the animal.
    pub async fn insert_animal(&self, a: &Animal) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO animals (id, tag, name, herd_id, collar_id, created_at, eid, breed, sex, born, notes, removed_at, removed_reason)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&a.id)
        .bind(&a.tag)
        .bind(&a.name)
        .bind(&a.herd_id)
        .bind(&a.collar_id)
        .bind(to_db(&crate::time::now()))
        .bind(&a.eid)
        .bind(&a.breed)
        .bind(a.sex.map(|s| s.as_db()))
        .bind(a.born.map(|d| d.format("%Y-%m-%d").to_string()))
        .bind(&a.notes)
        .bind(a.removed_at.as_ref().map(to_db))
        .bind(a.removed_reason.map(|r| r.as_db()))
        .execute(&mut *tx)
        .await?;
        link_collar(&mut tx, &a.id, a.collar_id.as_deref()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Also keeps `collars.animal_id` in step with `collar_id`.
    pub async fn update_animal(&self, a: &Animal) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE animals SET tag = ?, name = ?, herd_id = ?, collar_id = ?, eid = ?, breed = ?, sex = ?, born = ?, notes = ?, removed_at = ?, removed_reason = ?
             WHERE id = ?",
        )
        .bind(&a.tag)
        .bind(&a.name)
        .bind(&a.herd_id)
        .bind(&a.collar_id)
        .bind(&a.eid)
        .bind(&a.breed)
        .bind(a.sex.map(|s| s.as_db()))
        .bind(a.born.map(|d| d.format("%Y-%m-%d").to_string()))
        .bind(&a.notes)
        .bind(a.removed_at.as_ref().map(to_db))
        .bind(a.removed_reason.map(|r| r.as_db()))
        .bind(&a.id)
        .execute(&mut *tx)
            .await?;
        link_collar(&mut tx, &a.id, a.collar_id.as_deref()).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_animal(&self, id: &str) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE collars SET animal_id = NULL WHERE animal_id = ?").bind(id).execute(&mut *tx).await?;
        let n = sqlx::query("DELETE FROM animals WHERE id = ?").bind(id).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    pub async fn collar_exists(&self, id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("SELECT 1 FROM collars WHERE id = ?").bind(id).fetch_optional(&self.pool).await?.is_some())
    }

    /// The animal wearing a collar, if any.
    pub async fn animal_for_collar(&self, collar_id: &str) -> anyhow::Result<Option<Animal>> {
        let row = sqlx::query("SELECT * FROM animals WHERE collar_id = ?").bind(collar_id).fetch_optional(&self.pool).await?;
        row.map(|r| animal_from_row(&r)).transpose()
    }

    // Settings (key/value JSON)

    pub async fn get_setting_json(&self, key: &str) -> anyhow::Result<Option<Value>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(&self.pool).await?;
        row.map(|(v,)| serde_json::from_str(&v).map_err(Into::into)).transpose()
    }

    pub async fn set_setting_json(&self, key: &str, value: &Value) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_setting<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        self.get_setting_json(key).await?.map(|v| serde_json::from_value(v).map_err(Into::into)).transpose()
    }

    pub async fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> anyhow::Result<()> {
        self.set_setting_json(key, &serde_json::to_value(value)?).await
    }

    // Activity log

    pub async fn record_event(&self, e: &ActivityEvent) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO events (id, kind, source, occurred_at, recorded_at, title, body, payload) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&e.id)
            .bind(&e.kind)
            .bind(&e.source)
            .bind(to_db(&e.occurred_at))
            .bind(to_db(&e.recorded_at))
            .bind(&e.title)
            .bind(&e.body)
            .bind(e.payload.to_string())
            .execute(&mut *tx)
            .await?;
        for (kind, id) in &e.targets {
            sqlx::query("INSERT OR IGNORE INTO event_targets (event_id, target_type, target_id) VALUES (?, ?, ?)")
                .bind(&e.id)
                .bind(kind)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Newest first, optionally only events targeting `(target_type, target_id)`.
    pub async fn list_events(&self, target: Option<(&str, &str)>, limit: i64) -> anyhow::Result<Vec<ActivityEvent>> {
        let rows = match target {
            Some((kind, id)) => {
                sqlx::query(
                    "SELECT e.* FROM events e JOIN event_targets t ON t.event_id = e.id
                     WHERE t.target_type = ? AND t.target_id = ? ORDER BY e.occurred_at DESC LIMIT ?",
                )
                .bind(kind)
                .bind(id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => sqlx::query("SELECT * FROM events ORDER BY occurred_at DESC LIMIT ?").bind(limit).fetch_all(&self.pool).await?,
        };
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id")?;
            let targets: Vec<(String, String)> =
                sqlx::query_as("SELECT target_type, target_id FROM event_targets WHERE event_id = ? ORDER BY target_type, target_id")
                    .bind(&id)
                    .fetch_all(&self.pool)
                    .await?;
            out.push(ActivityEvent {
                id,
                kind: r.try_get("kind")?,
                source: r.try_get("source")?,
                occurred_at: from_db(&r.try_get::<String, _>("occurred_at")?)?,
                recorded_at: from_db(&r.try_get::<String, _>("recorded_at")?)?,
                title: r.try_get("title")?,
                body: r.try_get("body")?,
                payload: serde_json::from_str(&r.try_get::<String, _>("payload")?)?,
                targets,
            });
        }
        Ok(out)
    }
}

async fn link_collar(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, animal_id: &str, collar_id: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE collars SET animal_id = NULL WHERE animal_id = ? AND id IS NOT ?").bind(animal_id).bind(collar_id).execute(&mut **tx).await?;
    if let Some(c) = collar_id {
        sqlx::query("UPDATE collars SET animal_id = ? WHERE id = ?").bind(animal_id).bind(c).execute(&mut **tx).await?;
    }
    Ok(())
}

// Row mapping. Public so feature crates reading these tables get the same shapes.

pub fn farm_from_row(r: &SqliteRow) -> anyhow::Result<Farm> {
    Ok(Farm {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        timezone: r.try_get("timezone")?,
        center: [r.try_get("center_lon")?, r.try_get("center_lat")?],
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
    })
}

pub fn paddock_from_row(r: &SqliteRow) -> anyhow::Result<Paddock> {
    Ok(Paddock {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        geometry: serde_json::from_str(&r.try_get::<String, _>("geometry")?)?,
        area_ha: r.try_get("area_ha")?,
        status: PaddockStatus::from_db(&r.try_get::<String, _>("status")?)?,
        notes: r.try_get("notes")?,
        grazed_until: opt_from_db(r.try_get("grazed_until")?)?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        props: match serde_json::from_str::<Value>(&r.try_get::<String, _>("props")?)? {
            Value::Object(m) => m,
            _ => Default::default(),
        },
    })
}

pub fn herd_from_row(r: &SqliteRow) -> anyhow::Result<Herd> {
    Ok(Herd {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        species: Species::from_db(&r.try_get::<String, _>("species")?)?,
        count: r.try_get::<i64, _>("count")?.max(0) as u32,
        paddock_id: r.try_get("paddock_id")?,
        autonomy: Autonomy::from_db(&r.try_get::<String, _>("autonomy")?)?,
        timer_minutes: r.try_get::<i64, _>("timer_minutes")?.max(0) as u32,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
    })
}

pub fn animal_from_row(r: &SqliteRow) -> anyhow::Result<Animal> {
    let born: Option<String> = r.try_get("born")?;
    Ok(Animal {
        id: r.try_get("id")?,
        tag: r.try_get("tag")?,
        name: r.try_get("name")?,
        herd_id: r.try_get("herd_id")?,
        collar_id: r.try_get("collar_id")?,
        eid: r.try_get("eid")?,
        breed: r.try_get("breed")?,
        sex: r.try_get::<Option<String>, _>("sex")?.map(|s| Sex::from_db(&s)).transpose()?,
        born: born.map(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d")).transpose()?,
        notes: r.try_get("notes")?,
        removed_at: opt_from_db(r.try_get("removed_at")?)?,
        removed_reason: r.try_get::<Option<String>, _>("removed_reason")?.map(|s| RemovedReason::from_db(&s)).transpose()?,
    })
}

/// A `collars` row. `last_fix` is Fix JSON, `caps` a JSON array. The
/// `limits` column stays off the struct (op-ingest reads it).
pub fn collar_from_row(r: &SqliteRow) -> anyhow::Result<Collar> {
    let last_fix: Option<String> = r.try_get("last_fix")?;
    let caps: Option<String> = r.try_get("caps")?;
    Ok(Collar {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        herd_id: r.try_get("herd_id")?,
        animal_id: r.try_get("animal_id")?,
        last_seen: opt_from_db(r.try_get("last_seen")?)?,
        battery: r.try_get("battery")?,
        boundary_version: r.try_get::<Option<i64>, _>("boundary_version")?.map(|v| v as u32),
        state: FenceState::parse(&r.try_get::<String, _>("state")?),
        last_fix: last_fix.map(|s| serde_json::from_str(&s)).transpose()?,
        fw: r.try_get("fw")?,
        caps: caps.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default(),
        outside_since: opt_from_db(r.try_get("outside_since")?)?,
        parked_at: opt_from_db(r.try_get("parked_at")?)?,
        parked_reason: r.try_get::<Option<String>, _>("parked_reason")?.map(|s| ParkReason::from_db(&s)).transpose()?,
    })
}

/// A `boundaries` row.
pub fn boundary_from_row(r: &SqliteRow) -> anyhow::Result<Boundary> {
    Ok(Boundary {
        id: r.try_get("id")?,
        herd_id: r.try_get("herd_id")?,
        version: r.try_get::<i64, _>("version")? as u32,
        geometry: serde_json::from_str(&r.try_get::<String, _>("geometry")?)?,
        warn_m: r.try_get("warn_m")?,
        hysteresis_m: r.try_get("hysteresis_m")?,
        effective_at: opt_from_db(r.try_get("effective_at")?)?,
        decision_id: r.try_get("decision_id")?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        collar_id: r.try_get("collar_id")?,
    })
}

/// A `decisions` row.
pub fn decision_from_row(r: &SqliteRow) -> anyhow::Result<Decision> {
    let json = |col: &str| -> anyhow::Result<Option<Value>> {
        let s: Option<String> = r.try_get(col)?;
        Ok(s.map(|s| serde_json::from_str(&s)).transpose()?)
    };
    let geometry: Option<String> = r.try_get("geometry")?;
    Ok(Decision {
        id: r.try_get("id")?,
        herd_id: r.try_get("herd_id")?,
        source: DecisionSource::from_db(&r.try_get::<String, _>("source")?)?,
        brain: r.try_get::<Option<String>, _>("brain")?.map(|s| BrainId::from_db(&s)).transpose()?,
        model: r.try_get("model")?,
        status: DecisionStatus::from_db(&r.try_get::<String, _>("status")?)?,
        action: r.try_get::<Option<String>, _>("action")?.map(|s| DecisionAction::from_db(&s)).transpose()?,
        to_paddock_id: r.try_get("to_paddock_id")?,
        geometry: geometry.map(|s| serde_json::from_str(&s)).transpose()?,
        reasoning: r.try_get("reasoning")?,
        confidence: r.try_get("confidence")?,
        need: r.try_get("need")?,
        inputs: json("inputs")?.unwrap_or(Value::Null),
        apply_at: opt_from_db(r.try_get("apply_at")?)?,
        boundary_id: r.try_get("boundary_id")?,
        error: r.try_get("error")?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        responded_at: opt_from_db(r.try_get("responded_at")?)?,
        outcome: json("outcome")?,
    })
}

/// A write transaction that takes SQLite's write lock up front
/// (`BEGIN IMMEDIATE`). Use it whenever a transaction reads before it writes
/// (e.g. `MAX(version) + 1`): a deferred transaction that upgrades from read
/// to write fails at once with "database is locked" when another writer got
/// in first, without waiting on the busy timeout. Waiting for the lock uses the
/// pool's busy timeout; a still-busy database is retried a few times.
///
/// It starts in a task of its own (@L). sqlx's custom `BEGIN` awaits once
/// more after the `BEGIN` has run, and a caller dropped there (a collar that
/// gave up on its report while the lock was busy) got no `Transaction` to
/// roll back: its pooled connection kept the write lock for good, every
/// writer stalled on it, and every begin on it failed with "non-zero
/// transaction depth". A task runs to its end, and a `Transaction` no one
/// waits for any more is dropped, which rolls it back.
pub async fn begin_immediate(pool: &SqlitePool) -> anyhow::Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        let mut attempt = 0;
        loop {
            match pool.begin_with("BEGIN IMMEDIATE").await {
                Ok(tx) => return Ok(tx),
                Err(e) if attempt < 3 && is_busy(&e) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(50 * attempt)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    })
    .await?
}

/// SQLITE_BUSY / SQLITE_LOCKED (and their extended codes).
pub fn is_busy(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).and_then(|c| c.parse::<i32>().ok()).is_some_and(|c| matches!(c & 0xff, 5 | 6))
}
