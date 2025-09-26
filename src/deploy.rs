use std::{
  fs, io,
  path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use flate2::{write::GzEncoder, Compression};
use log2::*;
use sha2::{Digest, Sha256};
use tar::Builder;
use tokio::{process::Command, task::spawn_blocking};
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
  pub async fn package_files(
    &self,
    package_name: &str,
    config: &ClientPackageConfig,
  ) -> Result<(Vec<u8>, String)> {
    let package_name = package_name.to_string();
    let config = config.clone();
    spawn_blocking(move || Self::package_files_blocking(&package_name, &config))
      .await
      .map_err(|e| {
        Box::new(AdeployError::Deploy(format!(
          "Packaging task failed: {}",
          e
        )))
      })?
  }

  fn package_files_blocking(
    package_name: &str,
    config: &ClientPackageConfig,
  ) -> Result<(Vec<u8>, String)> {
    info!("Packaging {} sources: {:?}", package_name, config.sources);

    let mut archive = Vec::new();
    {
      let encoder = GzEncoder::new(&mut archive, Compression::default());
      let mut tar = Builder::new(encoder);

      for source_path in &config.sources {
        let path = Path::new(source_path);

        if !path.exists() {
          return Err(Box::new(AdeployError::FileSystem(format!(
            "Source path '{}' does not exist",
            source_path
          ))));
        }

        if path.is_file() {
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

    let mut hasher = Sha256::new();
    hasher.update(&archive);
    let hash = format!("{:x}", hasher.finalize());

    info!(
      "Created package {} ({} bytes, hash {})",
      package_name,
      archive.len(),
      hash
    );
    Ok((archive, hash))
  }

  /// Extract and deploy files with hash verification
  pub async fn extract_files(
    &self,
    archive_data: Vec<u8>,
    expected_hash: &str,
    config: &ServerPackageConfig,
    package_name: &str,
  ) -> Result<()> {
    info!("Extracting files into {}", config.deploy_path);
    info!("Archive size: {} bytes", archive_data.len());

    let archive_data = self
      .verify_archive_hash(archive_data, expected_hash)
      .await?;

    if config.backup_enabled {
      info!("Creating backup snapshot");
      self.create_backup(config, package_name).await?;
    }

    self.ensure_deploy_directory(&config.deploy_path).await?;

    self
      .unpack_archive(archive_data, &config.deploy_path)
      .await?;

    info!("Extraction complete: {}", config.deploy_path);
    Ok(())
  }

  /// Execute before-deployment script
  pub async fn execute_before_deploy_script(
    &self,
    config: &ServerPackageConfig,
  ) -> Result<Vec<String>> {
    self
      .run_deploy_script(config.before_deploy_script.as_deref(), "Before-deploy")
      .await
  }

  /// Execute after-deployment script
  pub async fn execute_after_deploy_script(
    &self,
    config: &ServerPackageConfig,
  ) -> Result<Vec<String>> {
    self
      .run_deploy_script(config.after_deploy_script.as_deref(), "After-deploy")
      .await
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

    let backup_dir_path = self.resolve_backup_directory(config, package_name)?;
    std::fs::create_dir_all(&backup_dir_path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create backup directory: {}",
        e
      )))
    })?;

    info!("Creating backup at {}", backup_dir_path.display());

    let backup_name = format!("backup_{}", self.start_time.format("%Y%m%d_%H%M%S"));
    let backup_full_path = backup_dir_path.join(backup_name);

    self.copy_existing_deploy(config, &backup_full_path).await?;
    self.log_backup_contents(&backup_full_path)?;
    Ok(())
  }

  /// Copy directory recursively
  async fn copy_directory(&self, src: &str, dst: &str) -> Result<()> {
    info!("Copying {} -> {}", src, dst);

    let src_path = PathBuf::from(src);
    let dst_path = PathBuf::from(dst);

    spawn_blocking(move || -> Result<()> {
      copy_dir_recursive(&src_path, &dst_path).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Directory copy failed: {}",
          e
        )))
      })?;
      Ok(())
    })
    .await
    .map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Directory copy task failed: {}",
        e
      )))
    })??;

    info!("Copied {} -> {}", src, dst);
    Ok(())
  }
}

impl DeployManager {
  async fn run_deploy_script(
    &self,
    script_path: Option<&str>,
    stage_name: &str,
  ) -> Result<Vec<String>> {
    let Some(path) = script_path else {
      info!("No {} script configured", stage_name);
      return Ok(vec![]);
    };

    info!("Running {} script {}", stage_name, path);
    match self.execute_script(path).await {
      Ok(logs) => {
        info!("{} script succeeded", stage_name);
        Ok(logs)
      }
      Err(e) => {
        error!("{} script failed: {}", stage_name, e);
        Err(e)
      }
    }
  }

  async fn verify_archive_hash(
    &self,
    archive_data: Vec<u8>,
    expected_hash: &str,
  ) -> Result<Vec<u8>> {
    let expected_hash = expected_hash.to_string();
    let (archive_data, actual_hash) = spawn_blocking(move || -> Result<(Vec<u8>, String)> {
      let mut hasher = Sha256::new();
      hasher.update(&archive_data);
      let actual_hash = format!("{:x}", hasher.finalize());
      Ok((archive_data, actual_hash))
    })
    .await
    .map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Hash computation task failed: {}",
        e
      )))
    })??;

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

    Ok(archive_data)
  }

  async fn ensure_deploy_directory(&self, path: &str) -> Result<()> {
    let deploy_path = PathBuf::from(path);
    spawn_blocking(move || fs::create_dir_all(&deploy_path))
      .await
      .map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Deploy directory task failed: {}",
          e
        )))
      })?
      .map_err(|e| {
        error!("Failed to create deploy directory: {}", e);
        Box::new(AdeployError::FileSystem(format!(
          "Failed to create deploy directory: {}",
          e
        )))
      })
  }

  async fn unpack_archive(&self, archive_data: Vec<u8>, deploy_path: &str) -> Result<()> {
    let deploy_path = deploy_path.to_string();
    spawn_blocking(move || -> Result<()> {
      let decoder = flate2::read::GzDecoder::new(&archive_data[..]);
      let mut archive = tar::Archive::new(decoder);
      archive.unpack(&deploy_path).map_err(|e| {
        Box::new(AdeployError::Deploy(format!(
          "Failed to extract archive: {}",
          e
        )))
      })?;
      Ok(())
    })
    .await
    .map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Archive extraction task failed: {}",
        e
      )))
    })??;
    Ok(())
  }

  fn resolve_backup_directory(
    &self,
    config: &ServerPackageConfig,
    package_name: &str,
  ) -> Result<PathBuf> {
    match &config.backup_path {
      Some(path) => {
        info!("Using custom backup path {}", path);
        Ok(Path::new(path).to_path_buf())
      }
      None => {
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

        Ok(current_dir.join(package_name))
      }
    }
  }

  async fn copy_existing_deploy(
    &self,
    config: &ServerPackageConfig,
    backup_full_path: &Path,
  ) -> Result<()> {
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
    Ok(())
  }

  fn log_backup_contents(&self, backup_full_path: &Path) -> Result<()> {
    if backup_full_path.exists() {
      for entry in backup_full_path.read_dir()? {
        let entry = entry?;
        info!("Backup item: {}", entry.file_name().to_string_lossy());
      }
    }
    Ok(())
  }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
  if !dst.exists() {
    fs::create_dir_all(dst)?;
  }

  for entry in fs::read_dir(src)? {
    let entry = entry?;
    let file_type = entry.file_type()?;
    let target = dst.join(entry.file_name());

    if file_type.is_dir() {
      copy_dir_recursive(&entry.path(), &target)?;
    } else {
      if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
      }
      fs::copy(entry.path(), &target)?;
    }
  }

  Ok(())
}

impl Default for DeployManager {
  fn default() -> Self {
    Self::new()
  }
}
