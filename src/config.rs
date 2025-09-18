use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::error::{AdeployError, Result};

/// Client configuration structure based on DESIGN.md
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
  pub packages: HashMap<String, ClientPackageConfig>,
  pub servers: HashMap<String, RemoteConfig>,
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
  pub key_path: Option<String>,
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
  pub pre_deploy_script: Option<String>,
  pub post_deploy_script: Option<String>,
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

/// Load client configuration from TOML file
pub fn load_client_config<P: AsRef<Path>>(path: P) -> Result<ClientConfig> {
  let content = std::fs::read_to_string(path).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to read config file: {}",
      e
    )))
  })?;

  let config: ClientConfig = toml::from_str(&content).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to parse TOML config: {}",
      e
    )))
  })?;

  Ok(config)
}

/// Load server configuration from TOML file
pub fn load_server_config<P: AsRef<Path>>(path: P) -> Result<ServerConfig> {
  let content = std::fs::read_to_string(path).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to read config file: {}",
      e
    )))
  })?;

  let config: ServerConfig = toml::from_str(&content).map_err(|e| {
    Box::new(AdeployError::Config(format!(
      "Failed to parse TOML config: {}",
      e
    )))
  })?;

  Ok(config)
}

/// Get server configuration by IP address, fallback to default if not found
pub fn get_remote_config<'a>(
  client_config: &'a ClientConfig,
  ip: &str,
) -> Option<&'a RemoteConfig> {
  client_config
    .servers
    .get(ip)
    .or_else(|| client_config.servers.get("default"))
}
