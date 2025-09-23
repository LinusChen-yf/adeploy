use std::{path::PathBuf, sync::Arc, time::Duration};

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tokio::sync::{watch, RwLock};
use tonic::{transport::Server, Request, Response, Status};

use crate::{
  adeploy::{
    deploy_service_server::{DeployService, DeployServiceServer},
    DeployRequest, DeployResponse,
  },
  auth::Auth,
  config::ServerConfig,
  deploy::DeployManager,
  error::{AdeployError, Result},
};

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
    let req = request.into_inner();

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

    let (allowed_keys, package_config) = {
      let config = self.config.read().await;
      let allowed_keys = config.server.allowed_keys.clone();
      let package_config = config.packages.get(&req.package_name).cloned();
      (allowed_keys, package_config)
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

    // Initialize deployment manager
    let deploy_manager = DeployManager::new();
    let deploy_id = deploy_manager.deploy_id.clone();

    info!("Starting deployment {} for {}", deploy_id, req.package_name);

    // Execute deployment synchronously for now
    // TODO: Implement proper async deployment with Send-safe types
    match Self::execute_deployment(
      &deploy_manager,
      &package_config,
      &req.file_data,
      &req.file_hash,
      &req.package_name,
    )
    .await
    {
      Ok(logs) => {
        info!(
          "Deployment {} completed for {}",
          deploy_id, req.package_name
        );

        Ok(Response::new(DeployResponse {
          success: true,
          message: "Deployment completed successfully".to_string(),
          deploy_id,
          logs,
        }))
      }
      Err(e) => {
        error!(
          "Deployment {} failed for {}: {}",
          deploy_id, req.package_name, e
        );

        // Always collect logs on failure
        let mut logs = vec![format!("ERROR: Deployment failed: {}", e)];

        // Include additional details when available
        if let AdeployError::Deploy(msg) = e.as_ref() {
          logs.push(format!("Details: {}", msg));
        }

        Ok(Response::new(DeployResponse {
          success: false,
          message: e.to_string(),
          deploy_id,
          logs,
        }))
      }
    }
  }
}

impl AdeployService {
  async fn execute_deployment(
    deploy_manager: &DeployManager,
    package_config: &crate::config::ServerPackageConfig,
    file_data: &[u8],
    file_hash: &str,
    package_name: &str,
  ) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    logs.push(format!(
      "[{}] Starting deployment execution",
      deploy_manager.deploy_id
    ));

    // Run before-deploy hook
    logs.push("Running Before-deploy script...".to_string());
    match deploy_manager.execute_before_deploy_script(package_config) {
      Ok(pre_logs) => {
        logs.extend(pre_logs);
        logs.push("Before-deploy script succeeded".to_string());
      }
      Err(e) => {
        error!("Before-deploy script failed: {}", e);
        logs.push(format!("ERROR: Before-deploy script failed: {}", e));
        return Err(e);
      }
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
    // Extract archive and verify hash
    logs.push("Extracting files...".to_string());
    match deploy_manager.extract_files(file_data, file_hash, package_config, package_name) {
      Ok(()) => {
        logs.push("Files extracted and deployed successfully".to_string());
      }
      Err(e) => {
        error!("File extraction failed: {}", e);
        logs.push(format!("ERROR: File extraction failed: {}", e));
        return Err(e);
      }
    }

    // Run after-deploy hook
    logs.push("Running After-deploy script...".to_string());
    match deploy_manager.execute_after_deploy_script(package_config) {
      Ok(post_logs) => {
        logs.extend(post_logs);
        logs.push("After-deploy script succeeded".to_string());
      }
      Err(e) => {
        error!("After-deploy script failed: {}", e);
        logs.push(format!("ERROR: After-deploy script failed: {}", e));
        // Deployment succeeds even if the After-deploy script fails
      }
    }

    logs.push(format!(
      "[{}] Deployment completed successfully",
      deploy_manager.deploy_id
    ));
    Ok(logs)
  }
}

/// Start the gRPC server using the default configuration path next to the binary
pub async fn start_server_with_default_config() -> Result<()> {
  let config_path = crate::config::resolve_default_config_path("server_config.toml");
  start_server_from_config_path(config_path).await
}

/// Start the gRPC server using a specific configuration path
pub async fn start_server_from_config_path<P>(config_path: P) -> Result<()>
where
  P: Into<PathBuf>,
{
  let config_path = config_path.into();
  info!(
    "Loading server configuration from {}",
    config_path.display()
  );
  let initial_config = crate::config::load_server_config(&config_path)?;
  info!(
    "Loaded server configuration; configured port {}",
    initial_config.server.port
  );
  start_server_inner(config_path, initial_config).await
}

/// Start the gRPC server using a resolved configuration
async fn start_server_inner(config_path: PathBuf, initial_config: ServerConfig) -> Result<()> {
  let port = initial_config.server.port;
  let addr = format!("0.0.0.0:{}", port)
    .parse()
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid address: {}", e))))?;

  let shared_config = Arc::new(RwLock::new(initial_config));
  let (shutdown_tx, shutdown_rx) = watch::channel(false);
  let _watcher_guard = WatcherGuard {
    sender: shutdown_tx,
  };
  spawn_config_watcher(config_path.clone(), shared_config.clone(), shutdown_rx);

  let adeploy_service = AdeployService::new(shared_config);

  info!("Binding ADeploy server on {}", addr);

  Server::builder()
    .add_service(
      DeployServiceServer::new(adeploy_service)
        .max_decoding_message_size(100 * 1024 * 1024) // 100 MB
        .max_encoding_message_size(100 * 1024 * 1024),
    ) // 100 MB
    .serve(addr)
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Server error: {}", e))))?;

  Ok(())
}

fn spawn_config_watcher(
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

      match crate::config::load_server_config(&config_path) {
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

struct WatcherGuard {
  sender: watch::Sender<bool>,
}

impl Drop for WatcherGuard {
  fn drop(&mut self) {
    let _ = self.sender.send(true);
  }
}
