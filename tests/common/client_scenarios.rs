//! Enumerates client-side integration test scenarios and helpers.

use std::{
  fs,
  path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientScenarioKind {
  /// Everything configured correctly.
  HappyPath,
  /// Remote host configuration is missing.
  MissingRemoteConfig,
  /// Package references a missing source file.
  MissingSourceFile,
  /// Remote uses a custom key path that does not exist.
  InvalidCustomKeyPath,
  /// Deployment is requested for a package not declared in the config.
  UnknownPackageName,
}

#[derive(Clone, Copy, Debug)]
pub struct ClientScenario {
  pub kind: ClientScenarioKind,
  pub name: &'static str,
  #[allow(dead_code)]
  pub description: &'static str,
}

impl ClientScenario {
  /// Package to deploy for this scenario.
  pub const fn package_name(&self) -> &'static str {
    match self.kind {
      ClientScenarioKind::UnknownPackageName => "missing-app",
      _ => "test-app",
    }
  }
}

const CLIENT_SCENARIOS: &[ClientScenario] = &[
  ClientScenario {
    kind: ClientScenarioKind::HappyPath,
    name: "client_happy_path",
    description: "Valid client configuration with generated key pair",
  },
  ClientScenario {
    kind: ClientScenarioKind::MissingRemoteConfig,
    name: "client_missing_remote_config",
    description: "No host-specific or default remote entry",
  },
  ClientScenario {
    kind: ClientScenarioKind::MissingSourceFile,
    name: "client_missing_source_file",
    description: "One of the declared source files is absent",
  },
  ClientScenario {
    kind: ClientScenarioKind::InvalidCustomKeyPath,
    name: "client_invalid_custom_key_path",
    description: "Custom key path points to a non-existent .pub file",
  },
  ClientScenario {
    kind: ClientScenarioKind::UnknownPackageName,
    name: "client_unknown_package_name",
    description: "Deployment is requested for a package not declared in config",
  },
];

/// All available client scenarios.
pub const fn all() -> &'static [ClientScenario] {
  CLIENT_SCENARIOS
}

/// Look up a scenario by kind.
pub fn get(kind: ClientScenarioKind) -> &'static ClientScenario {
  CLIENT_SCENARIOS
    .iter()
    .find(|scenario| scenario.kind == kind)
    .expect("Missing client scenario definition")
}

/// Create a client configuration tailored to the provided scenario.
pub fn write_client_config(
  scenario: ClientScenarioKind,
  client_dir: &Path,
  port: u16,
  public_key_path: &Path,
) -> PathBuf {
  let test1_path = client_dir.join("test1.txt");
  let test2_path = client_dir.join("test2.txt");

  fs::write(&test1_path, "test1 content").expect("Failed to write test1 file");
  fs::write(&test2_path, "test2 content").expect("Failed to write test2 file");

  let missing_key_path = client_dir.join("missing_custom_key.pub");

  let host_remote_block = match scenario {
    ClientScenarioKind::MissingRemoteConfig => String::new(),
    ClientScenarioKind::InvalidCustomKeyPath => format!(
      r#"[remotes."127.0.0.1"]
port = {port}
timeout = 30
key_path = "{missing_key}"

"#,
      port = port,
      missing_key = missing_key_path.display()
    ),
    _ => format!(
      r#"[remotes."127.0.0.1"]
port = {port}
timeout = 30
key_path = "{key}"

"#,
      port = port,
      key = public_key_path.display()
    ),
  };

  let default_remote_block = match scenario {
    ClientScenarioKind::MissingRemoteConfig => String::new(),
    ClientScenarioKind::InvalidCustomKeyPath => format!(
      r#"[remotes.default]
port = {port}
timeout = 30
key_path = "{missing_key}"

"#,
      port = port,
      missing_key = missing_key_path.display()
    ),
    _ => format!(
      r#"[remotes.default]
port = {port}
timeout = 30
key_path = "{key}"

"#,
      port = port,
      key = public_key_path.display()
    ),
  };

  let fallback_remote_block = match scenario {
    ClientScenarioKind::MissingRemoteConfig => format!(
      r#"[remotes."198.51.100.1"]
port = {port}
timeout = 30

"#,
      port = port
    ),
    _ => String::new(),
  };

  let config_content = format!(
    r#"[packages.test-app]
sources = [
  "{test1}",
  "{test2}",
]

{host_remote}{default_remote}{fallback_remote}"#,
    test1 = test1_path.display(),
    test2 = test2_path.display(),
    host_remote = host_remote_block,
    default_remote = default_remote_block,
    fallback_remote = fallback_remote_block,
  );

  let config_path = client_dir.join("client_config.toml");
  fs::write(&config_path, config_content).expect("Failed to write client config file");

  if matches!(scenario, ClientScenarioKind::MissingSourceFile) {
    fs::remove_file(&test2_path).expect("Failed to remove test2.txt for missing source scenario");
  }

  config_path
}
