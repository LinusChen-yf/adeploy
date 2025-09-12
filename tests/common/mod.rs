//! Common test utilities and helpers

use std::{collections::HashMap, path::PathBuf};

use adeploy::{
  adeploy::{
    DeployRequest, DeployResponse,
  },
  config::{DeployPackageConfig, ServerDeployConfig, ServerSettings},
};
use tempfile::TempDir;
use tokio::net::TcpListener;

/// Test configuration for server
pub fn create_test_server_config() -> ServerDeployConfig {
  let mut packages = HashMap::new();
  packages.insert(
    "test-app".to_string(),
    DeployPackageConfig {
      deploy_path: "/tmp/test-deploy".to_string(),
      pre_deploy_script: None,
      post_deploy_script: None,
      backup_enabled: false,
      backup_path: None,
      owner: None,
      permissions: None,
    },
  );

  ServerDeployConfig {
    packages,
    server: ServerSettings {
      port: 0,                    // Will be set dynamically
      max_file_size: 1024 * 1024, // 1MB
      allowed_ssh_keys: vec!["ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQC7test...".to_string()],
    },
  }
}

/// Create a temporary directory for testing
pub fn create_temp_dir() -> TempDir {
  tempfile::tempdir().expect("Failed to create temp directory")
}

/// Find an available port for testing
pub async fn find_available_port() -> u16 {
  let listener = TcpListener::bind("127.0.0.1:0")
    .await
    .expect("Failed to bind to port");
  let addr = listener.local_addr().expect("Failed to get local address");
  addr.port()
}

/// Create test deploy request
pub fn create_test_deploy_request() -> DeployRequest {
  DeployRequest {
    package_name: "test-app".to_string(),
    version: "1.0.0".to_string(),
    file_data: b"test file content".to_vec(),
    ssh_signature: "test_signature".to_string(),
    client_public_key: "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQC7test...".to_string(),
    metadata: HashMap::new(),
  }
}

/// Create test files in a directory
pub fn create_test_files(dir: &std::path::Path) -> std::io::Result<()> {
  std::fs::create_dir_all(dir)?;
  std::fs::write(dir.join("test.txt"), "Hello, World!")?;
  std::fs::write(dir.join("config.json"), r#"{"name": "test"}"#)?;

  let subdir = dir.join("subdir");
  std::fs::create_dir_all(&subdir)?;
  std::fs::write(subdir.join("nested.txt"), "Nested file")?;

  Ok(())
}

/// Mock SSH signature verification (always returns true for testing)
pub fn mock_ssh_verify(_public_key: &str, _data: &[u8], _signature: &[u8]) -> bool {
  true
}
