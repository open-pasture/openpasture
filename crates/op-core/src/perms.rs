//! Owner-only files and folders. The data dir holds the database (farm
//! records, the app token and other settings), its snapshots, keys and logs;
//! none of it is for other users of the machine or of a shared volume, so
//! folders are 0700 and files 0600 on Unix, whatever the umask. On Windows
//! the data dir under the user's profile inherits its ACL (the user, SYSTEM,
//! Administrators); these are no-ops there (docs/SELF-HOSTING.md says so).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Create `dir` (and parents) if needed and make it owner-only. A folder this
/// process can't change (someone else's) is left as it is, with a warning.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict(dir, 0o700);
    Ok(())
}

/// Make an existing file owner-only; a missing file is fine.
pub fn private_file(path: &Path) {
    if path.exists() {
        restrict(path, 0o600);
    }
}

/// Create (or truncate) a file that is owner-only from the start.
pub fn create_private(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let f = opts.open(path)?;
    restrict(path, 0o600);
    Ok(f)
}

/// Open a file for appending, owner-only (created so, or tightened).
pub fn append_private(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let f = opts.open(path)?;
    restrict(path, 0o600);
    Ok(f)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.permissions().mode() & 0o777 == mode {
        return;
    }
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        tracing::warn!(path = %path.display(), "can't make it private ({mode:o}): {e}");
    }
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}
