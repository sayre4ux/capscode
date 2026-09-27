use std::path::PathBuf;

use capscode_core::{
    AttachIo, ContainerSpec, ExecSpec, ImageRef, NetworkMode, OwnedObjectKind, RuntimeError,
    RuntimePref, SessionId, VolumeSpec, LABEL_CREATED, LABEL_OWNER, LABEL_OWNER_VALUE, LABEL_ROLE,
    LABEL_SESSION, ROLE_CAPSULE,
};
use capscode_runtime::detect;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Round-trips bytes through the non-interactive `exec` stdin pipe (the path used
/// to inject provider keys + the WireGuard config into the gateway tmpfs). `cat`
/// echoes stdin to stdout, so the output must equal the injected bytes verbatim.
#[tokio::test]
#[ignore = "requires a live Docker-compatible daemon and public registry access"]
async fn live_exec_stdin_round_trip() -> Result<(), RuntimeError> {
    let runtime = detect(RuntimePref::Auto).await?;
    let image = ImageRef("alpine:3".to_string());
    runtime.pull_image(&image, &mut |_| {}).await?;

    let session = SessionId::new();
    let container = runtime
        .create_container(&ContainerSpec {
            image,
            hostname: "stdin-test".to_string(),
            env: Vec::new(),
            cmd: Some(vec!["sleep".to_string(), "60".to_string()]),
            entrypoint: None,
            mounts: vec![capscode_core::Mount::Tmpfs {
                target: PathBuf::from("/run/capscode"),
                size_mb: Some(1),
                exec: false,
            }],
            network: NetworkMode::None,
            published_ports: Vec::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            no_new_privileges: false,
            devices: Vec::new(),
            sysctls: Vec::new(),
            labels: vec![
                (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
                (LABEL_SESSION.to_string(), session.as_str().to_string()),
                (LABEL_ROLE.to_string(), ROLE_CAPSULE.to_string()),
            ],
            tmpfs_home: false,
            read_only_rootfs: false,
            init: false,
            pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
            memory_bytes: None,
        })
        .await?;

    let result: Result<(), RuntimeError> = async {
        runtime.start(&container).await?;
        let secret = b"PRIVATE=super-secret-value\n".to_vec();

        // Echo path: `cat` copies stdin straight to stdout.
        let echoed = runtime
            .exec(
                &container,
                ExecSpec {
                    cmd: vec!["cat".to_string()],
                    stdin: Some(secret.clone()),
                    ..Default::default()
                },
            )
            .await?;
        if echoed.exit_code != 0 || echoed.stdout != secret {
            return Err(RuntimeError::Container(
                container.0.clone(),
                format!(
                    "stdin echo mismatch: code={} stdout={:?}",
                    echoed.exit_code,
                    String::from_utf8_lossy(&echoed.stdout)
                ),
            ));
        }

        // File path: write via stdin under umask 077, then read it back.
        let written = runtime
            .exec(
                &container,
                ExecSpec {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "umask 077; cat > \"$1\"".to_string(),
                        "sh".to_string(),
                        "/run/capscode/keys.env".to_string(),
                    ],
                    stdin: Some(secret.clone()),
                    ..Default::default()
                },
            )
            .await?;
        if written.exit_code != 0 {
            return Err(RuntimeError::Container(
                container.0.clone(),
                format!("stdin file write failed: code={}", written.exit_code),
            ));
        }
        let readback = runtime
            .exec(
                &container,
                ExecSpec {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "stat -c '%a' /run/capscode/keys.env; cat /run/capscode/keys.env"
                            .to_string(),
                    ],
                    ..Default::default()
                },
            )
            .await?;
        let out = String::from_utf8_lossy(&readback.stdout);
        if !out.starts_with("600") || !out.contains("PRIVATE=super-secret-value") {
            return Err(RuntimeError::Container(
                container.0.clone(),
                format!("stdin file readback unexpected: {out:?}"),
            ));
        }
        println!("stdin_echo=ok stdin_file=ok mode_and_content={out:?}");
        Ok(())
    }
    .await;

    let _ = runtime.remove(&container, true).await;
    result
}

#[tokio::test]
#[ignore = "requires a live Docker-compatible daemon and public registry access"]
async fn live_runtime_round_trip() -> Result<(), RuntimeError> {
    let runtime = detect(RuntimePref::Auto).await?;
    let info = runtime.preflight().await?;
    let image = ImageRef("alpine:3".to_string());

    let mut pull_events = 0_u64;
    runtime
        .pull_image(&image, &mut |progress| {
            pull_events += 1;
            println!(
                "pull: layer={:?} status={} current={} total={:?}",
                progress.layer_id, progress.status, progress.current, progress.total
            );
        })
        .await?;
    if pull_events == 0 {
        return Err(RuntimeError::ImagePull(
            image.clone(),
            "pull progress callback did not fire".to_string(),
        ));
    }
    if !runtime.image_present(&image).await? {
        return Err(RuntimeError::ImagePull(
            image.clone(),
            "pulled image is not present".to_string(),
        ));
    }

    let session = SessionId::new();
    let unique = session.as_str();
    let labels = vec![
        (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
        (LABEL_SESSION.to_string(), unique.to_string()),
        (LABEL_ROLE.to_string(), ROLE_CAPSULE.to_string()),
        (
            LABEL_CREATED.to_string(),
            "2026-07-18T00:00:00Z".to_string(),
        ),
    ];
    let volume = runtime
        .create_volume(&VolumeSpec {
            name: format!("capscode-ws2-{unique}"),
            labels: labels.clone(),
        })
        .await?;

    let container = match runtime
        .create_container(&ContainerSpec {
            image,
            hostname: "workspace".to_string(),
            env: Vec::new(),
            cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
            entrypoint: None,
            mounts: vec![capscode_core::Mount::Volume {
                id: volume.clone(),
                target: PathBuf::from("/workspace"),
                read_only: false,
            }],
            network: NetworkMode::None,
            published_ports: Vec::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            no_new_privileges: false,
            devices: Vec::new(),
            sysctls: Vec::new(),
            labels,
            tmpfs_home: true,
            read_only_rootfs: false,
            init: false,
            pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
            memory_bytes: None,
        })
        .await
    {
        Ok(container) => container,
        Err(error) => {
            let _ = runtime.remove_volume(&volume).await;
            return Err(error);
        }
    };

    let result: Result<(String, String, bool, bool, i32), RuntimeError> = async {
        runtime.start(&container).await?;
        let output = runtime
            .exec(
                &container,
                ExecSpec {
                    cmd: vec!["echo".to_string(), "hello".to_string()],
                    ..Default::default()
                },
            )
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.exit_code != 0 || stdout != "hello" {
            return Err(RuntimeError::Container(
                container.0.clone(),
                format!(
                    "unexpected echo result: code={} stdout={stdout:?}",
                    output.exit_code
                ),
            ));
        }

        let owned = runtime.list_owned().await?;
        let found_container = owned
            .iter()
            .any(|object| object.kind == OwnedObjectKind::Container && object.id == container.0);
        let found_volume = owned
            .iter()
            .any(|object| object.kind == OwnedObjectKind::Volume && object.id == volume.0);
        if !found_container || !found_volume {
            return Err(RuntimeError::Container(
                container.0.clone(),
                "list_owned did not return both labeled objects".to_string(),
            ));
        }

        let (mut input_writer, input_reader) = tokio::io::duplex(1024);
        input_writer
            .write_all(b"interactive hello\n")
            .await
            .map_err(|error| RuntimeError::Container(container.0.clone(), error.to_string()))?;
        input_writer
            .shutdown()
            .await
            .map_err(|error| RuntimeError::Container(container.0.clone(), error.to_string()))?;
        let (stdout_writer, mut stdout_reader) = tokio::io::duplex(4096);
        let (stderr_writer, mut stderr_reader) = tokio::io::duplex(4096);
        let exit_code = runtime
            .run_interactive(
                &container,
                ExecSpec {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "IFS= read -r line; printf 'tty:%s\\n' \"$line\"".to_string(),
                    ],
                    tty: true,
                    ..Default::default()
                },
                AttachIo {
                    stdin: Box::new(input_reader),
                    stdout: Box::new(stdout_writer),
                    stderr: Box::new(stderr_writer),
                },
            )
            .await?;
        let mut tty_stdout = Vec::new();
        stdout_reader
            .read_to_end(&mut tty_stdout)
            .await
            .map_err(|error| RuntimeError::Container(container.0.clone(), error.to_string()))?;
        let mut tty_stderr = Vec::new();
        stderr_reader
            .read_to_end(&mut tty_stderr)
            .await
            .map_err(|error| RuntimeError::Container(container.0.clone(), error.to_string()))?;
        let tty_output = String::from_utf8_lossy(&tty_stdout).to_string();
        if exit_code != 0 || !tty_output.contains("tty:interactive hello") {
            return Err(RuntimeError::Container(
                container.0.clone(),
                format!(
                    "unexpected TTY result: code={exit_code} stdout={tty_output:?} stderr={:?}",
                    String::from_utf8_lossy(&tty_stderr)
                ),
            ));
        }

        Ok((stdout, tty_output, found_container, found_volume, exit_code))
    }
    .await;

    let remove_container = runtime.remove(&container, true).await;
    let remove_volume = runtime.remove_volume(&volume).await;
    let (stdout, tty_output, found_container, found_volume, tty_exit) = result?;
    remove_container?;
    remove_volume?;

    println!(
        "runtime={:?} version={} socket={:?}",
        info.kind, info.version, info.socket_path
    );
    println!("pull_progress_events={pull_events}");
    println!("exec_stdout={stdout:?}");
    println!("list_owned_container={found_container} list_owned_volume={found_volume}");
    println!("tty_exit={tty_exit} tty_output={tty_output:?}");
    println!("cleanup=container_removed,volume_removed");
    Ok(())
}
