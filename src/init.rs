use std::{
  env, fs,
  path::{Path, PathBuf},
};

use log2::*;

use crate::{
  config::{ServerSettings, PROJECT_CONFIG_NAME},
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

# How this client reaches a server. The client is the one dialling, so the port
# and the timeouts belong here.
[defaults]
# Port to dial. Must match the server's [server].listen_port.
port = 6060
# Seconds allowed to establish the connection. 0 disables the limit.
connect_timeout = 5
# Seconds the server may spend deploying, measured from the moment the last byte
# arrives: the hooks, verification, the backup and the swap. The upload is not
# counted, so size this for what your hooks do. 0 disables the limit.
#
# The upload has no limit of its own and needs none: a broken connection is an
# error already, and a silently dead one is caught by HTTP/2 keepalive, which
# needs nothing tuned per package or per link.
#
# The server is told this value and stops at it too, rather than working on
# after the client has given up. That cuts both ways: too low a value aborts a
# deployment that was going to succeed, part way through. Raise it for a package
# whose hooks run an installer, per host in [remotes] if only some are slow.
deploy_timeout = 60
# Verify the server's identity and encrypt the connection. On by default.
#
# The archive is your project's code and whatever it carries with it, and
# without this it crosses the network in the clear to whichever machine answered
# on that address. `adeploy pair <host>` records which server that is - compare
# the fingerprint it prints with the one the server logs at startup - and every
# later connection checks against what was recorded, the way ssh does.
#
# There is no certificate authority involved and nothing to buy or renew: the
# server generates its own certificate on first run.
tls = true

# A package describes a deployment end to end: what to archive, where it goes
# and what runs around it. All of it travels to the server with the package, so
# a server needs no configuration for anything you deploy to it. Rename "demo"
# to suit, and add more tables for more packages.
[packages.demo]
# Files and directories to archive, in order. No glob expansion. Relative to the
# directory holding this file.
#
# A directory contributes its contents, not itself: "./dist/demo" puts whatever
# is inside dist/demo at the top of the deployment. Nest anything you want in a
# subdirectory - scripts, say - inside a source directory.
sources = ["./dist/demo"]
# Absolute directory on the server to unpack into. Absolute because the server
# keeps no root of its own for a relative path to hang from.
deploy_path = "/opt/demo"
# Snapshot the directory before unpacking, and what `adeploy rollback` restores.
# Snapshots are kept beside the server binary, in a directory named after the
# package: <server dir>/demo/backup_<timestamp>/
backup_enabled = true
# Commands run on the server before the new deployment goes live. One command or
# a list of them; the first failure aborts the deployment, which makes this the
# place to stop a service that holds the files open.
#
# They run with the unpacked package as the working directory, so a script the
# package ships is reachable by a relative path and never has to be put on the
# server by hand:
#
#   before_deploy = ["sc stop demo", "scripts/prepare.cmd"]
#
# before_deploy = "systemctl stop demo"
# Commands run once the deployment is live, from its directory. A failure is
# logged as a warning but the deployment still counts as successful.
# after_deploy = "systemctl start demo"

# Per-host overrides. Only list what differs from [defaults]; everything else is
# inherited. The key is the host you pass on the command line. A "default" entry
# applies to any host without its own table.
# [remotes."192.0.2.10"]
# port = 6070
# deploy_timeout = 1800   # this one runs an installer
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
#
# There is nothing here about any package. A deploying client brings its own
# description of what to install and where, so a server needs to know only who
# it will listen to.

[server]
# Port to bind. Clients must dial this same port. Changing it needs a restart.
listen_port = {port}
# Serve over TLS. On by default.
#
# The certificate and key beside this file were generated on first run, and the
# fingerprint printed at startup is what a pairing client compares against.
# Turning this off leaves every deployment readable by anyone on the network,
# and lets any machine on this address pass for this one.
tls = true
# Base64 Ed25519 public keys allowed to deploy here. A client that is not
# listed is rejected and prints its own key, ready to be pasted in below.
#
# `adeploy server clients` maintains the same list interactively
# through pairing, which is usually easier than copying a key by hand.
allowed_keys = []
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

  let settings = ServerSettings::default();
  let content = SERVER_TEMPLATE.replace("{port}", &settings.listen_port.to_string());

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

    assert_eq!(config.server.listen_port, 6060);
    assert!(
      config.server.tls,
      "a generated server must serve over TLS without being asked"
    );
    // The server template carries no client settings at all.
    assert!(
      !text.contains("[defaults]"),
      "the server never reads [defaults], so it must not be generated"
    );
    // Empty on purpose: the key does not exist until a client first runs.
    assert!(config.server.allowed_keys.is_empty());
    // Nothing about any package: a deploying client brings its own.
    assert!(
      config.packages.is_empty(),
      "the server template must declare no packages"
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
    assert_eq!(config.defaults.deploy_timeout, 60);
    assert!(config.defaults.tls);

    let demo = config
      .packages
      .get("demo")
      .expect("template must define the demo package");
    assert_eq!(demo.sources, vec!["./dist/demo".to_string()]);
    assert_eq!(demo.deploy_path.as_deref(), Some("/opt/demo"));
    assert!(demo.backup_enabled);
  }

  #[test]
  fn template_defaults_match_the_struct_defaults() {
    // The commented template and the serde defaults must not drift apart.
    let from_template: ProjectConfig = toml::from_str(TEMPLATE).expect("valid TOML");
    let from_empty: ProjectConfig = toml::from_str("").expect("empty config is valid");

    assert_eq!(from_template.defaults.port, from_empty.defaults.port);
    assert_eq!(from_template.defaults.tls, from_empty.defaults.tls);
    assert_eq!(
      from_template.defaults.connect_timeout,
      from_empty.defaults.connect_timeout
    );
    assert_eq!(
      from_template.defaults.deploy_timeout,
      from_empty.defaults.deploy_timeout
    );
  }
}
