use std::{fs, path::Path, process::Command};

use chrono::{DateTime, Utc};
use flate2::{write::GzEncoder, Compression};
use log2::*;
use sha2::{Digest, Sha256};
use tar::Builder;
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
    info!("Packaging files from sources: {:?}", config.sources);

    let mut archive = Vec::new();
    {
      let encoder = GzEncoder::new(&mut archive, Compression::default());
      let mut tar = Builder::new(encoder);

      // Process each source path in the sources list
      for source_path in &config.sources {
        let path = Path::new(source_path);

        if !path.exists() {
          return Err(Box::new(AdeployError::FileSystem(format!(
            "Source path '{}' does not exist",
            source_path
          ))));
        }

        if path.is_file() {
          // Add single file to archive
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

          info!("Added file: {}", source_path);
        } else if path.is_dir() {
          // Add entire directory to archive
          tar.append_dir_all("", path).map_err(|e| {
            Box::new(AdeployError::FileSystem(format!(
              "Failed to add directory '{}' to archive: {}",
              source_path, e
            )))
          })?;

          info!("Added directory: {}", source_path);
        }
      }

      tar.finish().map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to finalize archive: {}",
          e
        )))
      })?;
    }

    // Calculate SHA256 hash of the archive
    let mut hasher = Sha256::new();
    hasher.update(&archive);
    let hash = format!("{:x}", hasher.finalize());

    info!(
      "Package created, size: {} bytes, hash: {}",
      archive.len(),
      hash
    );
    Ok((archive, hash))
  }

  /// Extract and deploy files with hash verification
  pub fn extract_files(
    &self,
    archive_data: &[u8],
    expected_hash: &str,
    config: &ServerPackageConfig,
    package_name: &str,
  ) -> Result<()> {
    info!(
      "Starting file extraction process to: {}",
      config.deploy_path
    );
    info!("Archive size: {} bytes", archive_data.len());

    // Verify hash before extraction
    info!("Verifying file hash...");
    let mut hasher = Sha256::new();
    hasher.update(archive_data);
    let actual_hash = format!("{:x}", hasher.finalize());

    if actual_hash != expected_hash {
      error!(
        "Hash verification failed. Expected: {}, Actual: {}",
        expected_hash, actual_hash
      );
      return Err(Box::new(AdeployError::Deploy(format!(
        "Hash verification failed. Expected: {}, Actual: {}",
        expected_hash, actual_hash
      ))));
    }

    info!("Hash verification successful: {}", actual_hash);

    // Create backup if enabled
    if config.backup_enabled {
      info!("Backup is enabled, creating backup...");
      self.create_backup(config, package_name)?;
    }

    // Ensure deploy path exists
    info!("Ensuring deploy directory exists: {}", config.deploy_path);
    fs::create_dir_all(&config.deploy_path).map_err(|e| {
      error!("Failed to create deploy directory: {}", e);
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create deploy directory: {}",
        e
      )))
    })?;

    // Extract archive
    info!("Extracting archive...");
    let decoder = flate2::read::GzDecoder::new(archive_data);
    let mut archive = tar::Archive::new(decoder);

    archive.unpack(&config.deploy_path).map_err(|e| {
      error!("Failed to extract archive: {}", e);
      Box::new(AdeployError::Deploy(format!(
        "Failed to extract archive: {}",
        e
      )))
    })?;

    info!("Files extracted successfully to: {}", config.deploy_path);
    Ok(())
  }

  /// Execute before-deployment script
  pub fn execute_before_deploy_script(&self, config: &ServerPackageConfig) -> Result<Vec<String>> {
    if let Some(script_path) = &config.before_deploy_script {
      info!("Executing before-deploy script: {}", script_path);
      match self.execute_script(script_path) {
        Ok(logs) => {
          info!("Before-deploy script executed successfully");
          Ok(logs)
        }
        Err(e) => {
          error!("Before-deploy script failed: {}", e);
          Err(e)
        }
      }
    } else {
      info!("No before-deploy script configured");
      Ok(vec![])
    }
  }

  /// Execute after-deployment script
  pub fn execute_after_deploy_script(&self, config: &ServerPackageConfig) -> Result<Vec<String>> {
    if let Some(script_path) = &config.after_deploy_script {
      info!("Executing after-deploy script: {}", script_path);
      match self.execute_script(script_path) {
        Ok(logs) => {
          info!("After-deploy script executed successfully");
          Ok(logs)
        }
        Err(e) => {
          error!("After-deploy script failed: {}", e);
          Err(e)
        }
      }
    } else {
      info!("No after-deploy script configured");
      Ok(vec![])
    }
  }

  /// Execute a shell script
  fn execute_script(&self, script_path: &str) -> Result<Vec<String>> {
    info!("Executing script: {}", script_path);

    let output = Command::new("sh")
      .arg("-c")
      .arg(script_path)
      .output()
      .map_err(|e| {
        Box::new(AdeployError::Deploy(format!(
          "Failed to execute script '{}': {}",
          script_path, e
        )))
      })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut logs = vec![];
    if !stdout.is_empty() {
      info!("Script stdout: {}", stdout);
      logs.extend(stdout.lines().map(|s| s.to_string()));
    }
    if !stderr.is_empty() {
      warn!("Script stderr: {}", stderr);
      logs.extend(stderr.lines().map(|s| format!("STDERR: {}", s)));
    }

    if !output.status.success() {
      let exit_code = output.status.code().unwrap_or(-1);
      error!(
        "Script '{}' failed with exit code: {}",
        script_path, exit_code
      );
      return Err(Box::new(AdeployError::Deploy(format!(
        "Script '{}' execution failed with exit code: {}",
        script_path, exit_code
      ))));
    }

    info!("Script '{}' executed successfully", script_path);
    Ok(logs)
  }

  /// Create backup of existing deployment
  fn create_backup(&self, config: &ServerPackageConfig, package_name: &str) -> Result<()> {
    if !config.backup_enabled {
      error!("Backup is disabled for package: {}", package_name);
      return Ok(());
    }

    // Determine backup directory path
    let backup_dir_path = match &config.backup_path {
      Some(path) => {
        info!("Using custom backup path: {}", path);
        Path::new(path).to_path_buf()
      }
      None => {
        // Get current executable directory
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

        // Create backup directory with package name
        current_dir.join(package_name)
      }
    };

    std::fs::create_dir_all(&backup_dir_path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create backup directory: {}",
        e
      )))
    })?;

    info!("Creating backup at: {}", backup_dir_path.display());

    // Copy current deployment to backup with timestamp
    let backup_name = format!("backup_{}", self.start_time.format("%Y%m%d_%H%M%S"));
    let backup_full_path = backup_dir_path.join(backup_name);

    if Path::new(&config.deploy_path).exists() {
      self.copy_directory(&config.deploy_path, &backup_full_path.to_string_lossy())?;
      info!(
        "Backup created successfully at: {}",
        backup_full_path.display()
      );
    } else {
      info!(
        "No existing deployment found at: {}, skipping backup",
        config.deploy_path
      );
    }

    if backup_full_path.exists() {
      for entry in backup_full_path.read_dir()? {
        let entry = entry?;
        info!("Backup file: {}", entry.file_name().to_string_lossy());
      }
    }
    Ok(())
  }

  /// Copy directory recursively
  fn copy_directory(&self, src: &str, dst: &str) -> Result<()> {
    info!("Copying directory from {} to {}", src, dst);

    let output = Command::new("cp")
      .arg("-r")
      .arg(src)
      .arg(dst)
      .output()
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

    info!("Directory copied successfully from {} to {}", src, dst);
    Ok(())
  }
}

impl Default for DeployManager {
  fn default() -> Self {
    Self::new()
  }
}
