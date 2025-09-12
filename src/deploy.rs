use std::{path::Path, process::Command};

use chrono::{DateTime, Utc};
use flate2::{write::GzEncoder, Compression};
use log2::*;
use tar::Builder;
use uuid::Uuid;

use crate::{
  config::{DeployPackageConfig, PackageConfig},
  error::{AdeployError, Result},
};

/// Deployment manager
pub struct DeployManager {
  pub deploy_id: String,
  pub start_time: DateTime<Utc>,
}

impl DeployManager {
  pub fn new() -> Self {
    Self {
      deploy_id: Uuid::new_v4().to_string(),
      start_time: Utc::now(),
    }
  }

  /// Scan and package files for deployment
  pub fn package_files(&self, package_name: &str, config: &PackageConfig) -> Result<Vec<u8>> {
    info!("Packaging files from sources: {:?}", config.sources);

    let mut archive = Vec::new();
    {
      let encoder = GzEncoder::new(&mut archive, Compression::default());
      let mut tar = Builder::new(encoder);

      // Add files from each source to archive
      for (index, source_path) in config.sources.iter().enumerate() {
        let source_name = if config.sources.len() == 1 {
          package_name.to_string()
        } else {
          format!("{}/source_{}", package_name, index)
        };

        tar.append_dir_all(&source_name, source_path).map_err(|e| {
          AdeployError::FileSystem(format!(
            "Failed to add source '{}' to archive: {}",
            source_path, e
          ))
        })?;
      }

      tar
        .finish()
        .map_err(|e| AdeployError::FileSystem(format!("Failed to finalize archive: {}", e)))?;
    }

    info!("Package created, size: {} bytes", archive.len());
    Ok(archive)
  }

  /// Extract and deploy files
  pub fn extract_files(&self, archive_data: &[u8], config: &DeployPackageConfig) -> Result<()> {
    info!("Extracting files to: {}", config.deploy_path);

    // Create backup if enabled
    if config.backup_enabled {
      self.create_backup(config)?;
    }

    // Extract archive
    let decoder = flate2::read::GzDecoder::new(archive_data);
    let mut archive = tar::Archive::new(decoder);

    archive
      .unpack(&config.deploy_path)
      .map_err(|e| AdeployError::Deploy(format!("Failed to extract archive: {}", e)))?;

    // Set permissions if specified
    if let Some(permissions) = &config.permissions {
      self.set_permissions(&config.deploy_path, permissions)?;
    }

    // Change owner if specified
    if let Some(owner) = &config.owner {
      self.change_owner(&config.deploy_path, owner)?;
    }

    info!("Files extracted successfully");
    Ok(())
  }

  /// Execute pre-deployment script
  pub fn execute_pre_deploy_script(&self, config: &DeployPackageConfig) -> Result<Vec<String>> {
    if let Some(script_path) = &config.pre_deploy_script {
      info!("Executing pre-deploy script: {}", script_path);
      self.execute_script(script_path)
    } else {
      Ok(vec![])
    }
  }

  /// Execute post-deployment script
  pub fn execute_post_deploy_script(&self, config: &DeployPackageConfig) -> Result<Vec<String>> {
    if let Some(script_path) = &config.post_deploy_script {
      info!("Executing post-deploy script: {}", script_path);
      self.execute_script(script_path)
    } else {
      Ok(vec![])
    }
  }

  /// Execute a shell script
  fn execute_script(&self, script_path: &str) -> Result<Vec<String>> {
    let output = Command::new("sh")
      .arg("-c")
      .arg(script_path)
      .output()
      .map_err(|e| AdeployError::Deploy(format!("Failed to execute script: {}", e)))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut logs = vec![];
    if !stdout.is_empty() {
      logs.extend(stdout.lines().map(|s| s.to_string()));
    }
    if !stderr.is_empty() {
      logs.extend(stderr.lines().map(|s| format!("ERROR: {}", s)));
    }

    if !output.status.success() {
      return Err(AdeployError::Deploy(format!(
        "Script execution failed with exit code: {}",
        output.status.code().unwrap_or(-1)
      )));
    }

    Ok(logs)
  }

  /// Create backup of existing deployment
  fn create_backup(&self, config: &DeployPackageConfig) -> Result<()> {
    if let Some(backup_path) = &config.backup_path {
      info!("Creating backup at: {}", backup_path);

      // Create backup directory if it doesn't exist
      std::fs::create_dir_all(backup_path).map_err(|e| {
        AdeployError::FileSystem(format!("Failed to create backup directory: {}", e))
      })?;

      // Copy current deployment to backup
      let backup_name = format!("backup_{}", self.start_time.format("%Y%m%d_%H%M%S"));
      let backup_full_path = Path::new(backup_path).join(backup_name);

      if Path::new(&config.deploy_path).exists() {
        self.copy_directory(&config.deploy_path, &backup_full_path.to_string_lossy())?;
        info!("Backup created successfully");
      }
    }
    Ok(())
  }

  /// Copy directory recursively
  fn copy_directory(&self, src: &str, dst: &str) -> Result<()> {
    let output = Command::new("cp")
      .arg("-r")
      .arg(src)
      .arg(dst)
      .output()
      .map_err(|e| AdeployError::FileSystem(format!("Failed to copy directory: {}", e)))?;

    if !output.status.success() {
      return Err(AdeployError::FileSystem(
        "Directory copy failed".to_string(),
      ));
    }

    Ok(())
  }

  /// Set file permissions
  fn set_permissions(&self, path: &str, permissions: &str) -> Result<()> {
    info!("Setting permissions {} on {}", permissions, path);

    let output = Command::new("chmod")
      .arg("-R")
      .arg(permissions)
      .arg(path)
      .output()
      .map_err(|e| AdeployError::Deploy(format!("Failed to set permissions: {}", e)))?;

    if !output.status.success() {
      return Err(AdeployError::Deploy(
        "Failed to set permissions".to_string(),
      ));
    }

    Ok(())
  }

  /// Change file owner
  fn change_owner(&self, path: &str, owner: &str) -> Result<()> {
    info!("Changing owner to {} on {}", owner, path);

    let output = Command::new("chown")
      .arg("-R")
      .arg(owner)
      .arg(path)
      .output()
      .map_err(|e| AdeployError::Deploy(format!("Failed to change owner: {}", e)))?;

    if !output.status.success() {
      return Err(AdeployError::Deploy("Failed to change owner".to_string()));
    }

    Ok(())
  }
}

impl Default for DeployManager {
  fn default() -> Self {
    Self::new()
  }
}
