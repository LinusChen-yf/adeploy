//! Common test utilities and helpers

use std::{collections::BTreeSet, path::Path, sync::Mutex};

use tempfile::TempDir;
use tokio::net::TcpListener;

/// Create a temporary directory for testing
#[allow(dead_code)]
pub fn create_temp_dir() -> TempDir {
  tempfile::tempdir().expect("Failed to create temp directory")
}

/// Find a port no other test in this run will also be handed.
///
/// Binding `:0` only reserves a port until the listener is dropped, and the
/// server binds its own afterwards. Two test binaries running in parallel could
/// therefore both be handed the same port, which showed up as an occasional
/// failure with no other explanation. Starting from a base derived from this
/// process and remembering what it has already given out keeps the binaries out
/// of each other's way.
#[allow(dead_code)]
pub async fn find_available_port() -> u16 {
  static HANDED_OUT: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

  let base = 20_000 + (std::process::id() % 20_000) as u16;

  for offset in 0..1_000 {
    let candidate = base + offset;

    // Claim the number before testing it. Checking the set and inserting after
    // the bind leaves a gap on either side for another test to claim the same
    // port, which is how two of them ended up talking to each other's servers.
    let claimed = HANDED_OUT.lock().expect("port registry").insert(candidate);
    if !claimed {
      continue;
    }

    if TcpListener::bind(("127.0.0.1", candidate)).await.is_ok() {
      return candidate;
    }
  }

  panic!("No free port found starting from {base}");
}

/// Escape Windows backslashes so TOML paths parse correctly across platforms.
#[allow(dead_code)]
pub fn toml_escape_path(path: &Path) -> String {
  path.to_string_lossy().replace('\\', "\\\\")
}
