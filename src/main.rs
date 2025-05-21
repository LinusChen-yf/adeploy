use std::{env, path::PathBuf};

use anyhow::bail;
use clap::{Parser, Subcommand};
use log2::*;

mod client;
mod rhai_utils;
mod server;

#[derive(Parser)]
struct CliArgs {
  /// IP address for client mode.
  #[clap(
      index = 1, // This is the first positional argument
  )]
  ip: Option<String>,

  /// Path to the file to deploy in client mode.
  /// Default: deploy.rhai
  #[clap(
      index = 2, // This is the second positional argument
  )]
  script: Option<String>,

  /// Specifies the mode of operation.
  #[clap(subcommand)]
  command: Option<AppMode>,
}

#[derive(Subcommand)]
enum AppMode {
  /// Run in server mode. Does not accept IP or file path.
  #[clap(name = "server")] // Explicitly name the subcommand "server"
  Server,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  let _log2 = log2::stdout().module_with_line(true).level("info").start();
  let args = CliArgs::parse();

  match args.command {
    Some(AppMode::Server) => {
      info!("running server mode");
      if let Err(e) = server::run_server().await {
        error!("Server error: {}", e);
      }
    }
    None => {
      let ip = args.ip.expect("IP address is required for client mode");
      let script_path_str = match args.script {
        Some(path) => path,
        None => {
          let current_path = env::current_dir().expect("Failed to get current directory");
          let script_file = current_path.join("deploy.rhai");
          if !script_file.exists() {
            bail!("Missing deploy.rhai script in the current directory.")
          }
          script_file.to_string_lossy().into_owned()
        }
      };

      info!("Using script path: {}", script_path_str);
      let script_path = PathBuf::from(script_path_str.clone());
      let engine = rhai::Engine::new();
      let ast = match engine.compile_file(script_path.clone()) {
        Ok(ast) => ast,
        Err(e) => {
          bail!("Failed to compile Rhai script: {}", e);
        }
      };

      match rhai_utils::parse_source_path(&engine, &ast) {
        Ok(source_path) => {
          if let Err(e) = client::run_client(
            &ip,
            &script_path, // Pass the script path
            &source_path, // Pass the source program path
          )
          .await
          {
            error!("Client error: {}", e);
          }
        }
        Err(e) => {
          bail!("Error processing Rhai script: {}", e);
        }
      }
    }
  }
  Ok(())
}
