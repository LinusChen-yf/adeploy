use std::{collections::HashMap, path::PathBuf, sync::Arc};

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tokio::sync::RwLock;
use tonic::{transport::Server, Request, Response, Status};

use crate::{
  adeploy::{
    deploy_service_server::{DeployService, DeployServiceServer},
    status_response::DeployStatus,
    DeployRequest, DeployResponse, ListPackagesRequest, ListPackagesResponse, PackageInfo,
    StatusRequest, StatusResponse,
  },
  auth::SshAuth,
  config::{load_server_config, ServerDeployConfig},
  deploy::DeployManager,
  error::{AdeployError, Result},
};

/// Deployment status tracking
#[derive(Debug, Clone)]
struct DeploymentStatus {
  status: DeployStatus,
  message: String,
  logs: Vec<String>,
}

/// ADeploy gRPC service implementation
pub struct AdeployService {
  config: ServerDeployConfig,
  deployments: Arc<RwLock<HashMap<String, DeploymentStatus>>>,
}

impl AdeployService {
  pub fn new(config: ServerDeployConfig) -> Self {
    Self {
      config,
      deployments: Arc::new(RwLock::new(HashMap::new())),
    }
  }
}

#[tonic::async_trait]
impl DeployService for AdeployService {
  async fn deploy(
    &self,
    request: Request<DeployRequest>,
  ) -> std::result::Result<Response<DeployResponse>, Status> {
    let req = request.into_inner();

    info!("Received deploy request for package: {}", req.package_name);

    // Verify SSH signature
    let signature = general_purpose::STANDARD
      .decode(&req.ssh_signature)
      .map_err(|e| Status::invalid_argument(format!("Invalid signature: {}", e)))?;

    if !SshAuth::verify_signature(&req.client_public_key, &req.file_data, &signature)
      .map_err(|e| Status::unauthenticated(format!("Auth error: {}", e)))?
    {
      return Err(Status::unauthenticated("Invalid SSH signature"));
    }

    // Check if package is configured
    let package_config =
      self.config.packages.get(&req.package_name).ok_or_else(|| {
        Status::not_found(format!("Package '{}' not configured", req.package_name))
      })?;

    // Create deployment manager
    let deploy_manager = DeployManager::new();
    let deploy_id = deploy_manager.deploy_id.clone();

    // Initialize deployment status
    {
      let mut deployments = self.deployments.write().await;
      deployments.insert(
        deploy_id.clone(),
        DeploymentStatus {
          status: DeployStatus::Running,
          message: "Deployment started".to_string(),
          logs: vec!["Starting deployment...".to_string()],
        },
      );
    }

    // Execute deployment synchronously for now
    // TODO: Implement proper async deployment with Send-safe types
    let result = Self::execute_deployment(&deploy_manager, &package_config, &req.file_data).await;

    match result {
      Ok(logs) => {
        let mut deployments = self.deployments.write().await;
        deployments.insert(
          deploy_id.clone(),
          DeploymentStatus {
            status: DeployStatus::Success,
            message: "Deployment completed successfully".to_string(),
            logs: logs.clone(),
          },
        );

        Ok(Response::new(DeployResponse {
          success: true,
          message: "Deployment completed successfully".to_string(),
          deploy_id,
          logs,
        }))
      }
      Err(e) => {
        let mut deployments = self.deployments.write().await;
        deployments.insert(
          deploy_id.clone(),
          DeploymentStatus {
            status: DeployStatus::Failed,
            message: format!("Deployment failed: {}", e),
            logs: vec![format!("ERROR: {}", e)],
          },
        );

        Err(Status::internal(format!("Deployment failed: {}", e)))
      }
    }
  }

  async fn get_status(
    &self,
    request: Request<StatusRequest>,
  ) -> std::result::Result<Response<StatusResponse>, Status> {
    let req = request.into_inner();

    let deployments = self.deployments.read().await;
    let deployment = deployments
      .get(&req.deploy_id)
      .ok_or_else(|| Status::not_found("Deploy ID not found"))?;

    Ok(Response::new(StatusResponse {
      status: deployment.status as i32,
      message: deployment.message.clone(),
      logs: deployment.logs.clone(),
    }))
  }

  async fn list_packages(
    &self,
    _request: Request<ListPackagesRequest>,
  ) -> std::result::Result<Response<ListPackagesResponse>, Status> {
    let packages: Vec<PackageInfo> = self
      .config
      .packages
      .iter()
      .map(|(name, config)| PackageInfo {
        name: name.clone(),
        deploy_path: config.deploy_path.clone(),
        backup_enabled: config.backup_enabled,
        last_deploy_time: "N/A".to_string(), // TODO: Track actual deploy times
        version: "N/A".to_string(),          // TODO: Track versions
      })
      .collect();

    Ok(Response::new(ListPackagesResponse { packages }))
  }
}

impl AdeployService {
  async fn execute_deployment(
    deploy_manager: &DeployManager,
    package_config: &crate::config::DeployPackageConfig,
    file_data: &[u8],
  ) -> Result<Vec<String>> {
    let mut logs = Vec::new();

    // Execute pre-deploy script
    let pre_logs = deploy_manager.execute_pre_deploy_script(package_config)?;
    logs.extend(pre_logs);

    // Extract and deploy files
    deploy_manager.extract_files(file_data, package_config)?;
    logs.push("Files extracted successfully".to_string());

    // Execute post-deploy script
    let post_logs = deploy_manager.execute_post_deploy_script(package_config)?;
    logs.extend(post_logs);

    logs.push("Deployment completed successfully".to_string());
    Ok(logs)
  }
}

/// Start the gRPC server
pub async fn start_server(
  port: u16,
  config_path: PathBuf,
  _daemon: bool, // TODO: Implement daemon mode
) -> Result<()> {
  // Load server configuration
  let config = load_server_config(config_path)?;

  // Initialize logger for server with file output
  std::fs::create_dir_all("./logs").ok();
  log2::open("./logs/server.log")
    .size(10 * 1024 * 1024)
    .rotate(5)
    .level("info")
    .start();

  let addr = format!("0.0.0.0:{}", port)
    .parse()
    .map_err(|e| AdeployError::Network(format!("Invalid address: {}", e)))?;

  let adeploy_service = AdeployService::new(config);

  info!("Starting ADeploy server on {}", addr);

  Server::builder()
    .add_service(DeployServiceServer::new(adeploy_service))
    .serve(addr)
    .await
    .map_err(|e| AdeployError::Network(format!("Server error: {}", e)))?;

  Ok(())
}
