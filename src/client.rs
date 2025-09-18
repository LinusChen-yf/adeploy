use std::path::PathBuf;

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::transport::Channel;

use crate::{
  adeploy::{deploy_service_client::DeployServiceClient, DeployRequest},
  auth::Auth,
  config::{get_remote_config, load_client_config},
  deploy::DeployManager,
  error::{AdeployError, Result},
};

/// Deploy to a remote server
pub async fn deploy(host: &str, config_path: PathBuf, package_name: &str) -> Result<()> {
  deploy_packages(host, config_path, Some(vec![package_name.to_string()])).await
}

/// Deploy specific packages to a remote server
pub async fn deploy_packages(
  host: &str,
  config_path: PathBuf,
  package_names: Option<Vec<String>>,
) -> Result<()> {
  // Load client configuration
  let config = load_client_config(config_path)?;

  // Get server configuration for the target host
  let server_config = get_remote_config(&config, host).ok_or_else(|| {
    Box::new(AdeployError::Config(format!(
      "No server configuration found for host: {}",
      host
    )))
  })?;

  // Use port from config
  let actual_port = server_config.port;

  // Connect to server
  let endpoint = format!("http://{}:{}", host, actual_port);
  let channel = Channel::from_shared(endpoint)
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid endpoint: {}", e))))?
    .connect()
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Failed to connect: {}", e))))?;

  let mut client = DeployServiceClient::new(channel)
    .max_decoding_message_size(100 * 1024 * 1024) // 100MB
    .max_encoding_message_size(100 * 1024 * 1024); // 100MB

  // Create deployment manager
  let deploy_manager = DeployManager::new();

  // Setup SSH authentication with key pair
  // Determine key paths based on server configuration
  let (private_key_path, public_key_path) = if let Some(custom_key_path) = &server_config.key_path {
    // Use custom key path if specified
    let public_key_path = PathBuf::from(custom_key_path);

    // Check if the custom key file exists
    if !public_key_path.exists() {
      return Err(Box::new(AdeployError::FileSystem(format!(
        "Custom key file not found at specified path: {}",
        custom_key_path
      ))));
    }

    // Derive private key path from public key path
    let private_key_path = if custom_key_path.ends_with(".pub") {
      PathBuf::from(&custom_key_path[..custom_key_path.len() - 4])
    } else {
      return Err(Box::new(AdeployError::Config(format!(
        "Custom key_path should point to a .pub file: {}",
        custom_key_path
      ))));
    };

    // Check if private key exists
    if !private_key_path.exists() {
      return Err(Box::new(AdeployError::FileSystem(format!(
        "Private key file not found at: {}",
        private_key_path.display()
      ))));
    }

    (private_key_path, public_key_path)
  } else {
    // Use default key paths
    let key_dir = PathBuf::from(".key");
    let private_key_path = key_dir.join("id_ed25519");
    let public_key_path = key_dir.join("id_ed25519.pub");

    // Check if key directory exists, create if not
    if !key_dir.exists() {
      std::fs::create_dir_all(&key_dir).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to create key directory: {}",
          e
        )))
      })?;
    }

    // Check if key files exist, generate if not
    if !private_key_path.exists() || !public_key_path.exists() {
      info!("Generating new Ed25519 key pair...");
      Auth::generate_key_pair(
        &public_key_path.to_string_lossy(),
        &private_key_path.to_string_lossy(),
      )?;
      info!("Key pair generated successfully at: {:?}", key_dir);
    }

    (private_key_path, public_key_path)
  };

  // Load the keypair
  let keypair = Auth::load_key_pair(&private_key_path.to_string_lossy()).map_err(|e| {
    Box::new(AdeployError::Auth(format!(
      "Failed to load SSH key pair: {}",
      e
    )))
  })?;
  let ssh_auth = Auth::with_key_pair(keypair);

  // Load public key
  let public_key = Auth::load_public_key(&public_key_path).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to load public key: {}",
      e
    )))
  })?;

  // Determine which packages to deploy
  let packages_to_deploy: Vec<_> = if let Some(names) = package_names {
    // Deploy only specified packages
    names
      .into_iter()
      .filter_map(|name| config.packages.get(&name).map(|pkg| (name, pkg)))
      .collect()
  } else {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  };

  if packages_to_deploy.is_empty() {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  }

  // Deploy each package
  for (package_name, package_config) in packages_to_deploy {
    info!("Deploying package: {}", package_name);

    // Package files
    let (archive_data, file_hash) = deploy_manager.package_files(&package_name, package_config)?;

    // Sign the data
    let signature = ssh_auth
      .sign_data(&archive_data)
      .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign data: {}", e))))?;

    // Create deploy request
    let request = tonic::Request::new(DeployRequest {
      package_name: package_name.clone(),
      version: "1.0.0".to_string(), // Default version, could be made configurable
      file_data: archive_data,
      file_hash,
      signature: general_purpose::STANDARD.encode(&signature),
      public_key: public_key.clone(),
      metadata: std::collections::HashMap::new(),
    });

    // Send deploy request
    info!("Sending deployment request for package: {}", package_name);
    let response = client
      .deploy(request)
      .await
      .map_err(|e| Box::new(AdeployError::Grpc(e)))?;

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
      return Err(Box::new(AdeployError::Deploy(format!(
        "Package {} deployment failed: {}",
        package_name, deploy_response.message
      ))));
    }
  }

  Ok(())
}
