use std::{path::PathBuf, process, sync::Arc};

// The binary is a front end over the library, rather than a second copy of it.
// Declaring the modules here as well built every one of them twice and ran the
// unit tests twice, against two sets of types that only looked identical.
use adeploy::{
  client,
  config::{self, ConfigProvider},
  error::{AdeployError, Result},
  init,
  pairing::{self, PairStore},
  server,
};
use clap::{Args, Parser, Subcommand};
use log2::*;
use tokio::runtime::Builder as RuntimeBuilder;

#[derive(Parser)]
#[command(name = "adeploy")]
#[command(about = "A universal deployment tool", long_about = None)]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// Path to adeploy.toml; skips the upward search from the working directory
  #[arg(long, short = 'c', value_name = "PATH", global = true)]
  config: Option<PathBuf>,

  /// Server host (when using default client mode)
  #[arg(value_name = "HOST")]
  host: Option<String>,

  /// Package names (when using default client mode)
  #[arg(value_name = "PACKAGE", num_args = 0..)]
  packages: Vec<String>,

  /// Show what would be packaged and sent, without contacting the server
  #[arg(long)]
  dry_run: bool,
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
    /// Show what would be packaged and sent, without contacting the server
    #[arg(long)]
    dry_run: bool,
  },
  /// Show the packages and remotes this project declares
  List,
  /// Write a starter adeploy.toml into the current directory
  Init {
    /// Overwrite an existing adeploy.toml
    #[arg(long)]
    force: bool,
  },
  /// Ask a server to trust this machine's key, and record which server it is
  Pair {
    /// Server host
    host: String,
    /// Accept an identity that differs from the one on file, for a server that
    /// was rebuilt or replaced
    #[arg(long)]
    force: bool,
    /// Return as soon as the request is queued instead of waiting for someone
    /// to approve it
    #[arg(long)]
    no_wait: bool,
  },
  /// Put a previous deployment back
  Rollback {
    /// Server host
    host: String,
    /// Package to roll back
    package: String,
    /// Show the available snapshots instead of restoring one
    #[arg(long)]
    list: bool,
    /// Snapshot to restore; defaults to the most recent
    #[arg(long, value_name = "NAME")]
    to: Option<String>,
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
  /// List clients waiting to be approved
  Pending,
  /// Approve a waiting client, by list position or fingerprint
  Approve(PairSelectorArgs),
  /// Refuse a waiting client
  Reject(PairSelectorArgs),
  /// List the clients this server trusts
  Keys,
  /// Withdraw approval from a client
  Revoke(PairSelectorArgs),
}

#[derive(Args, Clone)]
struct PairSelectorArgs {
  /// Position in the list, or a fingerprint (a unique prefix is enough)
  selector: String,
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
  let mut log_handle = initialize_logging(&cli);
  let code = match run_cli(cli) {
    Ok(()) => 0,
    Err(err) => {
      error!("{err}");
      1
    }
  };
  // log2 hands records to a writer thread, and `process::exit` skips destructors,
  // so drain the queue explicitly instead of relying on `Handle::drop`.
  log_handle.stop();
  process::exit(code);
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
    config: config_override,
    host: default_host,
    packages: default_packages,
    dry_run: default_dry_run,
  } = cli;

  match command {
    Some(Commands::Server { action }) => {
      let action = action.unwrap_or(ServerAction::Run(ServiceRunArgs::default()));
      handle_server(action, config_override)?;
    }
    Some(Commands::Client {
      host,
      packages,
      dry_run,
    }) => {
      let runtime = build_runtime()?;
      runtime.block_on(run_client_mode(&host, packages, dry_run, config_override))?;
    }
    Some(Commands::List) => {
      let provider = config::ConfigProviderImpl::with_override(config_override);
      client::list(&provider)?;
    }
    Some(Commands::Init { force }) => {
      init::init_project_config(force)?;
    }
    Some(Commands::Pair {
      host,
      force,
      no_wait,
    }) => {
      let runtime = build_runtime()?;
      runtime.block_on(run_pair_mode(&host, force, !no_wait, config_override))?;
    }
    Some(Commands::Rollback {
      host,
      package,
      list,
      to,
    }) => {
      let runtime = build_runtime()?;
      runtime.block_on(run_rollback_mode(
        &host,
        &package,
        list,
        to,
        config_override,
      ))?;
    }
    None => {
      let Some(host) = default_host else {
        return Err(usage_error("Host is required when not using subcommands"));
      };
      if default_packages.is_empty() {
        return Err(usage_error(
          "At least one package is required when not using subcommands",
        ));
      }

      let runtime = build_runtime()?;
      runtime.block_on(run_client_mode(
        &host,
        default_packages,
        default_dry_run,
        config_override,
      ))?;
    }
  }

  Ok(())
}

async fn run_pair_mode(
  host: &str,
  force: bool,
  wait: bool,
  config_override: Option<PathBuf>,
) -> Result<()> {
  let provider: Arc<dyn config::ConfigProvider> =
    Arc::new(config::ConfigProviderImpl::with_override(config_override));

  client::pair(host, force, wait, provider.as_ref()).await
}

async fn run_rollback_mode(
  host: &str,
  package: &str,
  list: bool,
  to: Option<String>,
  config_override: Option<PathBuf>,
) -> Result<()> {
  let provider: Arc<dyn config::ConfigProvider> =
    Arc::new(config::ConfigProviderImpl::with_override(config_override));

  if list {
    client::list_backups(host, package, provider.as_ref()).await
  } else {
    client::rollback(host, package, to, provider.as_ref()).await
  }
}

async fn run_client_mode(
  host: &str,
  packages: Vec<String>,
  dry_run: bool,
  config_override: Option<PathBuf>,
) -> Result<()> {
  let provider: Arc<dyn config::ConfigProvider> =
    Arc::new(config::ConfigProviderImpl::with_override(config_override));

  if dry_run {
    client::dry_run(host, Some(packages), provider.as_ref()).await
  } else {
    client::deploy(host, Some(packages), provider.as_ref()).await
  }
}

fn usage_error(message: &str) -> Box<AdeployError> {
  Box::new(AdeployError::Config(format!(
    "{message}\n\
     Usage: adeploy <HOST> <PACKAGE> [PACKAGE...]\n\
     \x20  or: adeploy client <HOST> <PACKAGE> [PACKAGE...]\n\
     \x20  or: adeploy server [run|install|start|stop|status|uninstall]\n\
     \x20  or: adeploy server [pending|approve|reject|keys|revoke]\n\
     \x20  or: adeploy pair <HOST> [--force] [--no-wait]\n\
     \x20  or: adeploy rollback <HOST> <PACKAGE> [--list] [--to NAME]\n\
     \x20  or: adeploy list\n\
     \x20  or: adeploy init"
  )))
}

/// Where this server keeps its approvals: beside its configuration.
fn pair_store_path(config_override: Option<PathBuf>) -> Result<PathBuf> {
  let config_path = config::ConfigProviderImpl::for_server(config_override).get_config_path()?;
  Ok(
    config_path
      .parent()
      .map(|parent| parent.join(pairing::PAIRED_FILE_NAME))
      .unwrap_or_else(|| PathBuf::from(pairing::PAIRED_FILE_NAME)),
  )
}

fn load_pair_store(config_override: Option<PathBuf>) -> Result<PairStore> {
  PairStore::load(&pair_store_path(config_override)?)
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

fn handle_server(action: ServerAction, config_override: Option<PathBuf>) -> Result<()> {
  match action {
    ServerAction::Run(opts) => {
      let provider: Arc<dyn config::ConfigProvider> =
        Arc::new(config::ConfigProviderImpl::for_server(config_override));
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
      // Generate the configuration before registering the service, so the
      // service starts into a usable state instead of failing on a missing file.
      let config_path =
        config::ConfigProviderImpl::for_server(config_override).get_config_path()?;
      init::ensure_server_config(&config_path)?;

      server::install_service(
        &opts.label,
        opts.user,
        !opts.no_autostart,
        opts.disable_restart_on_failure,
        opts.working_directory.clone(),
        opts.username.clone(),
      )?;
      info!(
        "Installed ADeploy service '{}' at {} level",
        opts.label,
        if opts.user { "user" } else { "system" }
      );
    }
    ServerAction::Uninstall(opts) => {
      server::uninstall_service(&opts.label, opts.user)?;
      info!(
        "Uninstalled ADeploy service '{}' at {} level",
        opts.label,
        if opts.user { "user" } else { "system" }
      );
    }
    ServerAction::Start(opts) => {
      server::start_service(&opts.label, opts.user)?;
      info!(
        "Started ADeploy service '{}' at {} level",
        opts.label,
        if opts.user { "user" } else { "system" }
      );
    }
    ServerAction::Stop(opts) => {
      server::stop_service(&opts.label, opts.user)?;
      info!(
        "Stopped ADeploy service '{}' at {} level",
        opts.label,
        if opts.user { "user" } else { "system" }
      );
    }
    ServerAction::Pending => {
      let store = load_pair_store(config_override)?;
      if store.pending.is_empty() {
        info!("No clients are waiting for approval");
      } else {
        info!("Clients waiting for approval:");
        for (position, client) in store.pending.iter().enumerate() {
          info!("  {}. {}", position + 1, client.describe());
        }
        info!("Approve one with `adeploy server approve <number|fingerprint>`");
      }
    }
    ServerAction::Approve(opts) => {
      let path = pair_store_path(config_override)?;
      let mut store = PairStore::load(&path)?;
      let client = store.approve(&opts.selector)?;
      store.save(&path)?;
      info!("Approved {}", client.describe());
      info!("It can deploy now; the server picks this up without a restart");
    }
    ServerAction::Reject(opts) => {
      let path = pair_store_path(config_override)?;
      let mut store = PairStore::load(&path)?;
      let client = store.reject(&opts.selector)?;
      store.save(&path)?;
      info!("Rejected {}", client.describe());
    }
    ServerAction::Keys => {
      let store = load_pair_store(config_override)?;
      if store.approved.is_empty() {
        info!("No clients have been approved through pairing");
      } else {
        info!("Approved clients:");
        for (position, client) in store.approved.iter().enumerate() {
          info!("  {}. {}", position + 1, client.describe());
        }
      }
    }
    ServerAction::Revoke(opts) => {
      let path = pair_store_path(config_override)?;
      let mut store = PairStore::load(&path)?;
      let client = store.revoke(&opts.selector)?;
      store.save(&path)?;
      info!("Revoked {}", client.describe());
    }
    ServerAction::Status(opts) => {
      let status = server::service_status(&opts.label, opts.user)?;

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
