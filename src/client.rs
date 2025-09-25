use std::{convert::TryInto, time::Duration};

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::transport::{Channel, Endpoint};

use crate::{
  adeploy::{deploy_service_client::DeployServiceClient, DeployRequest},
  auth::Auth,
  config::{get_remote_config, ConfigProvider, ConfigType, RemoteConfig},
  deploy::DeployManager,
  error::{AdeployError, Result},
};

const DEFAULT_MAX_MESSAGE_SIZE: u64 = 100 * 1024 * 1024;

/// Deploy specific packages using an explicit provider
pub async fn deploy(
  host: &str,
  package_names: Option<Vec<String>>,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let config_path = provider.get_config_path(ConfigType::Client)?;
  let config = provider.load_client_config(config_path.as_path())?;
  info!(
    "Loading client configuration from {}",
    config_path.display()
  );

  // Look up server config for host
  let remote_config = get_remote_config(&config, host).ok_or_else(|| {
    Box::new(AdeployError::Config(format!(
      "No server configuration found for host: {}",
      host
    )))
  })?;

  // Use configured port
  let actual_port = remote_config.port;
  info!("Connecting to {}:{} for deployment", host, actual_port);

  // Build gRPC channel
  let endpoint_uri = format!("http://{}:{}", host, actual_port);
  let endpoint = Channel::from_shared(endpoint_uri)
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid endpoint: {}", e))))?;
  let endpoint = configure_endpoint(endpoint, remote_config.timeout);
  let channel = endpoint
    .connect()
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Failed to connect: {}", e))))?;

  let max_file_size = resolved_max_file_size(remote_config);
  let message_limit = clamp_message_limit(max_file_size);
  let mut client = DeployServiceClient::new(channel)
    .max_decoding_message_size(message_limit)
    .max_encoding_message_size(message_limit);

  // Initialize deployment manager
  let deploy_manager = DeployManager::new();

  // Prepare SSH authentication
  let key_paths = provider.get_key_paths()?;
  let private_key_path = key_paths.private_key;
  let public_key_path = key_paths.public_key;

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

  // Pick packages for deployment
  let packages_to_deploy: Vec<_> = if let Some(names) = package_names {
    // Filter to requested packages
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
    info!("Deploying {}", package_name);

    // Bundle files
    let (archive_data, file_hash) = deploy_manager
      .package_files(&package_name, package_config)
      .await?;

    enforce_client_archive_size(&archive_data, max_file_size)?;

    // Sign the archive
    let signature = ssh_auth
      .sign_data(&archive_data)
      .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign data: {}", e))))?;

    // Build deploy request
    let request = tonic::Request::new(DeployRequest {
      package_name: package_name.clone(),
      version: "1.0.0".to_string(), // Default version, could be made configurable
      file_data: archive_data,
      file_hash,
      signature: general_purpose::STANDARD.encode(&signature),
      public_key: public_key.clone(),
      metadata: std::collections::HashMap::new(),
    });

    // Invoke gRPC deploy
    let response = match client.deploy(request).await {
      Ok(resp) => resp,
      Err(status) => {
        if status.code() == tonic::Code::Unauthenticated {
          error!(
            "Deployment rejected (unauthenticated). Add this public key to the server's `allowed_keys`: {}",
            public_key.trim()
          );
        }
        return Err(Box::new(AdeployError::Grpc(status)));
      }
    };

    let deploy_response = response.into_inner();

    if deploy_response.success {
      info!(
        "Deployment succeeded for {} (ID: {})",
        package_name, deploy_response.deploy_id
      );
      for log_line in &deploy_response.logs {
        info!("{}", log_line);
      }
    } else {
      error!(
        "Deployment failed for {}: {}",
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

fn configure_endpoint(endpoint: Endpoint, timeout_secs: u64) -> Endpoint {
  if timeout_secs == 0 {
    endpoint
  } else {
    let timeout = Duration::from_secs(timeout_secs);
    endpoint.connect_timeout(timeout).timeout(timeout)
  }
}

fn resolved_max_file_size(config: &RemoteConfig) -> u64 {
  config
    .max_file_size
    .filter(|value| *value > 0)
    .unwrap_or(DEFAULT_MAX_MESSAGE_SIZE)
}

fn clamp_message_limit(limit: u64) -> usize {
  limit
    .min(usize::MAX as u64)
    .try_into()
    .unwrap_or(usize::MAX)
}

fn enforce_client_archive_size(data: &[u8], limit: u64) -> Result<()> {
  if limit > 0 {
    let archive_size = data.len() as u64;
    if archive_size > limit {
      return Err(Box::new(AdeployError::Deploy(format!(
        "Archive size {} exceeds configured max_file_size {}",
        archive_size, limit
      ))));
    }
  }
  Ok(())
}
