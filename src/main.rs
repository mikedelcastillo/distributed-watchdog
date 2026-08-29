mod cluster;
mod config;
mod http;
mod metrics;
mod models;
mod power;
mod screenshot;
mod speedtest;
mod telegram;
mod updater;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::signal;
use tracing::{error, info, warn};

use crate::{cluster::AppState, config::Config};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve,
    Metrics,
    Screenshot {
        #[arg(short, long, default_value = "screenshot.png")]
        output: PathBuf,
    },
    ConfigCheck,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "distributed_watchdog=info,tower_http=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)
        .with_context(|| format!("failed to load config {}", cli.config.display()))?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::Metrics => {
            let mut collector = metrics::MetricsCollector::new();
            let metrics = collector.collect(&config.node.id).await?;
            println!("{}", serde_json::to_string_pretty(&metrics)?);
            Ok(())
        }
        Command::Screenshot { output } => {
            let capture = screenshot::capture().await?;
            tokio::fs::write(&output, capture.bytes)
                .await
                .with_context(|| format!("failed to write {}", output.display()))?;
            println!("wrote {}", output.display());
            Ok(())
        }
        Command::ConfigCheck => {
            config.validate()?;
            println!("config ok: node {}", config.node.id);
            Ok(())
        }
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    config.validate()?;

    if config.telegram_token().is_none() {
        warn!(
            "telegram bot token is not set; API and peer election will run without Telegram handling"
        );
    }

    let bind = config.node.bind_addr()?;
    let state = Arc::new(AppState::new(config).context("failed to initialize state")?);

    if let Err(err) = state.refresh_self().await {
        warn!(?err, "failed to refresh local metrics during startup");
    }
    if let Err(err) = state.poll_peers().await {
        warn!(?err, "failed to poll peers during startup");
    }
    state.recompute_leader().await;

    let peer_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            if let Err(err) = peer_state.refresh_self().await {
                warn!(?err, "failed to refresh local metrics");
            }
            if let Err(err) = peer_state.poll_peers().await {
                warn!(?err, "failed to poll peers");
            }
            peer_state.recompute_leader().await;
            tokio::time::sleep(Duration::from_secs(
                peer_state.config.cluster.heartbeat_interval_seconds,
            ))
            .await;
        }
    });

    let telegram_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            match telegram::run(Arc::clone(&telegram_state)).await {
                Ok(()) => break,
                Err(err) => {
                    error!(?err, "telegram loop exited; restarting");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    info!(%bind, "starting distributed-watchdog");
    let server = http::serve(state, bind);

    tokio::select! {
        result = server => result,
        _ = shutdown_signal() => {
            info!("shutdown signal received");
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
