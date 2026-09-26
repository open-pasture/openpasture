use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "openpasture", version, about = "Run collars, store their data, and decide where the herd goes.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server: API, collar endpoints, live feed and web UI.
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
    Token {
        /// Data directory. Default: OPENPASTURE_DATA_DIR or the platform data dir.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Print the version.
    Version,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => println!("openpasture {}", op_server::VERSION),
        Command::Token { data_dir } => {
            let ctx = op_core::Ctx::open(data_dir.unwrap_or_else(op_core::default_data_dir)).await?;
            println!("{}", ctx.settings().await?.server.app_token);
        }
        Command::Serve { bind, port, free_port, data_dir } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,tower_http=info,tantivy=warn,rmcp=warn".into()),
                )
                .init();
            let handle = op_server::serve(op_server::ServeOptions { data_dir, bind, port, free_port, cors: None }).await?;
            println!("openpasture {} at {}", op_server::VERSION, handle.url());
            if let Some(lan) = handle.lan_url() {
                println!("LAN: {lan}");
                println!("Off this machine the app asks for its token: openpasture token");
            }
            tokio::signal::ctrl_c().await?;
            tracing::info!("shutting down");
            handle.shutdown().await?;
        }
    }
    Ok(())
}
