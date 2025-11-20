use std::{path::PathBuf, process, sync::Arc};

use clap::{Args, Parser, Subcommand};
use log2::*;
use tokio::runtime::Builder as RuntimeBuilder;

mod auth;
mod client;
mod config;
mod deploy;
mod deploy_log;
mod error;
mod server;
use crate::error::{AdeployError, Result};

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

const DEFAULT_SERVICE_LABEL: &str = "adeploy";

#[derive(Subcommand)]
enum Commands {
  /// Manage the deployment server
  Server {
    #[command(subcommand)]
    action: Option<ServerAction>,
  },
  /// Deploy to a server (explicit client mode)
  Client {
    /// Server host
    host: String,
    /// Package names to deploy
    #[arg(value_name = "PACKAGE", num_args = 1..)]
    packages: Vec<String>,
  },
}

#[derive(Subcommand)]
enum ServerAction {
  /// Run the server in the foreground (default)
  Run(ServiceRunArgs),
  /// Install the server as a service
  Install(ServiceInstallArgs),
  /// Uninstall the server service
  Uninstall(ServiceTargetArgs),
  /// Start the installed server service
  Start(ServiceTargetArgs),
  /// Stop the running server service
  Stop(ServiceTargetArgs),
  /// Show the current service status
  Status(ServiceTargetArgs),
}

#[derive(Args, Clone, Default)]
struct ServiceRunArgs {
  /// Internal: service identifier when running under a supervisor
  #[arg(long, hide = true)]
  service_label: Option<String>,
}

#[derive(Args, Clone)]
struct ServiceInstallArgs {
  /// Service label / identifier
  #[arg(long, default_value = DEFAULT_SERVICE_LABEL)]
  label: String,
  /// Install as a per-user service instead of system-wide
  #[arg(long)]
  user: bool,
  /// Disable automatic restart if the service fails
  #[arg(long)]
  disable_restart_on_failure: bool,
  /// Do not start the service automatically on boot
  #[arg(long)]
  no_autostart: bool,
  /// Working directory used by the service
  #[arg(long, value_name = "PATH")]
  working_directory: Option<PathBuf>,
  /// Run the service under a specific username (platform-specific)
  #[arg(long)]
  username: Option<String>,
}

#[derive(Args, Clone)]
struct ServiceTargetArgs {
  /// Service label / identifier
  #[arg(long, default_value = DEFAULT_SERVICE_LABEL)]
  label: String,
  /// Target a per-user service instead of system-wide
  #[arg(long)]
  user: bool,
}

fn main() {
  let cli = Cli::parse();
  let _log_handle = initialize_logging(&cli);
  if let Err(err) = run_cli(cli) {
    error!("{err}");
    process::exit(1);
  }
}

fn initialize_logging(cli: &Cli) -> log2::Handle {
  match &cli.command {
    Some(Commands::Server { action }) => match action.as_ref() {
      Some(ServerAction::Run(_)) | None => server::init_server_logging(),
      _ => log2::stdout().level("info").start(),
    },
    _ => log2::stdout().level("info").start(),
  }
}

fn run_cli(cli: Cli) -> Result<()> {
  let Cli {
    command,
    host: default_host,
    packages: default_packages,
  } = cli;

  match command {
    Some(Commands::Server { action }) => {
      let action = action.unwrap_or(ServerAction::Run(ServiceRunArgs::default()));
      handle_server(action)?;
    }
    Some(Commands::Client { host, packages }) => {
      let runtime = build_runtime()?;
      runtime.block_on(run_client_mode(&host, packages));
    }
    None => {
      let host = default_host
        .unwrap_or_else(|| usage_and_exit("Host is required when not using subcommands"));
      if default_packages.is_empty() {
        usage_and_exit("At least one package is required when not using subcommands");
      }

      let runtime = build_runtime()?;
      runtime.block_on(run_client_mode(&host, default_packages));
    }
  }

  Ok(())
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
  error!("   or: adeploy server [run|install|start|stop|status|uninstall]");
  std::process::exit(1);
}

fn build_runtime() -> Result<tokio::runtime::Runtime> {
  RuntimeBuilder::new_multi_thread()
    .enable_all()
    .build()
    .map_err(|err| {
      Box::new(AdeployError::Service(format!(
        "Failed to initialize runtime: {err}"
      )))
    })
}

fn handle_server(action: ServerAction) -> Result<()> {
  match action {
    ServerAction::Run(opts) => {
      let provider: Arc<dyn config::ConfigProvider> = Arc::new(config::ConfigProviderImpl);
      #[cfg(windows)]
      {
        let service_name = opts
          .service_label
          .as_deref()
          .unwrap_or(DEFAULT_SERVICE_LABEL);
        if server::try_run_windows_service(provider.clone(), service_name)? {
          return Ok(());
        }
      }
      #[cfg(not(windows))]
      let _ = &opts;

      let runtime = build_runtime()?;
      runtime.block_on(server::start_server(provider))?;
    }
    ServerAction::Install(opts) => {
      if let Err(e) = server::install_service(
        &opts.label,
        opts.user,
        !opts.no_autostart,
        opts.disable_restart_on_failure,
        opts.working_directory.clone(),
        opts.username.clone(),
      ) {
        error!("{e}");
        process::exit(1);
      } else {
        info!(
          "Installed ADeploy service '{}' at {} level",
          opts.label,
          if opts.user { "user" } else { "system" }
        );
      }
    }
    ServerAction::Uninstall(opts) => {
      if let Err(e) = server::uninstall_service(&opts.label, opts.user) {
        error!("{e}");
        process::exit(1);
      } else {
        info!(
          "Uninstalled ADeploy service '{}' at {} level",
          opts.label,
          if opts.user { "user" } else { "system" }
        );
      }
    }
    ServerAction::Start(opts) => {
      if let Err(e) = server::start_service(&opts.label, opts.user) {
        error!("{e}");
        process::exit(1);
      } else {
        info!(
          "Started ADeploy service '{}' at {} level",
          opts.label,
          if opts.user { "user" } else { "system" }
        );
      }
    }
    ServerAction::Stop(opts) => {
      if let Err(e) = server::stop_service(&opts.label, opts.user) {
        error!("{e}");
        process::exit(1);
      } else {
        info!(
          "Stopped ADeploy service '{}' at {} level",
          opts.label,
          if opts.user { "user" } else { "system" }
        );
      }
    }
    ServerAction::Status(opts) => {
      let status = match server::service_status(&opts.label, opts.user) {
        Ok(status) => status,
        Err(e) => {
          error!("{e}");
          process::exit(1);
        }
      };

      info!(
        "Service '{}'(level: {}) status: {}",
        opts.label,
        if opts.user { "user" } else { "system" },
        server::format_service_status(&status)
      );
    }
  }

  Ok(())
}
