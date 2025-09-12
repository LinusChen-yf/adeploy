use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::error::{AdeployError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
  pub package: PackageConfig,
  pub server: ServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageConfig {
  pub name: String,
  pub version: String,
  pub path: String,
  pub exclude: Vec<String>,
  pub include_hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
  pub host: String,
  pub port: u16,
  pub ssh_key_path: String,
  pub timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDeployConfig {
  pub packages: HashMap<String, DeployPackageConfig>,
  pub server: ServerSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployPackageConfig {
  pub deploy_path: String,
  pub pre_deploy_script: Option<String>,
  pub post_deploy_script: Option<String>,
  pub backup_enabled: bool,
  pub backup_path: Option<String>,
  pub owner: Option<String>,
  pub permissions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
  pub port: u16,
  pub max_file_size: u64,
  pub allowed_ssh_keys: Vec<String>,
}

pub fn load_client_config<P: AsRef<Path>>(path: P) -> Result<ClientConfig> {
  let content = std::fs::read_to_string(path)
    .map_err(|e| AdeployError::Config(format!("Failed to read config file: {}", e)))?;

  // Use a simple JSON-based configuration for now to avoid Rhai Send issues
  let config: ClientConfig = serde_json::from_str(&content)
    .map_err(|e| AdeployError::Config(format!("Failed to parse JSON config: {}", e)))?;

  Ok(config)
}

pub fn load_server_config<P: AsRef<Path>>(path: P) -> Result<ServerDeployConfig> {
  let content = std::fs::read_to_string(path)
    .map_err(|e| AdeployError::Config(format!("Failed to read config file: {}", e)))?;

  // Use a simple JSON-based configuration for now to avoid Rhai Send issues
  let config: ServerDeployConfig = serde_json::from_str(&content)
    .map_err(|e| AdeployError::Config(format!("Failed to parse JSON config: {}", e)))?;

  Ok(config)
}
