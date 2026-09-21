use std::{
  convert::TryFrom,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use async_stream::stream;
use base64::{engine::general_purpose, Engine as _};
use log2::*;
use tempfile::{Builder, NamedTempFile};
use tokio::{io::AsyncReadExt, time::timeout};
use tokio_stream::Stream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use uuid::Uuid;

use crate::{
  adeploy::{
    deploy_chunk::Payload, deploy_event::Event, deploy_log::Level as DeployLogLevel,
    deploy_service_client::DeployServiceClient, BackupListRequest, DeployChunk, DeployEvent,
    DeployLog, DeployManifest, DeployResult, DeployStart, PairRequest, PairResponse, PairState,
    RollbackRequest,
  },
  auth::{
    backup_list_signing_payload, deploy_start_signing_payload, fingerprint, pair_signing_payload,
    rollback_signing_payload, Auth,
  },
  config::{ConfigProvider, LoadedConfig, ProjectConfig, ResolvedRemote},
  deploy::{describe_archive, DeployManager},
  error::{AdeployError, Result},
  identity::{fetch_server_certificate, SERVER_TLS_NAME},
  known_servers::{KnownServers, RecordOutcome},
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

/// What this client will accept from the other end of a connection.
///
/// Signatures already say who is asking and bind a request to the archive it
/// describes. This is the other direction: which server the client believes it
/// reached, and whether anyone in between can read what it sends.
enum ServerTrust {
  /// No TLS. Everything crosses the network in the clear and any machine on
  /// that address will do.
  Insecure,
  /// Exactly the certificate recorded when this machine paired with the host.
  Pinned(String),
}

/// Work out what this client will accept from `host`.
///
/// A host with nothing on file is refused rather than trusted on sight: the
/// first connection to a server is the one an impostor would most like to have,
/// and `adeploy pair` exists to make that moment deliberate.
fn server_trust(
  host: &str,
  remote: &ResolvedRemote,
  provider: &dyn ConfigProvider,
) -> Result<ServerTrust> {
  if !remote.tls {
    return Ok(ServerTrust::Insecure);
  }

  let path = provider.get_key_paths()?.known_servers();
  let known = KnownServers::load(&path)?;
  let server = known.get(host).ok_or_else(|| {
    Box::new(AdeployError::Config(format!(
      "This machine has not paired with {host}. Run `adeploy pair {host}` first, and compare the \
       fingerprint it prints with the `Server identity` line in that server's own log."
    )))
  })?;

  Ok(ServerTrust::Pinned(server.certificate.clone()))
}

/// Deploy specific packages using an explicit provider
pub async fn deploy(
  host: &str,
  package_names: Option<Vec<String>>,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let loaded = provider.load()?;
  info!("Loaded configuration from {}", loaded.path.display());

  let remote = loaded.config.resolve_remote(host);
  let trust = server_trust(host, &remote, provider)?;
  let mut client = connect_deploy_client(host, &remote, &trust).await?;
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
    let archive = staged_archive()?;
    let (size, hash) = deploy_manager
      .package_files_into(&package.name, &package.sources, archive.path())
      .await?;

    let entries = describe_archive(archive.path())?;
    let uncompressed: u64 = entries.iter().map(|entry| entry.size).sum();

    info!("Would deploy {} to {}:{}", package.name, host, remote.port);
    for entry in &entries {
      info!("      {:<48}  {}", entry.path, format_size(entry.size));
    }
    info!(
      "  {} file(s), {} packed into {}, sha256 {}",
      entries.len(),
      format_size(uncompressed),
      format_size(size),
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

  let trust = server_trust(host, &remote, provider)?;
  let mut client = connect_deploy_client(host, &remote, &trust).await?;
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

  let trust = server_trust(host, &remote, provider)?;
  let mut client = connect_deploy_client(host, &remote, &trust).await?;
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
pub async fn pair(
  host: &str,
  force: bool,
  wait: bool,
  provider: &dyn ConfigProvider,
) -> Result<()> {
  let loaded = provider.load()?;
  let remote = loaded.config.resolve_remote(host);
  let auth = prepare_auth_resources(provider)?;

  let key_fingerprint = fingerprint(&auth.public_key);
  let client_name = local_hostname();

  info!("Pairing with {}:{} as {}", host, remote.port, client_name);
  info!("This machine's key fingerprint: {}", key_fingerprint);

  // Both directions are settled here, on the one trip an operator already
  // makes: this machine records who the server is, and the server queues this
  // machine's key for a person to approve.
  let trust = record_server_identity(host, &remote, provider, force).await?;
  let mut client = connect_deploy_client(host, &remote, &trust).await?;

  let response = send_pair_request(&mut client, &auth, &client_name).await?;
  report_pair_state(
    host,
    &response.state,
    &response.fingerprint,
    &response.message,
  );

  let queued = matches!(PairState::try_from(response.state), Ok(PairState::Pending));
  if wait && queued {
    await_approval(&mut client, host, &auth, &client_name).await?;
  }
  Ok(())
}

/// Build and send one pairing request.
///
/// A fresh nonce and timestamp each time, which is what lets the same call be
/// repeated while waiting without the server's replay protection turning the
/// second one away.
async fn send_pair_request(
  client: &mut DeployServiceClient<Channel>,
  auth: &AuthResources,
  client_name: &str,
) -> Result<PairResponse> {
  let nonce = Uuid::new_v4().to_string();
  let timestamp_ms = now_ms();
  let payload = pair_signing_payload(&auth.public_key, client_name, &nonce, timestamp_ms);
  let signature = auth
    .ssh_auth
    .sign_data(&payload)
    .map_err(|e| Box::new(AdeployError::Auth(format!("Failed to sign request: {}", e))))?;

  Ok(
    client
      .pair(tonic::Request::new(PairRequest {
        public_key: auth.public_key.clone(),
        client_name: client_name.to_string(),
        nonce,
        timestamp_ms,
        signature: general_purpose::STANDARD.encode(&signature),
      }))
      .await
      .map_err(|status| Box::new(AdeployError::Grpc(status)))?
      .into_inner(),
  )
}

/// Hold the command open until a person on the server has decided.
///
/// `Pair` is idempotent - a key already queued gets its state back rather than
/// a second slot in the queue - so waiting needs no protocol of its own, only
/// the same call repeated. Nothing is lost by giving up either: the request is
/// already recorded on the server, so Ctrl-C costs the wait and not the work.
async fn await_approval(
  client: &mut DeployServiceClient<Channel>,
  host: &str,
  auth: &AuthResources,
  client_name: &str,
) -> Result<()> {
  info!("Waiting for that approval - Ctrl-C is safe, the request stays queued");

  let started = Instant::now();
  let mut last_heartbeat = Instant::now();
  loop {
    tokio::time::sleep(PAIR_POLL_INTERVAL).await;
    let response = send_pair_request(client, auth, client_name).await?;

    match PairState::try_from(response.state).unwrap_or(PairState::Unspecified) {
      PairState::Approved => {
        info!(
          "Approved by {} after {}; deployments will work now",
          host,
          describe_elapsed(started.elapsed())
        );
        return Ok(());
      }
      PairState::Rejected => {
        return Err(Box::new(AdeployError::Auth(format!(
          "{} refused this key: {}",
          host, response.message
        ))));
      }
      PairState::Pending | PairState::Unspecified => {
        // Say something occasionally, so a long wait stays distinguishable
        // from the silent hang this command used to be able to produce.
        if last_heartbeat.elapsed() >= PAIR_WAIT_HEARTBEAT {
          info!(
            "Still waiting on {} ({} so far)",
            host,
            describe_elapsed(started.elapsed())
          );
          last_heartbeat = Instant::now();
        }
      }
    }
  }
}

/// A wait in the units a person waiting would use.
fn describe_elapsed(elapsed: Duration) -> String {
  let seconds = elapsed.as_secs();
  if seconds < 60 {
    format!("{seconds}s")
  } else {
    format!("{}m{:02}s", seconds / 60, seconds % 60)
  }
}

/// Learn which server answers at `host`, and record it.
///
/// The one connection that cannot verify the other end, for the same reason
/// `Pair` is the one method that cannot require a key: it is what establishes
/// the thing every later connection checks against. That is why the fingerprint
/// is printed rather than quietly filed - an operator running this is already
/// on their way to the server to approve the key, and the identity to compare
/// is in the log they are about to read.
async fn record_server_identity(
  host: &str,
  remote: &ResolvedRemote,
  provider: &dyn ConfigProvider,
  force: bool,
) -> Result<ServerTrust> {
  if !remote.tls {
    warn!(
      "TLS is off for {}, so this pairing cannot tell that server from any other machine on its address",
      host
    );
    return Ok(ServerTrust::Insecure);
  }

  let presented = fetch_server_certificate(
    host,
    remote.port,
    (remote.connect_timeout > 0).then(|| Duration::from_secs(remote.connect_timeout)),
  )
  .await?;
  let path = provider.get_key_paths()?.known_servers();
  let mut known = KnownServers::load(&path)?;

  match known.record(
    host,
    &presented.certificate_pem,
    &presented.fingerprint,
    force,
  ) {
    RecordOutcome::Recorded => {
      info!("Server identity: {}  (recorded)", presented.fingerprint);
      warn!(
        "Check it matches the `Server identity` line in {}'s own log before approving this machine",
        host
      );
    }
    RecordOutcome::Unchanged => {
      info!(
        "Server identity: {}  (already known)",
        presented.fingerprint
      );
    }
    RecordOutcome::Replaced { previous } => {
      warn!("Server identity replaced for {}", host);
      warn!("  was:  {}", previous);
      warn!("  now:  {}", presented.fingerprint);
    }
    RecordOutcome::Conflict { recorded } => {
      error!(
        "{} presented an identity this machine has not seen before",
        host
      );
      error!("  recorded:  {}", recorded);
      error!("  presented: {}", presented.fingerprint);
      return Err(Box::new(AdeployError::Auth(format!(
        "{host} is not the server this machine paired with. If it was rebuilt or replaced, \
         confirm the new fingerprint above against its own log and run \
         `adeploy pair {host} --force`; otherwise something else is answering on that address."
      ))));
    }
  }

  known.save(&path)?;
  Ok(ServerTrust::Pinned(presented.certificate_pem))
}

/// `key_fingerprint` is this machine's own key, as the server read it - the
/// value an operator pastes into `adeploy server approve`. The server's own
/// identity is a different fingerprint entirely, reported above by
/// `record_server_identity`.
fn report_pair_state(host: &str, state: &i32, key_fingerprint: &str, message: &str) {
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
        host, key_fingerprint
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
#[derive(Debug)]
struct SelectedPackage {
  name: String,
  sources: Vec<PathBuf>,
  manifest: DeployManifest,
}

async fn connect_deploy_client(
  host: &str,
  remote: &ResolvedRemote,
  trust: &ServerTrust,
) -> Result<DeployServiceClient<Channel>> {
  info!("Connecting to {}:{}", host, remote.port);

  let pinned = matches!(trust, ServerTrust::Pinned(_));
  let scheme = if pinned { "https" } else { "http" };
  let endpoint = Channel::from_shared(format!("{}://{}:{}", scheme, host, remote.port))
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid endpoint: {}", e))))?;
  let mut endpoint = configure_endpoint(endpoint, remote);

  if let ServerTrust::Pinned(certificate) = trust {
    // The recorded certificate is the whole trust root. No authority signed it
    // and none needs to: the question is only whether this is the same server
    // as last time, and the name inside it is fixed so the address can change.
    endpoint = endpoint
      .tls_config(
        ClientTlsConfig::new()
          .ca_certificate(Certificate::from_pem(certificate))
          .domain_name(SERVER_TLS_NAME),
      )
      .map_err(|e| Box::new(AdeployError::Network(format!("TLS setup failed: {e}"))))?;
  }

  let channel = endpoint.connect().await.map_err(|e| {
    let hint = if pinned {
      format!(
        ". If {host} was rebuilt or replaced, its identity changed; run `adeploy pair {host} --force` after checking that is what happened"
      )
    } else {
      String::new()
    };
    Box::new(AdeployError::Network(format!(
      "Failed to connect to {}:{}: {}{}",
      host,
      remote.port,
      describe_with_causes(&e),
      hint
    )))
  })?;

  Ok(DeployServiceClient::new(channel).max_decoding_message_size(MAX_RESPONSE_SIZE))
}

/// An error together with what actually caused it.
///
/// tonic reports every failed connection as "transport error" and leaves the
/// reason in the source chain, so a certificate that does not match reads as an
/// ordinary network problem - the one case where knowing the difference matters
/// most.
fn describe_with_causes(error: &(dyn std::error::Error + 'static)) -> String {
  let mut description = error.to_string();
  let mut source = error.source();
  while let Some(cause) = source {
    let text = cause.to_string();
    if !description.contains(&text) {
      description.push_str(": ");
      description.push_str(&text);
    }
    source = cause.source();
  }
  description
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
  let mut unknown = Vec::new();
  for name in names {
    let Some(sources) = loaded.config.resolved_sources(&name, &loaded.base_dir) else {
      unknown.push(name);
      continue;
    };
    let manifest = manifest_for(&loaded.config, &name)?;
    packages.push(SelectedPackage {
      name,
      sources,
      manifest,
    });
  }

  // A name with no package used to be skipped in silence, so a typo deployed
  // whatever else was asked for and reported success - which reads exactly like
  // having deployed all of them.
  if !unknown.is_empty() {
    return Err(Box::new(AdeployError::Config(format!(
      "No package named {} in {}. {}",
      unknown
        .iter()
        .map(|name| format!("'{}'", name))
        .collect::<Vec<_>>()
        .join(", "),
      loaded.path.display(),
      declared_packages(&loaded.config),
    ))));
  }

  if packages.is_empty() {
    return Err(Box::new(AdeployError::Config(
      "No packages found to deploy".to_string(),
    )));
  }

  Ok(packages)
}

/// The names that would have worked, for an error saying one did not.
fn declared_packages(config: &ProjectConfig) -> String {
  let mut names: Vec<&str> = config.packages.keys().map(String::as_str).collect();
  if names.is_empty() {
    return "It declares no packages; add a [packages.<name>] table.".to_string();
  }

  names.sort_unstable();
  format!("Declared: {}", names.join(", "))
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

  // Held until the deployment is over: the upload reads from it as it goes,
  // and dropping it removes the file however this ends.
  let archive = staged_archive()?;
  let (total_size, file_hash) = deploy_manager
    .package_files_into(package_name, &package.sources, archive.path())
    .await?;

  let start = build_start_message(
    ssh_auth,
    public_key,
    package_name,
    total_size,
    file_hash,
    remote,
    package,
  )?;

  let request = tonic::Request::new(upload_stream(start, archive.path().to_path_buf()));

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
  total_size: u64,
  file_hash: String,
  remote: &ResolvedRemote,
  package: &SelectedPackage,
) -> Result<DeployStart> {
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

/// The opening message followed by the archive, read a chunk at a time.
///
/// Nothing but the chunk in flight is held: each one is read into a buffer that
/// is then handed to the message, rather than copied out of an archive the
/// process was keeping in memory for the duration.
///
/// A read that fails ends the stream early. There is no way to report it
/// through a stream of messages, but the server counts what it receives and
/// refuses an upload that stops short of its declared size, so the deployment
/// fails rather than half-lands - and the real cause is logged here.
fn upload_stream(
  start: DeployStart,
  archive_path: PathBuf,
) -> impl Stream<Item = DeployChunk> + Send + 'static {
  let total_size = start.total_size.max(1);

  stream! {
    yield DeployChunk { payload: Some(Payload::Start(start)) };

    let mut file = match tokio::fs::File::open(&archive_path).await {
      Ok(file) => file,
      Err(e) => {
        error!("Failed to read {}: {}", archive_path.display(), e);
        return;
      }
    };

    let mut sent = 0u64;
    let mut next_report = PROGRESS_STEP;

    loop {
      let mut chunk = vec![0u8; CHUNK_SIZE];
      let read = match file.read(&mut chunk).await {
        Ok(0) => break,
        Ok(read) => read,
        Err(e) => {
          error!("Failed to read {}: {}", archive_path.display(), e);
          return;
        }
      };
      chunk.truncate(read);
      sent += read as u64;

      yield DeployChunk { payload: Some(Payload::Data(chunk)) };

      let percent = sent.saturating_mul(100) / total_size;
      if percent >= next_report {
        info!("Uploaded {}% ({}/{} bytes)", percent, sent, total_size);
        while next_report <= percent {
          next_report += PROGRESS_STEP;
        }
      }
    }
  }
}

/// A temporary file for the archive about to be built.
///
/// Honours `TMPDIR` and its equivalents, which is the lever to pull when the
/// system temporary directory is small or sits in memory.
fn staged_archive() -> Result<NamedTempFile> {
  Builder::new()
    .prefix("adeploy-")
    .suffix(".tar.gz")
    .tempfile()
    .map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create a temporary file for the archive: {}",
        e
      )))
    })
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
/// How often to ask whether a queued request has been dealt with.
///
/// Paced for a person walking to another machine, not for a machine: the
/// answer arrives when somebody types, and polling faster only adds load.
const PAIR_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often to speak up while waiting.
const PAIR_WAIT_HEARTBEAT: Duration = Duration::from_secs(30);

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

#[cfg(test)]
mod tests {
  use super::*;

  /// A prefix that is absolute on the platform running the test.
  ///
  /// `manifest_for` refuses a `deploy_path` that is not absolute, and on
  /// Windows a leading separator alone is not.
  const ROOT: &str = if cfg!(windows) { "C:/some" } else { "/some" };

  fn loaded(toml_text: &str) -> LoadedConfig {
    LoadedConfig {
      config: toml::from_str(toml_text).expect("configuration should parse"),
      base_dir: PathBuf::from(format!("{ROOT}/projects/app")),
      path: PathBuf::from(format!("{ROOT}/projects/app/adeploy.toml")),
    }
  }

  fn two_packages() -> String {
    format!(
      r#"
[packages.api]
sources = ["./dist/api"]
deploy_path = "{ROOT}/api"

[packages.web]
sources = ["./dist/web"]
deploy_path = "{ROOT}/web"
"#
    )
  }

  #[test]
  fn a_mistyped_name_fails_instead_of_deploying_the_rest() {
    let loaded = loaded(&two_packages());

    let error = select_packages(&loaded, Some(vec!["api".into(), "wbe".into()]))
      .expect_err("a name with no package must not be skipped");

    let message = error.to_string();
    assert!(
      message.contains("'wbe'"),
      "must name the typo, got: {message}"
    );
    assert!(
      message.contains("api, web"),
      "must say what would have worked, got: {message}"
    );
  }

  #[test]
  fn every_unknown_name_is_reported_at_once() {
    let loaded = loaded(&two_packages());

    let error = select_packages(&loaded, Some(vec!["one".into(), "two".into()]))
      .expect_err("unknown names must fail");

    let message = error.to_string();
    assert!(
      message.contains("'one'") && message.contains("'two'"),
      "got: {message}"
    );
  }

  #[test]
  fn names_that_all_exist_are_selected_in_order() {
    let loaded = loaded(&two_packages());

    let selected = select_packages(&loaded, Some(vec!["web".into(), "api".into()]))
      .expect("declared packages should be selected");

    let names: Vec<&str> = selected
      .iter()
      .map(|package| package.name.as_str())
      .collect();
    assert_eq!(names, ["web", "api"]);
    assert_eq!(
      selected[0].sources,
      vec![PathBuf::from(format!("{ROOT}/projects/app/dist/web"))]
    );
  }

  #[test]
  fn an_empty_project_says_so_rather_than_listing_nothing() {
    let loaded = loaded("");

    let error = select_packages(&loaded, Some(vec!["api".into()]))
      .expect_err("an empty project cannot deploy anything");

    assert!(
      error.to_string().contains("[packages.<name>]"),
      "got: {error}"
    );
  }
}
