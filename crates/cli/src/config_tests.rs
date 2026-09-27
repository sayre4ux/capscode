use super::*;
use capscode_core::AuthKind;
use secrecy::ExposeSecret;
use std::cell::Cell;

static CONFIG_ENV_LOCK: Mutex<()> = Mutex::new(());

fn restore_env(name: &str, old: Option<std::ffi::OsString>) {
    match old {
        Some(value) => env::set_var(name, value),
        None => env::remove_var(name),
    }
}

fn jwt_with_exp(exp: u64) -> String {
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
    format!("header.{payload}.signature")
}

fn write_codex_auth(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn resolve_from(
    pairs: &[(&str, &str)],
    keychain: std::result::Result<&str, &'static str>,
) -> ResolvedProviderKeys {
    let mut map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    map.entry("USER".to_string())
        .or_insert_with(|| "test-user".to_string());
    provider_keys_from(
        |name| map.get(name).cloned(),
        |service, account| {
            assert_eq!(service, DEFAULT_KEYCHAIN_SERVICE);
            assert_eq!(account, "test-user");
            keychain.map(str::to_string)
        },
    )
}

#[test]
fn repeated_keychain_resolution_reads_once_per_process() {
    let cache = Mutex::new(None);
    let reads = Cell::new(0);
    for _ in 0..6 {
        let value = cached_keychain_read(&cache, || {
            reads.set(reads.get() + 1);
            Ok(Some(SecretString::from(
                "sk-ant-oat01-KEYCHAIN".to_string(),
            )))
        })
        .unwrap();
        assert_eq!(
            value.as_ref().map(|value| value.expose_secret()),
            Some("sk-ant-oat01-KEYCHAIN")
        );
    }
    assert_eq!(reads.get(), 1);
}

#[test]
fn box_candidates_prefer_environment_then_store_then_anthropic_keychain() {
    let registry = providers::load_registry().unwrap();
    let env_values = HashMap::from([("OPENAI_API_KEY".to_string(), "env-openai-key".to_string())]);
    let store_values = HashMap::from([("kimi".to_string(), "stored-kimi-key".to_string())]);
    let anthropic_keychain =
        ProviderCredential::oauth(SecretString::from("keychain-anthropic-token"));
    let candidates = box_candidates_from(
        registry,
        |name| env_values.get(name).cloned(),
        |id| store_values.get(id).cloned(),
        Some(anthropic_keychain),
        Some(ProviderCredential::codex_access_token(SecretString::from(
            "codex-login-token",
        ))),
    );
    let get = |id: &str| {
        candidates
            .iter()
            .find(|(provider, _)| provider.id == id)
            .map(|(_, credential)| credential)
    };
    assert_eq!(
        get("openai").unwrap().secret.expose_secret(),
        "env-openai-key"
    );
    assert_eq!(
        get("kimi").unwrap().secret.expose_secret(),
        "stored-kimi-key"
    );
    assert_eq!(
        get("anthropic").unwrap().secret.expose_secret(),
        "keychain-anthropic-token"
    );
    assert_eq!(get("anthropic").unwrap().kind, AuthKind::OAuth);
}

#[test]
fn box_candidates_use_the_codex_chatgpt_login_when_openai_has_no_api_key() {
    let registry = providers::load_registry().unwrap();
    let candidates = box_candidates_from(
        registry,
        |_| None,
        |_| None,
        None,
        Some(ProviderCredential::codex_access_token(SecretString::from(
            "codex-login-token",
        ))),
    );
    let (_, openai) = candidates
        .iter()
        .find(|(provider, _)| provider.id == "openai")
        .expect("the ChatGPT login makes openai routable");
    assert_eq!(openai.secret.expose_secret(), "codex-login-token");
    assert_eq!(openai.kind, AuthKind::CodexAccessToken);
    assert_eq!(candidates.len(), 1);
}

#[test]
fn box_tool_defaults_use_persisted_choice_then_built_in_default() {
    let registry = providers::load_registry().unwrap();
    let document: toml::Value = toml::from_str("[defaults]\npi = 'openai'\n").unwrap();
    let defaults = tool_defaults_from(&document, &registry).unwrap();
    assert_eq!(
        defaults.get(&Tool::Claude).map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        defaults.get(&Tool::Codex).map(String::as_str),
        Some("openai")
    );
    assert_eq!(defaults.get(&Tool::Pi).map(String::as_str), Some("openai"));
    assert!(!defaults.contains_key(&Tool::Omp));
}

#[test]
fn failed_keychain_resolution_is_also_cached() {
    let cache = Mutex::new(None);
    let reads = Cell::new(0);
    for _ in 0..4 {
        let result = cached_keychain_read(&cache, || {
            reads.set(reads.get() + 1);
            Err("access denied".to_string())
        });
        assert_eq!(result.unwrap_err(), "access denied");
    }
    assert_eq!(reads.get(), 1);
}

#[test]
fn codex_chatgpt_auth_reads_only_access_token_and_checks_expiry() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("auth.json");
    let token = jwt_with_exp(10_000);
    write_codex_auth(
        &path,
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": token,
                "refresh_token": "must-not-be-deserialized",
                "id_token": "must-not-be-deserialized"
            }
        }),
    );

    let resolved = read_codex_auth_at(&path, 1_000).unwrap().unwrap();
    assert_eq!(resolved.source, "codex-auth:chatgpt");
    assert_eq!(resolved.credential.kind, AuthKind::CodexAccessToken);
    assert_eq!(resolved.credential.secret.expose_secret(), token);
    let rendered = format!("{} {:?}", resolved.source, resolved.credential.kind);
    assert!(!rendered.contains("must-not-be-deserialized"));
    assert!(!rendered.contains(&token));
}

#[test]
fn codex_chatgpt_auth_rejects_token_with_less_than_one_hour_left() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("auth.json");
    write_codex_auth(
        &path,
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": jwt_with_exp(4_600) }
        }),
    );

    let error = match read_codex_auth_at(&path, 1_000) {
        Ok(_) => panic!("near-expiry token should be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("expires within one hour"));
}

#[test]
fn codex_api_key_auth_file_remains_supported() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("auth.json");
    write_codex_auth(
        &path,
        serde_json::json!({
            "auth_mode": "apikey",
            "OPENAI_API_KEY": "sk-openai-auth-file"
        }),
    );

    let resolved = read_codex_auth_at(&path, 1_000).unwrap().unwrap();
    assert_eq!(resolved.source, "codex-auth:api-key");
    assert_eq!(resolved.credential.kind, AuthKind::ApiKey);
    assert_eq!(
        resolved.credential.secret.expose_secret(),
        "sk-openai-auth-file"
    );
}

#[cfg(unix)]
#[test]
fn codex_auth_rejects_permissions_broader_than_user_only() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("auth.json");
    write_codex_auth(
        &path,
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": jwt_with_exp(10_000) }
        }),
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

    let error = match read_codex_auth_at(&path, 1_000) {
        Ok(_) => panic!("overly broad permissions should be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("chmod 600"));
}

#[test]
fn env_oauth_wins_over_keychain() {
    let resolved = resolve_from(
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-api03-KEY"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-TOKEN"),
            ("OPENAI_API_KEY", "sk-openai-KEY"),
        ],
        Ok("sk-ant-oat01-KEYCHAIN"),
    );

    let anthropic = resolved
        .keys
        .0
        .get(&Provider::Anthropic)
        .expect("anthropic credential");
    assert_eq!(anthropic.kind, AuthKind::OAuth);
    assert_eq!(anthropic.secret.expose_secret(), "sk-ant-oat01-TOKEN");
    assert_eq!(resolved.anthropic.source(), "env:CLAUDE_CODE_OAUTH_TOKEN");

    // OpenAI is unaffected and stays an API key.
    let openai = resolved
        .keys
        .0
        .get(&Provider::OpenAI)
        .expect("openai credential");
    assert_eq!(openai.kind, AuthKind::ApiKey);
    assert_eq!(openai.secret.expose_secret(), "sk-openai-KEY");
}

#[test]
fn env_api_key_wins_over_keychain() {
    let resolved = resolve_from(
        &[("ANTHROPIC_API_KEY", "sk-ant-api03-KEY")],
        Ok("sk-ant-oat01-KEYCHAIN"),
    );
    let anthropic = resolved
        .keys
        .0
        .get(&Provider::Anthropic)
        .expect("anthropic credential");
    assert_eq!(anthropic.kind, AuthKind::ApiKey);
    assert_eq!(anthropic.secret.expose_secret(), "sk-ant-api03-KEY");
    assert_eq!(resolved.anthropic.source(), "env:ANTHROPIC_API_KEY");
}

#[test]
fn keychain_oauth_is_used_without_environment_credential() {
    let resolved = resolve_from(&[], Ok("sk-ant-oat01-KEYCHAIN"));
    let anthropic = resolved
        .keys
        .0
        .get(&Provider::Anthropic)
        .expect("anthropic credential");
    assert_eq!(anthropic.kind, AuthKind::OAuth);
    assert_eq!(anthropic.secret.expose_secret(), "sk-ant-oat01-KEYCHAIN");
    assert_eq!(resolved.anthropic.source(), "keychain:claude-setup-token");
    assert_eq!(resolved.anthropic.kind(), Some(AuthKind::OAuth));
}

#[test]
fn keychain_service_override_is_honored() {
    // CAPSCODE_KEYCHAIN_SERVICE replaces the default service the keychain is
    // read under, without changing the default for anyone who does not set it.
    let map: HashMap<String, String> = [
        ("USER", "test-user"),
        ("CAPSCODE_KEYCHAIN_SERVICE", "capscode-selftest"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let seen_service = std::cell::RefCell::new(String::new());
    let resolved = provider_keys_from(
        |name| map.get(name).cloned(),
        |service, account| {
            *seen_service.borrow_mut() = service.to_string();
            assert_eq!(account, "test-user");
            Ok::<_, &'static str>("sk-ant-oat01-OVERRIDE".to_string())
        },
    );
    assert_eq!(seen_service.into_inner(), "capscode-selftest");
    assert_eq!(resolved.anthropic.source(), "keychain:capscode-selftest");
    assert_eq!(
        resolved
            .keys
            .0
            .get(&Provider::Anthropic)
            .expect("anthropic credential")
            .secret
            .expose_secret(),
        "sk-ant-oat01-OVERRIDE"
    );
}

#[test]
fn keychain_error_falls_through_to_none() {
    let resolved = resolve_from(&[], Err("Secret Service unavailable"));
    assert!(!resolved.keys.0.contains_key(&Provider::Anthropic));
    assert_eq!(resolved.anthropic.source(), "none");
    assert_eq!(resolved.anthropic.kind(), None);
    // status/doctor lines consult the process-wide keychain opt-in flag.
    // Default (and fail-safe on missing config) is disabled, so the
    // discoverability hint is expected.
    let status = resolved.anthropic.status_line();
    let doctor = resolved.anthropic.doctor_line();
    assert!(
        status.contains("keychain lookup disabled") || status == "Anthropic credential: none",
        "unexpected status_line: {status}"
    );
    assert!(
        doctor.contains("keychain lookup disabled")
            || doctor == "Anthropic credential: not configured",
        "unexpected doctor_line: {doctor}"
    );
}

#[test]
fn keychain_gate_skips_probe_when_disabled() {
    let probed = Cell::new(false);
    let result = keychain_gate(false, || {
        probed.set(true);
        Some("sk-ant-oat01-SHOULD-NOT-APPEAR".to_string())
    });
    assert!(result.is_none());
    assert!(
        !probed.get(),
        "probe must not run when keychain is disabled"
    );
}

#[test]
fn keychain_gate_runs_probe_when_enabled() {
    let probed = Cell::new(false);
    let result = keychain_gate(true, || {
        probed.set(true);
        Some("sk-ant-oat01-KEYCHAIN".to_string())
    });
    assert_eq!(result.as_deref(), Some("sk-ant-oat01-KEYCHAIN"));
    assert!(probed.get());
}

#[test]
fn provider_keystore_value_is_gated_by_auth_keychain() {
    let mut keys = std::collections::BTreeMap::new();
    keys.insert(
        "openai".to_string(),
        crate::keystore::BundleKey {
            value: "sk-keystore-test".to_string(),
            kind: "api_key".to_string(),
            stored_at: 1,
        },
    );
    let state = crate::keystore::KeystoreState::Ok(crate::keystore::Bundle {
        version: 1,
        revision: 1,
        keys,
    });

    assert_eq!(
        provider_keystore_value(false, state.clone(), "openai"),
        None
    );
    assert_eq!(
        provider_keystore_value(true, state, "openai").as_deref(),
        Some("sk-keystore-test")
    );
}

#[test]
fn keychain_enabled_flag_round_trips() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    use figment::providers::Format;

    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        &config_path,
        "runtime = \"docker\"\n\n[network]\ntelemetry_sinkhole = false\n",
    )
    .unwrap();

    set_keychain_enabled_at(&config_path, true).unwrap();
    let enabled: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(enabled["runtime"].as_str(), Some("docker"));
    assert_eq!(
        enabled["network"]["telemetry_sinkhole"].as_bool(),
        Some(false)
    );
    assert_eq!(enabled["auth"]["keychain"].as_bool(), Some(true));

    // Round-trip through figment/Config so the typed field is what load sees.
    let cfg: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert!(cfg.auth.keychain);

    set_keychain_enabled_at(&config_path, false).unwrap();
    let disabled: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(disabled["auth"]["keychain"].as_bool(), Some(false));
    assert_eq!(disabled["runtime"].as_str(), Some("docker"));
    assert_eq!(
        disabled["network"]["telemetry_sinkhole"].as_bool(),
        Some(false)
    );
}

#[test]
fn workspace_mode_config_key_round_trips_set_get_unset() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    use figment::providers::Format;

    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "runtime = \"docker\"\n").unwrap();

    set_workspace_mode_at(&config_path, WorkspaceMode::Mount).unwrap();
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(document["workspace"]["mode"].as_str(), Some("mount"));
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::WorkspaceMode).unwrap(),
        ConfigScalar::String("mount".to_string())
    );

    let cfg: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert_eq!(cfg.workspace.mode, WorkspaceMode::Mount);

    unset_workspace_mode_at(&config_path).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared.get("workspace").is_none());
    assert_eq!(cleared["runtime"].as_str(), Some("docker"));

    let rows = config_rows_at(&config_path).unwrap();
    let row = rows
        .iter()
        .find(|row| row.key == ConfigKeyArg::WorkspaceMode)
        .unwrap();
    assert_eq!(row.persisted, None);
    assert_eq!(
        row.effective,
        Some(ConfigScalar::String("copy".to_string()))
    );
    assert_eq!(row.source, "built-in");
}

#[test]
fn box_prefix_config_key_round_trips_set_get_unset() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    use figment::providers::Format;

    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "runtime = \"docker\"\n").unwrap();

    set_box_prefix_at(&config_path, "C-a").unwrap();
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(document["box"]["prefix"].as_str(), Some("C-a"));
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::BoxPrefix).unwrap(),
        ConfigScalar::String("C-a".to_string())
    );
    let cfg: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert_eq!(cfg.r#box.prefix, "C-a");

    assert!(set_box_prefix_at(&config_path, "C-;").is_err());

    unset_box_prefix_at(&config_path).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared.get("box").is_none());
    assert_eq!(cleared["runtime"].as_str(), Some("docker"));

    let rows = config_rows_at(&config_path).unwrap();
    let row = rows
        .iter()
        .find(|row| row.key == ConfigKeyArg::BoxPrefix)
        .unwrap();
    assert_eq!(row.persisted, None);
    assert_eq!(row.effective, Some(ConfigScalar::String("C-b".to_string())));
    assert_eq!(row.source, "built-in");
}

#[test]
fn capsule_limit_config_keys_round_trip_set_get_unset_list() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    use figment::providers::Format;
    let old_memory = env::var_os("CAPSCODE_CAPSULE__MEMORY");
    let old_pids = env::var_os("CAPSCODE_CAPSULE__PIDS");
    env::remove_var("CAPSCODE_CAPSULE__MEMORY");
    env::remove_var("CAPSCODE_CAPSULE__PIDS");

    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "runtime = \"docker\"\n").unwrap();

    // Unset: no memory limit, pids at the built-in cap.
    let rows = config_rows_at(&config_path).unwrap();
    let row = |rows: &[ConfigRow], key| rows.iter().find(|row| row.key == key).unwrap().clone();
    let memory = row(&rows, ConfigKeyArg::CapsuleMemory);
    assert_eq!((memory.persisted, memory.effective), (None, None));
    assert_eq!(memory.source, "built-in");
    let pids = row(&rows, ConfigKeyArg::CapsulePids);
    assert_eq!(pids.effective, Some(ConfigScalar::Integer(1024)));
    assert_eq!(pids.source, "built-in");

    set_capsule_limit_at(&config_path, ConfigKeyArg::CapsuleMemory, "4096M").unwrap();
    set_capsule_limit_at(&config_path, ConfigKeyArg::CapsulePids, "2048").unwrap();
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(document["capsule"]["memory"].as_str(), Some("4g"));
    assert_eq!(document["capsule"]["pids"].as_integer(), Some(2048));
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::CapsuleMemory).unwrap(),
        ConfigScalar::String("4g".to_string())
    );
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::CapsulePids).unwrap(),
        ConfigScalar::Integer(2048)
    );
    let cfg: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert_eq!(cfg.capsule.memory_bytes(), Some(4 << 30));
    assert_eq!(cfg.capsule.pids_limit(), 2048);
    let rows = config_rows_at(&config_path).unwrap();
    assert_eq!(
        row(&rows, ConfigKeyArg::CapsuleMemory).effective,
        Some(ConfigScalar::String("4g".to_string()))
    );
    assert_eq!(row(&rows, ConfigKeyArg::CapsulePids).source, "file");
    let table = format_config_table(&rows);
    let cells = |key: &str| {
        table
            .lines()
            .find(|line| line.starts_with(key))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
    };
    assert_eq!(
        cells("capsule.pids"),
        Some(vec!["capsule.pids", "2048", "2048", "file"])
    );
    assert_eq!(
        cells("capsule.memory"),
        Some(vec!["capsule.memory", "4g", "4g", "file"])
    );

    // Invalid values are refused and leave the file alone.
    for (key, bad) in [
        (ConfigKeyArg::CapsuleMemory, "lots"),
        (ConfigKeyArg::CapsuleMemory, "1m"),
        (ConfigKeyArg::CapsulePids, "0"),
        (ConfigKeyArg::CapsulePids, "-1"),
    ] {
        assert!(
            set_capsule_limit_at(&config_path, key, bad).is_err(),
            "{bad}"
        );
    }
    let unchanged: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(unchanged, document);

    unset_capsule_limit_at(&config_path, ConfigKeyArg::CapsuleMemory).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared["capsule"].get("memory").is_none());
    assert_eq!(cleared["capsule"]["pids"].as_integer(), Some(2048));
    unset_capsule_limit_at(&config_path, ConfigKeyArg::CapsulePids).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared.get("capsule").is_none());
    assert_eq!(cleared["runtime"].as_str(), Some("docker"));

    // A hand-edited byte count is a valid memory value.
    fs::write(&config_path, "[capsule]\nmemory = 1073741824\n").unwrap();
    let document = read_persisted_document_at(&config_path).unwrap();
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::CapsuleMemory).unwrap(),
        ConfigScalar::Integer(1 << 30)
    );
    let rows = config_rows_at(&config_path).unwrap();
    assert_eq!(
        row(&rows, ConfigKeyArg::CapsuleMemory).effective,
        Some(ConfigScalar::String("1g".to_string()))
    );

    // A hand-edited invalid value surfaces as a row error, not a silent default.
    fs::write(&config_path, "[capsule]\npids = 0\nmemory = \"lots\"\n").unwrap();
    let rows = config_rows_at(&config_path).unwrap();
    assert!(row(&rows, ConfigKeyArg::CapsulePids).error.is_some());
    assert!(row(&rows, ConfigKeyArg::CapsuleMemory).error.is_some());

    restore_env("CAPSCODE_CAPSULE__MEMORY", old_memory);
    restore_env("CAPSCODE_CAPSULE__PIDS", old_pids);
}

#[test]
fn capsule_opencode_lsp_download_config_key_round_trips_set_get_unset_list() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    use figment::providers::Format;

    let env_name = "CAPSCODE_CAPSULE__OPENCODE_LSP_DOWNLOAD";
    let old_value = env::var_os(env_name);
    env::remove_var(env_name);
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "runtime = \"docker\"\n").unwrap();

    let row = |rows: &[ConfigRow]| {
        rows.iter()
            .find(|row| row.key == ConfigKeyArg::CapsuleOpencodeLspDownload)
            .unwrap()
            .clone()
    };
    let initial = row(&config_rows_at(&config_path).unwrap());
    assert_eq!(initial.persisted, None);
    assert_eq!(initial.effective, Some(ConfigScalar::Bool(true)));
    assert_eq!(initial.source, "built-in");

    set_capsule_opencode_lsp_download_at(&config_path, false).unwrap();
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(
        document["capsule"]["opencode_lsp_download"].as_bool(),
        Some(false)
    );
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::CapsuleOpencodeLspDownload).unwrap(),
        ConfigScalar::Bool(false)
    );
    let config: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert!(!config.capsule.opencode_lsp_download);
    let configured = row(&config_rows_at(&config_path).unwrap());
    assert_eq!(configured.effective, Some(ConfigScalar::Bool(false)));
    assert_eq!(configured.source, "file");
    assert!(parse_opencode_lsp_download("yes").is_err());

    unset_capsule_opencode_lsp_download_at(&config_path).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared.get("capsule").is_none());
    let restored = row(&config_rows_at(&config_path).unwrap());
    assert_eq!(restored.effective, Some(ConfigScalar::Bool(true)));
    assert_eq!(restored.source, "built-in");

    assert!(validate_config_document("[capsule]\nopencode_lsp_download = false\n").is_ok());
    assert!(validate_config_document("[capsule]\nopencode_lsp_download = \"false\"\n").is_err());

    restore_env(env_name, old_value);
}

#[test]
fn capsule_cache_volume_config_key_round_trips_set_get_unset_list() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    use figment::providers::Format;

    let env_name = "CAPSCODE_CAPSULE__CACHE_VOLUME";
    let old_value = env::var_os(env_name);
    env::remove_var(env_name);
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "runtime = \"docker\"\n").unwrap();

    let row = |rows: &[ConfigRow]| {
        rows.iter()
            .find(|row| row.key == ConfigKeyArg::CapsuleCacheVolume)
            .unwrap()
            .clone()
    };
    let initial = row(&config_rows_at(&config_path).unwrap());
    assert_eq!(initial.persisted, None);
    assert_eq!(initial.effective, Some(ConfigScalar::Bool(true)));
    assert_eq!(initial.source, "built-in");

    set_capsule_cache_volume_at(&config_path, false).unwrap();
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(document["capsule"]["cache_volume"].as_bool(), Some(false));
    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::CapsuleCacheVolume).unwrap(),
        ConfigScalar::Bool(false)
    );
    let config: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert!(!config.capsule.cache_volume);
    let configured = row(&config_rows_at(&config_path).unwrap());
    assert_eq!(configured.effective, Some(ConfigScalar::Bool(false)));
    assert_eq!(configured.source, "file");
    assert!(parse_cache_volume("yes").is_err());

    unset_capsule_cache_volume_at(&config_path).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared.get("capsule").is_none());
    let restored = row(&config_rows_at(&config_path).unwrap());
    assert_eq!(restored.effective, Some(ConfigScalar::Bool(true)));
    assert_eq!(restored.source, "built-in");

    assert!(validate_config_document("[capsule]\ncache_volume = false\n").is_ok());
    assert!(validate_config_document("[capsule]\ncache_volume = \"false\"\n").is_err());

    restore_env(env_name, old_value);
}

#[test]
fn edited_config_validates_capsule_limits() {
    assert!(validate_config_document("[capsule]\nmemory = \"512m\"\npids = 4096\n").is_ok());
    for bad in [
        "[capsule]\nmemory = \"half\"\n",
        "[capsule]\npids = 0\n",
        "[capsule]\nswap = \"1g\"\n",
    ] {
        assert!(validate_config_document(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn box_prefix_accepts_tmux_key_names() {
    for key in [
        "C-b", "C-a", "C-q", "M-a", "C-M-x", "S-F1", "F12", "^a", "`", "C-Space", "C-]",
    ] {
        assert_eq!(parse_box_prefix(key).unwrap(), key, "{key}");
    }
}

#[test]
fn box_prefix_rejects_non_keys() {
    for key in [
        "", " ", "C-", "C-bb", "ctrl+b", "C-b ", ";", "C-;", "None", "C-b;", "\u{e9}",
    ] {
        assert!(parse_box_prefix(key).is_err(), "{key:?}");
    }
    let err = parse_box_prefix("ctrl+b").unwrap_err().to_string();
    assert!(err.contains("C-b"));
}

#[test]
fn workspace_mode_rejects_invalid_values() {
    assert!(parse_workspace_mode("bind").is_err());
    assert!(parse_workspace_mode("true").is_err());
    assert!(parse_workspace_mode("").is_err());
    let err = parse_workspace_mode("rsync").unwrap_err().to_string();
    assert!(err.contains("copy"));
    assert!(err.contains("mount"));
}

#[test]
fn workspace_mode_flags_beat_config_both_ways() {
    assert_eq!(
        resolve_workspace_mode(true, false, WorkspaceMode::Copy).unwrap(),
        WorkspaceMode::Mount
    );
    assert_eq!(
        resolve_workspace_mode(false, true, WorkspaceMode::Mount).unwrap(),
        WorkspaceMode::Copy
    );
    assert_eq!(
        resolve_workspace_mode(false, false, WorkspaceMode::Mount).unwrap(),
        WorkspaceMode::Mount
    );
    assert_eq!(
        resolve_workspace_mode(false, false, WorkspaceMode::Copy).unwrap(),
        WorkspaceMode::Copy
    );
}

#[test]
fn workspace_mode_both_flags_together_is_error() {
    let err = resolve_workspace_mode(true, true, WorkspaceMode::Copy)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--mount"));
    assert!(err.contains("--copy"));
}

#[test]
fn env_credentials_still_win_over_keychain_when_keychain_would_succeed() {
    // Precedence must not change: env OAuth and env API key beat keychain.
    let oauth = resolve_from(
        &[("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-ENV")],
        Ok("sk-ant-oat01-KEYCHAIN"),
    );
    assert_eq!(oauth.anthropic.source(), "env:CLAUDE_CODE_OAUTH_TOKEN");
    assert_eq!(
        oauth
            .keys
            .0
            .get(&Provider::Anthropic)
            .unwrap()
            .secret
            .expose_secret(),
        "sk-ant-oat01-ENV"
    );

    let api_key = resolve_from(
        &[("ANTHROPIC_API_KEY", "sk-ant-api03-ENV")],
        Ok("sk-ant-oat01-KEYCHAIN"),
    );
    assert_eq!(api_key.anthropic.source(), "env:ANTHROPIC_API_KEY");
    assert_eq!(
        api_key
            .keys
            .0
            .get(&Provider::Anthropic)
            .unwrap()
            .secret
            .expose_secret(),
        "sk-ant-api03-ENV"
    );
}

#[test]
fn auth_status_strings_identify_each_source_without_secrets() {
    let cases = [
        (
            resolve_from(
                &[("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-ENV")],
                Err("must not be read"),
            ),
            "Anthropic credential: env:CLAUDE_CODE_OAUTH_TOKEN (OAuth token)",
        ),
        (
            resolve_from(
                &[("ANTHROPIC_API_KEY", "sk-ant-api03-ENV")],
                Err("must not be read"),
            ),
            "Anthropic credential: env:ANTHROPIC_API_KEY (API key)",
        ),
        (
            resolve_from(&[], Ok("sk-ant-oat01-KEYCHAIN")),
            "Anthropic credential: keychain:claude-setup-token (OAuth token)",
        ),
    ];
    for (resolved, expected) in cases {
        assert_eq!(resolved.anthropic.status_line(), expected);
        let rendered = format!("{resolved:?} {}", resolved.anthropic.status_line());
        assert!(!rendered.contains("sk-ant-"));
    }
}

#[test]
fn wireguard_import_and_remove_preserve_unrelated_config() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        &config_path,
        "runtime = \"docker\"\n\n[network]\ntelemetry_sinkhole = false\n",
    )
    .unwrap();
    let wg_path = home.path().join("provider.conf");
    fs::write(
            &wg_path,
            "# provider export\n[Interface]\nPrivateKey= AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nAddress=10.99.0.2/32\nDNS=10.99.0.1\n[Peer]\nPublicKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint=VPN.Example.COM:51820\nAllowedIPs=0.0.0.0/0\n",
        )
        .unwrap();
    let preexisting_path = config_path.with_file_name("wireguard.conf");
    fs::write(&preexisting_path, "user-owned file\n").unwrap();

    set_wireguard_conf_at(&config_path, Some(&wg_path)).unwrap();
    let imported: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let stored_wg_path = PathBuf::from(imported["network"]["wireguard_conf"].as_str().unwrap());
    assert_eq!(imported["runtime"].as_str(), Some("docker"));
    assert_eq!(
        imported["network"]["telemetry_sinkhole"].as_bool(),
        Some(false)
    );
    assert_ne!(stored_wg_path, preexisting_path);
    assert!(imported["network"]["wireguard_conf_sha256"]
        .as_str()
        .is_some());
    let stored_text = fs::read_to_string(&stored_wg_path).unwrap();
    assert!(stored_text.starts_with("[Interface]\nPrivateKey = "));
    assert!(stored_text.contains("Endpoint = vpn.example.com:51820\n"));
    assert!(!stored_text.contains("# provider export"));
    assert_eq!(
        fs::read_to_string(&preexisting_path).unwrap(),
        "user-owned file\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&stored_wg_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    set_wireguard_conf_at(&config_path, None).unwrap();
    let removed: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(removed["runtime"].as_str(), Some("docker"));
    assert_eq!(
        removed["network"]["telemetry_sinkhole"].as_bool(),
        Some(false)
    );
    assert!(removed["network"].get("wireguard_conf").is_none());
    assert!(removed["network"].get("wireguard_conf_sha256").is_none());
    assert!(
        wg_path.exists(),
        "the user's source file must remain untouched"
    );
    assert!(
        !stored_wg_path.exists(),
        "the managed canonical copy is removed"
    );
    assert_eq!(
        fs::read_to_string(&preexisting_path).unwrap(),
        "user-owned file\n"
    );
}

#[test]
fn wireguard_remove_preserves_untracked_legacy_config_file() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let legacy_path = config_path.with_file_name("wireguard.conf");
    fs::write(&legacy_path, "legacy user-owned WireGuard file\n").unwrap();
    fs::write(
        &config_path,
        format!(
            "[network]\nwireguard_conf = {:?}\nenforce_vpn = true\n",
            legacy_path.to_str().unwrap()
        ),
    )
    .unwrap();

    set_wireguard_conf_at(&config_path, None).unwrap();

    assert_eq!(
        fs::read_to_string(&legacy_path).unwrap(),
        "legacy user-owned WireGuard file\n"
    );
    let removed: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(removed["network"].get("wireguard_conf").is_none());
}

#[test]
fn wireguard_remove_preserves_a_managed_file_that_was_modified() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let source = home.path().join("provider.conf");
    fs::write(
            &source,
            "[Interface]\nPrivateKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS=1.1.1.1\n[Peer]\nPublicKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint=vpn.example.com:51820\nAllowedIPs=0.0.0.0/0\n",
        )
        .unwrap();

    set_wireguard_conf_at(&config_path, Some(&source)).unwrap();
    let imported: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let stored_path = PathBuf::from(imported["network"]["wireguard_conf"].as_str().unwrap());
    fs::write(&stored_path, "user-edited WireGuard file\n").unwrap();

    set_wireguard_conf_at(&config_path, None).unwrap();

    assert_eq!(
        fs::read_to_string(&stored_path).unwrap(),
        "user-edited WireGuard file\n"
    );
}

#[test]
fn wireguard_import_rejects_hooks_before_storing_a_config() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let source = home.path().join("provider.conf");
    fs::write(
            &source,
            "[Interface]\nPrivateKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS=1.1.1.1\nPostUp=touch /tmp/should-not-run\n[Peer]\nPublicKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint=vpn.example.com:51820\nAllowedIPs=0.0.0.0/0\n",
        )
        .unwrap();

    let error = set_wireguard_conf_at(&config_path, Some(&source)).unwrap_err();
    assert!(format!("{error:#}").contains("PostUp"));
    assert!(!config_path.exists());
    assert!(!config_path.with_file_name("wireguard.conf").exists());
}

#[test]
fn wireguard_import_does_not_store_a_copy_when_network_config_is_invalid() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "network = \"not-a-table\"\n").unwrap();
    let source = home.path().join("provider.conf");
    fs::write(
            &source,
            "[Interface]\nPrivateKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS=1.1.1.1\n[Peer]\nPublicKey=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint=vpn.example.com:51820\nAllowedIPs=0.0.0.0/0\n",
        )
        .unwrap();

    let error = set_wireguard_conf_at(&config_path, Some(&source)).unwrap_err();
    assert!(format!("{error:#}").contains("network"));
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        "network = \"not-a-table\"\n"
    );
    assert!(!config_path.with_file_name("wireguard.conf").exists());
}

#[test]
fn provider_default_round_trips_and_drives_resolution() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");

    use figment::providers::Format;
    set_provider_default_at(&config_path, Tool::Claude, Some("deepseek")).unwrap();
    let cfg: Config = Config::figment_with_file(figment::providers::Toml::file(&config_path))
        .extract()
        .unwrap();
    assert_eq!(cfg.defaults.get(Tool::Claude), Some("deepseek"));

    let registry = capscode_core::load_bundled_provider_registry().unwrap();
    let resolved = providers::resolve_provider_from(
        &registry,
        None,
        Tool::Claude,
        &cfg.defaults,
        |_| None,
        || None,
        |_| None,
    )
    .unwrap();
    assert_eq!(resolved.spec.id, "deepseek");

    set_provider_default_at(&config_path, Tool::Claude, None).unwrap();
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(document
        .get("defaults")
        .is_none_or(|defaults| defaults.get("claude").is_none()));
}

#[test]
fn picker_requires_no_explicit_provider_no_default_and_a_tty() {
    assert!(should_show_provider_picker(false, false, true));
    assert!(!should_show_provider_picker(true, false, true));
    assert!(!should_show_provider_picker(false, true, true));
    assert!(!should_show_provider_picker(false, false, false));
}

#[test]
fn persisted_reader_does_not_merge_environment_values() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    let old_runtime = env::var_os("CAPSCODE_RUNTIME");
    env::set_var("CAPSCODE_RUNTIME", "auto");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "runtime = \"docker\"\n").unwrap();

    let document = read_persisted_document_at(&path).unwrap();
    assert_eq!(document["runtime"].as_str(), Some("docker"));
    restore_env("CAPSCODE_RUNTIME", old_runtime);
}

#[test]
fn config_mutations_canonicalize_and_prune_provider_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        "runtime = \"docker\"\n\n[network]\ntelemetry_sinkhole = false\n",
    )
    .unwrap();

    set_provider_default_at(&path, Tool::Pi, Some("KIMI-CODING-OPENAI")).unwrap();
    set_runtime_at(&path, "orbstack").unwrap();
    let document = read_persisted_document_at(&path).unwrap();
    assert_eq!(document["runtime"].as_str(), Some("orbstack"));
    assert_eq!(
        document["defaults"]["pi"].as_str(),
        Some("kimi-coding-openai")
    );
    assert_eq!(
        document["network"]["telemetry_sinkhole"].as_bool(),
        Some(false)
    );

    set_provider_default_at(&path, Tool::Pi, None).unwrap();
    let document = read_persisted_document_at(&path).unwrap();
    assert!(document.get("defaults").is_none());
    assert_eq!(document["runtime"].as_str(), Some("orbstack"));
}

#[test]
fn provider_defaults_reject_incompatible_tools_and_empty_ids() {
    assert!(validate_provider_default(Tool::Codex, "deepseek").is_err());
    assert!(validate_provider_default(Tool::Claude, "grok").is_err());

    for tool in [
        Tool::Claude,
        Tool::Codex,
        Tool::Opencode,
        Tool::Pi,
        Tool::Omp,
    ] {
        assert!(validate_provider_default(tool, "").is_err());
        assert!(validate_provider_default(tool, " \t ").is_err());
    }
}

#[test]
fn provider_endpoint_suffix_matches_api_style() {
    assert!(
        validate_provider_endpoint_suffix(ApiStyle::Anthropic, "https://api.example.com").is_ok()
    );
    assert!(
        validate_provider_endpoint_suffix(ApiStyle::Anthropic, "https://api.example.com/v1")
            .is_err()
    );
    assert!(
        validate_provider_endpoint_suffix(ApiStyle::OpenAI, "https://api.example.com/v1").is_ok()
    );
    assert!(
        validate_provider_endpoint_suffix(ApiStyle::OpenAI, "https://api.example.com").is_err()
    );
}

#[test]
fn pi_and_omp_reject_user_providers_without_a_usable_default_model() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let old_home = env::var_os("CAPSCODE_HOME");
    env::set_var("CAPSCODE_HOME", home.path());

    // User registry only — never touches the real ~/.capscode or keychain.
    // Bundled providers all ship a default_model, so this branch is only
    // reachable through an overlay like this.
    fs::write(
        home.path().join("providers.yaml"),
        r#"
no-default-model:
  api_style: openai
  base_url: https://example.invalid/v1
  auth: bearer
  key_env: NO_DEFAULT_MODEL_KEY

empty-default-model:
  api_style: openai
  base_url: https://example.invalid/v1
  auth: bearer
  key_env: EMPTY_DEFAULT_MODEL_KEY
  default_model: ""

ws-default-model:
  api_style: anthropic
  base_url: https://example.invalid/anthropic
  auth: bearer
  key_env: WS_DEFAULT_MODEL_KEY
  default_model: "   "
"#,
    )
    .unwrap();

    for id in [
        "no-default-model",
        "empty-default-model",
        "ws-default-model",
    ] {
        for tool in [Tool::Pi, Tool::Omp] {
            let error = validate_provider_default(tool, id)
                .expect_err("Pi/Omp require a non-empty default_model")
                .to_string();
            assert!(
                error.contains("default_model"),
                "tool={tool:?} id={id}: {error}"
            );
        }
        // Other tools do not gate on default_model at config-set time.
        assert!(
            validate_provider_default(Tool::Opencode, id).is_ok(),
            "OpenCode should accept provider {id} without a default_model"
        );
    }

    restore_env("CAPSCODE_HOME", old_home);
}

#[test]
fn config_edit_validation_message_preserves_anyhow_chain() {
    let error = validate_config_document("[bad").expect_err("unclosed table");
    let rendered = invalid_config_edit_message(&error);
    assert!(
        rendered.contains("parsing config TOML"),
        "missing outer context: {rendered}"
    );
    assert!(
        rendered.contains("unclosed") || rendered.contains("TOML parse"),
        "missing root cause: {rendered}"
    );

    let typed = validate_config_document("runtime = 5").expect_err("type error");
    let typed_rendered = format!("{typed:#}");
    assert!(
        typed_rendered.contains("validating typed config"),
        "{typed_rendered}"
    );
}

#[test]
fn invalid_mutations_leave_existing_bytes_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = "runtime = \"docker\"\n[not-valid\n";
    fs::write(&path, original).unwrap();

    assert!(set_runtime_at(&path, "bad").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    assert!(set_runtime_at(&path, "orbstack").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn config_rows_resolve_each_key_and_preserve_source_precedence() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    let old_runtime = env::var_os("CAPSCODE_RUNTIME");
    let old_opencode = env::var_os("CAPSCODE_DEFAULTS__OPENCODE");
    env::remove_var("CAPSCODE_DEFAULTS__OPENCODE");
    env::set_var("CAPSCODE_RUNTIME", "docker");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "runtime = \"auto\"\n").unwrap();

    let rows = config_rows_at(&path).unwrap();
    let runtime = rows
        .iter()
        .find(|row| row.key == ConfigKeyArg::Runtime)
        .unwrap();
    assert_eq!(
        runtime.persisted.as_ref().and_then(toml::Value::as_str),
        Some("auto")
    );
    assert_eq!(
        runtime.effective,
        Some(ConfigScalar::String("docker".to_string()))
    );
    assert_eq!(runtime.source, "env:CAPSCODE_RUNTIME");

    env::set_var("CAPSCODE_RUNTIME", "auto");
    let rows = config_rows_at(&path).unwrap();
    let runtime = rows
        .iter()
        .find(|row| row.key == ConfigKeyArg::Runtime)
        .unwrap();
    assert_eq!(
        runtime.effective,
        Some(ConfigScalar::String("auto".to_string()))
    );
    assert_eq!(runtime.source, "env:CAPSCODE_RUNTIME");

    for key in [
        ConfigKeyArg::DefaultsOpencode,
        ConfigKeyArg::DefaultsPi,
        ConfigKeyArg::DefaultsOmp,
    ] {
        let row = rows.iter().find(|row| row.key == key).unwrap();
        assert_eq!(row.effective, None);
        assert_eq!(row.error, None);
        assert_eq!(row.source, "built-in");
    }
    restore_env("CAPSCODE_RUNTIME", old_runtime);
    restore_env("CAPSCODE_DEFAULTS__OPENCODE", old_opencode);
}

#[test]
fn config_table_aligns_long_provider_values_without_tabs() {
    let rows = [
        ConfigRow {
            key: ConfigKeyArg::DefaultsPi,
            persisted: Some(toml::Value::String("kimi-coding-openai".to_string())),
            effective: Some(ConfigScalar::String("kimi-coding-openai".to_string())),
            source: "file".to_string(),
            error: None,
        },
        ConfigRow {
            key: ConfigKeyArg::DefaultsOmp,
            persisted: Some(toml::Value::String("zai-coding-openai".to_string())),
            effective: Some(ConfigScalar::String("zai-coding-openai".to_string())),
            source: "file".to_string(),
            error: None,
        },
    ];

    assert_eq!(
        format_config_table(&rows),
        "KEY           PERSISTED           EFFECTIVE           SOURCE\n\
             defaults.pi   kimi-coding-openai  kimi-coding-openai  file\n\
             defaults.omp  zai-coding-openai   zai-coding-openai   file"
    );
}

#[test]
fn wrong_persisted_type_stays_visible_in_config_rows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "[auth]\nkeychain = 1\n").unwrap();

    let rows = config_rows_at(&path).unwrap();
    let row = rows
        .iter()
        .find(|row| row.key == ConfigKeyArg::AuthKeychain)
        .unwrap();
    assert_eq!(row.persisted, Some(toml::Value::Integer(1)));
    assert!(row.effective.is_none());
    assert!(row.error.as_deref().unwrap().contains("boolean"));
    assert_eq!(
        format_persisted_text(row.persisted.as_ref().unwrap()),
        "INVALID(1)"
    );
}

#[test]
fn persisted_get_type_checks_without_consulting_provider_registry() {
    let document: toml::Value = toml::from_str(
        r#"
[defaults]
claude = "not-a-provider"
codex = " "
"#,
    )
    .unwrap();

    assert_eq!(
        persisted_value_for_get(&document, ConfigKeyArg::DefaultsClaude).unwrap(),
        ConfigScalar::String("not-a-provider".to_string())
    );
    let error = persisted_value_for_get(&document, ConfigKeyArg::DefaultsCodex)
        .unwrap_err()
        .to_string();
    assert!(error.contains("must not be empty"), "{error}");
}

#[test]
fn absent_unset_is_a_no_op_and_does_not_create_a_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    unset_runtime_at(&path).unwrap();
    assert!(!path.exists());

    fs::write(&path, "runtime = \"docker\"\n").unwrap();
    let original = fs::read(&path).unwrap();
    set_provider_default_at(&path, Tool::Pi, None).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn successful_config_write_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested/config.toml");
    set_runtime_at(&path, "docker").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[cfg(unix)]
#[test]
fn config_writer_never_widens_a_stricter_parent_mode() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("locked");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    let path = parent.join("config.toml");

    assert!(set_runtime_at(&path, "docker").is_err());
    assert_eq!(
        fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o500
    );
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn config_writer_rejects_a_path_without_a_parent() {
    let document = toml::Value::Table(toml::toml! { runtime = "docker" });
    let error = write_config_document(Path::new("config.toml"), &document)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no parent directory"), "{error}");
}

#[test]
fn pre_rename_failure_leaves_existing_target_intact() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("config.toml");
    fs::create_dir(&target).unwrap();
    let marker = target.join("marker");
    fs::write(&marker, "keep").unwrap();
    let document = toml::Value::Table(toml::toml! { runtime = "docker" });

    assert!(write_config_document(&target, &document).is_err());
    assert_eq!(fs::read_to_string(marker).unwrap(), "keep");
}

#[test]
fn keychain_cache_refresh_is_key_specific_and_fail_closed() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    let old_auth = env::var_os("CAPSCODE_AUTH__KEYCHAIN");
    let old_runtime = env::var_os("CAPSCODE_RUNTIME");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "[auth]\nkeychain = true\n").unwrap();

    env::set_var("CAPSCODE_RUNTIME", "not-a-runtime");
    env::remove_var("CAPSCODE_AUTH__KEYCHAIN");
    refresh_keychain_enabled_cache_at(&path);
    assert!(keychain_lookup_enabled());

    env::set_var("CAPSCODE_AUTH__KEYCHAIN", "not-a-bool");
    refresh_keychain_enabled_cache_at(&path);
    assert!(!keychain_lookup_enabled());

    restore_env("CAPSCODE_AUTH__KEYCHAIN", old_auth);
    restore_env("CAPSCODE_RUNTIME", old_runtime);
    *KEYCHAIN_ENABLED_CACHE.lock().unwrap() = None;
}

#[test]
fn file_only_edit_validation_accepts_known_sections_and_rejects_recursive_typos() {
    let valid = r#"
runtime = "docker"
[image]
pull_policy = "never"
[network]
enforce_vpn = false
[broker]
enabled = true
[git]
mount = false
[profile]
include = []
[defaults]
claude = "anthropic"
codex = "openai"
[auth]
keychain = false
"#;
    assert!(validate_config_document(valid).is_ok());

    let typo = valid.replace("mount = false", "munt = false");
    let error = validate_config_document(&typo).unwrap_err().to_string();
    assert!(error.contains("unknown config key 'git.munt'"), "{error}");

    let top_level_typo = valid.replace("runtime =", "runtim =");
    let error = validate_config_document(&top_level_typo)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown config key 'runtim'"), "{error}");
}

#[test]
fn editor_command_splits_ascii_whitespace_and_defaults_to_vi() {
    assert_eq!(
        editor_command(Some("code --wait")),
        ("code".to_string(), vec!["--wait".to_string()])
    );
    assert_eq!(
        editor_command(Some("   ")),
        ("vi".to_string(), Vec::<String>::new())
    );
    assert_eq!(
        editor_command(None),
        ("vi".to_string(), Vec::<String>::new())
    );
}

#[test]
fn update_config_keys_round_trip_and_validate_the_channel() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    let names = [
        "CAPSCODE_UPDATE__CHECK",
        "CAPSCODE_UPDATE__CHANNEL",
        "CAPSCODE_UPDATE__VERIFY_WITH_GH",
    ];
    let saved: Vec<_> = names.iter().map(|name| (name, env::var_os(name))).collect();
    for name in names {
        env::remove_var(name);
    }
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".capscode/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, "runtime = \"docker\"\n").unwrap();
    let effective = |key: ConfigKeyArg| {
        config_rows_at(&config_path)
            .unwrap()
            .into_iter()
            .find(|row| row.key == key)
            .unwrap()
            .effective
    };
    assert_eq!(
        effective(ConfigKeyArg::UpdateCheck),
        Some(ConfigScalar::Bool(true))
    );
    assert_eq!(
        effective(ConfigKeyArg::UpdateVerifyWithGh),
        Some(ConfigScalar::Bool(false))
    );
    assert_eq!(
        effective(ConfigKeyArg::UpdateChannel),
        Some(ConfigScalar::String("sayre4ux/capscode".into()))
    );

    set_section_value_at(
        &config_path,
        ConfigKeyArg::UpdateCheck,
        toml::Value::Boolean(false),
    )
    .unwrap();
    set_section_value_at(
        &config_path,
        ConfigKeyArg::UpdateChannel,
        toml::Value::String(parse_update_channel("fork-owner/capscode").unwrap()),
    )
    .unwrap();
    let text = fs::read_to_string(&config_path).unwrap();
    let config = validate_config_document(&text).unwrap();
    assert!(!config.update.check);
    assert_eq!(config.update.channel, "fork-owner/capscode");
    assert_eq!(
        effective(ConfigKeyArg::UpdateCheck),
        Some(ConfigScalar::Bool(false))
    );

    assert!(parse_update_channel("https://evil.example/x").is_err());
    assert!(parse_update_bool(ConfigKeyArg::UpdateCheck, "yes").is_err());
    let invalid = text.replace("fork-owner/capscode", "not a repo");
    assert!(validate_config_document(&invalid).is_err());
    fs::write(&config_path, &invalid).unwrap();
    assert!(config_rows_at(&config_path)
        .unwrap()
        .into_iter()
        .find(|row| row.key == ConfigKeyArg::UpdateChannel)
        .unwrap()
        .error
        .is_some());

    unset_section_value_at(&config_path, ConfigKeyArg::UpdateCheck).unwrap();
    unset_section_value_at(&config_path, ConfigKeyArg::UpdateChannel).unwrap();
    let cleared: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(cleared.get("update").is_none());
    for (name, value) in saved {
        restore_env(name, value);
    }
}
