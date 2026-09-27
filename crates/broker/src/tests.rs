//! End-to-end tests for the secret broker.
//!
//! Stands up: (1) a fake provider as a local TLS server added to the allowlist,
//! (2) the broker with a self-signed session CA, and (3) a client that trusts
//! the session CA and verifies the broker routes requests safely.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::upstream::{DialMode, Upstream};
use crate::{Broker, ProviderManifest, SessionCa};

/// Real upstream key for the anthropic-style bearer provider (kimi/Moonshot).
const REAL_KEY: &str = "sk-REAL-upstream-key-DO-NOT-LOG-abcdef123456";
/// Real upstream key for the openai-style bearer provider (grok/x.ai).
const GROK_KEY: &str = "xai-REAL-grok-key-DO-NOT-LOG-abcdef123456";
/// Real Anthropic API key for the explicit x-api-key provider.
const API_KEY: &str = "sk-ant-api03-FAKE-api-key-DO-NOT-LOG";
/// The capsule-side sentinel the broker must strip and never forward or log.
const SENTINEL_KEY: &str = "sk-capscode-proxied-SENTINEL";
const FAKE_SNI: &str = "upstream.test";
/// The OAuth beta value Claude Code adds; the broker must preserve it upstream.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The non-secret providers manifest exercised end to end: an anthropic-style
/// x-api-key provider, an anthropic-style *bearer* provider with a base_path
/// prefix (kimi/Moonshot), and an openai-style bearer provider (grok/x.ai). The
/// allowlist and route table are built ENTIRELY from this — nothing hardcoded.
const MANIFEST_JSON: &str = r#"[
  {"id":"anthropic","vhost":"anthropic.broker.capscode.local","authority":"api.anthropic.com",
   "base_path":"","api_style":"anthropic","inject":"x-api-key","key_env":"ANTHROPIC_API_KEY"},
  {"id":"kimi","vhost":"kimi.broker.capscode.local","authority":"api.moonshot.ai",
   "base_path":"/anthropic","api_style":"anthropic","inject":"bearer","key_env":"MOONSHOT_API_KEY"},
  {"id":"grok","vhost":"grok.broker.capscode.local","authority":"api.x.ai",
   "base_path":"","api_style":"openai","inject":"bearer","key_env":"XAI_API_KEY"}
]"#;

/// Build a credentials map (keyed by `key_env`) from `(env, value)` pairs.
fn creds(pairs: &[(&str, &str)]) -> HashMap<String, SecretString> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), SecretString::from(v.to_string())))
        .collect()
}

fn install_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

// ---------------------------------------------------------------------------
// Log capture: a global tracing subscriber writing every level into a buffer,
// so we can grep it for the key value.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BufMakeWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufMakeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMakeWriter {
    type Writer = BufMakeWriter;
    fn make_writer(&'a self) -> BufMakeWriter {
        self.clone()
    }
}

// ---------------------------------------------------------------------------
// Certificate helpers.
// ---------------------------------------------------------------------------

struct FakeCerts {
    leaf_chain: Vec<CertificateDer<'static>>,
    leaf_key: PrivateKeyDer<'static>,
    ca_der: CertificateDer<'static>,
}

/// A CA + leaf (SAN `upstream.test`) standing in for a real provider's chain.
fn generate_fake_upstream_certs() -> FakeCerts {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "fake upstream CA");
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_der = ca_cert.der().clone();

    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf_params = CertificateParams::new(vec![FAKE_SNI.to_string()]).unwrap();
    leaf_params.is_ca = IsCa::NoCa;
    let issuer: Issuer<'_, KeyPair> = Issuer::new(ca_params, ca_key);
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    FakeCerts {
        leaf_chain: vec![leaf_cert.der().clone()],
        leaf_key: PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        ca_der,
    }
}

fn root_store_with(cert: CertificateDer<'static>) -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    roots
}

fn client_roots_for_ca_pem(ca_pem: &str) -> RootCertStore {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut ca_pem.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    let mut roots = RootCertStore::empty();
    for c in certs {
        roots.add(c).unwrap();
    }
    roots
}

// ---------------------------------------------------------------------------
// Fake upstream provider.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Captured {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

struct FakeUpstream {
    addr: SocketAddr,
    ca_der: CertificateDer<'static>,
    captured: Arc<Mutex<Vec<Captured>>>,
    conn_count: Arc<AtomicUsize>,
    /// Gate that releases the second SSE chunk on the `/stream` path.
    stream_gate: Arc<Notify>,
}

impl FakeUpstream {
    async fn start() -> FakeUpstream {
        let certs = generate_fake_upstream_certs();
        let ca_der = certs.ca_der.clone();

        let mut server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs.leaf_chain, certs.leaf_key)
            .unwrap();
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let captured = Arc::new(Mutex::new(Vec::new()));
        let conn_count = Arc::new(AtomicUsize::new(0));
        let stream_gate = Arc::new(Notify::new());

        let cap = captured.clone();
        let cc = conn_count.clone();
        let gate = stream_gate.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let acceptor = acceptor.clone();
                let cap = cap.clone();
                let cc = cc.clone();
                let gate = gate.clone();
                tokio::spawn(async move {
                    let _ = handle_fake_conn(acceptor, tcp, cap, cc, gate).await;
                });
            }
        });

        FakeUpstream {
            addr,
            ca_der,
            captured,
            conn_count,
            stream_gate,
        }
    }
}

async fn handle_fake_conn(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    captured: Arc<Mutex<Vec<Captured>>>,
    conn_count: Arc<AtomicUsize>,
    stream_gate: Arc<Notify>,
) -> std::io::Result<()> {
    let mut tls = acceptor.accept(tcp).await?;
    conn_count.fetch_add(1, Ordering::SeqCst);

    // Read the request head (up to the blank line). GET requests have no body.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = tls.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let path = request_line.split(' ').nth(1).unwrap_or("/").to_string();
    captured.lock().unwrap().push(Captured {
        request_line,
        headers,
    });

    if path == "/stream" {
        // Chunked SSE, second event gated so a buffering proxy would deadlock.
        tls.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .await?;
        write_chunk(&mut tls, b"data: one\n\n").await?;
        tls.flush().await?;
        stream_gate.notified().await;
        write_chunk(&mut tls, b"data: two\n\n").await?;
        tls.write_all(b"0\r\n\r\n").await?;
        tls.flush().await?;
    } else {
        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        tls.flush().await?;
    }
    Ok(())
}

async fn write_chunk<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    w.write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    w.write_all(data).await?;
    w.write_all(b"\r\n").await?;
    w.flush().await
}

// ---------------------------------------------------------------------------
// Client that trusts the session CA and speaks to the broker.
// ---------------------------------------------------------------------------

/// Connect to the broker over TLS, presenting `sni` (the broker leaf must cover
/// it — proving the manifest-driven SANs). Routing is then chosen by the Host
/// header of each request, independent of this SNI.
async fn connect_client(
    broker_addr: SocketAddr,
    session_ca_pem: &str,
    sni: &str,
) -> hyper::client::conn::http1::SendRequest<Empty<Bytes>> {
    let roots = client_roots_for_ca_pem(session_ca_pem);
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = TlsConnector::from(Arc::new(config));

    let tcp = TcpStream::connect(broker_addr).await.unwrap();
    let sni = ServerName::try_from(sni.to_string()).unwrap();
    let tls = connector.connect(sni, tcp).await.unwrap();
    let io = TokioIo::new(tls);
    let (sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

/// Build a GET request carrying inbound sentinel + fingerprint headers.
fn probe_request(host: &str, path: &str) -> Request<Empty<Bytes>> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("host", host)
        .header("x-api-key", SENTINEL_KEY)
        .header("authorization", format!("Bearer {SENTINEL_KEY}"))
        // Claude Code adds an anthropic-beta header (esp. in OAuth mode); the broker
        // must forward it untouched. Included on every probe to prove preservation.
        .header("anthropic-beta", OAUTH_BETA)
        .header("user-agent", "claude-cli/1.9.9 (node/20; darwin arm64)")
        .header("x-forwarded-for", "10.1.2.3")
        .header("x-stainless-os", "MacOS")
        .header("x-stainless-runtime", "node")
        .body(Empty::<Bytes>::new())
        .unwrap()
}

async fn body_to_string(resp: Response<Incoming>) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

// ---------------------------------------------------------------------------
// The acceptance test.
// ---------------------------------------------------------------------------

/// A fixed-dial upstream that always connects to the fake provider (presenting
/// `FAKE_SNI`) but still runs full certificate verification against its roots.
fn fixed_upstream(fake: &FakeUpstream) -> Upstream {
    Upstream::from_roots(
        root_store_with(fake.ca_der.clone()),
        DialMode::Fixed {
            addr: fake.addr,
            sni: ServerName::try_from(FAKE_SNI.to_string()).unwrap(),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_broker_acceptance() {
    install_crypto();

    let logbuf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(BufMakeWriter(logbuf.clone()))
        .try_init();

    let fake = FakeUpstream::start().await;

    // The route table AND the allowlist are built ENTIRELY from this manifest —
    // three providers, none hardcoded in the broker.
    let manifest = ProviderManifest::from_json(MANIFEST_JSON).unwrap();
    let vhosts: Vec<String> = manifest.vhosts().map(|s| s.to_string()).collect();

    // The leaf covers exactly the manifest vhosts (kimi/grok/anthropic), so TLS
    // termination works for user-selected providers — not just the built-ins.
    let ca = SessionCa::generate_with_sans(&vhosts).unwrap();
    let broker = Broker::build_for_test(
        &ca,
        &manifest,
        creds(&[
            ("ANTHROPIC_API_KEY", API_KEY),
            ("MOONSHOT_API_KEY", REAL_KEY),
            ("XAI_API_KEY", GROK_KEY),
        ]),
        fixed_upstream(&fake),
    )
    .unwrap();
    let ca_pem = broker.ca_cert_pem().to_string();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broker_addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let broker_task = tokio::spawn(async move {
        let _ = broker
            .serve_on(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    println!("\n===== BROKER MANIFEST-DRIVEN REAL-RUN EVIDENCE =====");
    println!("broker listening on   {broker_addr}");
    println!("fake upstream on      {} (SNI {FAKE_SNI})", fake.addr);
    println!("manifest authorities  api.anthropic.com, api.moonshot.ai, api.x.ai");

    // ---- Scenario A: anthropic-style provider (kimi): bearer + base_path -----
    {
        // Connect with the KIMI vhost SNI (proves the manifest-driven leaf SAN),
        // then route to it via the Host header.
        let mut sender = connect_client(broker_addr, &ca_pem, "kimi.broker.capscode.local").await;
        let resp = sender
            .send_request(probe_request("kimi.broker.capscode.local", "/v1/messages"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_to_string(resp).await, "ok");

        let cap = fake.captured.lock().unwrap().last().cloned().unwrap();
        println!("\n[A] anthropic-style (kimi, bearer, base_path /anthropic):");
        println!("    request-line : {}", cap.request_line);
        println!("    host         : {:?}", cap.get("host"));
        println!("    authorization: {:?}", cap.get("authorization"));
        println!("    x-api-key    : {:?}", cap.get("x-api-key"));

        // Real key injected as Bearer; base_path prefixed onto the path.
        assert_eq!(
            cap.get("authorization"),
            Some(format!("Bearer {REAL_KEY}").as_str())
        );
        assert!(cap.request_line.contains("/anthropic/v1/messages"));
        // No x-api-key on a bearer route; routed to the real authority.
        assert_eq!(cap.get("x-api-key"), None);
        assert_eq!(cap.get("host"), Some("api.moonshot.ai"));
        // Tool headers pass through; the sentinel is stripped and Host is rewritten.
        assert_eq!(cap.get("anthropic-beta"), Some(OAUTH_BETA));
        for (_, v) in &cap.headers {
            assert!(!v.contains(SENTINEL_KEY), "sentinel leaked upstream: {v}");
        }
        assert_eq!(
            cap.get("user-agent"),
            Some("claude-cli/1.9.9 (node/20; darwin arm64)")
        );
        assert_eq!(cap.get("x-forwarded-for"), Some("10.1.2.3"));
        assert_eq!(cap.get("x-stainless-os"), Some("MacOS"));
        assert_eq!(cap.get("x-stainless-runtime"), Some("node"));
        println!("    => Bearer <real>, path prefixed /anthropic, no x-api-key, client headers kept, sentinel absent ✔");
    }

    // ---- Scenario A2: openai-style provider (grok): bearer, base_path empty --
    {
        let mut sender = connect_client(broker_addr, &ca_pem, "grok.broker.capscode.local").await;
        let resp = sender
            .send_request(probe_request(
                "grok.broker.capscode.local",
                "/v1/chat/completions",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = body_to_string(resp).await;

        let cap = fake.captured.lock().unwrap().last().cloned().unwrap();
        println!("\n[A2] openai-style (grok, bearer, api.x.ai):");
        println!("    request-line : {}", cap.request_line);
        println!("    host         : {:?}", cap.get("host"));
        println!("    authorization: {:?}", cap.get("authorization"));

        assert_eq!(
            cap.get("authorization"),
            Some(format!("Bearer {GROK_KEY}").as_str())
        );
        // Empty base_path: path forwarded unchanged; routed to api.x.ai.
        assert!(cap.request_line.contains("/v1/chat/completions"));
        assert!(!cap.request_line.contains("/anthropic"));
        assert_eq!(cap.get("host"), Some("api.x.ai"));
        assert_eq!(cap.get("x-api-key"), None);
        println!("    => Bearer <real> injected, routed to api.x.ai, base_path empty ✔");
    }

    // ---- Scenario A3: anthropic API-key provider: x-api-key, no base_path ----
    {
        let mut sender =
            connect_client(broker_addr, &ca_pem, "anthropic.broker.capscode.local").await;
        let resp = sender
            .send_request(probe_request(
                "anthropic.broker.capscode.local",
                "/v1/messages",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = body_to_string(resp).await;

        let cap = fake.captured.lock().unwrap().last().cloned().unwrap();
        println!("\n[A3] anthropic-style (anthropic, x-api-key):");
        println!("    x-api-key    : {:?}", cap.get("x-api-key"));
        println!("    authorization: {:?}", cap.get("authorization"));

        // x-api-key injected; NO Authorization; host + path as expected.
        assert_eq!(cap.get("x-api-key"), Some(API_KEY));
        assert_eq!(cap.get("authorization"), None);
        assert_eq!(cap.get("host"), Some("api.anthropic.com"));
        assert!(cap.request_line.contains("/v1/messages"));
        assert_eq!(cap.get("anthropic-beta"), Some(OAUTH_BETA));
        println!("    => x-api-key <real> injected, no Authorization, beta preserved ✔");
    }

    // ---- Scenario B: vhost NOT in the manifest → default-deny, no connect ----
    {
        let conn_before = fake.conn_count.load(Ordering::SeqCst);
        // Valid TLS (anthropic SNI), but the Host header names a vhost that is not
        // in the manifest at all — there is no route and no allowlisted authority.
        let mut sender =
            connect_client(broker_addr, &ca_pem, "anthropic.broker.capscode.local").await;
        let resp = sender
            .send_request(probe_request(
                "openai.broker.capscode.local",
                "/v1/chat/completions",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let conn_after = fake.conn_count.load(Ordering::SeqCst);
        println!("\n[B] vhost not in manifest (openai.broker.capscode.local):");
        println!(
            "    status               : {} (Forbidden)",
            resp.status().as_u16()
        );
        println!("    upstream connections : before={conn_before} after={conn_after} (unchanged)");
        assert_eq!(
            conn_before, conn_after,
            "denied request must not open an upstream connection"
        );
        println!("    => HostNotAllowed before any upstream connect (default deny) ✔");
    }

    // ---- Scenario C: SSE streaming passthrough (not buffered) --------------
    {
        let mut sender =
            connect_client(broker_addr, &ca_pem, "anthropic.broker.capscode.local").await;
        let resp = sender
            .send_request(probe_request("anthropic.broker.capscode.local", "/stream"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();

        // First event must arrive BEFORE we release the upstream's second event.
        // A buffer-to-completion proxy would never deliver event one → timeout.
        let first = tokio::time::timeout(Duration::from_secs(5), next_data(&mut body))
            .await
            .expect("first SSE event timed out (streaming not preserved)")
            .expect("first SSE event missing");
        assert_eq!(first, Bytes::from_static(b"data: one\n\n"));
        println!("\n[C] SSE streaming:");
        println!("    received event 1 while upstream still holds event 2 ✔");

        // Release the second event and read it.
        fake.stream_gate.notify_one();
        let second = tokio::time::timeout(Duration::from_secs(5), next_data(&mut body))
            .await
            .expect("second SSE event timed out")
            .expect("second SSE event missing");
        assert_eq!(second, Bytes::from_static(b"data: two\n\n"));
        println!("    received event 2 after release ✔");
        println!("    => end-to-end streaming preserved (no buffering) ✔");
    }

    // ---- Scenario G: provider with NO key → forwarded WITHOUT a credential ---
    {
        // A manifest with an anthropic route (has a key) plus an "orphan" route
        // whose key_env has no matching key. The orphan authority is still
        // allowlisted (manifest-derived), so its request is forwarded — but with
        // no credential at all, so a real provider would 401 ("fails auth
        // downstream"). Here we prove the forward happens and no auth is injected.
        let orphan_json = r#"[
          {"id":"anthropic","vhost":"anthropic.broker.capscode.local","authority":"api.anthropic.com",
           "base_path":"","api_style":"anthropic","inject":"x-api-key","key_env":"ANTHROPIC_API_KEY"},
          {"id":"orphan","vhost":"orphan.broker.capscode.local","authority":"api.orphan.test",
           "base_path":"","api_style":"openai","inject":"bearer","key_env":"ORPHAN_KEY"}
        ]"#;
        let orphan_manifest = ProviderManifest::from_json(orphan_json).unwrap();
        let orphan_vhosts: Vec<String> = orphan_manifest.vhosts().map(|s| s.to_string()).collect();
        let ca_orphan = SessionCa::generate_with_sans(&orphan_vhosts).unwrap();
        // Only the anthropic key is present; the orphan route has none.
        let broker_orphan = Broker::build_for_test(
            &ca_orphan,
            &orphan_manifest,
            creds(&[("ANTHROPIC_API_KEY", API_KEY)]),
            fixed_upstream(&fake),
        )
        .unwrap();
        let ca_pem_orphan = broker_orphan.ca_cert_pem().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_orphan = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = broker_orphan
                .serve_on(listener, async move {
                    let _ = rx.await;
                })
                .await;
        });

        let conn_before = fake.conn_count.load(Ordering::SeqCst);
        let mut sender =
            connect_client(addr_orphan, &ca_pem_orphan, "orphan.broker.capscode.local").await;
        let resp = sender
            .send_request(probe_request(
                "orphan.broker.capscode.local",
                "/v1/messages",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = body_to_string(resp).await;
        let conn_after = fake.conn_count.load(Ordering::SeqCst);

        let cap = fake.captured.lock().unwrap().last().cloned().unwrap();
        println!("\n[G] keyless provider (orphan, no ORPHAN_KEY):");
        println!("    upstream connections : before={conn_before} after={conn_after} (forwarded)");
        println!("    authorization: {:?}", cap.get("authorization"));
        println!("    x-api-key    : {:?}", cap.get("x-api-key"));

        // It WAS forwarded (a new upstream connection), routed to the authority,
        // but carries neither an injected credential nor the inbound sentinel.
        assert_eq!(
            conn_after,
            conn_before + 1,
            "keyless route must still forward"
        );
        assert_eq!(cap.get("host"), Some("api.orphan.test"));
        assert_eq!(cap.get("authorization"), None);
        assert_eq!(cap.get("x-api-key"), None);
        for (_, v) in &cap.headers {
            assert!(!v.contains(SENTINEL_KEY), "sentinel leaked upstream: {v}");
        }
        println!("    => forwarded with NO credential (upstream would 401); sentinel absent ✔");

        let _ = tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }

    // ---- Scenario D: no key anywhere in the logs ---------------------------
    {
        // Give spawned logging tasks a moment to flush.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let logs = logbuf.lock().unwrap();
        let text = String::from_utf8_lossy(&logs);
        let real_hits = text.matches(REAL_KEY).count();
        let grok_hits = text.matches(GROK_KEY).count();
        let api_hits = text.matches(API_KEY).count();
        let sentinel_hits = text.matches(SENTINEL_KEY).count();
        println!(
            "\n[D] log scan ({} bytes captured, all levels):",
            logs.len()
        );
        println!("    occurrences of kimi/moonshot key : {real_hits}");
        println!("    occurrences of grok key          : {grok_hits}");
        println!("    occurrences of anthropic API key : {api_hits}");
        println!("    occurrences of SENTINEL key      : {sentinel_hits}");
        assert_eq!(real_hits, 0, "moonshot key appeared in logs");
        assert_eq!(grok_hits, 0, "grok key appeared in logs");
        assert_eq!(api_hits, 0, "anthropic API key appeared in logs");
        assert_eq!(sentinel_hits, 0, "sentinel key appeared in logs");
        println!("    => zero credential material in logs at any level ✔");
    }

    // ---- CA hygiene: only the cert PEM is exposed --------------------------
    {
        assert!(ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(!ca_pem.contains("PRIVATE KEY"));
        let dbg = format!("{:?}", ca);
        assert!(!dbg.contains("PRIVATE KEY"));
        assert!(!dbg.contains(REAL_KEY));
        println!("\n[CA] ca_cert_pem exposes only a CERTIFICATE (no private key); Debug redacts ✔");
    }

    println!("\n===== ALL BROKER ACCEPTANCE ITEMS PASSED =====\n");

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), broker_task).await;
    // Silence unused-writer lints.
    let _ = std::io::stdout().flush();
}

/// Pull the next DATA frame's bytes from a streaming body.
async fn next_data(body: &mut Incoming) -> Option<Bytes> {
    while let Some(frame) = body.frame().await {
        let frame = frame.ok()?;
        if let Ok(data) = frame.into_data() {
            return Some(data);
        }
    }
    None
}
