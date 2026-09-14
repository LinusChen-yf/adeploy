use std::{
  fs, io,
  io::Read,
  path::{Path, PathBuf},
  process::Stdio,
};

use chrono::{DateTime, Utc};
use flate2::{write::GzEncoder, Compression};
use log2::*;
use sha2::{Digest, Sha256};
use tar::Builder;
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::Command,
  task::spawn_blocking,
};
use uuid::Uuid;

use crate::{
  config::PackageConfig,
  deploy_log::LogSink,
  error::{AdeployError, Result},
};

/// Read size when hashing or decompressing a staged archive.
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

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
  ///
  /// `sources` are already absolute: the caller resolved them against the
  /// directory holding `adeploy.toml`, so packaging does not depend on the
  /// working directory.
  pub async fn package_files(
    &self,
    package_name: &str,
    sources: &[PathBuf],
  ) -> Result<(Vec<u8>, String)> {
    let package_name = package_name.to_string();
    let sources = sources.to_vec();
    spawn_blocking(move || Self::package_files_blocking(&package_name, &sources))
      .await
      .map_err(|e| {
        Box::new(AdeployError::Deploy(format!(
          "Packaging task failed: {}",
          e
        )))
      })?
  }

  fn package_files_blocking(package_name: &str, sources: &[PathBuf]) -> Result<(Vec<u8>, String)> {
    info!("Packaging {} sources: {:?}", package_name, sources);

    let mut archive = Vec::new();
    {
      let encoder = GzEncoder::new(&mut archive, Compression::default());
      let mut tar = Builder::new(encoder);

      for path in sources {
        if !path.exists() {
          return Err(Box::new(AdeployError::FileSystem(format!(
            "Source path '{}' does not exist",
            path.display()
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
              path.display(),
              e
            )))
          })?;
          info!("Archived file {}", path.display());
        } else if path.is_dir() {
          tar.append_dir_all("", path).map_err(|e| {
            Box::new(AdeployError::FileSystem(format!(
              "Failed to add directory '{}' to archive: {}",
              path.display(),
              e
            )))
          })?;

          info!("Archived directory {}", path.display());
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

  /// Verify and unpack an archive that was streamed to disk.
  ///
  /// The archive stays on disk throughout: it is hashed and decompressed in
  /// fixed-size reads, so the server's memory does not grow with the size of
  /// the package being deployed.
  pub async fn extract_files(
    &self,
    archive_path: &Path,
    expected_hash: &str,
    config: &PackageConfig,
    package_name: &str,
    deploy_path: &Path,
    sink: &LogSink,
  ) -> Result<()> {
    sink.info("Verifying archive hash").await;
    self
      .verify_archive_hash(archive_path, expected_hash)
      .await?;

    if config.backup_enabled {
      sink.info("Creating backup snapshot").await;
      self
        .create_backup(config, package_name, deploy_path)
        .await?;
    }

    sink
      .info(format!("Extracting files into {}", deploy_path.display()))
      .await;
    self
      .swap_into_place(archive_path, deploy_path, config.clean_deploy)
      .await?;

    info!("Extraction complete: {}", deploy_path.display());
    sink.info("Extraction complete").await;
    Ok(())
  }

  /// Execute before-deployment script
  pub async fn execute_before_deploy_script(
    &self,
    config: &PackageConfig,
    sink: &LogSink,
  ) -> Result<()> {
    self
      .run_deploy_script(
        config.before_deploy_script.as_deref(),
        "Before-deploy",
        sink,
      )
      .await
  }

  /// Execute after-deployment script
  pub async fn execute_after_deploy_script(
    &self,
    config: &PackageConfig,
    sink: &LogSink,
  ) -> Result<()> {
    self
      .run_deploy_script(config.after_deploy_script.as_deref(), "After-deploy", sink)
      .await
  }

  /// Run a hook, forwarding its output as it is produced.
  ///
  /// Output used to be collected with `Command::output()`, which waits for the
  /// process to exit. An installer that ran for minutes therefore produced
  /// nothing until it finished, which is indistinguishable from a hang.
  async fn execute_script(&self, script_path: &str, sink: &LogSink) -> Result<()> {
    let exe_dir = executable_dir()?;

    info!(
      "Executing script in adeploy directory: {}",
      exe_dir.display()
    );

    let mut command = if cfg!(target_os = "windows") {
      let mut cmd = Command::new("cmd");
      cmd.arg("/C").arg(script_path);
      cmd
    } else {
      let mut cmd = Command::new("sh");
      cmd.arg("-c").arg(script_path);
      cmd
    };

    command
      .current_dir(&exe_dir)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Failed to execute script '{}': {}",
        script_path, e
      )))
    })?;

    let stdout = child.stdout.take().ok_or_else(|| {
      Box::new(AdeployError::Deploy(
        "Failed to capture script stdout".to_string(),
      ))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
      Box::new(AdeployError::Deploy(
        "Failed to capture script stderr".to_string(),
      ))
    })?;

    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    let (mut stdout_done, mut stderr_done) = (false, false);

    while !(stdout_done && stderr_done) {
      tokio::select! {
        line = stdout_lines.next_line(), if !stdout_done => match line {
          Ok(Some(line)) => {
            info!("Script stdout: {}", line);
            sink.info(line).await;
          }
          Ok(None) => stdout_done = true,
          Err(e) => {
            warn!("Failed to read script stdout: {}", e);
            stdout_done = true;
          }
        },
        line = stderr_lines.next_line(), if !stderr_done => match line {
          Ok(Some(line)) => {
            warn!("Script stderr: {}", line);
            sink.warn(format!("STDERR: {}", line)).await;
          }
          Ok(None) => stderr_done = true,
          Err(e) => {
            warn!("Failed to read script stderr: {}", e);
            stderr_done = true;
          }
        },
      }
    }

    let status = child.wait().await.map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Failed to wait for script '{}': {}",
        script_path, e
      )))
    })?;

    if !status.success() {
      let exit_code = status.code().unwrap_or(-1);
      error!("Script {} failed with exit code {}", script_path, exit_code);
      return Err(Box::new(AdeployError::Deploy(format!(
        "Script '{}' execution failed with exit code: {}",
        script_path, exit_code
      ))));
    }

    info!("Script {} completed", script_path);
    Ok(())
  }

  /// Create backup of existing deployment
  async fn create_backup(
    &self,
    config: &PackageConfig,
    package_name: &str,
    deploy_path: &Path,
  ) -> Result<()> {
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

    self
      .copy_existing_deploy(deploy_path, &backup_full_path)
      .await?;
    self.log_backup_contents(&backup_full_path)?;
    Ok(())
  }

  /// Copy directory recursively
  async fn copy_directory(&self, src: &Path, dst: &Path) -> Result<()> {
    info!("Copying {} -> {}", src.display(), dst.display());

    let src_path = src.to_path_buf();
    let dst_path = dst.to_path_buf();

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

    info!("Copied {} -> {}", src.display(), dst.display());
    Ok(())
  }
}

impl DeployManager {
  async fn run_deploy_script(
    &self,
    script_path: Option<&str>,
    stage_name: &str,
    sink: &LogSink,
  ) -> Result<()> {
    let Some(path) = script_path else {
      info!("No {} script configured", stage_name);
      return Ok(());
    };

    info!("Running {} script {}", stage_name, path);
    sink
      .info(format!("Running {} script {}", stage_name, path))
      .await;

    match self.execute_script(path, sink).await {
      Ok(()) => {
        info!("{} script succeeded", stage_name);
        sink.info(format!("{} script succeeded", stage_name)).await;
        Ok(())
      }
      Err(e) => {
        error!("{} script failed: {}", stage_name, e);
        Err(e)
      }
    }
  }

  /// Hash a staged archive without loading it into memory.
  async fn verify_archive_hash(&self, archive_path: &Path, expected_hash: &str) -> Result<()> {
    let archive_path = archive_path.to_path_buf();
    let expected_hash = expected_hash.to_string();

    let actual_hash = spawn_blocking(move || -> Result<String> {
      let mut file = fs::File::open(&archive_path).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to open staged archive {}: {}",
          archive_path.display(),
          e
        )))
      })?;

      let mut hasher = Sha256::new();
      let mut buffer = vec![0u8; STREAM_BUFFER_SIZE];
      loop {
        let read = file.read(&mut buffer).map_err(|e| {
          Box::new(AdeployError::FileSystem(format!(
            "Failed to read staged archive: {}",
            e
          )))
        })?;
        if read == 0 {
          break;
        }
        hasher.update(&buffer[..read]);
      }

      Ok(format!("{:x}", hasher.finalize()))
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

    Ok(())
  }

  /// Build the new deployment beside the old one, then swap it in.
  ///
  /// Unpacking straight over `deploy_path` meant a failure part way through
  /// left a directory that was neither the old deployment nor the new one, and
  /// a running service could read half-replaced files for as long as the
  /// extraction took. The new tree is assembled under a sibling name and moved
  /// into place with a rename, so the only moment anything is inconsistent is
  /// the gap between two renames.
  async fn swap_into_place(
    &self,
    archive_path: &Path,
    deploy_path: &Path,
    clean: bool,
  ) -> Result<()> {
    let archive_path = archive_path.to_path_buf();
    let deploy_path = deploy_path.to_path_buf();
    // Enough to tell concurrent deployments apart without unwieldy names.
    let suffix: String = self.deploy_id.chars().take(8).collect();

    spawn_blocking(move || swap_into_place_blocking(&archive_path, &deploy_path, clean, &suffix))
      .await
      .map_err(|e| {
        Box::new(AdeployError::Deploy(format!(
          "Archive extraction task failed: {}",
          e
        )))
      })?
  }

  fn resolve_backup_directory(
    &self,
    config: &PackageConfig,
    package_name: &str,
  ) -> Result<PathBuf> {
    match &config.backup_path {
      Some(path) => {
        info!("Using custom backup path {}", path);
        Ok(Path::new(path).to_path_buf())
      }
      None => Ok(executable_dir()?.join(package_name)),
    }
  }

  async fn copy_existing_deploy(&self, deploy_path: &Path, backup_full_path: &Path) -> Result<()> {
    if deploy_path.exists() {
      self.copy_directory(deploy_path, backup_full_path).await?;
      info!("Backup stored at {}", backup_full_path.display());
    } else {
      info!(
        "No existing deployment at {}; skipping backup",
        deploy_path.display()
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

/// Assemble the new tree, then move it over the old one.
fn swap_into_place_blocking(
  archive_path: &Path,
  deploy_path: &Path,
  clean: bool,
  suffix: &str,
) -> Result<()> {
  // Siblings rather than a shared staging directory: `rename` cannot cross
  // filesystems, and a sibling is on the same one by construction.
  let incoming = sibling_path(deploy_path, "incoming", suffix)?;
  let previous = sibling_path(deploy_path, "previous", suffix)?;

  if let Some(parent) = deploy_path.parent() {
    fs::create_dir_all(parent).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create {}: {}",
        parent.display(),
        e
      )))
    })?;
  }

  // Anything left by a crashed run would otherwise merge into this deployment.
  remove_directory(&incoming)?;
  fs::create_dir_all(&incoming).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to create {}: {}",
      incoming.display(),
      e
    )))
  })?;

  let assembled = (|| -> Result<()> {
    if !clean && deploy_path.exists() {
      // Merge semantics: start from what is there so files the package does
      // not ship survive, then let the archive overwrite what it does.
      copy_dir_recursive(deploy_path, &incoming).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to seed the new deployment from {}: {}",
          deploy_path.display(),
          e
        )))
      })?;
    }
    unpack_archive_into(archive_path, &incoming)
  })();

  if let Err(e) = assembled {
    // The live directory has not been touched yet, so there is nothing to undo.
    let _ = remove_directory(&incoming);
    return Err(e);
  }

  let had_existing = deploy_path.exists();
  if had_existing {
    fs::rename(deploy_path, &previous).map_err(|e| {
      let _ = remove_directory(&incoming);
      Box::new(AdeployError::FileSystem(format!(
        "Failed to move the existing deployment aside: {}",
        e
      )))
    })?;
  }

  if let Err(e) = fs::rename(&incoming, deploy_path) {
    // Put the old deployment back rather than leaving nothing in its place.
    if had_existing {
      if let Err(restore) = fs::rename(&previous, deploy_path) {
        error!(
          "Failed to restore the previous deployment from {}: {}",
          previous.display(),
          restore
        );
      }
    }
    let _ = remove_directory(&incoming);
    return Err(Box::new(AdeployError::FileSystem(format!(
      "Failed to move the new deployment into place: {}",
      e
    ))));
  }

  // The deployment is live at this point, so a cleanup failure is worth
  // reporting but not worth failing over.
  if had_existing {
    if let Err(e) = remove_directory(&previous) {
      warn!("Failed to remove {}: {}", previous.display(), e);
    }
  }

  Ok(())
}

/// Decompress a staged archive straight from disk into `target`.
fn unpack_archive_into(archive_path: &Path, target: &Path) -> Result<()> {
  let file = fs::File::open(archive_path).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to open staged archive {}: {}",
      archive_path.display(),
      e
    )))
  })?;

  let reader = io::BufReader::with_capacity(STREAM_BUFFER_SIZE, file);
  let decoder = flate2::read::GzDecoder::new(reader);
  let mut archive = tar::Archive::new(decoder);
  archive.unpack(target).map_err(|e| {
    Box::new(AdeployError::Deploy(format!(
      "Failed to extract archive: {}",
      e
    )))
  })?;
  Ok(())
}

/// A working directory next to `deploy_path`, on the same filesystem.
fn sibling_path(deploy_path: &Path, tag: &str, suffix: &str) -> Result<PathBuf> {
  let name = deploy_path.file_name().ok_or_else(|| {
    Box::new(AdeployError::FileSystem(format!(
      "Deploy path {} has no directory name to work beside",
      deploy_path.display()
    )))
  })?;

  Ok(deploy_path.with_file_name(format!("{}.{}-{}", name.to_string_lossy(), tag, suffix)))
}

/// Remove a directory if it is there, treating absence as success.
fn remove_directory(path: &Path) -> Result<()> {
  match fs::remove_dir_all(path) {
    Ok(()) => Ok(()),
    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(e) => Err(Box::new(AdeployError::FileSystem(format!(
      "Failed to remove {}: {}",
      path.display(),
      e
    )))),
  }
}

fn executable_dir() -> Result<PathBuf> {
  std::env::current_exe()
    .map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Failed to get current executable path: {}",
        e
      )))
    })?
    .parent()
    .map(Path::to_path_buf)
    .ok_or_else(|| {
      Box::new(AdeployError::Deploy(
        "Failed to get parent directory of executable".to_string(),
      ))
    })
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

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  /// A real tar.gz containing one file, built the way the client would.
  fn archive_with(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let source = dir.join("sources");
    fs::create_dir_all(&source).expect("source dir");
    fs::write(source.join(name), contents).expect("source file");

    let (bytes, _) =
      DeployManager::package_files_blocking("demo", &[source]).expect("build archive");
    let path = dir.join("archive.tar.gz");
    fs::write(&path, bytes).expect("write archive");
    path
  }

  /// Working directories must never outlive the deployment that made them.
  fn siblings_of(deploy_path: &Path) -> Vec<String> {
    let parent = deploy_path.parent().expect("parent");
    let prefix = format!("{}.", deploy_path.file_name().unwrap().to_string_lossy());
    fs::read_dir(parent)
      .expect("read parent")
      .filter_map(|entry| entry.ok())
      .map(|entry| entry.file_name().to_string_lossy().to_string())
      .filter(|name| name.starts_with(&prefix))
      .collect()
  }

  #[test]
  fn it_deploys_into_a_directory_that_does_not_exist_yet() {
    let temp = TempDir::new().expect("temp dir");
    let archive = archive_with(temp.path(), "app.txt", "v1");
    let deploy_path = temp.path().join("live");

    swap_into_place_blocking(&archive, &deploy_path, false, "abcd1234").expect("deploy");

    assert_eq!(
      fs::read_to_string(deploy_path.join("app.txt")).expect("read"),
      "v1"
    );
    assert!(siblings_of(&deploy_path).is_empty());
  }

  #[test]
  fn a_second_deployment_replaces_the_first() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");

    let first = archive_with(&temp.path().join("one"), "app.txt", "v1");
    swap_into_place_blocking(&first, &deploy_path, false, "1111").expect("first deploy");

    let second = archive_with(&temp.path().join("two"), "app.txt", "v2");
    swap_into_place_blocking(&second, &deploy_path, false, "2222").expect("second deploy");

    assert_eq!(
      fs::read_to_string(deploy_path.join("app.txt")).expect("read"),
      "v2"
    );
    assert!(siblings_of(&deploy_path).is_empty());
  }

  #[test]
  fn merging_keeps_files_the_package_does_not_ship() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");
    fs::create_dir_all(&deploy_path).expect("deploy dir");
    // Something the deployment did not put there: uploads, logs, a database.
    fs::write(deploy_path.join("runtime.db"), "keep me").expect("runtime file");

    let archive = archive_with(temp.path(), "app.txt", "v1");
    swap_into_place_blocking(&archive, &deploy_path, false, "abcd").expect("deploy");

    assert_eq!(
      fs::read_to_string(deploy_path.join("runtime.db")).expect("read"),
      "keep me",
      "the default must not wipe files the package does not ship"
    );
    assert!(deploy_path.join("app.txt").exists());
  }

  #[test]
  fn cleaning_removes_files_the_package_no_longer_ships() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");
    fs::create_dir_all(&deploy_path).expect("deploy dir");
    fs::write(deploy_path.join("stale.dll"), "old version").expect("stale file");

    let archive = archive_with(temp.path(), "app.txt", "v1");
    swap_into_place_blocking(&archive, &deploy_path, true, "abcd").expect("deploy");

    assert!(
      !deploy_path.join("stale.dll").exists(),
      "clean_deploy must leave only what the package ships"
    );
    assert!(deploy_path.join("app.txt").exists());
  }

  #[test]
  fn a_failed_extraction_leaves_the_live_deployment_untouched() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");

    let good = archive_with(&temp.path().join("one"), "app.txt", "v1");
    swap_into_place_blocking(&good, &deploy_path, false, "1111").expect("first deploy");

    // Not a gzip stream. Its hash is whatever it is, so this stands in for an
    // archive that passed verification and still cannot be read.
    let corrupt = temp.path().join("corrupt.tar.gz");
    fs::write(&corrupt, vec![0x42u8; 4096]).expect("write corrupt archive");

    let failure = swap_into_place_blocking(&corrupt, &deploy_path, false, "2222")
      .expect_err("a corrupt archive must fail");
    assert!(
      failure.to_string().contains("extract"),
      "the error should say extraction failed, got: {failure}"
    );

    assert_eq!(
      fs::read_to_string(deploy_path.join("app.txt")).expect("read"),
      "v1",
      "the previous deployment must survive a failed one intact"
    );
    assert!(
      siblings_of(&deploy_path).is_empty(),
      "a failed deployment must not leave working directories behind"
    );
  }

  #[test]
  fn leftovers_from_a_crashed_run_do_not_merge_into_the_next_one() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");

    // What a process killed mid-deployment would leave behind.
    let stranded = sibling_path(&deploy_path, "incoming", "abcd").expect("sibling");
    fs::create_dir_all(&stranded).expect("stranded dir");
    fs::write(stranded.join("garbage.txt"), "from a crash").expect("stranded file");

    let archive = archive_with(temp.path(), "app.txt", "v1");
    swap_into_place_blocking(&archive, &deploy_path, false, "abcd").expect("deploy");

    assert!(
      !deploy_path.join("garbage.txt").exists(),
      "a stale working directory must be cleared, not reused"
    );
    assert!(deploy_path.join("app.txt").exists());
  }
}
