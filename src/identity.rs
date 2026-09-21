//! The server's TLS identity, and how a client learns it.
//!
//! Signatures already prove who is asking and bind a request to the archive it
//! describes, so nothing here is about authorising a deployment. What it adds
//! is the other half: the payload stops travelling in the clear, and the client
//! learns which server it is talking to rather than trusting whichever machine
//! answered on that address.
//!
//! No certificate authority is involved. Both machines belong to the same
//! person, so the trust root is simply the certificate itself, recorded the
//! first time and compared every time after - what SSH does with host keys.

use std::{fs, path::Path, sync::Arc, time::Duration};

use log2::*;
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
  pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime},
  ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};

use crate::error::{AdeployError, Result};

/// The name a server's certificate is issued for, and the name a client
/// verifies it against.
///
/// Deliberately not a host name or an address. The server cannot know which of
/// its addresses a client will dial, and an address that ends up in a
/// certificate is one that cannot change without reissuing it. The connection
/// still goes to whatever host the command names; only the name inside the
/// certificate is fixed, and what actually decides trust is the recorded
/// certificate itself.
pub const SERVER_TLS_NAME: &str = "adeploy";

/// Files the server keeps beside its configuration.
pub const CERTIFICATE_FILE_NAME: &str = "server.crt";
pub const PRIVATE_KEY_FILE_NAME: &str = "server.key";

/// A server's certificate and the key that goes with it.
pub struct ServerIdentity {
  pub certificate_pem: String,
  pub key_pem: String,
  pub fingerprint: String,
}

/// Load the server's identity, creating one if this is the first run.
///
/// Generated rather than asked for, the way the configuration and the client's
/// signing key already are: a certificate an operator has to produce by hand is
/// a step that gets skipped, and a deployment tool that is awkward to secure
/// ends up running without it.
pub fn ensure_server_identity(directory: &Path) -> Result<(ServerIdentity, bool)> {
  let certificate_path = directory.join(CERTIFICATE_FILE_NAME);
  let key_path = directory.join(PRIVATE_KEY_FILE_NAME);

  if certificate_path.exists() && key_path.exists() {
    let certificate_pem = read_file(&certificate_path)?;
    let key_pem = read_file(&key_path)?;
    let fingerprint = fingerprint_of_pem(&certificate_pem)?;
    return Ok((
      ServerIdentity {
        certificate_pem,
        key_pem,
        fingerprint,
      },
      false,
    ));
  }

  fs::create_dir_all(directory).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to create {}: {}",
      directory.display(),
      e
    )))
  })?;

  let issued =
    rcgen::generate_simple_self_signed(vec![SERVER_TLS_NAME.to_string()]).map_err(|e| {
      Box::new(AdeployError::Auth(format!(
        "Failed to generate a certificate: {e}"
      )))
    })?;
  let certificate_pem = issued.cert.pem();
  let key_pem = issued.signing_key.serialize_pem();
  let fingerprint = fingerprint_of_der(issued.cert.der());

  write_private(&key_path, &key_pem)?;
  fs::write(&certificate_path, &certificate_pem).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to write {}: {}",
      certificate_path.display(),
      e
    )))
  })?;

  info!("Generated {}", certificate_path.display());
  Ok((
    ServerIdentity {
      certificate_pem,
      key_pem,
      fingerprint,
    },
    true,
  ))
}

/// What a server presented, before anything has decided to trust it.
#[derive(Debug)]
pub struct PresentedCertificate {
  pub certificate_pem: String,
  pub fingerprint: String,
}

/// Run `operation`, giving up after `limit` with a message of its own.
///
/// The error says what timed out rather than surfacing a bare Elapsed, because
/// "timed out reaching the host" and "the handshake never finished" point at
/// completely different things to go and check.
async fn within<T>(
  limit: Option<Duration>,
  operation: impl std::future::Future<Output = T>,
  describe: impl FnOnce() -> String,
) -> Result<T> {
  match limit {
    Some(limit) => tokio::time::timeout(limit, operation)
      .await
      .map_err(|_| Box::new(AdeployError::Network(describe()))),
    None => Ok(operation.await),
  }
}

/// What to go and check when nothing answers at all.
///
/// A timeout means the packets may not even be arriving, which is a different
/// search from a refusal - so it names the things that silently drop them.
fn unreachable_advice(host: &str, port: u16, waited: Duration) -> String {
  format!(
    "Timed out reaching {host}:{port} after {}s. Nothing answered, so check:\n  \
     - is `adeploy server` running on that machine?\n  \
     - does its firewall allow inbound TCP {port}? Windows blocks this by \
     default when the network is set to \"Public\"\n  \
     - is {port} the port it listens on (`listen_port` on the server)?",
    waited.as_secs()
  )
}

/// What to go and check when the machine is up and said no.
///
/// A refusal is good news by comparison: it proves the host is reachable and
/// the firewall is not in the way, leaving only what is or is not listening.
fn refused_advice(host: &str, port: u16, error: &std::io::Error) -> String {
  let mut message = format!("Failed to reach {host}:{port}: {error}");
  if error.kind() == std::io::ErrorKind::ConnectionRefused {
    message.push_str(&format!(
      "\n  The machine is up and refused the connection, so the network is \
       fine and nothing is listening on {port}:\n  \
       - is `adeploy server` running?\n  \
       - does its `listen_port` match the port this project is using?"
    ));
  }
  message
}

/// What to go and check when the connection is accepted and then goes quiet.
fn handshake_timeout_advice(host: &str, port: u16, waited: Duration) -> String {
  format!(
    "TLS handshake with {host}:{port} timed out after {}s. The connection was \
     accepted and then nothing came back, which is exactly what a server \
     running without TLS looks like:\n  \
     - does that server have `tls = true`? This client is using TLS\n  \
     - is something other than adeploy listening on {port}?",
    waited.as_secs()
  )
}

/// Ask `host` what certificate it serves, without trusting the answer.
///
/// The one connection that cannot verify the server, for the same reason
/// pairing is the one method that cannot require a key: it is what establishes
/// the thing everything else checks against. The certificate is recorded and
/// its fingerprint printed for a human to compare, exactly as the client's own
/// key fingerprint already is - and nothing is sent over this connection, which
/// is closed as soon as the certificate is in hand.
///
/// `connect_timeout` bounds both reaching the host and the handshake. Without
/// it a dropped SYN - a firewall, a machine that is not there - leaves this
/// waiting for the operating system to give up, which on Linux is over two
/// minutes and looks from the outside like the command having silently stopped.
pub async fn fetch_server_certificate(
  host: &str,
  port: u16,
  connect_timeout: Option<Duration>,
) -> Result<PresentedCertificate> {
  let provider = Arc::new(rustls::crypto::ring::default_provider());
  let mut config = ClientConfig::builder_with_provider(provider.clone())
    .with_safe_default_protocol_versions()
    .map_err(|e| Box::new(AdeployError::Network(format!("TLS setup failed: {e}"))))?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(RecordWithoutVerifying(provider)))
    .with_no_client_auth();
  config.alpn_protocols = vec![b"h2".to_vec()];

  let stream = within(
    connect_timeout,
    tokio::net::TcpStream::connect((host, port)),
    || unreachable_advice(host, port, connect_timeout.unwrap_or_default()),
  )
  .await?
  .map_err(|e| Box::new(AdeployError::Network(refused_advice(host, port, &e))))?;

  let name = ServerName::try_from(SERVER_TLS_NAME)
    .map_err(|e| Box::new(AdeployError::Network(format!("Invalid TLS name: {e}"))))?
    .to_owned();
  let session = within(
    connect_timeout,
    tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, stream),
    || handshake_timeout_advice(host, port, connect_timeout.unwrap_or_default()),
  )
  .await?
  .map_err(|e| {
    Box::new(AdeployError::Network(format!(
      "TLS handshake with {host}:{port} failed: {e}. Is the server running with TLS enabled?"
    )))
  })?;

  let (_, connection) = session.get_ref();
  let presented = connection
    .peer_certificates()
    .and_then(|certificates| certificates.first())
    .ok_or_else(|| {
      Box::new(AdeployError::Network(
        "The server presented no certificate".to_string(),
      ))
    })?;

  Ok(PresentedCertificate {
    fingerprint: fingerprint_of_der(presented),
    certificate_pem: der_to_pem(presented),
  })
}

/// A short, comparable name for a certificate, in the same style as the key
/// fingerprints an operator already compares.
pub fn fingerprint_of_der(der: &[u8]) -> String {
  format!(
    "SHA256:{}",
    base64::engine::Engine::encode(
      &base64::engine::general_purpose::STANDARD_NO_PAD,
      Sha256::digest(der)
    )
  )
}

/// The same, for a certificate that is only on hand as PEM.
pub fn fingerprint_of_pem(pem: &str) -> Result<String> {
  let der = CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| {
    Box::new(AdeployError::Auth(format!(
      "Failed to read a certificate: {e}"
    )))
  })?;
  Ok(fingerprint_of_der(&der))
}

fn der_to_pem(der: &CertificateDer<'_>) -> String {
  let body =
    base64::engine::Engine::encode(&base64::engine::general_purpose::STANDARD, der.as_ref());
  let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
  for line in body.as_bytes().chunks(64) {
    pem.push_str(&String::from_utf8_lossy(line));
    pem.push('\n');
  }
  pem.push_str("-----END CERTIFICATE-----\n");
  pem
}

fn read_file(path: &Path) -> Result<String> {
  fs::read_to_string(path).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to read {}: {}",
      path.display(),
      e
    )))
  })
}

/// Write a private key, readable only by its owner where that can be expressed.
fn write_private(path: &Path, contents: &str) -> Result<()> {
  fs::write(path, contents).map_err(|e| {
    Box::new(AdeployError::FileSystem(format!(
      "Failed to write {}: {}",
      path.display(),
      e
    )))
  })?;

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| {
      Box::new(AdeployError::FileSystem(format!(
        "Failed to restrict {}: {}",
        path.display(),
        e
      )))
    })?;
  }

  Ok(())
}

/// Accepts whatever a server presents, so it can be recorded and shown.
///
/// Only ever used by `fetch_server_certificate`, on a connection that carries
/// no request. What makes the certificate trustworthy afterwards is a person
/// comparing its fingerprint with the one the server logged, which is the same
/// bargain the pairing queue already makes in the other direction.
#[derive(Debug)]
struct RecordWithoutVerifying(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for RecordWithoutVerifying {
  fn verify_server_cert(
    &self,
    _end_entity: &CertificateDer<'_>,
    _intermediates: &[CertificateDer<'_>],
    _server_name: &ServerName<'_>,
    _ocsp_response: &[u8],
    _now: UnixTime,
  ) -> std::result::Result<ServerCertVerified, rustls::Error> {
    Ok(ServerCertVerified::assertion())
  }

  fn verify_tls12_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls12_signature(
      message,
      cert,
      dss,
      &self.0.signature_verification_algorithms,
    )
  }

  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(
      message,
      cert,
      dss,
      &self.0.signature_verification_algorithms,
    )
  }

  fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
    self.0.signature_verification_algorithms.supported_schemes()
  }
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  #[test]
  fn an_identity_is_generated_once_and_then_reused() {
    let temp = TempDir::new().expect("temp dir");

    let (first, generated) = ensure_server_identity(temp.path()).expect("generate");
    assert!(generated, "a missing identity must be created");
    assert!(first.fingerprint.starts_with("SHA256:"));

    let (second, generated) = ensure_server_identity(temp.path()).expect("reuse");
    assert!(
      !generated,
      "an existing identity must not be reported as new"
    );
    assert_eq!(
      first.fingerprint, second.fingerprint,
      "a restart must not change who the server says it is"
    );
    assert_eq!(first.certificate_pem, second.certificate_pem);
  }

  #[test]
  fn the_fingerprint_is_the_same_whichever_form_it_is_taken_from() {
    // One is computed as the certificate is generated, the other after reading
    // it back from disk; a client compares the two.
    let temp = TempDir::new().expect("temp dir");
    let (identity, _) = ensure_server_identity(temp.path()).expect("generate");

    assert_eq!(
      identity.fingerprint,
      fingerprint_of_pem(&identity.certificate_pem).expect("from pem")
    );
  }

  #[test]
  fn two_servers_do_not_share_an_identity() {
    let one = TempDir::new().expect("temp dir");
    let two = TempDir::new().expect("temp dir");

    let (first, _) = ensure_server_identity(one.path()).expect("generate");
    let (second, _) = ensure_server_identity(two.path()).expect("generate");

    assert_ne!(first.fingerprint, second.fingerprint);
  }

  #[cfg(unix)]
  #[test]
  fn the_private_key_is_not_readable_by_anyone_else() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("temp dir");
    ensure_server_identity(temp.path()).expect("generate");

    let mode = fs::metadata(temp.path().join(PRIVATE_KEY_FILE_NAME))
      .expect("metadata")
      .permissions()
      .mode();
    assert_eq!(mode & 0o077, 0, "group and other must have no access");
  }

  #[tokio::test]
  async fn a_handshake_that_never_answers_gives_up() {
    // A server running without TLS accepts the connection and then says
    // nothing, which is indistinguishable from a healthy one until something
    // decides to stop waiting. Nothing did: connect_timeout was applied to the
    // gRPC channel and not to this probe, so `adeploy pair` against such a
    // server hung with no output past the fingerprint it had already printed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
      .await
      .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
      // Accept and hold, never speaking.
      let mut held = Vec::new();
      while let Ok((stream, _)) = listener.accept().await {
        held.push(stream);
      }
    });

    let started = std::time::Instant::now();
    let failure = fetch_server_certificate("127.0.0.1", port, Some(Duration::from_millis(300)))
      .await
      .expect_err("a silent peer must not be waited on forever");

    assert!(
      started.elapsed() < Duration::from_secs(5),
      "it gave up after {:?}, which is not the limit it was given",
      started.elapsed()
    );
    assert!(
      failure.to_string().contains("timed out"),
      "the error should say it timed out, got: {failure}"
    );
  }

  #[tokio::test]
  async fn no_limit_means_no_timeout_error() {
    // Nothing is listening, so this fails immediately - the point is that it
    // fails for the right reason rather than being reported as a timeout.
    let port = {
      let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
      probe.local_addr().expect("addr").port()
    };

    let failure = fetch_server_certificate("127.0.0.1", port, None)
      .await
      .expect_err("nothing is listening");

    assert!(
      failure.to_string().contains("Failed to reach"),
      "a refused connection is not a timeout, got: {failure}"
    );
  }
}
