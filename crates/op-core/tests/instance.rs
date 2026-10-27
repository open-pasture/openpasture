//! One server per data dir: the lock in `Ctx::open` and `server.json`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use op_core::Ctx;
use op_core::instance::{self, AlreadyRunning, RunningServer};

/// Set in the child process that holds a data dir's lock for the cross-process test.
const HOLD_ENV: &str = "OPENPASTURE_TEST_HOLD_DIR";

#[tokio::test]
async fn a_second_open_in_this_process_is_refused_until_the_first_stops() {
    let dir = tempfile::tempdir().unwrap();
    let first = Ctx::open(dir.path()).await.unwrap();
    let err = Ctx::open(dir.path()).await.err().expect("refused");
    let running = err.downcast_ref::<AlreadyRunning>().expect("typed error");
    assert_eq!(running.server.as_ref().map(|s| s.pid), Some(std::process::id()));
    assert!(instance::running(dir.path()).is_some());

    // A server that is shutting down hands the data dir on.
    first.shutdown();
    let second = Ctx::open(dir.path()).await.unwrap();
    drop(first);
    drop(second);
    // Nothing holds it once every Ctx is gone.
    assert!(instance::running(dir.path()).is_none());
    drop(Ctx::open(dir.path()).await.unwrap());
}

#[tokio::test]
async fn another_process_holding_the_data_dir_is_named_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "hold_the_lock_for_the_parent_test", "--nocapture", "--test-threads=1"])
        .env(HOLD_ENV, dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(lines.any(|l| l.is_ok_and(|l| l.contains("LOCK-HELD"))), "the child took the lock");

    let err = Ctx::open(dir.path()).await.err().expect("refused");
    let running = err.downcast_ref::<AlreadyRunning>().expect("typed error");
    let server = running.server.clone().expect("server.json read");
    assert_eq!(server.pid, child.id());
    assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:7999"));
    assert!(err.to_string().contains("http://127.0.0.1:7999"), "{err}");
    assert_eq!(instance::running(dir.path()).map(|s| s.pid), Some(child.id()));

    child.kill().unwrap();
    child.wait().unwrap();
    // The OS lets go of a dead process's lock; its server.json is stale.
    assert!(instance::running(dir.path()).is_none());
    drop(Ctx::open(dir.path()).await.unwrap());
}

/// Not a test on its own: the child process of the test above.
#[test]
fn hold_the_lock_for_the_parent_test() {
    let Some(dir) = std::env::var_os(HOLD_ENV) else { return };
    let dir = std::path::PathBuf::from(dir);
    let _lock = instance::lock(&dir).unwrap();
    let me = RunningServer { url: Some("http://127.0.0.1:7999".into()), port: Some(7999), ..instance::this_process() };
    instance::write_server_file(&dir, &me).unwrap();
    println!("LOCK-HELD");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(60));
}
