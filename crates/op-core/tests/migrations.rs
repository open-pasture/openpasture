//! Released migrations never change, new ones sort after them, and a database
//! from the last release upgrades (with a snapshot first).

use std::borrow::Cow;
use std::path::Path;

use op_core::Ctx;
use op_core::store::{DB_FILE, MIGRATOR, SNAPSHOT_DIR, SNAPSHOTS_KEPT};
use sqlx::sqlite::SqliteConnectOptions;

const SHIPPED: &str = include_str!("../migrations/SHIPPED");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `(version, sha384, file)` per line of `migrations/SHIPPED`.
fn shipped() -> Vec<(i64, String, String)> {
    SHIPPED
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut parts = l.split_whitespace();
            let v = parts.next().unwrap().parse().unwrap();
            (v, parts.next().unwrap().to_owned(), parts.next().unwrap().to_owned())
        })
        .collect()
}

#[test]
fn shipped_migrations_never_change_and_new_ones_sort_after_them() {
    let shipped = shipped();
    assert!(!shipped.is_empty());
    assert!(shipped.windows(2).all(|w| w[0].0 < w[1].0), "SHIPPED is in version order");
    for (v, sum, file) in &shipped {
        let m = MIGRATOR.iter().find(|m| m.version == *v).unwrap_or_else(|| panic!("released migration {file} is missing"));
        assert_eq!(format!("{:04}_{}.sql", m.version, m.description.replace(' ', "_")), *file, "released migration {v} renamed");
        assert_eq!(hex(&m.checksum), *sum, "released migration {file} was edited: put the change in a new migration");
    }
    let last = shipped.last().unwrap().0;
    let new: Vec<_> = MIGRATOR.iter().filter(|m| !shipped.iter().any(|s| s.0 == m.version)).collect();
    for m in &new {
        assert!(m.version > last, "migration {} is numbered below the last released one ({last}): installed databases would run it out of order", m.version);
    }
    // For the release checklist: the lines to append to SHIPPED.
    for m in new {
        println!("{:04} {} {:04}_{}.sql", m.version, hex(&m.checksum), m.version, m.description.replace(' ', "_"));
    }
}

/// A database as the last release left it: the released migrations only.
async fn released_db(dir: &Path) -> sqlx::SqlitePool {
    let opts = SqliteConnectOptions::new().filename(dir.join(DB_FILE)).create_if_missing(true).foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    let versions: Vec<i64> = shipped().iter().map(|s| s.0).collect();
    let released = sqlx::migrate::Migrator {
        migrations: Cow::Owned(MIGRATOR.migrations.iter().filter(|m| versions.contains(&m.version)).cloned().collect()),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    released.run(&pool).await.unwrap();
    pool
}

fn snapshots(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(SNAPSHOT_DIR))
        .map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.ends_with(".db")).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn a_released_database_upgrades_after_a_snapshot_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let pool = released_db(dir.path()).await;
    let token = "0123456789abcdef0123456789abcdef";
    for sql in [
        "INSERT INTO farm VALUES ('farm_1', 'Old farm', 'America/Chicago', -93.62, 42.03, '2026-01-01T00:00:00.000Z')".to_string(),
        "INSERT INTO herds (id, name, species, count, created_at) VALUES ('herd_1', 'Cows', 'cattle', 40, '2026-01-01T00:00:00.000Z')".into(),
        format!(
            r#"INSERT INTO settings (key, value) VALUES ('settings', '{{"decision_time":"05:30","server":{{"bind":"0.0.0.0","port":7878,"app_token":"{token}"}},"units":"imperial"}}')"#
        ),
        // A report stored twice: two fixes and a warn and an outside cue on the
        // same fix, all twice; one more fix only once.
        "INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m) VALUES
            ('col_1', 'herd_1', '2026-01-02T00:00:00.000Z', 1767312000000, -93.62, 42.03, 3.0),
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, -93.62, 42.03, 3.0),
            ('col_1', 'herd_1', '2026-01-02T00:00:00.000Z', 1767312000000, -93.62, 42.03, 3.0),
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, -93.62, 42.03, 3.0),
            ('col_2', 'herd_1', '2026-01-02T00:00:00.000Z', 1767312000000, -93.61, 42.03, 3.0),
            ('col_2', 'herd_1', '2026-01-02T00:00:00.000Z', 1767312000000, -93.60, 42.03, 3.0)"
            .into(),
        "INSERT INTO cues (collar_id, herd_id, at, t, level, margin_m, kind) VALUES
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, 1, 2.0, 'warn'),
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, 3, -1.0, 'outside'),
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, 1, 2.0, 'warn'),
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, 3, -1.0, 'outside'),
            ('col_1', 'herd_1', '2026-01-02T00:01:00.000Z', 1767312060000, 2, 1.0, 'warn'),
            ('col_1', 'herd_1', '2026-01-02T00:02:00.000Z', 1767312120000, 1, 2.0, NULL),
            ('col_1', 'herd_1', '2026-01-02T00:02:00.000Z', 1767312120000, 1, 2.0, NULL),
            ('col_1', 'herd_1', '2026-01-02T00:02:00.000Z', 1767312120000, 3, -1.0, NULL)"
            .into(),
    ] {
        sqlx::query(&sql).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let ctx = Ctx::open(dir.path()).await.unwrap();
    let snaps = snapshots(dir.path());
    assert_eq!(snaps.len(), 1, "one snapshot before migrating");
    assert!(snaps[0].starts_with("pre-migrate-") && snaps[0].ends_with("-v0880.db"), "{}", snaps[0]);

    // The snapshot is the released database as it was.
    let snap = sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(dir.path().join(SNAPSHOT_DIR).join(&snaps[0]))).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixes").fetch_one(&snap).await.unwrap();
    assert_eq!(n, 6);
    let max: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations").fetch_one(&snap).await.unwrap();
    assert_eq!(max, 880);
    snap.close().await;

    // Owner-only, whatever the umask: the data dir, the database and the snapshot.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.path()), 0o700);
        assert_eq!(mode(&dir.path().join(DB_FILE)), 0o600);
        assert_eq!(mode(&dir.path().join(SNAPSHOT_DIR)), 0o700);
        assert_eq!(mode(&dir.path().join(SNAPSHOT_DIR).join(&snaps[0])), 0o600);
    }

    // Upgraded: data read back through the current code, identical copies
    // gone, a different fix or cue at the same instant kept.
    assert_eq!(ctx.store().get_farm().await.unwrap().unwrap().name, "Old farm");
    assert_eq!(ctx.store().list_herds().await.unwrap().len(), 1);
    let s = ctx.settings().await.unwrap();
    assert_eq!((s.server.app_token.as_str(), s.decision_time.as_str()), (token, "05:30"));
    let fixes: Vec<(String, i64)> = sqlx::query_as("SELECT collar_id, t FROM fixes ORDER BY collar_id, t").fetch_all(ctx.db()).await.unwrap();
    assert_eq!(fixes, [("col_1".into(), 1767312000000), ("col_1".into(), 1767312060000), ("col_2".into(), 1767312000000), ("col_2".into(), 1767312000000)]);
    let lons: Vec<f64> = sqlx::query_scalar("SELECT lon FROM fixes WHERE collar_id = 'col_2' ORDER BY id").fetch_all(ctx.db()).await.unwrap();
    assert_eq!(lons, [-93.61, -93.60], "the conflicting fix survives the upgrade");
    // Firmware 0.1's cues have no kind: a warn and an outside (by margin) at one instant are two.
    let cues: Vec<(i64, Option<String>, i64)> = sqlx::query_as("SELECT t, kind, level FROM cues ORDER BY t, kind, level").fetch_all(ctx.db()).await.unwrap();
    assert_eq!(
        cues,
        [
            (1767312060000, Some("outside".into()), 3),
            (1767312060000, Some("warn".into()), 1),
            (1767312060000, Some("warn".into()), 2),
            (1767312120000, None, 1),
            (1767312120000, None, 3)
        ]
    );

    // A resent cue is now stored once (fixes: by ingest, op-ingest tests/replay.rs).
    let legacy =
        "INSERT INTO cues (collar_id, at, t, level, margin_m) VALUES ('col_1', '2026-01-02T00:02:00.000Z', 1767312120000, 1, 2.0) ON CONFLICT DO NOTHING";
    assert_eq!(sqlx::query(legacy).execute(ctx.db()).await.unwrap().rows_affected(), 0);
    // A different cue at an instant already held is a conflict, stored.
    let other =
        "INSERT INTO cues (collar_id, at, t, level, margin_m) VALUES ('col_1', '2026-01-02T00:02:00.000Z', 1767312120000, 2, 0.5) ON CONFLICT DO NOTHING";
    assert_eq!(sqlx::query(other).execute(ctx.db()).await.unwrap().rows_affected(), 1);
    let conflicts: Option<i64> = sqlx::query_scalar("SELECT conflicts FROM health LIMIT 1").fetch_optional(ctx.db()).await.unwrap().flatten();
    assert_eq!(conflicts, None, "health.conflicts exists");
    // A record the collar numbered (v2) is stored once by its number, whatever its time.
    let seq = |t: i64| format!("INSERT INTO collar_records (collar_id, seq, type, t) VALUES ('col_3', 7, 'status', {t}) ON CONFLICT DO NOTHING");
    assert_eq!(sqlx::query(&seq(1)).execute(ctx.db()).await.unwrap().rows_affected(), 1);
    assert_eq!(sqlx::query(&seq(2)).execute(ctx.db()).await.unwrap().rows_affected(), 0);
    drop(ctx);

    // Opening it again migrates nothing, so takes no snapshot.
    let ctx = Ctx::open(dir.path()).await.unwrap();
    assert_eq!(snapshots(dir.path()).len(), 1);
    drop(ctx);
}

#[tokio::test]
async fn a_new_database_takes_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    assert!(snapshots(dir.path()).is_empty());
    let status = op_core::store::migration_status(ctx.db()).await.unwrap();
    assert!(status.pending.is_empty() && status.unknown.is_empty());
}

#[tokio::test]
async fn only_the_newest_snapshots_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let to = dir.path().join(SNAPSHOT_DIR);
    let mut made = Vec::new();
    for v in 0..SNAPSHOTS_KEPT + 2 {
        made.push(op_core::store::snapshot(ctx.db(), &to, v as i64).await.unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let kept = snapshots(dir.path());
    assert_eq!(kept.len(), SNAPSHOTS_KEPT);
    let newest: Vec<String> = made[made.len() - SNAPSHOTS_KEPT..].iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(kept, newest);
}

#[tokio::test]
async fn a_database_from_a_newer_release_is_refused_with_a_way_back() {
    let dir = tempfile::tempdir().unwrap();
    drop(Ctx::open(dir.path()).await.unwrap());
    let pool = sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(dir.path().join(DB_FILE))).await.unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (99999, 'from the future', CURRENT_TIMESTAMP, 1, x'00', 0)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let err = format!("{:#}", Ctx::open(dir.path()).await.err().expect("refused"));
    assert!(err.contains("newer openpasture") && err.contains("99999") && err.contains(SNAPSHOT_DIR), "{err}");
}

#[test]
fn migration_versions_are_unique() {
    let mut seen = std::collections::BTreeMap::new();
    for m in MIGRATOR.iter().filter(|m| !m.migration_type.is_down_migration()) {
        if let Some(other) = seen.insert(m.version, m.description.clone()) {
            panic!(
                "migrations {:04}_{} and {:04}_{} share a version: every database would fail to open; renumber one",
                m.version, other, m.version, m.description
            );
        }
    }
}

/// 0890 as the prod/readiness branch had it before release (time-only dedupe).
const PRERELEASE_0890: &str = "
DELETE FROM fixes WHERE EXISTS (SELECT 1 FROM fixes f WHERE f.collar_id = fixes.collar_id AND f.t = fixes.t AND f.id < fixes.id);
DROP INDEX IF EXISTS fixes_collar_t;
CREATE UNIQUE INDEX fixes_collar_t ON fixes(collar_id, t);
CREATE UNIQUE INDEX cues_collar_t_kind ON cues(collar_id, t, COALESCE(kind, CASE WHEN margin_m < 0 THEN 'outside' ELSE 'warn' END));
ALTER TABLE fixes ADD COLUMN seq INTEGER;
ALTER TABLE cues ADD COLUMN seq INTEGER;
CREATE UNIQUE INDEX fixes_collar_seq ON fixes(collar_id, seq) WHERE seq IS NOT NULL;
CREATE UNIQUE INDEX cues_collar_seq ON cues(collar_id, seq) WHERE seq IS NOT NULL;
";

#[tokio::test]
async fn a_branch_database_with_the_prerelease_0890_is_brought_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let pool = released_db(dir.path()).await;
    sqlx::raw_sql(PRERELEASE_0890).execute(&pool).await.unwrap();
    let old_sum: Vec<u8> = (0..48)
        .map(|i| {
            u8::from_str_radix(&"e7c7a75c791f87cf9468690756772dd6542951290f02b2d0b3041fb3c5b8ee5ae15eb1e61e342d8c783a2449c0ccb023"[i * 2..i * 2 + 2], 16)
                .unwrap()
        })
        .collect();
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (890, 'report idempotency', CURRENT_TIMESTAMP, 1, ?, 0)")
        .bind(old_sum)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let ctx = Ctx::open(dir.path()).await.expect("opens instead of 'migration 890 was previously applied but has been modified'");
    let sum: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 890").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(sum, MIGRATOR.iter().find(|m| m.version == 890).unwrap().checksum.to_vec());
    // Its keys are the released ones: a different fix or cue at one instant is kept.
    for lon in [-93.62, -93.61] {
        let sql = "INSERT INTO fixes (collar_id, at, t, lon, lat, accuracy_m) VALUES ('c', '2026-01-02T00:00:00.000Z', 1, ?, 42.0, 3.0) ON CONFLICT DO NOTHING";
        assert_eq!(sqlx::query(sql).bind(lon).execute(ctx.db()).await.unwrap().rows_affected(), 1);
    }
    for level in [1, 2, 2] {
        sqlx::query(
            "INSERT INTO cues (collar_id, at, t, level, margin_m, kind) VALUES ('c', '2026-01-02T00:00:00.000Z', 1, ?, 1.0, 'warn') ON CONFLICT DO NOTHING",
        )
        .bind(level)
        .execute(ctx.db())
        .await
        .unwrap();
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cues WHERE collar_id = 'c'").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(n, 2);
    let _: Option<i64> = sqlx::query_scalar("SELECT conflicts FROM health LIMIT 1").fetch_optional(ctx.db()).await.unwrap().flatten();
    let _: Option<i64> = sqlx::query_scalar("SELECT seq FROM collar_records LIMIT 1").fetch_optional(ctx.db()).await.unwrap();
}

#[tokio::test]
async fn a_snapshot_that_cant_be_written_stops_the_upgrade_and_leaves_the_database_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    released_db(dir.path()).await.close().await;
    // Something in the way of the backups folder (as a full disk would stop the copy).
    std::fs::write(dir.path().join(SNAPSHOT_DIR), b"not a folder").unwrap();
    let err = format!("{:#}", Ctx::open(dir.path()).await.err().expect("refused"));
    assert!(err.contains("snapshot"), "{err}");
    let pool = sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(dir.path().join(DB_FILE))).await.unwrap();
    let max: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    assert_eq!(max, 880, "nothing migrated without a snapshot");
    pool.close().await;
    // Cleared, it upgrades.
    std::fs::remove_file(dir.path().join(SNAPSHOT_DIR)).unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    assert_eq!(snapshots(dir.path()).len(), 1);
    assert!(op_core::store::migration_status(ctx.db()).await.unwrap().pending.is_empty());
}

#[tokio::test]
async fn opening_reports_its_progress_and_ends_ready() {
    let dir = tempfile::tempdir().unwrap();
    released_db(dir.path()).await.close().await;
    let opening = Ctx::begin_open(dir.path()).unwrap();
    let progress = opening.progress();
    assert_eq!(progress.lock().unwrap().phase, "opening");
    let _ctx = opening.finish().await.unwrap();
    let p = progress.lock().unwrap().clone();
    assert_eq!(p.phase, "ready");
    assert!(p.pending.is_empty() && p.db_bytes > 0, "{p:?}");
}
