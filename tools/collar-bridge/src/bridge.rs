//! `collar-bridge bridge`: a real collar on USB serial, reporting to a
//! server as if it had its own modem.
//!
//! Every report interval (60 s, or the cadence of the config the collar
//! took) it asks the collar for `status` and posts `/collar/v1/report` with
//! the fixes, cues and episodes it printed since. A config on the reply goes
//! to the console as `config {…}`; the collar's answer goes back on the next
//! report (`config_version` or `config_reject`). Every download interval it
//! asks `/collar/v1/boundary?have=…` and writes what comes back as
//! `boundary {…}`; the console's answer is posted as the ack. An unplugged
//! collar is waited for; what it printed before stays queued.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use op_protocol::{AckStatus, RejectCode};

use crate::capture::Capture;
use crate::collar::{BOUNDARY_WAIT, Collar, CommandError, REPLY_WAIT, say};
use crate::console::Reply;
use crate::link::Link;
use crate::session::{Session, fw_at_least};
use crate::uplink::{Sent, Uplink};

pub struct Options {
    pub port: PathBuf,
    pub server: String,
    pub key: String,
    /// Provision the collar with this first, unless it already is this collar.
    pub link: Option<Link>,
    /// Provision even when the collar already is this collar (wipes its boundaries).
    pub force: bool,
    pub capture: Arc<Capture>,
    pub verbose: bool,
}

/// Boundaries fetched in a row before going back to listening.
const DOWNLOADS_IN_A_ROW: usize = 4;

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let uplink = Uplink::new(&opts.server, &opts.key, opts.capture.clone());
    if let Some(p) = opts.capture.path() {
        say(format!("capturing to {}", p.display()));
    }
    let mut session = Session::new();
    let mut provisioned = false;
    loop {
        let mut collar = wait_for_port(&opts, session).await;
        say(format!("connected to {}", opts.port.display()));
        opts.capture.note(format!("connected to {}", opts.port.display()));
        if let Err(e) = serve(&mut collar, &uplink, &opts, &mut provisioned).await
            && collar.gone() != Some(e.as_str())
        {
            say(e);
        }
        say(format!("{}: waiting for the collar", collar.gone().unwrap_or("disconnected")));
        session = collar.into_session();
    }
}

async fn wait_for_port(opts: &Options, session: Session) -> Collar {
    let mut told = false;
    loop {
        match Collar::open(&opts.port, opts.capture.clone(), opts.verbose) {
            Ok(mut c) => {
                c.session = session;
                return c;
            }
            Err(e) => {
                if !told {
                    say(format!("can't open {} ({e}): plug the collar in; trying every 2 s", opts.port.display()));
                    told = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Talk to one connected collar until it goes away.
async fn serve(collar: &mut Collar, uplink: &Uplink, opts: &Options, provisioned: &mut bool) -> Result<(), String> {
    // The collar may be mid-boot: give it a moment, then ask who it is.
    collar.listen(Duration::from_millis(500)).await;
    let first = collar.status().await.map_err(|e| e.to_string())?;
    describe(&first);

    if let Some(link) = &opts.link
        && !*provisioned
    {
        let same = collar.session.status.as_ref().is_some_and(|s| s.collar_id == link.collar_id);
        if same && !opts.force {
            say(format!("already provisioned as {}; not provisioning again (--force wipes its boundaries and does)", link.collar_id));
        } else {
            provision(collar, link).await?;
            describe(&collar.status().await.map_err(|e| e.to_string())?);
        }
        *provisioned = true;
    }
    if collar.session.status.is_none() {
        say("the collar isn't provisioned: its fixes are reported, but it can't take boundaries (run with --provision <link.json>)");
    }

    let mut next_report = Instant::now();
    let mut next_poll = Instant::now();
    loop {
        if let Some(why) = collar.gone() {
            return Err(why.to_owned());
        }
        collar.listen(Duration::from_millis(250)).await;
        flush_acks(collar, uplink).await;
        let now = Instant::now();
        let (report_s, poll_s) = collar.session.intervals(now);
        if now >= next_report {
            next_report = now + Duration::from_secs(report_s.into());
            if report(collar, uplink).await {
                next_poll = now;
            }
        }
        if now >= next_poll {
            next_poll = now + Duration::from_secs(poll_s.into());
            download(collar, uplink).await;
        }
    }
}

fn describe(r: &Reply) {
    match r {
        Reply::Status(s) => {
            let active = s.active.map_or("none".into(), |v| format!("v{v}"));
            let config = s.config_version.map_or("none".into(), |v| format!("v{v}"));
            say(format!("collar {} herd {} firmware {}: boundary {active}, holds {:?}, config {config}", s.collar_id, s.herd_id, s.fw, s.slots));
        }
        Reply::Unprovisioned { fw } => say(format!("collar not provisioned, firmware {fw}")),
        other => say(format!("unexpected reply to status: {other:?}")),
    }
}

/// Write `provision {…}` and check the collar took it.
pub async fn provision(collar: &mut Collar, link: &Link) -> Result<(), String> {
    let payload = link.payload().map_err(|e| e.to_string())?;
    say(format!("provisioning as {} (herd {}, endpoint {})", link.collar_id, link.herd_id, link.endpoint));
    match collar.command(&format!("provision {payload}"), REPLY_WAIT).await {
        Ok(Reply::Provisioned(id)) if id == link.collar_id => {
            say(format!("provisioned: {id}"));
            Ok(())
        }
        Ok(Reply::Provisioned(id)) => Err(format!("the collar says it is {id}, not {}", link.collar_id)),
        Ok(Reply::Error(code)) => Err(format!("the collar refused the provisioning: {code}")),
        Ok(other) => Err(format!("unexpected reply to provision: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Post a report; true when the server holds a version the collar doesn't.
async fn report(collar: &mut Collar, uplink: &Uplink) -> bool {
    // Fresh slots and config version for the report. A busy collar still reports what it printed.
    if let Err(e @ CommandError::Gone(_)) = collar.status().await {
        say(e.to_string());
        return false;
    }
    let rep = collar.session.take_report();
    let (fixes, cues, episodes) = (rep.fixes.len(), rep.cues.len(), rep.episodes.len());
    let have = collar.session.status.as_ref().map_or(0, |s| s.have());
    match uplink.report(&rep).await {
        Sent::Ok(reply) => {
            let latest = reply.latest_version.map_or("none".into(), |v| format!("v{v}"));
            say(format!("report: {fixes} fixes, {cues} cues, {episodes} episodes; server's latest {latest}"));
            if let Some((wire, cmd)) = reply.config {
                let version = cmd.version;
                match collar.command(&format!("config {wire}"), REPLY_WAIT).await {
                    Ok(Reply::ConfigOk(v)) => {
                        let fast = cmd.fast_report_s.map(|f| format!(", fast {f} s")).unwrap_or_default();
                        say(format!("config v{v} taken: report every {} s, download every {} s{fast}", cmd.report_s, cmd.poll_s));
                        collar.session.config_accepted(cmd);
                    }
                    Ok(Reply::Error(code)) => {
                        say(format!("config v{version} refused: {code}"));
                        collar.session.config_refused(version, RejectCode::parse(&code));
                    }
                    Ok(other) => say(format!("unexpected reply to config: {other:?}")),
                    Err(e) => say(format!("config v{version}: {e}")),
                }
            }
            reply.latest_version.is_some_and(|v| v > have)
        }
        Sent::Unlinked(m) => {
            say(format!("the server doesn't take this collar key ({m}); is it the collar's current key?"));
            collar.session.restore(rep);
            false
        }
        Sent::Refused { status, message } => {
            say(format!("report refused ({status}): {message}. Not sent again."));
            false
        }
        Sent::Down(e) => {
            collar.session.restore(rep);
            let (f, _, _) = collar.session.waiting();
            say(format!("server unreachable ({e}); {f} fixes kept for the next try"));
            false
        }
    }
}

/// Fetch boundaries newer than the collar holds and write them to it.
async fn download(collar: &mut Collar, uplink: &Uplink) {
    for _ in 0..DOWNLOADS_IN_A_ROW {
        let Some(status) = collar.session.status.clone() else { return };
        let free = fw_at_least(&status.fw, 0, 2).then(|| op_protocol::CollarLimits::V0.slots.saturating_sub(status.slots.len()));
        let d = match uplink.boundary(status.have(), free).await {
            Sent::Ok(Some(d)) => d,
            Sent::Ok(None) => return,
            Sent::Unlinked(m) => return say(format!("download: the server doesn't take this collar key ({m})")),
            Sent::Refused { status, message } => return say(format!("download refused ({status}): {message}")),
            Sent::Down(e) => return say(format!("download: server unreachable ({e})")),
        };
        say(format!("boundary v{} ({}, {} bytes) to the collar", d.version, d.command_id, d.wire.len()));
        collar.session.delivered(d.version, &d.command_id);
        let now = Instant::now;
        let refused = match collar.command(&format!("boundary {}", d.wire), BOUNDARY_WAIT).await {
            Ok(Reply::Boundary { version, status }) => {
                say(format!("boundary v{version} {}", status.as_str()));
                collar.session.boundary_reply(version, status, None, now());
                false
            }
            Ok(Reply::Rejected { version, code }) => {
                say(format!("boundary v{version} rejected: {code}"));
                collar.session.boundary_reply(version, AckStatus::Rejected, RejectCode::parse(&code), now());
                true
            }
            // Refused before it could be read (too large, unreadable): still the collar's answer.
            Ok(Reply::Error(code)) if code != "unprovisioned" => {
                say(format!("boundary v{} refused: {code}", d.version));
                collar.session.boundary_reply(d.version, AckStatus::Rejected, RejectCode::parse(&code), now());
                true
            }
            Ok(other) => {
                say(format!("unexpected reply to boundary: {other:?}"));
                return;
            }
            Err(e) => {
                say(format!("boundary v{}: {e}", d.version));
                return;
            }
        };
        // The collar's log line with its ack time follows the reply.
        collar.listen(Duration::from_millis(300)).await;
        flush_acks(collar, uplink).await;
        if refused || collar.status().await.is_err() {
            return;
        }
    }
}

async fn flush_acks(collar: &mut Collar, uplink: &Uplink) {
    for owed in collar.session.take_acks(Instant::now()) {
        let owed = match owed {
            Ok(o) => o,
            Err(v) => {
                say(format!("v{v}'s ack can't be posted: this bridge didn't deliver v{v}, so it has no command id. The next report's slots show it."));
                continue;
            }
        };
        let a = &owed.ack;
        let what = format!("ack v{} {}{}", a.version, a.status.as_str(), a.code.map(|c| format!(" {}", c.as_str())).unwrap_or_default());
        match uplink.ack(a).await {
            Sent::Ok(()) => {
                let clock = if owed.host_time { " (no GNSS time yet: stamped with this computer's clock)" } else { "" };
                say(format!("{what} posted{clock}"));
            }
            Sent::Down(e) => {
                say(format!("{what}: server unreachable ({e}); trying again"));
                collar.session.retry_ack(owed, Instant::now());
            }
            Sent::Unlinked(m) => say(format!("{what}: the server doesn't take this collar key ({m})")),
            Sent::Refused { status, message } => say(format!("{what} refused ({status}): {message}")),
        }
    }
}
