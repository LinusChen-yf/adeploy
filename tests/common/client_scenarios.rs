//! Enumerates client-side integration test scenarios and helpers.

use std::{
  fs,
  path::{Path, PathBuf},
};

use crate::{common::toml_escape_path, server_scenarios::ServerFixture};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientScenarioKind {
  /// Everything configured correctly.
  HappyPath,
  /// Host has no `[remotes]` entry and inherits `[defaults]`.
  RemoteDefaultsFallback,
  /// Package references a missing source file.
  MissingSourceFile,
  /// Client fails because signing keys are unavailable.
  MissingKeyMaterial,
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
    kind: ClientScenarioKind::RemoteDefaultsFallback,
    name: "client_remote_defaults_fallback",
    description: "No host-specific remote entry, so [defaults] applies",
  },
  ClientScenario {
    kind: ClientScenarioKind::MissingSourceFile,
    name: "client_missing_source_file",
    description: "One of the declared source files is absent",
  },
  ClientScenario {
    kind: ClientScenarioKind::MissingKeyMaterial,
    name: "client_missing_key_material",
    description: "Client cannot locate the required signing keys",
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

/// Create the project's `adeploy.toml` for a scenario.
///
/// It describes the deployment end to end now - what to archive and what the
/// server should do with it - because the server holds nothing of its own. The
/// paths and scripts come from what the server scenario laid out.
pub fn write_client_config(
  scenario: ClientScenarioKind,
  client_dir: &Path,
  port: u16,
  fixture: &ServerFixture,
) -> PathBuf {
  let test1_path = client_dir.join("test1.txt");
  let test2_path = client_dir.join("test2.txt");

  fs::write(&test1_path, "test1 content").expect("Failed to write test1 file");
  fs::write(&test2_path, "test2 content").expect("Failed to write test2 file");

  // Omitting the table entirely is the point of the fallback scenario: the
  // deployment must still work off `[defaults]` alone.
  let remote_block = match scenario {
    ClientScenarioKind::RemoteDefaultsFallback => String::new(),
    _ => format!(
      r#"
[remotes."127.0.0.1"]
port = {port}
"#
    ),
  };

  let config_content = format!(
    r#"[defaults]
port = {port}
connect_timeout = 5
deploy_timeout = 30

[packages.test-app]
sources = [
  "{test1}",
  "{test2}",
]
deploy_path = "{deploy_path}"
backup_enabled = {backup_enabled}
before_deploy = ["{pre_script}"]
after_deploy = ["{post_script}"]
{remote_block}"#,
    port = port,
    test1 = toml_escape_path(&test1_path),
    test2 = toml_escape_path(&test2_path),
    deploy_path = toml_escape_path(&fixture.deploy_path),
    backup_enabled = fixture.backup_enabled,
    pre_script = toml_escape_path(&fixture.pre_script),
    post_script = toml_escape_path(&fixture.post_script),
    remote_block = remote_block,
  );

  let config_path = client_dir.join("adeploy.toml");
  fs::write(&config_path, config_content).expect("Failed to write client config file");

  if matches!(scenario, ClientScenarioKind::MissingSourceFile) {
    fs::remove_file(&test2_path).expect("Failed to remove test2.txt for missing source scenario");
  }

  config_path
}
