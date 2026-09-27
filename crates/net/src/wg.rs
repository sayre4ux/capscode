//! WireGuard configuration parsing and canonical emission.
//!
//! Only the keys needed by WireGuard and wg-quick are accepted. The host parses
//! imported files into typed values and emits this canonical form before the
//! gateway ever receives a config, so wg-quick cannot execute config hooks.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use capscode_core::NetError;
use ipnet::IpNet;
use zeroize::Zeroize;

/// A parsed WireGuard `[Interface]` + `[Peer]` config.
///
/// Key bytes are retained only so the gateway config can be re-emitted from
/// validated fields. Their `Debug` implementation is redacted and the bytes are
/// zeroized when dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgConfig {
    private_key: WgKey,
    addresses: Vec<IpNet>,
    dns: Vec<IpAddr>,
    mtu: Option<u16>,
    listen_port: Option<u16>,
    /// Validated peers (at least one, each with an endpoint and allowed-ips).
    pub peers: Vec<WgPeer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgPeer {
    public_key: WgKey,
    preshared_key: Option<WgKey>,
    /// Validated endpoint host in canonical IPv4 or DNS form.
    pub endpoint_host: String,
    pub endpoint_port: u16,
    /// `AllowedIPs` entries, parsed as IP networks.
    pub allowed_ips: Vec<IpNet>,
    persistent_keepalive: Option<u16>,
}

#[derive(Clone, PartialEq, Eq)]
struct WgKey([u8; 32]);

impl WgKey {
    fn parse(value: &str, name: &str, line: usize) -> Result<Self, NetError> {
        let bytes = STANDARD
            .decode(value)
            .map_err(|_| line_bad(line, name, "must be base64 encoding of exactly 32 bytes"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| line_bad(line, name, "must be base64 encoding of exactly 32 bytes"))?;
        Ok(Self(bytes))
    }

    fn append_encoded(&self, text: &mut String) {
        STANDARD.encode_string(self.0, text);
    }
}

impl fmt::Debug for WgKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted WireGuard key]")
    }
}

impl Drop for WgKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl WgConfig {
    /// Resolver addresses for the capsule and kill switch.
    pub fn tunnel_resolvers(&self) -> Vec<String> {
        self.dns.iter().map(ToString::to_string).collect()
    }

    pub(crate) fn dns_addresses(&self) -> &[IpAddr] {
        &self.dns
    }

    /// The peer endpoint ports, sorted and de-duplicated for the renderer.
    pub fn endpoint_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self.peers.iter().map(|peer| peer.endpoint_port).collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// Emit the typed config using wg-quick's canonical section and key names.
    pub fn to_wg_quick(&self) -> String {
        let mut text = String::from("[Interface]\nPrivateKey = ");
        self.private_key.append_encoded(&mut text);
        text.push('\n');
        push_list(&mut text, "Address", &self.addresses);
        push_list(&mut text, "DNS", &self.dns);
        if let Some(mtu) = self.mtu {
            text.push_str(&format!("MTU = {mtu}\n"));
        }
        if let Some(port) = self.listen_port {
            text.push_str(&format!("ListenPort = {port}\n"));
        }

        for peer in &self.peers {
            text.push_str("\n[Peer]\nPublicKey = ");
            peer.public_key.append_encoded(&mut text);
            text.push('\n');
            if let Some(key) = &peer.preshared_key {
                text.push_str("PresharedKey = ");
                key.append_encoded(&mut text);
                text.push('\n');
            }
            text.push_str("Endpoint = ");
            if peer
                .endpoint_host
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_ipv6())
            {
                text.push('[');
                text.push_str(&peer.endpoint_host);
                text.push(']');
            } else {
                text.push_str(&peer.endpoint_host);
            }
            text.push_str(&format!(":{}\n", peer.endpoint_port));
            push_list(&mut text, "AllowedIPs", &peer.allowed_ips);
            if let Some(keepalive) = peer.persistent_keepalive {
                text.push_str(&format!("PersistentKeepalive = {keepalive}\n"));
            }
        }
        text
    }
}

fn push_list<T: fmt::Display>(text: &mut String, key: &str, values: &[T]) {
    if values.is_empty() {
        return;
    }
    text.push_str(key);
    text.push_str(" = ");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        text.push_str(&value.to_string());
    }
    text.push('\n');
}

/// Parse and validate a `wg-quick` config. Unsupported keys fail closed with
/// their source line so shell hooks and future syntax cannot pass through.
pub fn parse_and_validate(text: &str) -> Result<WgConfig, NetError> {
    let mut section: Option<Section> = None;
    let mut interface: Option<InterfaceBuilder> = None;
    let mut peers: Vec<PeerBuilder> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = section_header(line) {
            match name.to_ascii_lowercase().as_str() {
                "interface" => {
                    if interface.is_some() {
                        return Err(bad(format!(
                            "line {line_number}: duplicate [Interface] section"
                        )));
                    }
                    interface = Some(InterfaceBuilder::default());
                    section = Some(Section::Interface);
                }
                "peer" => {
                    peers.push(PeerBuilder::default());
                    section = Some(Section::Peer);
                }
                other => {
                    return Err(bad(format!(
                        "line {line_number}: unknown section [{other}]"
                    )));
                }
            }
            continue;
        }

        let (key, value) = split_kv(line)
            .ok_or_else(|| bad(format!("line {line_number}: expected `key = value`")))?;
        let key_l = key.to_ascii_lowercase();

        match section {
            Some(Section::Interface) => {
                let config = interface.as_mut().expect("interface section has a builder");
                match key_l.as_str() {
                    "privatekey" => set_once(
                        &mut config.private_key,
                        WgKey::parse(value, key, line_number)?,
                        key,
                        line_number,
                    )?,
                    "address" => {
                        config
                            .addresses
                            .extend(parse_list::<IpNet>(value, key, line_number)?)
                    }
                    "dns" => config
                        .dns
                        .extend(parse_list::<IpAddr>(value, key, line_number)?),
                    "mtu" => set_once(
                        &mut config.mtu,
                        parse_range::<u16>(value, key, line_number, 1, u16::MAX)?,
                        key,
                        line_number,
                    )?,
                    "listenport" => set_once(
                        &mut config.listen_port,
                        parse_range::<u16>(value, key, line_number, 0, u16::MAX)?,
                        key,
                        line_number,
                    )?,
                    _ => return Err(unsupported_key(key, line_number)),
                }
            }
            Some(Section::Peer) => {
                let peer = peers.last_mut().expect("peer section pushes a builder");
                match key_l.as_str() {
                    "publickey" => set_once(
                        &mut peer.public_key,
                        WgKey::parse(value, key, line_number)?,
                        key,
                        line_number,
                    )?,
                    "presharedkey" => set_once(
                        &mut peer.preshared_key,
                        WgKey::parse(value, key, line_number)?,
                        key,
                        line_number,
                    )?,
                    "endpoint" => set_once(
                        &mut peer.endpoint,
                        parse_endpoint(value).ok_or_else(|| {
                            if endpoint_has_ipv6_host(value) {
                                line_bad(line_number, key, "IPv6 endpoints are not supported yet")
                            } else {
                                line_bad(
                                    line_number,
                                    key,
                                    "must be a valid IP or DNS host and port",
                                )
                            }
                        })?,
                        key,
                        line_number,
                    )?,
                    "allowedips" => {
                        peer.allowed_ips
                            .extend(parse_list::<IpNet>(value, key, line_number)?)
                    }
                    "persistentkeepalive" => set_once(
                        &mut peer.persistent_keepalive,
                        parse_range::<u16>(value, key, line_number, 0, u16::MAX)?,
                        key,
                        line_number,
                    )?,
                    _ => return Err(unsupported_key(key, line_number)),
                }
            }
            None => {
                return Err(bad(format!(
                    "line {line_number}: `{key}` appears before any [Interface]/[Peer] section"
                )));
            }
        }
    }

    let interface = interface.ok_or_else(|| bad("missing [Interface] section".to_string()))?;
    let private_key = interface
        .private_key
        .ok_or_else(|| bad("[Interface] missing a PrivateKey".to_string()))?;
    if interface.dns.is_empty() {
        return Err(bad(
            "[Interface] has no DNS = resolver; DNS-through-tunnel cannot be enforced".to_string(),
        ));
    }
    if peers.is_empty() {
        return Err(bad("config defines no [Peer]".to_string()));
    }

    let mut built = Vec::with_capacity(peers.len());
    for (index, peer) in peers.into_iter().enumerate() {
        let number = index + 1;
        let public_key = peer
            .public_key
            .ok_or_else(|| bad(format!("[Peer] #{number} missing a PublicKey")))?;
        let (endpoint_host, endpoint_port) = peer
            .endpoint
            .ok_or_else(|| bad(format!("[Peer] #{number} has no Endpoint")))?;
        if peer.allowed_ips.is_empty() {
            return Err(bad(format!("[Peer] #{number} has no AllowedIPs")));
        }
        built.push(WgPeer {
            public_key,
            preshared_key: peer.preshared_key,
            endpoint_host,
            endpoint_port,
            allowed_ips: peer.allowed_ips,
            persistent_keepalive: peer.persistent_keepalive,
        });
    }

    Ok(WgConfig {
        private_key,
        addresses: interface.addresses,
        dns: interface.dns,
        mtu: interface.mtu,
        listen_port: interface.listen_port,
        peers: built,
    })
}

#[derive(Clone, Copy)]
enum Section {
    Interface,
    Peer,
}

#[derive(Default)]
struct InterfaceBuilder {
    private_key: Option<WgKey>,
    addresses: Vec<IpNet>,
    dns: Vec<IpAddr>,
    mtu: Option<u16>,
    listen_port: Option<u16>,
}

#[derive(Default)]
struct PeerBuilder {
    public_key: Option<WgKey>,
    preshared_key: Option<WgKey>,
    endpoint: Option<(String, u16)>,
    allowed_ips: Vec<IpNet>,
    persistent_keepalive: Option<u16>,
}

fn set_once<T>(slot: &mut Option<T>, value: T, key: &str, line: usize) -> Result<(), NetError> {
    if slot.replace(value).is_some() {
        return Err(bad(format!("line {line}: duplicate `{key}` key")));
    }
    Ok(())
}

fn parse_list<T>(value: &str, key: &str, line: usize) -> Result<Vec<T>, NetError>
where
    T: FromStr,
{
    value
        .split(',')
        .map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(line_bad(line, key, "contains an empty list entry"));
            }
            entry
                .parse::<T>()
                .map_err(|_| line_bad(line, key, "contains an invalid IP address or network"))
        })
        .collect()
}

fn parse_range<T>(value: &str, key: &str, line: usize, min: T, max: T) -> Result<T, NetError>
where
    T: FromStr + PartialOrd,
{
    let parsed = value
        .parse::<T>()
        .map_err(|_| line_bad(line, key, "must be an integer in range"))?;
    if parsed < min || parsed > max {
        return Err(line_bad(line, key, "must be an integer in range"));
    }
    Ok(parsed)
}

fn bad(reason: String) -> NetError {
    NetError::BadWgConfig(reason)
}

fn line_bad(line: usize, key: &str, reason: &str) -> NetError {
    bad(format!("line {line}: `{key}` {reason}"))
}

fn unsupported_key(key: &str, line: usize) -> NetError {
    bad(format!("line {line}: unsupported WireGuard key `{key}`"))
}

/// Strip a trailing `# comment`. wg-quick configs do not quote `#`.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

fn section_header(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
        Some(line[1..line.len() - 1].trim())
    } else {
        None
    }
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    let index = line.find('=')?;
    let key = line[..index].trim();
    let value = line[index + 1..].trim();
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Parse an endpoint and normalize its host. IPv6 peer endpoints are rejected
/// because the gateway kill switch currently permits IPv4 endpoints only.
fn parse_endpoint(value: &str) -> Option<(String, u16)> {
    let value = value.trim();
    if endpoint_has_ipv6_host(value) {
        return None;
    }
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = rest[..close].parse::<IpAddr>().ok()?;
        if !host.is_ipv6() {
            return None;
        }
        let port = rest[close + 1..].strip_prefix(':')?;
        (host.to_string(), port)
    } else {
        let (host, port) = value.rsplit_once(':')?;
        let normalized_host = if let Ok(ip) = host.parse::<IpAddr>() {
            ip.to_string()
        } else {
            if !valid_dns_name(host) {
                return None;
            }
            host.to_ascii_lowercase()
        };
        let port = port.parse::<u16>().ok()?;
        if port == 0 {
            return None;
        }
        return Some((normalized_host, port));
    };

    let port = port.parse::<u16>().ok()?;
    if port == 0 {
        return None;
    }
    Some((host, port))
}

fn endpoint_has_ipv6_host(value: &str) -> bool {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[') {
        return rest
            .find(']')
            .and_then(|close| rest[..close].parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_ipv6());
    }
    value
        .rsplit_once(':')
        .and_then(|(host, _)| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_ipv6())
}

fn valid_dns_name(host: &str) -> bool {
    if !host.is_ascii() || host.is_empty() || host.len() > 254 {
        return false;
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
    const VALID: &str = r#"
[Interface]
PrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=
Address = 10.99.0.2/32
DNS = 10.99.0.1, 1.1.1.1

[Peer]
PublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=
Endpoint = vpn.example.com:51820
AllowedIPs = 0.0.0.0/0, ::/0
PersistentKeepalive = 25
"#;

    #[test]
    fn parses_a_valid_config() {
        let cfg = parse_and_validate(VALID).expect("valid config");
        assert_eq!(cfg.peers.len(), 1);
        assert_eq!(cfg.peers[0].endpoint_host, "vpn.example.com");
        assert_eq!(cfg.peers[0].endpoint_port, 51820);
        assert_eq!(
            cfg.peers[0].allowed_ips,
            vec![
                "0.0.0.0/0".parse::<IpNet>().unwrap(),
                "::/0".parse().unwrap()
            ]
        );
        assert_eq!(cfg.tunnel_resolvers(), vec!["10.99.0.1", "1.1.1.1"]);
        assert_eq!(cfg.endpoint_ports(), vec![51820]);
    }

    #[test]
    fn debug_never_contains_key_material() {
        let cfg = parse_and_validate(VALID).unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("PrivateKey"));
        assert!(!dbg.contains(KEY));
        assert!(!dbg.contains("PublicKey"));
    }

    #[test]
    fn canonical_round_trip_preserves_normal_configuration() {
        let parsed = parse_and_validate(
            "# imported\n[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nAddress=10.99.0.2/32, fd00::2/128\nDNS=10.99.0.1, 2606:4700:4700::1111\nMTU=1420\nListenPort=51821\n[Peer]\nPublicKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nPresharedKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint=vpn.example.com:51820\nAllowedIPs=0.0.0.0/0, ::/0\nPersistentKeepalive=25\n",
        )
        .unwrap();
        let canonical = parsed.to_wg_quick();
        assert!(canonical.starts_with("[Interface]\nPrivateKey = "));
        assert!(canonical.contains("Endpoint = vpn.example.com:51820\n"));
        assert!(!canonical.contains("# imported"));
        assert_eq!(
            parse_and_validate(&canonical).unwrap().to_wg_quick(),
            canonical
        );
    }

    #[test]
    fn rejects_ipv6_peer_endpoints_with_actionable_error() {
        for endpoint in ["[2606:4700:4700::1111]:51820", "2606:4700:4700::1111:51820"] {
            let config = VALID.replace("vpn.example.com:51820", endpoint);
            let error = parse_and_validate(&config).unwrap_err().to_string();
            assert!(
                error.contains("IPv6 endpoints are not supported yet"),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_hook_and_unsupported_keys_with_name_and_line() {
        for key in [
            "PostUp",
            "PreUp",
            "PostDown",
            "PreDown",
            "SaveConfig",
            "Table",
            "FwMark",
        ] {
            let input = format!(
                "[Interface]\nPrivateKey = {KEY}\nDNS = 1.1.1.1\n{key} = touch /tmp/escaped\n[Peer]\nPublicKey = {KEY}\nEndpoint = vpn.example.com:51820\nAllowedIPs = 0.0.0.0/0\n"
            );
            let error = parse_and_validate(&input).unwrap_err().to_string();
            assert!(error.contains(key), "missing rejected key {key}: {error}");
            assert!(error.contains("line 4"), "missing source line: {error}");
        }
    }

    #[test]
    fn rejects_injection_strings_in_dns_and_allowed_ips() {
        let dns = VALID.replace(
            "DNS = 10.99.0.1, 1.1.1.1",
            "DNS = 1.1.1.1; udp dport 53 accept",
        );
        assert!(parse_and_validate(&dns)
            .unwrap_err()
            .to_string()
            .contains("DNS"));

        let allowed = VALID.replace(
            "AllowedIPs = 0.0.0.0/0, ::/0",
            "AllowedIPs = 0.0.0.0/0 accept; ip daddr 0.0.0.0/0",
        );
        assert!(parse_and_validate(&allowed)
            .unwrap_err()
            .to_string()
            .contains("AllowedIPs"));
    }

    #[test]
    fn rejects_hostname_dns_and_malformed_or_out_of_range_values() {
        let hostname_dns = VALID.replace("DNS = 10.99.0.1, 1.1.1.1", "DNS = resolver.example");
        assert!(parse_and_validate(&hostname_dns).is_err());
        let bad_mtu = VALID.replace("Address = 10.99.0.2/32", "MTU = 65536");
        assert!(parse_and_validate(&bad_mtu).is_err());
        let bad_key = VALID.replace(&format!("PrivateKey = {KEY}"), "PrivateKey = short");
        assert!(parse_and_validate(&bad_key).is_err());
    }

    #[test]
    fn canonicalizes_endpoint_dns_case() {
        let config = format!(
            "[Interface]\nPrivateKey = {KEY}\nDNS = 10.0.0.1\n[Peer]\nPublicKey = {KEY}\nEndpoint = VPN.Example.COM:51820\nAllowedIPs = 0.0.0.0/0\n"
        );
        let cfg = parse_and_validate(&config).unwrap();
        assert_eq!(cfg.peers[0].endpoint_host, "vpn.example.com");
        assert_eq!(cfg.peers[0].endpoint_port, 51820);
    }
}
