//! Running the official CLIs: short status checks, and long streamed runs
//! with stdin, a timeout, and kill on drop. Each run is its own process group,
//! and the whole group is killed on timeout or drop, so tools the CLI started
//! (node workers, shells) don't outlive it.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::detect::Locator;

pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// The last few stderr lines, for error messages.
    pub fn stderr_tail(&self) -> String {
        let lines: Vec<&str> = self.stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        lines[lines.len().saturating_sub(3)..].join(" | ")
    }
}

const MAX_STDERR: usize = 64 * 1024;
/// Most stdout kept for the answer; the rest is still read, then dropped.
const MAX_STDOUT: usize = 8 * 1024 * 1024;
/// Longest stdout line passed on; longer ones are cut.
const MAX_LINE: usize = 1024 * 1024;

fn command(bin: &Path, args: &[impl AsRef<OsStr>]) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(args).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    // GUI apps start with a minimal PATH; node-based CLIs need `node` next to them.
    cmd.env("PATH", Locator::from_env().child_path(bin));
    cmd.env("NO_COLOR", "1");
    cmd
}

/// A quick command with no stdin, e.g. `codex login status`.
pub async fn run_quick(bin: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<Output> {
    let mut cmd = command(bin, args);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().with_context(|| format!("starting {}", bin.display()))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output()).await.context("timed out")??;
    Ok(Output {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Kills the child's whole process group when dropped (run over, timed out,
/// or the request went away).
struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
            // SAFETY: killpg only sends a signal; the group id is our child's pid.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

/// Run with `stdin` as input and `env` added, calling `on_line` for every
/// stdout line (invalid UTF-8 replaced). The process group is killed on
/// timeout or when the future is dropped.
pub async fn run_streaming(
    bin: &Path,
    args: &[std::ffi::OsString],
    cwd: &Path,
    env: &[(String, String)],
    stdin: String,
    timeout: Duration,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<Output> {
    let mut cmd = command(bin, args);
    cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", bin.display()))?;
    let _group = GroupKill(child.id());

    let mut child_stdin = child.stdin.take().context("no stdin")?;
    let writer = tokio::spawn(async move {
        let _ = child_stdin.write_all(stdin.as_bytes()).await;
        let _ = child_stdin.shutdown().await;
    });
    let mut child_stderr = child.stderr.take().context("no stderr")?;
    let reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match child_stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < MAX_STDERR {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let mut stdout = child.stdout.take().context("no stdout")?;

    let run = async {
        let mut all = String::new();
        let mut line: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        let mut emit = |line: &mut Vec<u8>, all: &mut String| {
            let text = String::from_utf8_lossy(line);
            on_line(&text);
            if all.len() + text.len() < MAX_STDOUT {
                all.push_str(&text);
                all.push('\n');
            }
            line.clear();
        };
        loop {
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            for &b in &chunk[..n] {
                if b == b'\n' {
                    emit(&mut line, &mut all);
                } else if line.len() < MAX_LINE {
                    line.push(b);
                }
            }
        }
        if !line.is_empty() {
            emit(&mut line, &mut all);
        }
        let status = child.wait().await?;
        anyhow::Ok((status, all))
    };
    let (status, stdout) = match tokio::time::timeout(timeout, run).await {
        Ok(r) => r?,
        Err(_) => {
            let _ = child.start_kill();
            anyhow::bail!("timed out after {} s", timeout.as_secs());
        }
    };
    let _ = writer.await;
    let stderr = reader.await.unwrap_or_default();
    Ok(Output { success: status.success(), stdout, stderr })
}
