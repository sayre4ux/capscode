use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, HostConfig, Mount as DockerMount, MountTmpfsOptions,
    MountType, PortBinding, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, ListContainersOptionsBuilder, ListNetworksOptionsBuilder,
    ListVolumesOptionsBuilder, RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder,
    StopContainerOptionsBuilder,
};
use bollard::Docker;
use capscode_core::{
    AttachIo, ContainerId, ContainerSpec, ExecOutput, ExecSpec, ImageRef, LocalImageInfo, Mount,
    NetworkMode, OwnedObject, OwnedObjectKind, PortProtocol, PublishMode, PullProgress,
    RuntimeAdapter, RuntimeCapabilities, RuntimeError, RuntimeInfo, RuntimeKind, SessionId,
    VolumeId, VolumeSpec, LABEL_CREATED, LABEL_OWNER, LABEL_OWNER_VALUE, LABEL_ROLE, LABEL_SESSION,
};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStdout};
use zeroize::Zeroize;

const DEFAULT_DOCKER_SOCKET: &str = "/var/run/docker.sock";
const DEFAULT_ORBSTACK_SOCKET_SUFFIX: &str = ".orbstack/run/docker.sock";

struct ForwardedStdin {
    child: Child,
    stdout: ChildStdout,
}

impl ForwardedStdin {
    fn spawn(mut command: tokio::process::Command) -> io::Result<Self> {
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("stdin forwarder stdout was not piped"))?;
        Ok(Self { child, stdout })
    }
}

impl AsyncRead for ForwardedStdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(context, buffer)
    }
}

impl Drop for ForwardedStdin {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Returns host stdin through a disposable forwarding process. Unlike
/// `tokio::io::stdin`, dropping this reader cancels an outstanding terminal
/// read, so the Tokio runtime can shut down after an interactive exec exits.
#[cfg(unix)]
pub fn cancellable_stdin() -> io::Result<Box<dyn AsyncRead + Send + Unpin>> {
    let mut command = tokio::process::Command::new("cat");
    command.stdin(Stdio::inherit());
    Ok(Box::new(ForwardedStdin::spawn(command)?))
}

#[cfg(not(unix))]
pub fn cancellable_stdin() -> io::Result<Box<dyn AsyncRead + Send + Unpin>> {
    Ok(Box::new(tokio::io::stdin()))
}

#[derive(Debug)]
struct BollardAdapter {
    docker: Docker,
    kind: RuntimeKind,
    endpoint: String,
}

impl BollardAdapter {
    fn connect(kind: RuntimeKind, endpoint: impl Into<String>) -> Result<Self, RuntimeError> {
        let endpoint = normalize_endpoint(endpoint.into());
        let docker = Docker::connect_with_host(&endpoint)
            .map_err(|error| RuntimeError::Unreachable(kind, error.to_string()))?;
        Ok(Self {
            docker,
            kind,
            endpoint,
        })
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            localhost_publish: PublishMode::Native,
            shared_netns: true,
            tmpfs_mounts: true,
            cap_add_net_admin: true,
            dev_net_tun: true,
        }
    }

    async fn preflight(&self) -> Result<RuntimeInfo, RuntimeError> {
        self.docker
            .ping()
            .await
            .map_err(|error| self.unreachable(error))?;
        let version = self
            .docker
            .version()
            .await
            .map_err(|error| self.unreachable(error))?;

        if version.os.as_deref().is_some_and(|os| os != "linux") {
            return Err(RuntimeError::CapabilityUnsupported(
                self.kind,
                "Linux containers are required".to_string(),
            ));
        }

        Ok(RuntimeInfo {
            kind: self.kind,
            version: version.version.unwrap_or_else(|| "unknown".to_string()),
            socket_path: socket_path(&self.endpoint),
            capabilities: self.capabilities(),
        })
    }

    async fn create_volume(&self, spec: &VolumeSpec) -> Result<VolumeId, RuntimeError> {
        let volume = self
            .docker
            .create_volume(VolumeCreateRequest {
                name: Some(spec.name.clone()),
                labels: some_map(&spec.labels),
                ..Default::default()
            })
            .await
            .map_err(api_error)?;
        Ok(VolumeId(volume.name))
    }

    async fn remove_volume(&self, id: &VolumeId) -> Result<(), RuntimeError> {
        match self
            .docker
            .remove_volume(
                &id.0,
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if has_status(&error, &[404]) => Ok(()),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn image_present(&self, image: &ImageRef) -> Result<bool, RuntimeError> {
        match self.docker.inspect_image(&image.0).await {
            Ok(_) => Ok(true),
            Err(error) if has_status(&error, &[404]) => Ok(false),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn pull_image(
        &self,
        image: &ImageRef,
        on_progress: &mut (dyn FnMut(PullProgress) + Send),
    ) -> Result<(), RuntimeError> {
        ensure_pull_allowed(image)?;
        let options = CreateImageOptionsBuilder::new()
            .from_image(&image.0)
            .build();
        let mut stream = self.docker.create_image(Some(options), None, None);

        while let Some(update) = stream.next().await {
            let update = update
                .map_err(|error| RuntimeError::ImagePull(image.clone(), error.to_string()))?;
            let detail = update.progress_detail.unwrap_or_default();
            on_progress(PullProgress {
                layer_id: update.id,
                status: update.status.unwrap_or_else(|| "pulling".to_string()),
                current: nonnegative_u64(detail.current),
                total: detail.total.and_then(nonnegative_u64_option),
            });
        }

        Ok(())
    }

    async fn create_container(&self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        let body = build_container_body(spec)?;
        let response = self
            .docker
            .create_container(
                None::<bollard::query_parameters::CreateContainerOptions>,
                body,
            )
            .await
            .map_err(api_error)?;
        Ok(ContainerId(response.id))
    }

    async fn start(&self, id: &ContainerId) -> Result<(), RuntimeError> {
        match self
            .docker
            .start_container(
                &id.0,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if has_status(&error, &[304]) => Ok(()),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn exec(&self, id: &ContainerId, mut spec: ExecSpec) -> Result<ExecOutput, RuntimeError> {
        let tty = spec.tty;
        // Secret injection path: when stdin bytes are present we attach stdin,
        // write them, and close the pipe so the command (e.g. `cat > file`) sees
        // EOF. The bytes never touch argv, env, or host disk.
        let stdin_bytes = spec.stdin.take();
        let attach_stdin = stdin_bytes.is_some();
        let exec = self
            .docker
            .create_exec(&id.0, exec_options(spec, attach_stdin)?)
            .await
            .map_err(api_error)?;
        let started = self
            .docker
            .start_exec(
                &exec.id,
                Some(StartExecOptions {
                    tty,
                    ..Default::default()
                }),
            )
            .await
            .map_err(api_error)?;

        let StartExecResults::Attached {
            mut output,
            mut input,
        } = started
        else {
            return Err(container_error(&id.0, "exec unexpectedly detached"));
        };

        if let Some(mut bytes) = stdin_bytes {
            input
                .write_all(&bytes)
                .await
                .map_err(|error| container_error(&id.0, error))?;
            input
                .shutdown()
                .await
                .map_err(|error| container_error(&id.0, error))?;
            // Zeroize the host-side copy of the secret once it has been sent.
            bytes.zeroize();
        }
        drop(input);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(chunk) = output.next().await {
            match chunk.map_err(api_error)? {
                LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                LogOutput::StdOut { message }
                | LogOutput::Console { message }
                | LogOutput::StdIn { message } => stdout.extend_from_slice(&message),
            }
        }

        Ok(ExecOutput {
            stdout,
            stderr,
            exit_code: self.exec_exit_code(&exec.id, &id.0).await?,
        })
    }

    async fn run_interactive(
        &self,
        id: &ContainerId,
        spec: ExecSpec,
        io: AttachIo,
    ) -> Result<i32, RuntimeError> {
        let tty = spec.tty;
        let exec = self
            .docker
            .create_exec(&id.0, exec_options(spec, true)?)
            .await
            .map_err(api_error)?;
        let started = self
            .docker
            .start_exec(
                &exec.id,
                Some(StartExecOptions {
                    tty,
                    ..Default::default()
                }),
            )
            .await
            .map_err(api_error)?;

        let StartExecResults::Attached {
            mut output,
            mut input,
        } = started
        else {
            return Err(container_error(
                &id.0,
                "interactive exec unexpectedly detached",
            ));
        };

        let AttachIo {
            mut stdin,
            mut stdout,
            mut stderr,
        } = io;

        let resize_task = if tty {
            self.start_resize_watcher(&exec.id).await?
        } else {
            None
        };

        let mut input_task = tokio::spawn(async move {
            tokio::io::copy(&mut stdin, &mut input).await?;
            if keep_exec_input_open_after_host_eof(tty) {
                // Closing a TTY exec's input on host EOF can end the pty and
                // terminate a command before its output has been collected.
                std::future::pending::<()>().await;
                Ok(())
            } else {
                input.shutdown().await
            }
        });
        let mut attach_tasks = AbortOnDrop::default();
        attach_tasks.push(input_task.abort_handle());
        let mut input_finished = false;

        let mut exit_watcher = tokio::spawn({
            let docker = self.docker.clone();
            let exec_id = exec.id.clone();
            async move {
                loop {
                    match docker.inspect_exec(&exec_id).await {
                        // Only a definitive not-running state ends the attach.
                        // `exit_code` can be present while the exec is still
                        // running, and a transient inspect error must not be read
                        // as completion — keep polling. A vanished exec closes the
                        // output stream, which the main loop treats as EOF.
                        Ok(inspected) if inspected.running == Some(false) => break,
                        Ok(_) | Err(_) => {}
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        });
        attach_tasks.push(exit_watcher.abort_handle());
        if let Some(task) = resize_task.as_ref() {
            attach_tasks.push(task.abort_handle());
        }
        let mut watcher_finished = false;

        let output_result = loop {
            tokio::select! {
                next = output.next() => {
                    let Some(chunk) = next else {
                        break Ok(());
                    };
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => break Err(api_error(error)),
                    };
                    let write = match chunk {
                        LogOutput::StdErr { message } if !tty => stderr.write_all(&message).await,
                        LogOutput::StdErr { message }
                        | LogOutput::StdOut { message }
                        | LogOutput::Console { message }
                        | LogOutput::StdIn { message } => stdout.write_all(&message).await,
                    };
                    if let Err(error) = write {
                        break Err(container_error(&id.0, error));
                    }
                    // Flush each chunk: TUI redraws rarely contain a newline, so a
                    // line-buffered stdout would otherwise hold them until exit and
                    // the visible screen would appear frozen mid-session.
                    if let Err(error) = stdout.flush().await {
                        break Err(container_error(&id.0, error));
                    }
                }
                result = &mut input_task, if !input_finished => {
                    input_finished = true;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => break Err(container_error(&id.0, error)),
                        Err(error) => break Err(container_error(&id.0, error)),
                    }
                }
                _ = &mut exit_watcher, if !watcher_finished => {
                    watcher_finished = true;
                    let mut drain_err = None;
                    // Bound the drain by an absolute wall-clock deadline as well as
                    // a per-frame idle gap, so a stream that never EOFs (or keeps
                    // yielding within the gap) can't hang the session.
                    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                    loop {
                        let idle = Duration::from_millis(400).min(
                            drain_deadline.saturating_duration_since(tokio::time::Instant::now()),
                        );
                        if idle.is_zero() {
                            break;
                        }
                        match tokio::time::timeout(idle, output.next()).await {
                            Ok(Some(Ok(chunk))) => {
                                let write = match chunk {
                                    LogOutput::StdErr { message } if !tty => stderr.write_all(&message).await,
                                    LogOutput::StdErr { message }
                                    | LogOutput::StdOut { message }
                                    | LogOutput::Console { message }
                                    | LogOutput::StdIn { message } => stdout.write_all(&message).await,
                                };
                                if let Err(error) = write {
                                    drain_err = Some(container_error(&id.0, error));
                                    break;
                                }
                                if let Err(error) = stdout.flush().await {
                                    drain_err = Some(container_error(&id.0, error));
                                    break;
                                }
                            }
                            Ok(Some(Err(error))) => {
                                drain_err = Some(api_error(error));
                                break;
                            }
                            Ok(None) => break,
                            Err(_elapsed) => break,
                        }
                    }
                    if let Some(err) = drain_err {
                        break Err(err);
                    }
                    break Ok(());
                }
            }
        };

        if !input_finished {
            input_task.abort();
            let _ = input_task.await;
        }
        if !watcher_finished {
            exit_watcher.abort();
            let _ = exit_watcher.await;
        }
        abort_task(resize_task);
        output_result?;
        stdout
            .flush()
            .await
            .map_err(|error| container_error(&id.0, error))?;
        stderr
            .flush()
            .await
            .map_err(|error| container_error(&id.0, error))?;
        self.exec_exit_code(&exec.id, &id.0).await
    }

    async fn start_resize_watcher(
        &self,
        exec_id: &str,
    ) -> Result<Option<tokio::task::JoinHandle<()>>, RuntimeError> {
        let Some((width, height)) = terminal_dimensions() else {
            return Ok(None);
        };
        self.docker
            .resize_exec(exec_id, ResizeExecOptions { width, height })
            .await
            .map_err(api_error)?;

        #[cfg(unix)]
        {
            let docker = self.docker.clone();
            let exec_id = exec_id.to_string();
            Ok(Some(tokio::spawn(async move {
                let Ok(mut signal) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                else {
                    return;
                };
                while signal.recv().await.is_some() {
                    if let Some((width, height)) = terminal_dimensions() {
                        let _ = docker
                            .resize_exec(&exec_id, ResizeExecOptions { width, height })
                            .await;
                    }
                }
            })))
        }

        #[cfg(not(unix))]
        Ok(None)
    }

    async fn exec_exit_code(&self, exec_id: &str, container_id: &str) -> Result<i32, RuntimeError> {
        // After the output stream closes, Docker can briefly still report the exec
        // as running with no exit code populated. Poll inspect_exec until it settles
        // (bounded ~3s) rather than failing on the first empty read.
        let clamp = |code: i64| code.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        // The output stream has already drained by the time we get here, but Docker
        // can take a while to reap the exec and populate the exit code (node/child
        // cleanup). Poll generously — up to ~60s — before giving up.
        for _ in 0..1200 {
            let inspected = self.docker.inspect_exec(exec_id).await.map_err(api_error)?;
            if let Some(code) = inspected.exit_code {
                return Ok(clamp(code));
            }
            if inspected.running == Some(false) {
                // Finished but Docker reported no code — treat as success.
                return Ok(0);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(container_error(
            container_id,
            "exec did not report an exit code within the timeout",
        ))
    }

    async fn stop(&self, id: &ContainerId, timeout: Duration) -> Result<(), RuntimeError> {
        let seconds = timeout
            .as_secs()
            .saturating_add(u64::from(timeout.subsec_nanos() != 0))
            .min(i32::MAX as u64) as i32;
        let options = StopContainerOptionsBuilder::new().t(seconds).build();
        match self.docker.stop_container(&id.0, Some(options)).await {
            Ok(()) => Ok(()),
            Err(error) if has_status(&error, &[304, 404]) => Ok(()),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn remove(&self, id: &ContainerId, force: bool) -> Result<(), RuntimeError> {
        let options = RemoveContainerOptionsBuilder::new().force(force).build();
        match self.docker.remove_container(&id.0, Some(options)).await {
            Ok(()) => Ok(()),
            Err(error) if has_status(&error, &[404]) => Ok(()),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn remove_network(&self, id: &str) -> Result<(), RuntimeError> {
        match self.docker.remove_network(id).await {
            Ok(()) => Ok(()),
            Err(error) if has_status(&error, &[404]) => Ok(()),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn list_local_images(&self) -> Result<Vec<String>, RuntimeError> {
        let mut images: Vec<_> = self
            .docker
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(api_error)?
            .into_iter()
            .flat_map(|image| image.repo_tags)
            .collect();
        images.sort();
        images.dedup();
        Ok(images)
    }

    async fn list_local_image_details(&self) -> Result<Vec<LocalImageInfo>, RuntimeError> {
        let images = self
            .docker
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(api_error)?;
        let mut details: Vec<_> = images
            .into_iter()
            .flat_map(|image| {
                let labels: BTreeMap<_, _> = image.labels.into_iter().collect();
                image
                    .repo_tags
                    .into_iter()
                    .map(move |reference| LocalImageInfo {
                        reference,
                        size_bytes: u64::try_from(image.size).ok(),
                        created_unix: Some(image.created),
                        labels: labels.clone(),
                    })
            })
            .collect();
        details.sort_by(|left, right| left.reference.cmp(&right.reference));
        details.dedup_by(|left, right| left.reference == right.reference);
        Ok(details)
    }

    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError> {
        let options = RemoveImageOptionsBuilder::new().build();
        match self.docker.remove_image(image, Some(options), None).await {
            Ok(_) => Ok(()),
            Err(error) if has_status(&error, &[404]) => Ok(()),
            Err(error) => Err(api_error(error)),
        }
    }

    async fn list_owned(&self) -> Result<Vec<OwnedObject>, RuntimeError> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{LABEL_OWNER}={LABEL_OWNER_VALUE}")],
        );

        let containers = self
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .map_err(api_error)?;
        let volumes = self
            .docker
            .list_volumes(Some(
                ListVolumesOptionsBuilder::new().filters(&filters).build(),
            ))
            .await
            .map_err(api_error)?
            .volumes
            .unwrap_or_default();
        let networks = self
            .docker
            .list_networks(Some(
                ListNetworksOptionsBuilder::new().filters(&filters).build(),
            ))
            .await
            .map_err(api_error)?;

        let mut owned = Vec::with_capacity(containers.len() + volumes.len() + networks.len());
        for container in containers {
            let image = container.image.clone();
            let labels = container.labels.unwrap_or_default();
            if !is_owned(&labels) {
                continue;
            }
            // The listing above is `all(true)`, so this includes exited and paused
            // containers; carry the state through instead of letting callers
            // assume everything returned is live.
            let state = container.state.map(|state| state.to_string());
            if let Some(id) = container.id {
                owned.push(owned_object(
                    OwnedObjectKind::Container,
                    id,
                    &labels,
                    state,
                    image,
                ));
            }
        }
        for volume in volumes {
            if is_owned(&volume.labels) {
                owned.push(owned_object(
                    OwnedObjectKind::Volume,
                    volume.name,
                    &volume.labels,
                    None,
                    None,
                ));
            }
        }
        for network in networks {
            let labels = network.labels.unwrap_or_default();
            if !is_owned(&labels) {
                continue;
            }
            if let Some(id) = network.id.or(network.name) {
                owned.push(owned_object(
                    OwnedObjectKind::Network,
                    id,
                    &labels,
                    None,
                    None,
                ));
            }
        }
        owned.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(owned)
    }

    fn unreachable(&self, error: BollardError) -> RuntimeError {
        RuntimeError::Unreachable(self.kind, error.to_string())
    }
}

fn keep_exec_input_open_after_host_eof(tty: bool) -> bool {
    tty
}

/// Return whether an image reference names an image in the `capscode` namespace.
/// Docker CLI shorthand and registry-qualified Docker Hub references are both
/// local-build only.
pub fn is_capscode_image(image: &ImageRef) -> bool {
    let name = image.0.split('@').next().unwrap_or(&image.0);
    let segments: Vec<_> = name.split('/').collect();
    if segments.len() < 2 {
        return false;
    }
    let namespace_index = if segments[0].contains('.')
        || segments[0].contains(':')
        || segments[0].eq_ignore_ascii_case("localhost")
    {
        1
    } else {
        0
    };
    segments
        .get(namespace_index)
        .is_some_and(|namespace| namespace.eq_ignore_ascii_case("capscode"))
        && segments
            .get(namespace_index + 1)
            .is_some_and(|image| !image.is_empty())
}

fn ensure_pull_allowed(image: &ImageRef) -> Result<(), RuntimeError> {
    if is_capscode_image(image) {
        return Err(RuntimeError::ImagePull(
            image.clone(),
            "CapsCode images are built locally and are never pulled".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct DockerAdapter {
    inner: BollardAdapter,
}

impl DockerAdapter {
    pub fn new() -> Result<Self, RuntimeError> {
        Self::from_endpoint(discover_docker_endpoint())
    }

    pub fn from_endpoint(endpoint: impl Into<String>) -> Result<Self, RuntimeError> {
        Ok(Self {
            inner: BollardAdapter::connect(RuntimeKind::Docker, endpoint)?,
        })
    }
}

#[derive(Debug)]
pub struct OrbStackAdapter {
    inner: BollardAdapter,
}

impl OrbStackAdapter {
    pub fn new() -> Result<Self, RuntimeError> {
        Self::from_endpoint(discover_orbstack_endpoint())
    }

    pub fn from_endpoint(endpoint: impl Into<String>) -> Result<Self, RuntimeError> {
        Ok(Self {
            inner: BollardAdapter::connect(RuntimeKind::OrbStack, endpoint)?,
        })
    }
}

macro_rules! impl_runtime_adapter {
    ($adapter:ty, $kind:expr) => {
        #[async_trait]
        impl RuntimeAdapter for $adapter {
            fn kind(&self) -> RuntimeKind {
                $kind
            }

            async fn preflight(&self) -> Result<RuntimeInfo, RuntimeError> {
                self.inner.preflight().await
            }

            fn capabilities(&self) -> RuntimeCapabilities {
                self.inner.capabilities()
            }

            async fn create_volume(&self, spec: &VolumeSpec) -> Result<VolumeId, RuntimeError> {
                self.inner.create_volume(spec).await
            }

            async fn remove_volume(&self, id: &VolumeId) -> Result<(), RuntimeError> {
                self.inner.remove_volume(id).await
            }

            async fn image_present(&self, image: &ImageRef) -> Result<bool, RuntimeError> {
                self.inner.image_present(image).await
            }

            async fn pull_image(
                &self,
                image: &ImageRef,
                on_progress: &mut (dyn FnMut(PullProgress) + Send),
            ) -> Result<(), RuntimeError> {
                self.inner.pull_image(image, on_progress).await
            }

            async fn create_container(
                &self,
                spec: &ContainerSpec,
            ) -> Result<ContainerId, RuntimeError> {
                self.inner.create_container(spec).await
            }

            async fn start(&self, id: &ContainerId) -> Result<(), RuntimeError> {
                self.inner.start(id).await
            }

            async fn exec(
                &self,
                id: &ContainerId,
                spec: ExecSpec,
            ) -> Result<ExecOutput, RuntimeError> {
                self.inner.exec(id, spec).await
            }

            async fn run_interactive(
                &self,
                id: &ContainerId,
                spec: ExecSpec,
                io: AttachIo,
            ) -> Result<i32, RuntimeError> {
                self.inner.run_interactive(id, spec, io).await
            }

            async fn stop(&self, id: &ContainerId, timeout: Duration) -> Result<(), RuntimeError> {
                self.inner.stop(id, timeout).await
            }

            async fn remove(&self, id: &ContainerId, force: bool) -> Result<(), RuntimeError> {
                self.inner.remove(id, force).await
            }

            async fn list_owned(&self) -> Result<Vec<OwnedObject>, RuntimeError> {
                self.inner.list_owned().await
            }

            async fn remove_network(&self, id: &str) -> Result<(), RuntimeError> {
                self.inner.remove_network(id).await
            }

            async fn list_local_images(&self) -> Result<Vec<String>, RuntimeError> {
                self.inner.list_local_images().await
            }

            async fn list_local_image_details(&self) -> Result<Vec<LocalImageInfo>, RuntimeError> {
                self.inner.list_local_image_details().await
            }

            async fn remove_image(&self, image: &str) -> Result<(), RuntimeError> {
                self.inner.remove_image(image).await
            }
        }
    };
}

impl_runtime_adapter!(DockerAdapter, RuntimeKind::Docker);
impl_runtime_adapter!(OrbStackAdapter, RuntimeKind::OrbStack);

fn build_container_body(spec: &ContainerSpec) -> Result<ContainerCreateBody, RuntimeError> {
    let mut mounts = spec
        .mounts
        .iter()
        .map(to_docker_mount)
        .collect::<Result<Vec<_>, _>>()?;
    if spec.tmpfs_home
        && !spec
            .mounts
            .iter()
            .any(|mount| mount_target(mount) == Path::new("/home/agent"))
    {
        mounts.push(DockerMount {
            target: Some("/home/agent".to_string()),
            typ: Some(MountType::TMPFS),
            ..Default::default()
        });
    }

    let mut port_bindings = HashMap::new();
    let mut exposed_ports = Vec::new();
    for publish in &spec.published_ports {
        let protocol = match publish.protocol {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        };
        let key = format!("{}/{protocol}", publish.container_port);
        exposed_ports.push(key.clone());
        port_bindings.insert(
            key,
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(publish.host_port.to_string()),
            }]),
        );
    }

    let network_mode = match &spec.network {
        NetworkMode::None => "none".to_string(),
        NetworkMode::Bridge(name) => name.clone(),
        NetworkMode::SharedWith(id) => format!("container:{}", id.0),
    };
    // A container that shares another's network namespace (`container:<id>`) also
    // shares its UTS namespace, so Docker rejects an explicit `hostname` alongside
    // it ("conflicting options: hostname and the network mode"). The hostname is
    // then inherited from the joined container (the gateway, named "workspace").
    let hostname = match &spec.network {
        NetworkMode::SharedWith(_) => None,
        _ => Some(spec.hostname.clone()),
    };
    let devices = spec
        .devices
        .iter()
        .map(|device| parse_device(device))
        .collect::<Result<Vec<_>, _>>()?;
    if spec.pids_limit == 0 {
        return Err(container_error(
            "create",
            "pids limit 0 would mean unlimited; refusing to create the container",
        ));
    }
    let memory = spec
        .memory_bytes
        .map(|bytes| {
            i64::try_from(bytes).map_err(|_| {
                container_error(
                    "create",
                    format!("memory limit {bytes} bytes exceeds Docker's limit"),
                )
            })
        })
        .transpose()?;

    Ok(ContainerCreateBody {
        hostname,
        env: some_env(&spec.env),
        cmd: spec.cmd.clone(),
        image: Some(spec.image.0.clone()),
        entrypoint: spec.entrypoint.clone(),
        exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
        labels: some_map(&spec.labels),
        host_config: Some(HostConfig {
            network_mode: Some(network_mode),
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            mounts: (!mounts.is_empty()).then_some(mounts),
            cap_add: (!spec.cap_add.is_empty()).then(|| spec.cap_add.clone()),
            cap_drop: (!spec.cap_drop.is_empty()).then(|| spec.cap_drop.clone()),
            security_opt: spec
                .no_new_privileges
                .then(|| vec!["no-new-privileges".to_string()]),
            devices: (!devices.is_empty()).then_some(devices),
            sysctls: some_map(&spec.sysctls),
            readonly_rootfs: Some(spec.read_only_rootfs),
            init: spec.init.then_some(true),
            pids_limit: Some(i64::from(spec.pids_limit)),
            memory,
            memory_swap: memory,
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn to_docker_mount(mount: &Mount) -> Result<DockerMount, RuntimeError> {
    match mount {
        Mount::Volume {
            id,
            target,
            read_only,
        } => Ok(DockerMount {
            target: Some(path_string(target)?),
            source: Some(id.0.clone()),
            typ: Some(MountType::VOLUME),
            read_only: Some(*read_only),
            ..Default::default()
        }),
        Mount::Tmpfs {
            target,
            size_mb,
            exec,
        } => {
            let size_bytes = size_mb
                .map(|size| {
                    size.checked_mul(1024 * 1024)
                        .and_then(|bytes| i64::try_from(bytes).ok())
                        .ok_or_else(|| {
                            container_error(
                                "create",
                                format!("tmpfs size {size} MiB exceeds Docker's limit"),
                            )
                        })
                })
                .transpose()?;
            Ok(DockerMount {
                target: Some(path_string(target)?),
                typ: Some(MountType::TMPFS),
                tmpfs_options: Some(MountTmpfsOptions {
                    size_bytes,
                    options: (*exec).then(|| vec![vec!["exec".to_string()]]),
                    ..Default::default()
                }),
                ..Default::default()
            })
        }
        Mount::Bind {
            source,
            target,
            read_only,
        } => Ok(DockerMount {
            target: Some(path_string(target)?),
            source: Some(path_string(source)?),
            typ: Some(MountType::BIND),
            read_only: Some(*read_only),
            ..Default::default()
        }),
    }
}

fn mount_target(mount: &Mount) -> &Path {
    match mount {
        Mount::Volume { target, .. } | Mount::Tmpfs { target, .. } | Mount::Bind { target, .. } => {
            target
        }
    }
}

fn parse_device(value: &str) -> Result<DeviceMapping, RuntimeError> {
    let parts = value.splitn(3, ':').collect::<Vec<_>>();
    let (host, container, permissions) = match parts.as_slice() {
        [path] if !path.is_empty() => (*path, *path, "rwm"),
        [host, container] if !host.is_empty() && !container.is_empty() => {
            (*host, *container, "rwm")
        }
        [host, container, permissions]
            if !host.is_empty() && !container.is_empty() && !permissions.is_empty() =>
        {
            (*host, *container, *permissions)
        }
        _ => {
            return Err(container_error(
                "create",
                format!("invalid device mapping {value:?}"),
            ));
        }
    };
    Ok(DeviceMapping {
        path_on_host: Some(host.to_string()),
        path_in_container: Some(container.to_string()),
        cgroup_permissions: Some(permissions.to_string()),
    })
}

fn exec_options(
    spec: ExecSpec,
    attach_stdin: bool,
) -> Result<CreateExecOptions<String>, RuntimeError> {
    let working_dir = spec.workdir.as_deref().map(path_string).transpose()?;
    Ok(CreateExecOptions {
        attach_stdin: Some(attach_stdin),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(spec.tty),
        env: some_env(&spec.env),
        cmd: Some(spec.cmd),
        user: spec.user,
        working_dir,
        ..Default::default()
    })
}

fn some_env(values: &[(String, String)]) -> Option<Vec<String>> {
    (!values.is_empty()).then(|| {
        values
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    })
}

fn some_map(values: &[(String, String)]) -> Option<HashMap<String, String>> {
    (!values.is_empty()).then(|| values.iter().cloned().collect())
}

fn path_string(path: &Path) -> Result<String, RuntimeError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| container_error("create", format!("path is not valid UTF-8: {path:?}")))
}

fn owned_object(
    kind: OwnedObjectKind,
    id: String,
    labels: &HashMap<String, String>,
    state: Option<String>,
    image: Option<String>,
) -> OwnedObject {
    OwnedObject {
        kind,
        id,
        session: labels.get(LABEL_SESSION).cloned().map(SessionId::from),
        role: labels.get(LABEL_ROLE).cloned(),
        created_at: labels.get(LABEL_CREATED).cloned(),
        state,
        image,
        labels: labels
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    }
}

fn is_owned(labels: &HashMap<String, String>) -> bool {
    labels.get(LABEL_OWNER).map(String::as_str) == Some(LABEL_OWNER_VALUE)
}

fn discover_docker_endpoint() -> String {
    if let Some(host) = env_value("DOCKER_HOST") {
        return host;
    }
    let mut candidates = Vec::new();
    if let Some(home) = env_value("HOME") {
        candidates.push(PathBuf::from(home).join(".docker/run/docker.sock"));
    }
    candidates.push(PathBuf::from(DEFAULT_DOCKER_SOCKET));
    candidates.push(PathBuf::from("/run/docker.sock"));
    first_socket(candidates).unwrap_or_else(|| format!("unix://{DEFAULT_DOCKER_SOCKET}"))
}

fn discover_orbstack_endpoint() -> String {
    let mut candidates = Vec::new();
    if let Some(home) = env_value("HOME") {
        candidates.push(PathBuf::from(home).join(DEFAULT_ORBSTACK_SOCKET_SUFFIX));
    }
    if let Some(runtime_dir) = env_value("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime_dir).join("orbstack/docker.sock"));
    }
    if let Some(socket) = first_socket(candidates) {
        return socket;
    }
    if let Some(host) = env_value("DOCKER_HOST") {
        return host;
    }
    let fallback = env_value("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join(DEFAULT_ORBSTACK_SOCKET_SUFFIX);
    format!("unix://{}", fallback.display())
}

fn first_socket(candidates: Vec<PathBuf>) -> Option<String> {
    candidates
        .into_iter()
        .find(|path| path.exists())
        .map(|path| format!("unix://{}", path.display()))
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn normalize_endpoint(endpoint: String) -> String {
    if endpoint.contains("://") {
        endpoint
    } else {
        format!("unix://{endpoint}")
    }
}

fn socket_path(endpoint: &str) -> Option<String> {
    endpoint
        .strip_prefix("unix://")
        .or_else(|| endpoint.strip_prefix("npipe://"))
        .map(ToOwned::to_owned)
}

fn api_error(error: BollardError) -> RuntimeError {
    RuntimeError::Api(Box::new(error))
}

fn has_status(error: &BollardError, expected: &[u16]) -> bool {
    matches!(
        error,
        BollardError::DockerResponseServerError { status_code, .. }
            if expected.contains(status_code)
    )
}

fn container_error(id: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Container(id.to_string(), error.to_string())
}

fn nonnegative_u64(value: Option<i64>) -> u64 {
    value.and_then(nonnegative_u64_option).unwrap_or(0)
}

fn nonnegative_u64_option(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn abort_task(task: Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
    }
}

#[derive(Default)]
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn push(&mut self, handle: tokio::task::AbortHandle) {
        self.0.push(handle);
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[cfg(unix)]
fn terminal_dimensions() -> Option<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `size` is a valid writable `winsize`, and the ioctl only writes to it.
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

#[cfg(not(unix))]
fn terminal_dimensions() -> Option<(u16, u16)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use capscode_core::PortPublish;
    use tokio::io::AsyncReadExt;

    #[test]
    fn tty_keeps_exec_input_open_after_host_eof() {
        assert!(keep_exec_input_open_after_host_eof(true));
        assert!(!keep_exec_input_open_after_host_eof(false));
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        // SAFETY: signal 0 performs existence/permission checking only.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_forwarded_stdin_terminates_pending_host_read() {
        let mut command = tokio::process::Command::new("/bin/cat");
        command.stdin(Stdio::piped());
        let forwarded = ForwardedStdin::spawn(command).unwrap();
        let pid = forwarded.child.id().expect("forwarder pid");

        let read = tokio::spawn(async move {
            let mut forwarded = forwarded;
            let mut bytes = Vec::new();
            forwarded.read_to_end(&mut bytes).await
        });
        tokio::task::yield_now().await;
        read.abort();
        tokio::time::timeout(Duration::from_secs(1), read)
            .await
            .expect("cancelled host read should drop promptly")
            .expect_err("read task should be cancelled");

        tokio::time::timeout(Duration::from_secs(1), async {
            while process_exists(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stdin forwarder should be killed and reaped");
    }

    #[tokio::test]
    async fn runtime_never_pulls_capscode_images() {
        let image = ImageRef("capscode/gateway:0.1.0".to_string());
        let error = ensure_pull_allowed(&image).unwrap_err();
        assert!(error.to_string().contains("built locally"));
    }

    #[tokio::test]
    async fn dropping_attach_cancels_all_forwarder_and_watcher_tasks() {
        let forwarder = tokio::spawn(std::future::pending::<()>());
        let watcher = tokio::spawn(std::future::pending::<()>());
        let mut tasks = AbortOnDrop::default();
        tasks.push(forwarder.abort_handle());
        tasks.push(watcher.abort_handle());
        drop(tasks);

        assert!(forwarder.await.unwrap_err().is_cancelled());
        assert!(watcher.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn capscode_namespace_matches_unqualified_and_docker_hub_refs() {
        for reference in [
            "capscode/gateway:0.1.0",
            "docker.io/capscode/gateway:0.1.0",
            "index.docker.io/capscode/capsule-claude:1",
            "capscode/seeder@sha256:abcd",
        ] {
            assert!(
                is_capscode_image(&ImageRef(reference.to_string())),
                "{reference}"
            );
        }
        assert!(!is_capscode_image(&ImageRef(
            "alpine@sha256:abcd".to_string()
        )));
        assert!(!is_capscode_image(&ImageRef(
            "ghcr.io/example/capsule:1".to_string()
        )));
    }

    #[test]
    fn container_body_honors_security_and_runtime_fields() {
        let spec = ContainerSpec {
            image: ImageRef("alpine:3".to_string()),
            hostname: "workspace".to_string(),
            env: vec![("A".to_string(), "B".to_string())],
            cmd: Some(vec!["sleep".to_string(), "1".to_string()]),
            entrypoint: Some(vec!["/bin/sh".to_string()]),
            mounts: vec![
                Mount::Volume {
                    id: VolumeId("work".to_string()),
                    target: PathBuf::from("/workspace"),
                    read_only: false,
                },
                Mount::Tmpfs {
                    target: PathBuf::from("/tmp"),
                    size_mb: Some(8),
                    exec: false,
                },
                Mount::Bind {
                    source: PathBuf::from("/host"),
                    target: PathBuf::from("/mnt/host"),
                    read_only: true,
                },
            ],
            network: NetworkMode::SharedWith(ContainerId("gateway".to_string())),
            published_ports: vec![PortPublish {
                host_port: 8443,
                container_port: 443,
                protocol: PortProtocol::Tcp,
            }],
            cap_add: vec!["NET_ADMIN".to_string()],
            cap_drop: Vec::new(),
            no_new_privileges: false,
            devices: vec!["/dev/net/tun".to_string()],
            sysctls: vec![("net.ipv4.ip_forward".to_string(), "1".to_string())],
            labels: vec![(LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string())],
            tmpfs_home: true,
            read_only_rootfs: true,
            init: true,
            pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
            memory_bytes: None,
        };

        let body = build_container_body(&spec).expect("body should build");
        // SharedWith network mode => hostname is inherited from the joined container,
        // so no explicit hostname is sent (Docker rejects the combination).
        assert_eq!(body.hostname, None);
        assert_eq!(body.env, Some(vec!["A=B".to_string()]));
        assert_eq!(body.cmd, spec.cmd);
        assert_eq!(body.entrypoint, spec.entrypoint);
        assert_eq!(
            body.labels
                .and_then(|labels| labels.get(LABEL_OWNER).cloned()),
            Some("1".to_string())
        );

        let host = body.host_config.expect("host config");
        assert_eq!(host.network_mode.as_deref(), Some("container:gateway"));
        assert_eq!(host.readonly_rootfs, Some(true));
        assert_eq!(host.init, Some(true));
        assert_eq!(host.cap_add, Some(vec!["NET_ADMIN".to_string()]));
        assert_eq!(host.cap_drop, None);
        assert_eq!(host.security_opt, None);
        assert_eq!(
            host.sysctls
                .and_then(|values| values.get("net.ipv4.ip_forward").cloned()),
            Some("1".to_string())
        );
        assert_eq!(
            host.devices.expect("devices")[0].path_on_host.as_deref(),
            Some("/dev/net/tun")
        );

        let mounts = host.mounts.expect("mounts");
        assert_eq!(mounts.len(), 4);
        assert!(mounts
            .iter()
            .any(|mount| mount.target.as_deref() == Some("/home/agent")
                && mount.typ == Some(MountType::TMPFS)));

        let port_bindings = host.port_bindings.expect("port bindings");
        let binding = &port_bindings["443/tcp"].as_ref().expect("bindings")[0];
        assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(binding.host_port.as_deref(), Some("8443"));
    }

    #[test]
    fn container_body_omits_init_when_false() {
        let spec = ContainerSpec {
            image: ImageRef("alpine:3".to_string()),
            hostname: "workspace".to_string(),
            env: Vec::new(),
            cmd: None,
            entrypoint: None,
            mounts: Vec::new(),
            network: NetworkMode::None,
            published_ports: Vec::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            no_new_privileges: false,
            devices: Vec::new(),
            sysctls: Vec::new(),
            labels: Vec::new(),
            tmpfs_home: false,
            read_only_rootfs: false,
            init: false,
            pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
            memory_bytes: None,
        };
        let body = build_container_body(&spec).expect("body should build");
        let host = body.host_config.expect("host config");
        assert_eq!(host.init, None);
        assert_eq!(host.pids_limit, Some(1024));
        assert_eq!(host.memory, None);
        assert_eq!(host.memory_swap, None);
    }

    fn limits_spec(pids_limit: u32, memory_bytes: Option<u64>) -> ContainerSpec {
        ContainerSpec {
            image: ImageRef("alpine:3".to_string()),
            hostname: "workspace".to_string(),
            env: Vec::new(),
            cmd: None,
            entrypoint: None,
            mounts: Vec::new(),
            network: NetworkMode::None,
            published_ports: Vec::new(),
            cap_add: Vec::new(),
            cap_drop: vec!["ALL".to_string()],
            no_new_privileges: true,
            devices: Vec::new(),
            sysctls: Vec::new(),
            labels: Vec::new(),
            tmpfs_home: false,
            read_only_rootfs: true,
            init: true,
            pids_limit,
            memory_bytes,
        }
    }

    #[test]
    fn container_body_caps_memory_and_swap_together() {
        let body = build_container_body(&limits_spec(2048, Some(4 << 30))).unwrap();
        let host = body.host_config.expect("host config");
        assert_eq!(host.pids_limit, Some(2048));
        assert_eq!(host.memory, Some(4 << 30));
        // Equal memory and memory+swap: no swap on top of the cap.
        assert_eq!(host.memory_swap, Some(4 << 30));
    }

    #[test]
    fn container_body_refuses_unlimited_pids_and_oversized_memory() {
        // Docker reads PidsLimit 0 as "unlimited".
        assert!(build_container_body(&limits_spec(0, None)).is_err());
        assert!(build_container_body(&limits_spec(1024, Some(u64::MAX))).is_err());
    }

    #[test]
    fn tmpfs_mount_threads_size_bytes() {
        let mount = to_docker_mount(&Mount::Tmpfs {
            target: PathBuf::from("/home/agent"),
            size_mb: Some(512),
            exec: false,
        })
        .expect("mount");
        assert_eq!(mount.target.as_deref(), Some("/home/agent"));
        assert_eq!(mount.typ, Some(MountType::TMPFS));
        assert_eq!(
            mount
                .tmpfs_options
                .as_ref()
                .and_then(|opts| opts.size_bytes),
            Some(512 * 1024 * 1024)
        );
    }

    #[test]
    fn tmpfs_mount_threads_exec_option_only_when_enabled() {
        let exec_mount = to_docker_mount(&Mount::Tmpfs {
            target: PathBuf::from("/tmp"),
            size_mb: Some(1024),
            exec: true,
        })
        .expect("exec mount");
        assert_eq!(
            exec_mount
                .tmpfs_options
                .as_ref()
                .and_then(|options| options.options.as_ref()),
            Some(&vec![vec!["exec".to_string()]])
        );

        let noexec_mount = to_docker_mount(&Mount::Tmpfs {
            target: PathBuf::from("/home/agent"),
            size_mb: Some(512),
            exec: false,
        })
        .expect("noexec mount");
        assert_eq!(
            noexec_mount
                .tmpfs_options
                .as_ref()
                .and_then(|options| options.options.as_ref()),
            None
        );
    }

    #[test]
    fn container_body_home_tmpfs_is_noexec() {
        let mut spec = limits_spec(1024, None);
        spec.tmpfs_home = true;
        let automatic = build_container_body(&spec).unwrap();
        let automatic_mount = automatic
            .host_config
            .unwrap()
            .mounts
            .unwrap()
            .into_iter()
            .find(|mount| mount.target.as_deref() == Some("/home/agent"))
            .expect("auto-pushed home tmpfs");
        assert_eq!(
            automatic_mount
                .tmpfs_options
                .as_ref()
                .and_then(|options| options.options.as_ref()),
            None
        );

        let explicit = to_docker_mount(&Mount::Tmpfs {
            target: PathBuf::from("/home/agent"),
            size_mb: Some(512),
            exec: false,
        })
        .unwrap();
        assert_eq!(
            explicit
                .tmpfs_options
                .as_ref()
                .and_then(|options| options.options.as_ref()),
            None
        );
    }

    #[test]
    fn tmpfs_mount_rejects_oversized_mib() {
        // i64::MAX / (1024*1024) + 1 overflows the Docker size_bytes field.
        let too_big = (i64::MAX as u64 / (1024 * 1024)) + 1;
        let err = to_docker_mount(&Mount::Tmpfs {
            target: PathBuf::from("/tmp"),
            size_mb: Some(too_big),
            exec: false,
        })
        .expect_err("oversized tmpfs should fail");
        assert!(
            err.to_string().contains("exceeds Docker's limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_device_mapping_forms() {
        let same = parse_device("/dev/net/tun").expect("single path");
        assert_eq!(same.path_in_container, same.path_on_host);
        assert_eq!(same.cgroup_permissions.as_deref(), Some("rwm"));

        let mapped = parse_device("/dev/a:/dev/b:rw").expect("mapped path");
        assert_eq!(mapped.path_on_host.as_deref(), Some("/dev/a"));
        assert_eq!(mapped.path_in_container.as_deref(), Some("/dev/b"));
        assert_eq!(mapped.cgroup_permissions.as_deref(), Some("rw"));
    }

    #[test]
    fn absent_socket_returns_unreachable_without_panicking() {
        let result = DockerAdapter::from_endpoint("/definitely/not/a/docker.sock");
        assert!(matches!(
            result,
            Err(RuntimeError::Unreachable(RuntimeKind::Docker, _))
        ));
    }
}
