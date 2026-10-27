//! `collar-sim farm`: a throwaway simulated farm in one command. It starts
//! its own openpasture server on its own data dir and port, makes a farm
//! ("Simulated farm", in the Flint Hills), two paddocks and a herd, runs the
//! collars against it, and stops the server when they stop.
//!
//! It refuses a data dir that holds real collars, the app's own data dir,
//! and port 7878 (where a real farm runs) unless `--force` says otherwise.
//! A data dir it made carries a `SIMULATED-FARM` file.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use op_geo::LonLat;
use op_geo::projection::Projection;
use serde_json::{Value, json};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

use crate::{Api, Sim};

/// The port a simulated farm takes by default: clear of 7878, the default
/// (and real) farm's.
pub const DEFAULT_PORT: u16 = 7991;
/// Where openpasture runs a real farm.
pub const REAL_PORT: u16 = 7878;
/// The file that marks a data dir as a simulated farm's.
pub const MARKER: &str = "SIMULATED-FARM";
/// The server's database in a data dir (`op_core::store::DB_FILE`).
const DB_FILE: &str = "openpasture.db";
/// Where the simulated farm lies: the Flint Hills, as the first-run story.
const CENTER: LonLat = [-96.476, 38.59];

#[derive(clap::Args)]
pub struct FarmArgs {
    /// Port for the simulated farm's server.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Data dir for the simulated farm. Default: a new one under the system
    /// temp dir, deleted afterwards unless --keep.
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Keep a new data dir after the run.
    #[arg(long)]
    pub keep: bool,
    /// The openpasture binary. Default: next to collar-sim, else on PATH.
    #[arg(long, value_name = "PATH")]
    pub openpasture: Option<PathBuf>,
    /// Run anyway on port 7878 or a data dir with real collars. Don't.
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub sim: Sim,
}

/// Refuse the real farm's port.
pub fn check_port(port: u16, force: bool) -> anyhow::Result<()> {
    if port == REAL_PORT && !force {
        bail!("port {REAL_PORT} is where a real farm runs; a simulated farm takes another (default {DEFAULT_PORT}). --force overrides.");
    }
    Ok(())
}

/// What a data dir is, as far as a simulated farm goes.
#[derive(Debug, PartialEq)]
pub enum DataDir {
    /// Missing or empty: the farm makes it.
    New,
    /// A simulated farm's from an earlier run.
    Simulated,
}

/// The app's own data dirs (`op_core::default_data_dir`), refused whatever they hold.
fn app_data_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = dirs::data_dir().map(|d| d.join("openpasture")).into_iter().collect();
    out.extend(std::env::var_os("OPENPASTURE_DATA_DIR").map(PathBuf::from));
    out
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Refuse a data dir that holds a real farm: real collars (any collar not
/// marked simulated), the app's own data dir, or a farm database this tool
/// didn't make and that holds no simulated collars.
pub async fn check_data_dir(dir: &Path, force: bool) -> anyhow::Result<DataDir> {
    let refuse = |why: String| -> anyhow::Result<DataDir> {
        if force {
            eprintln!("warning: {why}; running anyway (--force)");
            Ok(DataDir::Simulated)
        } else {
            bail!("{why}. --force overrides.")
        }
    };
    if app_data_dirs().iter().any(|d| same_dir(d, dir)) {
        return refuse(format!("{} is the app's own data dir, where a real farm lives", dir.display()));
    }
    let empty = match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    if empty {
        return Ok(DataDir::New);
    }
    let db = dir.join(DB_FILE);
    let marked = dir.join(MARKER).exists();
    if !db.exists() {
        return if marked { Ok(DataDir::Simulated) } else { refuse(format!("{} isn't empty and isn't a simulated farm's", dir.display())) };
    }
    let (real, simulated) = collars(&db).await?;
    if real > 0 {
        return refuse(format!("{} holds {real} real collar{}", dir.display(), if real == 1 { "" } else { "s" }));
    }
    if !marked && simulated == 0 {
        return refuse(format!("{} holds a farm that isn't a simulated farm's", dir.display()));
    }
    Ok(DataDir::Simulated)
}

/// (real, simulated) collars in a farm database, read only. Collars from
/// before the `simulated` column are all real.
async fn collars(db: &Path) -> anyhow::Result<(i64, i64)> {
    let mut conn = SqliteConnectOptions::new().filename(db).read_only(true).connect().await.with_context(|| format!("opening {}", db.display()))?;
    let has_table: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'collars'").fetch_one(&mut conn).await?;
    if has_table == 0 {
        conn.close().await?;
        return Ok((0, 0));
    }
    let has_flag: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('collars') WHERE name = 'simulated'").fetch_one(&mut conn).await?;
    let counts: (i64, i64) = if has_flag > 0 {
        sqlx::query_as("SELECT COUNT(*) - COALESCE(SUM(simulated), 0), COALESCE(SUM(simulated), 0) FROM collars").fetch_one(&mut conn).await?
    } else {
        (sqlx::query_scalar("SELECT COUNT(*) FROM collars").fetch_one(&mut conn).await?, 0)
    };
    conn.close().await?;
    Ok(counts)
}

/// The openpasture binary: `--openpasture`, else beside this one, else PATH.
fn server_binary(given: Option<&PathBuf>) -> PathBuf {
    if let Some(p) = given {
        return p.clone();
    }
    let name = if cfg!(windows) { "openpasture.exe" } else { "openpasture" };
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from(name))
}

/// A square paddock `w` metres a side, its south-west corner `e`, `n` metres from the centre.
fn square(p: &Projection, e: f64, n: f64, w: f64) -> Value {
    let ring = [p.offset(e, n), p.offset(e + w, n), p.offset(e + w, n + w), p.offset(e, n + w), p.offset(e, n)];
    json!({ "type": "Polygon", "coordinates": [ring] })
}

/// A farm, two paddocks and a herd per `--herds` entry (or one), on an empty server.
async fn make_farm(api: &Api, sim: &Sim) -> anyhow::Result<()> {
    let st = api.get("/api/state").await?;
    if !st["farm"].is_null() {
        return Ok(());
    }
    api.post("/api/farm", json!({ "name": "Simulated farm", "center": CENTER })).await?;
    let p = Projection::new(CENTER);
    let names: Vec<(String, usize)> = if sim.herds.is_empty() { vec![("Simulated herd".into(), sim.count)] } else { sim.herds.clone() };
    for (i, (name, count)) in names.iter().enumerate() {
        let e = i as f64 * 450.0 - 200.0;
        let first = api.post("/api/paddocks", json!({ "name": format!("Sim {}", 2 * i + 1), "geometry": square(&p, e, -200.0, 200.0) })).await?;
        api.post("/api/paddocks", json!({ "name": format!("Sim {}", 2 * i + 2), "geometry": square(&p, e, 0.0, 200.0) })).await?;
        let paddock = first["id"].as_str().context("paddock id")?;
        api.post("/api/herds", json!({ "name": name, "species": "cattle", "count": count, "paddock_id": paddock })).await?;
    }
    Ok(())
}

pub async fn run(a: FarmArgs) -> anyhow::Result<()> {
    check_port(a.port, a.force)?;
    TcpListener::bind(("127.0.0.1", a.port)).with_context(|| format!("port {} is taken; pick another with --port", a.port))?;
    let temp = a.data_dir.is_none();
    let dir = a.data_dir.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("collar-sim-farm-{}", std::process::id())));
    let kind = check_data_dir(&dir, a.force).await?;
    std::fs::create_dir_all(&dir)?;
    if kind == DataDir::New {
        std::fs::write(dir.join(MARKER), "A simulated farm made by `collar-sim farm`. Every collar here is simulated.\n")?;
    }

    let bin = server_binary(a.openpasture.as_ref());
    // The token first: it opens the data dir (and makes it) before the server holds it.
    let out = std::process::Command::new(&bin)
        .args(["token", "--data-dir"])
        .arg(&dir)
        .output()
        .with_context(|| format!("running {}; build it with cargo build -p op-cli, or pass --openpasture", bin.display()))?;
    if !out.status.success() {
        bail!("openpasture token: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();

    let log = std::fs::File::create(dir.join("server.log"))?;
    let mut server = tokio::process::Command::new(&bin)
        .args(["serve", "--bind", "127.0.0.1", "--port", &a.port.to_string(), "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    let url = format!("http://127.0.0.1:{}", a.port);
    let api = Api::new(&url, Some(token))?;

    let result = async {
        let mut up = false;
        for _ in 0..120 {
            if let Some(status) = server.try_wait()? {
                bail!("the server stopped ({status}); see {}", dir.join("server.log").display());
            }
            if api.get("/api/state").await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !up {
            bail!("the server didn't answer in 60 s; see {}", dir.join("server.log").display());
        }
        make_farm(&api, &a.sim).await?;
        println!("simulated farm at {url} (data {})", dir.display());
        crate::run(&api, &a.sim, None, &dir.join("collar-sim.json")).await
    }
    .await;

    stop(&mut server).await;
    if temp && !a.keep {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("kept {}; run again with --data-dir to pick it up", dir.display());
    }
    result
}

/// Ask the server to stop (ctrl-c, as a person would), then make sure.
async fn stop(server: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = server.id().and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: a signal to our own child.
        unsafe { libc::kill(pid, libc::SIGINT) };
        if tokio::time::timeout(Duration::from_secs(10), server.wait()).await.is_ok() {
            return;
        }
    }
    let _ = server.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_farms_port_is_refused() {
        assert!(check_port(REAL_PORT, false).unwrap_err().to_string().contains("7878"));
        assert!(check_port(REAL_PORT, true).is_ok());
        assert!(check_port(DEFAULT_PORT, false).is_ok());
    }

    async fn farm_db(dir: &Path, collars: &[bool], with_flag: bool) {
        let mut conn = SqliteConnectOptions::new().filename(dir.join(DB_FILE)).create_if_missing(true).connect().await.unwrap();
        let ddl = if with_flag { "CREATE TABLE collars (id TEXT, simulated INTEGER NOT NULL DEFAULT 0)" } else { "CREATE TABLE collars (id TEXT)" };
        sqlx::query(ddl).execute(&mut conn).await.unwrap();
        for (i, sim) in collars.iter().enumerate() {
            let q = if with_flag { "INSERT INTO collars (id, simulated) VALUES (?, ?)" } else { "INSERT INTO collars (id) VALUES (?)" };
            let mut q = sqlx::query(q).bind(format!("col_{i}"));
            if with_flag {
                q = q.bind(*sim);
            }
            q.execute(&mut conn).await.unwrap();
        }
        conn.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_data_dir_with_real_collars_is_refused() {
        let t = tempfile::tempdir().unwrap();
        // Missing, then empty: new.
        assert_eq!(check_data_dir(&t.path().join("none"), false).await.unwrap(), DataDir::New);
        assert_eq!(check_data_dir(t.path(), false).await.unwrap(), DataDir::New);

        // A farm with one real collar among simulated ones.
        let real = tempfile::tempdir().unwrap();
        farm_db(real.path(), &[true, false, true], true).await;
        let e = check_data_dir(real.path(), false).await.unwrap_err().to_string();
        assert!(e.contains("1 real collar"), "{e}");
        // Even with the marker.
        std::fs::write(real.path().join(MARKER), "").unwrap();
        assert!(check_data_dir(real.path(), false).await.is_err());
        assert!(check_data_dir(real.path(), true).await.is_ok(), "--force overrides");

        // A farm from before the flag: every collar is real.
        let old = tempfile::tempdir().unwrap();
        farm_db(old.path(), &[true], false).await;
        assert!(check_data_dir(old.path(), false).await.is_err());

        // A farm with no collars that this tool didn't make.
        let bare = tempfile::tempdir().unwrap();
        farm_db(bare.path(), &[], true).await;
        assert!(check_data_dir(bare.path(), false).await.is_err());
        std::fs::write(bare.path().join(MARKER), "").unwrap();
        assert_eq!(check_data_dir(bare.path(), false).await.unwrap(), DataDir::Simulated);

        // Simulated collars only: a simulated farm.
        let sim = tempfile::tempdir().unwrap();
        farm_db(sim.path(), &[true, true], true).await;
        assert_eq!(check_data_dir(sim.path(), false).await.unwrap(), DataDir::Simulated);

        // Other files and no farm: not ours.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("notes.txt"), "").unwrap();
        assert!(check_data_dir(other.path(), false).await.is_err());
    }

    #[tokio::test]
    async fn the_apps_own_data_dir_is_refused() {
        let Some(app) = app_data_dirs().into_iter().next() else { return };
        let e = check_data_dir(&app, false).await.unwrap_err().to_string();
        assert!(e.contains("app's own data dir"), "{e}");
    }
}
