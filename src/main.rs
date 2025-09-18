use std::{env, path::PathBuf};

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
  Server,
  /// Deploy to a server (explicit client mode)
  Client {
    /// Server host
    host: String,
    /// Package name
    package: String,
  },
}

/// Get the directory where the executable is located
fn get_executable_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
  let current_exe = env::current_exe()?;
  let current_dir = current_exe
    .parent()
    .ok_or("Failed to get parent directory")?;
  Ok(current_dir.to_path_buf())
}

/// Get the default config file path in the executable directory
fn get_default_config_path(config_name: &str) -> PathBuf {
  // Try to get executable directory, fallback to current directory if failed
  match get_executable_dir() {
    Ok(exe_dir) => exe_dir.join(config_name),
    Err(_) => PathBuf::from(config_name),
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let cli = Cli::parse();

  match cli.command {
    Some(Commands::Server) => {
      // Initialize logger for server with file output
      std::fs::create_dir_all("./logs").ok();
      let _log = log2::open("./logs/server.log")
        .size(10 * 1024 * 1024) // 10MB per log file
        .rotate(5) // Keep 5 backup files
        .level("info") // Log level
        .tee(true) // Also output to stdout
        .start();

      // Use default server config path
      let config_path = get_default_config_path("server_config.toml");

      // Load server configuration to get port
      let server_config = config::load_server_config(&config_path)?;
      let port = server_config.server.port;

      info!("========================================");
      info!("Starting ADeploy server on port {}", port);
      info!("Configuration file: {}", config_path.display());
      info!("========================================");
      server::start_server(port, server_config).await?
    }
    Some(Commands::Client { host, package }) => {
      let _log2 = log2::start();

      // Use default client config path
      let config_path = get_default_config_path("client_config.toml");

      // Load client configuration to get port
      let client_config = config::load_client_config(&config_path)?;
      let server_config = config::get_remote_config(&client_config, &host)
        .ok_or_else(|| "No server configuration found")?;
      let port = server_config.port;

      info!("========================================");
      info!("Deploying {} to {}:{}", package, host, port);
      info!("Configuration file: {}", config_path.display());
      info!("========================================");
      client::deploy(&host, client_config, &package).await?
    }
    None => {
      let _log2 = log2::start();
      // Default client mode - use positional arguments
      if let (Some(host), Some(package)) = (cli.host, cli.package) {
        let config_path = get_default_config_path("client_config.toml");
        let client_config = config::load_client_config(&config_path)?;
        let server_config = config::get_remote_config(&client_config, &host)
          .ok_or_else(|| "No server configuration found")?;
        let port = server_config.port;

        info!("========================================");
        info!("Deploying {} to {}:{}", package, host, port);
        info!(
          "Using default configuration file: {}",
          config_path.display()
        );
        info!("========================================");
        client::deploy(&host, client_config, &package).await?
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
