//! `capscode-broker` — the secret-broker reverse proxy .
//!
//! The broker is an **explicit base-URL reverse proxy**, not a transparent MITM.
//! The AI tools are pointed at per-provider virtual hosts (`*.broker.capscode.local`)
//! via `*_BASE_URL`. For each request the broker:
//!   1. terminates TLS with a per-session, CA-signed leaf ([`SessionCa`]);
//!   2. routes by SNI/Host to the correct [`Provider`](capscode_core::Provider);
//!   3. enforces a **default-deny** upstream allowlist *before* any connection;
//!   4. strips inbound credentials and injects the real provider key;
//!   5. forwards the client's other headers unchanged while removing
//!      hop-by-hop/proxy headers and rewriting Host;
//!   6. re-originates TLS to the real provider with **full** verification and
//!      streams the (SSE-preserving) response back.
//!
//! Keys are wrapped in [`secrecy::SecretString`]; no key type is ever `Debug`ged
//! or logged, and the CA private key never leaves this process.

mod ca;
mod manifest;
mod proxy;
mod route;
mod upstream;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rustls::ServerConfig;
use secrecy::SecretString;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use capscode_core::BrokerError;

pub use ca::SessionCa;
pub use manifest::{ManifestError, ProviderManifest};

use proxy::Inner;
use route::Route;
use upstream::Upstream;

/// The secret broker: a bound-ready reverse proxy holding the capsule-side TLS
/// config, the upstream client, the allowlist, and the real provider keys.
pub struct Broker {
    inner: Arc<Inner>,
    server_config: Arc<ServerConfig>,
    ca_cert_pem: String,
}

/// Installs a process-default rustls crypto provider exactly once. Safe to call
/// repeatedly — a second install is a no-op (the `Err` is ignored).
fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

impl Broker {
    /// Build the broker from the parsed providers manifest, the resolved
    /// credentials (keyed by `key_env`), and a per-session CA. Constructs the
    /// capsule-facing TLS server config (leaf signed by `ca`) and the production
    /// upstream client (webpki roots, full verification).
    ///
    /// The route table and the default-deny allowlist are derived **only** from
    /// `manifest` — no wildcard, no capsule-controllable host.
    pub fn build(
        manifest: &ProviderManifest,
        creds: HashMap<String, SecretString>,
        ca: &SessionCa,
    ) -> Result<Broker, BrokerError> {
        Broker::build_inner(manifest, creds, Upstream::real(), ca)
    }

    /// Shared constructor for the production and test builds. The allowlist is
    /// collected from the manifest authorities here, identically in both, so a
    /// test can never widen the allowlist beyond what the manifest declares.
    fn build_inner(
        manifest: &ProviderManifest,
        creds: HashMap<String, SecretString>,
        upstream: Upstream,
        ca: &SessionCa,
    ) -> Result<Broker, BrokerError> {
        ensure_crypto_provider();

        let server_config = build_server_config(ca)?;

        let routes: HashMap<String, Route> = manifest.routes.clone();
        let allow_hosts: HashSet<String> = routes.values().map(|r| r.authority.clone()).collect();

        let inner = Inner {
            routes,
            allow_hosts,
            creds,
            upstream,
        };

        Ok(Broker {
            inner: Arc::new(inner),
            server_config: Arc::new(server_config),
            ca_cert_pem: ca.ca_cert_pem().to_string(),
        })
    }

    /// The CA certificate PEM to inject into the capsule trust store.
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Bind `listen` and serve until `shutdown` resolves. Each accepted TLS
    /// connection is served on its own task; the SNI is captured post-handshake
    /// and used for routing.
    pub async fn serve(
        self,
        listen: SocketAddr,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), BrokerError> {
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|e| BrokerError::Bind(listen.to_string(), e.to_string()))?;
        self.run(listener, shutdown).await
    }

    /// Accept loop over an already-bound listener.
    async fn run(
        self,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), BrokerError> {
        let acceptor = TlsAcceptor::from(self.server_config.clone());
        let inner = self.inner.clone();

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!("broker shutting down");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (tcp, _peer) = match accepted {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed");
                            continue;
                        }
                    };
                    let acceptor = acceptor.clone();
                    let inner = inner.clone();
                    tokio::spawn(async move {
                        serve_conn(acceptor, inner, tcp).await;
                    });
                }
            }
        }
    }

    /// Serve on an already-bound listener (test-only, so the test can learn the
    /// ephemeral port before connecting).
    #[cfg(test)]
    pub(crate) async fn serve_on(
        self,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), BrokerError> {
        self.run(listener, shutdown).await
    }

    /// Test-only constructor that injects a custom upstream (fixed dial + custom
    /// roots) but derives the route table and allowlist from a real manifest, so
    /// the full proxy path can be exercised against a local fake provider — with
    /// verification still on and the allowlist provably manifest-derived.
    #[cfg(test)]
    pub(crate) fn build_for_test(
        ca: &SessionCa,
        manifest: &ProviderManifest,
        creds: HashMap<String, SecretString>,
        upstream: Upstream,
    ) -> Result<Broker, BrokerError> {
        Broker::build_inner(manifest, creds, upstream, ca)
    }
}

/// Build the capsule-facing rustls server config from the session CA's leaf.
fn build_server_config(ca: &SessionCa) -> Result<ServerConfig, BrokerError> {
    ensure_crypto_provider();
    let certs = ca.server_cert_chain();
    let key = ca.server_private_key()?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| BrokerError::Tls(format!("server config: {e}")))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Terminate TLS on one connection, capture the SNI, and serve HTTP/1.1.
async fn serve_conn(acceptor: TlsAcceptor, inner: Arc<Inner>, tcp: tokio::net::TcpStream) {
    let tls = match acceptor.accept(tcp).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "tls handshake failed");
            return;
        }
    };
    let sni = tls.get_ref().1.server_name().map(|s| s.to_string());

    let io = TokioIo::new(tls);
    let service = service_fn(move |req| {
        let inner = inner.clone();
        let sni = sni.clone();
        proxy::handle(inner, sni, req)
    });

    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
        tracing::debug!(error = %e, "connection error");
    }
}

#[cfg(test)]
mod tests;
