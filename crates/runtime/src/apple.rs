use std::time::Duration;

use async_trait::async_trait;
use capscode_core::{
    AttachIo, ContainerId, ContainerSpec, ExecOutput, ExecSpec, ImageRef, OwnedObject, PublishMode,
    PullProgress, RuntimeAdapter, RuntimeCapabilities, RuntimeError, RuntimeInfo, RuntimeKind,
    VolumeId, VolumeSpec,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AppleContainerAdapter;

impl AppleContainerAdapter {
    pub fn new() -> Self {
        Self
    }

    fn unsupported(operation: &str) -> RuntimeError {
        RuntimeError::CapabilityUnsupported(
            RuntimeKind::AppleContainer,
            format!("{operation} is deferred to a later milestone"),
        )
    }
}

#[async_trait]
impl RuntimeAdapter for AppleContainerAdapter {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::AppleContainer
    }

    async fn preflight(&self) -> Result<RuntimeInfo, RuntimeError> {
        Err(Self::unsupported("preflight"))
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            localhost_publish: PublishMode::HostProxyRequired,
            shared_netns: false,
            tmpfs_mounts: false,
            cap_add_net_admin: false,
            dev_net_tun: false,
        }
    }

    async fn create_volume(&self, _spec: &VolumeSpec) -> Result<VolumeId, RuntimeError> {
        Err(Self::unsupported("volume creation"))
    }

    async fn remove_volume(&self, _id: &VolumeId) -> Result<(), RuntimeError> {
        Err(Self::unsupported("volume removal"))
    }

    async fn image_present(&self, _image: &ImageRef) -> Result<bool, RuntimeError> {
        Err(Self::unsupported("image inspection"))
    }

    async fn pull_image(
        &self,
        _image: &ImageRef,
        _on_progress: &mut (dyn FnMut(PullProgress) + Send),
    ) -> Result<(), RuntimeError> {
        Err(Self::unsupported("image pull"))
    }

    async fn create_container(&self, _spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        Err(Self::unsupported("container creation"))
    }

    async fn start(&self, _id: &ContainerId) -> Result<(), RuntimeError> {
        Err(Self::unsupported("container start"))
    }

    async fn exec(&self, _id: &ContainerId, _spec: ExecSpec) -> Result<ExecOutput, RuntimeError> {
        Err(Self::unsupported("exec"))
    }

    async fn run_interactive(
        &self,
        _id: &ContainerId,
        _spec: ExecSpec,
        _io: AttachIo,
    ) -> Result<i32, RuntimeError> {
        Err(Self::unsupported("interactive exec"))
    }

    async fn stop(&self, _id: &ContainerId, _timeout: Duration) -> Result<(), RuntimeError> {
        Err(Self::unsupported("container stop"))
    }

    async fn remove(&self, _id: &ContainerId, _force: bool) -> Result<(), RuntimeError> {
        Err(Self::unsupported("container removal"))
    }

    async fn list_owned(&self) -> Result<Vec<OwnedObject>, RuntimeError> {
        Err(Self::unsupported("owned-object listing"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apple_container_is_an_explicit_unsupported_seam() {
        let adapter = AppleContainerAdapter::new();
        assert_eq!(
            adapter.capabilities().localhost_publish,
            PublishMode::HostProxyRequired
        );
        assert!(matches!(
            adapter.preflight().await,
            Err(RuntimeError::CapabilityUnsupported(
                RuntimeKind::AppleContainer,
                _
            ))
        ));
    }
}
