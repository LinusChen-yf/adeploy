//! End-to-end integration tests

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use adeploy::{client, server};
use log2::*;
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

mod common;

#[tokio::test]
async fn test_comprehensive_deployment_flow() {
  let _log2 = log2::start();

  // Run the comprehensive deployment test
  if let Err(e) = run_comprehensive_deployment_test().await {
    panic!("Test failed with error: {}", e);
  }
}

/// Test setup structure to hold all test resources
struct TestSetup {
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
  std::fs::create_dir_all(&server_dir).unwrap();
  let client_dir = temp_dir.path().join("client");
  std::fs::create_dir_all(&client_dir).unwrap();

  let private_key_path = server_dir.as_path().join("test_key");
  let public_key_path = server_dir.as_path().join("test_key.pub");

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
  // Verify test1.txt exists and has correct content
  let test1_path = deploy_path.join("test1.txt");
  assert!(
    test1_path.exists(),
    "test1.txt should exist in deploy directory"
  );
  let test1_content = std::fs::read_to_string(&test1_path).expect("Failed to read test1.txt");
  assert_eq!(
    test1_content, "test1 content",
    "test1.txt should have correct content"
  );

  // Verify test2.txt exists and has correct content
  let test2_path = deploy_path.join("test2.txt");
  assert!(
    test2_path.exists(),
    "test2.txt should exist in deploy directory"
  );
  let test2_content = std::fs::read_to_string(&test2_path).expect("Failed to read test2.txt");
  assert_eq!(
    test2_content, "test2 content",
    "test2.txt should have correct content"
  );
}

/// Verify script execution by checking marker files
fn verify_script_execution(deploy_path: &Path) {
  // Verify pre-deploy script was executed by checking marker file
  let pre_marker_path = deploy_path.join("pre_deploy_executed.marker");
  assert!(
    pre_marker_path.exists(),
    "Pre-deploy script marker file should exist, indicating script was executed"
  );
  info!("✅ Pre-deploy script execution verified");

  // Verify post-deploy script was executed by checking marker file
  let post_marker_path = deploy_path.join("post_deploy_executed.marker");
  assert!(
    post_marker_path.exists(),
    "Post-deploy script marker file should exist, indicating script was executed"
  );
  info!("✅ Post-deploy script execution verified");
}

/// Verify backup feature
fn verify_backup_feature(backup_path: &Path) {
  // Find any backup directory that starts with "backup_"
  let backup_dir = std::fs::read_dir(&backup_path)
    .unwrap()
    .filter_map(|entry| entry.ok())
    .find(|entry| {
      entry.file_name().to_string_lossy().starts_with("backup_")
        && entry.file_type().unwrap().is_dir()
    })
    .expect("Should find a backup directory starting with 'backup_'");

  let backup_file_path = backup_dir.path().join("backup.txt");
  assert!(
    backup_file_path.exists(),
    "backup.txt should exist in backup directory"
  );
}

/// Run comprehensive deployment test with all features enabled
async fn run_comprehensive_deployment_test() -> Result<(), Box<dyn std::error::Error>> {
  // Setup test environment
  let test_setup = setup_test().await;
  let package_name = test_setup.package_name;
  let port = test_setup.port;
  let server_dir = test_setup.server_dir;
  let client_dir = test_setup.client_dir;
  let public_key_path = test_setup.public_key_path;
  let private_key_path = test_setup.private_key_path;

  // Generate a key pair for all tests
  generate_test_keys(&public_key_path, &private_key_path);

  // Create server configuration with backup enabled and scripts
  info!("Setting up server configuration with backup and scripts");
  let public_key = std::fs::read_to_string(&public_key_path)?;
  let server_config_path =
    create_test_server_config_file(&server_dir, port, &public_key, &package_name);

  // Load server configuration
  let server_config = adeploy::config::load_server_config(&server_config_path)?;

  // Start server
  info!("Starting deployment server on port {}", port);
  let server_handle = tokio::spawn(async move { server::start_server(port, server_config).await });

  // Give server time to start
  sleep(Duration::from_millis(200)).await;

  // Create client configuration and test files
  let client_config_path =
    create_test_client_config(&client_dir, port, &public_key_path.to_string_lossy());

  // Load client configuration
  let client_config = adeploy::config::load_client_config(&client_config_path)?;

  // Comprehensive deployment with all features
  info!("Comprehensive deployment with backup and scripts");
  let deploy_result = timeout(
    Duration::from_secs(15),
    client::deploy("127.0.0.1", client_config, &package_name),
  )
  .await;

  match deploy_result {
    Ok(Ok(_)) => {
      info!("✅ Deployment completed successfully");

      // Verify deployment results
      info!("Verifying deployment results...");

      // Check if files were deployed correctly
      let deploy_path = server_dir.join("deploy");
      let backup_path = server_dir.join("backup");
      verify_deployed_files(&deploy_path);
      verify_script_execution(&deploy_path);
      verify_backup_feature(&backup_path);
    }
    Ok(Err(deploy_error)) => {
      error!("❌ Deployment failed: {:?}", deploy_error);
      return Err(Box::new(deploy_error));
    }
    Err(_) => {
      error!("❌ Deployment timed out - server communication issue");
      return Err("Server should respond within timeout period".into());
    }
  }

  server_handle.abort();
  info!("🎉 All comprehensive integration tests completed successfully!");
  Ok(())
}

/// Helper function to create server configuration file for testing
fn create_test_server_config_file(
  server_path: &PathBuf,
  port: u16,
  public_key: &str,
  package_name: &str,
) -> PathBuf {
  let deploy_path = server_path.join("deploy");
  let deploy_path_str = deploy_path.to_string_lossy().to_string();
  std::fs::create_dir_all(&deploy_path).expect("Failed to create deploy directory");
  let backup_file = deploy_path.join("backup.txt");
  std::fs::write(&backup_file, "backup content").expect("Failed to write backup file");

  // Create backup directory
  let backup_path = server_path.join("backup").to_string_lossy().to_string();
  std::fs::create_dir_all(&backup_path).expect("Failed to create backup directory");

  // Create simple test scripts
  let scripts_dir = server_path.join("scripts");
  std::fs::create_dir_all(&scripts_dir).expect("Failed to create scripts directory");

  // Create test scripts that touch marker files for verification
  let create_script = |script_name: &str, marker_name: &str| {
    let script_path = scripts_dir.join(script_name);
    let marker_file = format!("{}/{}", deploy_path_str, marker_name);
    let script_content = format!("#!/bin/sh\ntouch '{}'\n", marker_file);
    std::fs::write(&script_path, script_content)
      .expect(&format!("Failed to write {}", script_name));
    script_path
  };

  let pre_script_path = create_script("pre_deploy.sh", "pre_deploy_executed.marker");
  let post_script_path = create_script("post_deploy.sh", "post_deploy_executed.marker");

  // Make scripts executable
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&pre_script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&pre_script_path, perms)
      .expect("Failed to set pre-deploy script permissions");

    let mut perms = std::fs::metadata(&post_script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&post_script_path, perms)
      .expect("Failed to set post-deploy script permissions");
  }

  let config_content = format!(
    r#"
[server]
port = {}
max_file_size = 1048576  # 1MB in bytes
allowed_keys = [
    "{}"
]

# Package configurations - key is the package name
[packages.{}]
deploy_path = "{}"
backup_enabled = true
backup_path = "{}"
before_deploy_script = "{}"
after_deploy_script = "{}"
"#,
    port,
    public_key,
    package_name,
    deploy_path_str,
    backup_path,
    pre_script_path.to_string_lossy(),
    post_script_path.to_string_lossy()
  );

  let config_path = server_path.join("server_config.toml");
  std::fs::write(&config_path, config_content).expect("Failed to write server config file");

  config_path
}

/// Helper function to create client configuration for testing
fn create_test_client_config(client_dir: &PathBuf, port: u16, key_path: &str) -> PathBuf {
  let test1 = client_dir.join("test1.txt");
  std::fs::write(&test1, "test1 content").expect("Failed to write test1 file");
  let test2 = client_dir.join("test2.txt");
  std::fs::write(&test2, "test2 content").expect("Failed to write test2 file");

  let config_content = format!(
    r#"
# Test client configuration in TOML format

# Package configurations - key is the package name
[packages.test-app]
sources = ["{}", "{}"]

# Server configurations - key is the IP address
[servers."127.0.0.1"]
port = {}
timeout = 30
key_path = "{}"

# Default server configuration
[servers.default]
port = {}
timeout = 30
key_path = "{}"
"#,
    test1.to_string_lossy(),
    test2.to_string_lossy(),
    port,
    key_path,
    port,
    key_path
  );

  let config_path = client_dir.join("client_config.toml");
  std::fs::write(&config_path, config_content).expect("Failed to write config file");

  config_path
}
