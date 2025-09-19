use std::{env, path::PathBuf};

use clap::{Parser, Subcommand};
use log2::*;

mod auth;
mod client;
mod config;
mod deploy;
mod error;
mod server;

// Generated gRPC bindings
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

/// Resolve the config path next to the executable
fn get_default_config_path(config_name: &str) -> PathBuf {
  // Prefer the executable directory; fallback to the current directory
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
      // Initialize server logger
      std::fs::create_dir_all("./logs").ok();
      let _log = log2::open("./logs/server.log")
        .size(10 * 1024 * 1024) // 10MB per log file
        .rotate(5) // Keep 5 backup files
        .level("info") // Log level
        .tee(true) // Also output to stdout
        .start();

      // Resolve default server config
      let config_path = get_default_config_path("server_config.toml");

      // Load server configuration to get port
      let server_config = config::load_server_config(&config_path)?;
      let port = server_config.server.port;

      info!(
        "Starting ADeploy server on port {} (config: {})",
        port,
        config_path.display()
      );
      server::start_server(port, server_config).await?
    }
    Some(Commands::Client { host, package }) => {
      let _log2 = log2::start();

      // Resolve default client config
      let config_path = get_default_config_path("client_config.toml");

      // Load client configuration to get port
      let client_config = config::load_client_config(&config_path)?;
      let server_config = config::get_remote_config(&client_config, &host)
        .ok_or_else(|| "No server configuration found")?;
      let port = server_config.port;

      info!(
        "Deploying {} to {}:{} (config: {})",
        package,
        host,
        port,
        config_path.display()
      );
      client::deploy(&host, client_config, &package).await?
    }
    None => {
      let _log2 = log2::start();
      // Default client mode uses positional arguments
      if let (Some(host), Some(package)) = (cli.host, cli.package) {
        let config_path = get_default_config_path("client_config.toml");
        let client_config = config::load_client_config(&config_path)?;
        let server_config = config::get_remote_config(&client_config, &host)
          .ok_or_else(|| "No server configuration found")?;
        let port = server_config.port;

        info!(
          "Deploying {} to {}:{} (config: {})",
          package,
          host,
          port,
          config_path.display()
        );
        client::deploy(&host, client_config, &package).await?
      } else {
        error!("Host and package are required when not using subcommands");
        error!("Usage: adeploy <HOST> <PACKAGE>");
        error!("   or: adeploy client <HOST> <PACKAGE>");
        error!("   or: adeploy server");
        std::process::exit(1);
      }
    }
  }

  Ok(())
}
