use thiserror::Error;

#[derive(Error, Debug)]
pub enum AdeployError {
  #[error("Configuration error: {0}")]
  Config(String),

  #[error("Network error: {0}")]
  Network(String),

  #[error("Authentication error: {0}")]
  #[allow(dead_code)]
  Auth(String),

  #[error("Deploy error: {0}")]
  Deploy(String),

  #[error("File system error: {0}")]
  FileSystem(String),

  #[error("gRPC error: {0}")]
  Grpc(#[from] tonic::Status),

  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),

  #[error("TOML parsing error: {0}")]
  Toml(#[from] toml::de::Error),

  #[error("Serialization error: {0}")]
  Serde(#[from] serde_json::Error),
}

// Implement Send and Sync for AdeployError
unsafe impl Send for AdeployError {}
unsafe impl Sync for AdeployError {}

// TOML error handling is automatically implemented via #[from] attribute

pub type Result<T> = std::result::Result<T, AdeployError>;
