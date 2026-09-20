//! Protocol-level tests that talk to a real server with hand-built messages.
//!
//! The scenario matrix in `integration_tests.rs` drives `client::deploy`, so it
//! can only produce well-formed, honest requests. These cases are the opposite:
//! what a captured, tampered or lying client can do to a running server.

use std::{
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use adeploy::{
  adeploy::{
    deploy_chunk::Payload, deploy_event::Event, deploy_service_client::DeployServiceClient,
    DeployChunk, DeployManifest, DeployStart, PairRequest, PairState,
  },
  auth::{deploy_start_signing_payload, fingerprint, pair_signing_payload, Auth},
  config::{ConfigProvider, ConfigProviderImpl, KeyPairPaths, ProjectConfig},
  deploy::DeployManager,
  error::Result as AdeployResult,
  pairing::PairStore,
  replay::now_ms,
  server,
};
use base64::{engine::general_purpose, Engine as _};
use ed25519_dalek::SigningKey;
use tempfile::TempDir;
use tokio::time::sleep;
use tonic::transport::Channel;

mod common;

const PACKAGE: &str = "test-app";

#[derive(Clone)]
struct FixedProvider {
  config_path: PathBuf,
  key_paths: KeyPairPaths,
}

impl ConfigProvider for FixedProvider {
  fn get_config_path(&self) -> AdeployResult<PathBuf> {
    Ok(self.config_path.clone())
  }

  fn load_project_config(&self, path: &Path) -> AdeployResult<ProjectConfig> {
    ConfigProviderImpl::default().load_project_config(path)
  }

  fn get_key_paths(&self) -> AdeployResult<KeyPairPaths> {
    Ok(self.key_paths.clone())
  }
}

struct Harness {
  _temp: TempDir,
  port: u16,
  deploy_root: PathBuf,
  paired_path: PathBuf,
  public_key: String,
  signing_key: SigningKey,
  archive: Vec<u8>,
  file_hash: String,
  /// Carried in every start message; 0 leaves the phase unbounded.
  deploy_timeout_secs: u64,
  /// The server holds no package configuration, so every request brings this.
  manifest: DeployManifest,
}

impl Harness {
  /// A running server that trusts one key, plus a real archive to send it.
  async fn start() -> Self {
    Self::start_with(true, false).await
  }

  /// A running server that trusts nobody, so pairing is the only way in.
  async fn start_untrusted() -> Self {
    Self::start_with(false, false).await
  }

  /// A trusted server whose before-deploy hook outlasts any sane deadline.
  async fn start_slow() -> Self {
    let mut harness = Self::start_with(true, true).await;
    harness.deploy_timeout_secs = 1;
    harness
  }

  async fn start_with(trusted: bool, slow_hook: bool) -> Self {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();

    let private_key_path = root.join("id_ed25519");
    let public_key_path = root.join("id_ed25519.pub");
    Auth::generate_key_pair(
      &public_key_path.to_string_lossy(),
      &private_key_path.to_string_lossy(),
    )
    .expect("key pair");

    let public_key = std::fs::read_to_string(&public_key_path)
      .expect("read public key")
      .trim()
      .to_string();
    let signing_key =
      Auth::load_key_pair(&private_key_path.to_string_lossy()).expect("load signing key");

    let source_dir = root.join("src");
    std::fs::create_dir_all(&source_dir).expect("source dir");
    std::fs::write(source_dir.join("payload.txt"), "payload").expect("source file");

    // These tests build their chunks by hand, so they need the bytes; the
    // client itself streams the same file straight to the wire.
    let archive_path = root.join("package.tar.gz");
    let (_, file_hash) = DeployManager::new()
      .package_files_into(PACKAGE, &[source_dir], &archive_path)
      .await
      .expect("package sources");
    let archive = std::fs::read(&archive_path).expect("read archive");

    let port = common::find_available_port().await;
    let deploy_root = root.join("root");
    let config_path = root.join("adeploy.toml");
    let paired_path = root.join("paired.toml");
    let allowlist = if trusted {
      format!("[\"{public_key}\"]")
    } else {
      "[]".to_string()
    };

    // A hook that outlasts the deadline is the clearest way to ask whether the
    // server actually stops, rather than finishing with nobody listening.
    let before_deploy_commands = if slow_hook {
      let (name, body) = if cfg!(target_os = "windows") {
        // `timeout` needs a console and exits 1 when stdin is redirected, which
        // a hook's always is, so the script would fail instead of sleeping.
        ("slow.cmd", "@echo off\r\nping -n 31 127.0.0.1 > nul\r\n")
      } else {
        ("slow.sh", "#!/bin/sh\nsleep 30\n")
      };
      let script = root.join(name);
      std::fs::write(&script, body).expect("hook script");
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("hook permissions");
      }
      vec![script.to_string_lossy().to_string()]
    } else {
      Vec::new()
    };
    std::fs::write(
      &config_path,
      format!(
        r#"[server]
listen_port = {port}
allowed_keys = {allowlist}
"#
      ),
    )
    .expect("write server config");

    let manifest = DeployManifest {
      deploy_path: deploy_root.join(PACKAGE).to_string_lossy().to_string(),
      backup_enabled: false,
      before_deploy: before_deploy_commands,
      after_deploy: Vec::new(),
    };

    let provider: Arc<dyn ConfigProvider> = Arc::new(FixedProvider {
      config_path,
      key_paths: KeyPairPaths::new(private_key_path, public_key_path),
    });
    tokio::spawn(async move {
      let _ = server::start_server(provider).await;
    });
    sleep(Duration::from_millis(300)).await;

    Self {
      _temp: temp,
      port,
      deploy_root,
      paired_path,
      public_key,
      signing_key,
      archive,
      file_hash,
      // Long enough that nothing in these tests trips it by accident; the
      // deadline test sets its own.
      deploy_timeout_secs: 0,
      manifest,
    }
  }

  /// Send a pairing request signed with the harness key.
  async fn request_pairing(&self, name: &str) -> Result<(PairState, String), tonic::Status> {
    self
      .send_pairing(name, &self.public_key, &self.signing_key)
      .await
  }

  /// Send a pairing request presenting `public_key`, signed with `signer`.
  async fn send_pairing(
    &self,
    name: &str,
    public_key: &str,
    signer: &SigningKey,
  ) -> Result<(PairState, String), tonic::Status> {
    use ed25519_dalek::Signer;

    let nonce = uuid::Uuid::new_v4().to_string();
    let timestamp_ms = now_ms();
    let payload = pair_signing_payload(public_key, name, &nonce, timestamp_ms);

    let response = self
      .client()
      .await
      .pair(tonic::Request::new(PairRequest {
        public_key: public_key.to_string(),
        client_name: name.to_string(),
        nonce,
        timestamp_ms,
        signature: general_purpose::STANDARD.encode(signer.sign(&payload).to_bytes()),
      }))
      .await?
      .into_inner();

    Ok((
      PairState::try_from(response.state).unwrap_or(PairState::Unspecified),
      response.fingerprint,
    ))
  }

  /// Approve everything waiting, the way an operator would.
  fn approve_all_pending(&self) {
    let mut store = PairStore::load(&self.paired_path).expect("load store");
    while !store.pending.is_empty() {
      store.approve("1").expect("approve");
    }
    store.save(&self.paired_path).expect("save store");
  }

  async fn client(&self) -> DeployServiceClient<Channel> {
    DeployServiceClient::connect(format!("http://127.0.0.1:{}", self.port))
      .await
      .expect("connect to test server")
  }

  /// A correctly signed opening message.
  fn start_message(&self) -> DeployStart {
    self.signed_start(PACKAGE, self.archive.len() as u64, now_ms(), None)
  }

  /// A correctly signed message describing a body other than the harness one.
  fn start_message_for(&self, body: &[u8]) -> DeployStart {
    use ed25519_dalek::Signer;
    use sha2::{Digest, Sha256};

    let file_hash = format!("{:x}", Sha256::digest(body));
    let total_size = body.len() as u64;
    let nonce = uuid::Uuid::new_v4().to_string();
    let timestamp_ms = now_ms();
    let payload = deploy_start_signing_payload(
      PACKAGE,
      total_size,
      &file_hash,
      &self.public_key,
      &nonce,
      timestamp_ms,
      self.deploy_timeout_secs,
      Some(&self.manifest),
    );

    DeployStart {
      package_name: PACKAGE.to_string(),
      total_size,
      file_hash,
      public_key: self.public_key.clone(),
      nonce,
      timestamp_ms,
      deploy_timeout_secs: self.deploy_timeout_secs,
      manifest: Some(self.manifest.clone()),
      signature: general_purpose::STANDARD.encode(self.signing_key.sign(&payload).to_bytes()),
    }
  }

  /// An opening message signed over the given values.
  ///
  /// `claimed_package` replaces the package name *after* signing, which is how
  /// a captured request would be retargeted at another package.
  fn signed_start(
    &self,
    package: &str,
    total_size: u64,
    timestamp_ms: i64,
    claimed_package: Option<&str>,
  ) -> DeployStart {
    use ed25519_dalek::Signer;

    let nonce = uuid::Uuid::new_v4().to_string();
    let payload = deploy_start_signing_payload(
      package,
      total_size,
      &self.file_hash,
      &self.public_key,
      &nonce,
      timestamp_ms,
      self.deploy_timeout_secs,
      Some(&self.manifest),
    );
    let signature = self.signing_key.sign(&payload);

    DeployStart {
      package_name: claimed_package.unwrap_or(package).to_string(),
      total_size,
      file_hash: self.file_hash.clone(),
      public_key: self.public_key.clone(),
      nonce,
      timestamp_ms,
      deploy_timeout_secs: self.deploy_timeout_secs,
      manifest: Some(self.manifest.clone()),
      signature: general_purpose::STANDARD.encode(signature.to_bytes()),
    }
  }

  /// Send an opening message followed by `body`, and collect the outcome.
  async fn send(&self, start: DeployStart, body: Vec<u8>) -> Result<bool, tonic::Status> {
    let (success, _) = self.send_reporting(start, body).await?;
    Ok(success)
  }

  /// Send, returning the final result flag and its message.
  async fn send_reporting(
    &self,
    start: DeployStart,
    body: Vec<u8>,
  ) -> Result<(bool, String), tonic::Status> {
    let mut client = self.client().await;
    let mut chunks = vec![DeployChunk {
      payload: Some(Payload::Start(start)),
    }];
    for slice in body.chunks(64 * 1024) {
      chunks.push(DeployChunk {
        payload: Some(Payload::Data(slice.to_vec())),
      });
    }

    let mut events = client
      .deploy(tonic::Request::new(tokio_stream::iter(chunks)))
      .await?
      .into_inner();

    let mut success = false;
    let mut message = String::new();
    while let Some(event) = events.message().await? {
      if let Some(Event::Result(result)) = event.event {
        success = result.success;
        message = result.message;
      }
    }
    Ok((success, message))
  }
}

#[tokio::test]
async fn a_well_formed_deployment_succeeds() {
  let harness = Harness::start().await;

  let success = harness
    .send(harness.start_message(), harness.archive.clone())
    .await
    .expect("deployment should be accepted");

  assert!(success, "a correctly signed deployment must succeed");
  assert!(
    harness
      .deploy_root
      .join(PACKAGE)
      .join("payload.txt")
      .exists(),
    "the package must be unpacked under deploy_root"
  );
}

#[tokio::test]
async fn a_replayed_request_is_refused() {
  let harness = Harness::start().await;
  let start = harness.start_message();

  assert!(harness
    .send(start.clone(), harness.archive.clone())
    .await
    .expect("first use should be accepted"));

  // Byte-for-byte the same request, which is exactly what a captured one is.
  let status = harness
    .send(start, harness.archive.clone())
    .await
    .expect_err("a reused nonce must be refused");

  assert_eq!(status.code(), tonic::Code::Unauthenticated);
  assert!(
    status.message().contains("nonce"),
    "the refusal should name the nonce, got: {}",
    status.message()
  );
}

#[tokio::test]
async fn a_request_cannot_be_retargeted_at_another_package() {
  let harness = Harness::start().await;

  // Signed for one package, presented as another: this is what an unsigned
  // package_name allowed, letting one package's archive land in another's
  // deploy path and run its hooks.
  let start = harness.signed_start(
    "some-other-package",
    harness.archive.len() as u64,
    now_ms(),
    Some(PACKAGE),
  );

  let status = harness
    .send(start, harness.archive.clone())
    .await
    .expect_err("a retargeted request must be refused");

  assert_eq!(status.code(), tonic::Code::Unauthenticated);
  assert!(
    status.message().contains("signature"),
    "the refusal should blame the signature, got: {}",
    status.message()
  );
}

#[tokio::test]
async fn a_stale_request_is_refused() {
  let harness = Harness::start().await;
  let long_ago = now_ms() - 3_600_000;

  let start = harness.signed_start(PACKAGE, harness.archive.len() as u64, long_ago, None);
  let status = harness
    .send(start, harness.archive.clone())
    .await
    .expect_err("a stale request must be refused");

  assert_eq!(status.code(), tonic::Code::Unauthenticated);
  assert!(
    status.message().contains("window"),
    "the refusal should mention the acceptance window, got: {}",
    status.message()
  );
}

#[tokio::test]
async fn a_stream_longer_than_it_declared_is_cut_off() {
  let harness = Harness::start().await;

  // The declared size is a claim, so the server counts what actually arrives.
  let start = harness.start_message();
  let mut oversized = harness.archive.clone();
  oversized.extend_from_slice(&vec![0u8; 4096]);

  let status = harness
    .send(start, oversized)
    .await
    .expect_err("a stream exceeding its declared size must be cut off");

  assert_eq!(status.code(), tonic::Code::InvalidArgument);
  assert!(
    status.message().contains("declared size"),
    "the refusal should mention the declared size, got: {}",
    status.message()
  );
}

#[tokio::test]
async fn a_truncated_stream_is_refused() {
  let harness = Harness::start().await;

  let start = harness.start_message();
  let truncated = harness.archive[..harness.archive.len() / 2].to_vec();

  let status = harness
    .send(start, truncated)
    .await
    .expect_err("a stream shorter than declared must be refused");

  assert_eq!(status.code(), tonic::Code::InvalidArgument);
  assert!(
    status.message().contains("declared bytes"),
    "the refusal should say how much arrived, got: {}",
    status.message()
  );
}

#[tokio::test]
async fn an_unknown_key_never_gets_to_send_an_archive() {
  let harness = Harness::start().await;

  // A key the server has never heard of, signing correctly for itself.
  let temp = tempfile::tempdir().expect("temp dir");
  let private = temp.path().join("id_ed25519");
  let public = temp.path().join("id_ed25519.pub");
  Auth::generate_key_pair(&public.to_string_lossy(), &private.to_string_lossy()).expect("key pair");
  let stranger_public = std::fs::read_to_string(&public)
    .expect("read")
    .trim()
    .to_string();
  let stranger_key = Auth::load_key_pair(&private.to_string_lossy()).expect("load");

  use ed25519_dalek::Signer;
  let nonce = uuid::Uuid::new_v4().to_string();
  let timestamp_ms = now_ms();
  let total_size = harness.archive.len() as u64;
  let payload = deploy_start_signing_payload(
    PACKAGE,
    total_size,
    &harness.file_hash,
    &stranger_public,
    &nonce,
    timestamp_ms,
    0,
    Some(&harness.manifest),
  );

  let start = DeployStart {
    package_name: PACKAGE.to_string(),
    total_size,
    file_hash: harness.file_hash.clone(),
    public_key: stranger_public,
    nonce,
    timestamp_ms,
    deploy_timeout_secs: 0,
    manifest: Some(harness.manifest.clone()),
    signature: general_purpose::STANDARD.encode(stranger_key.sign(&payload).to_bytes()),
  };

  let status = harness
    .send(start, harness.archive.clone())
    .await
    .expect_err("an unknown key must be refused");

  assert_eq!(status.code(), tonic::Code::Unauthenticated);
  assert!(
    !harness.deploy_root.join(PACKAGE).exists(),
    "nothing should have been written for an unauthorised caller"
  );
}

#[tokio::test]
async fn pairing_queues_a_key_that_cannot_yet_deploy() {
  let harness = Harness::start_untrusted().await;

  let (state, server_fingerprint) = harness
    .request_pairing("dev-box")
    .await
    .expect("pairing request should be accepted");

  assert_eq!(state, PairState::Pending);
  assert_eq!(
    server_fingerprint,
    fingerprint(&harness.public_key),
    "both ends must derive the same fingerprint, or comparing them proves nothing"
  );

  // Queued is not trusted: the whole point is that a human decides.
  let status = harness
    .send(harness.start_message(), harness.archive.clone())
    .await
    .expect_err("a queued key must not be able to deploy");
  assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn an_approved_key_can_deploy_without_restarting_the_server() {
  let harness = Harness::start_untrusted().await;
  harness
    .request_pairing("dev-box")
    .await
    .expect("pairing request");

  harness.approve_all_pending();

  // The server was never restarted; it reads approvals per request.
  let success = harness
    .send(harness.start_message(), harness.archive.clone())
    .await
    .expect("an approved key should be able to deploy");
  assert!(success);
}

#[tokio::test]
async fn a_pairing_request_must_be_signed_by_the_key_it_presents() {
  let harness = Harness::start_untrusted().await;

  // Someone else's public key, signed with our own: this is how a stranger
  // would fill an operator's queue with keys they do not control.
  let temp = tempfile::tempdir().expect("temp dir");
  let other_public = temp.path().join("other.pub");
  let other_private = temp.path().join("other");
  Auth::generate_key_pair(
    &other_public.to_string_lossy(),
    &other_private.to_string_lossy(),
  )
  .expect("key pair");
  let stranger_key = std::fs::read_to_string(&other_public)
    .expect("read")
    .trim()
    .to_string();

  let status = harness
    .send_pairing("impostor", &stranger_key, &harness.signing_key)
    .await
    .expect_err("a request not signed by its own key must be refused");

  assert_eq!(status.code(), tonic::Code::Unauthenticated);
  assert!(
    PairStore::load(&harness.paired_path)
      .expect("load store")
      .pending
      .is_empty(),
    "nothing should have been queued"
  );
}

#[tokio::test]
async fn repeating_a_pairing_request_does_not_queue_it_twice() {
  let harness = Harness::start_untrusted().await;

  for _ in 0..3 {
    let (state, _) = harness
      .request_pairing("dev-box")
      .await
      .expect("pairing request");
    assert_eq!(state, PairState::Pending);
  }

  let store = PairStore::load(&harness.paired_path).expect("load store");
  assert_eq!(
    store.pending.len(),
    1,
    "a client polling while it waits must not fill the queue"
  );
}

#[tokio::test]
async fn pairing_reports_a_key_the_allowlist_already_names() {
  let harness = Harness::start().await;

  let (state, _) = harness
    .request_pairing("dev-box")
    .await
    .expect("pairing request");

  assert_eq!(
    state,
    PairState::Approved,
    "a key already in allowed_keys should be told so rather than queued"
  );
  assert!(PairStore::load(&harness.paired_path)
    .expect("load store")
    .pending
    .is_empty());
}

#[tokio::test]
async fn a_corrupt_archive_leaves_the_live_deployment_intact() {
  let harness = Harness::start().await;
  let deployed = harness.deploy_root.join(PACKAGE).join("payload.txt");

  assert!(harness
    .send(harness.start_message(), harness.archive.clone())
    .await
    .expect("first deployment"));
  assert_eq!(std::fs::read_to_string(&deployed).expect("read"), "payload");

  // Bytes that hash to exactly what they claim, and are still not a gzip
  // stream: verification passes and extraction is what fails, part way in.
  let corrupt = vec![0x42u8; 8192];
  let success = harness
    .send(harness.start_message_for(&corrupt), corrupt)
    .await
    .expect("the stream itself completes");

  assert!(!success, "a corrupt archive must not report success");
  assert_eq!(
    std::fs::read_to_string(&deployed).expect("read"),
    "payload",
    "the previous deployment must survive a failed one"
  );

  let leftovers: Vec<String> = std::fs::read_dir(&harness.deploy_root)
    .expect("read deploy root")
    .filter_map(|entry| entry.ok())
    .map(|entry| entry.file_name().to_string_lossy().to_string())
    .filter(|name| name.starts_with(&format!("{PACKAGE}.")))
    .collect();
  assert!(
    leftovers.is_empty(),
    "no working directories should be left behind, found: {leftovers:?}"
  );
}

#[tokio::test]
async fn a_deployment_that_outlasts_its_deadline_is_stopped() {
  // tonic applies `grpc-timeout` to the future that produces the response, and
  // for a streaming method that resolves as soon as the handler hands back the
  // stream - before any of the work it opened. The deadline went unenforced for
  // exactly that reason once already, silently, which is why this exists.
  let harness = Harness::start_slow().await;

  let (success, message) = harness
    .send_reporting(harness.start_message(), harness.archive.clone())
    .await
    .expect("the stream itself completes");

  assert!(
    !success,
    "a deployment past its deadline must not report success"
  );
  assert!(
    message.contains("Deadline"),
    "the server should say why it stopped, got: {message}"
  );

  // The hook runs for 30 seconds. If the deadline were not enforced, the
  // deployment would carry on and eventually unpack.
  sleep(Duration::from_secs(2)).await;
  assert!(
    !harness.deploy_root.join(PACKAGE).exists(),
    "nothing should have been unpacked after the deadline passed"
  );
}

#[tokio::test]
async fn a_slow_transfer_is_not_cut_off_for_taking_long() {
  // Nothing bounds the upload, and this is what says so: a transfer that takes
  // far longer than the deployment budget must still be allowed to finish,
  // which would not hold if anyone reintroduced a limit that counted it.
  let mut harness = Harness::start().await;
  harness.deploy_timeout_secs = 1;
  let start = harness.start_message();
  let body = harness.archive.clone();

  let outbound = async_stream::stream! {
    yield DeployChunk { payload: Some(Payload::Start(start)) };
    for piece in body.chunks(32) {
      // Slow, but never silent for longer than the bound allows.
      sleep(Duration::from_millis(50)).await;
      yield DeployChunk { payload: Some(Payload::Data(piece.to_vec())) };
    }
  };

  let mut events = harness
    .client()
    .await
    .deploy(tonic::Request::new(outbound))
    .await
    .expect("accepted")
    .into_inner();

  let mut success = false;
  while let Some(event) = events.message().await.expect("no transport error") {
    if let Some(Event::Result(result)) = event.event {
      success = result.success;
    }
  }

  assert!(
    success,
    "a transfer outlasting the deployment budget must still be allowed to finish"
  );
}

#[tokio::test]
async fn a_second_deployment_to_the_same_directory_is_refused() {
  // Two of them would each assemble a tree and then swap in whatever order
  // they finished, with the loser snapshotting and carrying over from the
  // winner's half-installed state.
  let mut harness = Harness::start_slow().await;
  // Unbounded, so the first deployment is still inside its hook when the
  // second arrives rather than having already been cut off by its deadline.
  harness.deploy_timeout_secs = 0;
  let harness = Arc::new(harness);

  let first = tokio::spawn({
    let harness = harness.clone();
    async move {
      let start = harness.start_message();
      harness.send(start, harness.archive.clone()).await
    }
  });

  // Long enough for the opening message to be accepted even on a slow runner;
  // the hook it then runs lasts far longer than the rest of this test, so
  // there is no race at the other end.
  sleep(Duration::from_secs(2)).await;

  let start = harness.start_message();
  let refusal = harness
    .send(start, harness.archive.clone())
    .await
    .expect_err("a second deployment to the same directory must be refused");

  assert_eq!(
    refusal.code(),
    tonic::Code::Aborted,
    "a directory already being replaced is a concurrency conflict, got: {refusal}"
  );
  assert!(
    refusal.message().contains("Another deployment"),
    "the refusal should say what is in the way, got: {refusal}"
  );

  // Dropping the first stream releases the claim, which the next deployment
  // needs; without it this would be the only deployment the server ever took.
  first.abort();
  let _ = first.await;
  sleep(Duration::from_millis(300)).await;

  // Deliberately one byte short of what it declares. That fails inside the
  // upload, before the hook this harness makes slow, so the status it comes
  // back with answers whether the directory was free without waiting thirty
  // seconds for a hook nobody is testing.
  let start = harness.start_message();
  let mut truncated = harness.archive.clone();
  truncated.pop();
  let status = harness
    .send(start, truncated)
    .await
    .expect_err("an upload short of its declared size fails either way");

  assert_eq!(
    status.code(),
    tonic::Code::InvalidArgument,
    "the directory must be free once the deployment holding it ends, got: {status}"
  );
}
