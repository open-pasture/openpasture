use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "openpasture", version, about = "Run collars, store their data, and decide where the herd goes.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server: API, collar endpoints, live feed and web UI. Stops
    /// cleanly on Ctrl-C or SIGTERM.
    Serve {
        /// Address to bind. Default: settings (127.0.0.1).
        #[arg(long)]
        bind: Option<String>,
        /// Port. Default: settings (7878).
        #[arg(long)]
        port: Option<u16>,
        /// Let the OS pick a free port.
        #[arg(long, conflicts_with = "port")]
        free_port: bool,
        /// Data directory. Default: OPENPASTURE_DATA_DIR or the platform data dir.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Print the app token. Off this machine the app and API ask for it.
    /// Reads it without disturbing a running server.
    Token {
        /// Data directory. Default: OPENPASTURE_DATA_DIR or the platform data dir.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Print a one-time sign-in URL for this machine's owner (the running
    /// server's; it works once, within 10 minutes).
    SignIn {
        /// Data directory. Default: OPENPASTURE_DATA_DIR or the platform data dir.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Ask the running server for /healthz. Exits non-zero when it doesn't
    /// answer or says it is down (for Docker's HEALTHCHECK and monitors).
    Health {
        /// Data directory whose server.json names the server. Default:
        /// OPENPASTURE_DATA_DIR or the platform data dir.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// Ask this server instead, e.g. http://127.0.0.1:7878.
        #[arg(long)]
        url: Option<String>,
    },
    /// Settings maintenance.
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// Print the version.
    Version,
}

#[derive(Subcommand)]
enum SettingsCommand {
    /// When the stored settings can't be read (the server won't start): keep
    /// every setting that can be, the app token among them, reset the rest,
    /// and keep the unreadable copy. Run with the server stopped.
    Repair {
        /// Data directory. Default: OPENPASTURE_DATA_DIR or the platform data dir.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let dir = |d: Option<PathBuf>| d.unwrap_or_else(op_core::default_data_dir);
    match cli.command {
        Command::Version => println!("openpasture {}", op_server::VERSION),
        Command::Token { data_dir } => {
            let data_dir = dir(data_dir);
            let token = match op_core::settings::read_stored(&data_dir).await? {
                Some(s) => s.server.app_token,
                // First run: create the data dir and its settings.
                None => op_core::Ctx::open(&data_dir).await?.settings().await?.server.app_token,
            };
            println!("{token}");
        }
        Command::SignIn { data_dir } => {
            let data_dir = dir(data_dir);
            let url = op_core::instance::running(&data_dir)
                .and_then(|s| s.url)
                .ok_or_else(|| anyhow::anyhow!("no openpasture server is running for {}", data_dir.display()))?;
            println!("{}", op_server::session::sign_in_url_in(&data_dir, &url, "cli").await?);
        }
        Command::Health { data_dir, url } => {
            let url = match url {
                Some(u) => u,
                None => {
                    let data_dir = dir(data_dir);
                    op_core::instance::running(&data_dir)
                        .and_then(|s| s.url)
                        .ok_or_else(|| anyhow::anyhow!("no openpasture server is running for {}", data_dir.display()))?
                }
            };
            let (status, body) = get(&format!("{}/healthz", url.trim_end_matches('/')))?;
            println!("{body}");
            if status != 200 {
                std::process::exit(1);
            }
        }
        Command::Settings { command: SettingsCommand::Repair { data_dir } } => repair_settings(&dir(data_dir)).await?,
        Command::Serve { bind, port, free_port, data_dir } => {
            match std::env::var_os(op_server::logging::LOG_DIR_ENV).filter(|d| !d.is_empty()) {
                Some(logs) => op_server::logging::init_file(Path::new(&logs), "server.log", op_server::logging::DEFAULT_FILTER),
                None => op_server::logging::init_stdout(op_server::logging::DEFAULT_FILTER),
            }
            let handle = op_server::serve(op_server::ServeOptions { data_dir, bind, port, free_port, cors: None }).await?;
            println!("openpasture {} at {}", op_server::VERSION, handle.url());
            println!("Sign in on this machine (works once, within 10 minutes; openpasture sign-in for another): {}", handle.sign_in_url("serve").await?);
            if let Some(lan) = handle.lan_url() {
                println!("LAN: {lan}");
                println!("Off this machine the app asks for its token: openpasture token");
            }
            op_server::ops::shutdown_signal().await;
            handle.shutdown().await?;
        }
    }
    Ok(())
}

async fn repair_settings(data_dir: &Path) -> anyhow::Result<()> {
    let _lock = op_core::instance::lock(data_dir)?;
    let store = op_core::Store::open(data_dir).await?;
    match op_core::settings::repair(store.pool()).await? {
        None => println!("The settings read fine; nothing to repair."),
        Some(r) => {
            println!("Settings repaired. The unreadable copy is kept under the settings key {}.", r.kept_as);
            if r.reset.is_empty() {
                println!("Every setting was kept.");
            } else {
                println!("Reset to defaults: {}", r.reset.join(", "));
            }
        }
    }
    store.pool().close().await;
    Ok(())
}

/// A plain HTTP/1.0 GET (no TLS): the status code and body.
fn get(url: &str) -> anyhow::Result<(u16, String)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| anyhow::anyhow!("only http:// URLs: {url}"))?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&host)?.next().ok_or_else(|| anyhow::anyhow!("can't resolve {host}"))?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut res = String::new();
    s.read_to_string(&mut res)?;
    let status = res.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow::anyhow!("not an HTTP reply from {url}"))?;
    let body = res.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    Ok((status, body))
}
