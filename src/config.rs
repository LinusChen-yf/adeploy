use std::{
  collections::HashMap,
  env, fs,
  path::{Path, PathBuf},
};

use log2::*;
use serde::{Deserialize, Serialize};

use crate::{
  auth::Auth,
  error::{AdeployError, Result},
};

pub enum ConfigType {
  Client,
  Server,
}

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
pub trait ConfigProvider: Send + Sync {
  fn get_config_path(&self, config_type: ConfigType) -> Result<PathBuf>;

  fn load_client_config(&self, path: &Path) -> Result<ClientConfig>;
  fn load_server_config(&self, path: &Path) -> Result<ServerConfig>;

  fn get_key_paths(&self) -> Result<KeyPairPaths>;
}

/// Default provider that reads TOML files from disk and stores keys next to the binary.
#[derive(Default, Clone)]
pub struct ConfigProviderImpl;

impl ConfigProvider for ConfigProviderImpl {
  fn get_config_path(&self, config_type: ConfigType) -> Result<PathBuf> {
    let config_name = match config_type {
      ConfigType::Client => "client_config.toml",
      ConfigType::Server => "server_config.toml",
    };

    let Ok(exe_path) = env::current_exe() else {
      return Err(Box::new(AdeployError::FileSystem(
        "Failed to get current executable path".to_string(),
      )));
    };

    Ok(exe_path.join(config_name))
  }

  fn load_client_config(&self, path: &Path) -> Result<ClientConfig> {
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

  fn load_server_config(&self, path: &Path) -> Result<ServerConfig> {
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

  fn get_key_paths(&self) -> Result<KeyPairPaths> {
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
