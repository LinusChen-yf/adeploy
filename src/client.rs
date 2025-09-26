use std::{convert::TryInto, time::Duration};

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::transport::{Channel, Endpoint};

use crate::{
  adeploy::{deploy_service_client::DeployServiceClient, DeployRequest},
  auth::Auth,
  config::{
    get_remote_config, ClientConfig, ClientPackageConfig, ConfigProvider, ConfigType, RemoteConfig,
  },
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
  let config = load_client_configuration(provider)?;
  let remote_config = resolve_remote_configuration(&config, host)?;
  let max_file_size = resolved_max_file_size(remote_config);
  let mut client = connect_deploy_client(host, remote_config).await?;
  let deploy_manager = DeployManager::new();
  let auth_resources = prepare_auth_resources(provider)?;
  let packages_to_deploy = select_packages(&config, package_names)?;

  for (package_name, package_config) in packages_to_deploy {
    deploy_single_package(
      &deploy_manager,
      &mut client,
      &auth_resources.ssh_auth,
      &auth_resources.public_key,
      &package_name,
      package_config,
      max_file_size,
    )
    .await?;
  }

  Ok(())
}

struct AuthResources {
  ssh_auth: Auth,
  public_key: String,
}

fn load_client_configuration(provider: &dyn ConfigProvider) -> Result<ClientConfig> {
  let config_path = provider.get_config_path(ConfigType::Client)?;
  let config = provider.load_client_config(config_path.as_path())?;
  info!(
    "Loading client configuration from {}",
    config_path.display()
  );
  Ok(config)
}

fn resolve_remote_configuration<'a>(
  config: &'a ClientConfig,
  host: &str,
) -> Result<&'a RemoteConfig> {
  get_remote_config(config, host).ok_or_else(|| {
    Box::new(AdeployError::Config(format!(
      "No server configuration found for host: {}",
      host
    )))
  })
}

async fn connect_deploy_client(
  host: &str,
  remote_config: &RemoteConfig,
) -> Result<DeployServiceClient<Channel>> {
  let actual_port = remote_config.port;
  info!("Connecting to {}:{} for deployment", host, actual_port);

  let endpoint_uri = format!("http://{}:{}", host, actual_port);
  let endpoint = Channel::from_shared(endpoint_uri)
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid endpoint: {}", e))))?;
  let endpoint = configure_endpoint(endpoint, remote_config.timeout);
  let channel = endpoint
    .connect()
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Failed to connect: {}", e))))?;

  let message_limit = clamp_message_limit(resolved_max_file_size(remote_config));
  Ok(
    DeployServiceClient::new(channel)
      .max_decoding_message_size(message_limit)
      .max_encoding_message_size(message_limit),
  )
}

fn prepare_auth_resources(provider: &dyn ConfigProvider) -> Result<AuthResources> {
  let key_paths = provider.get_key_paths()?;
  let private_key_path = key_paths.private_key;
  let public_key_path = key_paths.public_key;

  let keypair = Auth::load_key_pair(&private_key_path.to_string_lossy()).map_err(|e| {
    Box::new(AdeployError::Auth(format!(
      "Failed to load SSH key pair: {}",
      e
    )))
  })?;
  let ssh_auth = Auth::with_key_pair(keypair);

  let public_key = Auth::load_public_key(&public_key_path).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to load public key: {}",
      e
    )))
  })?;

  Ok(AuthResources {
    ssh_auth,
    public_key,
  })
}

fn select_packages(
  config: &ClientConfig,
  package_names: Option<Vec<String>>,
) -> Result<Vec<(String, &ClientPackageConfig)>> {
  let Some(names) = package_names else {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  };

  let packages: Vec<_> = names
    .into_iter()
    .filter_map(|name| config.packages.get(&name).map(|pkg| (name, pkg)))
    .collect();

  if packages.is_empty() {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  }

  Ok(packages)
}

async fn deploy_single_package(
  deploy_manager: &DeployManager,
  client: &mut DeployServiceClient<Channel>,
  ssh_auth: &Auth,
  public_key: &str,
  package_name: &str,
  package_config: &ClientPackageConfig,
  max_file_size: u64,
) -> Result<()> {
  info!("Deploying {}", package_name);

  let (archive_data, file_hash) = deploy_manager
    .package_files(package_name, package_config)
    .await?;

  enforce_client_archive_size(&archive_data, max_file_size)?;

  let signature = ssh_auth
    .sign_data(&archive_data)
    .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign data: {}", e))))?;

  let request = tonic::Request::new(DeployRequest {
    package_name: package_name.to_string(),
    version: "1.0.0".to_string(),
    file_data: archive_data,
    file_hash,
    signature: general_purpose::STANDARD.encode(&signature),
    public_key: public_key.to_string(),
    metadata: std::collections::HashMap::new(),
  });

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
    Ok(())
  } else {
    error!(
      "Deployment failed for {}: {}",
      package_name, deploy_response.message
    );
    for log_line in &deploy_response.logs {
      error!("{}", log_line);
    }
    Err(Box::new(AdeployError::Deploy(format!(
      "Package {} deployment failed: {}",
      package_name, deploy_response.message
    ))))
  }
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
