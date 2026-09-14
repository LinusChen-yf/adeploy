use std::{
  collections::HashMap,
  env, fs,
  path::{Path, PathBuf},
};

use log2::*;
use serde::{Deserialize, Serialize};

use crate::{
  auth::Auth,
  error::{AdeployError, Result},
};

/// File name of the unified project configuration.
pub const PROJECT_CONFIG_NAME: &str = "adeploy.toml";

/// Legacy split configuration files, kept only to produce a helpful error.
const LEGACY_CONFIG_NAMES: [&str; 2] = ["client_config.toml", "server_config.toml"];

/// Absolute paths to the client's signing key pair.
#[derive(Clone, Debug)]
pub struct KeyPairPaths {
  pub private_key: PathBuf,
  pub public_key: PathBuf,
}

impl KeyPairPaths {
  pub fn new(private_key: PathBuf, public_key: PathBuf) -> Self {
    Self {
      private_key,
      public_key,
    }
  }
}

/// A parsed `adeploy.toml` together with the directory it was loaded from.
///
/// Relative `sources` are resolved against `base_dir`, so a configuration
/// committed to a repository behaves the same regardless of the working
/// directory the command was invoked from.
#[derive(Clone, Debug)]
pub struct LoadedConfig {
  pub config: ProjectConfig,
  pub base_dir: PathBuf,
  pub path: PathBuf,
}

/// Abstraction over how configuration and key material are discovered.
pub trait ConfigProvider: Send + Sync {
  /// Locate `adeploy.toml` for the current invocation.
  fn get_config_path(&self) -> Result<PathBuf>;

  /// Parse the configuration found at `path`.
  fn load_project_config(&self, path: &Path) -> Result<ProjectConfig>;

  fn get_key_paths(&self) -> Result<KeyPairPaths>;

  /// Locate and parse the configuration in one step.
  fn load(&self) -> Result<LoadedConfig> {
    let path = self.get_config_path()?;
    let config = self.load_project_config(&path)?;
    let base_dir = config_base_dir(&path)?;
    Ok(LoadedConfig {
      config,
      base_dir,
      path,
    })
  }
}

/// How `ConfigProviderImpl` locates `adeploy.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Discovery {
  /// Walk up from the working directory, then fall back to the executable's
  /// directory. Used by the client so a checkout carries its own config.
  #[default]
  Project,
  /// Use the executable's directory only.
  ///
  /// The server must never adopt a project's configuration just because it was
  /// started from inside a checkout, and the path has to be predictable so the
  /// file can be created when it does not exist yet.
  Server,
}

/// Default provider. Discovery depends on the mode it was built with.
#[derive(Default, Clone)]
pub struct ConfigProviderImpl {
  /// Explicit `--config` override; skips discovery entirely when set.
  override_path: Option<PathBuf>,
  discovery: Discovery,
}

impl ConfigProviderImpl {
  /// Provider for client commands: upward search from the working directory.
  pub fn with_override(path: Option<PathBuf>) -> Self {
    Self {
      override_path: path,
      discovery: Discovery::Project,
    }
  }

  /// Provider for the server: the executable's own directory.
  ///
  /// The returned path is where the configuration belongs, whether or not it
  /// exists; callers bootstrap it with `init::ensure_server_config`.
  pub fn for_server(path: Option<PathBuf>) -> Self {
    Self {
      override_path: path,
      discovery: Discovery::Server,
    }
  }
}

impl ConfigProvider for ConfigProviderImpl {
  fn get_config_path(&self) -> Result<PathBuf> {
    if let Some(path) = &self.override_path {
      // The server creates a missing file; a client has nothing to create.
      if self.discovery == Discovery::Project && !path.exists() {
        return Err(Box::new(AdeployError::Config(format!(
          "Configuration file not found: {}",
          path.display()
        ))));
      }
      return Ok(path.clone());
    }

    if self.discovery == Discovery::Server {
      return Ok(executable_dir()?.join(PROJECT_CONFIG_NAME));
    }

    // Walk up from the working directory so a repository checkout carries its
    // own configuration, the way `cargo` finds `Cargo.toml`.
    if let Ok(cwd) = env::current_dir() {
      if let Some(found) = find_upwards(&cwd, PROJECT_CONFIG_NAME) {
        return Ok(found);
      }
    }

    // Fall back to the executable's own directory.
    let exe_dir = executable_dir()?;
    let candidate = exe_dir.join(PROJECT_CONFIG_NAME);
    if candidate.exists() {
      return Ok(candidate);
    }

    Err(Box::new(AdeployError::Config(missing_config_message(
      &exe_dir,
    ))))
  }

  fn load_project_config(&self, path: &Path) -> Result<ProjectConfig> {
    let content = fs::read_to_string(path).map_err(|e| {
      Box::new(AdeployError::Config(format!(
        "Failed to read {}: {}",
        path.display(),
        e
      )))
    })?;

    toml::from_str(&content).map_err(|e| {
      Box::new(AdeployError::Config(format!(
        "Failed to parse {}: {}",
        path.display(),
        e
      )))
    })
  }

  fn get_key_paths(&self) -> Result<KeyPairPaths> {
    let exe_dir = executable_dir()?;
    let key_dir = exe_dir.join(".key");
    let private_key_path = key_dir.join("id_ed25519");
    let public_key_path = key_dir.join("id_ed25519.pub");

    if !key_dir.exists() {
      fs::create_dir_all(&key_dir).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to create key directory: {}",
          e
        )))
      })?;
    }

    if !private_key_path.exists() || !public_key_path.exists() {
      info!("Generating Ed25519 key pair");
      Auth::generate_key_pair(
        &public_key_path.to_string_lossy(),
        &private_key_path.to_string_lossy(),
      )?;
      info!("Stored key pair in {:?}", key_dir);
    }

    Ok(KeyPairPaths::new(private_key_path, public_key_path))
  }
}

/// Unified project configuration, shared by the client and the server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
  /// Values shared by every package and remote.
  #[serde(default)]
  pub defaults: Defaults,
  /// Deployable units, describing both what to package and how to install it.
  #[serde(default)]
  pub packages: HashMap<String, PackageConfig>,
  /// Per-host overrides, keyed by host or alias.
  #[serde(default)]
  pub remotes: HashMap<String, RemoteOverride>,
  /// Machine-local server settings; absent in a project checkout.
  #[serde(default)]
  pub server: ServerSettings,
}

/// Fallback values for `[remotes.*]`.
///
/// Every field here describes how *this client* reaches a server. The server
/// reads none of them: it is the client that dials, so the port and both
/// timeouts belong to the caller. Keeping server policy out of this table
/// matters because a project's configuration travels to the server with the
/// package, and nothing a client sends may decide what the server enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
  /// Port to dial. Must match the server's `[server].listen_port`.
  #[serde(default = "default_port")]
  pub port: u16,
  /// Seconds allowed to establish the connection. 0 disables the limit.
  #[serde(default = "default_connect_timeout")]
  pub connect_timeout: u64,
  /// Seconds allowed for upload plus remote deployment. 0 disables the limit.
  ///
  /// Sent to the server as the request's gRPC deadline, so both ends stop at
  /// the same moment instead of the server working on after the client gave up.
  #[serde(default = "default_deploy_timeout")]
  pub deploy_timeout: u64,
}

impl Default for Defaults {
  fn default() -> Self {
    Self {
      port: default_port(),
      connect_timeout: default_connect_timeout(),
      deploy_timeout: default_deploy_timeout(),
    }
  }
}

/// A deployable unit: what the client packages and how the server installs it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageConfig {
  /// Client side: files and directories to archive, relative to `adeploy.toml`.
  #[serde(default)]
  pub sources: Vec<String>,
  /// Server side: where to unpack. Relative paths land under `deploy_root`.
  #[serde(default)]
  pub deploy_path: Option<String>,
  /// Server side: run before unpacking; a non-zero exit aborts the deployment.
  #[serde(default)]
  pub before_deploy_script: Option<String>,
  /// Server side: run after unpacking; failure is logged but not fatal.
  #[serde(default)]
  pub after_deploy_script: Option<String>,
  /// Server side: replace the deploy directory rather than merging into it.
  ///
  /// Off by default because the directory may hold things the package does not
  /// ship - uploads, logs, a database - and wiping those on the next deploy
  /// would be a surprising way to find out. Turn it on for a package whose
  /// directory is entirely build output, so files dropped from the package
  /// stop lingering on the server.
  #[serde(default)]
  pub clean_deploy: bool,
  /// Server side: snapshot the existing directory before unpacking.
  #[serde(default)]
  pub backup_enabled: bool,
  /// Server side: where snapshots go; defaults to a directory beside the binary.
  #[serde(default)]
  pub backup_path: Option<String>,
}

/// Per-host overrides. Every field falls back to `[defaults]` when omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOverride {
  #[serde(default)]
  pub port: Option<u16>,
  #[serde(default)]
  pub connect_timeout: Option<u64>,
  #[serde(default)]
  pub deploy_timeout: Option<u64>,
}

/// Machine-local server settings. Not meaningful in a project checkout.
///
/// This is the only table the server takes policy from. Everything else in a
/// configuration may have come from a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSettings {
  /// Port to bind. Changing it requires a restart.
  #[serde(default = "default_port")]
  pub listen_port: u16,
  /// Largest archive this server accepts, in bytes.
  ///
  /// A request is decoded before the handler that checks `allowed_keys` runs,
  /// so this bounds what an unauthenticated caller can make the server buffer.
  #[serde(default = "default_max_file_size")]
  pub max_file_size: u64,
  /// Base64 Ed25519 public keys permitted to deploy.
  #[serde(default)]
  pub allowed_keys: Vec<String>,
  /// Root for package `deploy_path` values that are relative.
  #[serde(default)]
  pub deploy_root: Option<String>,
}

impl Default for ServerSettings {
  fn default() -> Self {
    Self {
      listen_port: default_port(),
      max_file_size: default_max_file_size(),
      allowed_keys: Vec::new(),
      deploy_root: None,
    }
  }
}

/// A remote's settings after `[defaults]` and `[remotes.*]` are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRemote {
  pub port: u16,
  pub connect_timeout: u64,
  pub deploy_timeout: u64,
}

impl ProjectConfig {
  /// Merge `[defaults]` with the `[remotes.*]` entry for `host`.
  ///
  /// Lookup order is the exact host, then `default`, then bare `[defaults]`, so
  /// a configuration without any `[remotes]` table still deploys anywhere.
  pub fn resolve_remote(&self, host: &str) -> ResolvedRemote {
    let over = self
      .remotes
      .get(host)
      .or_else(|| self.remotes.get("default"));

    ResolvedRemote {
      port: over.and_then(|o| o.port).unwrap_or(self.defaults.port),
      connect_timeout: over
        .and_then(|o| o.connect_timeout)
        .unwrap_or(self.defaults.connect_timeout),
      deploy_timeout: over
        .and_then(|o| o.deploy_timeout)
        .unwrap_or(self.defaults.deploy_timeout),
    }
  }

  /// Absolute directory a package unpacks into on this server.
  ///
  /// A relative `deploy_path` lands under `[server].deploy_root`, so a client
  /// cannot pick an arbitrary absolute location on the target machine.
  /// `fallback_root` is used when no `deploy_root` is configured.
  pub fn resolve_deploy_path(&self, package: &str, fallback_root: &Path) -> Option<PathBuf> {
    let config = self.packages.get(package)?;
    let root = self
      .server
      .deploy_root
      .as_deref()
      .map(PathBuf::from)
      .unwrap_or_else(|| fallback_root.to_path_buf());

    Some(match config.deploy_path.as_deref() {
      Some(path) => resolve_against(&root, path),
      None => root.join(package),
    })
  }

  /// Absolute source paths for `package`, resolved against `base_dir`.
  pub fn resolved_sources(&self, package: &str, base_dir: &Path) -> Option<Vec<PathBuf>> {
    let package = self.packages.get(package)?;
    Some(
      package
        .sources
        .iter()
        .map(|source| resolve_against(base_dir, source))
        .collect(),
    )
  }
}

/// Resolve `candidate` against `base_dir`, leaving absolute paths untouched.
pub fn resolve_against(base_dir: &Path, candidate: &str) -> PathBuf {
  let path = Path::new(candidate);
  let joined = if path.is_absolute() {
    path.to_path_buf()
  } else {
    base_dir.join(path)
  };

  // Collapse the `.` a `./relative` source leaves in the middle of the path, so
  // logs and error messages show something a reader recognises. This is pure
  // lexical cleanup: the filesystem is not consulted and `..` is left alone.
  joined.components().collect()
}

/// Directory that relative paths in a configuration resolve against.
pub fn config_base_dir(config_path: &Path) -> Result<PathBuf> {
  config_path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .map(Path::to_path_buf)
    .ok_or_else(|| {
      Box::new(AdeployError::Config(format!(
        "Cannot determine the directory containing {}",
        config_path.display()
      )))
    })
}

/// Walk from `start` toward the filesystem root looking for `file_name`.
fn find_upwards(start: &Path, file_name: &str) -> Option<PathBuf> {
  let mut current = Some(start);
  while let Some(dir) = current {
    let candidate = dir.join(file_name);
    if candidate.is_file() {
      return Some(candidate);
    }
    current = dir.parent();
  }
  None
}

/// Error text shown when no configuration can be found anywhere.
fn missing_config_message(exe_dir: &Path) -> String {
  let legacy: Vec<&str> = LEGACY_CONFIG_NAMES
    .iter()
    .copied()
    .filter(|name| exe_dir.join(name).exists())
    .collect();

  let mut message = format!(
    "No {name} found. Searched upward from the working directory and in {dir}.\n\
     Run `adeploy init` to create one.",
    name = PROJECT_CONFIG_NAME,
    dir = exe_dir.display()
  );

  if !legacy.is_empty() {
    message.push_str(&format!(
      "\nFound legacy {} in {}; these were replaced by a single {}.",
      legacy.join(" and "),
      exe_dir.display(),
      PROJECT_CONFIG_NAME
    ));
  }

  message
}

const fn default_port() -> u16 {
  6060
}

const fn default_connect_timeout() -> u64 {
  5
}

const fn default_deploy_timeout() -> u64 {
  600
}

const fn default_max_file_size() -> u64 {
  100 * 1024 * 1024
}

/// Directory containing the running executable.
pub fn executable_dir() -> Result<PathBuf> {
  let current_exe = env::current_exe().map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to get current executable path: {}",
      e
    )))
  })?;

  let current_dir = current_exe.parent().ok_or_else(|| {
    Box::new(AdeployError::FileSystem(
      "Failed to get parent directory of executable".to_string(),
    ))
  })?;

  Ok(current_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  fn parse(toml_text: &str) -> ProjectConfig {
    toml::from_str(toml_text).expect("configuration should parse")
  }

  #[test]
  fn empty_config_yields_documented_defaults() {
    let config = parse("");

    assert_eq!(config.defaults.port, 6060);
    assert_eq!(config.defaults.connect_timeout, 5);
    assert_eq!(config.defaults.deploy_timeout, 600);
    assert!(config.packages.is_empty());
    assert!(config.server.allowed_keys.is_empty());
  }

  #[test]
  fn server_policy_has_its_own_defaults() {
    let config = parse("");

    assert_eq!(config.server.listen_port, 6060);
    assert_eq!(config.server.max_file_size, 100 * 1024 * 1024);
    assert!(config.server.deploy_root.is_none());
  }

  #[test]
  fn client_tables_cannot_carry_server_policy() {
    // A project's configuration travels to the server with its packages, so the
    // parser must refuse server policy written into a client table rather than
    // quietly ignoring it.
    for text in [
      "[defaults]\nmax_file_size = 1\n",
      "[defaults]\nlisten_port = 1\n",
      "[remotes.default]\nmax_file_size = 1\n",
      "[defaults]\nallowed_keys = []\n",
    ] {
      assert!(
        toml::from_str::<ProjectConfig>(text).is_err(),
        "server policy must be rejected in a client table: {text}"
      );
    }
  }

  #[test]
  fn server_tables_cannot_carry_client_settings() {
    // The mirror image: the server never dials, so a connect timeout there is a
    // mistake worth reporting.
    for text in ["[server]\nconnect_timeout = 5\n", "[server]\nport = 6060\n"] {
      assert!(
        toml::from_str::<ProjectConfig>(text).is_err(),
        "client settings must be rejected in [server]: {text}"
      );
    }
  }

  #[test]
  fn unknown_fields_are_rejected() {
    // A typo should fail loudly rather than being silently ignored.
    let error = toml::from_str::<ProjectConfig>("[defaults]\nconect_timeout = 5\n")
      .expect_err("unknown key must be rejected");
    assert!(
      error.to_string().contains("conect_timeout"),
      "error should name the offending key, got: {error}"
    );
  }

  #[test]
  fn remote_lookup_prefers_host_then_default_then_defaults() {
    let config = parse(
      r#"
[defaults]
port = 1000
deploy_timeout = 600

[remotes.default]
port = 2000

[remotes."10.0.0.1"]
port = 3000
deploy_timeout = 30
"#,
    );

    let exact = config.resolve_remote("10.0.0.1");
    assert_eq!(exact.port, 3000);
    assert_eq!(exact.deploy_timeout, 30);

    // Falls back to [remotes.default] for the port, but [defaults] for the
    // timeout that entry does not override.
    let fallback = config.resolve_remote("10.0.0.2");
    assert_eq!(fallback.port, 2000);
    assert_eq!(fallback.deploy_timeout, 600);
  }

  #[test]
  fn listen_port_is_independent_of_the_dial_port() {
    // They are different concepts that happen to share a default, so a config
    // setting one must not move the other.
    let config = parse("[defaults]\nport = 7000\n\n[server]\nlisten_port = 8000\n");

    assert_eq!(config.resolve_remote("anywhere").port, 7000);
    assert_eq!(config.server.listen_port, 8000);
  }

  #[test]
  fn remote_lookup_works_without_any_remotes_table() {
    // The zero-configuration path: deploy anywhere using [defaults] alone.
    let config = parse("[defaults]\nport = 7000\nconnect_timeout = 9\n");

    let remote = config.resolve_remote("192.0.2.1");
    assert_eq!(remote.port, 7000);
    assert_eq!(remote.connect_timeout, 9);
  }

  #[test]
  fn sources_resolve_against_the_config_directory() {
    let config = parse("[packages.demo]\nsources = [\"./dist/demo\", \"/absolute/path\"]\n");
    let base = Path::new("/projects/app");

    let sources = config
      .resolved_sources("demo", base)
      .expect("package should exist");

    // The `./` from the config is collapsed rather than carried into logs.
    assert_eq!(sources[0], PathBuf::from("/projects/app/dist/demo"));
    // An absolute source is left untouched.
    assert_eq!(sources[1], PathBuf::from("/absolute/path"));
  }

  #[test]
  fn unknown_package_has_no_sources() {
    let config = parse("[packages.demo]\nsources = []\n");
    assert!(config
      .resolved_sources("absent", Path::new("/tmp"))
      .is_none());
  }

  #[test]
  fn relative_deploy_path_lands_under_deploy_root() {
    let config = parse(
      r#"
[server]
deploy_root = "/opt"

[packages.demo]
deploy_path = "demo"
"#,
    );

    let resolved = config
      .resolve_deploy_path("demo", Path::new("/fallback"))
      .expect("package should exist");
    assert_eq!(resolved, PathBuf::from("/opt/demo"));
  }

  #[test]
  fn absolute_deploy_path_overrides_deploy_root() {
    let config = parse(
      r#"
[server]
deploy_root = "/opt"

[packages.demo]
deploy_path = "/srv/demo"
"#,
    );

    let resolved = config
      .resolve_deploy_path("demo", Path::new("/fallback"))
      .expect("package should exist");
    assert_eq!(resolved, PathBuf::from("/srv/demo"));
  }

  #[test]
  fn missing_deploy_path_defaults_to_the_package_name() {
    let config = parse("[packages.demo]\nsources = []\n");

    // No deploy_root either, so the fallback root is used.
    let resolved = config
      .resolve_deploy_path("demo", Path::new("/var/lib/adeploy"))
      .expect("package should exist");
    assert_eq!(resolved, PathBuf::from("/var/lib/adeploy/demo"));
  }

  #[test]
  fn find_upwards_locates_a_config_in_an_ancestor() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    let nested = root.join("crates").join("app").join("src");
    fs::create_dir_all(&nested).expect("create nested dirs");
    let config_path = root.join(PROJECT_CONFIG_NAME);
    fs::write(&config_path, "").expect("write config");

    let found = find_upwards(&nested, PROJECT_CONFIG_NAME).expect("config should be found");
    assert_eq!(found, config_path);
  }

  #[test]
  fn find_upwards_returns_none_when_absent() {
    let temp = TempDir::new().expect("temp dir");
    assert!(find_upwards(temp.path(), PROJECT_CONFIG_NAME).is_none());
  }

  #[test]
  fn server_discovery_accepts_a_path_that_does_not_exist_yet() {
    // The server creates its configuration, so a missing file is not an error
    // the way it is for a client that has nothing to generate.
    let temp = TempDir::new().expect("temp dir");
    let missing = temp.path().join("adeploy.toml");

    let resolved = ConfigProviderImpl::for_server(Some(missing.clone()))
      .get_config_path()
      .expect("server discovery must tolerate a missing file");
    assert_eq!(resolved, missing);
  }

  #[test]
  fn server_discovery_ignores_the_working_directory() {
    // Starting the server from inside a project checkout must not make it adopt
    // that project's configuration.
    let resolved = ConfigProviderImpl::for_server(None)
      .get_config_path()
      .expect("server discovery should resolve");

    assert_eq!(
      resolved,
      executable_dir().expect("exe dir").join(PROJECT_CONFIG_NAME)
    );
  }

  #[test]
  fn override_path_must_exist() {
    let temp = TempDir::new().expect("temp dir");
    let missing = temp.path().join("nope.toml");
    let provider = ConfigProviderImpl::with_override(Some(missing.clone()));

    let error = provider
      .get_config_path()
      .expect_err("a missing --config file must fail");
    assert!(error.to_string().contains("nope.toml"));
  }

  #[test]
  fn load_reports_the_directory_relative_paths_resolve_against() {
    let temp = TempDir::new().expect("temp dir");
    let config_path = temp.path().join(PROJECT_CONFIG_NAME);
    fs::write(&config_path, "[packages.demo]\nsources = [\"./dist\"]\n").expect("write config");

    let loaded = ConfigProviderImpl::with_override(Some(config_path.clone()))
      .load()
      .expect("configuration should load");

    assert_eq!(loaded.path, config_path);
    assert_eq!(loaded.base_dir, temp.path());
  }
}
