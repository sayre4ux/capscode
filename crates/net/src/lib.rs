//! `capscode-net` — WireGuard + nftables network policy stack implementing
//! [`capscode_core::NetworkStack`].
//!
//! The network controls live in a **gateway** container that owns the network
//! namespace; the capsule shares that namespace ([`NetworkMode::SharedWith`]) and
//! carries no elevated capabilities. This crate:
//!
//! * builds the gateway [`ContainerSpec`] ([`spec::gateway_spec`]) — userspace
//!   WireGuard + nftables + broker, `NET_ADMIN` (+ `/dev/net/tun` in tunnel mode);
//!   secrets (provider keys, WG config) are delivered into a tmpfs after start,
//!   never via env;
//! * parses and validates the operator's WireGuard config ([`wg`]);
//! * renders the default-DROP nftables kill switch ([`killswitch`]);
//! * verifies the kill switch and DNS-through-tunnel from inside the gateway
//!   ([`verify`]), which also backs `capscode doctor`.
//!
//! The WireGuard data plane and `nft -f` application run inside the gateway image
//! (`images/gateway.Dockerfile` + `images/gateway-entrypoint.sh`); this crate
//! never shells out on the host.

pub mod killswitch;
pub mod sinkhole;
pub mod spec;
pub mod verify;
pub mod wg;

use async_trait::async_trait;

use capscode_core::{
    ContainerId, ContainerSpec, NetContext, NetError, NetVerifyReport, NetworkConfig, NetworkStack,
    RuntimeAdapter,
};

pub use killswitch::{render as render_killswitch, KillswitchParams, Mode};
pub use spec::{gateway_spec, provider_cred_env, provider_key_env, BROKER_LISTEN};
pub use verify::{passed as verify_passed, PROBE_SCRIPT};
pub use wg::{parse_and_validate as parse_wireguard_conf, WgConfig, WgPeer};

/// The concrete [`NetworkStack`] implementation for CapsCode.
///
/// Holds the session's [`NetworkConfig`] so [`NetworkStack::verify`] — which is
/// only handed the gateway id + runtime — knows whether a tunnel is expected.
#[derive(Clone, Debug)]
pub struct NetStack {
    network: NetworkConfig,
}

impl NetStack {
    pub fn new(network: NetworkConfig) -> Self {
        NetStack { network }
    }

    /// A tunnel is expected only when a WireGuard config is configured *and* VPN
    /// enforcement is on; otherwise the session runs the containment-only path.
    pub fn expects_tunnel(&self) -> bool {
        self.network.wireguard_conf.is_some() && self.network.enforce_vpn
    }
}

#[async_trait]
impl NetworkStack for NetStack {
    fn gateway_spec(&self, ctx: &NetContext) -> Result<ContainerSpec, NetError> {
        spec::gateway_spec(ctx)
    }

    async fn verify(
        &self,
        gateway: &ContainerId,
        rt: &dyn RuntimeAdapter,
    ) -> Result<NetVerifyReport, NetError> {
        verify::run(self.expects_tunnel(), gateway, rt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn expects_tunnel_requires_conf_and_enforcement() {
        let none = NetStack::new(NetworkConfig::default());
        assert!(!none.expects_tunnel());

        let disabled = NetStack::new(NetworkConfig {
            wireguard_conf: Some(PathBuf::from("/x.conf")),
            enforce_vpn: false,
            telemetry_sinkhole: true,
            extra_blocked_cidrs: vec![],
        });
        assert!(!disabled.expects_tunnel());

        let on = NetStack::new(NetworkConfig {
            wireguard_conf: Some(PathBuf::from("/x.conf")),
            enforce_vpn: true,
            telemetry_sinkhole: true,
            extra_blocked_cidrs: vec![],
        });
        assert!(on.expects_tunnel());
    }

    #[test]
    fn is_object_safe_as_dyn_network_stack() {
        let _n: Box<dyn NetworkStack> = Box::new(NetStack::new(NetworkConfig::default()));
    }
}
