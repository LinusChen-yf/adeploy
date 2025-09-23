use std::{fs, path::Path};

use chrono::{DateTime, Utc};
use flate2::{write::GzEncoder, Compression};
use log2::*;
use sha2::{Digest, Sha256};
use tar::Builder;
use tokio::process::Command;
use uuid::Uuid;

use crate::{
  config::{ClientPackageConfig, ServerPackageConfig},
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

  /// Package files from sources with hash verification
  pub fn package_files(
    &self,
    _package_name: &str,
    config: &ClientPackageConfig,
  ) -> Result<(Vec<u8>, String)> {
    info!("Packaging sources: {:?}", config.sources);

    let mut archive = Vec::new();
    {
      let encoder = GzEncoder::new(&mut archive, Compression::default());
      let mut tar = Builder::new(encoder);

      // Iterate over configured paths
      for source_path in &config.sources {
        let path = Path::new(source_path);

        if !path.exists() {
          return Err(Box::new(AdeployError::FileSystem(format!(
            "Source path '{}' does not exist",
            source_path
          ))));
        }

        if path.is_file() {
          // Archive a single file
          let file_name = path
            .file_name()
            .ok_or_else(|| Box::new(AdeployError::FileSystem("Invalid file name".to_string())))?
            .to_string_lossy()
            .to_string();

          tar.append_path_with_name(path, file_name).map_err(|e| {
            Box::new(AdeployError::FileSystem(format!(
              "Failed to add file '{}' to archive: {}",
              source_path, e
            )))
          })?;
          info!("Archived file {}", source_path);
        } else if path.is_dir() {
          // Archive a directory tree
          tar.append_dir_all("", path).map_err(|e| {
            Box::new(AdeployError::FileSystem(format!(
              "Failed to add directory '{}' to archive: {}",
              source_path, e
            )))
          })?;

          info!("Archived directory {}", source_path);
        }
      }

      tar.finish().map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to finalize archive: {}",
          e
        )))
      })?;
    }

    // Compute SHA256 for the archive
    let mut hasher = Sha256::new();
    hasher.update(&archive);
    let hash = format!("{:x}", hasher.finalize());

    info!("Created package ({} bytes, hash {})", archive.len(), hash);
    Ok((archive, hash))
  }

  /// Extract and deploy files with hash verification
  pub async fn extract_files(
    &self,
    archive_data: &[u8],
    expected_hash: &str,
    config: &ServerPackageConfig,
    package_name: &str,
  ) -> Result<()> {
    info!("Extracting files into {}", config.deploy_path);
    info!("Archive size: {} bytes", archive_data.len());

    // Verify hash before extraction
    let mut hasher = Sha256::new();
    hasher.update(archive_data);
    let actual_hash = format!("{:x}", hasher.finalize());

    if actual_hash != expected_hash {
      error!(
        "Hash mismatch: expected {}, actual {}",
        expected_hash, actual_hash
      );
      return Err(Box::new(AdeployError::Deploy(format!(
        "Hash verification failed. Expected: {}, Actual: {}",
        expected_hash, actual_hash
      ))));
    }

    // Create backup if enabled
    if config.backup_enabled {
      info!("Creating backup snapshot");
      self.create_backup(config, package_name).await?;
    }

    // Ensure deploy path exists
    fs::create_dir_all(&config.deploy_path).map_err(|e| {
      error!("Failed to create deploy directory: {}", e);
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create deploy directory: {}",
        e
      )))
    })?;

    // Extract archive
    let decoder = flate2::read::GzDecoder::new(archive_data);
    let mut archive = tar::Archive::new(decoder);

    archive.unpack(&config.deploy_path).map_err(|e| {
      error!("Failed to extract archive: {}", e);
      Box::new(AdeployError::Deploy(format!(
        "Failed to extract archive: {}",
        e
      )))
    })?;

    info!("Extraction complete: {}", config.deploy_path);
    Ok(())
  }

  /// Execute before-deployment script
  pub async fn execute_before_deploy_script(
    &self,
    config: &ServerPackageConfig,
  ) -> Result<Vec<String>> {
    if let Some(script_path) = &config.before_deploy_script {
      info!("Running Before-deploy script {}", script_path);
      match self.execute_script(script_path).await {
        Ok(logs) => {
          info!("Before-deploy script succeeded");
          Ok(logs)
        }
        Err(e) => {
          error!("Before-deploy script failed: {}", e);
          Err(e)
        }
      }
    } else {
      info!("No Before-deploy script configured");
      Ok(vec![])
    }
  }

  /// Execute after-deployment script
  pub async fn execute_after_deploy_script(
    &self,
    config: &ServerPackageConfig,
  ) -> Result<Vec<String>> {
    if let Some(script_path) = &config.after_deploy_script {
      info!("Running After-deploy script {}", script_path);
      match self.execute_script(script_path).await {
        Ok(logs) => {
          info!("After-deploy script succeeded");
          Ok(logs)
        }
        Err(e) => {
          error!("After-deploy script failed: {}", e);
          Err(e)
        }
      }
    } else {
      info!("No After-deploy script configured");
      Ok(vec![])
    }
  }

  /// Execute a shell script
  async fn execute_script(&self, script_path: &str) -> Result<Vec<String>> {
    let mut command = if cfg!(target_os = "windows") {
      let mut cmd = Command::new("cmd");
      cmd.arg("/C").arg(script_path);
      cmd
    } else {
      let mut cmd = Command::new("sh");
      cmd.arg("-c").arg(script_path);
      cmd
    };

    let output = command.output().await.map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Failed to execute script '{}': {}",
        script_path, e
      )))
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut logs = vec![];
    if !stdout.is_empty() {
      info!("Script stdout: {}", stdout.trim_end());
      logs.extend(stdout.lines().map(|s| s.to_string()));
    }
    if !stderr.is_empty() {
      warn!("Script stderr: {}", stderr.trim_end());
      logs.extend(stderr.lines().map(|s| format!("STDERR: {}", s)));
    }

    if !output.status.success() {
      let exit_code = output.status.code().unwrap_or(-1);
      error!("Script {} failed with exit code {}", script_path, exit_code);
      return Err(Box::new(AdeployError::Deploy(format!(
        "Script '{}' execution failed with exit code: {}",
        script_path, exit_code
      ))));
    }

    info!("Script {} completed", script_path);
    Ok(logs)
  }

  /// Create backup of existing deployment
  async fn create_backup(&self, config: &ServerPackageConfig, package_name: &str) -> Result<()> {
    if !config.backup_enabled {
      warn!("Backup disabled for {}", package_name);
      return Ok(());
    }

    // Choose backup directory
    let backup_dir_path = match &config.backup_path {
      Some(path) => {
        info!("Using custom backup path {}", path);
        Path::new(path).to_path_buf()
      }
      None => {
        // Use executable directory as base
        let current_exe = std::env::current_exe().map_err(|e| {
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

        // Append package name to backup path
        current_dir.join(package_name)
      }
    };

    std::fs::create_dir_all(&backup_dir_path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create backup directory: {}",
        e
      )))
    })?;

    info!("Creating backup at {}", backup_dir_path.display());

    // Copy deployment into timestamped folder
    let backup_name = format!("backup_{}", self.start_time.format("%Y%m%d_%H%M%S"));
    let backup_full_path = backup_dir_path.join(backup_name);

    if Path::new(&config.deploy_path).exists() {
      self
        .copy_directory(&config.deploy_path, &backup_full_path.to_string_lossy())
        .await?;
      info!("Backup stored at {}", backup_full_path.display());
    } else {
      info!(
        "No existing deployment at {}; skipping backup",
        config.deploy_path
      );
    }

    if backup_full_path.exists() {
      for entry in backup_full_path.read_dir()? {
        let entry = entry?;
        info!("Backup item: {}", entry.file_name().to_string_lossy());
      }
    }
    Ok(())
  }

  /// Copy directory recursively
  async fn copy_directory(&self, src: &str, dst: &str) -> Result<()> {
    info!("Copying {} -> {}", src, dst);

    let output = Command::new("cp")
      .arg("-r")
      .arg(src)
      .arg(dst)
      .output()
      .await
      .map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to copy directory: {}",
          e
        )))
      })?;

    if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(Box::new(AdeployError::FileSystem(format!(
        "Directory copy failed: {}",
        stderr
      ))));
    }

    info!("Copied {} -> {}", src, dst);
    Ok(())
  }
}

impl Default for DeployManager {
  fn default() -> Self {
    Self::new()
  }
}
