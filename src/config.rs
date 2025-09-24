use std::{
  collections::HashMap,
  env, fs,
  path::{Path, PathBuf},
};

#[cfg(test)]
use mockall::automock;

use log2::*;
use serde::{Deserialize, Serialize};

use crate::{
  auth::Auth,
  error::{AdeployError, Result},
};

/// Absolute paths to the client's signing key pair.
#[derive(Clone, Debug)]
pub struct KeyPairPaths {
  pub private_key: PathBuf,
  pub public_key: PathBuf,
}

impl KeyPairPaths {
  pub fn new(private_key: PathBuf, public_key: PathBuf) -> Self {
    Self {
      private_key,
      public_key,
    }
  }
}

/// Abstraction over how configuration and key material are discovered.
#[cfg_attr(test, automock)]
pub trait ConfigProvider: Send + Sync {
  fn load_client_config(&self, path: &Path) -> Result<ClientConfig>;
  fn load_server_config(&self, path: &Path) -> Result<ServerConfig>;
  fn resolve_key_paths(&self, remote_config: &RemoteConfig) -> Result<KeyPairPaths>;
}

/// Default provider that reads TOML files from disk and stores keys next to the binary.
#[derive(Default)]
pub struct FileConfigProvider;

impl ConfigProvider for FileConfigProvider {
  fn load_client_config(&self, path: &Path) -> Result<ClientConfig> {
    read_client_config(path)
  }

  fn load_server_config(&self, path: &Path) -> Result<ServerConfig> {
    read_server_config(path)
  }

  fn resolve_key_paths(&self, _remote_config: &RemoteConfig) -> Result<KeyPairPaths> {
    default_key_paths()
  }
}

/// Client configuration structure based on DESIGN.md
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
  pub packages: HashMap<String, ClientPackageConfig>,
  pub remotes: HashMap<String, RemoteConfig>,
}

/// Package configuration for client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientPackageConfig {
  pub sources: Vec<String>,
}

/// Remote server configuration for client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
  pub port: u16,
  pub timeout: u64,
  #[serde(default)]
  pub max_file_size: Option<u64>,
}

/// Server deployment configuration structure based on DESIGN.md
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
  pub packages: HashMap<String, ServerPackageConfig>,
  pub server: ServerSettings,
}

/// Package deployment configuration for server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerPackageConfig {
  pub deploy_path: String,
  pub before_deploy_script: Option<String>,
  pub after_deploy_script: Option<String>,
  #[serde(default)]
  pub backup_enabled: bool,
  pub backup_path: Option<String>,
}

/// Server settings configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
  pub port: u16,
  pub max_file_size: u64,
  pub allowed_keys: Vec<String>,
}

/// Load client configuration using the active provider
pub fn load_client_config<P: AsRef<Path>>(path: P) -> Result<ClientConfig> {
  let provider = FileConfigProvider::default();
  load_client_config_with_provider(&provider, path.as_ref())
}

/// Load server configuration using the active provider
pub fn load_server_config<P: AsRef<Path>>(path: P) -> Result<ServerConfig> {
  let provider = FileConfigProvider::default();
  load_server_config_with_provider(&provider, path.as_ref())
}

/// Locate key material for a remote using the active provider
#[allow(dead_code)]
pub fn resolve_key_paths(remote_config: &RemoteConfig) -> Result<KeyPairPaths> {
  let provider = FileConfigProvider::default();
  resolve_key_paths_with_provider(&provider, remote_config)
}

/// Load client configuration with an explicit provider
pub fn load_client_config_with_provider<P>(provider: &P, path: &Path) -> Result<ClientConfig>
where
  P: ConfigProvider + ?Sized,
{
  provider.load_client_config(path)
}

/// Load server configuration with an explicit provider
pub fn load_server_config_with_provider<P>(provider: &P, path: &Path) -> Result<ServerConfig>
where
  P: ConfigProvider + ?Sized,
{
  provider.load_server_config(path)
}

/// Resolve key paths with an explicit provider
pub fn resolve_key_paths_with_provider<P>(
  provider: &P,
  remote_config: &RemoteConfig,
) -> Result<KeyPairPaths>
where
  P: ConfigProvider + ?Sized,
{
  provider.resolve_key_paths(remote_config)
}

/// Get server configuration by IP address, fallback to default if not found
pub fn get_remote_config<'a>(
  client_config: &'a ClientConfig,
  ip: &str,
) -> Option<&'a RemoteConfig> {
  client_config
    .remotes
    .get(ip)
    .or_else(|| client_config.remotes.get("default"))
}

/// Resolve a configuration file path next to the running executable
pub fn resolve_default_config_path(config_name: &str) -> PathBuf {
  env::current_exe()
    .ok()
    .and_then(|exe| exe.parent().map(|dir| dir.join(config_name)))
    .unwrap_or_else(|| PathBuf::from(config_name))
}

fn read_client_config(path: &Path) -> Result<ClientConfig> {
  let content = fs::read_to_string(path).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to read config file: {}",
      e
    )))
  })?;

  toml::from_str(&content).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to parse TOML config: {}",
      e
    )))
  })
}

fn read_server_config(path: &Path) -> Result<ServerConfig> {
  let content = fs::read_to_string(path).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to read config file: {}",
      e
    )))
  })?;

  toml::from_str(&content).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to parse TOML config: {}",
      e
    )))
  })
}

fn default_key_paths() -> Result<KeyPairPaths> {
  let exe_dir = executable_dir()?;
  let key_dir = exe_dir.join(".key");
  let private_key_path = key_dir.join("id_ed25519");
  let public_key_path = key_dir.join("id_ed25519.pub");

  if !key_dir.exists() {
    fs::create_dir_all(&key_dir).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create key directory: {}",
        e
      )))
    })?;
  }

  if !private_key_path.exists() || !public_key_path.exists() {
    info!("Generating Ed25519 key pair");
    Auth::generate_key_pair(
      &public_key_path.to_string_lossy(),
      &private_key_path.to_string_lossy(),
    )?;
    info!("Stored key pair in {:?}", key_dir);
  }

  Ok(KeyPairPaths::new(private_key_path, public_key_path))
}

fn executable_dir() -> Result<PathBuf> {
  let current_exe = env::current_exe().map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to get current executable path: {}",
      e
    )))
  })?;

  let current_dir = current_exe.parent().ok_or_else(|| {
    Box::new(AdeployError::FileSystem(
      "Failed to get parent directory of executable".to_string(),
    ))
  })?;

  Ok(current_dir.to_path_buf())
}
