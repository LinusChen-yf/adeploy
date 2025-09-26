use std::sync::Arc;

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

  /// Package names (when using default client mode)
  #[arg(value_name = "PACKAGE", num_args = 0..)]
  packages: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the deployment server
  Server,
  /// Deploy to a server (explicit client mode)
  Client {
    /// Server host
    host: String,
    /// Package names to deploy
    #[arg(value_name = "PACKAGE", num_args = 1..)]
    packages: Vec<String>,
  },
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();
  let Cli {
    command,
    host: default_host,
    packages: default_packages,
  } = cli;

  match command {
    Some(Commands::Server) => {
      std::fs::create_dir_all("./logs").ok();
      let _log = log2::open("./logs/server.log")
        .size(10 * 1024 * 1024) // 10MB per log file
        .rotate(5) // Keep 5 backup files
        .level("info") // Log level
        .tee(true) // Also output to stdout
        .start();

      let provider: Arc<dyn config::ConfigProvider> = Arc::new(config::ConfigProviderImpl);
      if let Err(e) = server::start_server(provider.clone()).await {
        error!("{}", e);
        std::process::exit(1);
      }
    }
    Some(Commands::Client { host, packages }) => {
      let _log2 = log2::start();
      run_client_mode(&host, packages).await;
    }
    None => {
      let _log2 = log2::start();
      let host = default_host
        .unwrap_or_else(|| usage_and_exit("Host is required when not using subcommands"));
      if default_packages.is_empty() {
        usage_and_exit("At least one package is required when not using subcommands");
      }

      run_client_mode(&host, default_packages).await;
    }
  };
}

async fn run_client_mode(host: &str, packages: Vec<String>) {
  let provider: Arc<dyn config::ConfigProvider> = Arc::new(config::ConfigProviderImpl);

  if let Err(e) = client::deploy(host, Some(packages), provider.as_ref()).await {
    error!("{}", e);
    std::process::exit(1);
  }
}

fn usage_and_exit(message: &str) -> ! {
  error!("{message}");
  error!("Usage: adeploy <HOST> <PACKAGE> [PACKAGE...]");
  error!("   or: adeploy client <HOST> <PACKAGE> [PACKAGE...]");
  error!("   or: adeploy server");
  std::process::exit(1);
}
