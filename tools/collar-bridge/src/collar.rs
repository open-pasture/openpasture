//! A collar on a serial port: lines in, commands out, one at a time.
//!
//! The firmware drops what arrives while it is still handling the last line
//! (`console.c`), so a command is written only after the previous one's
//! reply. Log lines that arrive meanwhile go to the [`Session`].

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::capture::{Capture, Dir};
use crate::console::{self, Line, Logged, Reply};
use crate::serial::{Port, SerialIn};
use crate::session::Session;

/// Most console commands answer at once; a boundary is verified first.
pub const REPLY_WAIT: Duration = Duration::from_secs(5);
pub const BOUNDARY_WAIT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum CommandError {
    /// No reply in time.
    Silent,
    /// The port went away.
    Gone(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Silent => write!(f, "the collar didn't answer"),
            Self::Gone(why) => write!(f, "the collar went away ({why})"),
        }
    }
}

pub struct Collar {
    port: Port,
    rx: mpsc::Receiver<SerialIn>,
    pub session: Session,
    capture: Arc<Capture>,
    verbose: bool,
    gone: Option<String>,
}

/// One line for the terminal, with the time.
pub fn say(msg: impl AsRef<str>) {
    println!("{} {}", chrono::Local::now().format("%H:%M:%S"), msg.as_ref());
}

impl Collar {
    pub fn open(path: &Path, capture: Arc<Capture>, verbose: bool) -> std::io::Result<Collar> {
        let port = Port::open(path)?;
        let rx = port.reader()?;
        Ok(Collar { port, rx, session: Session::new(), capture, verbose, gone: None })
    }

    /// The session, to carry over to the next connection.
    pub fn into_session(self) -> Session {
        self.session
    }

    /// Why the port is gone, once it is.
    pub fn gone(&self) -> Option<&str> {
        self.gone.as_deref()
    }

    fn take(&mut self, raw: String) -> Logged {
        self.capture.serial(Dir::In, &raw);
        if self.verbose {
            say(format!("< {raw}"));
        }
        let l = console::parse(&raw);
        if let Some(msg) = self.session.on_line(&l, Instant::now()) {
            say(msg);
        }
        l
    }

    /// The next line, handled; `None` after `wait` or once the port is gone.
    pub async fn next(&mut self, wait: Duration) -> Option<Logged> {
        if self.gone.is_some() {
            return None;
        }
        match tokio::time::timeout(wait, self.rx.recv()).await {
            Ok(Some(SerialIn::Line(raw))) => Some(self.take(raw)),
            Ok(Some(SerialIn::Gone(why))) => {
                self.capture.note(format!("port gone: {why}"));
                self.gone = Some(why);
                None
            }
            Ok(None) => {
                self.gone = Some("the reader stopped".into());
                None
            }
            Err(_) => None,
        }
    }

    /// Handle lines for `wait`.
    pub async fn listen(&mut self, wait: Duration) {
        let until = Instant::now() + wait;
        while self.gone.is_none() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.next(left).await;
        }
    }

    /// Write a command and wait for its reply.
    pub async fn command(&mut self, line: &str, wait: Duration) -> Result<Reply, CommandError> {
        if let Some(why) = &self.gone {
            return Err(CommandError::Gone(why.clone()));
        }
        self.capture.serial(Dir::Out, line);
        if self.verbose {
            say(format!("> {}", crate::capture::redact_line(line)));
        }
        if let Err(e) = self.port.write_line(line).await {
            self.gone = Some(e.to_string());
            return Err(CommandError::Gone(e.to_string()));
        }
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CommandError::Silent);
            }
            match self.next(left).await {
                Some(Logged { line: Line::Reply(r), .. }) => return Ok(r),
                Some(_) => {}
                None => {
                    if let Some(why) = &self.gone {
                        return Err(CommandError::Gone(why.clone()));
                    }
                }
            }
        }
    }

    /// Ask for `status` and keep what it says.
    pub async fn status(&mut self) -> Result<Reply, CommandError> {
        let r = self.command("status", REPLY_WAIT).await?;
        match &r {
            Reply::Status(s) => {
                self.session.fw = Some(s.fw.clone());
                self.session.status = Some(s.clone());
            }
            Reply::Unprovisioned { fw } => {
                self.session.fw = Some(fw.clone());
                self.session.status = None;
            }
            _ => {}
        }
        Ok(r)
    }
}
