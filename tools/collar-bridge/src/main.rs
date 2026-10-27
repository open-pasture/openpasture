//! `collar-bridge`: see README.md.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use collar_bridge::capture::Capture;
use collar_bridge::collar::{Collar, say};
use collar_bridge::link::Link;
use collar_bridge::{bridge, factory, replay, serial};

#[derive(Parser)]
#[command(name = "collar-bridge", version, about = "A real OpenCollar on USB serial, talking to an openpasture server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report for a collar on the bench: its console log to /collar/v1, boundaries and configs back.
    Bridge {
        /// The collar's serial port, e.g. /dev/cu.usbmodem1101.
        #[arg(long)]
        port: PathBuf,
        /// The server, e.g. http://127.0.0.1:7878.
        #[arg(long)]
        server: String,
        /// The collar key (shown once at link time). Taken from --provision when absent.
        #[arg(long, env = "COLLAR_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Provision the collar first from a link: a bulk-link response, a single link or a card payload (JSON file).
        #[arg(long, value_name = "LINK_JSON")]
        provision: Option<PathBuf>,
        /// Which collar of a bulk-link response: id, name or tag.
        #[arg(long)]
        collar: Option<String>,
        /// The https endpoint written into the collar, when the link's isn't https.
        #[arg(long)]
        endpoint: Option<String>,
        /// Provision even if the collar already is this collar (wipes its boundaries).
        #[arg(long)]
        force: bool,
        /// Append every exchange to this file (JSON lines). Default: capture-<time>.jsonl.
        #[arg(long)]
        capture: Option<PathBuf>,
        /// Keep no capture.
        #[arg(long, conflicts_with = "capture")]
        no_capture: bool,
        /// Echo every serial line.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Provision a collar over serial and stop.
    Provision {
        #[arg(long)]
        port: PathBuf,
        #[arg(value_name = "LINK_JSON")]
        link: PathBuf,
        #[arg(long)]
        collar: Option<String>,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Send a captured session's requests again, in order, to a (test) server.
    Replay {
        capture: PathBuf,
        #[arg(long, requires = "key")]
        server: Option<String>,
        #[arg(long, env = "COLLAR_KEY", hide_env_values = true)]
        key: Option<String>,
        /// A token for the server, to check every collar on it is simulated before anything is sent.
        #[arg(long, env = "OPENPASTURE_TOKEN", hide_env_values = true)]
        api_key: Option<String>,
        /// Replay into this farm (its exact name) even though it has collars that aren't simulated.
        #[arg(long)]
        into_farm: Option<String>,
        /// Keep the captured gaps between requests.
        #[arg(long)]
        pace: bool,
        /// Capture the replay to this file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Test station: one unit after another through the test plan for its class.
    Factory {
        #[arg(long)]
        server: String,
        #[arg(long, env = "STATION_TOKEN", hide_env_values = true)]
        station_token: String,
        #[arg(long)]
        port: PathBuf,
        /// Device class: node, anchor, …
        #[arg(long = "class")]
        device_class: String,
        /// Board revision, e.g. N1.0.
        #[arg(long)]
        hw_rev: String,
        /// Station name (default: the token's).
        #[arg(long)]
        station: Option<String>,
        #[arg(long)]
        fixture: Option<String>,
        /// Test one unit with this serial and stop.
        #[arg(long)]
        serial: Option<String>,
        /// Seconds to wait for a GNSS fix.
        #[arg(long, default_value_t = 120)]
        gnss_timeout: u64,
        /// The firmware version the fixture flashes, e.g. 0.2.0. Without it fw_flash is skipped (and the run fails).
        #[arg(long = "fw")]
        expect_fw: Option<String>,
        /// Ask for readings and checks the console can't give (a meter, an ear).
        #[arg(long)]
        operator: bool,
        /// Directory for runs.jsonl, every run whatever the server says.
        #[arg(long, default_value = "factory-results")]
        results: PathBuf,
        #[arg(long)]
        capture: Option<PathBuf>,
        #[arg(short, long)]
        verbose: bool,
    },
    /// List serial ports that look like a collar.
    Ports,
}

fn capture_for(path: Option<PathBuf>, none: bool) -> anyhow::Result<Arc<Capture>> {
    if none {
        return Ok(Arc::new(Capture::none()));
    }
    let path = path.unwrap_or_else(|| PathBuf::from(format!("capture-{}.jsonl", chrono::Local::now().format("%Y%m%d-%H%M%S"))));
    Ok(Arc::new(Capture::open(&path)?))
}

fn load_link(path: &std::path::Path, collar: Option<&str>, endpoint: Option<String>) -> anyhow::Result<Link> {
    let mut link = Link::load(path, collar)?;
    if let Some(e) = endpoint {
        link.endpoint = e;
    }
    link.payload()?;
    Ok(link)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Bridge { port, server, key, provision, collar, endpoint, force, capture, no_capture, verbose } => {
            let link = provision.map(|p| load_link(&p, collar.as_deref(), endpoint)).transpose()?;
            let key = match (key, &link) {
                (Some(k), _) => k,
                (None, Some(l)) => l.key.clone(),
                (None, None) => bail!("the collar key: --key, COLLAR_KEY, or --provision with its link"),
            };
            let capture = capture_for(capture, no_capture)?;
            bridge::run(bridge::Options { port, server, key, link, force, capture, verbose }).await
        }
        Cmd::Provision { port, link, collar, endpoint, verbose } => {
            let link = load_link(&link, collar.as_deref(), endpoint)?;
            let mut c = Collar::open(&port, Arc::new(Capture::none()), verbose).with_context(|| format!("opening {}", port.display()))?;
            c.listen(Duration::from_millis(500)).await;
            bridge::provision(&mut c, &link).await.map_err(anyhow::Error::msg)?;
            match c.status().await {
                Ok(r) => say(format!("{r:?}")),
                Err(e) => say(e.to_string()),
            }
            Ok(())
        }
        Cmd::Replay { capture, server, key, api_key, into_farm, pace, out } => {
            let out = match out {
                Some(p) => Arc::new(Capture::open(&p)?),
                None => Arc::new(Capture::none()),
            };
            let target = server.zip(key);
            let dry = target.is_none();
            let sum = replay::run(replay::Options { capture, target, api_key, into_farm, pace, out }).await?;
            if dry {
                println!("{} requests (pass --server and --key to send them)", sum.requests);
            } else {
                println!("{} requests: {} got the same status, {} didn't", sum.requests, sum.same, sum.different);
            }
            Ok(())
        }
        Cmd::Factory {
            server,
            station_token,
            port,
            device_class,
            hw_rev,
            station,
            fixture,
            serial,
            gnss_timeout,
            expect_fw,
            operator,
            results,
            capture,
            verbose,
        } => {
            let capture = match capture {
                Some(p) => Arc::new(Capture::open(&p)?),
                None => Arc::new(Capture::none()),
            };
            let code = factory::run(factory::Options {
                server,
                station_token,
                port,
                device_class,
                hw_rev,
                station,
                fixture_id: fixture,
                serial,
                gnss_timeout: Duration::from_secs(gnss_timeout),
                expect_fw,
                operator,
                results,
                capture,
                verbose,
            })
            .await?;
            // Nonzero unless every unit went all the way (none can yet; see `factory`).
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Cmd::Ports => {
            let ports = serial::list();
            if ports.is_empty() {
                println!("no USB serial ports found");
            }
            for p in ports {
                println!("{}", p.display());
            }
            Ok(())
        }
    }
}
