//! The transport: which server the client believes it reached, and what
//! happens when that stops being true.
//!
//! The scenario matrix and the protocol cases both run without TLS, because
//! what they are about is the deployment and the messages. This file is the
//! other half - it runs the way a real install does, with the server's
//! generated identity and the client's record of it deciding who may talk.

use std::{
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use adeploy::{
  auth::Auth,
  client,
  config::{ConfigProvider, ConfigProviderImpl, KeyPairPaths, ProjectConfig},
  error::Result as AdeployResult,
  identity::{fingerprint_of_pem, CERTIFICATE_FILE_NAME},
  known_servers::KnownServers,
  pairing::PairStore,
  server,
};
use tempfile::TempDir;

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
  server_dir: PathBuf,
  paired_path: PathBuf,
  provider: Arc<FixedProvider>,
  known_servers_path: PathBuf,
  deploy_path: PathBuf,
}

impl Harness {
  /// A server with a generated identity, and a client that knows nothing yet.
  /// A server that already lists this client in `allowed_keys`.
  async fn start() -> Self {
    Self::start_with(true).await
  }

  /// A server that has never heard of this client, so pairing has to queue.
  async fn start_unknown() -> Self {
    Self::start_with(false).await
  }

  async fn start_with(pre_trusted: bool) -> Self {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();

    let client_keys = root.join("client-keys");
    std::fs::create_dir_all(&client_keys).expect("key dir");
    let private_key = client_keys.join("id_ed25519");
    let public_key = client_keys.join("id_ed25519.pub");
    Auth::generate_key_pair(
      &public_key.to_string_lossy(),
      &private_key.to_string_lossy(),
    )
    .expect("key pair");
    let public_key_text = std::fs::read_to_string(&public_key)
      .expect("read public key")
      .trim()
      .to_string();

    let source_dir = root.join("sources");
    std::fs::create_dir_all(&source_dir).expect("source dir");
    std::fs::write(source_dir.join("app.txt"), "v1").expect("source file");

    let deploy_path = root.join("live").join(PACKAGE);
    let port = common::find_available_port().await;

    // No `tls` key anywhere: this is what a real install gets by default.
    let server_dir = root.join("server");
    std::fs::create_dir_all(&server_dir).expect("server dir");
    let server_config = server_dir.join("adeploy.toml");
    std::fs::write(
      &server_config,
      format!(
        "[server]\nlisten_port = {port}\nallowed_keys = [{keys}]\n",
        keys = if pre_trusted {
          format!("\"{public_key_text}\"")
        } else {
          String::new()
        }
      ),
    )
    .expect("write server config");

    let client_config = root.join("adeploy.toml");
    std::fs::write(
      &client_config,
      format!(
        "[defaults]\nport = {port}\nconnect_timeout = 5\ndeploy_timeout = 30\n\n\
         [packages.{PACKAGE}]\nsources = [\"{sources}\"]\ndeploy_path = \"{deploy}\"\n",
        sources = source_dir.to_string_lossy().replace('\\', "\\\\"),
        deploy = deploy_path.to_string_lossy().replace('\\', "\\\\"),
      ),
    )
    .expect("write client config");

    let provider_for_server: Arc<dyn ConfigProvider> = Arc::new(FixedProvider {
      config_path: server_config,
      key_paths: KeyPairPaths::new(private_key.clone(), public_key.clone()),
    });
    tokio::spawn(async move {
      let _ = server::start_server(provider_for_server).await;
    });
    common::wait_until_listening(port).await;

    let key_paths = KeyPairPaths::new(private_key, public_key);
    let known_servers_path = key_paths.known_servers();

    Self {
      _temp: temp,
      paired_path: server_dir.join("paired.toml"),
      server_dir,
      provider: Arc::new(FixedProvider {
        config_path: client_config,
        key_paths,
      }),
      known_servers_path,
      deploy_path,
    }
  }

  /// What the server generated for itself, read the way an operator reads it
  /// off the startup log.
  fn server_fingerprint(&self) -> String {
    let pem = std::fs::read_to_string(self.server_dir.join(CERTIFICATE_FILE_NAME))
      .expect("the server must have generated a certificate");
    fingerprint_of_pem(&pem).expect("fingerprint")
  }

  fn known_servers(&self) -> KnownServers {
    KnownServers::load(&self.known_servers_path).expect("load known servers")
  }

  fn approve_all_pending(&self) {
    let mut store = PairStore::load(&self.paired_path).expect("load store");
    while !store.pending.is_empty() {
      store.approve("1").expect("approve");
    }
    store.save(&self.paired_path).expect("save store");
  }

  fn pending_count(&self) -> usize {
    PairStore::load(&self.paired_path)
      .expect("load store")
      .pending
      .len()
  }

  fn reject_all_pending(&self) {
    let mut store = PairStore::load(&self.paired_path).expect("load store");
    while !store.pending.is_empty() {
      store.reject("1").expect("reject");
    }
    store.save(&self.paired_path).expect("save store");
  }

  /// Pair and return as soon as the request is queued.
  ///
  /// These tests approve out of band afterwards, so waiting here would be
  /// waiting on something this task is the one that has to go and do.
  async fn pair(&self, force: bool) -> AdeployResult<()> {
    client::pair("127.0.0.1", force, false, self.provider.as_ref()).await
  }

  /// Pair and hold until somebody approves, which is what an operator gets.
  async fn pair_and_wait(&self, force: bool) -> AdeployResult<()> {
    client::pair("127.0.0.1", force, true, self.provider.as_ref()).await
  }

  async fn deploy(&self) -> AdeployResult<()> {
    client::deploy(
      "127.0.0.1",
      Some(vec![PACKAGE.to_string()]),
      self.provider.as_ref(),
    )
    .await
  }

  /// Put someone else's certificate on file for this host, which is what a
  /// rebuilt server - or another machine on that address - would look like.
  fn record_a_different_certificate(&self) {
    let other = rcgen::generate_simple_self_signed(vec!["adeploy".to_string()]).expect("cert");
    let pem = other.cert.pem();
    let fingerprint = fingerprint_of_pem(&pem).expect("fingerprint");

    let mut known = self.known_servers();
    known.record("127.0.0.1", &pem, &fingerprint, true);
    known.save(&self.known_servers_path).expect("save");
  }
}

#[tokio::test]
async fn a_server_generates_an_identity_and_a_client_records_it() {
  let harness = Harness::start().await;

  assert!(
    harness.known_servers().get("127.0.0.1").is_none(),
    "nothing may be trusted before anyone has paired"
  );

  harness.pair(false).await.expect("pairing should succeed");

  let recorded = harness.known_servers();
  let server = recorded
    .get("127.0.0.1")
    .expect("pairing must record who answered");
  assert_eq!(
    server.fingerprint,
    harness.server_fingerprint(),
    "the recorded identity must be the one the server generated - this is the \
     comparison an operator makes by eye"
  );
}

#[tokio::test]
async fn a_deployment_over_tls_arrives_intact() {
  let harness = Harness::start().await;
  harness.pair(false).await.expect("pair");
  harness.approve_all_pending();

  harness.deploy().await.expect("deployment should succeed");

  assert_eq!(
    std::fs::read_to_string(harness.deploy_path.join("app.txt")).expect("read deployed file"),
    "v1"
  );
}

#[tokio::test]
async fn a_server_this_machine_has_not_paired_with_is_refused() {
  let harness = Harness::start().await;

  let failure = harness
    .deploy()
    .await
    .expect_err("an unknown server must not be trusted on sight");

  let message = failure.to_string();
  assert!(
    message.contains("has not paired"),
    "the refusal should say what is missing, got: {message}"
  );
  assert!(
    message.contains("adeploy pair"),
    "and what to do about it, got: {message}"
  );
}

#[tokio::test]
async fn a_server_whose_identity_changed_is_refused() {
  let harness = Harness::start().await;
  harness.pair(false).await.expect("pair");
  harness.approve_all_pending();
  harness.deploy().await.expect("deploying once must work");

  // The recorded certificate no longer matches what the server serves.
  harness.record_a_different_certificate();

  let failure = harness
    .deploy()
    .await
    .expect_err("a server that is not the one on file must be refused");
  let message = failure.to_string();
  assert!(
    message.contains("invalid peer certificate"),
    "the refusal must be about the certificate, not any connection problem that \
     would also print the hint below, got: {message}"
  );
  assert!(
    message.contains("adeploy pair 127.0.0.1 --force"),
    "and should say how to accept a server that really was rebuilt, got: {message}"
  );
}

#[tokio::test]
async fn re_pairing_needs_force_once_an_identity_is_on_file() {
  let harness = Harness::start().await;
  harness.pair(false).await.expect("pair");
  let genuine = harness.server_fingerprint();

  harness.record_a_different_certificate();
  let stale = harness
    .known_servers()
    .get("127.0.0.1")
    .expect("recorded")
    .fingerprint
    .clone();
  assert_ne!(stale, genuine);

  let refusal = harness
    .pair(false)
    .await
    .expect_err("pairing again must not quietly overwrite a recorded identity");
  assert!(refusal.to_string().contains("--force"), "got: {refusal}");
  assert_eq!(
    harness
      .known_servers()
      .get("127.0.0.1")
      .expect("recorded")
      .fingerprint,
    stale,
    "a refused pairing must not have written anything"
  );

  harness.pair(true).await.expect("--force should re-record");
  assert_eq!(
    harness
      .known_servers()
      .get("127.0.0.1")
      .expect("recorded")
      .fingerprint,
    genuine,
    "and the server's real identity is what ends up on file"
  );
}

#[tokio::test]
async fn pairing_twice_with_the_same_server_is_not_a_change() {
  let harness = Harness::start().await;

  harness.pair(false).await.expect("first pair");
  harness
    .pair(false)
    .await
    .expect("pairing again with the same server must not be refused");

  assert_eq!(
    harness.known_servers().servers.len(),
    1,
    "and must not add a second entry"
  );
}

#[tokio::test]
async fn pairing_holds_until_someone_approves() {
  // What an operator actually does: start the pair, walk to the other machine,
  // approve, and have the first command notice. Before this the client printed
  // the approval instructions and exited, leaving them to run it all again.
  let harness = Harness::start_unknown().await;

  let waiting = async {
    let outcome = harness.pair_and_wait(false).await;
    (outcome, Instant::now())
  };
  let approving = async {
    // Long enough that the request is queued and the wait is really underway.
    tokio::time::sleep(Duration::from_secs(1)).await;
    harness.approve_all_pending();
    Instant::now()
  };

  let started = Instant::now();
  let ((outcome, returned_at), approved_at) = tokio::join!(waiting, approving);
  outcome.expect("an approved request must return successfully");

  // The ordering is the whole claim. Returning early and happening to be
  // approved afterwards would satisfy every other assertion here.
  assert!(
    returned_at > approved_at,
    "pairing returned {:?} before the approval, so it is not waiting on it",
    approved_at.duration_since(returned_at)
  );
  assert!(
    started.elapsed() < Duration::from_secs(30),
    "it waited {:?}, so it is not noticing the approval either",
    started.elapsed()
  );

  // And the wait left something usable behind, not just a happy exit code.
  harness.deploy().await.expect("deploying after the wait");
}

#[tokio::test]
async fn pairing_gives_up_when_the_server_refuses() {
  // The other way a wait can end. Rejection is a decision, not a failure to
  // reach anyone, so it has to break the loop rather than be polled forever.
  let harness = Harness::start_unknown().await;

  let waiting = harness.pair_and_wait(false);
  let rejecting = async {
    tokio::time::sleep(Duration::from_secs(1)).await;
    harness.reject_all_pending();
  };

  let (outcome, ()) = tokio::join!(waiting, rejecting);
  let failure = outcome.expect_err("a refused key must not be reported as paired");
  assert!(
    failure.to_string().contains("refused"),
    "the error should say it was refused, got: {failure}"
  );

  // A refusal answers one request. Asking again has to reach the queue, or
  // rejecting would be a ban - and a client that polls while it waits would
  // be refused for ever by a decision it already acted on.
  harness
    .pair(false)
    .await
    .expect("asking again after a refusal must be allowed");
  assert_eq!(
    harness.pending_count(),
    1,
    "and must arrive as a fresh request in the queue"
  );
}
