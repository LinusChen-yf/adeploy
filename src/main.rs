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
  command: Option<Commands>,
  
  /// Server host (when using default client mode)
  #[arg(value_name = "HOST")]
  host: Option<String>,
  
  /// Package name (when using default client mode)
  #[arg(value_name = "PACKAGE")]
  package: Option<String>,
  
  /// Server port
  #[arg(short, long, default_value_t = 6060)]
  port: u16,
  
  /// Configuration file path
  #[arg(short, long, default_value = "adeploy.toml")]
  config: PathBuf,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the deployment server
  Server {
    /// Port to listen on
    #[arg(short, long, default_value_t = 6060)]
    port: u16,
    /// Configuration file path
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
  },
  /// Deploy to a server (explicit client mode)
  Client {
    /// Server host
    host: String,
    /// Package name
    package: String,
    /// Server port
    #[arg(short, long, default_value_t = 6060)]
    port: u16,
    /// Configuration file path
    #[arg(short, long, default_value = "adeploy.toml")]
    config: PathBuf,
  },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  // Initialize logger

  let cli = Cli::parse();

  match cli.command {
    Some(Commands::Server { port, config }) => {
      // Initialize logger for server with file output
      std::fs::create_dir_all("./logs").ok();
      let _log = log2::open("./logs/server.log")
        .size(10 * 1024 * 1024)
        .rotate(5)
        .level("info")
        .tee(true)
        .start();
      info!("Starting ADeploy server on port {}", port);
      server::start_server(port, config).await?
    }
    Some(Commands::Client { host, package, port, config }) => {
      let _log2 = log2::start();
      info!("Deploying {} to {}:{}", package, host, port);
      client::deploy(&host, port, config, &package).await?
    }
    None => {
      let _log2 = log2::start();
      // Default client mode - use positional arguments
      if let (Some(host), Some(package)) = (cli.host, cli.package) {
        info!("Deploying {} to {}:{}", package, host, cli.port);
        client::deploy(&host, cli.port, cli.config, &package).await?
      } else {
        error!("Error: Host and package are required when not using subcommands");
        error!("Usage: adeploy <HOST> <PACKAGE>");
        error!("   or: adeploy client <HOST> <PACKAGE>");
        error!("   or: adeploy server");
        std::process::exit(1);
      }
    }
  }

  Ok(())
}
