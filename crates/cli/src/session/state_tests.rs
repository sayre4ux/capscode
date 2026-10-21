use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use capscode_core::{
    EgressReport, ExecOutput, IngressProgress, IngressReport, NetError, NetVerifyReport,
    OwnedObject, Provider, ProviderCredential, ProviderKeys, RuntimeCapabilities, RuntimeError,
    RuntimeInfo, RuntimeKind, VolumeSpec,
};
use secrecy::SecretString;

use super::*;

#[test]
fn old_numeric_created_labels_render_as_rfc3339() {
    assert_eq!(format_created_at("0"), "1970-01-01T00:00:00Z");
    assert_eq!(format_created_at("1786007224"), "2026-08-06T09:07:04Z");
    assert_eq!(
        format_created_at("2026-09-24T10:00:00Z"),
        "2026-09-24T10:00:00Z"
    );
}

fn options_with_config(config: Config) -> SessionOptions {
    let selected_credential = config.broker.keys.0.get(&Provider::Anthropic).cloned();
    let selected_provider = capscode_core::load_bundled_provider_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    SessionOptions {
        session: SessionId::from("00000000-0000-0000-0000-000000000009".to_string()),
        config,
        host_repo: PathBuf::from("."),
        tool_command: "claude".into(),
        tool_args: vec![],
        gateway_image: ImageRef("gateway:test".into()),
        capsule_image: ImageRef("capsule:test".into()),
        seeder_image: ImageRef("seeder:test".into()),
        selected_provider,
        selected_credential,
        selected_credential_source: None,
        extra_providers: Vec::new(),
        interactive_handoff: true,
        verbose: false,
        r#box: None,
    }
}

fn box_options() -> SessionOptions {
    let mut options = options_with_config(Config::default());
    let registry = capscode_core::load_bundled_provider_registry().unwrap();
    options.selected_provider = registry
        .iter()
        .find(|provider| provider.id == "anthropic")
        .unwrap()
        .clone();
    options.selected_credential = Some(ProviderCredential::api_key(SecretString::from(
        "anthropic-test-key",
    )));
    let openai = registry
        .iter()
        .find(|provider| provider.id == "openai")
        .unwrap()
        .clone();
    options.extra_providers.push((
        openai,
        ProviderCredential::api_key(SecretString::from("openai-test-key")),
    ));
    let mut tool_defaults = BTreeMap::new();
    for (tool, id) in [
        (Tool::Claude, "anthropic"),
        (Tool::Codex, "openai"),
        (Tool::Opencode, "anthropic"),
        (Tool::Pi, "openai"),
        (Tool::Omp, "anthropic"),
    ] {
        tool_defaults.insert(tool, id.to_string());
    }
    options.r#box = Some(BoxSpec {
        name: "demo-a1b2c3".into(),
        id: "a1b2c3".into(),
        project: "/work/demo".into(),
        project_hash: "abcd1234".into(),
        routed: vec!["anthropic".into(), "openai".into()],
        tool_defaults,
        toolchain: "fb24882402b5".into(),
        cache_volume: true,
    });
    options
}

fn codex_chatgpt_options() -> SessionOptions {
    let mut options = options_with_config(Config::default());
    options.tool_command = "codex".to_string();
    options.selected_provider = capscode_core::load_bundled_provider_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == "openai")
        .unwrap();
    options.selected_credential = Some(ProviderCredential::codex_access_token(SecretString::from(
        "real-chatgpt-access-token".to_string(),
    )));
    options
}

fn pi_options(provider_id: &str) -> SessionOptions {
    let mut options = options_with_config(Config::default());
    options.tool_command = "pi".to_string();
    options.selected_provider = capscode_core::load_bundled_provider_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .unwrap();
    options.selected_credential = Some(ProviderCredential::api_key(SecretString::from(
        "real-provider-key".to_string(),
    )));
    options
}

fn omp_options(provider_id: &str) -> SessionOptions {
    let mut options = pi_options(provider_id);
    options.tool_command = "omp".to_string();
    options
}

fn verified_net_report() -> NetVerifyReport {
    NetVerifyReport {
        egress_via_wireguard_only: true,
        resolver_in_tunnel: true,
        rfc1918_blocked: true,
        host_gateway_blocked: true,
        ipv6_contained: ProbeOutcome::Dropped,
        host_gateway: vec![
            ("default-gw".into(), ProbeOutcome::Dropped),
            ("host-alias".into(), ProbeOutcome::Dropped),
        ],
        public_probe: Some(ProbeOutcome::Reachable),
        egress_dev: "wg0".into(),
        egress_ip: Some("203.0.113.7".into()),
        egress_loc: Some("NL".into()),
        details: Vec::new(),
    }
}

#[test]
fn containment_panel_renders_tunnel_mode_all_verified() {
    let panel = render_containment_panel(&verified_net_report(), true, Caps::PLAIN);
    assert_eq!(
        panel,
        concat!(
            "containment   probes    RFC1918 10.255.255.1:80 dropped\n",
            "                        host-gateway default-gw TCP/22 dropped\n",
            "                        host-gateway host-alias TCP/22 dropped\n",
            "                        IPv6 host alias dropped\n",
            "              tunnel    route to 1.1.1.1 via wg0 · first resolver inside tunnel\n",
            "              egress    203.0.113.7 (NL) — observed, not a containment check\n",
            "              capsule   no env value equals a real credential",
        )
    );
}

#[test]
fn containment_panel_renders_ipv6_not_tested_reason() {
    let mut report = verified_net_report();
    report.ipv6_contained = ProbeOutcome::NotTested("host alias not resolved".into());
    let panel = render_containment_panel(&report, true, Caps::PLAIN);
    assert!(panel.contains("IPv6 host alias: host alias not resolved — not tested"));
    assert!(!panel.contains("IPv6 host alias dropped"));
}

#[test]
fn containment_panel_marks_reachable_probe_failed() {
    let mut report = verified_net_report();
    report.host_gateway[1].1 = ProbeOutcome::Reachable;
    let panel = render_containment_panel(&report, true, Caps::PLAIN);
    assert!(panel.contains("host-gateway host-alias TCP/22 reachable — failed"));
}

#[test]
fn containment_panel_renders_containment_only_without_claiming_no_vpn() {
    let mut report = verified_net_report();
    report.egress_via_wireguard_only = false;
    report.resolver_in_tunnel = false;
    report.egress_dev = "eth0".into();
    let panel = render_containment_panel(&report, false, Caps::PLAIN);
    assert!(panel.contains("VPN not enforced"));
    assert!(panel.contains("203.0.113.7 (NL) via eth0 — observed"));
    assert!(!panel.contains("no VPN"));
}

#[test]
fn containment_panel_renders_missing_egress_device_as_unavailable() {
    let mut report = verified_net_report();
    report.egress_dev.clear();
    let panel = render_containment_panel(&report, true, Caps::PLAIN);
    assert!(panel.contains("route to 1.1.1.1 via unavailable"));
    assert!(!panel.contains("via  "));
}

#[test]
fn containment_panel_gives_failed_public_probe_its_own_warning_line() {
    let mut report = verified_net_report();
    report.public_probe = Some(ProbeOutcome::Dropped);
    let panel = render_containment_panel(&report, true, Caps::PLAIN);
    assert!(panel
        .lines()
        .any(|line| line.trim() == "public probe 1.1.1.1:443 dropped — failed"));
}

#[test]
fn generated_branch_name_is_clear_and_session_unique() {
    assert_eq!(
        handoff_choices("capscode/00000000")[0],
        "Save as a branch (auto-generated: capscode/00000000)"
    );
}

fn capsule_env(spec: &ContainerSpec, key: &str) -> Option<String> {
    spec.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn capsule_tool_env_keys(spec: &ContainerSpec) -> Vec<String> {
    capsule_env(spec, "CAPSCODE_TOOL_ENV_KEYS")
        .expect("CLI-generated tool environment key list")
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn tmpfs_size(spec: &ContainerSpec, target: &str) -> Option<u64> {
    spec.mounts.iter().find_map(|mount| match mount {
        Mount::Tmpfs {
            target: path,
            size_mb,
            ..
        } if path == Path::new(target) => *size_mb,
        _ => None,
    })
}

fn tmpfs_exec(spec: &ContainerSpec, target: &str) -> Option<bool> {
    spec.mounts.iter().find_map(|mount| match mount {
        Mount::Tmpfs {
            target: path, exec, ..
        } if path == Path::new(target) => Some(*exec),
        _ => None,
    })
}

#[test]
fn parse_tmpfs_mib_defaults_and_parses() {
    assert_eq!(
        parse_tmpfs_mib(None, "CAPSCODE_HOME_TMPFS_MIB", DEFAULT_HOME_TMPFS_MIB).unwrap(),
        512
    );
    assert_eq!(
        parse_tmpfs_mib(None, "CAPSCODE_SHM_TMPFS_MIB", DEFAULT_SHM_TMPFS_MIB).unwrap(),
        256
    );
    assert_eq!(
        parse_tmpfs_mib(None, "CAPSCODE_TMP_TMPFS_MIB", DEFAULT_TMP_TMPFS_MIB).unwrap(),
        1024
    );
    assert_eq!(
        parse_tmpfs_mib(
            Some("2048"),
            "CAPSCODE_TMP_TMPFS_MIB",
            DEFAULT_TMP_TMPFS_MIB
        )
        .unwrap(),
        2048
    );
    assert_eq!(
        parse_tmpfs_mib(
            Some("128"),
            "CAPSCODE_HOME_TMPFS_MIB",
            DEFAULT_HOME_TMPFS_MIB
        )
        .unwrap(),
        128
    );
}

#[test]
fn parse_tmpfs_mib_rejects_bad_values() {
    let err = parse_tmpfs_mib(
        Some("not-a-number"),
        "CAPSCODE_HOME_TMPFS_MIB",
        DEFAULT_HOME_TMPFS_MIB,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("invalid CAPSCODE_HOME_TMPFS_MIB"), "{err}");

    let err = parse_tmpfs_mib(Some("0"), "CAPSCODE_SHM_TMPFS_MIB", DEFAULT_SHM_TMPFS_MIB)
        .unwrap_err()
        .to_string();
    assert!(err.contains("greater than 0"), "{err}");
}

#[test]
fn capsule_spec_bounds_home_and_shm_tmpfs_and_sets_init() {
    // Defaults apply when the override env vars are unset. Tests must not
    // set those vars (parse_tmpfs_mib covers override/error paths pure).
    let options = options_with_config(Config::default());
    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();
    assert_eq!(tmpfs_size(&spec, "/home/agent"), Some(512));
    assert_eq!(tmpfs_size(&spec, "/tmp"), Some(1024));
    assert_eq!(tmpfs_size(&spec, "/dev/shm"), Some(256));
    assert_eq!(tmpfs_exec(&spec, "/tmp"), Some(true));
    assert_eq!(tmpfs_exec(&spec, "/home/agent"), Some(false));
    assert_eq!(tmpfs_exec(&spec, "/dev/shm"), Some(false));
    assert!(spec.tmpfs_home);

    // Classic path: Docker's init must NOT be inserted or its existing
    // container stop behavior changes. The keeper socket dir and
    // CAPSCODE_KEEPER belong to devbox mode only.
    assert!(!spec.init, "init must be off outside devbox mode");
    assert_eq!(tmpfs_size(&spec, "/run/capscode-keeper"), None);
    assert!(!spec.env.iter().any(|(k, _)| k == "CAPSCODE_KEEPER"));
}

#[test]
fn capsule_smoke_mount_arguments_match_capsule_spec() {
    // DECISION: the smoke harness keeps its Docker CLI mount arguments in named
    // constants; this test reads those constants and pins them to the Rust spec.
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/smoke-tui.sh"),
    )
    .expect("smoke-tui.sh");
    let values = script
        .lines()
        .filter_map(|line| line.strip_prefix("SMOKE_"))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.trim_matches(['\'', '"']).to_string()))
        .collect::<HashMap<_, _>>();
    let value = |key: &str| {
        values
            .get(key)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("smoke-tui.sh is missing SMOKE_{key}"))
    };

    assert_eq!(value("TMP_TARGET"), "/tmp");
    assert_eq!(value("TMP_OPTIONS"), "rw,exec,nosuid,nodev");
    assert_eq!(
        value("TMP_SIZE_MIB").parse::<u64>().unwrap(),
        DEFAULT_TMP_TMPFS_MIB
    );
    assert_eq!(value("HOME_TARGET"), "/home/agent");
    assert_eq!(value("HOME_OPTIONS"), "rw,noexec,nosuid,nodev");
    assert_eq!(
        value("HOME_SIZE_MIB").parse::<u64>().unwrap(),
        DEFAULT_HOME_TMPFS_MIB
    );
    assert_eq!(value("SHM_TARGET"), "/dev/shm");
    assert_eq!(
        value("SHM_SIZE_MIB").parse::<u64>().unwrap(),
        DEFAULT_SHM_TMPFS_MIB
    );
    assert_eq!(value("WORKSPACE_TARGET"), "/workspace");

    let options = options_with_config(Config::default());
    for keeper_mode in [false, true] {
        let spec = capsule_spec_with(
            &options,
            &ContainerId("gw".into()),
            &VolumeId("vol".into()),
            "cert",
            keeper_mode,
            options.config.workspace.mode,
        )
        .unwrap();
        for (name, target, size, exec) in [
            ("TMP", "/tmp", DEFAULT_TMP_TMPFS_MIB, true),
            ("HOME", "/home/agent", DEFAULT_HOME_TMPFS_MIB, false),
            ("SHM", "/dev/shm", DEFAULT_SHM_TMPFS_MIB, false),
        ] {
            assert_eq!(value(&format!("{name}_TARGET")), target);
            assert_eq!(tmpfs_size(&spec, target), Some(size));
            assert_eq!(tmpfs_exec(&spec, target), Some(exec));
        }
        assert!(spec.mounts.iter().any(|mount| match mount {
            Mount::Volume { target, .. } | Mount::Bind { target, .. } => {
                target == Path::new(value("WORKSPACE_TARGET"))
            }
            Mount::Tmpfs { .. } => false,
        }));
    }
}

#[test]
fn capsule_env_redirects_caches_and_controls_opencode_lsp_download() {
    let options = options_with_config(Config::default());
    assert!(options.config.capsule.opencode_lsp_download);
    for keeper_mode in [false, true] {
        let spec = capsule_spec_with(
            &options,
            &ContainerId("gw".into()),
            &VolumeId("vol".into()),
            "cert",
            keeper_mode,
            options.config.workspace.mode,
        )
        .unwrap();
        assert_eq!(
            capsule_env(&spec, "XDG_CACHE_HOME").as_deref(),
            Some(CAPSULE_CACHE_ROOT)
        );
        assert_eq!(
            capsule_env(&spec, "npm_config_cache").as_deref(),
            Some("/tmp/.cache/npm")
        );
        assert_eq!(
            capsule_env(&spec, "BUN_INSTALL_CACHE_DIR").as_deref(),
            Some("/tmp/.cache/bun")
        );
        assert!(!spec.mounts.iter().any(|mount| matches!(
            mount,
            Mount::Volume { target, .. } if target == Path::new("/cache")
        )));
        assert_eq!(
            capsule_env(&spec, "OPENCODE_DISABLE_AUTOUPDATE").as_deref(),
            Some("1")
        );
        assert!(capsule_env(&spec, "OPENCODE_DISABLE_LSP_DOWNLOAD").is_none());
        let keys = capsule_tool_env_keys(&spec);
        for key in [
            "XDG_CACHE_HOME",
            "npm_config_cache",
            "BUN_INSTALL_CACHE_DIR",
            "OPENCODE_DISABLE_AUTOUPDATE",
        ] {
            assert!(keys.iter().any(|candidate| candidate == key), "{key}");
        }
    }

    let mut config = Config::default();
    config.capsule.opencode_lsp_download = false;
    let options = options_with_config(config);
    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();
    assert_eq!(
        capsule_env(&spec, "OPENCODE_DISABLE_LSP_DOWNLOAD").as_deref(),
        Some("1")
    );
    assert!(capsule_tool_env_keys(&spec)
        .iter()
        .any(|key| key == "OPENCODE_DISABLE_LSP_DOWNLOAD"));
}

#[test]
fn box_cache_mount_and_environment_follow_the_cache_volume_setting() {
    let options = box_options();
    let spec = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("workspace".into()),
        "cert",
        false,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert!(spec.mounts.iter().any(|mount| matches!(
        mount,
        Mount::Volume { id, target, read_only: false }
            if id == &cache_volume(&options.session) && target == Path::new("/cache")
    )));
    assert_eq!(
        capsule_env(&spec, "XDG_CACHE_HOME").as_deref(),
        Some("/cache")
    );
    assert_eq!(
        capsule_env(&spec, "npm_config_cache").as_deref(),
        Some("/cache/npm")
    );
    assert_eq!(
        capsule_env(&spec, "BUN_INSTALL_CACHE_DIR").as_deref(),
        Some("/cache/bun")
    );
    let keys = capsule_tool_env_keys(&spec);
    for key in [
        "XDG_CACHE_HOME",
        "npm_config_cache",
        "BUN_INSTALL_CACHE_DIR",
    ] {
        assert!(keys.iter().any(|candidate| candidate == key), "{key}");
    }

    let mut no_cache = box_options();
    no_cache.r#box.as_mut().unwrap().cache_volume = false;
    let spec = capsule_spec_with(
        &no_cache,
        &ContainerId("gw".into()),
        &VolumeId("workspace".into()),
        "cert",
        false,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert!(!spec.mounts.iter().any(|mount| matches!(
        mount,
        Mount::Volume { target, .. } if target == Path::new("/cache")
    )));
    assert_eq!(
        capsule_env(&spec, "XDG_CACHE_HOME").as_deref(),
        Some(CAPSULE_CACHE_ROOT)
    );
    assert_eq!(
        capsule_env(&spec, "npm_config_cache").as_deref(),
        Some("/tmp/.cache/npm")
    );
    assert_eq!(
        capsule_env(&spec, "BUN_INSTALL_CACHE_DIR").as_deref(),
        Some("/tmp/.cache/bun")
    );
}

#[test]
fn codex_daemon_signal_is_set_only_for_codex_routed_keeper_boxes() {
    let options = box_options();
    let routed = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert_eq!(
        capsule_env(&routed, "CAPSCODE_CODEX_DAEMON").as_deref(),
        Some("1")
    );
    assert!(capsule_tool_env_keys(&routed)
        .iter()
        .all(|key| key != "CAPSCODE_CODEX_DAEMON"));

    let one_shot = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        false,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert!(capsule_env(&one_shot, "CAPSCODE_CODEX_DAEMON").is_none());

    let mut unrouted_options = options;
    unrouted_options
        .r#box
        .as_mut()
        .unwrap()
        .routed
        .retain(|id| id != "openai");
    let unrouted = capsule_spec_with(
        &unrouted_options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert!(capsule_env(&unrouted, "CAPSCODE_CODEX_DAEMON").is_none());
}

#[test]
fn capsule_spec_carries_pids_cap_and_configured_memory_cap() {
    let gateway = ContainerId("gw".into());
    let volume = VolumeId("vol".into());
    let options = options_with_config(Config::default());
    let spec = capsule_spec(&options, &gateway, &volume, "cert").unwrap();
    assert_eq!(spec.pids_limit, 1024);
    assert_eq!(
        spec.memory_bytes, None,
        "memory is unlimited unless configured"
    );

    let mut config = Config::default();
    config.capsule.memory = Some(capscode_core::MemoryLimit::parse("4g").unwrap());
    config.capsule.pids = Some(capscode_core::PidsLimit::parse("2048").unwrap());
    let options = options_with_config(config);
    for devbox in [false, true] {
        let spec = capsule_spec_with(
            &options,
            &gateway,
            &volume,
            "cert",
            devbox,
            options.config.workspace.mode,
        )
        .unwrap();
        assert_eq!(spec.pids_limit, 2048);
        assert_eq!(spec.memory_bytes, Some(4 << 30));
    }
}

#[test]
fn capsule_spec_devbox_mode_adds_keeper_socket_init_and_env() {
    let options = options_with_config(Config::default());
    let spec = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        options.config.workspace.mode,
    )
    .unwrap();

    // The socket dir must be a writable mount: read_only_rootfs makes the
    // image's own /run/capscode-keeper unwritable, so tmux could not create
    // its socket and the keeper would never start.
    assert_eq!(tmpfs_size(&spec, "/run/capscode-keeper"), Some(1));
    assert!(spec.init, "keeper needs an init to reap orphans");
    assert!(spec
        .env
        .iter()
        .any(|(k, v)| k == "CAPSCODE_KEEPER" && v == "1"));

    // Copy mode: the workspace dies with the container, so the periodic WIP
    // commit is the box's only crash insurance and must stay on (the keeper
    // defaults to on when the var is absent).
    assert!(
        !spec.env.iter().any(|(k, _)| k == "KEEPER_AUTOSYNC"),
        "copy mode must leave the keeper's autosync default alone"
    );
}

#[test]
fn capsule_spec_devbox_mount_mode_disables_keeper_autosync() {
    let mut config = Config::default();
    config.workspace.mode = WorkspaceMode::Mount;
    let options = options_with_config(config);
    let spec = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        options.config.workspace.mode,
    )
    .unwrap();

    // /workspace is the user's own directory here, so a WIP commit writes to
    // their real repository. The entry banner already promises this is off.
    assert!(
        spec.env
            .iter()
            .any(|(k, v)| k == "KEEPER_AUTOSYNC" && v == "0"),
        "mount mode must switch the keeper's autosync loop off"
    );
    assert!(spec
        .env
        .iter()
        .any(|(key, value)| key == "CAPSCODE_WORKSPACE_MODE" && value == "mount"));
}

#[test]
fn keeper_and_classic_tool_env_keys_match_for_every_tool() {
    for tool in capscode_core::Tool::ALL.iter().map(|tool| tool.name()) {
        let mut options = options_with_config(Config::default());
        options.tool_command = tool.to_string();
        if tool == "codex" {
            options.selected_provider = capscode_core::load_bundled_provider_registry()
                .unwrap()
                .into_iter()
                .find(|provider| provider.id == "openai")
                .unwrap();
            options.selected_credential = None;
        }
        let classic = capsule_spec_with(
            &options,
            &ContainerId("gw".into()),
            &VolumeId("vol".into()),
            "cert",
            false,
            options.config.workspace.mode,
        )
        .unwrap();
        let keeper = capsule_spec_with(
            &options,
            &ContainerId("gw".into()),
            &VolumeId("vol".into()),
            "cert",
            true,
            options.config.workspace.mode,
        )
        .unwrap();
        let classic_keys = capsule_tool_env_keys(&classic);
        let keeper_keys = capsule_tool_env_keys(&keeper);
        assert_eq!(keeper_keys, classic_keys, "tool={tool}");

        let spec_keys = classic
            .env
            .iter()
            .map(|(key, _)| key.clone())
            .filter(|key| key != "CAPSCODE_TOOL_ENV_KEYS")
            .collect::<Vec<_>>();
        assert_eq!(classic_keys, spec_keys, "tool={tool}");
        assert!(classic_keys.contains(&tool_base_url_env(&options).to_string()));
        for ca_key in [
            "NODE_EXTRA_CA_CERTS",
            "SSL_CERT_FILE",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
            "GIT_SSL_CAINFO",
            "GIT_CONFIG_GLOBAL",
        ] {
            assert!(
                classic_keys.iter().any(|key| key == ca_key),
                "tool={tool} {ca_key}"
            );
        }
    }
}

#[test]
fn box_environment_has_non_colliding_tool_sentinels_and_only_routed_defaults() {
    let options = box_options();
    let spec = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        WorkspaceMode::Copy,
    )
    .unwrap();
    let env = |key: &str| capsule_env(&spec, key);
    assert_eq!(env("CAPSCODE_BOX").as_deref(), Some("demo-a1b2c3"));
    assert_eq!(
        env("ANTHROPIC_AUTH_TOKEN").as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(
        env("CAPSCODE_OPENCODE_API_KEY").as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(
        env(CAPSULE_PI_API_KEY_ENV).as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(
        env(CAPSULE_OMP_API_KEY_ENV).as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(env("OPENAI_API_KEY").as_deref(), Some(SENTINEL_API_KEY));
    assert!(env("ANTHROPIC_API_KEY").is_none());
    assert!(env("ANTHROPIC_BASE_URL").is_some());
    assert!(env("OPENAI_BASE_URL").is_some());
    assert_eq!(env("DISABLE_AUTOUPDATER").as_deref(), Some("1"));
    assert_eq!(env("CAPSCODE_PI_DEFAULT_MODEL").as_deref(), Some("gpt-5.5"));
    assert_eq!(
        env("CAPSCODE_OMP_DEFAULT_MODEL").as_deref(),
        Some("claude-opus-4-8")
    );
    // The box shims pass these to `--provider`, so they must name catalog entries.
    assert_eq!(
        env("CAPSCODE_PI_DEFAULT_PROVIDER").as_deref(),
        Some("capscode-openai")
    );
    assert_eq!(
        env("CAPSCODE_OMP_DEFAULT_PROVIDER").as_deref(),
        Some("capscode-anthropic")
    );
    let catalogs = generated_tool_configs(&options).unwrap();
    for (path, provider) in [
        ("/home/agent/.pi/agent/models.json", "capscode-openai"),
        ("/home/agent/.omp/agent/models.yml", "capscode-anthropic"),
    ] {
        let (_, contents) = catalogs.iter().find(|(file, _)| *file == path).unwrap();
        let document: serde_json::Value = serde_json::from_str(contents).unwrap();
        assert!(
            document["providers"][provider].is_object(),
            "{path} lacks {provider}"
        );
    }
    assert!(capsule_tool_env_keys(&spec).contains(&"CAPSCODE_OPENCODE_API_KEY".to_string()));

    let mut unrouted = box_options();
    let box_spec = unrouted.r#box.as_mut().unwrap();
    box_spec.routed.retain(|id| id == "openai");
    box_spec
        .tool_defaults
        .insert(Tool::Opencode, "anthropic".into());
    box_spec.tool_defaults.insert(Tool::Pi, "anthropic".into());
    box_spec.tool_defaults.insert(Tool::Omp, "anthropic".into());
    let spec = capsule_spec_with(
        &unrouted,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert!(capsule_env(&spec, "OPENAI_BASE_URL").is_some());
    assert!(capsule_env(&spec, "ANTHROPIC_BASE_URL").is_none());
    assert!(capsule_env(&spec, "ANTHROPIC_AUTH_TOKEN").is_none());
    assert!(capsule_env(&spec, "CAPSCODE_OPENCODE_API_KEY").is_none());
    assert!(capsule_env(&spec, CAPSULE_OMP_API_KEY_ENV).is_none());
    assert!(capsule_env(&spec, "CAPSCODE_PI_DEFAULT_MODEL").is_none());
    assert!(capsule_env(&spec, "CAPSCODE_PI_DEFAULT_PROVIDER").is_none());
}

#[test]
fn box_codex_config_routes_chatgpt_access_token_to_its_broker_host() {
    let mut options = codex_chatgpt_options();
    let mut defaults = BTreeMap::new();
    defaults.insert(Tool::Codex, "openai".to_string());
    options.r#box = Some(BoxSpec {
        name: "demo-a1b2c3".into(),
        id: "a1b2c3".into(),
        project: "/work/demo".into(),
        project_hash: "abcd1234".into(),
        routed: vec!["openai".into()],
        tool_defaults: defaults,
        toolchain: "fb24882402b5".into(),
        cache_volume: true,
    });

    let configs = generated_tool_configs(&options).unwrap();
    let codex = &configs
        .iter()
        .find(|(path, _)| *path == "/home/agent/.codex/config.toml")
        .unwrap()
        .1;
    let config: toml::Value = toml::from_str(codex).unwrap();
    assert_eq!(
        config["model_provider"].as_str(),
        Some("capscode-openai-chatgpt")
    );
    let provider = &config["model_providers"]["capscode-openai-chatgpt"];
    assert_eq!(
        provider["base_url"].as_str(),
        Some("https://codex-chatgpt.broker.capscode.local:8443")
    );
    assert_eq!(provider["env_key"].as_str(), Some(CAPSULE_CODEX_TOKEN_ENV));
    assert_eq!(config["check_for_update_on_startup"].as_bool(), Some(false));
    assert!(config["features"].get("daemon_auto_start").is_none());

    let capsule = capsule_spec_with(
        &options,
        &ContainerId("gateway".into()),
        &VolumeId("volume".into()),
        "cert",
        true,
        WorkspaceMode::Copy,
    )
    .unwrap();
    assert_eq!(
        capsule_env(&capsule, CAPSULE_CODEX_TOKEN_ENV).as_deref(),
        Some(SENTINEL_CODEX_ACCESS_TOKEN)
    );
    assert_eq!(
        capsule_env(&capsule, "OPENAI_BASE_URL").as_deref(),
        Some("https://codex-chatgpt.broker.capscode.local:8443")
    );
    assert!(!capsule
        .env
        .iter()
        .any(|(_, value)| value.contains("real-chatgpt-access-token")));
}

#[test]
fn box_generated_configs_cover_every_tool_and_every_routed_provider() {
    let options = box_options();
    let configs = generated_tool_configs(&options).unwrap();
    assert_eq!(configs.len(), 5);
    let paths = configs.iter().map(|(path, _)| *path).collect::<Vec<_>>();
    assert!(paths.contains(&"/home/agent/.claude.json"));
    assert!(paths.contains(&"/home/agent/.codex/config.toml"));
    let opencode = configs
        .iter()
        .find(|(path, _)| path.ends_with("opencode.json"))
        .unwrap()
        .1
        .as_str();
    assert!(opencode.contains("anthropic"));
    assert!(opencode.contains("openai"));
    assert!(opencode.contains("\"autoupdate\": false"));
    let pi = configs
        .iter()
        .find(|(path, _)| path.ends_with("models.json"))
        .unwrap()
        .1
        .as_str();
    assert!(pi.contains("capscode-anthropic"));
    assert!(pi.contains("capscode-openai"));
    let omp = configs
        .iter()
        .find(|(path, _)| path.ends_with("models.yml"))
        .unwrap()
        .1
        .as_str();
    assert!(omp.contains("capscode-anthropic"));
    assert!(omp.contains("capscode-openai"));
}

#[test]
fn box_labels_are_applied_to_capsule_and_workspace_volume() {
    let options = box_options();
    let spec = capsule_spec_with(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
        true,
        WorkspaceMode::Copy,
    )
    .unwrap();
    let cache_labels = cache_volume_labels(&options);
    for labels in [
        &spec.labels,
        &workspace_volume_labels(&options),
        &cache_labels,
    ] {
        assert_eq!(
            labels.iter().find(|(key, _)| key == LABEL_BOX).unwrap().1,
            "demo-a1b2c3"
        );
        assert_eq!(
            labels
                .iter()
                .find(|(key, _)| key == LABEL_BOX_ID)
                .unwrap()
                .1,
            "a1b2c3"
        );
        assert_eq!(
            labels
                .iter()
                .find(|(key, _)| key == LABEL_PROJECT)
                .unwrap()
                .1,
            "/work/demo"
        );
        assert_eq!(
            labels
                .iter()
                .find(|(key, _)| key == LABEL_PROJECT_HASH)
                .unwrap()
                .1,
            "abcd1234"
        );
        assert_eq!(
            labels
                .iter()
                .find(|(key, _)| key == LABEL_PROVIDERS)
                .unwrap()
                .1,
            "anthropic,openai"
        );
    }
    assert_eq!(
        cache_labels
            .iter()
            .find(|(key, _)| key == LABEL_ROLE)
            .unwrap()
            .1,
        capscode_core::ROLE_CACHE
    );
    assert_eq!(
        cache_labels
            .iter()
            .find(|(key, _)| key == LABEL_OWNER)
            .unwrap()
            .1,
        capscode_core::LABEL_OWNER_VALUE
    );
    assert_eq!(
        cache_labels
            .iter()
            .find(|(key, _)| key == LABEL_SESSION)
            .unwrap()
            .1,
        options.session.to_string()
    );
    assert_eq!(
        cache_labels
            .iter()
            .find(|(key, _)| key == LABEL_TOOLCHAIN)
            .unwrap()
            .1,
        "fb24882402b5"
    );
}

#[tokio::test]
async fn provision_existing_preserves_volume_and_labels_gateway_on_failure() {
    let _home_lock = TEST_CONFIG_HOME_LOCK.lock().await;
    let temp_home = tempfile::tempdir().unwrap();
    let _restore_home = RestoreEnv::set("CAPSCODE_HOME", temp_home.path());
    let runtime = Arc::new(MockRuntime::new());
    let mut driver = SessionDriver::new(
        box_options(),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    let volume = VolumeId("existing-box-volume".into());
    runtime
        .create_volume(&VolumeSpec {
            name: volume.0.clone(),
            labels: workspace_volume_labels(&driver.options),
        })
        .await
        .unwrap();
    assert_eq!(
        runtime.created_volume_labels.lock().unwrap()[0]
            .iter()
            .find(|(key, _)| key == LABEL_BOX)
            .unwrap()
            .1,
        "demo-a1b2c3"
    );
    let result = driver.provision_existing(volume.clone()).await;
    assert!(result.is_err());
    assert!(!runtime.removed_volume.load(Ordering::SeqCst));
    let gateway_labels = runtime.created_labels.lock().unwrap();
    let gateway = gateway_labels.first().expect("gateway spec captured");
    assert_eq!(
        gateway.iter().find(|(key, _)| key == LABEL_BOX).unwrap().1,
        "demo-a1b2c3"
    );
    assert_eq!(
        gateway
            .iter()
            .find(|(key, _)| key == LABEL_PROVIDERS)
            .unwrap()
            .1,
        "anthropic,openai"
    );
}

#[tokio::test]
async fn provision_existing_keeps_volume_when_box_is_ready_but_config_fails() {
    let _home_lock = TEST_CONFIG_HOME_LOCK.lock().await;
    let temp_home = tempfile::tempdir().unwrap();
    let _restore_home = RestoreEnv::set("CAPSCODE_HOME", temp_home.path());
    let runtime = Arc::new(MockRuntime::new());
    runtime.fail_gateway.store(false, Ordering::SeqCst);
    runtime.fail_tool_config.store(true, Ordering::SeqCst);
    let mut driver = SessionDriver::new(
        box_options(),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    let result = driver
        .provision_existing(VolumeId("existing-box-volume".into()))
        .await;
    assert!(result.is_err());
    assert!(!driver.box_ready);
    assert!(!runtime.removed_volume.load(Ordering::SeqCst));
    assert_eq!(runtime.removed_containers.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn provision_existing_does_not_inspect_or_pull_the_seeder_image() {
    let _home_lock = TEST_CONFIG_HOME_LOCK.lock().await;
    let temp_home = tempfile::tempdir().unwrap();
    let _restore_home = RestoreEnv::set("CAPSCODE_HOME", temp_home.path());
    let runtime = Arc::new(MockRuntime::new());
    runtime.fail_gateway.store(false, Ordering::SeqCst);
    let mut driver = SessionDriver::new(
        box_options(),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();

    driver
        .provision_existing(VolumeId("existing-box-volume".into()))
        .await
        .unwrap();

    assert_eq!(runtime.image_inspections.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.image_pulls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn detach_leaves_resources_populated_destroy_empties_them() {
    let mut driver = SessionDriver::new(
        options_with_config(Config::default()),
        Arc::new(MockRuntime::new()),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    driver.resources.gateway = Some(ContainerId("gateway".into()));
    driver.resources.volume = Some(VolumeId("volume".into()));
    driver.work_captured = true;

    driver.detach().await.unwrap();
    assert!(driver.resources.capsule.is_some());
    assert!(driver.resources.gateway.is_some());
    assert!(driver.resources.volume.is_some());

    driver.destroy().await.unwrap();
    assert!(driver.resources.capsule.is_none());
    assert!(driver.resources.gateway.is_none());
    assert!(driver.resources.volume.is_none());
}

#[tokio::test]
async fn keeper_draining_and_failed_handoff_only_detach() {
    let runtime = Arc::new(MockRuntime::new());
    let mut driver = SessionDriver::new(
        options_with_config(Config::default()),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.keeper_mode = true;
    driver.box_ready = true;
    driver.state = State::Draining;
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    driver.resources.gateway = Some(ContainerId("gateway".into()));
    driver.resources.volume = Some(VolumeId("volume".into()));
    driver.resources.ingress_receipt = Some(IngressReceipt::Shadow {
        git_dir: PathBuf::from("shadow.git"),
        baseline: git2::Oid::ZERO_SHA1.to_string(),
        state_dir: PathBuf::from("state"),
    });

    driver.advance().await.unwrap();
    assert_eq!(driver.state, State::TearingDown);
    assert!(!driver.work_captured);
    driver.advance().await.unwrap();
    assert_eq!(driver.state, State::Destroyed);
    assert!(driver.resources.capsule.is_some());
    assert!(driver.resources.gateway.is_some());
    assert!(driver.resources.volume.is_some());
    assert!(!runtime.removed_volume.load(Ordering::SeqCst));

    driver.state = State::Failed;
    driver.failure = Some(anyhow!("handoff failed"));
    driver.advance().await.unwrap();
    assert_eq!(driver.state, State::TearingDown);
    driver.advance().await.unwrap();
    assert_eq!(driver.state, State::Destroyed);
    assert!(driver.resources.capsule.is_some());
    assert!(driver.resources.gateway.is_some());
    assert!(driver.resources.volume.is_some());
    assert!(!runtime.removed_volume.load(Ordering::SeqCst));
}

#[test]
fn capsule_env_oauth_sets_oauth_sentinel_not_api_key() {
    let mut config = Config::default();
    let mut map = HashMap::new();
    map.insert(
        Provider::Anthropic,
        ProviderCredential::oauth(SecretString::from("sk-ant-oat01-REAL-token".to_string())),
    );
    config.broker.keys = ProviderKeys(map);
    let options = options_with_config(config);

    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "-----BEGIN CERTIFICATE-----\nMII...\n-----END CERTIFICATE-----\n",
    )
    .unwrap();

    // OAuth mode: the oauth sentinel is set, the api-key sentinel is absent.
    assert_eq!(
        capsule_env(&spec, "CLAUDE_CODE_OAUTH_TOKEN").as_deref(),
        Some(SENTINEL_OAUTH_TOKEN)
    );
    assert_eq!(capsule_env(&spec, "ANTHROPIC_API_KEY"), None);
    // Base URL still points at the broker vhost.
    assert_eq!(
        capsule_env(&spec, "ANTHROPIC_BASE_URL").as_deref(),
        Some("https://anthropic.broker.capscode.local:8443")
    );
    // The real token never crossed into the capsule env.
    assert!(!spec
        .env
        .iter()
        .any(|(_, v)| v.contains("sk-ant-oat01-REAL-token")));
    // The leak guard agrees.
    assert_capsule_has_no_real_keys(
        &spec,
        &options.config,
        options.selected_credential.as_ref(),
        &options.extra_providers,
    )
    .unwrap();
}

#[test]
fn claude_api_key_uses_auth_token_sentinel_without_api_key_prompt() {
    let mut config = Config::default();
    let mut map = HashMap::new();
    map.insert(
        Provider::Anthropic,
        ProviderCredential::api_key(SecretString::from("sk-ant-api03-REAL-key".to_string())),
    );
    config.broker.keys = ProviderKeys(map);
    let options = options_with_config(config);

    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();

    assert_eq!(
        capsule_env(&spec, "ANTHROPIC_AUTH_TOKEN").as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(capsule_env(&spec, "ANTHROPIC_API_KEY"), None);
    assert_eq!(capsule_env(&spec, "CLAUDE_CODE_OAUTH_TOKEN"), None);
    assert!(!spec
        .env
        .iter()
        .any(|(_, v)| v.contains("sk-ant-api03-REAL-key")));
}

#[test]
fn claude_without_anthropic_credential_still_uses_auth_token_sentinel() {
    let options = options_with_config(Config::default());
    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();
    assert_eq!(
        capsule_env(&spec, "ANTHROPIC_AUTH_TOKEN").as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(capsule_env(&spec, "ANTHROPIC_API_KEY"), None);
    assert_eq!(capsule_env(&spec, "CLAUDE_CODE_OAUTH_TOKEN"), None);
}

#[test]
fn claude_custom_provider_uses_auth_token_sentinel_without_api_key_prompt() {
    let mut options = options_with_config(Config::default());
    options.selected_provider = ProviderSpec {
        id: "acme".to_string(),
        label: None,
        api_style: ApiStyle::Anthropic,
        base_url: "https://llm.internal.example/prod/gateway/v1".to_string(),
        auth: AuthScheme::Bearer,
        key_env: "ACME_TOKEN".to_string(),
        default_model: None,
        models: Vec::new(),
        experimental: false,
    };
    options.selected_credential = Some(ProviderCredential::api_key(SecretString::from(
        "real-acme-token".to_string(),
    )));

    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();

    assert_eq!(
        capsule_env(&spec, "ANTHROPIC_AUTH_TOKEN").as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(capsule_env(&spec, "ANTHROPIC_API_KEY"), None);
    assert_eq!(
        capsule_env(&spec, "ANTHROPIC_BASE_URL").as_deref(),
        Some("https://acme.broker.capscode.local:8443")
    );
    assert!(!spec
        .env
        .iter()
        .any(|(_, value)| value.contains("real-acme-token")));
}

#[test]
fn kimi_manifest_matches_gateway_contract() {
    let provider = capscode_core::load_bundled_provider_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == "kimi")
        .unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&provider_manifest_json(&provider, None).unwrap()).unwrap();
    assert_eq!(
        manifest,
        serde_json::json!([{
            "id": "kimi",
            "vhost": "kimi.broker.capscode.local",
            "authority": "api.moonshot.ai",
            "base_path": "/anthropic",
            "api_style": "anthropic",
            "inject": "bearer",
            "key_env": "MOONSHOT_API_KEY"
        }])
    );
}

#[test]
fn capsule_base_url_uses_selected_provider_vhost_while_broker_owns_path() {
    let mut options = options_with_config(Config::default());
    options.selected_provider = capscode_core::load_bundled_provider_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == "kimi")
        .unwrap();
    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();
    assert_eq!(
        capsule_env(&spec, "ANTHROPIC_BASE_URL").as_deref(),
        Some("https://kimi.broker.capscode.local:8443")
    );
    assert_eq!(
        capsule_env(&spec, "CAPSCODE_BROKER_ALIASES").as_deref(),
        Some("kimi.broker.capscode.local")
    );
}

#[test]
fn opencode_config_is_valid_json_with_vhost_and_sentinel() {
    let mut options = pi_options("kimi-coding-openai");
    options.tool_command = "opencode".to_string();
    let (path, rendered) = generated_tool_config(&options).unwrap().unwrap();
    assert_eq!(path, "/home/agent/.config/opencode/opencode.json");
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        document["enabled_providers"],
        serde_json::json!(["kimi-coding-openai"])
    );
    assert_eq!(
        document["provider"]["kimi-coding-openai"]["options"]["baseURL"],
        "https://kimi-coding-openai.broker.capscode.local:8443"
    );
    assert_eq!(
        document["provider"]["kimi-coding-openai"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
    assert_eq!(
        document["provider"]["kimi-coding-openai"]["models"],
        serde_json::json!({
            "kimi-for-coding": { "name": "CapsCode kimi-for-coding" },
            "kimi-for-coding-highspeed": { "name": "CapsCode kimi-for-coding-highspeed" },
            "k3": { "name": "CapsCode k3" },
        })
    );
}

#[test]
fn pi_config_defines_a_model_for_every_compatible_bundled_provider() {
    let registry = capscode_core::load_bundled_provider_registry().unwrap();
    for provider in registry
        .into_iter()
        .filter(|provider| provider.supports_tool(capscode_core::Tool::Pi))
    {
        let options = pi_options(&provider.id);
        let (path, rendered) = generated_tool_config(&options).unwrap().unwrap();
        assert_eq!(path, "/home/agent/.pi/agent/models.json");
        let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let configured = &document["providers"][format!("capscode-{}", provider.id)];
        assert_eq!(
            configured["baseUrl"],
            format!("https://{}.broker.capscode.local:8443", provider.id)
        );
        assert_eq!(configured["apiKey"], "$CAPSCODE_PI_API_KEY");
        assert!(
            configured["models"]
                .as_array()
                .is_some_and(|models| !models.is_empty()),
            "provider '{}' has no generated Pi models: {rendered}",
            provider.id
        );
    }
}

#[test]
fn pi_config_lists_all_kimi_coding_models_and_unlocks_k3_context() {
    for provider_id in ["kimi-coding", "kimi-coding-openai"] {
        let options = pi_options(provider_id);
        let (_, rendered) = generated_tool_config(&options).unwrap().unwrap();
        let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let models = &document["providers"][format!("capscode-{provider_id}")]["models"];
        assert_eq!(
            models,
            &serde_json::json!([
                {
                    "id": "kimi-for-coding",
                    "name": "CapsCode kimi-for-coding"
                },
                {
                    "id": "kimi-for-coding-highspeed",
                    "name": "CapsCode kimi-for-coding-highspeed"
                },
                {
                    "id": "k3",
                    "name": "CapsCode k3",
                    "contextWindow": 1_048_576
                }
            ])
        );
    }
}

#[test]
fn pi_capsule_uses_a_dedicated_sentinel_that_cannot_activate_builtins() {
    for provider_id in ["kimi", "kimi-openai"] {
        let options = pi_options(provider_id);
        let spec = capsule_spec(
            &options,
            &ContainerId("gw".into()),
            &VolumeId("vol".into()),
            "cert",
        )
        .unwrap();
        assert_eq!(
            capsule_env(&spec, "CAPSCODE_PI_API_KEY").as_deref(),
            Some(SENTINEL_API_KEY)
        );
        assert_eq!(capsule_env(&spec, "OPENAI_API_KEY"), None);
        assert_eq!(capsule_env(&spec, "ANTHROPIC_API_KEY"), None);
        assert_eq!(capsule_env(&spec, "CLAUDE_CODE_OAUTH_TOKEN"), None);
        assert!(!spec
            .env
            .iter()
            .any(|(_, value)| value.contains("real-provider-key")));
    }
}

#[test]
fn pi_command_forces_the_broker_provider_and_default_model() {
    let options = pi_options("kimi-openai");
    assert_eq!(
        pi_command(&options).unwrap(),
        [
            "pi",
            "--provider",
            "capscode-kimi-openai",
            "--model",
            "kimi-k2.6"
        ]
    );
}

#[test]
fn pi_command_allows_a_model_override_but_rejects_route_and_key_overrides() {
    let mut options = pi_options("kimi-openai");
    options.tool_args = vec!["--model".into(), "kimi-k2.7-code".into()];
    assert_eq!(
        pi_command(&options).unwrap(),
        [
            "pi",
            "--provider",
            "capscode-kimi-openai",
            "--model",
            "kimi-k2.7-code"
        ]
    );

    for forbidden in ["--provider=openai", "--api-key=real-key"] {
        options.tool_args = vec![forbidden.into()];
        let error = pi_command(&options).unwrap_err().to_string();
        assert!(error.contains("controlled by CapsCode"), "{error}");
    }
}

#[test]
fn pi_command_fails_closed_without_registry_model_metadata() {
    let mut options = pi_options("kimi-openai");
    options.selected_provider.default_model = None;
    let error = pi_command(&options).unwrap_err().to_string();
    assert!(error.contains("has no default_model"), "{error}");
}

#[test]
fn injection_rejects_control_characters_and_names_the_credential_source() {
    let options = pi_options("kimi-openai");
    let credential =
        ProviderCredential::api_key(SecretString::from("env-secret\ninjected".to_string()));
    let error = validate_injected_credential(
        &options.selected_provider,
        &credential,
        Some("env:XAI_API_KEY"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("env:XAI_API_KEY"), "{error}");
    assert!(!error.contains("env-secret"), "{error}");
}

#[test]
fn omp_config_writes_models_yml_with_bare_env_api_key() {
    let options = omp_options("kimi-openai");
    let (path, rendered) = generated_tool_config(&options).unwrap().unwrap();
    assert_eq!(path, "/home/agent/.omp/agent/models.yml");
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let configured = &document["providers"]["capscode-kimi-openai"];
    assert_eq!(
        configured["baseUrl"],
        "https://kimi-openai.broker.capscode.local:8443"
    );
    // omp resolves apiKey as an env-var NAME — no `$` prefix.
    assert_eq!(configured["apiKey"], "CAPSCODE_OMP_API_KEY");
    assert_eq!(configured["api"], "openai-completions");
    assert!(
        configured["models"]
            .as_array()
            .is_some_and(|models| !models.is_empty()),
        "omp models missing: {rendered}"
    );
}

#[test]
fn omp_config_uses_anthropic_messages_api_for_anthropic_providers() {
    let options = omp_options("anthropic");
    let (_, rendered) = generated_tool_config(&options).unwrap().unwrap();
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        document["providers"]["capscode-anthropic"]["api"],
        "anthropic-messages"
    );
    assert_eq!(
        document["providers"]["capscode-anthropic"]["apiKey"],
        "CAPSCODE_OMP_API_KEY"
    );
}

#[test]
fn omp_command_pins_provider_model_form() {
    let options = omp_options("kimi-openai");
    assert_eq!(
        omp_command(&options).unwrap(),
        ["omp", "--model", "capscode-kimi-openai/kimi-k2.6"]
    );
}

#[test]
fn omp_command_allows_model_override_but_rejects_route_and_key_overrides() {
    let mut options = omp_options("kimi-openai");
    options.tool_args = vec![
        "--model".into(),
        "capscode-kimi-openai/kimi-k2.7-code".into(),
    ];
    assert_eq!(
        omp_command(&options).unwrap(),
        ["omp", "--model", "capscode-kimi-openai/kimi-k2.7-code"]
    );

    for forbidden in [
        "--provider=openai",
        "--api-key=real-key",
        "--profile=escape",
    ] {
        options.tool_args = vec![forbidden.into()];
        let error = omp_command(&options).unwrap_err().to_string();
        assert!(error.contains("controlled by CapsCode"), "{error}");
    }
}

#[test]
fn omp_capsule_uses_dedicated_sentinel_and_broker_base_url() {
    let options = omp_options("kimi");
    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();
    assert_eq!(
        capsule_env(&spec, "CAPSCODE_OMP_API_KEY").as_deref(),
        Some(SENTINEL_API_KEY)
    );
    assert_eq!(capsule_env(&spec, "OMP_SKIP_SETUP").as_deref(), Some("1"));
    assert_eq!(
        capsule_env(&spec, tool_base_url_env(&options)).as_deref(),
        Some(
            format!(
                "https://{}.broker.capscode.local:8443",
                options.selected_provider.id
            )
            .as_str()
        )
    );
    assert_eq!(capsule_env(&spec, "OPENAI_API_KEY"), None);
    assert_eq!(capsule_env(&spec, "ANTHROPIC_API_KEY"), None);
    assert_eq!(capsule_env(&spec, "CAPSCODE_PI_API_KEY"), None);
    assert!(!spec
        .env
        .iter()
        .any(|(_, value)| value.contains("real-provider-key")));
}

#[test]
fn codex_chatgpt_manifest_has_chat_route_for_gateway_secret() {
    let options = codex_chatgpt_options();
    let manifest: serde_json::Value = serde_json::from_str(
        &provider_manifest_json(
            &options.selected_provider,
            options.selected_credential.as_ref(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest,
        serde_json::json!([{
            "id": "openai-chatgpt",
            "vhost": CODEX_CHATGPT_VHOST,
            "authority": "chatgpt.com",
            "base_path": "/backend-api/codex",
            "api_style": "openai",
            "inject": "bearer",
            "key_env": CODEX_ACCESS_TOKEN_ENV
        }])
    );
}

#[test]
fn codex_chatgpt_capsule_gets_only_sentinel_and_broker_routes() {
    let options = codex_chatgpt_options();
    let spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();

    assert_eq!(
        capsule_env(&spec, CAPSULE_CODEX_TOKEN_ENV).as_deref(),
        Some(SENTINEL_CODEX_ACCESS_TOKEN)
    );
    assert_eq!(capsule_env(&spec, CODEX_ACCESS_TOKEN_ENV), None);
    assert_eq!(capsule_env(&spec, "OPENAI_API_KEY"), None);
    assert_eq!(
        capsule_env(&spec, "OPENAI_BASE_URL").as_deref(),
        Some(format!("https://{CODEX_CHATGPT_VHOST}:8443").as_str())
    );
    assert_eq!(capsule_env(&spec, "CODEX_AUTHAPI_BASE_URL"), None);
    assert_eq!(
        capsule_env(&spec, "CAPSCODE_BROKER_ALIASES").as_deref(),
        Some("codex-chatgpt.broker.capscode.local")
    );
    assert!(!spec
        .env
        .iter()
        .any(|(_, value)| value.contains("real-chatgpt-access-token")));
    assert_capsule_has_no_real_keys(
        &spec,
        &options.config,
        options.selected_credential.as_ref(),
        &options.extra_providers,
    )
    .unwrap();
}

#[test]
fn codex_chatgpt_config_points_chat_traffic_at_broker() {
    let options = codex_chatgpt_options();
    let (path, rendered) = generated_tool_config(&options).unwrap().unwrap();
    assert_eq!(path, "/home/agent/.codex/config.toml");
    let document: toml::Value = toml::from_str(&rendered).unwrap();
    assert_eq!(
        document["model_provider"].as_str(),
        Some("capscode-chatgpt")
    );
    let provider = &document["model_providers"]["capscode-chatgpt"];
    assert_eq!(provider["name"].as_str(), Some("CapsCode ChatGPT"));
    assert_eq!(
        provider["base_url"].as_str(),
        Some("https://codex-chatgpt.broker.capscode.local:8443")
    );
    assert_eq!(provider["env_key"].as_str(), Some(CAPSULE_CODEX_TOKEN_ENV));
    assert_eq!(
        document["check_for_update_on_startup"].as_bool(),
        Some(false)
    );
    assert!(document["features"].get("daemon_auto_start").is_none());
    assert_eq!(provider["wire_api"].as_str(), Some("responses"));
    assert_eq!(provider["requires_openai_auth"].as_bool(), Some(false));
    assert_eq!(
        document["features"]["shell_snapshot"].as_bool(),
        Some(false)
    );
    assert!(!rendered.contains("real-chatgpt-access-token"));
}

#[test]
fn leak_guard_checks_selected_custom_provider_credential() {
    let options = options_with_config(Config::default());
    let credential =
        ProviderCredential::api_key(SecretString::from("custom-real-secret".to_string()));
    let mut spec = capsule_spec(
        &options,
        &ContainerId("gw".into()),
        &VolumeId("vol".into()),
        "cert",
    )
    .unwrap();
    spec.env
        .push(("ACCIDENTAL".to_string(), "custom-real-secret".to_string()));
    assert!(
        assert_capsule_has_no_real_keys(&spec, &options.config, Some(&credential), &[],).is_err()
    );

    let extra = ProviderCredential::api_key(SecretString::from("extra-real-secret".to_string()));
    spec.env
        .push(("CAPSCODE_TEST_LEAK".into(), "extra-real-secret".into()));
    assert!(assert_capsule_has_no_real_keys(
        &spec,
        &options.config,
        None,
        &[(options.selected_provider.clone(), extra)],
    )
    .is_err());
}

#[tokio::test]
async fn local_capscode_images_are_inspected_and_never_pulled() {
    let runtime = MockRuntime::new();
    runtime.image_present.store(false, Ordering::SeqCst);
    let image = ImageRef("capscode/gateway:0.1.0".into());
    let error = ensure_image(
        &runtime,
        &image,
        PullPolicy::IfMissing,
        false,
        &NO_LAUNCH_OBSERVER,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "image capscode/gateway:0.1.0 is not built. Build it with: {}",
            crate::build_context::image_build_hint()
        )
    );
    assert_eq!(runtime.image_inspections.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.image_pulls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn always_pull_is_rejected_for_local_capscode_images() {
    let runtime = MockRuntime::new();
    let image = ImageRef("capscode/gateway:0.1.0".into());
    let error = ensure_image(
        &runtime,
        &image,
        PullPolicy::Always,
        false,
        &NO_LAUNCH_OBSERVER,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("pull policy 'always'"));
    assert_eq!(runtime.image_inspections.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.image_pulls.load(Ordering::SeqCst), 0);
}

struct MockRuntime {
    removed_volume: AtomicBool,
    image_present: AtomicBool,
    image_inspections: AtomicUsize,
    image_pulls: AtomicUsize,
    create_release: Option<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    fail_gateway: AtomicBool,
    fail_tool_config: AtomicBool,
    removed_containers: AtomicUsize,
    tool_exit_code: AtomicI32,
    keeper_tool_exit_status: AtomicI32,
    interactive_commands: Mutex<Vec<Vec<String>>>,
    interactive_entered: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    created_labels: Mutex<Vec<Vec<(String, String)>>>,
    created_volume_labels: Mutex<Vec<Vec<(String, String)>>>,
}

struct RestoreEnv {
    name: &'static str,
    old: Option<std::ffi::OsString>,
}

impl RestoreEnv {
    fn set(name: &'static str, value: &std::path::Path) -> Self {
        let old = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, old }
    }
}

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        if let Some(value) = self.old.take() {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

static TEST_CONFIG_HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl MockRuntime {
    fn new() -> Self {
        Self {
            removed_volume: AtomicBool::new(false),
            image_present: AtomicBool::new(true),
            image_inspections: AtomicUsize::new(0),
            image_pulls: AtomicUsize::new(0),
            create_release: None,
            fail_gateway: AtomicBool::new(true),
            fail_tool_config: AtomicBool::new(false),
            removed_containers: AtomicUsize::new(0),
            tool_exit_code: AtomicI32::new(0),
            keeper_tool_exit_status: AtomicI32::new(-1),
            interactive_commands: Mutex::new(Vec::new()),
            interactive_entered: None,
            created_labels: Mutex::new(Vec::new()),
            created_volume_labels: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl RuntimeAdapter for MockRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Docker
    }
    async fn preflight(&self) -> Result<RuntimeInfo, RuntimeError> {
        Ok(RuntimeInfo {
            kind: RuntimeKind::Docker,
            version: "mock".into(),
            socket_path: None,
            capabilities: self.capabilities(),
        })
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            localhost_publish: capscode_core::PublishMode::Native,
            shared_netns: true,
            tmpfs_mounts: true,
            cap_add_net_admin: true,
            dev_net_tun: true,
        }
    }
    async fn create_volume(&self, spec: &VolumeSpec) -> Result<VolumeId, RuntimeError> {
        self.created_volume_labels
            .lock()
            .unwrap()
            .push(spec.labels.clone());
        Ok(VolumeId(spec.name.clone()))
    }
    async fn remove_volume(&self, _: &VolumeId) -> Result<(), RuntimeError> {
        self.removed_volume.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn image_present(&self, _: &ImageRef) -> Result<bool, RuntimeError> {
        self.image_inspections.fetch_add(1, Ordering::SeqCst);
        Ok(self.image_present.load(Ordering::SeqCst))
    }
    async fn pull_image(
        &self,
        _: &ImageRef,
        _: &mut (dyn FnMut(capscode_core::PullProgress) + Send),
    ) -> Result<(), RuntimeError> {
        self.image_pulls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn create_container(&self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        self.created_labels
            .lock()
            .unwrap()
            .push(spec.labels.clone());
        if let Some(release) = &self.create_release {
            let receiver = release.lock().await.take();
            if let Some(receiver) = receiver {
                let _ = receiver.await;
            }
        }
        if self.fail_gateway.load(Ordering::SeqCst)
            && spec
                .labels
                .iter()
                .any(|(key, value)| key == LABEL_ROLE && value == capscode_core::ROLE_GATEWAY)
        {
            Err(RuntimeError::Container(
                "gateway".into(),
                "injected provisioning failure".into(),
            ))
        } else {
            Ok(ContainerId("container".into()))
        }
    }
    async fn start(&self, _: &ContainerId) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(&self, _: &ContainerId, spec: ExecSpec) -> Result<ExecOutput, RuntimeError> {
        let command = spec.cmd.join(" ");
        if self.fail_tool_config.load(Ordering::SeqCst)
            && command.contains(INJECT_TOOL_CONFIG_SCRIPT)
        {
            return Ok(ExecOutput {
                stdout: vec![],
                stderr: b"injected tool config failure".to_vec(),
                exit_code: 1,
            });
        }
        if command.contains("tool-exit-status") {
            let status = self.keeper_tool_exit_status.load(Ordering::SeqCst);
            return Ok(ExecOutput {
                stdout: if status < 0 {
                    vec![]
                } else {
                    format!("{status}\n").into_bytes()
                },
                stderr: vec![],
                exit_code: 0,
            });
        }
        if command.contains("/etc/capscode/broker-ca.pem") {
            return Ok(ExecOutput {
                stdout: b"-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n".to_vec(),
                stderr: vec![],
                exit_code: 0,
            });
        }
        Ok(ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
        })
    }
    async fn run_interactive(
        &self,
        _: &ContainerId,
        spec: ExecSpec,
        _: AttachIo,
    ) -> Result<i32, RuntimeError> {
        self.interactive_commands.lock().unwrap().push(spec.cmd);
        if let Some(entered) = &self.interactive_entered {
            let _ = entered.send(());
            return std::future::pending().await;
        }
        Ok(self.tool_exit_code.load(Ordering::SeqCst))
    }
    async fn stop(&self, _: &ContainerId, _: Duration) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn remove(&self, _: &ContainerId, _: bool) -> Result<(), RuntimeError> {
        self.removed_containers.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn list_owned(&self) -> Result<Vec<OwnedObject>, RuntimeError> {
        Ok(vec![])
    }
}

struct MockGit;

#[async_trait]
impl GitEngine for MockGit {
    async fn ingress(
        &self,
        _: &IngressPlan,
        _: &dyn RuntimeAdapter,
        _: &mut (dyn FnMut(IngressProgress) + Send),
    ) -> Result<IngressReport, capscode_core::GitError> {
        Ok(IngressReport {
            copied_bytes: 1,
            copied_files: 1,
            elapsed: Duration::ZERO,
            wip_commit: None,
            receipt: IngressReceipt::GitSnapshot { baseline: None },
            phases: Vec::new(),
        })
    }
    async fn egress(
        &self,
        _: &EgressPlan,
        _: &dyn RuntimeAdapter,
    ) -> Result<EgressReport, capscode_core::GitError> {
        unreachable!()
    }
    fn apply_as_branch(
        &self,
        _: &Path,
        _: &SessionId,
        _: &str,
    ) -> Result<String, capscode_core::GitError> {
        unreachable!()
    }
    fn apply_to_worktree(
        &self,
        _: &Path,
        _: &SessionId,
        _: git2::Oid,
    ) -> Result<(), capscode_core::GitError> {
        unreachable!()
    }
    fn discard(&self, _: &Path, _: &SessionId) -> Result<(), capscode_core::GitError> {
        unreachable!()
    }
}

struct MockNetwork;

#[async_trait]
impl NetworkStack for MockNetwork {
    fn gateway_spec(&self, ctx: &NetContext) -> Result<ContainerSpec, NetError> {
        Ok(ContainerSpec {
            image: ctx.gateway_image.clone(),
            hostname: "gateway".into(),
            env: vec![],
            cmd: None,
            entrypoint: None,
            mounts: vec![],
            network: NetworkMode::Bridge("bridge".into()),
            published_ports: vec![],
            cap_add: vec!["NET_ADMIN".into()],
            cap_drop: Vec::new(),
            no_new_privileges: false,
            devices: vec![],
            sysctls: vec![],
            labels: vec![(LABEL_ROLE.into(), capscode_core::ROLE_GATEWAY.into())],
            tmpfs_home: false,
            read_only_rootfs: false,
            init: false,
            pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
            memory_bytes: None,
        })
    }
    async fn verify(
        &self,
        _: &ContainerId,
        _: &dyn RuntimeAdapter,
    ) -> Result<NetVerifyReport, NetError> {
        Ok(NetVerifyReport {
            rfc1918_blocked: true,
            host_gateway_blocked: true,
            ..NetVerifyReport::default()
        })
    }
}

#[tokio::test]
async fn provisioning_error_preserves_uncaptured_volume_and_tears_down() {
    let _home_lock = TEST_CONFIG_HOME_LOCK.lock().await;
    let temp_home = tempfile::tempdir().unwrap();
    let _restore_home = RestoreEnv::set("CAPSCODE_HOME", temp_home.path());
    let runtime = Arc::new(MockRuntime::new());
    let options = SessionOptions {
        session: SessionId::from("00000000-0000-0000-0000-000000000001".to_string()),
        config: Config::default(),
        host_repo: PathBuf::from("."),
        tool_command: "claude".into(),
        tool_args: vec![],
        gateway_image: ImageRef("gateway:test".into()),
        capsule_image: ImageRef("capsule:test".into()),
        seeder_image: ImageRef("seeder:test".into()),
        selected_provider: capscode_core::load_bundled_provider_registry()
            .unwrap()
            .into_iter()
            .find(|provider| provider.id == "anthropic")
            .unwrap(),
        selected_credential: None,
        selected_credential_source: None,
        extra_providers: Vec::new(),
        interactive_handoff: true,
        verbose: false,
        r#box: None,
    };
    let mut driver = SessionDriver::new(
        options,
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    let (_tx, rx) = watch::channel(None::<SessionSignal>);
    let result = driver.drive(rx).await;
    assert!(result.is_err());
    assert_eq!(driver.state(), State::Destroyed);
    assert!(!runtime.removed_volume.load(Ordering::SeqCst));
    assert!(temp_home
        .path()
        .join("sessions/00000000-0000-0000-0000-000000000001/host-repo.json")
        .exists());
    assert!(driver
        .transitions()
        .windows(3)
        .any(|states| states == [State::Failed, State::TearingDown, State::Destroyed]));
}

#[tokio::test]
async fn mount_mode_is_persisted_and_skips_ingress_and_wip_commit() {
    let _home_lock = TEST_CONFIG_HOME_LOCK.lock().await;
    let temp_home = tempfile::tempdir().unwrap();
    let _restore_home = RestoreEnv::set("CAPSCODE_HOME", temp_home.path());
    let mut config = Config::default();
    config.workspace.mode = WorkspaceMode::Mount;
    let mut driver = SessionDriver::new(
        options_with_config(config),
        Arc::new(MockRuntime::new()),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.state = State::Ingesting;

    driver.advance().await.unwrap();
    assert_eq!(driver.state(), State::Provisioning);
    assert!(driver.resources.ingress_receipt.is_none());
    assert!(driver.resources.volume.is_none());
    assert_eq!(
        read_session_workspace_mode(&driver.options.session).unwrap(),
        Some(WorkspaceMode::Mount)
    );
    driver.commit_wip().await.unwrap();
}

#[tokio::test]
async fn tool_exit_code_survives_session_teardown() {
    let runtime = Arc::new(MockRuntime::new());
    runtime.keeper_tool_exit_status.store(37, Ordering::SeqCst);
    let mut driver = SessionDriver::new(
        options_with_config(Config::default()),
        runtime,
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.state = State::Running;
    driver.keeper_mode = true;
    driver.box_ready = true;
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    let (_sender, shutdown) = watch::channel(None::<SessionSignal>);

    assert_eq!(driver.drive(shutdown).await.unwrap(), 37);
    assert_eq!(driver.state(), State::Destroyed);
}

#[tokio::test]
async fn box_client_targets_the_keeper_socket() {
    let runtime = Arc::new(MockRuntime::new());
    let mut driver = SessionDriver::new(
        box_options(),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.state = State::Running;
    driver.box_ready = true;
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    let (_sender, shutdown) = watch::channel(None::<SessionSignal>);

    driver.drive(shutdown).await.unwrap();

    assert_eq!(
        runtime.interactive_commands.lock().unwrap().as_slice(),
        &[vec![
            "tmux",
            "-S",
            "/run/capscode-keeper/sock",
            "set-option",
            "-g",
            "prefix",
            "C-b",
            ";",
            "bind-key",
            "C-b",
            "send-prefix",
            ";",
            "new-session",
            "-A",
            "-s",
            "main",
            "/bin/bash",
            "-l",
        ]]
    );
}

#[tokio::test]
async fn box_client_applies_the_configured_prefix() {
    let runtime = Arc::new(MockRuntime::new());
    let mut options = box_options();
    options.config.r#box.prefix = "C-a".to_string();
    let mut driver = SessionDriver::new(
        options,
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.state = State::Running;
    driver.box_ready = true;
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    let (_sender, shutdown) = watch::channel(None::<SessionSignal>);

    driver.drive(shutdown).await.unwrap();

    let commands = runtime.interactive_commands.lock().unwrap();
    assert_eq!(
        &commands[0][3..11],
        &[
            "set-option",
            "-g",
            "prefix",
            "C-a",
            ";",
            "bind-key",
            "C-a",
            "send-prefix"
        ]
    );
}

#[tokio::test]
async fn hangup_tears_down_classic_sessions_and_detaches_ready_devboxes() {
    for keeper_mode in [false, true] {
        let runtime = Arc::new(MockRuntime::new());
        let mut driver = SessionDriver::new(
            options_with_config(Config::default()),
            runtime.clone(),
            Arc::new(MockGit),
            Arc::new(MockNetwork),
        )
        .unwrap();
        driver.state = State::Running;
        driver.keeper_mode = keeper_mode;
        driver.box_ready = keeper_mode;
        driver.resources.capsule = Some(ContainerId("capsule".into()));
        driver.resources.gateway = Some(ContainerId("gateway".into()));
        driver.resources.volume = Some(VolumeId("volume".into()));
        let (_sender, shutdown) = watch::channel(Some(SessionSignal::Hangup));

        assert!(driver.drive(shutdown).await.is_err());
        assert_eq!(driver.state(), State::Destroyed);
        assert_eq!(
            runtime.removed_containers.load(Ordering::SeqCst),
            if keeper_mode { 0 } else { 2 }
        );
        assert!(!runtime.removed_volume.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn in_flight_container_creation_is_finished_and_removed_during_teardown() {
    let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
    let runtime = Arc::new(MockRuntime {
        removed_volume: AtomicBool::new(false),
        image_present: AtomicBool::new(true),
        image_inspections: AtomicUsize::new(0),
        image_pulls: AtomicUsize::new(0),
        create_release: Some(tokio::sync::Mutex::new(Some(release_receiver))),
        fail_gateway: AtomicBool::new(false),
        fail_tool_config: AtomicBool::new(false),
        removed_containers: AtomicUsize::new(0),
        tool_exit_code: AtomicI32::new(0),
        keeper_tool_exit_status: AtomicI32::new(-1),
        interactive_commands: Mutex::new(Vec::new()),
        interactive_entered: None,
        created_labels: Mutex::new(Vec::new()),
        created_volume_labels: Mutex::new(Vec::new()),
    });
    let options = options_with_config(Config::default());
    let spec = capsule_spec(
        &options,
        &ContainerId("gateway".into()),
        &VolumeId("volume".into()),
        "cert",
    )
    .unwrap();
    let mut driver = SessionDriver::new(
        options,
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    let mut create = Box::pin(driver.create_container_tracked(spec, ContainerRole::Gateway));
    tokio::select! {
        _ = &mut create => panic!("container creation unexpectedly completed"),
        _ = tokio::task::yield_now() => {}
    }
    drop(create);
    release_sender.send(()).unwrap();

    driver.destroy().await.unwrap();
    assert_eq!(runtime.removed_containers.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hangup_interrupts_a_blocked_interactive_exec_and_tears_down_classic_session() {
    let (entered_sender, mut entered_receiver) = tokio::sync::mpsc::unbounded_channel();
    let runtime = Arc::new(MockRuntime {
        removed_volume: AtomicBool::new(false),
        image_present: AtomicBool::new(true),
        image_inspections: AtomicUsize::new(0),
        image_pulls: AtomicUsize::new(0),
        create_release: None,
        fail_gateway: AtomicBool::new(false),
        fail_tool_config: AtomicBool::new(false),
        removed_containers: AtomicUsize::new(0),
        tool_exit_code: AtomicI32::new(0),
        keeper_tool_exit_status: AtomicI32::new(-1),
        interactive_commands: Mutex::new(Vec::new()),
        interactive_entered: Some(entered_sender),
        created_labels: Mutex::new(Vec::new()),
        created_volume_labels: Mutex::new(Vec::new()),
    });
    let mut driver = SessionDriver::new(
        options_with_config(Config::default()),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.state = State::Running;
    driver.box_ready = true;
    driver.options.interactive_handoff = false;
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    driver.resources.gateway = Some(ContainerId("gateway".into()));
    let (signal_sender, signal_receiver) = watch::channel(None);
    let task = tokio::spawn(async move { driver.drive(signal_receiver).await });

    entered_receiver.recv().await.expect("exec should start");
    signal_sender
        .send(Some(SessionSignal::Hangup))
        .expect("session should still be listening");
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("hangup should interrupt the blocked attach")
        .unwrap();
    assert!(result.is_err());
    assert_eq!(runtime.removed_containers.load(Ordering::SeqCst), 2);
}

#[test]
fn apply_as_branch_leaves_host_worktree_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(temp.path()).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@example.invalid").unwrap();
    let base = repo
        .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
        .unwrap();
    drop(tree);

    std::fs::write(temp.path().join("tracked.txt"), "capsule\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let parent = repo.find_commit(base).unwrap();
    let fetched = repo
        .commit(None, &sig, &sig, "capsule", &tree, &[&parent])
        .unwrap();
    drop(tree);
    drop(parent);

    std::fs::write(temp.path().join("tracked.txt"), "dirty host worktree\n").unwrap();
    let session = SessionId::from("00000000-0000-0000-0000-000000000002".to_string());
    repo.reference(
        &format!("refs/capscode/{}/head", session.as_str()),
        fetched,
        true,
        "test",
    )
    .unwrap();
    let before = std::fs::read(temp.path().join("tracked.txt")).unwrap();
    let statuses_before = repo.statuses(None).unwrap().len();

    let engine = capscode_git::DefaultGitEngine::new();
    engine
        .apply_as_branch(temp.path(), &session, "safe")
        .unwrap();

    assert_eq!(
        std::fs::read(temp.path().join("tracked.txt")).unwrap(),
        before
    );
    assert_eq!(repo.statuses(None).unwrap().len(), statuses_before);
    assert_eq!(
        repo.find_reference("refs/heads/capscode/safe")
            .unwrap()
            .target(),
        Some(fetched)
    );
}

#[test]
fn only_box_capsules_get_the_kill_capability() {
    assert!(!capsule_cap_add(false).iter().any(|cap| cap == "KILL"));
    let with_keeper = capsule_cap_add(true);
    assert!(with_keeper.iter().any(|cap| cap == "KILL"));
    assert_eq!(with_keeper.len(), capsule_cap_add(false).len() + 1);
}

#[test]
fn an_empty_exit_round_keeps_handoff_data_an_earlier_sync_left() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!keep_state_after_exit_round(
        ShadowOutcome::NoChanges,
        dir.path()
    ));
    assert!(keep_state_after_exit_round(
        ShadowOutcome::Staged,
        dir.path()
    ));
    std::fs::create_dir_all(dir.path().join("backup")).unwrap();
    std::fs::write(
        dir.path().join("backup/notes.md"),
        "overwritten host file\n",
    )
    .unwrap();
    assert!(keep_state_after_exit_round(
        ShadowOutcome::NoChanges,
        dir.path()
    ));
}

#[tokio::test]
async fn an_exit_after_another_command_removed_the_session_hands_nothing_off() {
    let runtime = Arc::new(MockRuntime::new());
    let mut driver = SessionDriver::new(
        options_with_config(Config::default()),
        runtime.clone(),
        Arc::new(MockGit),
        Arc::new(MockNetwork),
    )
    .unwrap();
    driver.state = State::Draining;
    // The mock runtime lists nothing: a `capscode rm` removed every object while this exit
    // waited for the workspace lock.
    driver.resources.capsule = Some(ContainerId("capsule".into()));
    driver.resources.gateway = Some(ContainerId("gateway".into()));
    driver.resources.volume = Some(VolumeId("volume".into()));
    driver.resources.ingress_receipt = Some(IngressReceipt::Shadow {
        git_dir: PathBuf::from("shadow.git"),
        baseline: git2::Oid::ZERO_SHA1.to_string(),
        state_dir: PathBuf::from("state-that-does-not-exist"),
    });

    driver.advance().await.unwrap();
    assert_eq!(driver.state, State::TearingDown);
    assert!(driver.resources.capsule.is_none());
    assert!(driver.resources.gateway.is_none());
    assert!(driver.resources.volume.is_none());
    driver.advance().await.unwrap();
    assert_eq!(driver.state, State::Destroyed);
    assert!(!runtime.removed_volume.load(Ordering::SeqCst));
}
