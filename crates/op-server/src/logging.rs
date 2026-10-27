//! Logging for `openpasture serve` and the desktop app: an env filter
//! (`RUST_LOG`) over text lines, to stdout or to a log file that rotates by
//! size so a server left running for a season can't fill the disk.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Used when `RUST_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info,sqlx=warn,tower_http=warn,tantivy=warn,rmcp=warn";
/// A log file rotates once it would pass this size.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated files kept beside the live one (`name.1` is the newest).
pub const KEEP: usize = 5;
/// Directory for `openpasture serve` to log to instead of stdout.
pub const LOG_DIR_ENV: &str = "OPENPASTURE_LOG_DIR";

fn filter(default: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into())
}

/// Log to stdout. `default` is the filter when `RUST_LOG` is unset.
/// Colours only on a terminal, so `docker logs` and journald get plain text.
pub fn init_stdout(default: &str) {
    use std::io::IsTerminal;
    let _ = tracing_subscriber::fmt().with_env_filter(filter(default)).with_ansi(io::stdout().is_terminal()).try_init();
}

/// Log to `<dir>/<name>`, rotated at [`MAX_BYTES`] keeping [`KEEP`] old
/// files. Falls back to stderr if the file can't be opened.
pub fn init_file(dir: &Path, name: &str, default: &str) {
    match RollingFile::open(dir, name, MAX_BYTES, KEEP) {
        Ok(f) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter(default)).with_ansi(false).with_writer(Mutex::new(f)).try_init();
        }
        Err(e) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter(default)).with_writer(io::stderr).try_init();
            tracing::warn!(dir = %dir.display(), "can't open the log file, logging to stderr: {e}");
        }
    }
}

/// A file that appends until it would pass `max_bytes`, then is renamed
/// `name.1` (shifting older ones up, dropping past `keep`) and started anew.
pub struct RollingFile {
    dir: PathBuf,
    name: String,
    max_bytes: u64,
    keep: usize,
    file: File,
    size: u64,
}

impl RollingFile {
    pub fn open(dir: &Path, name: &str, max_bytes: u64, keep: usize) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new().create(true).append(true).open(dir.join(name))?;
        let size = file.metadata()?.len();
        Ok(Self { dir: dir.to_path_buf(), name: name.to_owned(), max_bytes, keep, file, size })
    }

    fn path(&self, n: usize) -> PathBuf {
        if n == 0 { self.dir.join(&self.name) } else { self.dir.join(format!("{}.{n}", self.name)) }
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let _ = std::fs::remove_file(self.path(self.keep));
        for n in (0..self.keep).rev() {
            let from = self.path(n);
            if from.exists() {
                std::fs::rename(&from, self.path(n + 1))?;
            }
        }
        self.file = OpenOptions::new().create(true).append(true).open(self.path(0))?;
        self.size = 0;
        Ok(())
    }
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_file_rotates_and_only_the_newest_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = RollingFile::open(dir.path(), "server.log", 100, 2).unwrap();
        for i in 0..10 {
            f.write_all(format!("{i:02} {}\n", "x".repeat(46)).as_bytes()).unwrap();
        }
        f.flush().unwrap();
        let mut names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["server.log", "server.log.1", "server.log.2"]);
        let read = |n: &str| std::fs::read_to_string(dir.path().join(n)).unwrap();
        assert!(read("server.log").starts_with("08 ") && read("server.log.1").starts_with("06 ") && read("server.log.2").starts_with("04 "));
        assert!(std::fs::metadata(dir.path().join("server.log")).unwrap().len() <= 100);
    }

    #[test]
    fn reopening_appends_and_counts_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        RollingFile::open(dir.path(), "a.log", 100, 2).unwrap().write_all(&[b'x'; 90]).unwrap();
        let mut f = RollingFile::open(dir.path(), "a.log", 100, 2).unwrap();
        f.write_all(&[b'y'; 20]).unwrap();
        assert_eq!(std::fs::read(dir.path().join("a.log.1")).unwrap().len(), 90);
        assert_eq!(std::fs::read(dir.path().join("a.log")).unwrap(), [b'y'; 20]);
    }
}
