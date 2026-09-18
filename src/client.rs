use std::{
  convert::TryFrom,
  path::{Path, PathBuf},
  time::Duration,
};

use async_stream::stream;
use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tokio::time::timeout;
use tokio_stream::Stream;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::{
  adeploy::{
    deploy_chunk::Payload, deploy_event::Event, deploy_log::Level as DeployLogLevel,
    deploy_service_client::DeployServiceClient, BackupListRequest, DeployChunk, DeployEvent,
    DeployLog, DeployManifest, DeployResult, DeployStart, PairRequest, PairState, RollbackRequest,
  },
  auth::{
    backup_list_signing_payload, deploy_start_signing_payload, fingerprint, pair_signing_payload,
    rollback_signing_payload, Auth,
  },
  config::{ConfigProvider, LoadedConfig, ProjectConfig, ResolvedRemote},
  deploy::{describe_archive, DeployManager},
  error::{AdeployError, Result},
  replay::now_ms,
};

/// Cap on the response, which carries only the server's deploy log.
///
/// The request has no client-side cap: how large an archive may be is the
/// server's policy, and it reports its own limit when it refuses one.
const MAX_RESPONSE_SIZE: usize = 16 * 1024 * 1024;

/// Bytes per upload message.
///
/// Large enough that per-message overhead is negligible, small enough that
/// neither end holds much of the archive at once and progress stays granular.
const CHUNK_SIZE: usize = 1024 * 1024;

/// Report upload progress at each multiple of this percentage.
const PROGRESS_STEP: u64 = 20;

/// How often the client checks that the server is still there, and how long a
/// ping may go unanswered. A dead link during a long deployment would otherwise
/// look exactly like a slow one.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long past the deadline the client keeps listening.
///
/// Both ends hold the same deadline, and the client's clock starts fractionally
/// earlier, so without this it always gives up first and reports a silent
/// server rather than the verdict the server is in the middle of sending. This
/// leaves the client-side limit as what it should be: a backstop for a server
/// that has stopped answering altogether.
const DEADLINE_GRACE: Duration = Duration::from_secs(2);

/// Deploy specific packages using an explicit provider
pub async fn deploy(
  host: &str,
  package_names: Option<Vec<String>>,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let loaded = provider.load()?;
  info!("Loaded configuration from {}", loaded.path.display());

  let remote = loaded.config.resolve_remote(host);
  let mut client = connect_deploy_client(host, &remote).await?;
  let deploy_manager = DeployManager::new();
  let auth_resources = prepare_auth_resources(provider)?;
  let packages_to_deploy = select_packages(&loaded, package_names)?;

  for package in packages_to_deploy {
    deploy_single_package(
      &deploy_manager,
      &mut client,
      &auth_resources.ssh_auth,
      &auth_resources.public_key,
      &package,
      &remote,
    )
    .await?;
  }

  Ok(())
}

struct AuthResources {
  ssh_auth: Auth,
  public_key: String,
}

/// Everything the server needs to know about a package, read from this project.
///
/// The server holds no record of any package, so this travels with every
/// request that names one - and is signed, so it cannot be rewritten in flight.
fn manifest_for(config: &ProjectConfig, package: &str) -> Result<DeployManifest> {
  let declared = config.packages.get(package).ok_or_else(|| {
    Box::new(AdeployError::Config(format!(
      "Package '{}' is not declared in this project",
      package
    )))
  })?;

  let deploy_path = declared.deploy_path.clone().ok_or_else(|| {
    Box::new(AdeployError::Config(format!(
      "Package '{}' has no deploy_path; the server needs one to know where it goes",
      package
    )))
  })?;

  // Absolute because there is no server-side root to resolve against any more,
  // and a path that means something different on each machine is worse than one
  // that is refused here.
  if !Path::new(&deploy_path).is_absolute() {
    return Err(Box::new(AdeployError::Config(format!(
      "deploy_path for '{}' must be absolute, got '{}'",
      package, deploy_path
    ))));
  }

  Ok(DeployManifest {
    deploy_path,
    backup_enabled: declared.backup_enabled,
    before_deploy: declared.before_deploy.clone(),
    after_deploy: declared.after_deploy.clone(),
  })
}

/// Show what this project declares, without contacting anything.
///
/// The package and remote names are the arguments every other command takes,
/// and reading them out of the file by hand is the sort of friction that makes
/// a tool feel heavier than it is.
pub fn list(provider: &dyn ConfigProvider) -> Result<()> {
  let loaded = provider.load()?;
  info!("Configuration: {}", loaded.path.display());

  if loaded.config.packages.is_empty() {
    warn!("No packages are declared; add a [packages.<name>] table");
  } else {
    info!("Packages:");
    let mut names: Vec<&String> = loaded.config.packages.keys().collect();
    names.sort();
    for name in names {
      let sources = loaded
        .config
        .resolved_sources(name, &loaded.base_dir)
        .unwrap_or_default();

      info!("  {}", name);
      for source in sources {
        // A source that is not there is the most common reason packaging
        // fails, and it costs nothing to say so before anyone tries.
        let marker = if source.exists() { "" } else { "   (missing)" };
        info!("      {}{}", source.display(), marker);
      }
    }
  }

  info!("Remotes:");
  // Looking up "default" answers what a host with no entry of its own gets,
  // whether that comes from [remotes.default] or straight from [defaults].
  let fallback = loaded.config.resolve_remote("default");
  info!(
    "  {:<16}  port {}  connect {}s  deploy {}s",
    "(any other host)", fallback.port, fallback.connect_timeout, fallback.deploy_timeout
  );

  let mut hosts: Vec<&String> = loaded
    .config
    .remotes
    .keys()
    .filter(|host| host.as_str() != "default")
    .collect();
  hosts.sort();
  for host in hosts {
    let remote = loaded.config.resolve_remote(host);
    info!(
      "  {:<16}  port {}  connect {}s  deploy {}s",
      host, remote.port, remote.connect_timeout, remote.deploy_timeout
    );
  }

  Ok(())
}

/// Package everything that would be sent, and report it without sending.
///
/// The archive is really built, so this also answers whether it can be.
pub async fn dry_run(
  host: &str,
  package_names: Option<Vec<String>>,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let loaded = provider.load()?;
  info!("Loaded configuration from {}", loaded.path.display());

  let remote = loaded.config.resolve_remote(host);
  let deploy_manager = DeployManager::new();
  let packages = select_packages(&loaded, package_names)?;

  for package in packages {
    let (archive, hash) = deploy_manager
      .package_files(&package.name, &package.sources)
      .await?;

    let entries = describe_archive(&archive)?;
    let uncompressed: u64 = entries.iter().map(|entry| entry.size).sum();

    info!("Would deploy {} to {}:{}", package.name, host, remote.port);
    for entry in &entries {
      info!("      {:<48}  {}", entry.path, format_size(entry.size));
    }
    info!(
      "  {} file(s), {} packed into {}, sha256 {}",
      entries.len(),
      format_size(uncompressed),
      format_size(archive.len() as u64),
      hash
    );
  }

  info!("Nothing was sent; drop --dry-run to deploy");
  Ok(())
}

/// Show which snapshots `host` holds for `package`.
pub async fn list_backups(host: &str, package: &str, provider: &dyn ConfigProvider) -> Result<()> {
  let loaded = provider.load()?;
  let remote = loaded.config.resolve_remote(host);
  let auth = prepare_auth_resources(provider)?;

  let nonce = Uuid::new_v4().to_string();
  let timestamp_ms = now_ms();
  let manifest = manifest_for(&loaded.config, package)?;
  let payload = backup_list_signing_payload(
    package,
    &auth.public_key,
    &nonce,
    timestamp_ms,
    Some(&manifest),
  );
  let signature = sign(&auth, &payload)?;

  let mut client = connect_deploy_client(host, &remote).await?;
  let response = client
    .list_backups(tonic::Request::new(BackupListRequest {
      package_name: package.to_string(),
      public_key: auth.public_key.clone(),
      nonce,
      timestamp_ms,
      manifest: Some(manifest),
      signature,
    }))
    .await
    .map_err(|status| unauthenticated_hint(status, &auth.public_key))?
    .into_inner();

  if response.backups.is_empty() {
    info!(
      "{} holds no snapshots of {}; a deployment with backup_enabled creates them",
      host, package
    );
    return Ok(());
  }

  info!("Snapshots of {} on {}, newest first:", package, host);
  for backup in &response.backups {
    info!(
      "  {}  {}  ({})",
      backup.name,
      format_timestamp(backup.created_ms),
      format_size(backup.size_bytes)
    );
  }
  info!(
    "Roll back with `adeploy rollback {} {} --to <name>`",
    host, package
  );
  Ok(())
}

/// Put a snapshot back on `host`.
pub async fn rollback(
  host: &str,
  package: &str,
  backup_name: Option<String>,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let loaded = provider.load()?;
  let remote = loaded.config.resolve_remote(host);
  let auth = prepare_auth_resources(provider)?;
  let backup_name = backup_name.unwrap_or_default();

  let nonce = Uuid::new_v4().to_string();
  let timestamp_ms = now_ms();
  let manifest = manifest_for(&loaded.config, package)?;
  let payload = rollback_signing_payload(
    package,
    &backup_name,
    &auth.public_key,
    &nonce,
    timestamp_ms,
    remote.deploy_timeout,
    Some(&manifest),
  );
  let signature = sign(&auth, &payload)?;

  if backup_name.is_empty() {
    info!(
      "Rolling {} back to its most recent snapshot on {}",
      package, host
    );
  } else {
    info!("Rolling {} back to {} on {}", package, backup_name, host);
  }

  let mut client = connect_deploy_client(host, &remote).await?;
  let request = tonic::Request::new(RollbackRequest {
    package_name: package.to_string(),
    backup_name,
    public_key: auth.public_key.clone(),
    nonce,
    timestamp_ms,
    deploy_timeout_secs: remote.deploy_timeout,
    manifest: Some(manifest),
    signature,
  });
  let response = client
    .rollback(request)
    .await
    .map_err(|status| unauthenticated_hint(status, &auth.public_key))?;

  consume_events(
    response.into_inner(),
    package,
    Operation::Rollback,
    silence_limit(remote.deploy_timeout),
  )
  .await
}

fn sign(auth: &AuthResources, payload: &[u8]) -> Result<String> {
  let signature = auth
    .ssh_auth
    .sign_data(payload)
    .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign request: {}", e))))?;
  Ok(general_purpose::STANDARD.encode(&signature))
}

/// Turn a rejection into advice, rather than leaving the user to guess.
fn unauthenticated_hint(status: tonic::Status, public_key: &str) -> Box<AdeployError> {
  if status.code() == tonic::Code::Unauthenticated {
    error!(
      "Rejected (unauthenticated). This machine's key fingerprint is {}",
      fingerprint(public_key)
    );
    error!("Run `adeploy pair <host>` to request access, then have an operator approve it");
  }
  Box::new(AdeployError::Grpc(status))
}

fn format_timestamp(created_ms: i64) -> String {
  chrono::DateTime::from_timestamp_millis(created_ms)
    .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
    .unwrap_or_else(|| "unknown time".to_string())
}

fn format_size(bytes: u64) -> String {
  const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
  let mut size = bytes as f64;
  let mut unit = 0;
  while size >= 1024.0 && unit < UNITS.len() - 1 {
    size /= 1024.0;
    unit += 1;
  }
  if unit == 0 {
    format!("{} {}", bytes, UNITS[unit])
  } else {
    format!("{:.1} {}", size, UNITS[unit])
  }
}

/// Ask `host` to trust this machine's key.
///
/// Deliberately a separate command rather than something a failed deployment
/// does on its own: joining a server is a decision, and the operator on the
/// other end has to be told to expect it.
pub async fn pair(host: &str, provider: &dyn ConfigProvider) -> Result<()> {
  let loaded = provider.load()?;
  let remote = loaded.config.resolve_remote(host);
  let auth = prepare_auth_resources(provider)?;

  let key_fingerprint = fingerprint(&auth.public_key);
  let client_name = local_hostname();

  info!("Pairing with {}:{} as {}", host, remote.port, client_name);
  info!("This machine's key fingerprint: {}", key_fingerprint);

  let nonce = Uuid::new_v4().to_string();
  let timestamp_ms = now_ms();
  let payload = pair_signing_payload(&auth.public_key, &client_name, &nonce, timestamp_ms);
  let signature = auth
    .ssh_auth
    .sign_data(&payload)
    .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign request: {}", e))))?;

  let mut client = connect_deploy_client(host, &remote).await?;
  let response = client
    .pair(tonic::Request::new(PairRequest {
      public_key: auth.public_key.clone(),
      client_name,
      nonce,
      timestamp_ms,
      signature: general_purpose::STANDARD.encode(&signature),
    }))
    .await
    .map_err(|status| Box::new(AdeployError::Grpc(status)))?
    .into_inner();

  report_pair_state(
    host,
    &response.state,
    &response.fingerprint,
    &response.message,
  );
  Ok(())
}

fn report_pair_state(host: &str, state: &i32, server_fingerprint: &str, message: &str) {
  match PairState::try_from(*state).unwrap_or(PairState::Unspecified) {
    PairState::Approved => {
      info!(
        "{} already trusts this machine; deployments will work",
        host
      );
    }
    PairState::Pending => {
      info!("Request queued on {}: {}", host, message);
      warn!(
        "Approve it on {} with:  adeploy server approve {}",
        host, server_fingerprint
      );
      warn!("Check that fingerprint matches the one printed above before approving");
    }
    PairState::Rejected => {
      error!("{} has refused this key: {}", host, message);
    }
    PairState::Unspecified => {
      warn!(
        "{} returned an unrecognised pairing state: {}",
        host, message
      );
    }
  }
}

/// Name shown in the server's pending list.
fn local_hostname() -> String {
  hostname::get()
    .ok()
    .and_then(|name| name.into_string().ok())
    .unwrap_or_else(|| "unknown-host".to_string())
}

/// A package selected for deployment, with its sources already resolved to
/// absolute paths against the directory holding `adeploy.toml`.
struct SelectedPackage {
  name: String,
  sources: Vec<PathBuf>,
  manifest: DeployManifest,
}

async fn connect_deploy_client(
  host: &str,
  remote: &ResolvedRemote,
) -> Result<DeployServiceClient<Channel>> {
  info!("Connecting to {}:{}", host, remote.port);

  let endpoint_uri = format!("http://{}:{}", host, remote.port);
  let endpoint = Channel::from_shared(endpoint_uri)
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid endpoint: {}", e))))?;
  let endpoint = configure_endpoint(endpoint, remote);
  let channel = endpoint.connect().await.map_err(|e| {
    Box::new(AdeployError::Network(format!(
      "Failed to connect to {}:{}: {}",
      host, remote.port, e
    )))
  })?;

  Ok(DeployServiceClient::new(channel).max_decoding_message_size(MAX_RESPONSE_SIZE))
}

fn prepare_auth_resources(provider: &dyn ConfigProvider) -> Result<AuthResources> {
  let key_paths = provider.get_key_paths()?;
  let private_key_path = key_paths.private_key;
  let public_key_path = key_paths.public_key;

  let keypair = Auth::load_key_pair(&private_key_path.to_string_lossy()).map_err(|e| {
    Box::new(AdeployError::Auth(format!(
      "Failed to load signing key pair: {}",
      e
    )))
  })?;
  let ssh_auth = Auth::with_key_pair(keypair);

  let public_key = Auth::load_public_key(&public_key_path).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to load public key: {}",
      e
    )))
  })?;

  Ok(AuthResources {
    ssh_auth,
    public_key,
  })
}

fn select_packages(
  loaded: &LoadedConfig,
  package_names: Option<Vec<String>>,
) -> Result<Vec<SelectedPackage>> {
  let Some(names) = package_names else {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  };

  let mut packages = Vec::new();
  for name in names {
    let Some(sources) = loaded.config.resolved_sources(&name, &loaded.base_dir) else {
      continue;
    };
    let manifest = manifest_for(&loaded.config, &name)?;
    packages.push(SelectedPackage {
      name,
      sources,
      manifest,
    });
  }

  if packages.is_empty() {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  }

  Ok(packages)
}

async fn deploy_single_package(
  deploy_manager: &DeployManager,
  client: &mut DeployServiceClient<Channel>,
  ssh_auth: &Auth,
  public_key: &str,
  package: &SelectedPackage,
  remote: &ResolvedRemote,
) -> Result<()> {
  let package_name = package.name.as_str();
  info!("Deploying {}", package_name);

  let (archive_data, file_hash) = deploy_manager
    .package_files(package_name, &package.sources)
    .await?;

  let start = build_start_message(
    ssh_auth,
    public_key,
    package_name,
    &archive_data,
    file_hash,
    remote,
    package,
  )?;
  let total_size = start.total_size;

  let request = tonic::Request::new(upload_stream(start, archive_data));

  // Send the deadline with the request rather than keeping it on the channel.
  // `Endpoint::timeout` is client-side only, so the server kept unpacking and
  // running hooks after the client had already given up. The `grpc-timeout`
  // metadata this sets is honoured by tonic on both ends, so the two stop
  // together and cannot disagree about when.
  let response = match client.deploy(request).await {
    Ok(response) => response,
    Err(status) => {
      if status.code() == tonic::Code::Unauthenticated {
        error!(
          "Deployment rejected (unauthenticated). This machine's key fingerprint is {}",
          fingerprint(public_key)
        );
        error!("Run `adeploy pair <host>` to request access, then have an operator approve it");
      }
      return Err(Box::new(AdeployError::Grpc(status)));
    }
  };

  consume_events(
    response.into_inner(),
    package_name,
    Operation::Deploy { total_size },
    silence_limit(remote.deploy_timeout),
  )
  .await
}

/// How long the client tolerates hearing nothing at all from the server.
///
/// Measured between events rather than from the start, because the client
/// cannot see the moment the server considered the upload finished, which is
/// where the server's own deploy deadline begins. The grace lets the server's
/// verdict arrive first, leaving this as what it should be: a backstop for a
/// server that has stopped answering altogether.
fn silence_limit(deploy_timeout: u64) -> Option<Duration> {
  (deploy_timeout > 0).then(|| Duration::from_secs(deploy_timeout) + DEADLINE_GRACE)
}

/// Describe and sign the upload that is about to start.
fn build_start_message(
  ssh_auth: &Auth,
  public_key: &str,
  package_name: &str,
  archive_data: &[u8],
  file_hash: String,
  remote: &ResolvedRemote,
  package: &SelectedPackage,
) -> Result<DeployStart> {
  let total_size = archive_data.len() as u64;
  let nonce = Uuid::new_v4().to_string();
  let timestamp_ms = now_ms();

  // Signing the description rather than only the archive binds these bytes to
  // this package, this size and this one-time nonce.
  let payload = deploy_start_signing_payload(
    package_name,
    total_size,
    &file_hash,
    public_key,
    &nonce,
    timestamp_ms,
    remote.deploy_timeout,
    Some(&package.manifest),
  );
  let signature = ssh_auth
    .sign_data(&payload)
    .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign request: {}", e))))?;

  Ok(DeployStart {
    package_name: package_name.to_string(),
    total_size,
    file_hash,
    public_key: public_key.to_string(),
    nonce,
    timestamp_ms,
    deploy_timeout_secs: remote.deploy_timeout,
    manifest: Some(package.manifest.clone()),
    signature: general_purpose::STANDARD.encode(&signature),
  })
}

/// The opening message followed by the archive, split into chunks.
fn upload_stream(
  start: DeployStart,
  archive_data: Vec<u8>,
) -> impl Stream<Item = DeployChunk> + Send + 'static {
  let total_size = start.total_size.max(1);

  stream! {
    yield DeployChunk { payload: Some(Payload::Start(start)) };

    let mut offset = 0usize;
    let mut next_report = PROGRESS_STEP;

    while offset < archive_data.len() {
      let end = (offset + CHUNK_SIZE).min(archive_data.len());
      let chunk = archive_data[offset..end].to_vec();
      offset = end;

      yield DeployChunk { payload: Some(Payload::Data(chunk)) };

      let percent = (offset as u64).saturating_mul(100) / total_size;
      if percent >= next_report {
        info!("Uploaded {}% ({}/{} bytes)", percent, offset, total_size);
        while next_report <= percent {
          next_report += PROGRESS_STEP;
        }
      }
    }
  }
}

/// What the event stream is reporting on, so its messages can say so.
enum Operation {
  Deploy { total_size: u64 },
  Rollback,
}

impl Operation {
  fn accepted(&self, package_name: &str, deploy_id: &str) -> String {
    match self {
      Self::Deploy { total_size } => format!(
        "Server accepted {} ({} bytes), deploy ID {}",
        package_name, total_size, deploy_id
      ),
      Self::Rollback => format!(
        "Server accepted the rollback of {}, ID {}",
        package_name, deploy_id
      ),
    }
  }

  fn succeeded(&self, package_name: &str, deploy_id: &str) -> String {
    match self {
      Self::Deploy { .. } => format!(
        "Deployment succeeded for {} (ID: {})",
        package_name, deploy_id
      ),
      Self::Rollback => format!(
        "Rollback succeeded for {} (ID: {})",
        package_name, deploy_id
      ),
    }
  }

  fn failed(&self, package_name: &str, message: &str) -> String {
    match self {
      Self::Deploy { .. } => format!("Package {} deployment failed: {}", package_name, message),
      Self::Rollback => format!("Package {} rollback failed: {}", package_name, message),
    }
  }
}

/// Render the server's events as they arrive, and report the final outcome.
///
/// `deadline` is enforced here as well as on the server. tonic applies
/// `grpc-timeout` to the future that produces the response, not to the stream
/// that follows, so without this a server that stopped answering mid-deployment
/// would leave the client waiting indefinitely.
async fn consume_events(
  mut events: tonic::Streaming<DeployEvent>,
  package_name: &str,
  operation: Operation,
  silence_limit: Option<Duration>,
) -> Result<()> {
  let mut outcome: Option<DeployResult> = None;

  loop {
    let next = match silence_limit {
      Some(limit) => match timeout(limit, events.message()).await {
        Ok(next) => next,
        Err(_) => {
          return Err(Box::new(AdeployError::Deploy(format!(
            "{} heard nothing from the server for {}s",
            package_name,
            limit.as_secs()
          ))))
        }
      },
      None => events.message().await,
    };

    let message = next.map_err(|status| Box::new(AdeployError::Grpc(status)))?;

    let Some(event) = message else { break };

    match event.event {
      Some(Event::Accepted(accepted)) => {
        info!("{}", operation.accepted(package_name, &accepted.deploy_id));
      }
      Some(Event::Log(entry)) => log_deploy_server_entry(&entry),
      Some(Event::Result(result)) => outcome = Some(result),
      None => {}
    }
  }

  match outcome {
    Some(result) if result.success => {
      info!("{}", operation.succeeded(package_name, &result.deploy_id));
      Ok(())
    }
    Some(result) => {
      error!("Failed for {}: {}", package_name, result.message);
      Err(Box::new(AdeployError::Deploy(
        operation.failed(package_name, &result.message),
      )))
    }
    // The stream ended without a verdict, which means the server went away
    // mid-deployment rather than deciding anything.
    None => Err(Box::new(AdeployError::Deploy(format!(
      "Package {} deployment ended without a result from the server",
      package_name
    )))),
  }
}

/// Bound how long reaching the host may take, and keep the link checked.
///
/// The upload is not given a deadline of its own: a broken connection is an
/// error already, and a silently dead one is what the keepalive pings are for.
/// The deployment deadline travels in the request instead, so the server
/// applies the same one from the moment the last byte lands.
fn configure_endpoint(endpoint: Endpoint, remote: &ResolvedRemote) -> Endpoint {
  let endpoint = endpoint
    .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
    .keep_alive_timeout(KEEPALIVE_TIMEOUT);

  if remote.connect_timeout == 0 {
    endpoint
  } else {
    endpoint.connect_timeout(Duration::from_secs(remote.connect_timeout))
  }
}

fn log_deploy_server_entry(entry: &DeployLog) {
  let level = DeployLogLevel::try_from(entry.level).unwrap_or(DeployLogLevel::Info);
  let message = entry.message.as_str();
  match level {
    DeployLogLevel::Error => error!("{}", message),
    DeployLogLevel::Warn => warn!("{}", message),
    DeployLogLevel::Unspecified | DeployLogLevel::Info => info!("{}", message),
  }
}
