//! Enumerates server-side integration test scenarios and helpers.
//!
//! A server holds no package configuration, so what these scenarios set up is
//! the machine: its allow list, the directories a deployment will touch, and
//! the hook scripts it will run. Which of those the deployment actually uses is
//! decided by the manifest the client sends, built in `client_scenarios`.

use std::{
  fs,
  path::{Path, PathBuf},
};

/// Everything a scenario laid out on the server, for the client to point at.
pub struct ServerFixture {
  pub config_path: PathBuf,
  pub deploy_path: PathBuf,
  pub pre_script: PathBuf,
  pub post_script: PathBuf,
  pub backup_enabled: bool,
}

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
    kind: ServerScenarioKind::UnauthorizedKey,
    name: "server_unauthorized_key",
    description: "Client public key is not allowed",
  },
];

/// Where the server under test will keep snapshots of `test-app`.
pub fn snapshot_directory() -> PathBuf {
  std::env::current_exe()
    .expect("current exe")
    .parent()
    .expect("exe dir")
    .join("test-app")
}

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

/// Lay out the server side of a scenario.
pub fn prepare_server(
  scenario: ServerScenarioKind,
  server_dir: &Path,
  port: u16,
  public_key: &str,
) -> ServerFixture {
  use ServerScenarioKind::*;

  let deploy_path = server_dir.join("deploy");
  fs::create_dir_all(&deploy_path).expect("Failed to create deploy directory");

  let seed_file = deploy_path.join("backup.txt");
  fs::write(&seed_file, "backup content").expect("Failed to write backup seed file");

  // Snapshots go beside the server binary now, which under test is whatever is
  // running these cases. Cleared so a previous run cannot be mistaken for this
  // one's snapshot.
  let _ = fs::remove_dir_all(snapshot_directory());

  let scripts_dir = server_dir.join("scripts");
  fs::create_dir_all(&scripts_dir).expect("Failed to create scripts directory");

  let pre_marker = deploy_path.join("pre_deploy_executed.marker");
  let post_marker = deploy_path.join("post_deploy_executed.marker");

  let windows = cfg!(target_os = "windows");

  let pre_script_path = scripts_dir.join(if windows {
    "pre_deploy.cmd"
  } else {
    "pre_deploy.sh"
  });

  let pre_script_content = if windows {
    match scenario {
      PreDeployScriptFailure => {
        "@echo off\r\necho pre hook failed 1>&2\r\nexit /B 1\r\n".to_string()
      }
      _ => format!("@echo off\r\ntype nul > \"{}\"\r\n", pre_marker.display()),
    }
  } else {
    match scenario {
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
    }
  };
  fs::write(&pre_script_path, pre_script_content).expect("Failed to write Before-deploy script");

  let post_script_path = scripts_dir.join(if windows {
    "post_deploy.cmd"
  } else {
    "post_deploy.sh"
  });

  let post_script_content = if windows {
    match scenario {
      PostDeployScriptFailure => {
        "@echo off\r\necho post hook failed 1>&2\r\nexit /B 1\r\n".to_string()
      }
      _ => format!("@echo off\r\ntype nul > \"{}\"\r\n", post_marker.display()),
    }
  } else {
    match scenario {
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
    }
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

  let allowed_key_entry = if matches!(scenario, UnauthorizedKey) {
    "invalid-test-key".to_string()
  } else {
    public_key.to_string()
  };

  let backup_enabled = !matches!(scenario, BackupDisabled);

  let config_content = format!(
    r#"[server]
listen_port = {port}
allowed_keys = [
  "{allowed_key}"
]
"#,
    port = port,
    allowed_key = allowed_key_entry,
  );

  let config_path = server_dir.join("adeploy.toml");
  fs::write(&config_path, config_content).expect("Failed to write server config file");

  ServerFixture {
    config_path,
    deploy_path,
    pre_script: pre_script_path,
    post_script: post_script_path,
    backup_enabled,
  }
}
