use std::path::Path;

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use log2::*;
use rand::rngs::OsRng;

use crate::error::{AdeployError, Result};

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

    // Write private key
    std::fs::write(private_key_path, signing_key.to_bytes()).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to write private key: {}",
        e
      )))
    })?;

    // Write public key as base64
    let verifying_key = signing_key.verifying_key();
    let public_key_str = base64::engine::general_purpose::STANDARD.encode(verifying_key.to_bytes());
    std::fs::write(public_key_path, public_key_str).map_err(|e| {
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

impl Default for Auth {
  fn default() -> Self {
    Self::new()
  }
}
