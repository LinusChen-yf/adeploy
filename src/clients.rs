//! The one place an operator manages who may deploy.
//!
//! This replaced five subcommands - `pending`, `approve`, `reject`, `keys`,
//! `revoke` - which were five things to remember for one job, and which each
//! showed a slice of the picture: who is waiting and who is trusted had to be
//! read from separate commands, then acted on with a third that took a
//! selector copied between them.
//!
//! Here both lists are on screen at once and the action applies to the row in
//! front of you, so the fingerprint being compared and the client being
//! approved are visibly the same one.

use std::{
  io::{self, IsTerminal, Write},
  path::Path,
};

use chrono::{DateTime, Utc};

use crate::{
  auth::fingerprint,
  error::Result,
  pairing::{PairStore, PairedClient},
};

/// Which list a row came from, and therefore what can be done to it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Membership {
  Waiting,
  Trusted,
  Refused,
  /// Listed in `allowed_keys` in the server's `adeploy.toml`.
  Configured,
}

impl Membership {
  fn heading(self) -> &'static str {
    match self {
      Membership::Waiting => "Waiting for approval",
      Membership::Trusted | Membership::Configured => "Trusted",
      Membership::Refused => "Refused",
    }
  }
}

/// One numbered line, and enough to act on it.
struct Row {
  membership: Membership,
  fingerprint: String,
  /// Who and from where, or a note for a key that has only ever been config.
  description: String,
  /// Position within its own list, which is what the store's selectors take.
  position: usize,
  when: Option<DateTime<Utc>>,
}

/// Show the lists, and act on them until the operator is done.
///
/// `allowed_keys` is read separately from the store because it lives in the
/// operator's own `adeploy.toml`: it is shown so the picture is complete, but
/// not edited here, since rewriting a hand-edited file would cost them their
/// comments and layout.
pub fn browse(store_path: &Path, allowed_keys: &[String]) -> Result<()> {
  let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();

  loop {
    // Reloaded every pass: the server queues new requests into this same file
    // while this is open, and with `adeploy pair` now holding until somebody
    // decides, a request arriving mid-session is the expected case.
    let store = PairStore::load(store_path)?;
    let rows = collect(&store, allowed_keys);
    render(&rows);

    if !interactive {
      return Ok(());
    }

    let Some(choice) = prompt("Pick a number, [r]efresh, [q]uit: ")? else {
      return Ok(());
    };
    match choice.as_str() {
      "" | "r" | "R" => continue,
      "q" | "Q" => return Ok(()),
      other => match other.parse::<usize>().ok().and_then(|n| rows.get(n - 1)) {
        Some(row) => act(store_path, row)?,
        None => println!("  No row {other}.\n"),
      },
    }
  }
}

/// Flatten the three lists plus `allowed_keys` into one numbered sequence.
///
/// One numbering across every section is what lets a row be picked without
/// first saying which list it is in - the thing the old `approve 1` /
/// `revoke 1` pair got wrong, where the same "1" meant two different clients.
fn collect(store: &PairStore, allowed_keys: &[String]) -> Vec<Row> {
  let mut rows = Vec::new();

  for (position, client) in store.pending.iter().enumerate() {
    rows.push(row_for(
      client,
      Membership::Waiting,
      position,
      client.first_seen,
    ));
  }
  for (position, client) in store.approved.iter().enumerate() {
    rows.push(row_for(
      client,
      Membership::Trusted,
      position,
      client.decided_at.unwrap_or(client.first_seen),
    ));
  }
  for key in allowed_keys {
    // A key that is also in the store would otherwise appear twice; the store
    // entry is the more informative of the two, so that is the one kept.
    let print = fingerprint(key.trim());
    if rows.iter().any(|row| row.fingerprint == print) {
      continue;
    }
    rows.push(Row {
      membership: Membership::Configured,
      fingerprint: print,
      description: "listed in allowed_keys".to_string(),
      position: 0,
      when: None,
    });
  }
  for (position, client) in store.rejected.iter().enumerate() {
    rows.push(row_for(
      client,
      Membership::Refused,
      position,
      client.decided_at.unwrap_or(client.first_seen),
    ));
  }

  rows
}

fn row_for(
  client: &PairedClient,
  membership: Membership,
  position: usize,
  when: DateTime<Utc>,
) -> Row {
  let description = match &client.client_address {
    Some(address) => format!("{}  {}", client.client_name, address),
    None => client.client_name.clone(),
  };
  Row {
    membership,
    fingerprint: client.fingerprint.clone(),
    description,
    position,
    when: Some(when),
  }
}

fn render(rows: &[Row]) {
  println!();
  if rows.is_empty() {
    println!("  Nobody has paired with this server yet.");
    println!("  Run `adeploy pair <this server>` on a client to start.\n");
    return;
  }

  let mut heading = None;
  for (index, row) in rows.iter().enumerate() {
    if heading != Some(row.membership.heading()) {
      heading = Some(row.membership.heading());
      println!("{}", row.membership.heading());
    }
    println!(
      "  {:>2}  {:<38}  {}{}",
      index + 1,
      row.description,
      row.fingerprint,
      row
        .when
        .map(|when| format!("   {}", ago(when)))
        .unwrap_or_default()
    );
  }
  println!();
}

/// Offer only what applies to this row, so no verb has to be matched to a list.
fn act(store_path: &Path, row: &Row) -> Result<()> {
  println!();
  println!("  {}", row.description);
  println!("  {}", row.fingerprint);

  let answer = match row.membership {
    Membership::Waiting => {
      println!("  Compare that fingerprint with the one printed on the client itself.");
      prompt("  [a]pprove, [r]eject, [Enter] to go back: ")?
    }
    Membership::Trusted => prompt("  [r]evoke trust, [Enter] to go back: ")?,
    Membership::Refused => {
      println!("  A refused client is turned away without being queued again.");
      prompt("  [f]orget the refusal so it may ask again, [Enter] to go back: ")?
    }
    Membership::Configured => {
      println!("  This key is in `allowed_keys` in the server's adeploy.toml.");
      println!("  Remove it there; nothing here rewrites that file.\n");
      return Ok(());
    }
  };

  // End of input here means the same as backing out.
  let Some(answer) = answer else {
    return Ok(());
  };

  // Loaded again immediately before writing, rather than reusing what was
  // rendered: the server may have queued a request in between, and saving a
  // copy read earlier would drop it.
  let selector = (row.position + 1).to_string();
  let mut store = PairStore::load(store_path)?;
  let outcome = match (row.membership, answer.as_str()) {
    (Membership::Waiting, "a" | "A") => store.approve(&selector).map(|client| {
      format!(
        "Approved {}. It can deploy now - the server picks this up without a restart.",
        client.client_name
      )
    }),
    (Membership::Waiting, "r" | "R") => store
      .reject(&selector)
      .map(|client| format!("Refused {}.", client.client_name)),
    (Membership::Trusted, "r" | "R") => store.revoke(&selector).map(|client| {
      format!(
        "Withdrew trust from {}. It may pair again.",
        client.client_name
      )
    }),
    (Membership::Refused, "f" | "F") => store.forget(&selector).map(|client| {
      format!(
        "Forgot the refusal of {}. It may ask again.",
        client.client_name
      )
    }),
    _ => {
      println!();
      return Ok(());
    }
  };

  match outcome {
    Ok(message) => {
      store.save(store_path)?;
      println!("  {message}\n");
    }
    // A stale row, which means the file moved under us - the next pass will
    // show what is actually there now.
    Err(error) => println!("  {error}\n"),
  }
  Ok(())
}

fn prompt(question: &str) -> Result<Option<String>> {
  print!("{question}");
  io::stdout().flush().ok();

  let mut line = String::new();
  // Zero bytes is end of input, which is the operator closing the session -
  // the same intent as `q`, and not something to keep prompting about.
  if io::stdin().read_line(&mut line)? == 0 {
    println!();
    return Ok(None);
  }
  Ok(Some(line.trim().to_string()))
}

/// A rough age, in the unit a person would say it in.
fn ago(when: DateTime<Utc>) -> String {
  let seconds = Utc::now().signed_duration_since(when).num_seconds().max(0);
  match seconds {
    0..=59 => "just now".to_string(),
    60..=3599 => format!("{}m ago", seconds / 60),
    3600..=86399 => format!("{}h ago", seconds / 3600),
    _ => format!("{}d ago", seconds / 86400),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn store_with_one_of_each() -> PairStore {
    let mut store = PairStore::default();
    store.request("waiting-key", "laptop", Some("10.0.0.2:5000".into()));
    store.request("trusted-key", "build-box", None);
    store.approve("2").expect("approve");
    store.request("refused-key", "stranger", None);
    store.reject("2").expect("reject");
    store
  }

  #[test]
  fn every_list_is_numbered_once_across_the_whole_view() {
    let store = store_with_one_of_each();
    let rows = collect(&store, &[]);

    assert_eq!(rows.len(), 3, "one row per client, whichever list it is in");
    // The position carried by each row is its index in its own list, which is
    // what the store's selectors resolve against - not the number on screen.
    assert_eq!(rows[0].position, 0);
    assert_eq!(rows[1].position, 0);
    assert_eq!(rows[2].position, 0);
  }

  #[test]
  fn a_configured_key_that_is_also_approved_is_not_listed_twice() {
    let store = store_with_one_of_each();
    let already_trusted = "trusted-key".to_string();

    let rows = collect(&store, std::slice::from_ref(&already_trusted));

    let trusted = fingerprint(&already_trusted);
    assert_eq!(
      rows.iter().filter(|row| row.fingerprint == trusted).count(),
      1,
      "the same key in both places is one client, not two"
    );
  }

  #[test]
  fn a_configured_key_that_is_not_in_the_store_is_still_shown() {
    let store = store_with_one_of_each();
    let config_only = "config-only-key".to_string();

    let rows = collect(&store, std::slice::from_ref(&config_only));

    let row = rows
      .iter()
      .find(|row| row.fingerprint == fingerprint(&config_only))
      .expect("a key that can deploy must appear, wherever it is configured");
    assert!(
      matches!(row.membership, Membership::Configured),
      "and must be marked as coming from the config, since it cannot be edited here"
    );
  }
}
