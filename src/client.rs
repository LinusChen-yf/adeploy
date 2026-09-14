use std::{convert::TryFrom, path::PathBuf, time::Duration};

use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tonic::transport::{Channel, Endpoint};

/// Cap on the response, which carries only the server's deploy log.
///
/// The request has no client-side cap: how large an archive may be is the
/// server's policy, and it reports its own limit when it refuses one.
const MAX_RESPONSE_SIZE: usize = 16 * 1024 * 1024;

use crate::{
  adeploy::{
    deploy_log::Level as DeployLogLevel, deploy_service_client::DeployServiceClient, DeployLog,
    DeployRequest,
  },
  auth::Auth,
  config::{ConfigProvider, LoadedConfig, ResolvedRemote},
  deploy::DeployManager,
  error::{AdeployError, Result},
};

/// Deploy specific packages using an explicit provider
pub async fn deploy(
  host: &str,
  package_names: Option<Vec<String>>,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let loaded = provider.load()?;
  info!("Loaded configuration from {}", loaded.path.display());

  let remote = loaded.config.resolve_remote(host);
  let mut client = connect_deploy_client(host, &remote).await?;
  let deploy_manager = DeployManager::new();
  let auth_resources = prepare_auth_resources(provider)?;
  let packages_to_deploy = select_packages(&loaded, package_names)?;

  for package in packages_to_deploy {
    deploy_single_package(
      &deploy_manager,
      &mut client,
      &auth_resources.ssh_auth,
      &auth_resources.public_key,
      &package,
      remote.deploy_timeout,
    )
    .await?;
  }

  Ok(())
}

struct AuthResources {
  ssh_auth: Auth,
  public_key: String,
}

/// A package selected for deployment, with its sources already resolved to
/// absolute paths against the directory holding `adeploy.toml`.
struct SelectedPackage {
  name: String,
  sources: Vec<PathBuf>,
}

async fn connect_deploy_client(
  host: &str,
  remote: &ResolvedRemote,
) -> Result<DeployServiceClient<Channel>> {
  info!("Connecting to {}:{} for deployment", host, remote.port);

  let endpoint_uri = format!("http://{}:{}", host, remote.port);
  let endpoint = Channel::from_shared(endpoint_uri)
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid endpoint: {}", e))))?;
  let endpoint = configure_endpoint(endpoint, remote);
  let channel = endpoint
    .connect()
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Failed to connect: {}", e))))?;

  Ok(DeployServiceClient::new(channel).max_decoding_message_size(MAX_RESPONSE_SIZE))
}

fn prepare_auth_resources(provider: &dyn ConfigProvider) -> Result<AuthResources> {
  let key_paths = provider.get_key_paths()?;
  let private_key_path = key_paths.private_key;
  let public_key_path = key_paths.public_key;

  let keypair = Auth::load_key_pair(&private_key_path.to_string_lossy()).map_err(|e| {
    Box::new(AdeployError::Auth(format!(
      "Failed to load signing key pair: {}",
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
  loaded: &LoadedConfig,
  package_names: Option<Vec<String>>,
) -> Result<Vec<SelectedPackage>> {
  let Some(names) = package_names else {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  };

  let packages: Vec<SelectedPackage> = names
    .into_iter()
    .filter_map(|name| {
      loaded
        .config
        .resolved_sources(&name, &loaded.base_dir)
        .map(|sources| SelectedPackage { name, sources })
    })
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
  package: &SelectedPackage,
  deploy_timeout: u64,
) -> Result<()> {
  let package_name = package.name.as_str();
  info!("Deploying {}", package_name);

  let (archive_data, file_hash) = deploy_manager
    .package_files(package_name, &package.sources)
    .await?;

  let signature = ssh_auth
    .sign_data(&archive_data)
    .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign data: {}", e))))?;

  let mut request = tonic::Request::new(DeployRequest {
    package_name: package_name.to_string(),
    version: "1.0.0".to_string(),
    file_data: archive_data,
    file_hash,
    signature: general_purpose::STANDARD.encode(&signature),
    public_key: public_key.to_string(),
    metadata: std::collections::HashMap::new(),
  });

  // Send the deadline with the request rather than keeping it on the channel.
  // `Endpoint::timeout` is client-side only, so the server kept unpacking and
  // running hooks after the client had already given up. The `grpc-timeout`
  // metadata this sets is honoured by tonic on both ends, so the two stop
  // together and cannot disagree about when.
  if deploy_timeout > 0 {
    request.set_timeout(Duration::from_secs(deploy_timeout));
  }

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
      log_deploy_server_entry(log_line);
    }
    Ok(())
  } else {
    error!(
      "Deployment failed for {}: {}",
      package_name, deploy_response.message
    );
    for log_line in &deploy_response.logs {
      log_deploy_server_entry(log_line);
    }
    Err(Box::new(AdeployError::Deploy(format!(
      "Package {} deployment failed: {}",
      package_name, deploy_response.message
    ))))
  }
}

/// Bound only how long reaching the host may take.
///
/// The deployment deadline rides on the request instead, so the server learns
/// about it too.
fn configure_endpoint(endpoint: Endpoint, remote: &ResolvedRemote) -> Endpoint {
  if remote.connect_timeout == 0 {
    endpoint
  } else {
    endpoint.connect_timeout(Duration::from_secs(remote.connect_timeout))
  }
}

fn log_deploy_server_entry(entry: &DeployLog) {
  let level = DeployLogLevel::try_from(entry.level).unwrap_or(DeployLogLevel::Info);
  let message = entry.message.as_str();
  match level {
    DeployLogLevel::Error => error!("{}", message),
    DeployLogLevel::Warn => warn!("{}", message),
    DeployLogLevel::Unspecified | DeployLogLevel::Info => info!("{}", message),
  }
}
