use std::path::PathBuf;

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::transport::Channel;

use crate::{
  adeploy::{
    deploy_service_client::DeployServiceClient, DeployRequest, ListPackagesRequest, StatusRequest,
  },
  auth::SshAuth,
  config::load_client_config,
  deploy::DeployManager,
  error::{AdeployError, Result},
};

/// Deploy to a remote server
pub async fn deploy(host: &str, port: u16, config_path: PathBuf) -> Result<()> {
  // Load client configuration
  let config = load_client_config(config_path)?;

  // Connect to server
  let endpoint = format!("http://{}:{}", host, port);
  let channel = Channel::from_shared(endpoint)
    .map_err(|e| AdeployError::Network(format!("Invalid endpoint: {}", e)))?
    .connect()
    .await
    .map_err(|e| AdeployError::Network(format!("Failed to connect: {}", e)))?;

  let mut client = DeployServiceClient::new(channel);

  // Create deployment manager
  let deploy_manager = DeployManager::new();

  // Package files
  let file_data = deploy_manager.package_files(&config.package)?;

  // Setup SSH authentication
  let ssh_auth = SshAuth::new();
  let public_key = SshAuth::load_public_key(&config.server.ssh_key_path)?;
  let signature = ssh_auth.sign_data(&file_data)?;

  // Create deploy request
  let request = tonic::Request::new(DeployRequest {
    package_name: config.package.name.clone(),
    version: config.package.version.clone(),
    file_data,
    ssh_signature: general_purpose::STANDARD.encode(&signature),
    client_public_key: public_key,
    metadata: std::collections::HashMap::new(),
  });

  // Send deploy request
  info!(
    "Sending deployment request for package: {}",
    config.package.name
  );
  let response = client
    .deploy(request)
    .await
    .map_err(|e| AdeployError::Grpc(e))?;

  let deploy_response = response.into_inner();

  if deploy_response.success {
    info!(
      "Deployment successful! Deploy ID: {}",
      deploy_response.deploy_id
    );
    for log_line in &deploy_response.logs {
      println!("{}", log_line);
    }
  } else {
    error!("Deployment failed: {}", deploy_response.message);
    for log_line in &deploy_response.logs {
      eprintln!("{}", log_line);
    }
    return Err(AdeployError::Deploy(deploy_response.message));
  }

  Ok(())
}

/// Check deployment status
pub async fn check_status(host: &str, port: u16, deploy_id: &str) -> Result<()> {
  // Connect to server
  let endpoint = format!("http://{}:{}", host, port);
  let channel = Channel::from_shared(endpoint)
    .map_err(|e| AdeployError::Network(format!("Invalid endpoint: {}", e)))?
    .connect()
    .await
    .map_err(|e| AdeployError::Network(format!("Failed to connect: {}", e)))?;

  let mut client = DeployServiceClient::new(channel);

  // Create status request
  let request = tonic::Request::new(StatusRequest {
    deploy_id: deploy_id.to_string(),
  });

  // Send status request
  let response = client
    .get_status(request)
    .await
    .map_err(|e| AdeployError::Grpc(e))?;

  let status_response = response.into_inner();

  println!("Deploy ID: {}", deploy_id);
  println!("Status: {:?}", status_response.status());
  println!("Message: {}", status_response.message);

  if !status_response.logs.is_empty() {
    println!("\nLogs:");
    for log_line in &status_response.logs {
      println!("{}", log_line);
    }
  }

  Ok(())
}

/// List packages on server
pub async fn list_packages(host: &str, port: u16) -> Result<()> {
  // Connect to server
  let endpoint = format!("http://{}:{}", host, port);
  let channel = Channel::from_shared(endpoint)
    .map_err(|e| AdeployError::Network(format!("Invalid endpoint: {}", e)))?
    .connect()
    .await
    .map_err(|e| AdeployError::Network(format!("Failed to connect: {}", e)))?;

  let mut client = DeployServiceClient::new(channel);

  // Create list request
  let request = tonic::Request::new(ListPackagesRequest {});

  // Send list request
  let response = client
    .list_packages(request)
    .await
    .map_err(|e| AdeployError::Grpc(e))?;

  let list_response = response.into_inner();

  if list_response.packages.is_empty() {
    println!("No packages configured on server");
  } else {
    println!("Available packages:");
    for package in &list_response.packages {
      println!("  Name: {}", package.name);
      println!("  Deploy Path: {}", package.deploy_path);
      println!("  Backup Enabled: {}", package.backup_enabled);
      println!("  Version: {}", package.version);
      println!("  Last Deploy: {}", package.last_deploy_time);
      println!();
    }
  }

  Ok(())
}
