# Networking

By default CapsCode runs **containment-only**: public internet access is allowed without
destination filtering, while configured host, LAN, and metadata ranges are blocked and known
telemetry endpoints are sinkholed. This mode uses the host's public egress IP and ambient DNS;
it does not filter data exfiltration. IPv6 ULA and link-local ranges are blocked, but global
IPv6 host aliases are not comprehensively covered.

To route the capsule's egress through a **VPN** with a fail-closed kill switch, point CapsCode
at a standard WireGuard config (the `.conf` your provider — Proton, Mullvad, … — hands you):

```bash
capscode vpn import ~/Downloads/proton-us.conf   # validates supported WireGuard settings
capscode vpn
capscode claude                                  # egress now only through the tunnel
```

CapsCode stores the file *path*, never its contents (the WireGuard private key stays in your
file and is injected into the gateway tmpfs at run time). Import accepts only an allowlist of
WireGuard configuration keys: `PreUp`, `PostUp`, `PreDown`, and `PostDown` hooks are rejected,
and DNS entries must be IP addresses. If the tunnel can't come up, the session aborts rather
than silently leaking — fail-closed. The kill switch checks the outgoing interface on every
packet, so if the tunnel goes away mid-session, connections that were already open are dropped
too rather than continuing over the host network. Remove with `capscode vpn remove`.

Every session states its egress up front (`Egress: WireGuard VPN — exit <endpoint>` or
`Egress: DIRECT`) and, once the in-gateway verifier passes, confirms what actually happened:
`Egress confirmed: wg0 — <public IP> (<country>)`. The same facts appear in `capscode doctor`.
The verifier contacts Cloudflare from the gateway on every session start: it opens a connection to
`1.1.1.1:443` to test public egress and, when that works, fetches `https://1.1.1.1/cdn-cgi/trace`
to learn the public IP and country. Both use the same egress path as the capsule.
