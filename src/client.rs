use std::path::PathBuf;

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::transport::Channel;

use crate::{
  adeploy::{
    deploy_service_client::DeployServiceClient, DeployRequest,
  },
  auth::SshAuth,
  config::{get_server_config, load_client_config},
  deploy::DeployManager,
  error::{AdeployError, Result},
};

/// Deploy to a remote server
pub async fn deploy(host: &str, port: u16, config_path: PathBuf, package_name: &str) -> Result<()> {
  deploy_packages(host, port, config_path, Some(vec![package_name.to_string()])).await
}

/// Deploy specific packages to a remote server
pub async fn deploy_packages(
  host: &str,
  port: u16,
  config_path: PathBuf,
  package_names: Option<Vec<String>>,
) -> Result<()> {
  // Load client configuration
  let config = load_client_config(config_path)?;

  // Get server configuration for the target host
  let server_config = get_server_config(&config, host).ok_or_else(|| {
    AdeployError::Config(format!("No server configuration found for host: {}", host))
  })?;

  // Use port from config if not overridden
  let actual_port = if port != 6060 {
    port
  } else {
    server_config.port
  };

  // Connect to server
  let endpoint = format!("http://{}:{}", host, actual_port);
  let channel = Channel::from_shared(endpoint)
    .map_err(|e| AdeployError::Network(format!("Invalid endpoint: {}", e)))?
    .connect()
    .await
    .map_err(|e| AdeployError::Network(format!("Failed to connect: {}", e)))?;

  let mut client = DeployServiceClient::new(channel);

  // Create deployment manager
  let deploy_manager = DeployManager::new();

  // Setup SSH authentication
  let ssh_auth = SshAuth::new();
  let public_key = SshAuth::load_public_key(&server_config.ssh_key_path)?;

  // Determine which packages to deploy
  let packages_to_deploy: Vec<_> = if let Some(names) = package_names {
    // Deploy only specified packages
    names
      .into_iter()
      .filter_map(|name| config.packages.get(&name).map(|pkg| (name, pkg)))
      .collect()
  } else {
    return Err(AdeployError::Config(
      "No packages found to deploy".to_string(),
    ));
  };

  if packages_to_deploy.is_empty() {
    return Err(AdeployError::Config(
      "No packages found to deploy".to_string(),
    ));
  }

  // Deploy each package
  for (package_name, package_config) in packages_to_deploy {
    info!("Deploying package: {}", package_name);

    // Package files
    let archive_data = deploy_manager.package_files(&package_name, package_config)?;

    // Sign the data
    let signature = ssh_auth.sign_data(&archive_data)?;

    // Create deploy request
    let request = tonic::Request::new(DeployRequest {
      package_name: package_name.clone(),
      version: "1.0.0".to_string(), // Default version, could be made configurable
      file_data: archive_data,
      ssh_signature: general_purpose::STANDARD.encode(&signature),
      client_public_key: public_key.clone(),
      metadata: std::collections::HashMap::new(),
    });

    // Send deploy request
    info!("Sending deployment request for package: {}", package_name);
    let response = client
      .deploy(request)
      .await
      .map_err(|e| AdeployError::Grpc(e))?;

    let deploy_response = response.into_inner();

    if deploy_response.success {
      info!(
        "Deployment successful for package: {}! Deploy ID: {}",
        package_name, deploy_response.deploy_id
      );
      for log_line in &deploy_response.logs {
        info!("{}", log_line);
      }
    } else {
      error!(
        "Deployment failed for package {}: {}",
        package_name, deploy_response.message
      );
      for log_line in &deploy_response.logs {
        error!("{}", log_line);
      }
      return Err(AdeployError::Deploy(format!(
        "Package {} deployment failed: {}",
        package_name, deploy_response.message
      )));
    }
  }

  Ok(())
}
