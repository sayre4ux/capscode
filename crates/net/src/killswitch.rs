//! nftables kill-switch ruleset generation .
//!
//! The ruleset is **default-DROP** on `output`/`forward` and is the single
//! security control that keeps the capsule from leaking outside the tunnel or
//! reaching the host/LAN. It is rendered here (source of truth, unit-tested) and
//! handed to the gateway container, which applies it verbatim with `nft -f`. A
//! byte-identical copy is committed at `images/killswitch.nft` and a test
//! (`committed_reference_matches_render`) fails if the two drift.
//!
//! ## Values known only at boot use nft sets, not string interpolation
//!
//! Three facts are unknown when we render on the host and are filled by the
//! gateway entrypoint into named sets *after* the tunnel is up:
//!
//! * `@wg_peer` — the peer endpoint address actually negotiated (`wg show`),
//!   correct even when the config `Endpoint` is a hostname.
//! * `@host_gw` — the Docker/OrbStack host-gateway address(es), which differ per
//!   runtime. The entrypoint fills this from BOTH the default-route gateway AND
//!   the resolved per-runtime host aliases (`host.docker.internal`,
//!   `host.orbstack.internal`, `gateway.docker.internal`). OrbStack's `198.19/16`
//!   host range is **not** RFC1918, so as a second, resolution-independent layer
//!   it is also baked into the static containment set below (defense-in-depth: the
//!   host is dropped even if none of the aliases resolve).
//! * `@sinkhole4` — resolved telemetry addresses (best effort; see [`crate::sinkhole`]).
//!
//! Everything else — the RFC1918/CGNAT/link-local drop set, the DNS pinning, the
//! peer port, extra operator CIDRs — is a compile-time constant or comes from the
//! validated [`crate::wg::WgConfig`] and is interpolated here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;

use crate::wg::WgConfig;

/// RFC1918 + CGNAT (`100.64/10`) + link-local (`169.254/16`) + the OrbStack
/// host range (`198.19/16`). These plus the per-runtime `@host_gw` set are what
/// stop the capsule reaching the host or LAN (SSRF / cloud-metadata / lateral
/// movement). `169.254.169.254` (cloud metadata) is inside `169.254/16` and thus
/// covered.
///
/// `198.19.0.0/16` is the reserved benchmarking range OrbStack uses for the host;
/// it is **not** RFC1918, so without this entry a capsule could reach the host on
/// OrbStack whenever the host-alias resolution in the entrypoint came up empty.
/// Baking it in here makes host containment hold regardless of `@host_gw`
/// resolution — the drop is enforced in **both** modes, ahead of any accept.
const CONTAINMENT_CIDRS: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "169.254.0.0/16",
    "198.19.0.0/16",
];

/// IPv6 host/LAN/metadata ranges: link-local (`fe80::/10`) and ULA (`fc00::/7`,
/// which covers `fd00::/8` — Docker/OrbStack host ULAs and the IPv6 metadata
/// endpoint `fd00:ec2::254`). The v6 mirror of `CONTAINMENT_CIDRS`.
const CONTAINMENT_CIDRS6: &[&str] = &["fe80::/10", "fc00::/7"];

/// Rendering inputs for [`render`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KillswitchParams {
    pub mode: Mode,
    /// Additional operator-supplied CIDRs to drop (`NetworkConfig::extra_blocked_cidrs`).
    pub extra_blocked_cidrs: Vec<IpNet>,
    /// Emit the `@sinkhole4` set + drop rule.
    pub sinkhole: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A validated WireGuard tunnel is in play: force **all** egress through
    /// `wg0`, pin DNS to the tunnel resolver(s), allow only the handshake off-tunnel.
    Tunnel {
        /// UDP ports the peer handshake may use.
        peer_ports: Vec<u16>,
        /// Resolver addresses reachable over `wg0` for `:53`.
        resolvers: Vec<IpAddr>,
    },
    /// No tunnel (`wireguard_conf = None`). VPN is optional; **containment is
    /// not** — RFC1918/host/sinkhole still drop, but ordinary egress leaves via
    /// the default interface and DNS is not pinned.
    ContainmentOnly,
}

impl KillswitchParams {
    /// Build params from a validated WG config (tunnel mode).
    pub fn tunnel(cfg: &WgConfig, extra_blocked_cidrs: Vec<IpNet>, sinkhole: bool) -> Self {
        KillswitchParams {
            mode: Mode::Tunnel {
                peer_ports: cfg.endpoint_ports(),
                resolvers: cfg.dns_addresses().to_vec(),
            },
            extra_blocked_cidrs,
            sinkhole,
        }
    }

    /// Build params for the no-VPN containment-only path.
    pub fn containment_only(extra_blocked_cidrs: Vec<IpNet>, sinkhole: bool) -> Self {
        KillswitchParams {
            mode: Mode::ContainmentOnly,
            extra_blocked_cidrs,
            sinkhole,
        }
    }
}

/// Render the full `nft -f` ruleset. Deterministic: identical params always
/// produce byte-identical output (no timestamps, no map iteration).
pub fn render(params: &KillswitchParams) -> String {
    let mut s = String::with_capacity(2048);
    let extra_v4: Vec<String> = params
        .extra_blocked_cidrs
        .iter()
        .filter(|cidr| cidr.addr().is_ipv4())
        .map(ToString::to_string)
        .collect();
    let extra_v6: Vec<String> = params
        .extra_blocked_cidrs
        .iter()
        .filter(|cidr| cidr.addr().is_ipv6())
        .map(ToString::to_string)
        .collect();

    s.push_str("#!/usr/sbin/nft -f\n");
    s.push_str("# CapsCode kill switch — generated by capscode-net.\n");
    s.push_str("# Default-DROP egress. Boot-filled sets: @wg_peer, @host_gw, @sinkhole4.\n");
    s.push_str("# Do not edit by hand; edit crates/net/src/killswitch.rs.\n\n");

    // Idempotent apply WITHOUT `flush ruleset`: flushing would also wipe the
    // container runtime's own DNS/NAT rules in this shared netns. Ensure-then-
    // replace only our own table.
    s.push_str("table inet capscode\n");
    s.push_str("delete table inet capscode\n\n");

    s.push_str("table inet capscode {\n");

    // --- sets --------------------------------------------------------------
    s.push_str("\tset containment4 {\n");
    s.push_str("\t\ttype ipv4_addr\n");
    s.push_str("\t\tflags interval\n");
    s.push_str("\t\telements = { ");
    s.push_str(&CONTAINMENT_CIDRS.join(", "));
    s.push_str(" }\n\t}\n\n");

    s.push_str("\tset containment6 {\n");
    s.push_str("\t\ttype ipv6_addr\n");
    s.push_str("\t\tflags interval\n");
    s.push_str("\t\telements = { ");
    s.push_str(&CONTAINMENT_CIDRS6.join(", "));
    s.push_str(" }\n\t}\n\n");

    if !extra_v4.is_empty() {
        s.push_str("\tset extra_blocked {\n");
        s.push_str("\t\ttype ipv4_addr\n");
        s.push_str("\t\tflags interval\n");
        s.push_str("\t\telements = { ");
        s.push_str(&extra_v4.join(", "));
        s.push_str(" }\n\t}\n\n");
    }
    if !extra_v6.is_empty() {
        s.push_str("\tset extra_blocked6 {\n");
        s.push_str("\t\ttype ipv6_addr\n");
        s.push_str("\t\tflags interval\n");
        s.push_str("\t\telements = { ");
        s.push_str(&extra_v6.join(", "));
        s.push_str(" }\n\t}\n\n");
    }

    // host-gateway address(es), filled at boot per runtime.
    s.push_str("\tset host_gw {\n\t\ttype ipv4_addr\n\t}\n\n");

    if params.sinkhole {
        s.push_str("\tset sinkhole4 {\n\t\ttype ipv4_addr\n\t}\n\n");
    }

    if matches!(params.mode, Mode::Tunnel { .. }) {
        s.push_str("\tset wg_peer {\n\t\ttype ipv4_addr\n\t}\n\n");
    }

    // --- input -------------------------------------------------------------
    // Default-drop inbound too: the only thing that must reach us is loopback
    // (broker on 127.0.0.1) and return traffic (incl. WG handshake replies).
    s.push_str("\tchain input {\n");
    s.push_str("\t\ttype filter hook input priority 0; policy drop;\n");
    s.push_str("\t\tiif \"lo\" accept\n");
    s.push_str("\t\tct state established,related accept\n");
    if let Mode::Tunnel { peer_ports, .. } = &params.mode {
        for port in peer_ports {
            s.push_str(&format!("\t\tip saddr @wg_peer udp sport {port} accept\n"));
        }
    }
    s.push_str("\t}\n\n");

    // --- output (the kill switch) -----------------------------------------
    // DECISION: no `ct state established,related accept` in this chain. Every
    // accept below matches on fields fixed for a flow's life (destination, port)
    // plus the output interface, so established packets of an allowed flow are
    // still accepted, but an established flow whose route moves off `wg0` (or
    // onto a contained address) is dropped like a new one.
    s.push_str("\tchain output {\n");
    s.push_str("\t\ttype filter hook output priority 0; policy drop;\n");
    s.push_str("\t\toif \"lo\" accept\n\n");

    match &params.mode {
        Mode::Tunnel {
            peer_ports,
            resolvers,
        } => {
            s.push_str("\t\t# WireGuard handshake to the negotiated peer endpoint (off-tunnel).\n");
            for port in peer_ports {
                s.push_str(&format!("\t\tip daddr @wg_peer udp dport {port} accept\n"));
            }
            s.push('\n');
            s.push_str("\t\t# DNS ONLY to the tunnel resolver(s), ONLY over wg0.\n");
            // Split resolvers by family: an IPv4 set must use `ip daddr` and an
            // IPv6 set `ip6 daddr` — nft rejects an IPv6 address inside an `ip`
            // (v4) match (ProtonVPN, e.g., hands out both a v4 and a v6 resolver).
            let v4: Vec<String> = resolvers
                .iter()
                .filter(|resolver| resolver.is_ipv4())
                .map(ToString::to_string)
                .collect();
            let v6: Vec<String> = resolvers
                .iter()
                .filter(|resolver| resolver.is_ipv6())
                .map(ToString::to_string)
                .collect();
            if !v4.is_empty() {
                let set = brace_set(&v4);
                s.push_str(&format!(
                    "\t\tip daddr {set} udp dport 53 oifname \"wg0\" accept\n"
                ));
                s.push_str(&format!(
                    "\t\tip daddr {set} tcp dport 53 oifname \"wg0\" accept\n"
                ));
            }
            if !v6.is_empty() {
                let set = brace_set(&v6);
                s.push_str(&format!(
                    "\t\tip6 daddr {set} udp dport 53 oifname \"wg0\" accept\n"
                ));
                s.push_str(&format!(
                    "\t\tip6 daddr {set} tcp dport 53 oifname \"wg0\" accept\n"
                ));
            }
            // Family-agnostic catch-all: everything else on :53 is dropped.
            s.push_str("\t\tudp dport 53 drop\n");
            s.push_str("\t\ttcp dport 53 drop\n\n");

            emit_containment_drops(&mut s, params, !extra_v4.is_empty(), !extra_v6.is_empty());

            s.push_str("\t\t# All remaining egress MUST exit the tunnel, established flows too.\n");
            s.push_str("\t\toifname \"wg0\" accept\n");
            s.push_str("\t\t# Anything else falls through to `policy drop` (fail-closed).\n");
        }
        Mode::ContainmentOnly => {
            s.push_str("\t\t# No tunnel: DNS uses the ambient resolver (loopback/embedded\n");
            s.push_str("\t\t# runtime DNS is covered by `oif lo`); it is not pinned.\n\n");

            emit_containment_drops(&mut s, params, !extra_v4.is_empty(), !extra_v6.is_empty());

            s.push_str("\t\t# Containment holds; ordinary egress leaves the default interface.\n");
            s.push_str("\t\taccept\n");
        }
    }

    s.push_str("\t}\n\n");

    // --- forward -----------------------------------------------------------
    // Same order as `output`: containment first, then the mode's accept. In
    // tunnel mode established forwarded flows get no state exemption from the
    // `wg0` interface check.
    s.push_str("\tchain forward {\n");
    s.push_str("\t\ttype filter hook forward priority 0; policy drop;\n");
    emit_containment_drops(&mut s, params, !extra_v4.is_empty(), !extra_v6.is_empty());
    match params.mode {
        Mode::Tunnel { .. } => s.push_str("\t\toifname \"wg0\" accept\n"),
        Mode::ContainmentOnly => s.push_str("\t\tct state established,related accept\n"),
    }
    s.push_str("\t}\n");

    s.push_str("}\n");
    s
}

/// Emit the RFC1918 / host-gateway / sinkhole / extra-CIDR drops shared by both
/// modes. These come *after* the resolver/handshake allows (a tunnel resolver is
/// frequently itself an RFC1918/CGNAT address) so those flows are accepted first.
fn emit_containment_drops(
    s: &mut String,
    params: &KillswitchParams,
    has_extra_v4: bool,
    has_extra_v6: bool,
) {
    s.push_str("\t\t# Containment: never reach host, LAN, CGNAT, link-local, metadata.\n");
    s.push_str("\t\tip daddr @containment4 drop\n");
    s.push_str("\t\tip6 daddr @containment6 drop\n");
    if has_extra_v4 {
        s.push_str("\t\tip daddr @extra_blocked drop\n");
    }
    if has_extra_v6 {
        s.push_str("\t\tip6 daddr @extra_blocked6 drop\n");
    }
    s.push_str("\t\tip daddr @host_gw drop\n");
    if params.sinkhole {
        s.push_str("\t\t# Telemetry sinkhole (independent of any tool telemetry setting).\n");
        s.push_str("\t\tip daddr @sinkhole4 drop\n");
    }
    s.push('\n');
}

/// Render an nft anonymous set literal, or a bare element when there is exactly
/// one (nft accepts both; a single bare element reads cleaner).
fn brace_set(items: &[String]) -> String {
    if items.len() == 1 {
        items[0].clone()
    } else {
        format!("{{ {} }}", items.join(", "))
    }
}

/// Pure model of the `output`-chain containment decision for an IPv4 destination,
/// lets tests check the drop semantics without a live `nft` engine.
///
/// Returns `true` when `addr` would hit a containment drop *before* the terminal
/// accept — i.e. it falls inside any [`CONTAINMENT_CIDRS`] range (RFC1918 / CGNAT
/// / link-local / metadata / the OrbStack `198.19/16` host range) or matches one
/// of the boot-filled `@host_gw` addresses. This encodes exactly the same set the
/// rendered ruleset drops, so a host address the entrypoint resolved *or* the
/// baked `198.19/16` range is provably contained in both `tunnel` and
/// `containment-only` mode.
pub fn is_contained_ipv4(addr: Ipv4Addr, host_gw: &[Ipv4Addr]) -> bool {
    if host_gw.contains(&addr) {
        return true;
    }
    CONTAINMENT_CIDRS
        .iter()
        .any(|cidr| cidr_contains_v4(cidr, addr))
}

/// Whether `addr` is inside the IPv4 CIDR `cidr` (`"base/prefix"`). A malformed
/// CIDR never matches (fail-closed for the caller's purposes: the constant set is
/// well-formed, so this only guards test/typo inputs).
fn cidr_contains_v4(cidr: &str, addr: Ipv4Addr) -> bool {
    let (base, prefix) = match cidr.split_once('/') {
        Some((b, p)) => (b, p),
        None => (cidr, "32"),
    };
    let base: Ipv4Addr = match base.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let prefix: u32 = match prefix.parse() {
        Ok(p) if p <= 32 => p,
        _ => return false,
    };
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from(base) & mask) == (u32::from(addr) & mask)
}

/// Pure model of the `output`-chain containment decision for an IPv6 destination.
/// Returns `true` when `addr` falls inside any [`CONTAINMENT_CIDRS6`] range
/// (link-local `fe80::/10` or ULA `fc00::/7`, covering Docker/OrbStack host ULAs
/// and the IPv6 metadata endpoint `fd00:ec2::254`).
pub fn is_contained_ipv6(addr: Ipv6Addr) -> bool {
    CONTAINMENT_CIDRS6.iter().any(|c| cidr_contains_v6(c, addr))
}

/// Whether `addr` is inside the IPv6 CIDR `cidr` (`"base/prefix"`). A malformed
/// CIDR never matches (fail-closed for the caller's purposes: the constant set is
/// well-formed, so this only guards test/typo inputs).
fn cidr_contains_v6(cidr: &str, addr: Ipv6Addr) -> bool {
    let (base, prefix) = match cidr.split_once('/') {
        Some((b, p)) => (b, p),
        None => (cidr, "128"),
    };
    let base: Ipv6Addr = match base.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let prefix: u32 = match prefix.parse() {
        Ok(p) if p <= 128 => p,
        _ => return false,
    };
    let mask = if prefix == 0 {
        0u128
    } else {
        u128::MAX << (128 - prefix)
    };
    (u128::from(base) & mask) == (u128::from(addr) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wg;

    fn tunnel_params() -> KillswitchParams {
        let cfg = wg::parse_and_validate(
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 10.99.0.1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = vpn.example.com:51820\nAllowedIPs = 0.0.0.0/0\n",
        )
        .unwrap();
        KillswitchParams::tunnel(&cfg, vec![], true)
    }

    #[test]
    fn tunnel_dns_splits_ipv4_and_ipv6_resolvers_by_family() {
        // A mixed v4/v6 resolver set (as ProtonVPN hands out) must render as
        // separate `ip daddr` / `ip6 daddr` rules: nft rejects an IPv6 address
        // inside an `ip` (v4) match, which crashed the kill switch and took the
        // whole tunnel bring-up down fail-closed.
        let cfg = wg::parse_and_validate(
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 10.2.0.1, 2a07:b944::2:1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = 1.2.3.4:51820\nAllowedIPs = 0.0.0.0/0\n",
        )
        .unwrap();
        let r = render(&KillswitchParams::tunnel(&cfg, vec![], true));
        assert!(r.contains("ip daddr 10.2.0.1 udp dport 53 oifname \"wg0\" accept"));
        assert!(r.contains("ip6 daddr 2a07:b944::2:1 udp dport 53 oifname \"wg0\" accept"));
        // The buggy form mixed both families into a single IPv4 set.
        assert!(!r.contains("ip daddr { 10.2.0.1, 2a07:b944::2:1 }"));
    }

    #[test]
    fn output_is_default_drop_in_both_modes() {
        let t = render(&tunnel_params());
        assert!(t.contains("hook output priority 0; policy drop;"));
        let c = render(&KillswitchParams::containment_only(vec![], true));
        assert!(c.contains("hook output priority 0; policy drop;"));
    }

    #[test]
    fn tunnel_forces_egress_via_wg0_and_pins_dns() {
        let r = render(&tunnel_params());
        assert!(r.contains("oifname \"wg0\" accept"));
        assert!(r.contains("ip daddr 10.99.0.1 udp dport 53 oifname \"wg0\" accept"));
        assert!(r.contains("udp dport 53 drop"));
        assert!(r.contains("tcp dport 53 drop"));
        assert!(r.contains("ip daddr @wg_peer udp dport 51820 accept"));
    }

    #[test]
    fn containment_only_keeps_drops_but_not_dns_pin_and_allows_default_egress() {
        let r = render(&KillswitchParams::containment_only(vec![], true));
        assert!(r.contains("ip daddr @containment4 drop"));
        assert!(r.contains("ip daddr @host_gw drop"));
        assert!(r.contains("ip daddr @sinkhole4 drop"));
        // No DNS pinning / no forced wg0 in containment-only mode.
        assert!(!r.contains("udp dport 53 drop"));
        assert!(!r.contains("oifname \"wg0\" accept"));
        // Ordinary egress is permitted after the drops.
        assert!(r.trim_end().contains("\t\taccept"));
        // No wg_peer set without a tunnel.
        assert!(!r.contains("set wg_peer"));
    }

    #[test]
    fn rfc1918_cgnat_linklocal_all_present() {
        let r = render(&tunnel_params());
        for c in [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "100.64.0.0/10",
            "169.254.0.0/16",
            "198.19.0.0/16",
        ] {
            assert!(r.contains(c), "missing containment cidr {c}");
        }
    }

    /// The OrbStack host range is in the static
    /// containment set (dropped regardless of any boot-time alias resolution) and
    /// is rendered into every ruleset.
    #[test]
    fn orbstack_host_range_in_containment_set() {
        assert!(
            CONTAINMENT_CIDRS.contains(&"198.19.0.0/16"),
            "198.19.0.0/16 (OrbStack host range) must be in the containment set"
        );
        // And it must appear in a containment-only render too, not just tunnel.
        let c = render(&KillswitchParams::containment_only(vec![], true));
        assert!(c.contains("198.19.0.0/16"));
    }

    /// The combined defense layers, exercised through the pure containment
    /// interpreter: in containment-only mode a packet to a simulated host alias is
    /// dropped whether the host is OrbStack's `198.19/16` (baked layer b) or an
    /// arbitrary non-RFC1918 address the entrypoint resolved into `@host_gw`
    /// (layer a). Non-host public traffic is NOT contained (ordinary egress).
    #[test]
    fn containment_only_drops_host_alias_ipv4() {
        // Layer (b): OrbStack host alias in 198.19/16 — dropped with an EMPTY
        // @host_gw, i.e. even when no alias resolved at boot.
        assert!(is_contained_ipv4("198.19.194.3".parse().unwrap(), &[]));
        // Docker Desktop's host is in 192.168/16 — already covered.
        assert!(is_contained_ipv4("192.168.65.2".parse().unwrap(), &[]));
        // Cloud metadata endpoint (inside 169.254/16).
        assert!(is_contained_ipv4("169.254.169.254".parse().unwrap(), &[]));
        // Layer (a): a non-RFC1918, non-198.19 host alias the entrypoint resolved
        // (host.docker.internal / host.orbstack.internal) is dropped via @host_gw.
        let resolved_alias: Ipv4Addr = "203.0.113.7".parse().unwrap();
        assert!(!is_contained_ipv4(resolved_alias, &[]));
        assert!(is_contained_ipv4(resolved_alias, &[resolved_alias]));
        // Ordinary public egress is NOT contained (containment-only permits it).
        assert!(!is_contained_ipv4("1.1.1.1".parse().unwrap(), &[]));
    }

    /// In containment-only mode every containment
    /// drop (RFC1918/198.19 set AND the per-runtime host-gateway set) is emitted
    /// BEFORE the terminal catch-all accept, so host/LAN egress can never fall
    /// through to accept.
    #[test]
    fn containment_only_drops_precede_terminal_accept() {
        let r = render(&KillswitchParams::containment_only(vec![], true));
        let containment_drop = r.find("ip daddr @containment4 drop").unwrap();
        let host_gw_drop = r.find("ip daddr @host_gw drop").unwrap();
        // The terminal catch-all is a bare `accept` line (distinct from the
        // `oif "lo" accept` allow above it).
        let terminal_accept = r.rfind("\t\taccept\n").unwrap();
        assert!(
            containment_drop < terminal_accept,
            "containment drop must precede the terminal accept"
        );
        assert!(
            host_gw_drop < terminal_accept,
            "host-gateway drop must precede the terminal accept"
        );
    }

    /// IPv6 containment: link-local + ULA (incl. Docker/OrbStack host ULAs and
    /// the IPv6 metadata endpoint) are contained; public v6 is not.
    #[test]
    fn is_contained_ipv6_link_local_ula_and_not_public() {
        assert!(is_contained_ipv6("fe80::1".parse().unwrap()));
        assert!(is_contained_ipv6("fd00:ec2::254".parse().unwrap()));
        assert!(is_contained_ipv6("fd12:3456:789a::1".parse().unwrap()));
        assert!(!is_contained_ipv6("2606:4700:4700::1111".parse().unwrap()));
    }

    /// Both modes emit the v6 containment set and drop rule (defense-in-depth
    /// for tunnel; required for containment-only which would otherwise accept
    /// any IPv6 packet that matches none of the v4-only drops).
    #[test]
    fn both_modes_emit_containment6_set_and_drop() {
        let t = render(&tunnel_params());
        let c = render(&KillswitchParams::containment_only(vec![], true));
        for r in [&t, &c] {
            assert!(r.contains("set containment6"), "missing containment6 set");
            assert!(
                r.contains("ip6 daddr @containment6 drop"),
                "missing containment6 drop"
            );
            assert!(r.contains("fe80::/10"));
            assert!(r.contains("fc00::/7"));
        }
    }

    /// Rule lines of `chain`, without comments, blank lines, or the hook line.
    fn chain_rules<'a>(rendered: &'a str, chain: &str) -> Vec<&'a str> {
        let header = format!("\tchain {chain} {{\n");
        let start = rendered.find(&header).expect("chain present") + header.len();
        let body = &rendered[start..];
        let end = body.find("\n\t}").expect("chain closes");
        body[..end]
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("type "))
            .collect()
    }

    fn position(rules: &[&str], rule: &str) -> usize {
        rules
            .iter()
            .position(|candidate| *candidate == rule)
            .unwrap_or_else(|| panic!("missing rule `{rule}` in {rules:#?}"))
    }

    /// A packet as the `output`/`forward` hooks see it.
    struct Packet {
        oif: &'static str,
        daddr: IpAddr,
        udp: bool,
        dport: u16,
        established: bool,
    }

    /// Whether `addr` is in the set `name` as `rendered` declares it. A set
    /// rendered with `elements` is matched against exactly those elements. The
    /// boot-filled sets take their contents from `wg_peer` / `host_gw`, and
    /// `@sinkhole4` is empty (its resolution is best effort at boot). Any other
    /// set, or one the ruleset does not declare, panics.
    fn set_contains(
        rendered: &str,
        name: &str,
        addr: IpAddr,
        wg_peer: &[Ipv4Addr],
        host_gw: &[Ipv4Addr],
    ) -> bool {
        let header = format!("\tset {name} {{\n");
        let start = rendered
            .find(&header)
            .unwrap_or_else(|| panic!("ruleset references undeclared set @{name}"))
            + header.len();
        let body = &rendered[start..start + rendered[start..].find("\n\t}").unwrap()];
        if let Some(elements) = body
            .lines()
            .find_map(|line| line.trim().strip_prefix("elements = { "))
        {
            return elements.trim_end_matches(" }").split(", ").any(|element| {
                match element.parse::<IpNet>() {
                    Ok(net) => net.contains(&addr),
                    Err(_) => element.parse::<IpAddr>().unwrap() == addr,
                }
            });
        }
        match (name, addr) {
            ("wg_peer", IpAddr::V4(addr)) => wg_peer.contains(&addr),
            ("host_gw", IpAddr::V4(addr)) => host_gw.contains(&addr),
            ("sinkhole4", _) => false,
            _ => panic!("evaluator has no contents for set @{name}"),
        }
    }

    /// Minimal evaluator for exactly the rule forms [`render`] emits. It panics
    /// on any token it does not know, so a new rule form fails the tests instead
    /// of being silently skipped. Sets are resolved by [`set_contains`].
    fn verdict(
        rendered: &str,
        rules: &[&str],
        packet: &Packet,
        wg_peer: &[Ipv4Addr],
        host_gw: &[Ipv4Addr],
    ) -> &'static str {
        for rule in rules {
            let tokens: Vec<&str> = rule.split_whitespace().collect();
            let mut matches = true;
            let mut index = 0;
            let verdict = loop {
                match tokens[index] {
                    "accept" => break "accept",
                    "drop" => break "drop",
                    "oif" | "oifname" => {
                        matches &= tokens[index + 1].trim_matches('"') == packet.oif;
                        index += 2;
                    }
                    "ct" => {
                        assert_eq!(tokens[index + 1], "state");
                        assert_eq!(tokens[index + 2], "established,related");
                        matches &= packet.established;
                        index += 3;
                    }
                    "udp" | "tcp" => {
                        assert_eq!(tokens[index + 1], "dport");
                        let port: u16 = tokens[index + 2].parse().unwrap();
                        matches &= (tokens[index] == "udp") == packet.udp && port == packet.dport;
                        index += 3;
                    }
                    family @ ("ip" | "ip6") => {
                        assert_eq!(tokens[index + 1], "daddr");
                        let target = tokens[index + 2];
                        let same_family = matches!(
                            (family, packet.daddr),
                            ("ip", IpAddr::V4(_)) | ("ip6", IpAddr::V6(_))
                        );
                        let hit = same_family
                            && match target.strip_prefix('@') {
                                Some(set) => {
                                    set_contains(rendered, set, packet.daddr, wg_peer, host_gw)
                                }
                                None => target.parse::<IpAddr>().unwrap() == packet.daddr,
                            };
                        matches &= hit;
                        index += 3;
                    }
                    other => panic!("evaluator does not know `{other}` in `{rule}`"),
                }
            };
            if matches {
                return verdict;
            }
        }
        "drop"
    }

    fn tcp(oif: &'static str, daddr: &str, established: bool) -> Packet {
        Packet {
            oif,
            daddr: daddr.parse().unwrap(),
            udp: false,
            dport: 443,
            established,
        }
    }

    /// Tunnel mode: conntrack state never exempts a packet from the interface
    /// check. An established flow whose route falls back to the uplink (wg0's
    /// route gone mid-session) is dropped in both `output` and `forward`; the
    /// same flow over wg0 and the peer handshake on the uplink still pass.
    #[test]
    fn tunnel_established_flow_off_wg0_is_dropped() {
        let rendered = render(&tunnel_params());
        let peer: Ipv4Addr = "198.51.100.7".parse().unwrap();
        for chain in ["output", "forward"] {
            let rules = chain_rules(&rendered, chain);
            for established in [false, true] {
                assert_eq!(
                    verdict(
                        &rendered,
                        &rules,
                        &tcp("eth0", "1.1.1.1", established),
                        &[peer],
                        &[]
                    ),
                    "drop",
                    "{chain}: uplink egress (established={established}) must drop"
                );
                assert_eq!(
                    verdict(
                        &rendered,
                        &rules,
                        &tcp("eth0", "2606:4700::1111", established),
                        &[peer],
                        &[]
                    ),
                    "drop",
                    "{chain}: uplink IPv6 egress (established={established}) must drop"
                );
                assert_eq!(
                    verdict(
                        &rendered,
                        &rules,
                        &tcp("wg0", "1.1.1.1", established),
                        &[peer],
                        &[]
                    ),
                    "accept",
                    "{chain}: tunnel egress (established={established}) must pass"
                );
                assert_eq!(
                    verdict(
                        &rendered,
                        &rules,
                        &tcp("wg0", "10.0.0.5", established),
                        &[peer],
                        &[]
                    ),
                    "drop",
                    "{chain}: contained address over wg0 (established={established}) must drop"
                );
            }
        }
        let output = chain_rules(&rendered, "output");
        let handshake = Packet {
            oif: "eth0",
            daddr: IpAddr::V4(peer),
            udp: true,
            dport: 51820,
            established: true,
        };
        assert_eq!(
            verdict(&rendered, &output, &handshake, &[peer], &[]),
            "accept"
        );
        let wrong_peer = Packet {
            daddr: "203.0.113.9".parse().unwrap(),
            ..handshake
        };
        assert_eq!(
            verdict(&rendered, &output, &wrong_peer, &[peer], &[]),
            "drop"
        );
        let resolver_dns = Packet {
            oif: "wg0",
            daddr: "10.99.0.1".parse().unwrap(),
            udp: true,
            dport: 53,
            established: true,
        };
        assert_eq!(
            verdict(&rendered, &output, &resolver_dns, &[peer], &[]),
            "accept"
        );
        let uplink_dns = Packet {
            oif: "eth0",
            ..resolver_dns
        };
        assert_eq!(
            verdict(&rendered, &output, &uplink_dns, &[peer], &[]),
            "drop"
        );
    }

    /// Containment-only: an established flow to the host, LAN, or metadata
    /// range is dropped (containment comes before any state accept), while
    /// ordinary public egress still passes.
    #[test]
    fn containment_only_established_flow_to_contained_address_is_dropped() {
        let rendered = render(&KillswitchParams::containment_only(vec![], true));
        let host: Ipv4Addr = "203.0.113.7".parse().unwrap();
        for chain in ["output", "forward"] {
            let rules = chain_rules(&rendered, chain);
            for daddr in [
                "192.168.65.2",
                "169.254.169.254",
                "198.19.194.3",
                "fd00:ec2::254",
            ] {
                assert_eq!(
                    verdict(&rendered, &rules, &tcp("eth0", daddr, true), &[], &[host]),
                    "drop",
                    "{chain}: established flow to {daddr} must drop"
                );
            }
            assert_eq!(
                verdict(
                    &rendered,
                    &rules,
                    &tcp("eth0", "203.0.113.7", true),
                    &[],
                    &[host]
                ),
                "drop",
                "{chain}: established flow to @host_gw must drop"
            );
        }
        let output = chain_rules(&rendered, "output");
        for established in [false, true] {
            assert_eq!(
                verdict(
                    &rendered,
                    &output,
                    &tcp("eth0", "1.1.1.1", established),
                    &[],
                    &[host]
                ),
                "accept"
            );
        }
    }

    /// Operator extra CIDRs (v4 and v6) are dropped in both modes and both egress
    /// chains, ahead of the terminal accept: the control address outside them
    /// passes on the same path, so ignoring `@extra_blocked` would fail this.
    #[test]
    fn extra_blocked_cidrs_drop_in_both_modes() {
        let cfg = wg::parse_and_validate(
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 10.99.0.1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = vpn.example.com:51820\nAllowedIPs = 0.0.0.0/0, ::/0\n",
        )
        .unwrap();
        let extra: Vec<IpNet> = vec![
            "203.0.113.0/24".parse().unwrap(),
            "2001:db8::/32".parse().unwrap(),
        ];
        for (params, oif) in [
            (KillswitchParams::tunnel(&cfg, extra.clone(), true), "wg0"),
            (
                KillswitchParams::containment_only(extra.clone(), true),
                "eth0",
            ),
        ] {
            let rendered = render(&params);
            for chain in ["output", "forward"] {
                let rules = chain_rules(&rendered, chain);
                for blocked in ["203.0.113.9", "2001:db8::1"] {
                    for established in [false, true] {
                        assert_eq!(
                            verdict(&rendered, &rules, &tcp(oif, blocked, established), &[], &[]),
                            "drop",
                            "{chain} via {oif}: {blocked} (established={established}) must drop"
                        );
                    }
                }
                for allowed in ["198.51.100.9", "2606:4700::1111"] {
                    assert_eq!(
                        verdict(&rendered, &rules, &tcp(oif, allowed, true), &[], &[]),
                        "accept",
                        "{chain} via {oif}: control {allowed} must pass"
                    );
                }
            }
        }
    }

    /// Rule ORDER in both modes and in both egress chains: every
    /// `ct state established,related` accept comes after all containment drops
    /// and, in tunnel mode, after the DNS pin; and in tunnel mode no accept on
    /// those chains can match the uplink except loopback and the peer handshake.
    #[test]
    fn established_accept_follows_interface_and_containment_checks() {
        let tunnel = render(&tunnel_params());
        let contained = render(&KillswitchParams::containment_only(vec![], true));
        for (rendered, is_tunnel) in [(&tunnel, true), (&contained, false)] {
            for chain in ["output", "forward"] {
                let rules = chain_rules(rendered, chain);
                let last_check = [
                    "ip daddr @containment4 drop",
                    "ip6 daddr @containment6 drop",
                    "ip daddr @host_gw drop",
                    "ip daddr @sinkhole4 drop",
                ]
                .iter()
                .map(|rule| position(&rules, rule))
                .chain(
                    rules
                        .iter()
                        .position(|rule| *rule == "tcp dport 53 drop")
                        .filter(|_| chain == "output" && is_tunnel),
                )
                .max()
                .unwrap();
                for (index, rule) in rules.iter().enumerate() {
                    if rule.contains("ct state") {
                        assert!(
                            index > last_check,
                            "{chain}: `{rule}` precedes a containment or DNS check"
                        );
                    }
                    if is_tunnel && rule.ends_with("accept") {
                        let uplink_ok = *rule == "oif \"lo\" accept"
                            || (rule.starts_with("ip daddr @wg_peer udp dport ")
                                && !rule.contains("ct state"));
                        assert!(
                            uplink_ok || rule.contains("oifname \"wg0\""),
                            "{chain}: `{rule}` could accept a packet leaving the uplink"
                        );
                    }
                }
            }
        }
        // The only state accept left in the egress chains is containment-only
        // forward, and it sits after the containment drops.
        let forward = chain_rules(&contained, "forward");
        assert!(
            position(&forward, "ip daddr @containment4 drop")
                < position(&forward, "ct state established,related accept")
        );
        assert!(!chain_rules(&tunnel, "output")
            .iter()
            .any(|r| r.contains("ct state")));
        assert!(!chain_rules(&tunnel, "forward")
            .iter()
            .any(|r| r.contains("ct state")));
        assert!(!chain_rules(&contained, "output")
            .iter()
            .any(|r| r.contains("ct state")));
        // Tunnel output order: lo, handshake, DNS pin, containment, wg0.
        let output = chain_rules(&tunnel, "output");
        let order = [
            "oif \"lo\" accept",
            "ip daddr @wg_peer udp dport 51820 accept",
            "tcp dport 53 drop",
            "ip daddr @containment4 drop",
            "ip daddr @host_gw drop",
            "oifname \"wg0\" accept",
        ]
        .map(|rule| position(&output, rule));
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "{output:#?}"
        );
        assert_eq!(order[5], output.len() - 1, "wg0 accept must be last");
    }

    /// Input keeps accepting return traffic by state: the output fix closes the
    /// leak path, and inbound replies carry no capsule data out.
    #[test]
    fn input_chain_keeps_state_accept() {
        for rendered in [
            render(&tunnel_params()),
            render(&KillswitchParams::containment_only(vec![], true)),
        ] {
            let rules = chain_rules(&rendered, "input");
            assert_eq!(rules[0], "iif \"lo\" accept");
            assert_eq!(rules[1], "ct state established,related accept");
        }
    }

    #[test]
    fn resolver_allow_precedes_containment_drop() {
        // A tunnel resolver is often itself RFC1918/CGNAT; its accept MUST come
        // before the containment drop or DNS through the tunnel would break.
        let r = render(&tunnel_params());
        let allow = r.find("dport 53 oifname \"wg0\" accept").unwrap();
        let drop = r.find("ip daddr @containment4 drop").unwrap();
        assert!(allow < drop, "resolver allow must precede containment drop");
    }

    #[test]
    fn sinkhole_toggle_controls_set_and_rule() {
        let on = render(&KillswitchParams::containment_only(vec![], true));
        assert!(on.contains("set sinkhole4"));
        let off = render(&KillswitchParams::containment_only(vec![], false));
        // The header comment names @sinkhole4; assert the set + rule are gone.
        assert!(!off.contains("set sinkhole4"));
        assert!(!off.contains("@sinkhole4 drop"));
    }

    #[test]
    fn extra_cidrs_emitted_only_when_present() {
        let with = render(&KillswitchParams::containment_only(
            vec!["203.0.113.0/24".parse().unwrap()],
            false,
        ));
        assert!(with.contains("set extra_blocked"));
        assert!(with.contains("203.0.113.0/24"));
        assert!(with.contains("ip daddr @extra_blocked drop"));
        let without = render(&KillswitchParams::containment_only(vec![], false));
        assert!(!without.contains("extra_blocked"));
    }

    #[test]
    fn extra_blocked_networks_render_by_typed_address_family() {
        let params = KillswitchParams::containment_only(
            vec![
                "203.0.113.0/24".parse().unwrap(),
                "2001:db8::/32".parse().unwrap(),
            ],
            false,
        );
        let rules = render(&params);
        assert!(rules.contains("elements = { 203.0.113.0/24 }"));
        assert!(rules.contains("set extra_blocked6"));
        assert!(rules.contains("elements = { 2001:db8::/32 }"));
        assert!(rules.contains("ip6 daddr @extra_blocked6 drop"));
    }

    #[test]
    fn no_flush_ruleset_so_runtime_dns_survives() {
        let r = render(&tunnel_params());
        assert!(!r.contains("flush ruleset"));
        assert!(r.contains("delete table inet capscode"));
    }

    #[test]
    fn deterministic() {
        assert_eq!(render(&tunnel_params()), render(&tunnel_params()));
    }

    /// The committed `images/killswitch.nft` must equal a canonical tunnel-mode
    /// render so the exact rules can be inspected. Regenerate with:
    /// `cargo test -p capscode-net -- --ignored regenerate_reference` (below).
    #[test]
    fn committed_reference_matches_render() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../images/killswitch.nft");
        let committed = std::fs::read_to_string(path).expect("images/killswitch.nft must exist");
        assert_eq!(
            committed,
            render(&canonical_reference_params()),
            "images/killswitch.nft is stale; run the regenerate_reference test"
        );
    }

    #[test]
    #[ignore = "writes images/killswitch.nft; run explicitly to regenerate"]
    fn regenerate_reference() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../images/killswitch.nft");
        std::fs::write(path, render(&canonical_reference_params())).unwrap();
    }

    /// Canonical params the committed reference is rendered from: a tunnel with a
    /// private-range resolver, sinkhole on, one representative extra CIDR.
    fn canonical_reference_params() -> KillswitchParams {
        let cfg = wg::parse_and_validate(
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 10.99.0.1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = vpn.example.com:51820\nAllowedIPs = 0.0.0.0/0, ::/0\n",
        )
        .unwrap();
        KillswitchParams::tunnel(&cfg, vec!["203.0.113.0/24".parse().unwrap()], true)
    }
}
