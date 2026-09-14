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

/// Carries progress out of a deployment while it is still running.
///
/// The server used to collect every entry into a `Vec` and return it with the
/// final response, so a deployment that spent minutes inside an installer
/// looked identical to one that had hung. Entries now leave through this sink
/// as they are produced and reach the client immediately.
#[derive(Clone)]
pub struct LogSink(tokio::sync::mpsc::Sender<DeployLogEntry>);

impl LogSink {
  pub fn new(sender: tokio::sync::mpsc::Sender<DeployLogEntry>) -> Self {
    Self(sender)
  }

  /// Send one entry, ignoring a receiver that has already gone away.
  ///
  /// A client that disconnected mid-deployment is not a reason to fail the
  /// deployment itself, and the server writes the same lines to its own log.
  pub async fn send(&self, entry: DeployLogEntry) {
    let _ = self.0.send(entry).await;
  }

  pub async fn info(&self, message: impl Into<String>) {
    self.send(DeployLogEntry::info(message)).await;
  }

  pub async fn warn(&self, message: impl Into<String>) {
    self.send(DeployLogEntry::warn(message)).await;
  }
}
