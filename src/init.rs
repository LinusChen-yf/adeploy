use std::{
  env, fs,
  path::{Path, PathBuf},
};

use log2::*;

use crate::{
  config::{executable_dir, Defaults, PROJECT_CONFIG_NAME},
  error::{AdeployError, Result},
};

/// Commented starting point written by `adeploy init`.
///
/// Every field the client and the server understand appears here, either set
/// to its default or commented out, so the file doubles as the reference.
const TEMPLATE: &str = r#"# adeploy configuration.
#
# Commit this file with the project it deploys. The client searches upward from
# the working directory to find it, so `adeploy <host> <package>` works from
# anywhere inside the checkout. Relative paths below resolve against the
# directory holding this file, never against the working directory.

[defaults]
# gRPC port. The server listens on it, the client dials it.
port = 6060
# Seconds allowed to establish the connection. 0 disables the limit.
connect_timeout = 5
# Seconds allowed for the upload plus everything the server does afterwards:
# unpacking, backups and both hook scripts. Installers and service restarts are
# slow, so keep this far larger than connect_timeout. 0 disables the limit.
deploy_timeout = 600
# Largest archive accepted, in bytes. Enforced on both ends. Default 100 MiB.
max_file_size = 104857600

# A package describes both halves of a deployment: what the client archives and
# what the server does with it. Rename "demo" to suit, and add more tables for
# more packages.
[packages.demo]
# Client: files and directories to archive, in order. No glob expansion.
sources = ["./dist/demo"]
# Server: where to unpack. A relative path lands under the server's deploy_root.
deploy_path = "demo"
# Server: snapshot the existing directory before unpacking.
backup_enabled = true
# Server: where snapshots go. Defaults to a directory named after the package,
# beside the server binary.
# backup_path = "/var/backups/demo"
# Server: runs before unpacking. A non-zero exit aborts the deployment, so this
# is the place to stop a service that holds the files open.
# before_deploy_script = "systemctl stop demo"
# Server: runs after unpacking. Failure is logged as a warning but the
# deployment still counts as successful.
# after_deploy_script = "systemctl start demo"

# Per-host overrides. Only list what differs from [defaults]; everything else is
# inherited. The key is the host you pass on the command line. A "default" entry
# applies to any host without its own table.
# [remotes."192.0.2.10"]
# port = 6070
# deploy_timeout = 1800

# Server-local settings. A project checkout leaves this out entirely; it belongs
# to the copy of this file sitting beside the server binary.
# [server]
# Base64 Ed25519 public keys allowed to deploy here. The client prints its own
# key when the server rejects it.
# allowed_keys = []
# Root for package deploy_path values that are relative.
# deploy_root = "/opt"
"#;

/// Configuration generated on the server's first run.
///
/// Only machine-local policy is filled in. `allowed_keys` starts empty on
/// purpose: the key it will hold does not exist until a client runs for the
/// first time, so asking an operator to write this file by hand always meant
/// writing it twice.
const SERVER_TEMPLATE: &str = r#"# adeploy server configuration, generated on first run.
#
# This file belongs to this machine, not to any project. It is reloaded
# automatically when it changes, so edits take effect without a restart.

[defaults]
# Port this server listens on. Changing it requires a restart.
port = {port}
# Largest archive accepted, in bytes. Default 100 MiB.
max_file_size = {max_file_size}

[server]
# Base64 Ed25519 public keys allowed to deploy here. A client that is not
# listed is rejected and prints its own key, ready to be pasted in below.
allowed_keys = []
# Base directory that relative deploy_path values land under. A package cannot
# escape it unless its deploy_path is written as an absolute path.
deploy_root = {deploy_root}

# One table per package this server accepts. The name must match what the
# client deploys. Remove the comment markers and adjust to taste.
# [packages.demo]
# deploy_path = "demo"
# backup_enabled = true
# before_deploy_script = "systemctl stop demo"
# after_deploy_script = "systemctl start demo"
"#;

/// Create the server configuration if it is not there yet.
///
/// Returns `true` when a file was generated, so the caller can tell a first
/// run from a restart.
pub fn ensure_server_config(path: &Path) -> Result<bool> {
  if path.exists() {
    return Ok(false);
  }

  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to create {}: {}",
        parent.display(),
        e
      )))
    })?;
  }

  let defaults = Defaults::default();
  let content = SERVER_TEMPLATE
    .replace("{port}", &defaults.port.to_string())
    .replace("{max_file_size}", &defaults.max_file_size.to_string())
    .replace("{deploy_root}", &toml_string(&default_deploy_root()?));

  fs::write(path, content).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to write {}: {}",
      path.display(),
      e
    )))
  })?;

  info!("Generated {}", path.display());
  Ok(true)
}

/// Where deployments land unless `deploy_root` says otherwise.
///
/// A `deploy` directory beside the binary, matching where the server already
/// keeps `logs/` and `.key/`. It needs no elevated privileges and behaves the
/// same on every platform.
pub fn default_deploy_root() -> Result<PathBuf> {
  Ok(executable_dir()?.join("deploy"))
}

/// Render a path as a TOML string literal, escaping Windows separators.
fn toml_string(path: &Path) -> String {
  toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

/// Write a starter `adeploy.toml` into the working directory.
pub fn init_project_config(force: bool) -> Result<()> {
  let target = working_directory()?.join(PROJECT_CONFIG_NAME);

  if target.exists() && !force {
    return Err(Box::new(AdeployError::Config(format!(
      "{} already exists. Pass --force to overwrite it.",
      target.display()
    ))));
  }

  fs::write(&target, TEMPLATE).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to write {}: {}",
      target.display(),
      e
    )))
  })?;

  info!("Created {}", target.display());
  info!("Edit the [packages.demo] table, then deploy with `adeploy <host> demo`");
  Ok(())
}

fn working_directory() -> Result<PathBuf> {
  env::current_dir().map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to determine the working directory: {}",
      e
    )))
  })
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::{ensure_server_config, TEMPLATE};
  use crate::config::ProjectConfig;

  #[test]
  fn generated_server_config_parses_and_is_ready_to_use() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("adeploy.toml");

    assert!(
      ensure_server_config(&path).expect("generation should succeed"),
      "a missing configuration must be created"
    );

    let text = std::fs::read_to_string(&path).expect("generated file should be readable");
    let config: ProjectConfig = toml::from_str(&text).expect("generated config must parse");

    assert_eq!(config.defaults.port, 6060);
    // Empty on purpose: the key does not exist until a client first runs.
    assert!(config.server.allowed_keys.is_empty());
    // A concrete root is written so deployments do not depend on the implicit
    // fallback, and so an operator can see where files will land.
    assert!(
      config.server.deploy_root.is_some(),
      "generated config must pin deploy_root"
    );
  }

  #[test]
  fn generating_the_server_config_is_idempotent() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("adeploy.toml");

    assert!(ensure_server_config(&path).expect("first call creates"));
    std::fs::write(&path, "[defaults]\nport = 7000\n").expect("operator edit");

    assert!(
      !ensure_server_config(&path).expect("second call is a no-op"),
      "an existing configuration must not be reported as generated"
    );

    let text = std::fs::read_to_string(&path).expect("read back");
    assert!(
      text.contains("port = 7000"),
      "an existing configuration must never be overwritten, got: {text}"
    );
  }

  #[test]
  fn server_config_is_created_with_its_parent_directory() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("nested").join("dir").join("adeploy.toml");

    assert!(ensure_server_config(&path).expect("generation should succeed"));
    assert!(path.exists(), "missing parent directories must be created");
  }

  #[test]
  fn template_parses_and_carries_the_documented_defaults() {
    let config: ProjectConfig = toml::from_str(TEMPLATE).expect("template must be valid TOML");

    assert_eq!(config.defaults.port, 6060);
    assert_eq!(config.defaults.connect_timeout, 5);
    assert_eq!(config.defaults.deploy_timeout, 600);

    let demo = config
      .packages
      .get("demo")
      .expect("template must define the demo package");
    assert_eq!(demo.sources, vec!["./dist/demo".to_string()]);
    assert_eq!(demo.deploy_path.as_deref(), Some("demo"));
    assert!(demo.backup_enabled);
  }

  #[test]
  fn template_defaults_match_the_struct_defaults() {
    // The commented template and the serde defaults must not drift apart.
    let from_template: ProjectConfig = toml::from_str(TEMPLATE).expect("valid TOML");
    let from_empty: ProjectConfig = toml::from_str("").expect("empty config is valid");

    assert_eq!(from_template.defaults.port, from_empty.defaults.port);
    assert_eq!(
      from_template.defaults.connect_timeout,
      from_empty.defaults.connect_timeout
    );
    assert_eq!(
      from_template.defaults.deploy_timeout,
      from_empty.defaults.deploy_timeout
    );
    assert_eq!(
      from_template.defaults.max_file_size,
      from_empty.defaults.max_file_size
    );
  }
}
