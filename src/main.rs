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

#[tokio::main]
async fn main() {
  let cli = Cli::parse();
  let Cli {
    command,
    host: default_host,
    package: default_package,
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

      if let Err(e) = server::start_server_with_default_config().await {
        error!("{}", e);
        std::process::exit(1);
      }
    }
    Some(Commands::Client { host, package }) => {
      let _log2 = log2::start();
      if let Err(e) = client::deploy_with_default_config(&host, &package).await {
        error!("{}", e);
        std::process::exit(1);
      }
    }
    None => {
      let _log2 = log2::start();
      match (default_host, default_package) {
        (Some(host), Some(package)) => {
          if let Err(e) = client::deploy_with_default_config(&host, &package).await {
            error!("{}", e);
            std::process::exit(1);
          }
        }
        _ => {
          let message = "Host and package are required when not using subcommands";
          error!("{message}");
          error!("Usage: adeploy <HOST> <PACKAGE>");
          error!("   or: adeploy client <HOST> <PACKAGE>");
          error!("   or: adeploy server");
          std::process::exit(1);
        }
      }
    }
  };
}
