//! Enumerates server-side integration test scenarios and helpers.

use std::{
  fs,
  path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServerScenarioKind {
  /// Server with backup + hooks executes successfully.
  StandardSuccess,
  /// Backup is disabled but hooks execute.
  BackupDisabled,
  /// Before-deploy script exits with a non-zero status.
  PreDeployScriptFailure,
  /// After-deploy script exits with a non-zero status.
  PostDeployScriptFailure,
  /// Package name is not present in server configuration.
  MissingPackage,
  /// Client public key is not on the allow list.
  UnauthorizedKey,
}

#[derive(Clone, Copy, Debug)]
pub struct ServerScenario {
  pub kind: ServerScenarioKind,
  pub name: &'static str,
  #[allow(dead_code)]
  pub description: &'static str,
}

const SERVER_SCENARIOS: &[ServerScenario] = &[
  ServerScenario {
    kind: ServerScenarioKind::StandardSuccess,
    name: "server_standard_success",
    description: "Backup enabled with both hooks succeeding",
  },
  ServerScenario {
    kind: ServerScenarioKind::BackupDisabled,
    name: "server_backup_disabled",
    description: "Backup disabled while hooks succeed",
  },
  ServerScenario {
    kind: ServerScenarioKind::PreDeployScriptFailure,
    name: "server_pre_deploy_script_failure",
    description: "Before hook fails and aborts the deployment",
  },
  ServerScenario {
    kind: ServerScenarioKind::PostDeployScriptFailure,
    name: "server_post_deploy_script_failure",
    description: "After hook fails but deployment is kept",
  },
  ServerScenario {
    kind: ServerScenarioKind::MissingPackage,
    name: "server_missing_package",
    description: "Requested package is not configured on the server",
  },
  ServerScenario {
    kind: ServerScenarioKind::UnauthorizedKey,
    name: "server_unauthorized_key",
    description: "Client public key is not allowed",
  },
];

/// All available server scenarios.
pub const fn all() -> &'static [ServerScenario] {
  SERVER_SCENARIOS
}

/// Look up a scenario by kind.
pub fn get(kind: ServerScenarioKind) -> &'static ServerScenario {
  SERVER_SCENARIOS
    .iter()
    .find(|scenario| scenario.kind == kind)
    .expect("Missing server scenario definition")
}

/// Create a server configuration tailored to the provided scenario.
pub fn write_server_config(
  scenario: ServerScenarioKind,
  server_dir: &Path,
  port: u16,
  public_key: &str,
  package_name: &str,
) -> PathBuf {
  use ServerScenarioKind::*;

  let deploy_path = server_dir.join("deploy");
  fs::create_dir_all(&deploy_path).expect("Failed to create deploy directory");

  let seed_file = deploy_path.join("backup.txt");
  fs::write(&seed_file, "backup content").expect("Failed to write backup seed file");

  let backup_path = server_dir.join("backup");
  fs::create_dir_all(&backup_path).expect("Failed to create backup directory");

  let scripts_dir = server_dir.join("scripts");
  fs::create_dir_all(&scripts_dir).expect("Failed to create scripts directory");

  let pre_marker = deploy_path.join("pre_deploy_executed.marker");
  let post_marker = deploy_path.join("post_deploy_executed.marker");

  let pre_script_path = scripts_dir.join("pre_deploy.sh");
  let pre_script_content = match scenario {
    PreDeployScriptFailure => r"#!/bin/sh
echo 'pre hook failed' >&2
exit 1
"
    .to_string(),
    _ => format!(
      r"#!/bin/sh
touch '{}'
",
      pre_marker.display()
    ),
  };
  fs::write(&pre_script_path, pre_script_content).expect("Failed to write Before-deploy script");

  let post_script_path = scripts_dir.join("post_deploy.sh");
  let post_script_content = match scenario {
    PostDeployScriptFailure => r"#!/bin/sh
echo 'post hook failed' >&2
exit 1
"
    .to_string(),
    _ => format!(
      r"#!/bin/sh
touch '{}'
",
      post_marker.display()
    ),
  };
  fs::write(&post_script_path, post_script_content).expect("Failed to write After-deploy script");

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    for script in [&pre_script_path, &post_script_path] {
      let mut perms = fs::metadata(script).unwrap().permissions();
      perms.set_mode(0o755);
      fs::set_permissions(script, perms).expect("Failed to set script permissions");
    }
  }

  let configured_package_name = match scenario {
    MissingPackage => "other-app",
    _ => package_name,
  };

  let allowed_key_entry = if matches!(scenario, UnauthorizedKey) {
    "invalid-test-key".to_string()
  } else {
    public_key.to_string()
  };

  let backup_enabled = !matches!(scenario, BackupDisabled);

  let config_content = format!(
    r#"[server]
port = {port}
max_file_size = 1048576
allowed_keys = [
  "{allowed_key}"
]

[packages.{package}]
deploy_path = "{deploy_path}"
backup_enabled = {backup_enabled}
backup_path = "{backup_path}"
before_deploy_script = "{pre_script}"
after_deploy_script = "{post_script}"
"#,
    port = port,
    allowed_key = allowed_key_entry,
    package = configured_package_name,
    deploy_path = deploy_path.display(),
    backup_enabled = backup_enabled,
    backup_path = backup_path.display(),
    pre_script = pre_script_path.display(),
    post_script = post_script_path.display(),
  );

  let config_path = server_dir.join("server_config.toml");
  fs::write(&config_path, config_content).expect("Failed to write server config file");

  config_path
}
