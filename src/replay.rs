use std::{
  collections::HashMap,
  sync::Mutex,
  time::{SystemTime, UNIX_EPOCH},
};

/// Why a `DeployStart` was refused before any archive was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayRejection {
  /// Older than the acceptance window: a captured request stops working.
  Stale,
  /// Dated further ahead than clock skew allows.
  Skewed,
  /// This nonce was already used inside the window.
  Replayed,
}

impl ReplayRejection {
  pub fn message(self) -> &'static str {
    match self {
      Self::Stale => "Request timestamp is outside the acceptance window",
      Self::Skewed => "Request timestamp is too far in the future; check the clocks",
      Self::Replayed => "Request nonce has already been used",
    }
  }
}

/// Remembers the nonces seen recently so a captured request cannot be resent.
///
/// A nonce alone would have to be remembered forever to be sound, so requests
/// also carry a timestamp: the guard only accepts a bounded window, which is
/// exactly the set of nonces it has to keep.
pub struct ReplayGuard {
  window_ms: i64,
  skew_ms: i64,
  max_entries: usize,
  seen: Mutex<HashMap<String, i64>>,
}

impl ReplayGuard {
  /// Window within which a signed request is accepted, in seconds.
  pub const DEFAULT_WINDOW_SECS: i64 = 300;
  /// Tolerance for a client clock running ahead of the server's, in seconds.
  pub const DEFAULT_SKEW_SECS: i64 = 60;
  /// Ceiling on remembered nonces. Only signature-verified, allow-listed
  /// clients ever reach the guard, so this is a memory bound rather than a
  /// defence; the oldest entries are dropped if it is ever hit.
  const MAX_ENTRIES: usize = 10_000;

  pub fn new() -> Self {
    Self {
      window_ms: Self::DEFAULT_WINDOW_SECS * 1000,
      skew_ms: Self::DEFAULT_SKEW_SECS * 1000,
      max_entries: Self::MAX_ENTRIES,
      seen: Mutex::new(HashMap::new()),
    }
  }

  /// Accept `nonce` once, judged against `now_ms`.
  pub fn admit_at(
    &self,
    nonce: &str,
    timestamp_ms: i64,
    now_ms: i64,
  ) -> std::result::Result<(), ReplayRejection> {
    if now_ms.saturating_sub(timestamp_ms) > self.window_ms {
      return Err(ReplayRejection::Stale);
    }
    if timestamp_ms.saturating_sub(now_ms) > self.skew_ms {
      return Err(ReplayRejection::Skewed);
    }

    let mut seen = self
      .seen
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Anything older than the window would be refused above anyway, so it no
    // longer needs remembering.
    let cutoff = now_ms.saturating_sub(self.window_ms);
    seen.retain(|_, seen_at| *seen_at >= cutoff);

    if seen.contains_key(nonce) {
      return Err(ReplayRejection::Replayed);
    }

    if seen.len() >= self.max_entries {
      if let Some(oldest) = seen
        .iter()
        .min_by_key(|(_, seen_at)| **seen_at)
        .map(|(key, _)| key.clone())
      {
        seen.remove(&oldest);
      }
    }

    seen.insert(nonce.to_string(), timestamp_ms);
    Ok(())
  }

  /// Accept `nonce` once, judged against the current clock.
  pub fn admit(&self, nonce: &str, timestamp_ms: i64) -> std::result::Result<(), ReplayRejection> {
    self.admit_at(nonce, timestamp_ms, now_ms())
  }
}

impl Default for ReplayGuard {
  fn default() -> Self {
    Self::new()
  }
}

/// Current time in Unix milliseconds.
pub fn now_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|elapsed| elapsed.as_millis() as i64)
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use super::*;

  const NOW: i64 = 1_700_000_000_000;

  #[test]
  fn a_fresh_nonce_is_accepted_once() {
    let guard = ReplayGuard::new();

    assert_eq!(guard.admit_at("abc", NOW, NOW), Ok(()));
    assert_eq!(
      guard.admit_at("abc", NOW, NOW),
      Err(ReplayRejection::Replayed),
      "the same nonce must not be usable twice"
    );
  }

  #[test]
  fn distinct_nonces_do_not_collide() {
    let guard = ReplayGuard::new();

    assert_eq!(guard.admit_at("one", NOW, NOW), Ok(()));
    assert_eq!(guard.admit_at("two", NOW, NOW), Ok(()));
  }

  #[test]
  fn a_captured_request_stops_working_after_the_window() {
    let guard = ReplayGuard::new();
    let later = NOW + (ReplayGuard::DEFAULT_WINDOW_SECS + 1) * 1000;

    assert_eq!(
      guard.admit_at("abc", NOW, later),
      Err(ReplayRejection::Stale)
    );
  }

  #[test]
  fn a_timestamp_beyond_skew_tolerance_is_refused() {
    let guard = ReplayGuard::new();
    let ahead = NOW + (ReplayGuard::DEFAULT_SKEW_SECS + 10) * 1000;

    assert_eq!(
      guard.admit_at("abc", ahead, NOW),
      Err(ReplayRejection::Skewed)
    );
  }

  #[test]
  fn modest_clock_skew_is_tolerated() {
    let guard = ReplayGuard::new();
    let slightly_ahead = NOW + (ReplayGuard::DEFAULT_SKEW_SECS - 10) * 1000;

    assert_eq!(guard.admit_at("abc", slightly_ahead, NOW), Ok(()));
  }

  #[test]
  fn nonces_are_forgotten_once_they_leave_the_window() {
    let guard = ReplayGuard::new();
    assert_eq!(guard.admit_at("abc", NOW, NOW), Ok(()));

    // Well past the window, so the entry is pruned rather than kept forever.
    let later = NOW + (ReplayGuard::DEFAULT_WINDOW_SECS + 5) * 1000;
    let _ = guard.admit_at("other", later, later);

    let seen = guard.seen.lock().expect("lock");
    assert!(
      !seen.contains_key("abc"),
      "entries outside the window must not accumulate"
    );
  }
}
