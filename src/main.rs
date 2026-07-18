mod clipboard;
mod config;
mod daemon;
mod gui;
mod input;
mod integration;
mod ipc;
mod network;
mod ordering;
mod protocol;
mod service;
mod transfer;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run clipboard synchronization daemon.
    Daemon,
    /// Pair with another device. Stop daemon on both devices first.
    Pair,
    /// List trusted peers.
    Peers,
    /// Forget a trusted peer.
    Unpair { peer: String },
    /// Pause synchronization without stopping daemon.
    Pause,
    /// Resume synchronization from a fresh clipboard baseline.
    Resume,
    /// Show daemon and platform status.
    Status,
    /// Open GUI to share files with a paired peer.
    Share {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        peer: Option<String>,
    },
    /// Open transfer history window.
    Transfers,
    /// Configure cursor sharing with a paired peer.
    Cursor {
        #[command(subcommand)]
        action: CursorAction,
    },
    #[command(hide = true)]
    CursorBeaconUi {
        #[arg(long)]
        edge: input::protocol::Edge,
        #[arg(long)]
        position: f64,
        #[arg(long)]
        peer: String,
    },
    #[command(hide = true)]
    TransferUi {
        #[arg(long)]
        id: Uuid,
    },
    #[command(hide = true)]
    CopyShareUi {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Install or remove Finder/Thunar sharing actions.
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Read or change device name.
    Name { name: Option<String> },
    /// Manage manual-start user service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Clone, Subcommand)]
enum ServiceAction {
    Install,
    Start,
    Stop,
    Status,
    Uninstall,
}

#[derive(Clone, Subcommand)]
enum IntegrationAction {
    Install,
    Uninstall,
}

#[derive(Clone, Subcommand)]
enum CursorAction {
    /// Enable automatic edge discovery.
    Enable,
    /// Disable cursor sharing.
    Disable,
    /// Show cursor-sharing configuration.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Daemon => daemon::run().await,
        Command::Pair => network::pair_interactive().await,
        Command::Peers => {
            let cfg = config::Config::load_or_create()?;
            if cfg.peers.is_empty() {
                println!("No paired peers.");
            } else {
                for (id, peer) in cfg.peers {
                    println!("{id}\t{}", peer.name);
                }
            }
            Ok(())
        }
        Command::Unpair { peer } => ipc::request(ipc::Request::Unpair { peer }).await,
        Command::Pause => ipc::request(ipc::Request::Pause).await,
        Command::Resume => ipc::request(ipc::Request::Resume).await,
        Command::Status => ipc::request(ipc::Request::Status).await,
        Command::Share { paths, peer } => gui::share(paths, peer),
        Command::Transfers => gui::transfers(),
        Command::Cursor { action } => configure_cursor(action).await,
        Command::CursorBeaconUi {
            edge,
            position,
            peer,
        } => input::beacon::run_ui(edge, position, peer),
        Command::TransferUi { id } => gui::receive(id),
        Command::CopyShareUi { paths } => gui::copy_prompt(paths),
        Command::Integration { action } => integration::run(action),
        Command::Name { name: None } => {
            println!("{}", config::Config::load_or_create()?.name);
            Ok(())
        }
        Command::Name { name: Some(name) } => {
            let mut cfg = config::Config::load_or_create()?;
            cfg.set_name(name)?;
            cfg.save()
        }
        Command::Service { action } => service::run(action).await,
    }
}

async fn configure_cursor(action: CursorAction) -> Result<()> {
    if !matches!(action, CursorAction::Status) && ipc::daemon_available().await {
        anyhow::bail!("daemon is running; stop it before changing cursor sharing");
    }
    let mut cfg = config::Config::load_or_create()?;
    match action {
        CursorAction::Enable => {
            cfg.cursor.enabled = true;
            cfg.save()?;
            println!("Automatic cursor discovery enabled.");
        }
        CursorAction::Disable => {
            cfg.cursor = config::CursorConfig::default();
            cfg.save()?;
            println!("Cursor sharing disabled.");
        }
        CursorAction::Status => println!(
            "{}",
            if cfg.cursor.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
    }
    Ok(())
}
