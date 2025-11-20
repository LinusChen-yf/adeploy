use std::{
  convert::TryInto, env, ffi::OsString, future::Future, path::PathBuf, sync::Arc, time::Duration,
};

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use service_manager::{
  ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx, ServiceStatus,
  ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};
use tokio::sync::{watch, RwLock};
use tonic::{transport::Server, Request, Response, Status};

use crate::{
  adeploy::{
    deploy_service_server::{DeployService, DeployServiceServer},
    DeployRequest, DeployResponse,
  },
  auth::Auth,
  config::{ConfigProvider, ConfigType, ServerConfig},
  deploy::DeployManager,
  deploy_log::{DeployLogEntry, LogLevel},
  error::{AdeployError, Result},
};

const DEFAULT_MAX_MESSAGE_SIZE: u64 = 100 * 1024 * 1024;

/// ADeploy gRPC service implementation
#[derive(Clone)]
pub struct AdeployService {
  config: Arc<RwLock<ServerConfig>>,
}

impl AdeployService {
  pub fn new(config: Arc<RwLock<ServerConfig>>) -> Self {
    Self { config }
  }
}

#[tonic::async_trait]
impl DeployService for AdeployService {
  async fn deploy(
    &self,
    request: Request<DeployRequest>,
  ) -> std::result::Result<Response<DeployResponse>, Status> {
    let mut req = request.into_inner();

    info!("Received deploy request for {}", req.package_name);

    // Verify signature against allowlist
    let signature = match general_purpose::STANDARD.decode(&req.signature) {
      Ok(sig) => sig,
      Err(e) => {
        error!("Invalid signature format: {}", e);
        return Err(Status::invalid_argument(format!(
          "Invalid signature: {}",
          e
        )));
      }
    };

    let (allowed_keys, package_config, max_file_size) = {
      let config = self.config.read().await;
      let allowed_keys = config.server.allowed_keys.clone();
      let package_config = config.packages.get(&req.package_name).cloned();
      (allowed_keys, package_config, config.server.max_file_size)
    };

    // Ensure the provided public key is allowed
    let is_allowed = allowed_keys
      .iter()
      .any(|allowed_key| allowed_key == &req.public_key);

    if !is_allowed {
      error!("Public key not allowed for {}", req.package_name);
      return Err(Status::unauthenticated("Client public key not allowed"));
    }

    match Auth::verify_signature(&req.public_key, &req.file_data, &signature) {
      Ok(valid) => {
        if !valid {
          error!("Signature verification failed for {}", req.package_name);
          return Err(Status::unauthenticated("Invalid Ed25519 signature"));
        }
      }
      Err(e) => {
        error!("Ed25519 signature verification error: {}", e);
        return Err(Status::unauthenticated(format!("Auth error: {}", e)));
      }
    }

    // Ensure package configuration exists
    let package_config = match package_config {
      Some(config) => config,
      None => {
        error!("Package {} is not configured", req.package_name);
        return Err(Status::not_found(format!(
          "Package '{}' not configured",
          req.package_name
        )));
      }
    };

    if max_file_size > 0 && req.file_data.len() as u64 > max_file_size {
      error!(
        "Payload for {} exceeds configured max_file_size {}",
        req.package_name, max_file_size
      );
      return Err(Status::resource_exhausted(format!(
        "Archive size exceeds configured max_file_size ({} bytes)",
        max_file_size
      )));
    }

    let package_name = req.package_name.clone();
    let file_hash = req.file_hash.clone();
    let file_data = std::mem::take(&mut req.file_data);

    // Initialize deployment manager
    let deploy_manager = DeployManager::new();
    let deploy_id = deploy_manager.deploy_id.clone();

    info!("Starting deployment {} for {}", deploy_id, package_name);

    // Execute deployment synchronously for now
    // TODO: Implement proper async deployment with Send-safe types
    match Self::execute_deployment(
      &deploy_manager,
      &package_config,
      file_data,
      file_hash,
      &package_name,
    )
    .await
    {
      Ok(logs) => {
        info!("Deployment {} completed for {}", deploy_id, package_name);

        Ok(Response::new(DeployResponse {
          success: true,
          message: "Deployment completed successfully".to_string(),
          deploy_id,
          logs: Self::encode_logs(logs),
        }))
      }
      Err(e) => {
        error!(
          "Deployment {} failed for {}: {}",
          deploy_id, package_name, e
        );

        // Always collect logs on failure
        let mut logs = vec![DeployLogEntry::error(format!("Deployment failed: {}", e))];

        // Include additional details when available
        if let AdeployError::Deploy(msg) = e.as_ref() {
          logs.push(DeployLogEntry::error(format!("Details: {}", msg)));
        }

        Ok(Response::new(DeployResponse {
          success: false,
          message: e.to_string(),
          deploy_id,
          logs: Self::encode_logs(logs),
        }))
      }
    }
  }
}

impl AdeployService {
  fn encode_logs(logs: Vec<DeployLogEntry>) -> Vec<crate::adeploy::DeployLog> {
    logs
      .into_iter()
      .map(|entry| crate::adeploy::DeployLog {
        level: Self::map_log_level(entry.level) as i32,
        message: entry.message,
      })
      .collect()
  }

  fn map_log_level(level: LogLevel) -> crate::adeploy::deploy_log::Level {
    match level {
      LogLevel::Info => crate::adeploy::deploy_log::Level::Info,
      LogLevel::Warn => crate::adeploy::deploy_log::Level::Warn,
      LogLevel::Error => crate::adeploy::deploy_log::Level::Error,
    }
  }

  async fn execute_deployment(
    deploy_manager: &DeployManager,
    package_config: &crate::config::ServerPackageConfig,
    file_data: Vec<u8>,
    file_hash: String,
    package_name: &str,
  ) -> Result<Vec<DeployLogEntry>> {
    let mut logs = Vec::new();
    logs.push(DeployLogEntry::info(format!(
      "[{}] Starting deployment execution",
      deploy_manager.deploy_id
    )));

    // Run before-deploy hook
    logs.push(DeployLogEntry::info("Running Before-deploy script..."));
    match deploy_manager
      .execute_before_deploy_script(package_config)
      .await
    {
      Ok(pre_logs) => {
        logs.extend(pre_logs);
        logs.push(DeployLogEntry::info("Before-deploy script succeeded"));
      }
      Err(e) => {
        error!("Before-deploy script failed: {}", e);
        logs.push(DeployLogEntry::error(format!(
          "Before-deploy script failed: {}",
          e
        )));
        return Err(e);
      }
    }

    // Extract archive and verify hash
    logs.push(DeployLogEntry::info("Extracting files..."));
    match deploy_manager
      .extract_files(file_data, &file_hash, package_config, package_name)
      .await
    {
      Ok(()) => {
        logs.push(DeployLogEntry::info(
          "Files extracted and deployed successfully",
        ));
      }
      Err(e) => {
        error!("File extraction failed: {}", e);
        logs.push(DeployLogEntry::error(format!(
          "File extraction failed: {}",
          e
        )));
        return Err(e);
      }
    }

    // Run after-deploy hook
    logs.push(DeployLogEntry::info("Running After-deploy script..."));
    match deploy_manager
      .execute_after_deploy_script(package_config)
      .await
    {
      Ok(post_logs) => {
        logs.extend(post_logs);
        logs.push(DeployLogEntry::info("After-deploy script succeeded"));
      }
      Err(e) => {
        error!("After-deploy script failed: {}", e);
        logs.push(DeployLogEntry::error(format!(
          "After-deploy script failed: {}",
          e
        )));
        // Deployment succeeds even if the After-deploy script fails
      }
    }

    logs.push(DeployLogEntry::info(format!(
      "[{}] Deployment completed successfully",
      deploy_manager.deploy_id
    )));
    Ok(logs)
  }
}

pub async fn start_server(provider: Arc<dyn ConfigProvider>) -> Result<()> {
  start_server_with_shutdown(provider, std::future::pending()).await
}

pub async fn start_server_with_shutdown<F>(
  provider: Arc<dyn ConfigProvider>,
  shutdown: F,
) -> Result<()>
where
  F: Future<Output = ()> + Send + 'static,
{
  let config_path = provider.get_config_path(ConfigType::Server)?;
  let config = provider.load_server_config(config_path.as_path())?;

  let port = config.server.port;
  info!(
    "Loaded server configuration; configured port {}",
    config.server.port
  );

  let addr = format!("0.0.0.0:{}", port)
    .parse()
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid address: {}", e))))?;

  let message_limit = resolve_message_limit(config.server.max_file_size);
  let shared_config = Arc::new(RwLock::new(config));
  let (shutdown_tx, shutdown_rx) = watch::channel(false);
  let _watcher_guard = WatcherGuard {
    sender: shutdown_tx,
  };
  spawn_config_watcher(
    provider.clone(),
    config_path,
    shared_config.clone(),
    shutdown_rx,
  );

  let adeploy_service = AdeployService::new(shared_config);

  info!("Binding ADeploy server on {}", addr);

  Server::builder()
    .add_service(
      DeployServiceServer::new(adeploy_service)
        .max_decoding_message_size(message_limit)
        .max_encoding_message_size(message_limit),
    ) // 100 MB
    .serve_with_shutdown(addr, shutdown)
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Server error: {}", e))))?;

  Ok(())
}

fn resolve_message_limit(limit: u64) -> usize {
  let limit = if limit == 0 {
    DEFAULT_MAX_MESSAGE_SIZE
  } else {
    limit
  };
  limit
    .min(usize::MAX as u64)
    .try_into()
    .unwrap_or(usize::MAX)
}

fn spawn_config_watcher(
  provider: Arc<dyn ConfigProvider>,
  config_path: PathBuf,
  shared_config: Arc<RwLock<ServerConfig>>,
  mut shutdown_rx: watch::Receiver<bool>,
) {
  tokio::spawn(async move {
    let mut last_modified = std::fs::metadata(&config_path)
      .ok()
      .and_then(|metadata| metadata.modified().ok());
    let mut last_error: Option<String> = None;

    loop {
      if *shutdown_rx.borrow() {
        break;
      }

      tokio::select! {
        res = shutdown_rx.changed() => {
          match res {
            Ok(_) => {
              if *shutdown_rx.borrow() {
                break;
              } else {
                continue;
              }
            }
            Err(_) => break,
          }
        }
        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
      }

      if *shutdown_rx.borrow() {
        break;
      }

      let metadata = match std::fs::metadata(&config_path) {
        Ok(metadata) => {
          if last_error.is_some() {
            info!(
              "Server config file {} became available again",
              config_path.display()
            );
            last_error = None;
          }
          metadata
        }
        Err(err) => {
          let msg = format!("Failed to read server config metadata: {}", err);
          if last_error.as_ref() != Some(&msg) {
            warn!("{}", msg);
            last_error = Some(msg);
          }
          continue;
        }
      };

      let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(err) => {
          let msg = format!("Failed to read server config modified time: {}", err);
          if last_error.as_ref() != Some(&msg) {
            warn!("{}", msg);
            last_error = Some(msg);
          }
          continue;
        }
      };

      if let Some(last) = last_modified {
        if modified <= last {
          continue;
        }
      }

      match provider.load_server_config(config_path.as_path()) {
        Ok(mut new_config) => {
          last_error = None;

          let existing_port = {
            let guard = shared_config.read().await;
            guard.server.port
          };

          if new_config.server.port != existing_port {
            warn!(
              "Ignoring server port change from {} to {} in {}",
              existing_port,
              new_config.server.port,
              config_path.display()
            );
            new_config.server.port = existing_port;
          }

          {
            let mut guard = shared_config.write().await;
            *guard = new_config;
          }

          info!("Reloaded server config from {}", config_path.display());
          last_modified = Some(modified);
        }
        Err(err) => {
          let msg = format!("Failed to reload server config: {}", err);
          if last_error.as_ref() != Some(&msg) {
            warn!("{}", msg);
            last_error = Some(msg);
          }
        }
      }
    }
  });
}

pub fn init_server_logging() -> Handle {
  if let Some(log_path) = server_log_path() {
    let path_string = log_path.to_string_lossy().to_string();
    return log2::open(&path_string)
      .size(10 * 1024 * 1024)
      .rotate(5)
      .level("info")
      .tee(true)
      .start();
  }

  log2::start()
}

fn server_log_path() -> Option<PathBuf> {
  let exe_dir = env::current_exe()
    .ok()
    .and_then(|path| path.parent().map(PathBuf::from))?;
  let log_dir = exe_dir.join("logs");
  std::fs::create_dir_all(&log_dir).ok()?;
  Some(log_dir.join("server.log"))
}

struct WatcherGuard {
  sender: watch::Sender<bool>,
}

impl Drop for WatcherGuard {
  fn drop(&mut self) {
    let _ = self.sender.send(true);
  }
}

fn service_error(context: &str, err: impl std::fmt::Display) -> Box<AdeployError> {
  Box::new(AdeployError::Service(format!("{context}: {err}")))
}

fn build_service_manager(user: bool) -> Result<Box<dyn ServiceManager>> {
  let mut manager = <dyn ServiceManager>::native()
    .map_err(|e| service_error("Failed to detect native service manager", e))?;

  if user {
    manager
      .set_level(ServiceLevel::User)
      .map_err(|e| service_error("User-level services are not supported on this system", e))?;
  }

  let available = manager
    .available()
    .map_err(|e| service_error("Failed to query service manager availability", e))?;
  if !available {
    return Err(Box::new(AdeployError::Service(
      "Native service manager is not available on this system".to_string(),
    )));
  }

  Ok(manager)
}

fn parse_service_label(label: &str) -> Result<ServiceLabel> {
  label
    .parse::<ServiceLabel>()
    .map_err(|e| service_error(&format!("Invalid service label '{label}'"), e))
}

pub fn install_service(
  label: &str,
  user: bool,
  autostart: bool,
  disable_restart_on_failure: bool,
  working_directory: Option<PathBuf>,
  username: Option<String>,
) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let program = env::current_exe()
    .map_err(|e| service_error("Failed to resolve current executable path", e))?;
  let args = vec![
    OsString::from("server"),
    OsString::from("run"),
    OsString::from("--service-label"),
    OsString::from(label),
  ];

  let manager = build_service_manager(user)?;
  manager
    .install(ServiceInstallCtx {
      label: service_label.clone(),
      program,
      args,
      contents: None,
      username,
      working_directory,
      environment: None,
      autostart,
      disable_restart_on_failure,
    })
    .map_err(|e| service_error(&format!("Failed to install service '{service_label}'"), e))?;

  Ok(())
}

pub fn uninstall_service(label: &str, user: bool) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .uninstall(ServiceUninstallCtx {
      label: service_label.clone(),
    })
    .map_err(|e| service_error(&format!("Failed to uninstall service '{service_label}'"), e))?;
  Ok(())
}

pub fn start_service(label: &str, user: bool) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .start(ServiceStartCtx {
      label: service_label.clone(),
    })
    .map_err(|e| service_error(&format!("Failed to start service '{service_label}'"), e))?;
  Ok(())
}

pub fn stop_service(label: &str, user: bool) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .stop(ServiceStopCtx {
      label: service_label.clone(),
    })
    .map_err(|e| service_error(&format!("Failed to stop service '{service_label}'"), e))?;
  Ok(())
}

pub fn service_status(label: &str, user: bool) -> Result<ServiceStatus> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .status(ServiceStatusCtx {
      label: service_label.clone(),
    })
    .map_err(|e| {
      service_error(
        &format!("Failed to check service status for '{service_label}'"),
        e,
      )
    })
}

pub fn format_service_status(status: &ServiceStatus) -> String {
  match status {
    ServiceStatus::NotInstalled => "not installed".to_string(),
    ServiceStatus::Running => "running".to_string(),
    ServiceStatus::Stopped(Some(reason)) => format!("stopped ({reason})"),
    ServiceStatus::Stopped(None) => "stopped".to_string(),
  }
}

#[cfg(windows)]
mod windows_service_support {
  use std::{
    ffi::OsString,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
  };

  use tokio::{runtime::Builder as RuntimeBuilder, sync::oneshot};
  use windows_service::{
    define_windows_service,
    service::{
      ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
      ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher, Error,
  };
  use windows_sys::Win32::Foundation::ERROR_FAILED_SERVICE_CONTROLLER_CONNECT;

  use super::*;

  define_windows_service!(ffi_service_main, service_main);

  static SERVICE_PROVIDER: OnceLock<Arc<dyn ConfigProvider>> = OnceLock::new();
  static SERVICE_NAME: OnceLock<String> = OnceLock::new();
  const DEFAULT_SERVICE_NAME: &str = "adeploy";

  pub fn try_run_windows_service(
    provider: Arc<dyn ConfigProvider>,
    service_name: &str,
  ) -> Result<bool> {
    let owned_name = service_name.to_string();
    let _ = SERVICE_PROVIDER.set(provider.clone());
    let _ = SERVICE_NAME.set(owned_name.clone());

    match service_dispatcher::start(&owned_name, ffi_service_main) {
      Ok(()) => Ok(true),
      Err(Error::Winapi(io_err))
        if io_err.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) =>
      {
        Ok(false)
      }
      Err(err) => Err(service_error(
        "Failed to register Windows service dispatcher",
        err,
      )),
    }
  }

  fn service_main(_arguments: Vec<OsString>) {
    let provider = match SERVICE_PROVIDER.get() {
      Some(provider) => provider.clone(),
      None => {
        error!("ADeploy service provider not initialised");
        return;
      }
    };
    let service_name = SERVICE_NAME
      .get()
      .cloned()
      .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());

    run_service(provider, service_name);
  }

  fn run_service(provider: Arc<dyn ConfigProvider>, service_name: String) {
    info!("Launching ADeploy Windows service '{service_name}'");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_signal = Arc::new(Mutex::new(Some(shutdown_tx)));
    let handle_slot: Arc<Mutex<Option<ServiceStatusHandle>>> = Arc::new(Mutex::new(None));

    let shutdown_signal_for_handler = shutdown_signal.clone();
    let handle_slot_for_handler = handle_slot.clone();

    let status_handle =
      match service_control_handler::register(&service_name, move |control_event| {
        match control_event {
          ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
          ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(handle) = handle_slot_for_handler.lock().unwrap().as_ref() {
              let _ = handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::StopPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::NO_ERROR,
                checkpoint: 1,
                wait_hint: Duration::from_secs(5),
                process_id: None,
              });
            }
            if let Some(sender) = shutdown_signal_for_handler.lock().unwrap().take() {
              let _ = sender.send(());
            }
            ServiceControlHandlerResult::NoError
          }
          _ => ServiceControlHandlerResult::NotImplemented,
        }
      }) {
        Ok(handle) => handle,
        Err(err) => {
          error!("Failed to register Windows service handler: {err}");
          return;
        }
      };

    handle_slot.lock().unwrap().replace(status_handle);

    let start_pending = ServiceStatus {
      service_type: ServiceType::OWN_PROCESS,
      current_state: ServiceState::StartPending,
      controls_accepted: ServiceControlAccept::empty(),
      exit_code: ServiceExitCode::NO_ERROR,
      checkpoint: 1,
      wait_hint: Duration::from_secs(10),
      process_id: None,
    };

    if let Err(err) = status_handle.set_service_status(start_pending) {
      error!("Failed to report service start pending: {err}");
      return;
    }

    let running_status = ServiceStatus {
      service_type: ServiceType::OWN_PROCESS,
      current_state: ServiceState::Running,
      controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
      exit_code: ServiceExitCode::NO_ERROR,
      checkpoint: 0,
      wait_hint: Duration::default(),
      process_id: None,
    };

    if let Err(err) = status_handle.set_service_status(running_status) {
      error!("Failed to report service running: {err}");
      return;
    }

    let runtime = match RuntimeBuilder::new_multi_thread().enable_all().build() {
      Ok(rt) => rt,
      Err(err) => {
        error!("Failed to build Tokio runtime for Windows service: {err}");
        let _ = status_handle.set_service_status(ServiceStatus {
          service_type: ServiceType::OWN_PROCESS,
          current_state: ServiceState::Stopped,
          controls_accepted: ServiceControlAccept::empty(),
          exit_code: ServiceExitCode::ServiceSpecific(1),
          checkpoint: 0,
          wait_hint: Duration::default(),
          process_id: None,
        });
        return;
      }
    };

    let shutdown_future = async {
      let _ = shutdown_rx.await;
    };

    let result = runtime.block_on(super::start_server_with_shutdown(provider, shutdown_future));

    drop(runtime);

    let final_exit = match result {
      Ok(_) => ServiceExitCode::NO_ERROR,
      Err(err) => {
        error!("ADeploy service terminated with error: {err}");
        ServiceExitCode::ServiceSpecific(1)
      }
    };

    let stopped_status = ServiceStatus {
      service_type: ServiceType::OWN_PROCESS,
      current_state: ServiceState::Stopped,
      controls_accepted: ServiceControlAccept::empty(),
      exit_code: final_exit,
      checkpoint: 0,
      wait_hint: Duration::default(),
      process_id: None,
    };

    if let Err(err) = status_handle.set_service_status(stopped_status) {
      error!("Failed to report service stopped state: {err}");
    }
  }
}

#[cfg(windows)]
pub use windows_service_support::try_run_windows_service;

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn try_run_windows_service(
  _provider: Arc<dyn ConfigProvider>,
  _service_name: &str,
) -> Result<bool> {
  Ok(false)
}
