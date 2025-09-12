use std::path::PathBuf;

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::{transport::Server, Request, Response, Status};

use crate::{
  adeploy::{
    deploy_service_server::{DeployService, DeployServiceServer},
    DeployRequest, DeployResponse,
  },
  auth::SshAuth,
  config::{load_server_config, ServerDeployConfig},
  deploy::DeployManager,
  error::{AdeployError, Result},
};



/// ADeploy gRPC service implementation
#[derive(Clone)]
pub struct AdeployService {
  config: ServerDeployConfig,
}

impl AdeployService {
  pub fn new(config: ServerDeployConfig) -> Self {
    Self {
      config,
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

    // Log deployment start
    info!("Starting deployment for package: {}", req.package_name);

    // Execute deployment synchronously for now
    // TODO: Implement proper async deployment with Send-safe types
    let result = Self::execute_deployment(&deploy_manager, &package_config, &req.file_data).await;

    match result {
      Ok(logs) => {
        info!("Deployment completed successfully for package: {}", req.package_name);

        Ok(Response::new(DeployResponse {
          success: true,
          message: "Deployment completed successfully".to_string(),
          deploy_id,
          logs,
        }))
      }
      Err(e) => {
        error!("Deployment failed for package {}: {}", req.package_name, e);

        Err(Status::internal(format!("Deployment failed: {}", e)))
      }
    }
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
pub async fn start_server(port: u16, config_path: PathBuf) -> Result<()> {
  // Load server configuration
  let config = load_server_config(config_path)?;

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
