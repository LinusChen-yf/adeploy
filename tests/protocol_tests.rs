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
    DeployChunk, DeployStart,
  },
  auth::{deploy_start_signing_payload, Auth},
  config::{ConfigProvider, ConfigProviderImpl, KeyPairPaths, ProjectConfig},
  deploy::DeployManager,
  error::Result as AdeployResult,
  replay::now_ms,
  server,
};
use base64::{engine::general_purpose, Engine as _};
use ed25519_dalek::SigningKey;
use tempfile::TempDir;
use tokio::{net::TcpListener, time::sleep};
use tonic::transport::Channel;

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
  public_key: String,
  signing_key: SigningKey,
  archive: Vec<u8>,
  file_hash: String,
}

impl Harness {
  /// A running server that trusts one key, plus a real archive to send it.
  async fn start() -> Self {
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

    let (archive, file_hash) = DeployManager::new()
      .package_files(PACKAGE, &[source_dir])
      .await
      .expect("package sources");

    let port = free_port().await;
    let deploy_root = root.join("root");
    let config_path = root.join("adeploy.toml");
    std::fs::write(
      &config_path,
      format!(
        r#"[server]
listen_port = {port}
allowed_keys = ["{public_key}"]
deploy_root = "{deploy_root}"

[packages.{PACKAGE}]
deploy_path = "{PACKAGE}"
"#,
        deploy_root = deploy_root.to_string_lossy().replace('\\', "\\\\"),
      ),
    )
    .expect("write server config");

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
      public_key,
      signing_key,
      archive,
      file_hash,
    }
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
    );
    let signature = self.signing_key.sign(&payload);

    DeployStart {
      package_name: claimed_package.unwrap_or(package).to_string(),
      total_size,
      file_hash: self.file_hash.clone(),
      public_key: self.public_key.clone(),
      nonce,
      timestamp_ms,
      signature: general_purpose::STANDARD.encode(signature.to_bytes()),
    }
  }

  /// Send an opening message followed by `body`, and collect the outcome.
  async fn send(&self, start: DeployStart, body: Vec<u8>) -> Result<bool, tonic::Status> {
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
      .deploy(tokio_stream::iter(chunks))
      .await?
      .into_inner();

    let mut success = false;
    while let Some(event) = events.message().await? {
      if let Some(Event::Result(result)) = event.event {
        success = result.success;
      }
    }
    Ok(success)
  }
}

async fn free_port() -> u16 {
  TcpListener::bind("127.0.0.1:0")
    .await
    .expect("bind")
    .local_addr()
    .expect("addr")
    .port()
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
  );

  let start = DeployStart {
    package_name: PACKAGE.to_string(),
    total_size,
    file_hash: harness.file_hash.clone(),
    public_key: stranger_public,
    nonce,
    timestamp_ms,
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
