//! Client-side half of the platform's internal mutual TLS.
//!
//! The internal gRPC servers (ais_auth, ais_secretserver, RunpodManager,
//! keystore, domain_management, Billing) *require* a client certificate signed by
//! the internal CA. This is the matching helper for anything that dials them.
//!
//! This is a vendored file -- the same text lives in every crate that dials an
//! mTLS server, exactly like the `.proto` files did before `ais_proto`, because
//! `artisan_middleware`'s `mtls` module is deliberately tonic-free (each crate
//! pins its own tonic). Keep the copies identical; the canonical source is
//! `ais_proto/mtls_client.rs.tmpl`.
//!
//! Behaviour is chosen by the address scheme, so one binary works in both
//! worlds without a flag:
//!   * `https://host:port` -> mutual TLS. Requires client certificate material;
//!     the server's certificate is verified against the internal CA and
//!     against `server_name` (the leaf certs carry a DNS SAN equal to the
//!     service name the CA tool was given, e.g. `ais_auth`).
//!   * `http://host:port`  -> plaintext. Local development against a server
//!     that is itself running without TLS only; a production mTLS server will
//!     refuse it.
//!
//! No `tracing`/`log` dependency on purpose: errors are returned as `String`
//! for the caller to log with whatever it already uses.

// Vendored into several crates, each of which uses only part of it.
#![allow(dead_code)]

use std::path::PathBuf;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// This process's own client identity plus the CA it trusts.
#[derive(Clone)]
pub struct ClientMtls {
    identity: Identity,
    ca: Certificate,
}

impl ClientMtls {
    /// Loads `MTLS_CERT_PATH` / `MTLS_KEY_PATH` / `MTLS_CA_PATH`, defaulting to
    /// `/etc/artisan/tls/<own_service>.{crt,key}` and `/etc/artisan/tls/ca.crt`
    /// -- the same variables and layout the servers already use, so a process
    /// that is both a server and a client (ais_secretserver, RunpodManager)
    /// reuses one certificate for both directions.
    pub fn load(own_service: &str) -> Result<Self, String> {
        let dir = PathBuf::from("/etc/artisan/tls");
        let path = |var: &str, default: PathBuf| {
            std::env::var(var).map(PathBuf::from).unwrap_or(default)
        };
        let cert_path = path("MTLS_CERT_PATH", dir.join(format!("{own_service}.crt")));
        let key_path = path("MTLS_KEY_PATH", dir.join(format!("{own_service}.key")));
        let ca_path = path("MTLS_CA_PATH", dir.join("ca.crt"));

        let read = |p: &PathBuf, what: &str| {
            std::fs::read(p).map_err(|e| format!("failed to read {what} at {}: {e}", p.display()))
        };
        Ok(Self {
            identity: Identity::from_pem(read(&cert_path, "client cert")?, read(&key_path, "client key")?),
            ca: Certificate::from_pem(read(&ca_path, "CA cert")?),
        })
    }
}

/// True if `addr` asks for TLS. Lets a caller skip loading certificates at all
/// when it was pointed at a plaintext dev server.
pub fn wants_tls(addr: &str) -> bool {
    addr.starts_with("https://")
}

fn endpoint(addr: &str, server_name: &str, mtls: Option<&ClientMtls>) -> Result<Endpoint, String> {
    let endpoint = Channel::from_shared(addr.to_owned())
        .map_err(|e| format!("invalid gRPC address {addr:?}: {e}"))?;

    if !wants_tls(addr) {
        return Ok(endpoint);
    }

    let mtls = mtls.ok_or_else(|| {
        format!("{addr} is an https:// address but no client certificate was loaded")
    })?;
    let tls = ClientTlsConfig::new()
        .domain_name(server_name.to_owned())
        .ca_certificate(mtls.ca.clone())
        .identity(mtls.identity.clone());

    endpoint
        .tls_config(tls)
        .map_err(|e| format!("invalid TLS config for {addr}: {e}"))
}

/// Builds a lazily-connecting channel to an internal service.
///
/// Must be called from inside a Tokio runtime (`connect_lazy` registers with
/// the reactor). It performs no I/O -- a bad certificate or unreachable server
/// surfaces on the first RPC, as a normal `Status`, never as a panic here.
pub fn internal_channel(
    addr: &str,
    server_name: &str,
    mtls: Option<&ClientMtls>,
) -> Result<Channel, String> {
    Ok(endpoint(addr, server_name, mtls)?.connect_lazy())
}

/// Like [`internal_channel`] but dials now, so an unreachable server or a
/// failed handshake is reported here. For callers that want to fail fast and
/// retry later (a periodic sync loop) rather than discover it per-RPC.
pub async fn connect_internal(
    addr: &str,
    server_name: &str,
    mtls: Option<&ClientMtls>,
) -> Result<Channel, String> {
    endpoint(addr, server_name, mtls)?
        .connect()
        .await
        .map_err(|e| format!("connecting to {addr}: {e}"))
}

/// Loads this process's service credential -- the bearer secret ais_auth issued
/// it via `IssueServiceCredential`, presented to ais_secretserver /
/// RunpodManager in the `service_credential` field.
///
/// Read from the file named by `AIS_SERVICE_CREDENTIAL_FILE` if set (preferred:
/// a `0600` file keeps it out of `/proc/<pid>/environ` and `ps e`), otherwise
/// from `AIS_SERVICE_CREDENTIAL`. Surrounding whitespace is trimmed. It is a
/// secret: callers must not log it, and this crate never does.
pub fn load_service_credential() -> Result<String, String> {
    let raw = match std::env::var("AIS_SERVICE_CREDENTIAL_FILE") {
        Ok(path) => std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read AIS_SERVICE_CREDENTIAL_FILE {path}: {e}"))?,
        Err(_) => std::env::var("AIS_SERVICE_CREDENTIAL").map_err(|_| {
            "no service credential: set AIS_SERVICE_CREDENTIAL_FILE (or AIS_SERVICE_CREDENTIAL)"
                .to_owned()
        })?,
    };
    let credential = raw.trim().to_owned();
    if credential.is_empty() {
        return Err("the configured service credential is empty".to_owned());
    }
    Ok(credential)
}
