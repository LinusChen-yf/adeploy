use std::path::PathBuf;

use clap::{Parser, Subcommand};
use log2::*;

mod auth;
mod client;
mod config;
mod deploy;
mod error;
mod server;

// Include the generated gRPC code
pub mod adeploy {
  tonic::include_proto!("adeploy");
}

#[derive(Parser)]
#[command(name = "adeploy")]
#[command(about = "A universal deployment tool", long_about = None)]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the deployment server
  Server {
    /// Port to listen on
    #[arg(short, long, default_value_t = 6060)]
    port: u16,
    /// Configuration file path
    #[arg(short, long, default_value = "config.rhai")]
    config: PathBuf,
    /// Run as daemon
    #[arg(short, long)]
    daemon: bool,
  },
  /// Deploy to a server
  Deploy {
    /// Server host
    host: String,
    /// Server port
    #[arg(short, long, default_value_t = 6060)]
    port: u16,
    /// Configuration file path
    #[arg(short, long, default_value = "adeploy.rhai")]
    config: PathBuf,
  },
  /// Check deployment status
  Status {
    /// Server host
    host: String,
    /// Server port
    #[arg(short, long, default_value_t = 6060)]
    port: u16,
    /// Deploy ID
    deploy_id: String,
  },
  /// List packages on server
  List {
    /// Server host
    host: String,
    /// Server port
    #[arg(short, long, default_value_t = 6060)]
    port: u16,
  },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  // Initialize logger
  let _log2 = log2::start();

  let cli = Cli::parse();

  match cli.command {
    Commands::Server {
      port,
      config,
      daemon,
    } => {
      info!("Starting ADeploy server on port {}", port);
      server::start_server(port, config, daemon).await?
    }
    Commands::Deploy { host, port, config } => {
      info!("Deploying to {}:{}", host, port);
      client::deploy(&host, port, config).await?
    }
    Commands::Status {
      host,
      port,
      deploy_id,
    } => {
      info!("Checking status for deploy ID: {}", deploy_id);
      client::check_status(&host, port, &deploy_id).await?
    }
    Commands::List { host, port } => {
      info!("Listing packages on {}:{}", host, port);
      client::list_packages(&host, port).await?
    }
  }

  Ok(())
}
