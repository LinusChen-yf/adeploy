use std::{
  env,
  ffi::OsString,
  future::Future,
  path::{Path, PathBuf},
  pin::Pin,
  sync::Arc,
  time::Duration,
};

use async_stream::try_stream;
use base64::{engine::general_purpose, Engine as _};
use log2::*;
use service_manager::{
  ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx, ServiceStatus,
  ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};
use tokio::{
  io::AsyncWriteExt,
  sync::{mpsc, watch, RwLock},
};
use tokio_stream::Stream;
use tonic::{transport::Server, Request, Response, Status, Streaming};

use crate::{
  adeploy::{
    deploy_chunk::Payload,
    deploy_event::Event,
    deploy_service_server::{DeployService, DeployServiceServer},
    BackupEntry, BackupListRequest, BackupListResponse, DeployAccepted, DeployChunk, DeployEvent,
    DeployLog, DeployResult, DeployStart, PairRequest, PairResponse, PairState, RollbackRequest,
  },
  auth::{
    backup_list_signing_payload, deploy_start_signing_payload, fingerprint, pair_signing_payload,
    rollback_signing_payload, Auth,
  },
  config::{ConfigProvider, PackageConfig, ProjectConfig},
  deploy::{backup_directory, list_backups, DeployManager, DeployTarget},
  deploy_log::{DeployLogEntry, LogLevel, LogSink},
  error::{AdeployError, Result},
  init,
  pairing::{PairOutcome, PairStore, PAIRED_FILE_NAME},
  replay::ReplayGuard,
};

/// Ceiling on one inbound message.
///
/// Chunked uploads mean the largest message is a single chunk, not the whole
/// archive, so this bounds what any one message can allocate without bounding
/// how large a package may be. Comfortably above the client's chunk size, and
/// far below what a whole archive used to be allowed to reach.
const MAX_INBOUND_MESSAGE_SIZE: usize = 8 * 1024 * 1024;

/// Directory under the deploy root where uploads land before they are trusted.
const STAGING_DIR: &str = ".staging";

/// Buffer for the server-to-client event stream.
const EVENT_CHANNEL_SIZE: usize = 256;

/// ADeploy gRPC service implementation
#[derive(Clone)]
pub struct AdeployService {
  config: Arc<RwLock<ProjectConfig>>,
  replay: Arc<ReplayGuard>,
  /// Where approvals live. Read on each request rather than cached, because
  /// `adeploy server approve` is a separate process editing the same file.
  paired_path: PathBuf,
  /// Serialises this server's own read-modify-write of that file.
  pair_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AdeployService {
  pub fn new(config: Arc<RwLock<ProjectConfig>>, paired_path: PathBuf) -> Self {
    Self {
      config,
      replay: Arc::new(ReplayGuard::new()),
      paired_path,
      pair_lock: Arc::new(tokio::sync::Mutex::new(())),
    }
  }

  /// Keys trusted right now: the configured allowlist plus approved pairings.
  fn is_trusted(&self, allowed_keys: &[String], public_key: &str) -> bool {
    let key = public_key.trim();
    if allowed_keys.iter().any(|allowed| allowed.trim() == key) {
      return true;
    }

    match PairStore::load(&self.paired_path) {
      Ok(store) => store.is_approved(key),
      Err(e) => {
        error!("Failed to read {}: {}", self.paired_path.display(), e);
        false
      }
    }
  }
}

/// An opening message that passed every check, with the policy it resolved to.
struct AcceptedDeploy {
  start: DeployStart,
  package_config: PackageConfig,
  deploy_path: PathBuf,
  backup_dir: PathBuf,
  staging_dir: PathBuf,
}

/// Everything a package's name resolves to on this server.
struct PackageContext {
  config: PackageConfig,
  deploy_path: PathBuf,
  backup_dir: PathBuf,
  deploy_root: PathBuf,
}

/// An upload written to disk, removed when it goes out of scope.
struct StagedArchive {
  path: PathBuf,
}

impl Drop for StagedArchive {
  fn drop(&mut self) {
    if let Err(e) = std::fs::remove_file(&self.path) {
      if e.kind() != std::io::ErrorKind::NotFound {
        warn!(
          "Failed to remove staged archive {}: {}",
          self.path.display(),
          e
        );
      }
    }
  }
}

/// One turn of the event loop that interleaves progress with the deployment.
enum DeployStep {
  Log(DeployLogEntry),
  Finished(Result<()>),
}

#[tonic::async_trait]
impl DeployService for AdeployService {
  type DeployStream =
    Pin<Box<dyn Stream<Item = std::result::Result<DeployEvent, Status>> + Send + 'static>>;

  async fn deploy(
    &self,
    request: Request<Streaming<DeployChunk>>,
  ) -> std::result::Result<Response<Self::DeployStream>, Status> {
    let mut inbound = request.into_inner();

    // The opening message is checked before any response stream exists, so a
    // caller that fails authorisation gets a plain gRPC status and never sends
    // a byte of archive. Only this small message is decoded beforehand.
    let start = read_start_message(&mut inbound).await?;
    let accepted = self.authorize(start).await?;

    let deploy_manager = DeployManager::new();
    info!(
      "Accepted deployment {} for {} ({} bytes)",
      deploy_manager.deploy_id, accepted.start.package_name, accepted.start.total_size
    );

    let stream = deployment_stream(deploy_manager, accepted, inbound);
    Ok(Response::new(Box::pin(stream) as Self::DeployStream))
  }

  async fn list_backups(
    &self,
    request: Request<BackupListRequest>,
  ) -> std::result::Result<Response<BackupListResponse>, Status> {
    let request = request.into_inner();

    let payload = backup_list_signing_payload(
      &request.package_name,
      &request.public_key,
      &request.nonce,
      request.timestamp_ms,
    );
    self
      .authenticate(
        &request.public_key,
        &request.signature,
        &payload,
        &request.nonce,
        request.timestamp_ms,
      )
      .await?;

    let context = self.package_context(&request.package_name).await?;
    let backups = list_backups(&context.backup_dir)
      .map_err(|e| Status::internal(format!("Failed to read snapshots: {}", e)))?;

    info!(
      "Listed {} snapshot(s) for {}",
      backups.len(),
      request.package_name
    );

    Ok(Response::new(BackupListResponse {
      backups: backups
        .into_iter()
        .map(|backup| BackupEntry {
          name: backup.name,
          created_ms: backup.created_ms,
          size_bytes: backup.size_bytes,
        })
        .collect(),
    }))
  }

  type RollbackStream =
    Pin<Box<dyn Stream<Item = std::result::Result<DeployEvent, Status>> + Send + 'static>>;

  async fn rollback(
    &self,
    request: Request<RollbackRequest>,
  ) -> std::result::Result<Response<Self::RollbackStream>, Status> {
    let request = request.into_inner();

    let payload = rollback_signing_payload(
      &request.package_name,
      &request.backup_name,
      &request.public_key,
      &request.nonce,
      request.timestamp_ms,
    );
    self
      .authenticate(
        &request.public_key,
        &request.signature,
        &payload,
        &request.nonce,
        request.timestamp_ms,
      )
      .await?;

    let context = self.package_context(&request.package_name).await?;
    let backups = list_backups(&context.backup_dir)
      .map_err(|e| Status::internal(format!("Failed to read snapshots: {}", e)))?;

    // Choosing the snapshot before the response stream exists means "there is
    // nothing to roll back to" is a plain status rather than a stream that
    // opens and immediately fails.
    let chosen = if request.backup_name.is_empty() {
      backups.first().cloned().ok_or_else(|| {
        Status::not_found(format!(
          "No snapshots exist for '{}'; a deployment with backup_enabled creates them",
          request.package_name
        ))
      })?
    } else {
      backups
        .into_iter()
        .find(|backup| backup.name == request.backup_name)
        .ok_or_else(|| {
          Status::not_found(format!(
            "No snapshot named '{}' for '{}'",
            request.backup_name, request.package_name
          ))
        })?
    };

    let deploy_manager = DeployManager::new();
    info!(
      "Rolling {} back to {} ({})",
      request.package_name, chosen.name, deploy_manager.deploy_id
    );

    let stream = rollback_stream(deploy_manager, request.package_name, context, chosen);
    Ok(Response::new(Box::pin(stream) as Self::RollbackStream))
  }

  async fn pair(
    &self,
    request: Request<PairRequest>,
  ) -> std::result::Result<Response<PairResponse>, Status> {
    let client_address = request.remote_addr().map(|address| address.to_string());
    let request = request.into_inner();

    // Nothing here is trusted yet, so the request must at least prove it holds
    // the key it is presenting before it earns a slot in the operator's queue.
    let signature = general_purpose::STANDARD
      .decode(&request.signature)
      .map_err(|e| Status::invalid_argument(format!("Invalid signature: {}", e)))?;

    let payload = pair_signing_payload(
      &request.public_key,
      &request.client_name,
      &request.nonce,
      request.timestamp_ms,
    );

    match Auth::verify_signature(&request.public_key, &payload, &signature) {
      Ok(true) => {}
      Ok(false) => {
        error!("Pairing request failed its own signature check");
        return Err(Status::unauthenticated(
          "Pairing request is not signed by the key it presents",
        ));
      }
      Err(e) => {
        error!("Pairing signature verification error: {}", e);
        return Err(Status::unauthenticated(format!("Auth error: {}", e)));
      }
    }

    if let Err(rejection) = self.replay.admit(&request.nonce, request.timestamp_ms) {
      return Err(Status::unauthenticated(rejection.message()));
    }

    let allowed_keys = {
      let config = self.config.read().await;
      config.server.allowed_keys.clone()
    };
    let key_fingerprint = fingerprint(&request.public_key);

    if allowed_keys
      .iter()
      .any(|allowed| allowed.trim() == request.public_key.trim())
    {
      return Ok(Response::new(PairResponse {
        state: PairState::Approved as i32,
        fingerprint: key_fingerprint,
        message: "Already listed in the server's allowed_keys".to_string(),
      }));
    }

    let _guard = self.pair_lock.lock().await;
    let mut store = PairStore::load(&self.paired_path)
      .map_err(|e| Status::internal(format!("Failed to read the pairing store: {}", e)))?;

    let outcome = store.request(
      &request.public_key,
      &request.client_name,
      client_address.clone(),
    );

    if matches!(outcome, PairOutcome::Queued | PairOutcome::AlreadyPending) {
      store
        .save(&self.paired_path)
        .map_err(|e| Status::internal(format!("Failed to record the request: {}", e)))?;
    }

    let response = match outcome {
      PairOutcome::Queued => {
        info!(
          "Pairing requested by {} from {} ({})",
          request.client_name,
          client_address.as_deref().unwrap_or("an unknown address"),
          key_fingerprint
        );
        warn!("Run `adeploy server pending` to review it");
        PairResponse {
          state: PairState::Pending as i32,
          fingerprint: key_fingerprint,
          message: "Queued for approval".to_string(),
        }
      }
      PairOutcome::AlreadyPending => PairResponse {
        state: PairState::Pending as i32,
        fingerprint: key_fingerprint,
        message: "Already waiting for approval".to_string(),
      },
      PairOutcome::AlreadyApproved => PairResponse {
        state: PairState::Approved as i32,
        fingerprint: key_fingerprint,
        message: "Already approved".to_string(),
      },
      PairOutcome::Rejected => PairResponse {
        state: PairState::Rejected as i32,
        fingerprint: key_fingerprint,
        message: "This key was refused by an operator".to_string(),
      },
      PairOutcome::QueueFull => {
        error!("Pairing queue is full; refusing {}", key_fingerprint);
        return Err(Status::resource_exhausted(
          "The server's pairing queue is full; ask an operator to review it",
        ));
      }
    };

    Ok(Response::new(response))
  }
}

impl AdeployService {
  /// The checks every authenticated call shares: allowlist, signature, replay.
  ///
  /// Each caller supplies the bytes its own message signs, under its own domain
  /// tag, so a signature authorising one operation is never valid for another.
  async fn authenticate(
    &self,
    public_key: &str,
    signature_b64: &str,
    payload: &[u8],
    nonce: &str,
    timestamp_ms: i64,
  ) -> std::result::Result<(), Status> {
    let allowed_keys = {
      let config = self.config.read().await;
      config.server.allowed_keys.clone()
    };

    if !self.is_trusted(&allowed_keys, public_key.trim()) {
      error!("Public key not allowed ({})", fingerprint(public_key));
      return Err(Status::unauthenticated("Client public key not allowed"));
    }

    let signature = general_purpose::STANDARD
      .decode(signature_b64)
      .map_err(|e| {
        error!("Invalid signature format: {}", e);
        Status::invalid_argument(format!("Invalid signature: {}", e))
      })?;

    match Auth::verify_signature(public_key, payload, &signature) {
      Ok(true) => {}
      Ok(false) => {
        error!("Signature verification failed");
        return Err(Status::unauthenticated("Invalid Ed25519 signature"));
      }
      Err(e) => {
        error!("Ed25519 signature verification error: {}", e);
        return Err(Status::unauthenticated(format!("Auth error: {}", e)));
      }
    }

    // Only after the signature holds: an unsigned nonce could be invented by
    // anyone, and recording it would burn a value a real client might use.
    self
      .replay
      .admit(nonce, timestamp_ms)
      .map_err(|rejection| Status::unauthenticated(rejection.message()))
  }

  /// Resolve a package name against this server's configuration.
  async fn package_context(
    &self,
    package_name: &str,
  ) -> std::result::Result<PackageContext, Status> {
    let fallback_root = init::default_deploy_root().map_err(|e| {
      error!("Cannot determine the deploy root: {}", e);
      Status::internal(format!("Cannot determine deploy root: {}", e))
    })?;

    let config = self.config.read().await;
    let deploy_root = resolve_deploy_root(&config).unwrap_or_else(|_| fallback_root.clone());

    let package_config = config.packages.get(package_name).cloned();
    let deploy_path = config.resolve_deploy_path(package_name, &fallback_root);

    match (package_config, deploy_path) {
      (Some(package_config), Some(deploy_path)) => Ok(PackageContext {
        backup_dir: backup_directory(&package_config, package_name, &deploy_root),
        config: package_config,
        deploy_path,
        deploy_root,
      }),
      _ => {
        error!("Package {} is not configured", package_name);
        Err(Status::not_found(format!(
          "Package '{}' not configured",
          package_name
        )))
      }
    }
  }

  /// Check who is calling and what they are asking for, before accepting bytes.
  async fn authorize(&self, start: DeployStart) -> std::result::Result<AcceptedDeploy, Status> {
    info!("Deploy request for {} from a client", start.package_name);

    let payload = deploy_start_signing_payload(
      &start.package_name,
      start.total_size,
      &start.file_hash,
      &start.public_key,
      &start.nonce,
      start.timestamp_ms,
    );
    self
      .authenticate(
        &start.public_key,
        &start.signature,
        &payload,
        &start.nonce,
        start.timestamp_ms,
      )
      .await?;

    let context = self.package_context(&start.package_name).await?;

    Ok(AcceptedDeploy {
      start,
      package_config: context.config,
      deploy_path: context.deploy_path,
      backup_dir: context.backup_dir,
      staging_dir: context.deploy_root.join(STAGING_DIR),
    })
  }
}

/// The first message of the stream, which must describe the upload.
async fn read_start_message(
  inbound: &mut Streaming<DeployChunk>,
) -> std::result::Result<DeployStart, Status> {
  match inbound.message().await? {
    Some(DeployChunk {
      payload: Some(Payload::Start(start)),
    }) => Ok(start),
    Some(_) => Err(Status::invalid_argument(
      "The first message must be a DeployStart",
    )),
    None => Err(Status::invalid_argument(
      "Stream closed before the opening message",
    )),
  }
}

/// Drive the upload and the deployment, reporting progress as it happens.
///
/// The work runs inside the response stream rather than in a spawned task, so
/// a client that disconnects or whose deadline expires takes the deployment
/// down with it. A detached task would keep unpacking with nobody listening.
fn deployment_stream(
  deploy_manager: DeployManager,
  accepted: AcceptedDeploy,
  mut inbound: Streaming<DeployChunk>,
) -> impl Stream<Item = std::result::Result<DeployEvent, Status>> + Send + 'static {
  try_stream! {
    let deploy_id = deploy_manager.deploy_id.clone();
    yield accepted_event(&deploy_id);

    let staged = receive_archive(&mut inbound, &accepted, &deploy_id).await?;

    let (sender, mut receiver) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let sink = LogSink::new(sender);
    let work = execute_deployment(&deploy_manager, &accepted, &staged.path, &sink);
    tokio::pin!(work);

    loop {
      let step = tokio::select! {
        Some(entry) = receiver.recv() => DeployStep::Log(entry),
        outcome = &mut work => DeployStep::Finished(outcome),
      };

      match step {
        DeployStep::Log(entry) => yield log_event(entry),
        DeployStep::Finished(outcome) => {
          // Whatever the work produced before returning still belongs to the
          // client, including the entries explaining a failure.
          while let Ok(entry) = receiver.try_recv() {
            yield log_event(entry);
          }

          match outcome {
            Ok(()) => {
              info!("Deployment {} completed for {}", deploy_id, accepted.start.package_name);
              yield result_event(true, "Deployment completed successfully", &deploy_id);
            }
            Err(e) => {
              error!("Deployment {} failed for {}: {}", deploy_id, accepted.start.package_name, e);
              yield log_event(DeployLogEntry::error(format!("Deployment failed: {}", e)));
              yield result_event(false, &e.to_string(), &deploy_id);
            }
          }
          break;
        }
      }
    }
  }
}

/// Put a snapshot back, reporting progress the way a deployment does.
fn rollback_stream(
  deploy_manager: DeployManager,
  package_name: String,
  context: PackageContext,
  chosen: crate::deploy::BackupInfo,
) -> impl Stream<Item = std::result::Result<DeployEvent, Status>> + Send + 'static {
  try_stream! {
    let deploy_id = deploy_manager.deploy_id.clone();
    yield accepted_event(&deploy_id);

    let (sender, mut receiver) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let sink = LogSink::new(sender);
    let work = execute_rollback(&deploy_manager, &package_name, &context, &chosen, &sink);
    tokio::pin!(work);

    loop {
      let step = tokio::select! {
        Some(entry) = receiver.recv() => DeployStep::Log(entry),
        outcome = &mut work => DeployStep::Finished(outcome),
      };

      match step {
        DeployStep::Log(entry) => yield log_event(entry),
        DeployStep::Finished(outcome) => {
          while let Ok(entry) = receiver.try_recv() {
            yield log_event(entry);
          }

          match outcome {
            Ok(()) => {
              info!("Rollback {} completed for {}", deploy_id, package_name);
              yield result_event(
                true,
                &format!("Rolled back to {}", chosen.name),
                &deploy_id,
              );
            }
            Err(e) => {
              error!("Rollback {} failed for {}: {}", deploy_id, package_name, e);
              yield log_event(DeployLogEntry::error(format!("Rollback failed: {}", e)));
              yield result_event(false, &e.to_string(), &deploy_id);
            }
          }
          break;
        }
      }
    }
  }
}

/// Run the hooks around restoring a snapshot.
///
/// The same hooks a deployment runs, because putting files back has the same
/// requirements: the service holding them has to stop first and start after.
async fn execute_rollback(
  deploy_manager: &DeployManager,
  package_name: &str,
  context: &PackageContext,
  chosen: &crate::deploy::BackupInfo,
  sink: &LogSink,
) -> Result<()> {
  sink
    .info(format!(
      "[{}] Rolling back to {}",
      deploy_manager.deploy_id, chosen.name
    ))
    .await;

  deploy_manager
    .execute_before_deploy_script(&context.config, sink)
    .await?;

  // Snapshot what is there now, so a rollback can itself be undone.
  if context.config.backup_enabled {
    sink.info("Creating backup snapshot").await;
    deploy_manager
      .create_backup(package_name, &context.deploy_path, &context.backup_dir)
      .await?;
  }

  sink
    .info(format!(
      "Restoring {} into {}",
      chosen.name,
      context.deploy_path.display()
    ))
    .await;
  deploy_manager
    .restore_backup(&chosen.path, &context.deploy_path)
    .await?;
  sink.info("Restore complete").await;

  if let Err(e) = deploy_manager
    .execute_after_deploy_script(&context.config, sink)
    .await
  {
    warn!("After-deploy script failed: {}", e);
    sink
      .warn(format!("After-deploy script failed: {}", e))
      .await;
  }

  sink
    .info(format!(
      "[{}] Rollback completed successfully",
      deploy_manager.deploy_id
    ))
    .await;
  Ok(())
}

/// Write the incoming chunks to disk, holding the declared size to account.
async fn receive_archive(
  inbound: &mut Streaming<DeployChunk>,
  accepted: &AcceptedDeploy,
  deploy_id: &str,
) -> std::result::Result<StagedArchive, Status> {
  tokio::fs::create_dir_all(&accepted.staging_dir)
    .await
    .map_err(|e| {
      Status::internal(format!(
        "Failed to create staging directory {}: {}",
        accepted.staging_dir.display(),
        e
      ))
    })?;

  let path = accepted.staging_dir.join(format!("{}.tar.gz", deploy_id));
  let staged = StagedArchive { path };

  let mut file = tokio::fs::File::create(&staged.path).await.map_err(|e| {
    Status::internal(format!(
      "Failed to create staged archive {}: {}",
      staged.path.display(),
      e
    ))
  })?;

  let mut received: u64 = 0;
  while let Some(chunk) = inbound.message().await? {
    match chunk.payload {
      Some(Payload::Data(bytes)) => {
        received = received.saturating_add(bytes.len() as u64);
        // The declared size is a claim, not a fact, so the stream is measured
        // as it arrives rather than trusted to stop where it said it would.
        if received > accepted.start.total_size {
          return Err(Status::invalid_argument(format!(
            "Stream exceeded the declared size of {} bytes",
            accepted.start.total_size
          )));
        }
        file
          .write_all(&bytes)
          .await
          .map_err(|e| Status::internal(format!("Failed to write staged archive: {}", e)))?;
      }
      Some(Payload::Start(_)) => {
        return Err(Status::invalid_argument(
          "Only the first message may be a DeployStart",
        ));
      }
      None => {}
    }
  }

  file
    .flush()
    .await
    .map_err(|e| Status::internal(format!("Failed to flush staged archive: {}", e)))?;
  drop(file);

  if received != accepted.start.total_size {
    return Err(Status::invalid_argument(format!(
      "Stream ended after {} of the {} declared bytes",
      received, accepted.start.total_size
    )));
  }

  info!("Staged {} bytes at {}", received, staged.path.display());
  Ok(staged)
}

/// Run the hooks and the extraction, reporting each stage through `sink`.
async fn execute_deployment(
  deploy_manager: &DeployManager,
  accepted: &AcceptedDeploy,
  archive_path: &Path,
  sink: &LogSink,
) -> Result<()> {
  sink
    .info(format!(
      "[{}] Starting deployment execution",
      deploy_manager.deploy_id
    ))
    .await;

  deploy_manager
    .execute_before_deploy_script(&accepted.package_config, sink)
    .await?;

  deploy_manager
    .extract_files(
      archive_path,
      &accepted.start.file_hash,
      &accepted.package_config,
      DeployTarget {
        package_name: &accepted.start.package_name,
        deploy_path: &accepted.deploy_path,
        backup_dir: &accepted.backup_dir,
      },
      sink,
    )
    .await?;

  // A failed after-deploy hook has never failed the deployment: the files are
  // already in place, and reverting them is not this stage's job.
  if let Err(e) = deploy_manager
    .execute_after_deploy_script(&accepted.package_config, sink)
    .await
  {
    warn!("After-deploy script failed: {}", e);
    sink
      .warn(format!("After-deploy script failed: {}", e))
      .await;
  }

  sink
    .info(format!(
      "[{}] Deployment completed successfully",
      deploy_manager.deploy_id
    ))
    .await;
  Ok(())
}

fn accepted_event(deploy_id: &str) -> DeployEvent {
  DeployEvent {
    event: Some(Event::Accepted(DeployAccepted {
      deploy_id: deploy_id.to_string(),
    })),
  }
}

fn log_event(entry: DeployLogEntry) -> DeployEvent {
  DeployEvent {
    event: Some(Event::Log(DeployLog {
      level: map_log_level(entry.level) as i32,
      message: entry.message,
    })),
  }
}

fn result_event(success: bool, message: &str, deploy_id: &str) -> DeployEvent {
  DeployEvent {
    event: Some(Event::Result(DeployResult {
      success,
      message: message.to_string(),
      deploy_id: deploy_id.to_string(),
    })),
  }
}

fn map_log_level(level: LogLevel) -> crate::adeploy::deploy_log::Level {
  match level {
    LogLevel::Info => crate::adeploy::deploy_log::Level::Info,
    LogLevel::Warn => crate::adeploy::deploy_log::Level::Warn,
    LogLevel::Error => crate::adeploy::deploy_log::Level::Error,
  }
}

pub async fn start_server(provider: Arc<dyn ConfigProvider>) -> Result<()> {
  start_server_with_shutdown(provider, std::future::pending()).await
}

pub async fn start_server_with_shutdown<F>(
  provider: Arc<dyn ConfigProvider>,
  shutdown: F,
) -> Result<()>
where
  F: Future<Output = ()> + Send + 'static,
{
  let config_path = provider.get_config_path()?;
  let generated = init::ensure_server_config(&config_path)?;
  let config = provider.load_project_config(config_path.as_path())?;

  let port = config.server.listen_port;
  let deploy_root = resolve_deploy_root(&config)?;
  prepare_deploy_root(&deploy_root)?;
  log_startup_state(&config_path, &config, &deploy_root, generated);

  let addr = format!("0.0.0.0:{}", port)
    .parse()
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid address: {}", e))))?;

  // Beside the configuration, which is beside the binary in a real install and
  // inside the temporary directory under test.
  let paired_path = config_path
    .parent()
    .map(|parent| parent.join(PAIRED_FILE_NAME))
    .unwrap_or_else(|| PathBuf::from(PAIRED_FILE_NAME));

  let shared_config = Arc::new(RwLock::new(config));
  let (shutdown_tx, shutdown_rx) = watch::channel(false);
  let _watcher_guard = WatcherGuard {
    sender: shutdown_tx,
  };
  spawn_config_watcher(
    provider.clone(),
    config_path,
    shared_config.clone(),
    shutdown_rx,
  );

  let adeploy_service = AdeployService::new(shared_config, paired_path);

  info!("Binding ADeploy server on {}", addr);

  Server::builder()
    .add_service(
      DeployServiceServer::new(adeploy_service)
        .max_decoding_message_size(MAX_INBOUND_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_INBOUND_MESSAGE_SIZE),
    ) // 100 MB
    .serve_with_shutdown(addr, shutdown)
    .await
    .map_err(|e| Box::new(AdeployError::Network(format!("Server error: {}", e))))?;

  Ok(())
}

/// Directory that relative `deploy_path` values land under.
fn resolve_deploy_root(config: &ProjectConfig) -> Result<PathBuf> {
  match &config.server.deploy_root {
    Some(root) => Ok(PathBuf::from(root)),
    None => init::default_deploy_root(),
  }
}

/// Create the deploy root up front so the first deployment does not fail on a
/// missing directory, and so an operator can see where files will land.
fn prepare_deploy_root(deploy_root: &Path) -> Result<()> {
  std::fs::create_dir_all(deploy_root).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to create deploy root {}: {}",
      deploy_root.display(),
      e
    )))
  })?;
  Ok(())
}

/// Report what the server is about to do, and what it still needs.
///
/// Everything here was previously only discoverable by reading the config file,
/// which a freshly installed server does not have until this run creates it.
fn log_startup_state(
  config_path: &Path,
  config: &ProjectConfig,
  deploy_root: &Path,
  generated: bool,
) {
  if generated {
    info!("First run: generated {}", config_path.display());
  }
  info!("Configuration: {}", config_path.display());
  info!("Deploy root: {}", deploy_root.display());

  if config.server.allowed_keys.is_empty() {
    warn!(
      "No client keys authorized yet. A deploying client will be rejected and will print its public key; add it to `allowed_keys` in {} (reloaded automatically).",
      config_path.display()
    );
  } else {
    info!(
      "{} client key(s) authorized",
      config.server.allowed_keys.len()
    );
  }

  if config.packages.is_empty() {
    warn!(
      "No packages configured. Add a [packages.<name>] table to {}",
      config_path.display()
    );
  }
}

fn spawn_config_watcher(
  provider: Arc<dyn ConfigProvider>,
  config_path: PathBuf,
  shared_config: Arc<RwLock<ProjectConfig>>,
  mut shutdown_rx: watch::Receiver<bool>,
) {
  tokio::spawn(async move {
    let mut last_modified = std::fs::metadata(&config_path)
      .ok()
      .and_then(|metadata| metadata.modified().ok());
    let mut last_error: Option<String> = None;

    loop {
      if *shutdown_rx.borrow() {
        break;
      }

      tokio::select! {
        res = shutdown_rx.changed() => {
          match res {
            Ok(_) => {
              if *shutdown_rx.borrow() {
                break;
              } else {
                continue;
              }
            }
            Err(_) => break,
          }
        }
        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
      }

      if *shutdown_rx.borrow() {
        break;
      }

      let metadata = match std::fs::metadata(&config_path) {
        Ok(metadata) => {
          if last_error.is_some() {
            info!(
              "Server config file {} became available again",
              config_path.display()
            );
            last_error = None;
          }
          metadata
        }
        Err(err) => {
          let msg = format!("Failed to read server config metadata: {}", err);
          if last_error.as_ref() != Some(&msg) {
            warn!("{}", msg);
            last_error = Some(msg);
          }
          continue;
        }
      };

      let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(err) => {
          let msg = format!("Failed to read server config modified time: {}", err);
          if last_error.as_ref() != Some(&msg) {
            warn!("{}", msg);
            last_error = Some(msg);
          }
          continue;
        }
      };

      if let Some(last) = last_modified {
        if modified <= last {
          continue;
        }
      }

      match provider.load_project_config(config_path.as_path()) {
        Ok(mut new_config) => {
          last_error = None;

          let existing_port = {
            let guard = shared_config.read().await;
            guard.server.listen_port
          };

          if new_config.server.listen_port != existing_port {
            warn!(
              "Ignoring listen_port change from {} to {} in {}; restart to apply",
              existing_port,
              new_config.server.listen_port,
              config_path.display()
            );
            new_config.server.listen_port = existing_port;
          }

          {
            let mut guard = shared_config.write().await;
            *guard = new_config;
          }

          info!("Reloaded server config from {}", config_path.display());
          last_modified = Some(modified);
        }
        Err(err) => {
          let msg = format!("Failed to reload server config: {}", err);
          if last_error.as_ref() != Some(&msg) {
            warn!("{}", msg);
            last_error = Some(msg);
          }
        }
      }
    }
  });
}

pub fn init_server_logging() -> Handle {
  if let Some(log_path) = server_log_path() {
    let path_string = log_path.to_string_lossy().to_string();
    return log2::open(&path_string)
      .size(10 * 1024 * 1024)
      .rotate(5)
      .level("info")
      .tee(true)
      .start();
  }

  log2::start()
}

fn server_log_path() -> Option<PathBuf> {
  let exe_dir = env::current_exe()
    .ok()
    .and_then(|path| path.parent().map(PathBuf::from))?;
  let log_dir = exe_dir.join("logs");
  std::fs::create_dir_all(&log_dir).ok()?;
  Some(log_dir.join("server.log"))
}

struct WatcherGuard {
  sender: watch::Sender<bool>,
}

impl Drop for WatcherGuard {
  fn drop(&mut self) {
    let _ = self.sender.send(true);
  }
}

fn service_error(context: &str, err: impl std::fmt::Display) -> Box<AdeployError> {
  Box::new(AdeployError::Service(format!("{context}: {err}")))
}

fn build_service_manager(user: bool) -> Result<Box<dyn ServiceManager>> {
  let mut manager = <dyn ServiceManager>::native()
    .map_err(|e| service_error("Failed to detect native service manager", e))?;

  if user {
    manager
      .set_level(ServiceLevel::User)
      .map_err(|e| service_error("User-level services are not supported on this system", e))?;
  }

  let available = manager
    .available()
    .map_err(|e| service_error("Failed to query service manager availability", e))?;
  if !available {
    return Err(Box::new(AdeployError::Service(
      "Native service manager is not available on this system".to_string(),
    )));
  }

  Ok(manager)
}

fn parse_service_label(label: &str) -> Result<ServiceLabel> {
  label
    .parse::<ServiceLabel>()
    .map_err(|e| service_error(&format!("Invalid service label '{label}'"), e))
}

pub fn install_service(
  label: &str,
  user: bool,
  autostart: bool,
  disable_restart_on_failure: bool,
  working_directory: Option<PathBuf>,
  username: Option<String>,
) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let program = env::current_exe()
    .map_err(|e| service_error("Failed to resolve current executable path", e))?;
  let args = vec![
    OsString::from("server"),
    OsString::from("run"),
    OsString::from("--service-label"),
    OsString::from(label),
  ];

  let manager = build_service_manager(user)?;
  manager
    .install(ServiceInstallCtx {
      label: service_label.clone(),
      program,
      args,
      contents: None,
      username,
      working_directory,
      environment: None,
      autostart,
      disable_restart_on_failure,
    })
    .map_err(|e| service_error(&format!("Failed to install service '{service_label}'"), e))?;

  Ok(())
}

pub fn uninstall_service(label: &str, user: bool) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .uninstall(ServiceUninstallCtx {
      label: service_label.clone(),
    })
    .map_err(|e| service_error(&format!("Failed to uninstall service '{service_label}'"), e))?;
  Ok(())
}

pub fn start_service(label: &str, user: bool) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .start(ServiceStartCtx {
      label: service_label.clone(),
    })
    .map_err(|e| service_error(&format!("Failed to start service '{service_label}'"), e))?;
  Ok(())
}

pub fn stop_service(label: &str, user: bool) -> Result<()> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .stop(ServiceStopCtx {
      label: service_label.clone(),
    })
    .map_err(|e| service_error(&format!("Failed to stop service '{service_label}'"), e))?;
  Ok(())
}

pub fn service_status(label: &str, user: bool) -> Result<ServiceStatus> {
  let service_label = parse_service_label(label)?;
  let manager = build_service_manager(user)?;
  manager
    .status(ServiceStatusCtx {
      label: service_label.clone(),
    })
    .map_err(|e| {
      service_error(
        &format!("Failed to check service status for '{service_label}'"),
        e,
      )
    })
}

pub fn format_service_status(status: &ServiceStatus) -> String {
  match status {
    ServiceStatus::NotInstalled => "not installed".to_string(),
    ServiceStatus::Running => "running".to_string(),
    ServiceStatus::Stopped(Some(reason)) => format!("stopped ({reason})"),
    ServiceStatus::Stopped(None) => "stopped".to_string(),
  }
}

#[cfg(windows)]
mod windows_service_support {
  use std::{
    ffi::OsString,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
  };

  use tokio::{runtime::Builder as RuntimeBuilder, sync::oneshot};
  use windows_service::{
    define_windows_service,
    service::{
      ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
      ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher, Error,
  };
  use windows_sys::Win32::Foundation::ERROR_FAILED_SERVICE_CONTROLLER_CONNECT;

  use super::*;

  define_windows_service!(ffi_service_main, service_main);

  static SERVICE_PROVIDER: OnceLock<Arc<dyn ConfigProvider>> = OnceLock::new();
  static SERVICE_NAME: OnceLock<String> = OnceLock::new();
  const DEFAULT_SERVICE_NAME: &str = "adeploy";

  pub fn try_run_windows_service(
    provider: Arc<dyn ConfigProvider>,
    service_name: &str,
  ) -> Result<bool> {
    let owned_name = service_name.to_string();
    let _ = SERVICE_PROVIDER.set(provider.clone());
    let _ = SERVICE_NAME.set(owned_name.clone());

    match service_dispatcher::start(&owned_name, ffi_service_main) {
      Ok(()) => Ok(true),
      Err(Error::Winapi(io_err))
        if io_err.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) =>
      {
        Ok(false)
      }
      Err(err) => Err(service_error(
        "Failed to register Windows service dispatcher",
        err,
      )),
    }
  }

  fn service_main(_arguments: Vec<OsString>) {
    let provider = match SERVICE_PROVIDER.get() {
      Some(provider) => provider.clone(),
      None => {
        error!("ADeploy service provider not initialised");
        return;
      }
    };
    let service_name = SERVICE_NAME
      .get()
      .cloned()
      .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());

    run_service(provider, service_name);
  }

  fn run_service(provider: Arc<dyn ConfigProvider>, service_name: String) {
    info!("Launching ADeploy Windows service '{service_name}'");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_signal = Arc::new(Mutex::new(Some(shutdown_tx)));
    let handle_slot: Arc<Mutex<Option<ServiceStatusHandle>>> = Arc::new(Mutex::new(None));

    let shutdown_signal_for_handler = shutdown_signal.clone();
    let handle_slot_for_handler = handle_slot.clone();

    let status_handle =
      match service_control_handler::register(&service_name, move |control_event| {
        match control_event {
          ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
          ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(handle) = handle_slot_for_handler.lock().unwrap().as_ref() {
              let _ = handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::StopPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::NO_ERROR,
                checkpoint: 1,
                wait_hint: Duration::from_secs(5),
                process_id: None,
              });
            }
            if let Some(sender) = shutdown_signal_for_handler.lock().unwrap().take() {
              let _ = sender.send(());
            }
            ServiceControlHandlerResult::NoError
          }
          _ => ServiceControlHandlerResult::NotImplemented,
        }
      }) {
        Ok(handle) => handle,
        Err(err) => {
          error!("Failed to register Windows service handler: {err}");
          return;
        }
      };

    handle_slot.lock().unwrap().replace(status_handle);

    let start_pending = ServiceStatus {
      service_type: ServiceType::OWN_PROCESS,
      current_state: ServiceState::StartPending,
      controls_accepted: ServiceControlAccept::empty(),
      exit_code: ServiceExitCode::NO_ERROR,
      checkpoint: 1,
      wait_hint: Duration::from_secs(10),
      process_id: None,
    };

    if let Err(err) = status_handle.set_service_status(start_pending) {
      error!("Failed to report service start pending: {err}");
      return;
    }

    let running_status = ServiceStatus {
      service_type: ServiceType::OWN_PROCESS,
      current_state: ServiceState::Running,
      controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
      exit_code: ServiceExitCode::NO_ERROR,
      checkpoint: 0,
      wait_hint: Duration::default(),
      process_id: None,
    };

    if let Err(err) = status_handle.set_service_status(running_status) {
      error!("Failed to report service running: {err}");
      return;
    }

    let runtime = match RuntimeBuilder::new_multi_thread().enable_all().build() {
      Ok(rt) => rt,
      Err(err) => {
        error!("Failed to build Tokio runtime for Windows service: {err}");
        let _ = status_handle.set_service_status(ServiceStatus {
          service_type: ServiceType::OWN_PROCESS,
          current_state: ServiceState::Stopped,
          controls_accepted: ServiceControlAccept::empty(),
          exit_code: ServiceExitCode::ServiceSpecific(1),
          checkpoint: 0,
          wait_hint: Duration::default(),
          process_id: None,
        });
        return;
      }
    };

    let shutdown_future = async {
      let _ = shutdown_rx.await;
    };

    let result = runtime.block_on(super::start_server_with_shutdown(provider, shutdown_future));

    drop(runtime);

    let final_exit = match result {
      Ok(_) => ServiceExitCode::NO_ERROR,
      Err(err) => {
        error!("ADeploy service terminated with error: {err}");
        ServiceExitCode::ServiceSpecific(1)
      }
    };

    let stopped_status = ServiceStatus {
      service_type: ServiceType::OWN_PROCESS,
      current_state: ServiceState::Stopped,
      controls_accepted: ServiceControlAccept::empty(),
      exit_code: final_exit,
      checkpoint: 0,
      wait_hint: Duration::default(),
      process_id: None,
    };

    if let Err(err) = status_handle.set_service_status(stopped_status) {
      error!("Failed to report service stopped state: {err}");
    }
  }
}

#[cfg(windows)]
pub use windows_service_support::try_run_windows_service;

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn try_run_windows_service(
  _provider: Arc<dyn ConfigProvider>,
  _service_name: &str,
) -> Result<bool> {
  Ok(false)
}
