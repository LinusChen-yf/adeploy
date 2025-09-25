//! End-to-end integration tests covering multiple client/server scenarios.

use std::{
  fs,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use adeploy::{
  client,
  config::{
    ClientConfig, ConfigProvider, ConfigProviderImpl, ConfigType, KeyPairPaths, ServerConfig,
  },
  error::Result as AdeployResult,
  server,
};
use log2::*;
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

#[derive(Clone)]
struct ConfigProviderMock {
  client_config_path: PathBuf,
  server_config_path: PathBuf,
  key_paths: KeyPairPaths,
}

impl ConfigProviderMock {
  fn new(
    client_config_path: PathBuf,
    server_config_path: PathBuf,
    key_paths: KeyPairPaths,
  ) -> Self {
    Self {
      client_config_path,
      server_config_path,
      key_paths,
    }
  }
}

impl ConfigProvider for ConfigProviderMock {
  fn get_config_path(&self, config_type: ConfigType) -> AdeployResult<PathBuf> {
    match config_type {
      ConfigType::Client => Ok(self.client_config_path.clone()),
      ConfigType::Server => Ok(self.server_config_path.clone()),
    }
  }

  fn load_client_config(&self, path: &Path) -> AdeployResult<ClientConfig> {
    ConfigProviderImpl.load_client_config(path)
  }

  fn load_server_config(&self, path: &Path) -> AdeployResult<ServerConfig> {
    ConfigProviderImpl.load_server_config(path)
  }

  fn get_key_paths(&self) -> AdeployResult<KeyPairPaths> {
    Ok(self.key_paths.clone())
  }
}

#[path = "common/client_scenarios.rs"]
mod client_scenarios;
mod common;
#[path = "common/server_scenarios.rs"]
mod server_scenarios;

use client_scenarios::ClientScenarioKind;
use server_scenarios::ServerScenarioKind;

const DEPLOY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug)]
struct SuccessExpectation {
  expect_pre_marker: bool,
  expect_post_marker: bool,
  expect_backup_snapshot: bool,
}

impl SuccessExpectation {
  const fn new(pre: bool, post: bool, backup: bool) -> Self {
    Self {
      expect_pre_marker: pre,
      expect_post_marker: post,
      expect_backup_snapshot: backup,
    }
  }
}

#[derive(Clone, Copy, Debug)]
enum CombinedOutcome {
  Success(SuccessExpectation),
  ClientError(&'static str),
  ServerError(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct ScenarioCase {
  client_kind: ClientScenarioKind,
  server_kind: ServerScenarioKind,
  expected: CombinedOutcome,
}

#[tokio::test]
async fn test_deployment_scenario_matrix() {
  let _log2 = log2::start();

  for case in build_matrix() {
    let client = client_scenarios::get(case.client_kind);
    let server = server_scenarios::get(case.server_kind);

    info!(
      "🚀 Running integration case: client={}, server={}",
      client.name, server.name
    );

    if let Err(err) = run_case(&case).await {
      panic!(
        "Case failed (client={}, server={}): {}",
        client.name, server.name, err
      );
    }
  }
}

fn build_matrix() -> Vec<ScenarioCase> {
  let mut cases = Vec::new();
  for client in client_scenarios::all() {
    for server in server_scenarios::all() {
      if let Some(expected) = resolve_expected_outcome(client.kind, server.kind) {
        cases.push(ScenarioCase {
          client_kind: client.kind,
          server_kind: server.kind,
          expected,
        });
      }
    }
  }
  cases
}

fn resolve_expected_outcome(
  client_kind: ClientScenarioKind,
  server_kind: ServerScenarioKind,
) -> Option<CombinedOutcome> {
  use ClientScenarioKind::*;
  use ServerScenarioKind::*;

  match (client_kind, server_kind) {
    (HappyPath, StandardSuccess) => Some(CombinedOutcome::Success(SuccessExpectation::new(
      true, true, true,
    ))),
    (HappyPath, BackupDisabled) => Some(CombinedOutcome::Success(SuccessExpectation::new(
      true, true, false,
    ))),
    (HappyPath, PostDeployScriptFailure) => Some(CombinedOutcome::Success(
      SuccessExpectation::new(true, false, true),
    )),
    (HappyPath, PreDeployScriptFailure) => Some(CombinedOutcome::ServerError(
      "execution failed with exit code: 1",
    )),
    (HappyPath, MissingPackage) => Some(CombinedOutcome::ServerError(
      "Package 'test-app' not configured",
    )),
    (HappyPath, UnauthorizedKey) => Some(CombinedOutcome::ServerError(
      "Client public key not allowed",
    )),
    (MissingRemoteConfig, StandardSuccess) => Some(CombinedOutcome::ClientError(
      "No server configuration found for host",
    )),
    (MissingSourceFile, StandardSuccess) => Some(CombinedOutcome::ClientError("Source path")),
    (MissingKeyMaterial, StandardSuccess) => {
      Some(CombinedOutcome::ClientError("Failed to load SSH key pair"))
    }
    (UnknownPackageName, StandardSuccess) => {
      Some(CombinedOutcome::ClientError("No packages found to deploy"))
    }
    _ => None,
  }
}

async fn run_case(case: &ScenarioCase) -> Result<(), String> {
  let client_scenario = client_scenarios::get(case.client_kind);
  let server_scenario = server_scenarios::get(case.server_kind);

  let test_setup = setup_test().await;
  let package_name = client_scenario.package_name();
  let port = test_setup.port;
  let deploy_path = test_setup.server_dir.join("deploy");
  let backup_path = test_setup.server_dir.join("backup");

  generate_test_keys(&test_setup.public_key_path, &test_setup.private_key_path);

  let public_key = fs::read_to_string(&test_setup.public_key_path)
    .map_err(|e| format!("Failed to read public key: {}", e))?
    .trim()
    .to_string();

  let server_config_path = server_scenarios::write_server_config(
    server_scenario.kind,
    &test_setup.server_dir,
    port,
    &public_key,
    package_name,
  );

  let client_config_path =
    client_scenarios::write_client_config(case.client_kind, &test_setup.client_dir, port);

  let config_provider = build_config_provider(
    case.client_kind,
    &test_setup,
    client_config_path,
    server_config_path.clone(),
  );

  let requires_server_for_client_error = matches!(
    case.client_kind,
    ClientScenarioKind::MissingSourceFile
      | ClientScenarioKind::MissingKeyMaterial
      | ClientScenarioKind::UnknownPackageName
  );

  let should_start_server = matches!(
    case.expected,
    CombinedOutcome::Success(_) | CombinedOutcome::ServerError(_)
  ) || requires_server_for_client_error;

  let mut server_handle = None;
  if should_start_server {
    let server_provider = config_provider.clone();
    server_handle = Some(tokio::spawn(async move {
      let _ = server::start_server(server_provider).await;
    }));

    sleep(Duration::from_millis(200)).await;
  }

  let deploy_future = client::deploy(
    "127.0.0.1",
    Some(vec![package_name.to_string()]),
    config_provider.as_ref(),
  );
  let deploy_result = timeout(DEPLOY_TIMEOUT, deploy_future)
    .await
    .map_err(|_| "Deployment timed out".to_string())?;

  let result = match (&case.expected, deploy_result) {
    (CombinedOutcome::Success(expectation), Ok(())) => {
      verify_deployed_files(&deploy_path);
      assert_marker_state(
        &deploy_path,
        "pre_deploy_executed.marker",
        expectation.expect_pre_marker,
      );
      assert_marker_state(
        &deploy_path,
        "post_deploy_executed.marker",
        expectation.expect_post_marker,
      );
      assert_backup_state(&backup_path, expectation.expect_backup_snapshot);
      Ok(())
    }
    (CombinedOutcome::ClientError(message), Err(err)) if err.to_string().contains(message) => {
      Ok(())
    }
    (CombinedOutcome::ServerError(message), Err(err)) if err.to_string().contains(message) => {
      Ok(())
    }
    (CombinedOutcome::ClientError(message), Err(err)) => Err(format!(
      "Expected client error containing '{}' but got '{}'",
      message, err
    )),
    (CombinedOutcome::ServerError(message), Err(err)) => Err(format!(
      "Expected server error containing '{}' but got '{}'",
      message, err
    )),
    (CombinedOutcome::Success(_), Err(err)) => {
      Err(format!("Expected success but deployment failed: {}", err))
    }
    (_, Ok(())) => Err("Expected deployment to fail but it succeeded".to_string()),
  };

  if let Some(handle) = server_handle {
    handle.abort();
    let _ = handle.await;
  }

  result
}

fn build_config_provider(
  client_kind: ClientScenarioKind,
  test_setup: &TestSetup,
  client_config_path: PathBuf,
  server_config_path: PathBuf,
) -> Arc<dyn ConfigProvider> {
  let key_paths = match client_kind {
    ClientScenarioKind::MissingKeyMaterial => KeyPairPaths::new(
      test_setup.client_dir.join("missing_key"),
      test_setup.client_dir.join("missing_key.pub"),
    ),
    _ => KeyPairPaths::new(
      test_setup.private_key_path.clone(),
      test_setup.public_key_path.clone(),
    ),
  };

  Arc::new(ConfigProviderMock::new(
    client_config_path,
    server_config_path,
    key_paths,
  ))
}

/// Test setup structure to hold all test resources
struct TestSetup {
  #[allow(dead_code)]
  package_name: String,
  port: u16,
  #[allow(dead_code)]
  temp_dir: TempDir,
  server_dir: PathBuf,
  client_dir: PathBuf,
  public_key_path: PathBuf,
  private_key_path: PathBuf,
}

/// Initialize test environment
async fn setup_test() -> TestSetup {
  let package_name = "test-app";
  let port = common::find_available_port().await;
  let temp_dir = common::create_temp_dir();

  let server_dir = temp_dir.path().join("server");
  fs::create_dir_all(&server_dir).unwrap();
  let client_dir = temp_dir.path().join("client");
  fs::create_dir_all(&client_dir).unwrap();

  let private_key_path = server_dir.join("test_key");
  let public_key_path = server_dir.join("test_key.pub");

  TestSetup {
    package_name: package_name.to_string(),
    port,
    temp_dir,
    server_dir,
    client_dir,
    public_key_path,
    private_key_path,
  }
}

/// Generate key pair for testing
fn generate_test_keys(public_key_path: &Path, private_key_path: &Path) {
  let key_result = adeploy::auth::Auth::generate_key_pair(
    &public_key_path.to_string_lossy(),
    &private_key_path.to_string_lossy(),
  );
  assert!(key_result.is_ok(), "Failed to generate key pair");
}

/// Verify deployed files exist with correct content
fn verify_deployed_files(deploy_path: &Path) {
  let test1_path = deploy_path.join("test1.txt");
  assert!(
    test1_path.exists(),
    "test1.txt should exist in deploy directory"
  );
  let test1_content = fs::read_to_string(&test1_path).expect("Failed to read test1.txt");
  assert_eq!(
    test1_content, "test1 content",
    "test1.txt should have correct content"
  );

  let test2_path = deploy_path.join("test2.txt");
  assert!(
    test2_path.exists(),
    "test2.txt should exist in deploy directory"
  );
  let test2_content = fs::read_to_string(&test2_path).expect("Failed to read test2.txt");
  assert_eq!(
    test2_content, "test2 content",
    "test2.txt should have correct content"
  );
}

fn assert_marker_state(deploy_path: &Path, marker_name: &str, should_exist: bool) {
  let marker_path = deploy_path.join(marker_name);
  if should_exist {
    assert!(
      marker_path.exists(),
      "{} should exist in the deploy directory",
      marker_name
    );
  } else {
    assert!(
      !marker_path.exists(),
      "{} should not exist in the deploy directory",
      marker_name
    );
  }
}

fn assert_backup_state(backup_path: &Path, expect_backup: bool) {
  let snapshot_exists = fs::read_dir(backup_path)
    .map(|entries| {
      entries
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().starts_with("backup_"))
    })
    .unwrap_or(false);

  if expect_backup {
    assert!(
      snapshot_exists,
      "Expected a backup directory starting with 'backup_'"
    );
  } else {
    assert!(
      !snapshot_exists,
      "Did not expect a backup directory to be created"
    );
  }
}
