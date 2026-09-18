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
    target: DeployTarget<'_>,
    sink: &LogSink,
  ) -> Result<()> {
    sink.info("Verifying archive hash").await;
    self
      .verify_archive_hash(archive_path, expected_hash)
      .await?;

    if config.backup_enabled {
      sink.info("Creating backup snapshot").await;
      self
        .create_backup(target.package_name, target.deploy_path, target.backup_dir)
        .await?;
    }

    sink
      .info(format!(
        "Extracting files into {}",
        target.deploy_path.display()
      ))
      .await;
    self
      .swap_into_place(archive_path, target.deploy_path)
      .await?;

    info!("Extraction complete: {}", target.deploy_path.display());
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
  pub async fn create_backup(
    &self,
    package_name: &str,
    deploy_path: &Path,
    backup_dir: &Path,
  ) -> Result<()> {
    let backup_dir_path = backup_dir.to_path_buf();
    std::fs::create_dir_all(&backup_dir_path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create backup directory: {}",
        e
      )))
    })?;

    info!("Creating backup at {}", backup_dir_path.display());

    info!("Backing up {} from {}", package_name, deploy_path.display());
    let backup_full_path = unique_backup_path(
      &backup_dir_path,
      &format!(
        "{}{}",
        BACKUP_PREFIX,
        self.start_time.format("%Y%m%d_%H%M%S")
      ),
    );

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

  /// Put a snapshot back, using the same swap a deployment does.
  pub async fn restore_backup(&self, backup_path: &Path, deploy_path: &Path) -> Result<()> {
    let backup_path = backup_path.to_path_buf();
    let deploy_path = deploy_path.to_path_buf();
    let suffix: String = self.deploy_id.chars().take(8).collect();

    spawn_blocking(move || restore_backup_blocking(&backup_path, &deploy_path, &suffix))
      .await
      .map_err(|e| Box::new(AdeployError::Deploy(format!("Restore task failed: {}", e))))?
  }

  /// Build the new deployment beside the old one, then swap it in.
  ///
  /// Unpacking straight over `deploy_path` meant a failure part way through
  /// left a directory that was neither the old deployment nor the new one, and
  /// a running service could read half-replaced files for as long as the
  /// extraction took. The new tree is assembled under a sibling name and moved
  /// into place with a rename, so the only moment anything is inconsistent is
  /// the gap between two renames.
  async fn swap_into_place(&self, archive_path: &Path, deploy_path: &Path) -> Result<()> {
    let archive_path = archive_path.to_path_buf();
    let deploy_path = deploy_path.to_path_buf();
    // Enough to tell concurrent deployments apart without unwieldy names.
    let suffix: String = self.deploy_id.chars().take(8).collect();

    spawn_blocking(move || deploy_archive_blocking(&archive_path, &deploy_path, &suffix))
      .await
      .map_err(|e| {
        Box::new(AdeployError::Deploy(format!(
          "Archive extraction task failed: {}",
          e
        )))
      })?
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

/// One file inside an archive, as it will land on the server.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
  pub path: String,
  pub size: u64,
}

/// Read back what an archive holds.
///
/// Listing the built archive rather than walking the sources again means a
/// preview cannot disagree with what would actually be sent, and proves the
/// archive is readable in the first place.
pub fn describe_archive(archive: &[u8]) -> Result<Vec<ArchiveEntry>> {
  let decoder = flate2::read::GzDecoder::new(archive);
  let mut tar = tar::Archive::new(decoder);

  let entries = tar.entries().map_err(|e| {
    Box::new(AdeployError::Deploy(format!(
      "Failed to read the archive: {}",
      e
    )))
  })?;

  let mut listed = Vec::new();
  for entry in entries {
    let entry = entry.map_err(|e| {
      Box::new(AdeployError::Deploy(format!(
        "Failed to read an archive entry: {}",
        e
      )))
    })?;

    // Directories carry no payload and only add noise to a preview.
    if entry.header().entry_type().is_dir() {
      continue;
    }

    let path = entry
      .path()
      .map(|path| path.to_string_lossy().to_string())
      .unwrap_or_else(|_| "<unreadable path>".to_string());

    listed.push(ArchiveEntry {
      path,
      size: entry.header().size().unwrap_or(0),
    });
  }

  Ok(listed)
}

/// Where one package lives on this server.
///
/// Bundled because these three always travel together, and passing them
/// separately had grown the extraction signature past the point of being
/// readable at a call site.
#[derive(Debug, Clone, Copy)]
pub struct DeployTarget<'a> {
  pub package_name: &'a str,
  pub deploy_path: &'a Path,
  pub backup_dir: &'a Path,
}

/// A snapshot of a deployment, sitting on disk.
#[derive(Debug, Clone)]
pub struct BackupInfo {
  pub name: String,
  pub path: PathBuf,
  pub created_ms: i64,
  pub size_bytes: u64,
}

/// Prefix every snapshot directory carries.
const BACKUP_PREFIX: &str = "backup_";

/// Where a package's snapshots live: beside the server binary, under its name.
///
/// Not somewhere the client asks for. Snapshots are the server's own record of
/// what it replaced, and a client that could place them could also point them
/// at a directory it wanted emptied by the next rollback.
pub fn backup_directory(package_name: &str) -> Result<PathBuf> {
  Ok(executable_dir()?.join(package_name))
}

/// Snapshots available for a package, newest first.
pub fn list_backups(backup_dir: &Path) -> Result<Vec<BackupInfo>> {
  if !backup_dir.exists() {
    return Ok(Vec::new());
  }

  let entries = fs::read_dir(backup_dir).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to read {}: {}",
      backup_dir.display(),
      e
    )))
  })?;

  let mut backups: Vec<BackupInfo> = entries
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.path().is_dir())
    .filter_map(|entry| {
      let name = entry.file_name().to_string_lossy().to_string();
      if !name.starts_with(BACKUP_PREFIX) {
        return None;
      }
      let path = entry.path();
      Some(BackupInfo {
        created_ms: backup_created_ms(&name, &path),
        size_bytes: directory_size(&path),
        name,
        path,
      })
    })
    .collect();

  // The name encodes the timestamp, so sorting by it is chronological; doing it
  // by name rather than by created_ms keeps ordering stable when a directory's
  // mtime has been disturbed.
  backups.sort_by(|a, b| b.name.cmp(&a.name));
  Ok(backups)
}

/// A snapshot directory that does not exist yet.
///
/// Names carry a timestamp to one second, so two snapshots taken inside the
/// same second would otherwise be the same directory - and `copy_dir_recursive`
/// merges into whatever is there rather than refusing. A rollback takes a
/// snapshot of the current state before restoring, so the collision landed
/// exactly where it does the most damage: the snapshot being restored from was
/// overwritten with the state being replaced, and the rollback became a no-op
/// that also destroyed the thing it was meant to recover.
fn unique_backup_path(backup_dir: &Path, base_name: &str) -> PathBuf {
  let first = backup_dir.join(base_name);
  if !first.exists() {
    return first;
  }

  // Suffixes sort after the bare name, so a later snapshot still reads as the
  // newer one.
  for attempt in 2..1_000 {
    let candidate = backup_dir.join(format!("{base_name}-{attempt}"));
    if !candidate.exists() {
      return candidate;
    }
  }

  backup_dir.join(format!("{base_name}-{}", Uuid::new_v4()))
}

/// When a snapshot was taken, from its name, falling back to its mtime.
fn backup_created_ms(name: &str, path: &Path) -> i64 {
  let stamp = name.trim_start_matches(BACKUP_PREFIX);
  let stamp = stamp.split_once('-').map_or(stamp, |(head, _)| head);
  if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d_%H%M%S") {
    return parsed.and_utc().timestamp_millis();
  }

  fs::metadata(path)
    .and_then(|metadata| metadata.modified())
    .ok()
    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    .map(|elapsed| elapsed.as_millis() as i64)
    .unwrap_or(0)
}

/// Total size of a directory tree, for display only.
fn directory_size(path: &Path) -> u64 {
  let Ok(entries) = fs::read_dir(path) else {
    return 0;
  };

  entries
    .filter_map(|entry| entry.ok())
    .map(|entry| match entry.file_type() {
      Ok(file_type) if file_type.is_dir() => directory_size(&entry.path()),
      Ok(_) => entry.metadata().map(|metadata| metadata.len()).unwrap_or(0),
      Err(_) => 0,
    })
    .sum()
}

/// Copy a snapshot back over the live deployment.
///
/// A backup is a complete picture of what the directory held, so restoring it
/// always replaces rather than merges: merging would leave whatever the failed
/// deployment had added.
fn restore_backup_blocking(backup_path: &Path, deploy_path: &Path, suffix: &str) -> Result<()> {
  swap_into_place_blocking(deploy_path, suffix, &|incoming| {
    copy_dir_recursive(backup_path, incoming).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to restore from {}: {}",
        backup_path.display(),
        e
      )))
    })?;
    Ok(())
  })
}

/// Unpack an archive into a new tree, then move it over the old one.
///
/// The new tree starts as a copy of what is already deployed, so files the
/// package does not ship survive and the archive overwrites only what it does.
/// That is what unpacking over the top always did; assembling it beside the
/// live directory is what makes a failure part way through harmless.
fn deploy_archive_blocking(archive_path: &Path, deploy_path: &Path, suffix: &str) -> Result<()> {
  swap_into_place_blocking(deploy_path, suffix, &|incoming| {
    if deploy_path.exists() {
      copy_dir_recursive(deploy_path, incoming).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to seed the new deployment from {}: {}",
          deploy_path.display(),
          e
        )))
      })?;
    }
    unpack_archive_into(archive_path, incoming)
  })
}

/// Assemble a new tree with `populate`, then move it over the old one.
fn swap_into_place_blocking(
  deploy_path: &Path,
  suffix: &str,
  populate: &dyn Fn(&Path) -> Result<()>,
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

  if let Err(e) = populate(&incoming) {
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

    deploy_archive_blocking(&archive, &deploy_path, "abcd1234").expect("deploy");

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
    deploy_archive_blocking(&first, &deploy_path, "1111").expect("first deploy");

    let second = archive_with(&temp.path().join("two"), "app.txt", "v2");
    deploy_archive_blocking(&second, &deploy_path, "2222").expect("second deploy");

    assert_eq!(
      fs::read_to_string(deploy_path.join("app.txt")).expect("read"),
      "v2"
    );
    assert!(siblings_of(&deploy_path).is_empty());
  }

  #[test]
  fn a_deployment_keeps_files_the_package_does_not_ship() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");
    fs::create_dir_all(&deploy_path).expect("deploy dir");
    // Something the deployment did not put there: uploads, logs, a database.
    fs::write(deploy_path.join("runtime.db"), "keep me").expect("runtime file");

    let archive = archive_with(temp.path(), "app.txt", "v1");
    deploy_archive_blocking(&archive, &deploy_path, "abcd").expect("deploy");

    assert_eq!(
      fs::read_to_string(deploy_path.join("runtime.db")).expect("read"),
      "keep me",
      "a deployment must not wipe files it does not ship: the directory may \
       hold uploads, logs or a database that no package is responsible for"
    );
    assert!(deploy_path.join("app.txt").exists());
  }

  #[test]
  fn a_failed_extraction_leaves_the_live_deployment_untouched() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");

    let good = archive_with(&temp.path().join("one"), "app.txt", "v1");
    deploy_archive_blocking(&good, &deploy_path, "1111").expect("first deploy");

    // Not a gzip stream. Its hash is whatever it is, so this stands in for an
    // archive that passed verification and still cannot be read.
    let corrupt = temp.path().join("corrupt.tar.gz");
    fs::write(&corrupt, vec![0x42u8; 4096]).expect("write corrupt archive");

    let failure = deploy_archive_blocking(&corrupt, &deploy_path, "2222")
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
    deploy_archive_blocking(&archive, &deploy_path, "abcd").expect("deploy");

    assert!(
      !deploy_path.join("garbage.txt").exists(),
      "a stale working directory must be cleared, not reused"
    );
    assert!(deploy_path.join("app.txt").exists());
  }

  #[test]
  fn snapshots_are_listed_newest_first() {
    let temp = TempDir::new().expect("temp dir");
    let backups = temp.path().join("backups");
    for name in [
      "backup_20260101_010101",
      "backup_20260914_172600",
      "not-a-backup",
    ] {
      fs::create_dir_all(backups.join(name)).expect("snapshot dir");
      fs::write(backups.join(name).join("f.txt"), "x").expect("file");
    }

    let listed = list_backups(&backups).expect("list");

    assert_eq!(listed.len(), 2, "only backup_* directories count");
    assert_eq!(listed[0].name, "backup_20260914_172600");
    assert_eq!(listed[1].name, "backup_20260101_010101");
    assert!(listed[0].size_bytes > 0);
  }

  #[test]
  fn listing_a_directory_that_does_not_exist_is_not_an_error() {
    let temp = TempDir::new().expect("temp dir");
    let listed = list_backups(&temp.path().join("never-created")).expect("list");
    assert!(listed.is_empty());
  }

  #[test]
  fn a_snapshot_name_carries_the_time_it_was_taken() {
    let temp = TempDir::new().expect("temp dir");
    let backups = temp.path().join("backups");
    fs::create_dir_all(backups.join("backup_20260914_172600")).expect("snapshot dir");

    let listed = list_backups(&backups).expect("list");
    let taken = chrono::DateTime::from_timestamp_millis(listed[0].created_ms).expect("timestamp");

    assert_eq!(
      taken.format("%Y-%m-%d %H:%M:%S").to_string(),
      "2026-09-14 17:26:00"
    );
  }

  #[test]
  fn restoring_replaces_whatever_is_there_now() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");

    // A snapshot of an older, working deployment.
    let snapshot = temp.path().join("backup_20260101_010101");
    fs::create_dir_all(&snapshot).expect("snapshot");
    fs::write(snapshot.join("app.txt"), "v1").expect("snapshot file");

    // What is live now: a newer version plus a file the old one never had.
    fs::create_dir_all(&deploy_path).expect("deploy dir");
    fs::write(deploy_path.join("app.txt"), "v2-broken").expect("live file");
    fs::write(deploy_path.join("added-by-v2.txt"), "x").expect("extra file");

    restore_backup_blocking(&snapshot, &deploy_path, "abcd").expect("restore");

    assert_eq!(
      fs::read_to_string(deploy_path.join("app.txt")).expect("read"),
      "v1"
    );
    assert!(
      !deploy_path.join("added-by-v2.txt").exists(),
      "a snapshot is a complete picture, so restoring it must not leave the        failed deployment's additions behind"
    );
    assert!(siblings_of(&deploy_path).is_empty());
  }

  #[test]
  fn a_failed_restore_leaves_the_live_deployment_untouched() {
    let temp = TempDir::new().expect("temp dir");
    let deploy_path = temp.path().join("live");
    fs::create_dir_all(&deploy_path).expect("deploy dir");
    fs::write(deploy_path.join("app.txt"), "v2").expect("live file");

    let missing = temp.path().join("backup_that_vanished");
    restore_backup_blocking(&missing, &deploy_path, "abcd")
      .expect_err("restoring a snapshot that is gone must fail");

    assert_eq!(
      fs::read_to_string(deploy_path.join("app.txt")).expect("read"),
      "v2"
    );
    assert!(siblings_of(&deploy_path).is_empty());
  }

  #[test]
  fn snapshots_live_beside_the_server_binary_under_the_package_name() {
    // Not anywhere the client named: a client that could place them could also
    // point them at a directory it wanted emptied by the next rollback.
    let resolved = backup_directory("demo").expect("resolve");
    let expected = std::env::current_exe()
      .expect("exe")
      .parent()
      .expect("exe dir")
      .join("demo");

    assert_eq!(resolved, expected);
  }

  #[test]
  fn an_archive_lists_the_files_it_holds() {
    let temp = TempDir::new().expect("temp dir");
    let source = temp.path().join("sources");
    fs::create_dir_all(source.join("nested")).expect("dirs");
    fs::write(source.join("app.txt"), "1234567890").expect("file");
    fs::write(source.join("nested/lib.txt"), "abc").expect("nested file");

    let (archive, _) =
      DeployManager::package_files_blocking("demo", &[source]).expect("build archive");
    let mut entries = describe_archive(&archive).expect("describe");
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    // Directories carry no payload, so a preview lists only real files.
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "app.txt");
    assert_eq!(entries[0].size, 10);
    assert_eq!(entries[1].path, "nested/lib.txt");
    assert_eq!(entries[1].size, 3);
  }

  #[test]
  fn describing_something_that_is_not_an_archive_is_an_error() {
    // A preview must not claim an archive is fine when it cannot be read.
    let failure = describe_archive(&[0x42u8; 512]).expect_err("not a gzip stream");
    assert!(failure.to_string().contains("archive"));
  }

  #[test]
  fn two_snapshots_in_the_same_second_do_not_become_one() {
    let temp = TempDir::new().expect("temp dir");
    let backups = temp.path().join("backups");
    fs::create_dir_all(&backups).expect("backup dir");

    let first = unique_backup_path(&backups, "backup_20260915_073524");
    fs::create_dir_all(&first).expect("first snapshot");
    let second = unique_backup_path(&backups, "backup_20260915_073524");

    assert_ne!(
      first, second,
      "a rollback snapshots the current state before restoring, so a collision \
       here overwrites the snapshot being restored from"
    );
    assert_eq!(
      second.file_name().unwrap().to_string_lossy(),
      "backup_20260915_073524-2"
    );
  }

  #[test]
  fn a_disambiguated_snapshot_still_reads_as_the_newer_one() {
    let temp = TempDir::new().expect("temp dir");
    let backups = temp.path().join("backups");
    for name in ["backup_20260915_073524", "backup_20260915_073524-2"] {
      fs::create_dir_all(backups.join(name)).expect("snapshot");
    }

    let listed = list_backups(&backups).expect("list");

    assert_eq!(listed[0].name, "backup_20260915_073524-2");
    // The suffix is not part of the timestamp it carries.
    assert_eq!(listed[0].created_ms, listed[1].created_ms);
  }
}
