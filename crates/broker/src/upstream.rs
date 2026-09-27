//! Upstream re-origination: connect to the real provider over TLS with **full**
//! certificate verification and forward the (streaming) request/response
//! .

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use capscode_core::BrokerError;

/// Where the upstream connection is actually dialed.
pub(crate) enum DialMode {
    /// Production: resolve `<authority>:443` and use the authority as the SNI.
    Real,
    /// Test-only: connect to a fixed local socket, presenting `sni`. This still
    /// runs full verification against the injected client roots — it never
    /// disables certificate checking.
    #[cfg(test)]
    Fixed {
        addr: SocketAddr,
        sni: ServerName<'static>,
    },
}

/// The upstream TLS client side of the broker.
pub(crate) struct Upstream {
    connector: TlsConnector,
    dial: DialMode,
}

impl Upstream {
    /// Production upstream: verifies provider certificates against the webpki
    /// (Mozilla) root program. No `danger_accept_invalid_*`; verification on.
    pub(crate) fn real() -> Upstream {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Upstream::from_roots(roots, DialMode::Real)
    }

    /// Builds an upstream from an explicit root store + dial mode. Verification
    /// is always on — the verifier is the standard rustls path-building verifier.
    pub(crate) fn from_roots(roots: RootCertStore, dial: DialMode) -> Upstream {
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Upstream {
            connector: TlsConnector::from(Arc::new(config)),
            dial,
        }
    }

    /// Resolve the (socket address, SNI) the request will actually be dialed to.
    async fn dial_target(
        &self,
        authority: &str,
    ) -> Result<(SocketAddr, ServerName<'static>), BrokerError> {
        match &self.dial {
            DialMode::Real => {
                let addr = tokio::net::lookup_host((authority, 443u16))
                    .await
                    .map_err(|e| BrokerError::Upstream(authority.to_string(), format!("dns: {e}")))?
                    .next()
                    .ok_or_else(|| {
                        BrokerError::Upstream(authority.to_string(), "no address".to_string())
                    })?;
                let sni = ServerName::try_from(authority.to_string()).map_err(|e| {
                    BrokerError::Upstream(authority.to_string(), format!("sni: {e}"))
                })?;
                Ok((addr, sni))
            }
            #[cfg(test)]
            DialMode::Fixed { addr, sni } => Ok((*addr, sni.clone())),
        }
    }

    /// Open a verified TLS connection to the provider and forward `req`,
    /// returning the streaming response. The request/response bodies are never
    /// buffered here — hyper yields frames as they arrive.
    pub(crate) async fn send(
        &self,
        authority: &str,
        req: Request<Incoming>,
    ) -> Result<Response<Incoming>, BrokerError> {
        let (addr, sni) = self.dial_target(authority).await?;

        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| BrokerError::Upstream(authority.to_string(), format!("connect: {e}")))?;
        let tls = self
            .connector
            .connect(sni, tcp)
            .await
            .map_err(|e| BrokerError::Upstream(authority.to_string(), format!("tls: {e}")))?;

        let io = TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| BrokerError::Upstream(authority.to_string(), format!("handshake: {e}")))?;

        // Drive the connection in the background so the response body keeps
        // streaming after `send_request` returns.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(error = %e, "upstream connection closed");
            }
        });

        sender
            .send_request(req)
            .await
            .map_err(|e| BrokerError::Upstream(authority.to_string(), format!("request: {e}")))
    }
}
