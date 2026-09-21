use std::{fs, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
  auth::fingerprint,
  error::{AdeployError, Result},
};

/// File the server manages itself, beside its binary.
pub const PAIRED_FILE_NAME: &str = "paired.toml";

/// Ceiling on queued requests.
///
/// The queue is the one surface reachable without an approved key, so it is
/// also the one an unknown caller can fill. Requests are deduplicated by key,
/// so filling it needs that many distinct keys, and an operator looking at a
/// full queue can see that is what happened.
const MAX_PENDING: usize = 32;

/// A client that has asked to be trusted, or already is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedClient {
  /// Base64 Ed25519 public key.
  pub public_key: String,
  /// Derived from `public_key`; stored so the operator can read the file.
  pub fingerprint: String,
  /// Name the client reported, usually its hostname.
  pub client_name: String,
  /// Address the request arrived from, when the server could determine it.
  #[serde(default)]
  pub client_address: Option<String>,
  pub first_seen: DateTime<Utc>,
  #[serde(default)]
  pub decided_at: Option<DateTime<Utc>>,
}

impl PairedClient {
  pub fn new(public_key: String, client_name: String, client_address: Option<String>) -> Self {
    Self {
      fingerprint: fingerprint(&public_key),
      public_key,
      client_name,
      client_address,
      first_seen: Utc::now(),
      decided_at: None,
    }
  }

  /// One line for the operator, naming who and from where.
  pub fn describe(&self) -> String {
    match &self.client_address {
      Some(address) => format!("{} ({})  {}", self.client_name, address, self.fingerprint),
      None => format!("{}  {}", self.client_name, self.fingerprint),
    }
  }
}

/// Clients queued for approval, and those already approved or refused.
///
/// Kept apart from `adeploy.toml` on purpose: this file is written by the tool,
/// and rewriting an operator's hand-edited configuration would cost them their
/// comments and layout. `allowed_keys` there still works and is simply unioned
/// with what is approved here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairStore {
  #[serde(default)]
  pub pending: Vec<PairedClient>,
  #[serde(default)]
  pub approved: Vec<PairedClient>,
  #[serde(default)]
  pub rejected: Vec<PairedClient>,
}

/// What happened to a request that arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
  /// Newly queued.
  Queued,
  /// Already queued; the request changed nothing.
  AlreadyPending,
  /// Already trusted.
  AlreadyApproved,
  /// Previously refused by an operator.
  Rejected,
  /// The queue is full.
  QueueFull,
}

impl PairStore {
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

  /// Write the store atomically.
  ///
  /// The server appends pending requests while a separate `adeploy server
  /// approve` process rewrites the file, so a half-written file would be read
  /// by whichever of them looked next. Writing a temporary file and renaming it
  /// means a reader sees either the old contents or the new ones.
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
        "Failed to serialise the pairing store: {}",
        e
      )))
    })?;
    let content = format!(
      "# Managed by adeploy. Use `adeploy server clients` to edit it.\n\
       # rather than editing this file by hand.\n\n{body}"
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

  /// Is this key allowed to deploy?
  pub fn is_approved(&self, public_key: &str) -> bool {
    let key = public_key.trim();
    self
      .approved
      .iter()
      .any(|client| client.public_key.trim() == key)
  }

  /// Record a pairing request, returning what it resolved to.
  pub fn request(
    &mut self,
    public_key: &str,
    client_name: &str,
    client_address: Option<String>,
  ) -> PairOutcome {
    let key = public_key.trim();

    if self.is_approved(key) {
      return PairOutcome::AlreadyApproved;
    }
    // A refusal is a single answer to a single request, not a ban. It is kept
    // only long enough to reach the client that asked - the operator rejecting
    // and the server answering are different processes, so the decision has to
    // travel through this file - and is dropped as it is delivered. Asking
    // again afterwards queues a fresh request, which is what an operator who
    // refused the wrong row would expect, and it means a client polling while
    // it waits cannot be told "rejected" forever by a decision it already
    // acted on.
    if let Some(index) = self
      .rejected
      .iter()
      .position(|client| client.public_key.trim() == key)
    {
      self.rejected.remove(index);
      return PairOutcome::Rejected;
    }
    // Repeating a request must not consume another slot, so a client retrying
    // while it waits cannot fill the queue on its own.
    if let Some(existing) = self
      .pending
      .iter_mut()
      .find(|client| client.public_key.trim() == key)
    {
      existing.client_name = client_name.to_string();
      existing.client_address = client_address;
      return PairOutcome::AlreadyPending;
    }
    if self.pending.len() >= MAX_PENDING {
      return PairOutcome::QueueFull;
    }

    self.pending.push(PairedClient::new(
      key.to_string(),
      client_name.to_string(),
      client_address,
    ));
    PairOutcome::Queued
  }

  /// Approve a pending request chosen by list position or fingerprint.
  pub fn approve(&mut self, selector: &str) -> Result<PairedClient> {
    let index = self.find_pending(selector)?;
    let mut client = self.pending.remove(index);
    client.decided_at = Some(Utc::now());
    self.approved.push(client.clone());
    Ok(client)
  }

  /// Refuse a pending request, once.
  ///
  /// The entry left behind is an undelivered answer rather than a record: the
  /// next request from that key is told it was refused, and the entry goes.
  /// A client that never comes back leaves one behind, so the set is bounded
  /// the same way the queue is.
  pub fn reject(&mut self, selector: &str) -> Result<PairedClient> {
    let index = self.find_pending(selector)?;
    let mut client = self.pending.remove(index);
    client.decided_at = Some(Utc::now());
    self.rejected.push(client.clone());
    while self.rejected.len() > MAX_PENDING {
      self.rejected.remove(0);
    }
    Ok(client)
  }

  /// Withdraw trust from an approved client.
  pub fn revoke(&mut self, selector: &str) -> Result<PairedClient> {
    let index = find(&self.approved, selector).ok_or_else(|| {
      Box::new(AdeployError::Config(format!(
        "No approved client matches '{}'",
        selector
      )))
    })?;
    let client = self.approved.remove(index);
    Ok(client)
  }

  fn find_pending(&self, selector: &str) -> Result<usize> {
    find(&self.pending, selector).ok_or_else(|| {
      Box::new(AdeployError::Config(format!(
        "No pending request matches '{}'",
        selector
      )))
    })
  }
}

/// Resolve a selector against a list: a 1-based position, or a fingerprint
/// prefix so an operator can paste the part they can actually read.
fn find(clients: &[PairedClient], selector: &str) -> Option<usize> {
  let selector = selector.trim();

  if let Ok(position) = selector.parse::<usize>() {
    if position >= 1 && position <= clients.len() {
      return Some(position - 1);
    }
  }

  let matches: Vec<usize> = clients
    .iter()
    .enumerate()
    .filter(|(_, client)| {
      client.fingerprint == selector
        || client
          .fingerprint
          .trim_start_matches("SHA256:")
          .starts_with(selector.trim_start_matches("SHA256:"))
        || client.public_key.trim() == selector
    })
    .map(|(index, _)| index)
    .collect();

  // An ambiguous prefix must not silently pick one.
  match matches.as_slice() {
    [only] => Some(*only),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIexampleKeyAAAAAAAA=";
  const KEY_B: &str = "BBBBC3NzaC1lZDI1NTE5AAAAIexampleKeyBBBBBBBB=";

  fn queued_store() -> PairStore {
    let mut store = PairStore::default();
    assert_eq!(
      store.request(KEY_A, "dev-box", Some("192.0.2.10".into())),
      PairOutcome::Queued
    );
    store
  }

  #[test]
  fn a_request_queues_once_however_often_it_repeats() {
    let mut store = queued_store();

    assert_eq!(
      store.request(KEY_A, "dev-box", None),
      PairOutcome::AlreadyPending,
      "a client polling while it waits must not consume another slot"
    );
    assert_eq!(store.pending.len(), 1);
  }

  #[test]
  fn approving_moves_a_request_and_grants_access() {
    let mut store = queued_store();
    assert!(!store.is_approved(KEY_A));

    let approved = store.approve("1").expect("approve by position");

    assert_eq!(approved.public_key, KEY_A);
    assert!(store.pending.is_empty());
    assert!(store.is_approved(KEY_A));
    assert!(approved.decided_at.is_some());
  }

  #[test]
  fn a_refusal_is_delivered_once_and_then_forgotten() {
    let mut store = queued_store();
    store.reject("1").expect("reject");

    assert_eq!(
      store.request(KEY_A, "dev-box", None),
      PairOutcome::Rejected,
      "the client that was refused has to be told so"
    );
    assert!(
      store.pending.is_empty(),
      "and must not be queued again by the request that carried the answer"
    );

    // Asking again is a new request, not a repeat of the refused one. A
    // refusal that outlived its delivery would be a ban nobody asked for, and
    // would leave a client that polls while waiting stuck being refused a
    // decision it already acted on.
    assert_eq!(
      store.request(KEY_A, "dev-box", None),
      PairOutcome::Queued,
      "a later attempt must start over rather than inherit the refusal"
    );
    assert_eq!(store.pending.len(), 1);
  }

  #[test]
  fn undelivered_refusals_cannot_grow_without_bound() {
    let mut store = PairStore::default();
    for index in 0..MAX_PENDING + 5 {
      let key = format!("key-{index}");
      store.request(&key, "dev-box", None);
      store.reject("1").expect("reject");
    }

    assert_eq!(
      store.rejected.len(),
      MAX_PENDING,
      "clients that never come back must not grow this file for ever"
    );
  }

  #[test]
  fn an_approved_key_reports_itself_rather_than_queueing() {
    let mut store = queued_store();
    store.approve("1").expect("approve");

    assert_eq!(
      store.request(KEY_A, "dev-box", None),
      PairOutcome::AlreadyApproved
    );
  }

  #[test]
  fn revoking_withdraws_access() {
    let mut store = queued_store();
    store.approve("1").expect("approve");

    store.revoke(KEY_A).expect("revoke by key");
    assert!(!store.is_approved(KEY_A));
  }

  #[test]
  fn the_queue_is_bounded() {
    let mut store = PairStore::default();
    for index in 0..MAX_PENDING {
      assert_eq!(
        store.request(&format!("key-{index}"), "flood", None),
        PairOutcome::Queued
      );
    }

    assert_eq!(
      store.request("one-too-many", "flood", None),
      PairOutcome::QueueFull
    );
    assert_eq!(store.pending.len(), MAX_PENDING);
  }

  #[test]
  fn a_fingerprint_prefix_selects_one_request() {
    let mut store = queued_store();
    let fingerprint = store.pending[0].fingerprint.clone();
    let prefix = &fingerprint["SHA256:".len().."SHA256:".len() + 8];

    let approved = store
      .approve(prefix)
      .expect("approve by fingerprint prefix");
    assert_eq!(approved.public_key, KEY_A);
  }

  #[test]
  fn an_unmatched_selector_is_an_error_rather_than_a_guess() {
    let mut store = queued_store();

    assert!(store.approve("2").is_err(), "position past the end");
    assert!(store.approve("nope").is_err(), "no such fingerprint");
    assert_eq!(store.pending.len(), 1, "nothing may be consumed on error");
  }

  #[test]
  fn a_missing_file_loads_as_an_empty_store() {
    let temp = TempDir::new().expect("temp dir");
    let store = PairStore::load(&temp.path().join(PAIRED_FILE_NAME)).expect("load");

    assert!(store.pending.is_empty());
    assert!(store.approved.is_empty());
  }

  #[test]
  fn a_saved_store_round_trips() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join(PAIRED_FILE_NAME);

    let mut store = queued_store();
    store.request(KEY_B, "other-box", None);
    store.approve("1").expect("approve");
    store.save(&path).expect("save");

    let reloaded = PairStore::load(&path).expect("reload");
    assert!(reloaded.is_approved(KEY_A));
    assert_eq!(reloaded.pending.len(), 1);
    assert_eq!(reloaded.pending[0].public_key, KEY_B);
    assert!(
      !path.with_extension("toml.tmp").exists(),
      "the temporary file must not be left behind"
    );
  }
}
