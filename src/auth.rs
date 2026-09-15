use std::{
  fs::{self, OpenOptions},
  io::Write,
  path::Path,
};

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use log2::*;
use rand_core::OsRng;
use sha2::{Digest, Sha256};

use crate::{
  adeploy::DeployManifest,
  error::{AdeployError, Result},
};

/// Ed25519 authentication helper
pub struct Auth {
  keypair: Option<SigningKey>,
}

impl Auth {
  pub fn new() -> Self {
    Self { keypair: None }
  }

  /// Generate an Ed25519 key pair and save it to disk
  pub fn generate_key_pair(public_key_path: &str, private_key_path: &str) -> Result<()> {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);

    let private_path = Path::new(private_key_path);
    let mut private_file = OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .open(private_path)
      .map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to open private key for writing: {}",
          e
        )))
      })?;

    private_file
      .write_all(&signing_key.to_bytes())
      .map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to write private key: {}",
          e
        )))
      })?;

    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let perms = fs::Permissions::from_mode(0o600);
      fs::set_permissions(private_path, perms).map_err(|e| {
        Box::new(AdeployError::FileSystem(format!(
          "Failed to set private key permissions: {}",
          e
        )))
      })?;
    }

    // Write public key as base64
    let verifying_key = signing_key.verifying_key();
    let public_key_str = base64::engine::general_purpose::STANDARD.encode(verifying_key.to_bytes());
    fs::write(public_key_path, public_key_str).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to write public key: {}",
        e
      )))
    })?;

    info!(
      "Generated Ed25519 key pair at {} and {}",
      private_key_path, public_key_path
    );
    Ok(())
  }

  /// Load Ed25519 key pair from files
  pub fn load_key_pair(private_key_path: &str) -> Result<SigningKey> {
    // Read private key
    let private_key_bytes = std::fs::read(private_key_path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to read private key: {}",
        e
      )))
    })?;

    if private_key_bytes.len() != 32 {
      return Err(Box::new(AdeployError::Auth(
        "Invalid private key length".to_string(),
      )));
    }

    let signing_key = SigningKey::from_bytes(&private_key_bytes.try_into().map_err(|_| {
      Box::new(AdeployError::Auth(
        "Failed to convert private key bytes".to_string(),
      ))
    })?);

    Ok(signing_key)
  }

  /// Load Ed25519 public key from file
  pub fn load_public_key<P: AsRef<Path>>(path: P) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to read public key: {}",
        e
      )))
    })
  }

  /// Create Auth with key pair
  pub fn with_key_pair(signing_key: SigningKey) -> Self {
    Self {
      keypair: Some(signing_key),
    }
  }

  /// Generate Ed25519 signature for data
  pub fn sign_data(&self, data: &[u8]) -> Result<Vec<u8>> {
    if let Some(signing_key) = &self.keypair {
      let signature = signing_key.sign(data);
      Ok(signature.to_bytes().to_vec())
    } else {
      Err(Box::new(AdeployError::Auth(
        "No keypair available for signing".to_string(),
      )))
    }
  }

  /// Verify Ed25519 signature
  pub fn verify_signature(
    public_key_str: &str,
    data: &[u8],
    signature_bytes: &[u8],
  ) -> Result<bool> {
    // Decode the base64 public key
    let public_key_bytes = base64::engine::general_purpose::STANDARD
      .decode(public_key_str.trim())
      .map_err(|e| {
        Box::new(AdeployError::Auth(format!(
          "Failed to decode public key: {}",
          e
        )))
      })?;

    // Build verifying key
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes.try_into().map_err(|_| {
      Box::new(AdeployError::Auth(
        "Failed to convert public key bytes".to_string(),
      ))
    })?)
    .map_err(|e| {
      Box::new(AdeployError::Auth(format!(
        "Failed to parse public key: {}",
        e
      )))
    })?;

    // Build signature
    let signature = Signature::from_bytes(signature_bytes.try_into().map_err(|_| {
      Box::new(AdeployError::Auth(
        "Failed to convert signature bytes".to_string(),
      ))
    })?);

    // Verify the signature
    match verifying_key.verify(data, &signature) {
      Ok(()) => Ok(true),
      Err(_) => Ok(false),
    }
  }
}

/// Bytes signed by a `DeployStart`, and verified by the server.
///
/// Fields are length-prefixed so they cannot be confused with one another:
/// without prefixes a package named `a` with hash `bc` would produce the same
/// bytes as one named `ab` with hash `c`. The leading domain tag keeps a
/// signature from also being valid for some future message that happens to
/// serialise the same way.
///
/// Covering the description rather than only the archive bytes is what binds an
/// upload to the package it was meant for: previously `package_name` was
/// unsigned, so a captured request could be replayed against a different
/// package's deploy path and hooks.
#[allow(clippy::too_many_arguments)]
pub fn deploy_start_signing_payload(
  package_name: &str,
  total_size: u64,
  file_hash: &str,
  public_key: &str,
  nonce: &str,
  timestamp_ms: i64,
  deploy_timeout_secs: u64,
  manifest: Option<&DeployManifest>,
) -> Vec<u8> {
  // Bumped with the fields: a signature made for an older shape must not verify
  // against this one, where the new fields would otherwise be unauthenticated.
  const DOMAIN: &[u8] = b"adeploy:deploy-start:v3";

  let mut payload = Vec::with_capacity(DOMAIN.len() + 128);
  payload.extend_from_slice(DOMAIN);
  push_field(&mut payload, package_name.as_bytes());
  payload.extend_from_slice(&total_size.to_le_bytes());
  push_field(&mut payload, file_hash.as_bytes());
  push_field(&mut payload, public_key.as_bytes());
  push_field(&mut payload, nonce.as_bytes());
  payload.extend_from_slice(&timestamp_ms.to_le_bytes());
  payload.extend_from_slice(&deploy_timeout_secs.to_le_bytes());
  push_manifest(&mut payload, manifest);
  payload
}

/// Bytes self-signed by a `PairRequest`.
///
/// A pairing request cannot be checked against a key the server already knows,
/// so the signature proves only that the sender holds the key it is presenting.
/// That is enough to keep anyone from queueing keys they do not control.
pub fn pair_signing_payload(
  public_key: &str,
  client_name: &str,
  nonce: &str,
  timestamp_ms: i64,
) -> Vec<u8> {
  const DOMAIN: &[u8] = b"adeploy:pair:v1";

  let mut payload = Vec::with_capacity(DOMAIN.len() + 96);
  payload.extend_from_slice(DOMAIN);
  push_field(&mut payload, public_key.as_bytes());
  push_field(&mut payload, client_name.as_bytes());
  push_field(&mut payload, nonce.as_bytes());
  payload.extend_from_slice(&timestamp_ms.to_le_bytes());
  payload
}

/// Bytes signed by a `BackupListRequest`.
pub fn backup_list_signing_payload(
  package_name: &str,
  public_key: &str,
  nonce: &str,
  timestamp_ms: i64,
  manifest: Option<&DeployManifest>,
) -> Vec<u8> {
  const DOMAIN: &[u8] = b"adeploy:backup-list:v2";

  let mut payload = Vec::with_capacity(DOMAIN.len() + 96);
  payload.extend_from_slice(DOMAIN);
  push_field(&mut payload, package_name.as_bytes());
  push_field(&mut payload, public_key.as_bytes());
  push_field(&mut payload, nonce.as_bytes());
  payload.extend_from_slice(&timestamp_ms.to_le_bytes());
  push_manifest(&mut payload, manifest);
  payload
}

/// Bytes signed by a `RollbackRequest`.
///
/// A separate domain from a deployment's: a signature authorising one must
/// never be usable as the other, since rolling back replaces a live directory
/// just as thoroughly as deploying does.
#[allow(clippy::too_many_arguments)]
pub fn rollback_signing_payload(
  package_name: &str,
  backup_name: &str,
  public_key: &str,
  nonce: &str,
  timestamp_ms: i64,
  deploy_timeout_secs: u64,
  manifest: Option<&DeployManifest>,
) -> Vec<u8> {
  const DOMAIN: &[u8] = b"adeploy:rollback:v3";

  let mut payload = Vec::with_capacity(DOMAIN.len() + 128);
  payload.extend_from_slice(DOMAIN);
  push_field(&mut payload, package_name.as_bytes());
  push_field(&mut payload, backup_name.as_bytes());
  push_field(&mut payload, public_key.as_bytes());
  push_field(&mut payload, nonce.as_bytes());
  payload.extend_from_slice(&timestamp_ms.to_le_bytes());
  payload.extend_from_slice(&deploy_timeout_secs.to_le_bytes());
  push_manifest(&mut payload, manifest);
  payload
}

/// Append a manifest to a signing payload.
///
/// Every field, in a fixed order, length-prefixed like the rest. The manifest
/// says where an archive lands and what runs around it, so leaving any part of
/// it unsigned would leave that part open to being rewritten in flight.
fn push_manifest(buf: &mut Vec<u8>, manifest: Option<&DeployManifest>) {
  let Some(manifest) = manifest else {
    // Distinguish "no manifest" from an empty one, so the two cannot collide.
    buf.push(0);
    return;
  };

  buf.push(1);
  push_field(buf, manifest.deploy_path.as_bytes());
  buf.push(u8::from(manifest.clean_deploy));
  buf.push(u8::from(manifest.backup_enabled));
  push_field(buf, manifest.backup_path.as_bytes());
  push_field(buf, manifest.before_deploy_script.as_bytes());
  push_field(buf, manifest.after_deploy_script.as_bytes());
}

/// A short, comparable name for a public key, in the style of SSH.
///
/// Printed by the client and shown in the server's pending list so an operator
/// can tell that the request they are approving came from the machine in front
/// of them, rather than whoever reached the queue first.
pub fn fingerprint(public_key: &str) -> String {
  let digest = Sha256::digest(public_key.trim().as_bytes());
  format!(
    "SHA256:{}",
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
  )
}

fn push_field(buf: &mut Vec<u8>, value: &[u8]) {
  buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
  buf.extend_from_slice(value);
}

impl Default for Auth {
  fn default() -> Self {
    Self::new()
  }
}
