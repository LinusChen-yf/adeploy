use thiserror::Error;

#[derive(Error, Debug)]
pub enum AdeployError {
  #[error("Configuration error: {0}")]
  Config(String),

  #[error("Network error: {0}")]
  Network(String),

  #[error("Authentication error: {0}")]
  Auth(String),

  #[error("Deploy error: {0}")]
  Deploy(String),

  #[error("File system error: {0}")]
  FileSystem(String),

  #[error("Service management error: {0}")]
  Service(String),

  #[error(
    "gRPC error (code: {code:?}, message: {message})",
    code = .0.code(),
    message = .0.message()
  )]
  Grpc(#[from] tonic::Status),

  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),

  #[error("TOML parsing error: {0}")]
  Toml(#[from] toml::de::Error),
}

// `Send` and `Sync` used to be asserted here by hand. Every variant already
// satisfies both, so the impls bought nothing and cost the compiler's check: a
// variant holding something that is not thread-safe would have kept compiling.

// Support converting std::io::Error into Box<AdeployError>
impl From<std::io::Error> for Box<AdeployError> {
  fn from(error: std::io::Error) -> Self {
    Box::new(AdeployError::Io(error))
  }
}

// TOML errors flow via the #[from] attribute

pub type Result<T> = std::result::Result<T, Box<AdeployError>>;
