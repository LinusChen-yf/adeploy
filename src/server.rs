use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use log2::*;
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
  config: ServerConfig,
}

impl AdeployService {
  pub fn new(config: ServerConfig) -> Self {
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

    info!("Received deploy request for package: {}", req.package_name);

    // Verify SSH signature against allowed keys
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

    // Check if the provided public key is in the allowed keys list
    let is_allowed = self
      .config
      .server
      .allowed_keys
      .iter()
      .any(|allowed_key| allowed_key == &req.public_key);

    if !is_allowed {
      error!(
        "Client public key is not in allowed keys list for package: {}",
        req.package_name
      );
      return Err(Status::unauthenticated("Client public key not allowed"));
    }

    match Auth::verify_signature(&req.public_key, &req.file_data, &signature) {
      Ok(valid) => {
        if !valid {
          error!(
            "Ed25519 signature verification failed for package: {}",
            req.package_name
          );
          return Err(Status::unauthenticated("Invalid Ed25519 signature"));
        }
      }
      Err(e) => {
        error!("Ed25519 signature verification error: {}", e);
        return Err(Status::unauthenticated(format!("Auth error: {}", e)));
      }
    }

    // Check if package is configured
    let package_config = match self.config.packages.get(&req.package_name) {
      Some(config) => config,
      None => {
        error!("Package '{}' not configured", req.package_name);
        return Err(Status::not_found(format!(
          "Package '{}' not configured",
          req.package_name
        )));
      }
    };

    // Create deployment manager
    let deploy_manager = DeployManager::new();
    let deploy_id = deploy_manager.deploy_id.clone();

    // Log deployment start
    info!(
      "Starting deployment [{}] for package: {}",
      deploy_id, req.package_name
    );

    // Execute deployment synchronously for now
    // TODO: Implement proper async deployment with Send-safe types
    match Self::execute_deployment(
      &deploy_manager,
      package_config,
      &req.file_data,
      &req.file_hash,
      &req.package_name,
    )
    .await
    {
      Ok(logs) => {
        info!(
          "Deployment [{}] completed successfully for package: {}",
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
          "Deployment [{}] failed for package {}: {}",
          deploy_id, req.package_name, e
        );

        // Collect logs even in case of failure
        let mut logs = vec![format!("ERROR: Deployment failed: {}", e)];

        // Try to get more detailed logs if available
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

    // Execute before-deploy script
    logs.push("Executing before-deploy script...".to_string());
    match deploy_manager.execute_before_deploy_script(package_config) {
      Ok(pre_logs) => {
        logs.extend(pre_logs);
        logs.push("Before-deploy script executed successfully".to_string());
      }
      Err(e) => {
        error!("Before-deploy script failed: {}", e);
        logs.push(format!("ERROR: Before-deploy script failed: {}", e));
        return Err(e);
      }
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
    // Extract and deploy files with hash verification
    logs.push("Extracting and deploying files...".to_string());
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

    // Execute after-deploy script
    logs.push("Executing after-deploy script...".to_string());
    match deploy_manager.execute_after_deploy_script(package_config) {
      Ok(post_logs) => {
        logs.extend(post_logs);
        logs.push("After-deploy script executed successfully".to_string());
      }
      Err(e) => {
        error!("After-deploy script failed: {}", e);
        logs.push(format!("ERROR: After-deploy script failed: {}", e));
        // Note: We don't return an error here as the deployment itself was successful
        // The after-deploy script failure is logged but doesn't fail the entire deployment
      }
    }

    logs.push(format!(
      "[{}] Deployment completed successfully",
      deploy_manager.deploy_id
    ));
    Ok(logs)
  }
}

/// Start the gRPC server
pub async fn start_server(port: u16, config: ServerConfig) -> Result<()> {
  let addr = format!("0.0.0.0:{}", port)
    .parse()
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid address: {}", e))))?;

  let adeploy_service = AdeployService::new(config);

  info!("Starting ADeploy server on {}", addr);

  Server::builder()
    .add_service(
      DeployServiceServer::new(adeploy_service)
        .max_decoding_message_size(100 * 1024 * 1024) // 100MB
        .max_encoding_message_size(100 * 1024 * 1024),
    ) // 100MB
    .serve(addr)
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Server error: {}", e))))?;

  Ok(())
}
