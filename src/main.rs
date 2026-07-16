mod clipboard;
mod config;
mod daemon;
mod ipc;
mod network;
mod ordering;
mod protocol;
mod service;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
