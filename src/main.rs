//! `epistole` - sovereign newsletter service.
//!
//! Boot path: parse args, load config, open the fjall keyspace, build
//! the axum router, listen on the configured bind address.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use epistole::{Config, Error, Result, Store};
use tracing_subscriber::{EnvFilter, fmt};

/// Command-line entry point for the epistole server.
#[derive(Parser, Debug)]
#[command(name = "epistole", version, about = "Sovereign newsletter service")]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, default_value = "epistole.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .json()
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let bind: SocketAddr =
        config
            .bind
            .parse()
            .map_err(|e: std::net::AddrParseError| Error::Config {
                reason: format!("invalid bind address {}: {e}", config.bind),
            })?;

    let store = Arc::new(Store::open(&config.data_dir)?);
    let app = epistole::router(store, Arc::new(config));

    tracing::info!(addr = %bind, "epistole listening");
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| Error::Bind {
            addr: bind.to_string(),
            source: e,
        })?;

    axum::serve(listener, app)
        .await
        .map_err(|e| Error::Serve { source: e })?;

    Ok(())
}
