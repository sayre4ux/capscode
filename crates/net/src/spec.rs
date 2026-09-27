//! Gateway `ContainerSpec` construction .
//!
//! The gateway is the only privileged container: it holds `NET_ADMIN` and
//! `/dev/net/tun`, the WireGuard tunnel, the nftables kill switch, the real API
//! keys, and the broker. The capsule shares only its *network* namespace and has
//! none of this (see the `capsule.*` images).
//!
//! Boot-time config is delivered through the gateway environment, **except the two
//! high-value secrets** — the provider API keys and the WireGuard config (which
//! carries a private key). Those are NOT placed in env (env is visible via
//! `docker inspect`); instead the gateway mounts a `tmpfs` at
//! [`SECRETS_DIR`] and the host CLI writes them there after start over an exec
//! stdin pipe . The env carries only the path
//! ([`ENV_SECRETS_DIR`]) plus non-secret values (kill-switch ruleset, mode,
//! resolvers, sinkhole flags, the public CA cert, broker listen).

use std::path::PathBuf;

use capscode_core::{
    AuthKind, ContainerSpec, ImageRef, Mount, NetContext, NetError, NetworkMode, Provider,
    LABEL_OWNER, LABEL_OWNER_VALUE, LABEL_ROLE, LABEL_SESSION, ROLE_GATEWAY,
};
use ipnet::IpNet;

use crate::killswitch::{self, KillswitchParams};
use crate::sinkhole::TELEMETRY_HOSTS;
use crate::wg;

// --- gateway env contract (read by images/gateway-entrypoint.sh) -------------

/// Rendered nftables kill-switch ruleset (applied with `nft -f`).
pub const ENV_KILLSWITCH_NFT: &str = "CAPSCODE_KILLSWITCH_NFT";
/// Directory of the gateway secrets `tmpfs`. The host CLI writes `keys.env`,
/// `wg0.conf` (tunnel mode), and a `ready` sentinel here after start; no secret
/// is ever placed in env. Consumed by the entrypoint and the broker.
pub const ENV_SECRETS_DIR: &str = "CAPSCODE_SECRETS_DIR";
/// Mount point of the secrets `tmpfs` inside the gateway.
pub const SECRETS_DIR: &str = "/run/capscode";
/// Space-separated tunnel resolver addresses; capsule resolv.conf is pinned to these.
pub const ENV_TUNNEL_DNS: &str = "CAPSCODE_TUNNEL_DNS";
/// Space-separated telemetry hostnames to resolve into the `@sinkhole4` drop set.
pub const ENV_SINKHOLE_HOSTS: &str = "CAPSCODE_SINKHOLE_HOSTS";
/// `1` when the telemetry sinkhole is enabled, else `0`.
pub const ENV_SINKHOLE: &str = "CAPSCODE_SINKHOLE";
/// `tunnel` or `containment-only`; selects the boot sequence.
pub const ENV_NET_MODE: &str = "CAPSCODE_NET_MODE";
/// Per-session CA certificate (PEM) for the broker leaf. Public cert only.
pub const ENV_CA_CERT_PEM: &str = "CAPSCODE_CA_CERT_PEM";
/// Loopback address:port the broker binds inside the shared netns.
pub const ENV_BROKER_LISTEN: &str = "CAPSCODE_BROKER_LISTEN";

/// The loopback endpoint the broker binds and the capsule reaches.
pub const BROKER_LISTEN: &str = "127.0.0.1:8443";

/// Build the gateway [`ContainerSpec`] for a session.
///
/// Fails with [`NetError::BadWgConfig`] if a configured WireGuard file is missing
/// or invalid — the session must never come up on an unvalidated tunnel config.
pub fn gateway_spec(ctx: &NetContext) -> Result<ContainerSpec, NetError> {
    let sinkhole = ctx.network.telemetry_sinkhole;
    let extra_blocked_cidrs = parse_extra_blocked_cidrs(&ctx.network.extra_blocked_cidrs)?;
    let mut env: Vec<(String, String)> = Vec::new();

    // Resolve the mode from the network config, parsing/validating the WG file.
    let (killswitch_params, mode_label, tunnel) = match &ctx.network.wireguard_conf {
        Some(path) if ctx.network.enforce_vpn => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                NetError::BadWgConfig(format!("cannot read {}: {e}", path.display()))
            })?;
            let cfg = wg::parse_and_validate(&text)?;
            let params = KillswitchParams::tunnel(&cfg, extra_blocked_cidrs, sinkhole);
            // NB: `text` (which contains the WG private key) is validated here to
            // drive the kill switch, but is deliberately NOT pushed into env — the
            // host CLI injects it into the secrets tmpfs after start .
            let resolvers = cfg.tunnel_resolvers().join(" ");
            env.push((ENV_TUNNEL_DNS.to_string(), resolvers));
            (params, "tunnel", true)
        }
        // wireguard_conf is None, or enforce_vpn disabled: containment-only.
        _ => {
            let params = KillswitchParams::containment_only(extra_blocked_cidrs, sinkhole);
            (params, "containment-only", false)
        }
    };

    env.push((ENV_NET_MODE.to_string(), mode_label.to_string()));
    env.push((
        ENV_KILLSWITCH_NFT.to_string(),
        killswitch::render(&killswitch_params),
    ));
    env.push((
        ENV_SINKHOLE.to_string(),
        if sinkhole { "1" } else { "0" }.to_string(),
    ));
    if sinkhole {
        env.push((ENV_SINKHOLE_HOSTS.to_string(), TELEMETRY_HOSTS.join(" ")));
    }
    env.push((ENV_CA_CERT_PEM.to_string(), ctx.ca_cert_pem.clone()));
    env.push((ENV_BROKER_LISTEN.to_string(), BROKER_LISTEN.to_string()));

    // Secrets are delivered into a tmpfs, never via the environment.
    // Env carries only the directory path; the host CLI writes the real provider
    // keys and (in tunnel mode) the WG config there after the gateway starts.
    env.push((ENV_SECRETS_DIR.to_string(), SECRETS_DIR.to_string()));

    // Privilege: NET_ADMIN is always required to load the nft ruleset; /dev/net/tun
    // is required only to bring up the userspace WireGuard tunnel.
    let cap_add = vec!["NET_ADMIN".to_string()];
    let devices = if tunnel {
        vec!["/dev/net/tun".to_string()]
    } else {
        Vec::new()
    };

    Ok(ContainerSpec {
        image: ImageRef(ctx.gateway_image.0.clone()),
        hostname: "workspace".to_string(),
        env,
        cmd: None,
        // Entrypoint is baked into images/gateway.Dockerfile.
        entrypoint: None,
        // In-memory tmpfs the host CLI injects secrets into (keys.env, wg0.conf,
        // ready). tmpfs ⇒ never touches disk; container teardown ⇒ gone.
        mounts: vec![Mount::Tmpfs {
            target: PathBuf::from(SECRETS_DIR),
            size_mb: Some(1),
            exec: false,
        }],
        // The gateway owns the netns; it needs its own routable interface to reach
        // the WG peer endpoint / the internet.
        network: NetworkMode::Bridge("bridge".to_string()),
        published_ports: Vec::new(),
        cap_add,
        cap_drop: Vec::new(),
        no_new_privileges: false,
        devices,
        sysctls: Vec::new(),
        labels: vec![
            (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
            (LABEL_SESSION.to_string(), ctx.session.to_string()),
            (LABEL_ROLE.to_string(), ROLE_GATEWAY.to_string()),
        ],
        // The gateway runs no untrusted code but must write /etc/wireguard,
        // /etc/resolv.conf and WG runtime state, so its rootfs stays writable.
        tmpfs_home: false,
        read_only_rootfs: false,
        init: false,
        // DECISION: the gateway gets the same default cap as the capsule, not the
        // `capsule.pids` override. The broker's tokio blocking pool tops out at 512
        // threads (upstream DNS lookups), so 1024 leaves headroom; memory stays
        // unset so a capsule-side cap can never starve the credential broker.
        pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
        memory_bytes: None,
    })
}

fn parse_extra_blocked_cidrs(values: &[String]) -> Result<Vec<IpNet>, NetError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.parse::<IpNet>().map_err(|_| {
                NetError::KillSwitch(format!(
                    "extra blocked CIDR {} is not a valid IP network",
                    index + 1
                ))
            })
        })
        .collect()
}

/// Environment/`keys.env` variable a credential is injected under, given its
/// provider **and** auth kind (consumed by the broker inside the gateway). This
/// is the mapping `capscode-broker`'s `KEY_ENVS` mirrors.
///
/// Anthropic has two shapes: a long-lived API key travels as `ANTHROPIC_API_KEY`
/// (injected upstream as `x-api-key`); a Claude Code subscription setup-token
/// travels as `CLAUDE_CODE_OAUTH_TOKEN` (injected upstream as `Authorization:
/// Bearer`). ChatGPT-backed Codex sessions use `CODEX_ACCESS_TOKEN`.
pub fn provider_cred_env(provider: Provider, kind: AuthKind) -> &'static str {
    match (provider, kind) {
        (Provider::Anthropic, AuthKind::OAuth) => "CLAUDE_CODE_OAUTH_TOKEN",
        (Provider::Anthropic, AuthKind::ApiKey) => "ANTHROPIC_API_KEY",
        (Provider::OpenAI, AuthKind::CodexAccessToken) => "CODEX_ACCESS_TOKEN",
        (Provider::OpenAI, AuthKind::ApiKey | AuthKind::OAuth) => "OPENAI_API_KEY",
        (Provider::Moonshot, _) => "MOONSHOT_API_KEY",
        (Provider::Anthropic, AuthKind::CodexAccessToken) => "CLAUDE_CODE_OAUTH_TOKEN",
    }
}

/// Back-compat provider-only accessor: the API-key env var for a provider. Kept
/// for callers that predate the OAuth auth-kind split; equivalent to
/// [`provider_cred_env`] with [`AuthKind::ApiKey`].
pub fn provider_key_env(provider: Provider) -> &'static str {
    provider_cred_env(provider, AuthKind::ApiKey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;

    use capscode_core::{BrokerConfig, NetworkConfig, ProviderCredential, ProviderKeys, SessionId};
    use secrecy::SecretString;

    fn ctx_with(network: NetworkConfig, keys: ProviderKeys) -> NetContext {
        NetContext {
            session: SessionId::from("test-session".to_string()),
            network,
            broker: BrokerConfig::default(),
            keys,
            ca_cert_pem: "-----BEGIN CERTIFICATE-----\nMII...\n-----END CERTIFICATE-----\n"
                .to_string(),
            gateway_image: ImageRef("capscode/gateway:test".to_string()),
        }
    }

    fn get<'a>(spec: &'a ContainerSpec, k: &str) -> Option<&'a str> {
        spec.env
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn gateway_has_net_admin_and_gateway_role() {
        let spec =
            gateway_spec(&ctx_with(NetworkConfig::default(), ProviderKeys::default())).unwrap();
        assert!(spec.cap_add.contains(&"NET_ADMIN".to_string()));
        // "workspace" so the capsule, which shares the gateway's UTS namespace,
        // inherits the normalized hostname .
        assert_eq!(spec.hostname, "workspace");
        assert!(spec
            .labels
            .iter()
            .any(|(k, v)| k == LABEL_ROLE && v == ROLE_GATEWAY));
        // Containment-only: no tun device required.
        assert!(spec.devices.is_empty());
        assert_eq!(get(&spec, ENV_NET_MODE), Some("containment-only"));
        assert_eq!(spec.pids_limit, capscode_core::DEFAULT_PIDS_LIMIT);
        assert_eq!(spec.memory_bytes, None);
    }

    #[test]
    fn cred_env_maps_provider_and_kind() {
        assert_eq!(
            provider_cred_env(Provider::Anthropic, AuthKind::OAuth),
            "CLAUDE_CODE_OAUTH_TOKEN"
        );
        assert_eq!(
            provider_cred_env(Provider::Anthropic, AuthKind::ApiKey),
            "ANTHROPIC_API_KEY"
        );
        assert_eq!(
            provider_cred_env(Provider::OpenAI, AuthKind::ApiKey),
            "OPENAI_API_KEY"
        );
        assert_eq!(
            provider_cred_env(Provider::Moonshot, AuthKind::ApiKey),
            "MOONSHOT_API_KEY"
        );
        // OpenAI/Moonshot never carry an OAuth token; the API-key var is returned
        // regardless of kind so a stray kind can never misname their var.
        assert_eq!(
            provider_cred_env(Provider::OpenAI, AuthKind::OAuth),
            "OPENAI_API_KEY"
        );
        assert_eq!(
            provider_cred_env(Provider::OpenAI, AuthKind::CodexAccessToken),
            "CODEX_ACCESS_TOKEN"
        );
        // Back-compat provider-only helper still yields the API-key var.
        assert_eq!(provider_key_env(Provider::Anthropic), "ANTHROPIC_API_KEY");
    }

    #[test]
    fn keys_are_never_in_env_only_the_secrets_dir_path_is() {
        let mut map = HashMap::new();
        map.insert(
            Provider::Anthropic,
            ProviderCredential::api_key(SecretString::from("sk-ant-REAL".to_string())),
        );
        map.insert(
            Provider::OpenAI,
            ProviderCredential::api_key(SecretString::from("sk-oai-REAL".to_string())),
        );
        let spec = gateway_spec(&ctx_with(NetworkConfig::default(), ProviderKeys(map))).unwrap();
        // No key value and no provider-key env var may appear in the gateway env.
        assert_eq!(get(&spec, "ANTHROPIC_API_KEY"), None);
        assert_eq!(get(&spec, "OPENAI_API_KEY"), None);
        assert!(
            !spec
                .env
                .iter()
                .any(|(_, v)| v == "sk-ant-REAL" || v == "sk-oai-REAL"),
            "a real provider key value leaked into the gateway env"
        );
        // Only the tmpfs path is advertised; a matching tmpfs mount must exist.
        assert_eq!(get(&spec, ENV_SECRETS_DIR), Some(SECRETS_DIR));
        assert!(spec.mounts.iter().any(|m| matches!(
            m,
            Mount::Tmpfs { target, .. } if target == std::path::Path::new(SECRETS_DIR)
        )));
        assert!(spec
            .labels
            .iter()
            .any(|(k, v)| k == LABEL_ROLE && v == ROLE_GATEWAY));
    }

    #[test]
    fn tunnel_mode_reads_and_validates_wg_conf_and_requests_tun() {
        let f = tempfile_conf(
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 10.99.0.1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = vpn.example.com:51820\nAllowedIPs = 0.0.0.0/0\n",
        );
        let net = NetworkConfig {
            wireguard_conf: Some(f.path().to_path_buf()),
            enforce_vpn: true,
            telemetry_sinkhole: true,
            extra_blocked_cidrs: vec![],
        };
        let spec = gateway_spec(&ctx_with(net, ProviderKeys::default())).unwrap();
        assert!(spec.devices.contains(&"/dev/net/tun".to_string()));
        assert_eq!(get(&spec, ENV_NET_MODE), Some("tunnel"));
        assert_eq!(get(&spec, ENV_TUNNEL_DNS), Some("10.99.0.1"));
        // The WG config text (with the private key) must NOT be in env; only the
        // resolvers it yielded are. Injection happens post-start into the tmpfs.
        assert!(
            !spec
                .env
                .iter()
                .any(|(_, v)| v.contains("[Peer]") || v.contains("PrivateKey")),
            "WireGuard config text leaked into the gateway env"
        );
        assert_eq!(get(&spec, ENV_SECRETS_DIR), Some(SECRETS_DIR));
        assert!(get(&spec, ENV_KILLSWITCH_NFT)
            .unwrap()
            .contains("oifname \"wg0\" accept"));
    }

    #[test]
    fn invalid_wg_conf_is_rejected_before_any_container() {
        let f = tempfile_conf("[Interface]\nPrivateKey = k\n# no peer, no dns\n");
        let net = NetworkConfig {
            wireguard_conf: Some(f.path().to_path_buf()),
            enforce_vpn: true,
            telemetry_sinkhole: true,
            extra_blocked_cidrs: vec![],
        };
        assert!(matches!(
            gateway_spec(&ctx_with(net, ProviderKeys::default())),
            Err(NetError::BadWgConfig(_))
        ));
    }

    #[test]
    fn extra_blocked_cidrs_are_parsed_as_networks_before_rendering() {
        let parsed =
            parse_extra_blocked_cidrs(&["203.0.113.0/24".to_string(), "2001:db8::/32".to_string()])
                .unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parse_extra_blocked_cidrs(&[
            "203.0.113.0/24 accept; ip daddr 0.0.0.0/0".to_string()
        ])
        .is_err());
    }

    #[test]
    fn missing_wg_file_is_bad_config_not_panic() {
        let net = NetworkConfig {
            wireguard_conf: Some("/no/such/capscode-wg.conf".into()),
            enforce_vpn: true,
            telemetry_sinkhole: true,
            extra_blocked_cidrs: vec![],
        };
        assert!(matches!(
            gateway_spec(&ctx_with(net, ProviderKeys::default())),
            Err(NetError::BadWgConfig(_))
        ));
    }

    // Minimal temp-file helper (no dev-dependency needed).
    struct TempConf {
        path: std::path::PathBuf,
    }
    impl TempConf {
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for TempConf {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    fn tempfile_conf(contents: &str) -> TempConf {
        let mut path = std::env::temp_dir();
        // Unique-enough within a single test binary run without extra deps.
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        path.push(format!("capscode-net-test-{}-{n}.conf", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        TempConf { path }
    }
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
}
