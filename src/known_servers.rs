//! The servers this machine has decided to trust, and what they look like.
//!
//! The mirror of `paired.toml`: that file is a server's record of the clients
//! it will accept, this one is a client's record of the servers it will talk
//! to. Both are written by the tool, both are built on a person comparing a
//! fingerprint once, and neither involves a certificate authority.

use std::{fs, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AdeployError, Result};

/// File the client manages itself, beside its keys.
pub const KNOWN_SERVERS_FILE_NAME: &str = "known_servers.toml";

/// A server this machine has recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownServer {
  /// Host as it is written on the command line, which is what a deployment
  /// looks up. An address and a name for the same machine are separate
  /// entries, because only one of them is what anybody typed.
  pub host: String,
  /// Derived from the certificate; stored so the file can be read by a person.
  pub fingerprint: String,
  /// The certificate itself, which is what the connection is verified against.
  pub certificate: String,
  pub first_seen: DateTime<Utc>,
}

/// What recording a certificate resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
  /// This host was not known before.
  Recorded,
  /// Exactly what was already on file.
  Unchanged,
  /// A different certificate than the one recorded, and no instruction to
  /// replace it. Nothing was written.
  Conflict { recorded: String },
  /// Replaced on request.
  Replaced { previous: String },
}

/// Servers this client trusts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownServers {
  #[serde(default)]
  pub servers: Vec<KnownServer>,
}

impl KnownServers {
  /// Read the store, treating a missing file as an empty one.
  pub fn load(path: &Path) -> Result<Self> {
    if !path.exists() {
      return Ok(Self::default());
    }

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

  /// Write the store atomically, for the same reason the pairing store does:
  /// a half-written file is one a later run would read as the truth.
  pub fn save(&self, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to create {}: {}",
          parent.display(),
          e
        )))
      })?;
    }

    let body = toml::to_string_pretty(self).map_err(|e| {
      Box::new(AdeployError::Config(format!(
        "Failed to serialise the known servers: {e}"
      )))
    })?;
    let content = format!(
      "# Servers this machine has paired with, written by adeploy.\n\
       # Remove an entry, or run `adeploy pair <host> --force`, to trust a\n\
       # different certificate for that host.\n\n{body}"
    );

    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, content).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to write {}: {}",
        temporary.display(),
        e
      )))
    })?;
    fs::rename(&temporary, path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to replace {}: {}",
        path.display(),
        e
      )))
    })?;

    Ok(())
  }

  /// What this machine has on file for `host`.
  pub fn get(&self, host: &str) -> Option<&KnownServer> {
    let host = host.trim();
    self.servers.iter().find(|server| server.host == host)
  }

  /// Record what `host` presented.
  ///
  /// A certificate that disagrees with the one on file is refused rather than
  /// quietly replaced: it means either the server was rebuilt, or something
  /// else is answering on that address, and only the person at the keyboard
  /// knows which.
  pub fn record(
    &mut self,
    host: &str,
    certificate: &str,
    fingerprint: &str,
    replace: bool,
  ) -> RecordOutcome {
    let host = host.trim();

    if let Some(existing) = self.servers.iter_mut().find(|server| server.host == host) {
      if existing.fingerprint == fingerprint {
        return RecordOutcome::Unchanged;
      }
      if !replace {
        return RecordOutcome::Conflict {
          recorded: existing.fingerprint.clone(),
        };
      }

      let previous = std::mem::replace(&mut existing.fingerprint, fingerprint.to_string());
      existing.certificate = certificate.to_string();
      existing.first_seen = Utc::now();
      return RecordOutcome::Replaced { previous };
    }

    self.servers.push(KnownServer {
      host: host.to_string(),
      fingerprint: fingerprint.to_string(),
      certificate: certificate.to_string(),
      first_seen: Utc::now(),
    });
    RecordOutcome::Recorded
  }

  /// Stop trusting `host`, reporting whether anything was on file.
  pub fn forget(&mut self, host: &str) -> bool {
    let host = host.trim();
    let before = self.servers.len();
    self.servers.retain(|server| server.host != host);
    self.servers.len() != before
  }
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  const CERT_A: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
  const CERT_B: &str = "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----\n";
  const PRINT_A: &str = "SHA256:aaaaaaaa";
  const PRINT_B: &str = "SHA256:bbbbbbbb";

  #[test]
  fn a_server_is_recorded_once_and_then_recognised() {
    let mut store = KnownServers::default();

    assert_eq!(
      store.record("host", CERT_A, PRINT_A, false),
      RecordOutcome::Recorded
    );
    assert_eq!(
      store.record("host", CERT_A, PRINT_A, false),
      RecordOutcome::Unchanged,
      "pairing again with the same server must not look like a change"
    );
    assert_eq!(store.servers.len(), 1);
    assert_eq!(store.get("host").expect("recorded").certificate, CERT_A);
  }

  #[test]
  fn a_different_certificate_is_refused_rather_than_replaced() {
    let mut store = KnownServers::default();
    store.record("host", CERT_A, PRINT_A, false);

    assert_eq!(
      store.record("host", CERT_B, PRINT_B, false),
      RecordOutcome::Conflict {
        recorded: PRINT_A.to_string()
      },
      "something else answering on that address must not silently take it over"
    );
    assert_eq!(
      store.get("host").expect("still recorded").fingerprint,
      PRINT_A,
      "and nothing may be written"
    );
  }

  #[test]
  fn a_replacement_is_possible_when_it_is_asked_for() {
    let mut store = KnownServers::default();
    store.record("host", CERT_A, PRINT_A, false);

    assert_eq!(
      store.record("host", CERT_B, PRINT_B, true),
      RecordOutcome::Replaced {
        previous: PRINT_A.to_string()
      }
    );
    assert_eq!(store.get("host").expect("recorded").fingerprint, PRINT_B);
    assert_eq!(store.servers.len(), 1, "replacing must not add an entry");
  }

  #[test]
  fn hosts_are_recorded_separately() {
    let mut store = KnownServers::default();
    store.record("one", CERT_A, PRINT_A, false);
    store.record("two", CERT_B, PRINT_B, false);

    assert_eq!(store.get("one").expect("one").fingerprint, PRINT_A);
    assert_eq!(store.get("two").expect("two").fingerprint, PRINT_B);
    assert!(store.get("three").is_none());
  }

  #[test]
  fn forgetting_reports_whether_there_was_anything_to_forget() {
    let mut store = KnownServers::default();
    store.record("host", CERT_A, PRINT_A, false);

    assert!(store.forget("host"));
    assert!(store.get("host").is_none());
    assert!(!store.forget("host"), "forgetting twice is not a change");
  }

  #[test]
  fn a_saved_store_round_trips() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join(KNOWN_SERVERS_FILE_NAME);

    let mut store = KnownServers::default();
    store.record("192.0.2.10", CERT_A, PRINT_A, false);
    store.save(&path).expect("save");

    let reloaded = KnownServers::load(&path).expect("reload");
    let server = reloaded.get("192.0.2.10").expect("recorded");
    assert_eq!(server.fingerprint, PRINT_A);
    assert_eq!(server.certificate, CERT_A);
    assert!(
      !path.with_extension("toml.tmp").exists(),
      "the temporary file must not be left behind"
    );
  }

  #[test]
  fn a_missing_file_loads_as_an_empty_store() {
    let temp = TempDir::new().expect("temp dir");
    let store = KnownServers::load(&temp.path().join(KNOWN_SERVERS_FILE_NAME)).expect("load");
    assert!(store.servers.is_empty());
  }
}
