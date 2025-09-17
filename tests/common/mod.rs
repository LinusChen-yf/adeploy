//! Common test utilities and helpers
use tempfile::TempDir;
use tokio::net::TcpListener;


/// Create a temporary directory for testing
pub fn create_temp_dir() -> TempDir {
  tempfile::tempdir().expect("Failed to create temp directory")
}

/// Find an available port for testing
pub async fn find_available_port() -> u16 {
  let listener = TcpListener::bind("127.0.0.1:0")
    .await
    .expect("Failed to bind to port");
  let addr = listener.local_addr().expect("Failed to get local address");
  addr.port()
}
