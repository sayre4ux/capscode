//! Kill-switch + DNS-leak verification from inside the gateway .
//!
//! `verify()` runs a single self-contained probe script in the gateway via the
//! runtime's `exec`, then interprets its `KEY=VALUE` output into a
//! [`NetVerifyReport`]. It is also what `capscode doctor` drives. The probe
//! program is fail-closed reading: a blocked destination must actively refuse or
//! time out, and "no evidence" is never scored as "blocked".
//!
//! Splitting the pure interpretation ([`interpret`]) from the async runner keeps
//! the scoring logic unit-testable without a live runtime.

use std::collections::HashMap;

use capscode_core::{
    ContainerId, ExecSpec, NetError, NetVerifyReport, ProbeOutcome, RuntimeAdapter,
};

/// The probe script executed inside the gateway. Emits `KEY=VALUE` lines on
/// stdout. Pure POSIX sh + iproute2 + netcat + getent (all in the gateway image).
pub const PROBE_SCRIPT: &str = r#"
set +e

for tool in ip sed awk head date timeout nc getent; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required probe tool: $tool" >&2
    exit 127
  fi
done

mode="${CAPSCODE_NET_MODE:-unknown}"
echo "MODE=$mode"

# --- which interface does ordinary egress leave by? ---
egress_dev=$(ip -o route get 1.1.1.1 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n1)
echo "EGRESS_DEV=$egress_dev"

# --- resolver in use and the interface reaching it ---
ns=$(awk '/^nameserver/{print $2; exit}' /etc/resolv.conf 2>/dev/null)
echo "RESOLVER=$ns"
if [ -n "$ns" ]; then
  rdev=$(ip -o route get "$ns" 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n1)
  echo "RESOLVER_DEV=$rdev"
fi

# --- probe helper: prints open|refused|dropped; only dropped == contained
#     (a refusal proves the packet reached the host). ---
probe() {
  t0=$(date +%s%N)
  timeout 3 nc -w 2 -z "$1" "$2" >/dev/null 2>&1
  rc=$?
  t1=$(date +%s%N)
  ms=$(( (t1 - t0) / 1000000 ))
  if [ "$rc" -eq 0 ]; then echo open
  elif [ "$ms" -lt 1500 ]; then echo refused
  else echo dropped
  fi
}

# (c) an RFC1918 address must be unreachable
echo "RFC1918_PROBE=$(probe 10.255.255.1 80)"

# (c) the host-gateway must be unreachable: default route gw AND the
#     runtime host alias, whichever resolve.
gw=$(ip route show default 2>/dev/null | awk '{print $3; exit}')
echo "DEFAULT_GW=$gw"
[ -n "$gw" ] && echo "DEFAULT_GW_PROBE=$(probe "$gw" 22)"
hg=$(getent hosts host.docker.internal host.orbstack.internal gateway.docker.internal 2>/dev/null | awk '{print $1; exit}')
echo "HOST_GW=$hg"
[ -n "$hg" ] && echo "HOST_GW_PROBE=$(probe "$hg" 22)"

# (c/v6) the host must be unreachable over IPv6 too.
hg6=$(getent ahostsv6 host.docker.internal host.orbstack.internal gateway.docker.internal 2>/dev/null | awk '{print $1; exit}')
echo "HOST_GW6=$hg6"
[ -n "$hg6" ] && echo "HOST_GW6_PROBE=$(probe "$hg6" 22)"

# (a) positive egress: can we reach a public host at all? (meaningful in tunnel mode)
public=$(probe 1.1.1.1 443)
echo "PUBLIC_PROBE=$public"

# (info) world-facing egress IP + country via Cloudflare trace (1.1.1.1 = same host the
# public probe already contacts). STRICTLY informational: never gates the session, fails
# soft. Only attempt it when public egress actually works, so a contained/DROP path pays
# no latency tax; tight timeouts bound the cost either way.
if [ "$public" = open ] && command -v curl >/dev/null 2>&1; then
  trace=$(curl -fsS --connect-timeout 2 --max-time 3 https://1.1.1.1/cdn-cgi/trace 2>/dev/null)
  echo "EGRESS_IP=$(printf '%s\n' "$trace" | sed -n 's/^ip=//p' | head -n1)"
  echo "EGRESS_LOC=$(printf '%s\n' "$trace" | sed -n 's/^loc=//p' | head -n1)"
  trace=
fi
"#;

/// Run the probes inside `gateway` and interpret the result.
pub async fn run(
    expects_tunnel: bool,
    gateway: &ContainerId,
    rt: &dyn RuntimeAdapter,
) -> Result<NetVerifyReport, NetError> {
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), PROBE_SCRIPT.to_string()],
        env: Vec::new(),
        workdir: None,
        user: None,
        tty: false,
        stdin: None,
    };
    let out = rt
        .exec(gateway, spec)
        .await
        .map_err(|e| NetError::Gateway(format!("verification probe exec failed: {e}")))?;

    if out.exit_code != 0 {
        // The script itself never returns nonzero (set +e); a nonzero code means
        // the shell/tooling is missing — treat as an operational gateway fault.
        return Err(NetError::Gateway(format!(
            "verification probe exited {}: {}",
            out.exit_code,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(interpret(&parse_kv(&stdout), expects_tunnel))
}

/// Parse `KEY=VALUE` lines into a map (later keys win; empty values kept).
pub fn parse_kv(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn probe_blocked(map: &HashMap<String, String>, key: &str) -> Option<bool> {
    map.get(key).map(|v| v == "dropped")
}

/// Interpret parsed probe output into a [`NetVerifyReport`]. Pure and total.
pub fn interpret(map: &HashMap<String, String>, expects_tunnel: bool) -> NetVerifyReport {
    let mut details = Vec::new();
    let egress_dev = map.get("EGRESS_DEV").cloned().unwrap_or_default();
    let egress_ip = map.get("EGRESS_IP").filter(|v| !v.is_empty()).cloned();
    let egress_loc = map.get("EGRESS_LOC").filter(|v| !v.is_empty()).cloned();
    let resolver = map.get("RESOLVER").map(String::as_str).unwrap_or("");
    let resolver_dev = map.get("RESOLVER_DEV").map(String::as_str).unwrap_or("");

    // (a) egress via wg0 only
    let egress_via_wireguard_only = if expects_tunnel {
        let ok = egress_dev == "wg0";
        if ok {
            details.push("egress default route is via wg0".to_string());
        } else {
            details.push(format!(
                "KILL-SWITCH: ordinary egress leaves via '{egress_dev}', not wg0"
            ));
        }
        ok
    } else {
        details.push(format!(
            "containment-only (no VPN): egress via '{egress_dev}'; wireguard-only not applicable"
        ));
        false
    };

    // (b) resolver in tunnel
    let resolver_in_tunnel = if expects_tunnel {
        let ok = resolver_dev == "wg0" && !resolver.is_empty();
        if ok {
            details.push(format!("resolver {resolver} is reached via wg0"));
        } else {
            details.push(format!(
                "DNS-LEAK: resolver '{resolver}' is reached via '{resolver_dev}', not wg0"
            ));
        }
        ok
    } else {
        details.push(format!(
            "containment-only: resolver '{resolver}' (ambient); tunnel resolver not applicable"
        ));
        false
    };

    // (c) RFC1918 blocked
    let rfc1918_blocked = probe_blocked(map, "RFC1918_PROBE").unwrap_or(false);
    if rfc1918_blocked {
        details.push("RFC1918 probe (10.255.255.1:80) blocked".to_string());
    } else {
        details.push("RFC1918 probe NOT blocked or not run".to_string());
    }

    // (c) host-gateway blocked: every host-gateway probe that ran must be blocked,
    // and at least one must have run.
    let default_gw = probe_blocked(map, "DEFAULT_GW_PROBE");
    let host_gw = probe_blocked(map, "HOST_GW_PROBE");
    let ran: Vec<bool> = [default_gw, host_gw].into_iter().flatten().collect();
    let host_gateway_blocked = !ran.is_empty() && ran.iter().all(|b| *b);
    if host_gateway_blocked {
        details.push(format!(
            "host-gateway probes blocked (default gw={:?}, host alias={:?})",
            map.get("DEFAULT_GW"),
            map.get("HOST_GW"),
        ));
    } else {
        details.push(format!(
            "host-gateway NOT confirmed blocked (default_gw_probe={:?}, host_gw_probe={:?})",
            map.get("DEFAULT_GW_PROBE"),
            map.get("HOST_GW_PROBE"),
        ));
    }

    // Per-candidate host-gateway outcomes (display detail alongside host_gateway_blocked).
    let host_gateway = vec![
        (
            "default-gw".to_string(),
            match default_gw {
                Some(true) => ProbeOutcome::Dropped,
                Some(false) => ProbeOutcome::Reachable,
                None => ProbeOutcome::NotTested("probe did not run".into()),
            },
        ),
        (
            "host-alias".to_string(),
            match host_gw {
                Some(true) => ProbeOutcome::Dropped,
                Some(false) => ProbeOutcome::Reachable,
                None => ProbeOutcome::NotTested("probe did not run".into()),
            },
        ),
    ];

    // (c/v6) IPv6 host containment: only `dropped` counts. If no v6 host
    // resolved, a v4-only host has nothing to leak — NotTested (still passes).
    let ipv6_contained = match map.get("HOST_GW6_PROBE") {
        Some(v) => {
            if v == "dropped" {
                details.push(format!(
                    "IPv6 host-gateway probe dropped (host={:?})",
                    map.get("HOST_GW6"),
                ));
                ProbeOutcome::Dropped
            } else {
                details.push(format!(
                    "IPv6 host-gateway NOT contained (probe={v}, host={:?})",
                    map.get("HOST_GW6"),
                ));
                ProbeOutcome::Reachable
            }
        }
        None => {
            details.push(
                "IPv6 host-gateway not resolved (v4-only host); ipv6_contained defaults true"
                    .to_string(),
            );
            ProbeOutcome::NotTested("IPv6 host-gateway not resolved (v4-only host)".into())
        }
    };

    let public_probe = map.get("PUBLIC_PROBE").map(|v| {
        if v == "dropped" {
            ProbeOutcome::Dropped
        } else {
            ProbeOutcome::Reachable
        }
    });
    if let Some(pub_probe) = map.get("PUBLIC_PROBE") {
        details.push(format!("public egress probe (1.1.1.1:443) = {pub_probe}"));
    }
    match &egress_ip {
        Some(ip) => details.push(format!(
            "observed egress {ip} ({}) via {egress_dev}",
            egress_loc.as_deref().unwrap_or("??")
        )),
        None => details.push(format!(
            "observed egress via {egress_dev}; public IP unavailable"
        )),
    }

    NetVerifyReport {
        egress_via_wireguard_only,
        resolver_in_tunnel,
        rfc1918_blocked,
        host_gateway_blocked,
        ipv6_contained,
        host_gateway,
        public_probe,
        egress_dev,
        egress_ip,
        egress_loc,
        details,
    }
}

/// Whether a report should let the session proceed to `Running`. In tunnel mode
/// all four controls must hold; in containment-only mode only the containment
/// controls (RFC1918 + host-gateway + IPv6) are asserted (VPN is optional).
pub fn passed(report: &NetVerifyReport, expects_tunnel: bool) -> bool {
    // Dropped and NotTested both pass (legacy bool defaulted true when untested);
    // only Reachable fails. public_probe / host_gateway are display-only.
    let ipv6_ok = !matches!(report.ipv6_contained, ProbeOutcome::Reachable);
    let containment = report.rfc1918_blocked && report.host_gateway_blocked && ipv6_ok;
    if expects_tunnel {
        containment && report.egress_via_wireguard_only && report.resolver_in_tunnel
    } else {
        containment
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn tunnel_all_green() {
        let m = map(&[
            ("MODE", "tunnel"),
            ("EGRESS_DEV", "wg0"),
            ("RESOLVER", "10.99.0.1"),
            ("RESOLVER_DEV", "wg0"),
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
            ("HOST_GW_PROBE", "dropped"),
            ("HOST_GW6_PROBE", "dropped"),
            ("PUBLIC_PROBE", "open"),
            ("EGRESS_IP", "203.0.113.7"),
            ("EGRESS_LOC", "SG"),
        ]);
        let r = interpret(&m, true);
        assert!(r.egress_via_wireguard_only);
        assert!(r.resolver_in_tunnel);
        assert!(r.rfc1918_blocked);
        assert!(r.host_gateway_blocked);
        assert_eq!(r.ipv6_contained, ProbeOutcome::Dropped);
        assert_eq!(
            r.host_gateway,
            vec![
                ("default-gw".into(), ProbeOutcome::Dropped),
                ("host-alias".into(), ProbeOutcome::Dropped),
            ]
        );
        assert_eq!(r.public_probe, Some(ProbeOutcome::Reachable));
        assert_eq!(r.egress_dev, "wg0");
        assert_eq!(r.egress_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(r.egress_loc.as_deref(), Some("SG"));
        assert!(passed(&r, true));
    }

    #[test]
    fn tunnel_egress_leak_fails() {
        let m = map(&[
            ("EGRESS_DEV", "eth0"),
            ("RESOLVER", "10.99.0.1"),
            ("RESOLVER_DEV", "wg0"),
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
        ]);
        let r = interpret(&m, true);
        assert!(!r.egress_via_wireguard_only);
        assert!(!passed(&r, true));
        assert!(r.details.iter().any(|d| d.contains("KILL-SWITCH")));
    }

    #[test]
    fn tunnel_dns_leak_fails() {
        let m = map(&[
            ("EGRESS_DEV", "wg0"),
            ("RESOLVER", "192.168.1.1"),
            ("RESOLVER_DEV", "eth0"),
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
        ]);
        let r = interpret(&m, true);
        assert!(!r.resolver_in_tunnel);
        assert!(!passed(&r, true));
        assert!(r.details.iter().any(|d| d.contains("DNS-LEAK")));
    }

    #[test]
    fn containment_only_passes_on_containment_alone() {
        let m = map(&[
            ("MODE", "containment-only"),
            ("EGRESS_DEV", "eth0"),
            ("RESOLVER", "127.0.0.11"),
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
            ("HOST_GW_PROBE", "dropped"),
            ("HOST_GW6_PROBE", "dropped"),
            ("PUBLIC_PROBE", "open"),
        ]);
        let r = interpret(&m, false);
        assert!(!r.egress_via_wireguard_only); // not applicable
        assert!(r.rfc1918_blocked);
        assert!(r.host_gateway_blocked);
        assert_eq!(r.ipv6_contained, ProbeOutcome::Dropped);
        assert_eq!(r.egress_dev, "eth0");
        assert_eq!(r.egress_ip, None);
        assert_eq!(r.egress_loc, None);
        assert!(passed(&r, false));
        // But it would NOT pass as a tunnel session.
        assert!(!passed(&r, true));
    }

    #[test]
    fn empty_egress_trace_values_are_unavailable() {
        let r = interpret(
            &map(&[
                ("EGRESS_DEV", "eth0"),
                ("EGRESS_IP", ""),
                ("EGRESS_LOC", ""),
            ]),
            false,
        );
        assert_eq!(r.egress_dev, "eth0");
        assert_eq!(r.egress_ip, None);
        assert_eq!(r.egress_loc, None);
        assert!(r
            .details
            .iter()
            .any(|detail| detail.contains("public IP unavailable")));
    }

    #[test]
    fn host_gateway_open_fails_containment() {
        let m = map(&[
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "open"),
            ("HOST_GW_PROBE", "dropped"),
        ]);
        let r = interpret(&m, false);
        assert!(!r.host_gateway_blocked);
        assert!(!passed(&r, false));
    }

    #[test]
    fn no_hostgw_probe_ran_is_not_scored_blocked() {
        let m = map(&[("RFC1918_PROBE", "dropped")]);
        let r = interpret(&m, false);
        assert!(
            !r.host_gateway_blocked,
            "absence of evidence is not blocked"
        );
    }

    #[test]
    fn ipv6_host_gateway_open_fails_containment() {
        let m = map(&[
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
            ("HOST_GW_PROBE", "dropped"),
            ("HOST_GW6_PROBE", "open"),
        ]);
        let r = interpret(&m, false);
        assert_eq!(r.ipv6_contained, ProbeOutcome::Reachable);
        assert!(!passed(&r, false));
    }

    #[test]
    fn ipv6_host_gateway_refused_fails_containment() {
        let m = map(&[
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
            ("HOST_GW_PROBE", "dropped"),
            ("HOST_GW6_PROBE", "refused"),
        ]);
        let r = interpret(&m, false);
        assert_eq!(r.ipv6_contained, ProbeOutcome::Reachable);
        assert!(!passed(&r, false));
    }

    #[test]
    fn ipv6_absent_defaults_contained_on_v4_only_host() {
        // No HOST_GW6_PROBE: v4-only host has nothing to leak over v6.
        let m = map(&[
            ("RFC1918_PROBE", "dropped"),
            ("DEFAULT_GW_PROBE", "dropped"),
            ("HOST_GW_PROBE", "dropped"),
        ]);
        let r = interpret(&m, false);
        assert_eq!(
            r.ipv6_contained,
            ProbeOutcome::NotTested("IPv6 host-gateway not resolved (v4-only host)".into())
        );
        assert!(passed(&r, false));
        assert_eq!(
            r.host_gateway,
            vec![
                ("default-gw".into(), ProbeOutcome::Dropped),
                ("host-alias".into(), ProbeOutcome::Dropped),
            ]
        );
        assert_eq!(r.public_probe, None);
    }

    #[test]
    fn passed_ipv6_tri_state_matches_legacy_bool() {
        // Dropped and NotTested pass; Reachable fails — both expects_tunnel modes.
        for expects_tunnel in [false, true] {
            let mut base = NetVerifyReport {
                rfc1918_blocked: true,
                host_gateway_blocked: true,
                ipv6_contained: ProbeOutcome::Dropped,
                egress_via_wireguard_only: expects_tunnel,
                resolver_in_tunnel: expects_tunnel,
                ..Default::default()
            };
            assert!(
                passed(&base, expects_tunnel),
                "Dropped must pass (expects_tunnel={expects_tunnel})"
            );

            base.ipv6_contained = ProbeOutcome::NotTested("untested".into());
            assert!(
                passed(&base, expects_tunnel),
                "NotTested must pass (expects_tunnel={expects_tunnel})"
            );

            base.ipv6_contained = ProbeOutcome::Reachable;
            assert!(
                !passed(&base, expects_tunnel),
                "Reachable must fail (expects_tunnel={expects_tunnel})"
            );
        }
    }

    #[test]
    fn parse_kv_handles_empty_values_and_extra_equals() {
        let m = parse_kv("A=\nB=x=y\nMODE=tunnel\n# junk\n");
        assert_eq!(m.get("A").map(String::as_str), Some(""));
        assert_eq!(m.get("B").map(String::as_str), Some("x=y"));
        assert_eq!(m.get("MODE").map(String::as_str), Some("tunnel"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_script_reports_missing_nc_as_an_operational_error() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let tools = std::env::temp_dir().join(format!(
            "capscode-probe-tools-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&tools).expect("create probe tool directory");
        for tool in ["ip", "sed", "awk", "head", "date", "timeout", "getent"] {
            let path = tools.join(tool);
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write fake probe tool");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("make fake probe tool executable");
        }

        let output = Command::new("/bin/sh")
            .args(["-c", PROBE_SCRIPT])
            .env("PATH", &tools)
            .output()
            .expect("run probe script");
        fs::remove_dir_all(&tools).expect("remove probe tool directory");

        assert_eq!(output.status.code(), Some(127));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "missing required probe tool: nc"
        );
    }
}
