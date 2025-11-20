//! Structured deployment log types used between the server and client.

#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
  Info,
  Warn,
  Error,
}

#[derive(Clone, Debug)]
pub struct DeployLogEntry {
  pub level: LogLevel,
  pub message: String,
}

impl DeployLogEntry {
  pub fn new(level: LogLevel, message: impl Into<String>) -> Self {
    Self {
      level,
      message: message.into(),
    }
  }

  pub fn info(message: impl Into<String>) -> Self {
    Self::new(LogLevel::Info, message)
  }

  pub fn warn(message: impl Into<String>) -> Self {
    Self::new(LogLevel::Warn, message)
  }

  pub fn error(message: impl Into<String>) -> Self {
    Self::new(LogLevel::Error, message)
  }
}
