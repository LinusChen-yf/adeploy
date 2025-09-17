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
}

#[derive(Subcommand)]
enum Commands {
  /// Start the deployment server
  Server {
    /// Configuration file path
    #[arg(short, long, default_value = "server_config.toml")]
    config: PathBuf,
  },
  /// Deploy to a server (explicit client mode)
  Client {
    /// Server host
    host: String,
    /// Package name
    package: String,
    /// Configuration file path
    #[arg(short, long, default_value = "client_config.toml")]
    config: PathBuf,
  },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let cli = Cli::parse();

  match cli.command {
    Some(Commands::Server { config }) => {
      // Initialize logger for server with file output
      std::fs::create_dir_all("./logs").ok();
      let _log = log2::open("./logs/server.log")
        .size(10 * 1024 * 1024) // 10MB per log file
        .rotate(5)              // Keep 5 backup files
        .level("info")          // Log level
        .tee(true)              // Also output to stdout
        .start();
      
      // Load server configuration to get port
      let server_config = config::load_server_config(&config)?;
      let port = server_config.server.port;
      
      info!("========================================");
      info!("Starting ADeploy server on port {}", port);
      info!("Configuration file: {:?}", config);
      info!("========================================");
      server::start_server(port, config).await?
    }
    Some(Commands::Client { host, package, config }) => {
      let _log2 = log2::start();
      
      // Load client configuration to get port
      let client_config = config::load_client_config(&config)?;
      let server_config = config::get_server_config(&client_config, &host).ok_or_else(|| "No server configuration found")?;
      let port = server_config.port;
      
      info!("========================================");
      info!("Deploying {} to {}:{}", package, host, port);
      info!("Configuration file: {:?}", config);
      info!("========================================");
      client::deploy(&host, config, &package).await?
    }
    None => {
      let _log2 = log2::start();
      // Default client mode - use positional arguments
      if let (Some(host), Some(package)) = (cli.host, cli.package) {
        let config_path = PathBuf::from("client_config.toml");
        let client_config = config::load_client_config(&config_path)?;
        let server_config = config::get_server_config(&client_config, &host).ok_or_else(|| "No server configuration found")?;
        let port = server_config.port;
        
        info!("========================================");
        info!("Deploying {} to {}:{}", package, host, port);
        info!("Using default configuration file: client_config.toml");
        info!("========================================");
        client::deploy(&host, config_path, &package).await?
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
