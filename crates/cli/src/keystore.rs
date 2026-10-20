use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use keyring::Entry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const SERVICE: &str = "capscode";
pub(crate) const ACCOUNT: &str = "provider-keys";
/// Overrides the keychain service the provider-key bundle is stored under, so a
/// development build can be pointed at a throwaway item instead of the real one.
/// Distinct from `CAPSCODE_KEYCHAIN_SERVICE`, which names the *Anthropic
/// credential* item; these are two separate keychain entries.
const SERVICE_ENV: &str = "CAPSCODE_KEYSTORE_SERVICE";
const VERSION: u64 = 1;
const API_KEY_KIND: &str = "api_key";

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) struct Bundle {
    pub(crate) version: u64,
    pub(crate) revision: u64,
    pub(crate) keys: BTreeMap<String, BundleKey>,
}

#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) struct BundleKey {
    pub(crate) value: String,
    pub(crate) kind: String,
    pub(crate) stored_at: u64,
}

impl fmt::Debug for BundleKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BundleKey")
            .field("value", &"<redacted>")
            .field("kind", &self.kind)
            .field("stored_at", &self.stored_at)
            .finish()
    }
}

impl fmt::Debug for Bundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bundle")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("key_count", &self.keys.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum KeystoreState {
    ItemMissing,
    AccessDenied,
    Corrupt,
    VersionTooNew,
    Ok(Bundle),
}

impl fmt::Debug for KeystoreState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ItemMissing => formatter.write_str("ItemMissing"),
            Self::AccessDenied => formatter.write_str("AccessDenied"),
            Self::Corrupt => formatter.write_str("Corrupt"),
            Self::VersionTooNew => formatter.write_str("VersionTooNew"),
            Self::Ok(bundle) => formatter.debug_tuple("Ok").field(bundle).finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ListedKey {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) stored_at: u64,
    pub(crate) unknown: bool,
}

/// Keychain service for the provider-key bundle, honoring [`SERVICE_ENV`].
pub(crate) fn service() -> String {
    std::env::var(SERVICE_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| SERVICE.to_string())
}

/// The keychain entry holding the provider-key bundle.
fn entry() -> std::result::Result<Entry, keyring::Error> {
    Entry::new(&service(), ACCOUNT)
}

pub(crate) fn load() -> KeystoreState {
    load_password(read_password())
}

pub(crate) fn set_key(id: &str, value: &str) -> Result<()> {
    if id.is_empty() {
        bail!("provider id must not be empty");
    }
    if value.is_empty() {
        bail!("provider key must not be empty");
    }
    mutate(|bundle| {
        bundle.keys.insert(
            id.to_string(),
            BundleKey {
                value: value.to_string(),
                kind: API_KEY_KIND.to_string(),
                stored_at: now_unix_seconds(),
            },
        );
        Ok(())
    })
}

/// Returns whether a key was actually removed. A missing item or an absent id
/// is a no-op: only `set` may initialize the bundle, and a delete that removed
/// nothing must not touch the keychain (no write, no revision bump).
pub(crate) fn remove_key(id: &str) -> Result<bool> {
    if id.is_empty() {
        bail!("provider id must not be empty");
    }
    match remove_key_from_state(load(), id)? {
        Some(bundle) => {
            write(&bundle)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

fn remove_key_from_state(state: KeystoreState, id: &str) -> Result<Option<Bundle>> {
    if state == KeystoreState::ItemMissing {
        return Ok(None);
    }
    let mut bundle = bundle_for_mutation(state)?;
    if bundle.keys.remove(id).is_none() {
        return Ok(None);
    }
    bundle.revision = bundle
        .revision
        .checked_add(1)
        .context("provider keychain bundle revision overflow")?;
    Ok(Some(bundle))
}

pub(crate) fn reset() -> Result<()> {
    match entry()
        .context("opening provider keychain entry")?
        .delete_credential()
    {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("resetting provider keychain bundle"),
    }
}

pub(crate) fn list(registry_ids: &HashSet<String>) -> Result<Vec<ListedKey>> {
    listed_keys(load(), registry_ids)
}

fn listed_keys(state: KeystoreState, registry_ids: &HashSet<String>) -> Result<Vec<ListedKey>> {
    let bundle = match state {
        KeystoreState::Ok(bundle) => bundle,
        // An absent item is the normal first state, not a failure.
        KeystoreState::ItemMissing => return Ok(Vec::new()),
        state => return Err(state_error(state)),
    };
    Ok(bundle
        .keys
        .into_iter()
        .map(|(id, key)| ListedKey {
            unknown: !registry_ids.contains(&id),
            id,
            kind: key.kind,
            stored_at: key.stored_at,
        })
        .collect())
}

pub(crate) fn state_error(state: KeystoreState) -> anyhow::Error {
    match state {
        KeystoreState::ItemMissing => anyhow!(
            "provider keychain item is missing; run 'capscode key set <provider>' to create it"
        ),
        KeystoreState::AccessDenied => anyhow!(
            "provider keychain is unreadable (access denied, locked, or no keychain backend); check OS keychain permissions and try again"
        ),
        KeystoreState::Corrupt => anyhow!(
            "provider keychain bundle is corrupt; it was not deleted; run 'capscode key reset' to recover"
        ),
        KeystoreState::VersionTooNew => anyhow!(
            "provider keychain bundle version is too new; upgrade capscode or run 'capscode key reset'"
        ),
        KeystoreState::Ok(_) => anyhow!("provider keychain bundle is valid"),
    }
}

pub(crate) fn stored_at_iso(seconds: u64) -> String {
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;

    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * doy + 2) / 153;
    let day = doy - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn read_password() -> std::result::Result<String, keyring::Error> {
    entry()?.get_password()
}

fn load_password(result: std::result::Result<String, keyring::Error>) -> KeystoreState {
    match result {
        Ok(payload) => parse_payload(&payload),
        Err(keyring::Error::NoEntry) => KeystoreState::ItemMissing,
        Err(_) => KeystoreState::AccessDenied,
    }
}

fn parse_payload(payload: &str) -> KeystoreState {
    let document: Value = match serde_json::from_str(payload) {
        Ok(document) => document,
        Err(_) => return KeystoreState::Corrupt,
    };
    let Some(version) = document.get("version").and_then(Value::as_u64) else {
        return KeystoreState::Corrupt;
    };
    if version == 0 {
        return KeystoreState::Corrupt;
    }
    if version > VERSION {
        return KeystoreState::VersionTooNew;
    }

    let bundle: Bundle = match serde_json::from_value(document) {
        Ok(bundle) => bundle,
        Err(_) => return KeystoreState::Corrupt,
    };
    if bundle.version != VERSION || bundle.keys.values().any(|key| key.kind != API_KEY_KIND) {
        return KeystoreState::Corrupt;
    }
    KeystoreState::Ok(bundle)
}

fn mutate(apply: impl FnOnce(&mut Bundle) -> Result<()>) -> Result<()> {
    let bundle = mutate_bundle(load(), apply)?;
    write(&bundle)
}

fn mutate_bundle(
    state: KeystoreState,
    apply: impl FnOnce(&mut Bundle) -> Result<()>,
) -> Result<Bundle> {
    let mut bundle = bundle_for_mutation(state)?;
    apply(&mut bundle)?;
    bundle.revision = bundle
        .revision
        .checked_add(1)
        .context("provider keychain bundle revision overflow")?;
    Ok(bundle)
}

fn bundle_for_mutation(state: KeystoreState) -> Result<Bundle> {
    Ok(match state {
        KeystoreState::ItemMissing => Bundle {
            version: VERSION,
            revision: 0,
            keys: BTreeMap::new(),
        },
        KeystoreState::Ok(bundle) => bundle,
        state => return Err(state_error(state)),
    })
}

fn write(bundle: &Bundle) -> Result<()> {
    let payload = serde_json::to_string(bundle).context("serializing provider keychain bundle")?;
    entry()
        .context("opening provider keychain entry")?
        .set_password(&payload)
        .context("writing provider keychain bundle")
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_with(id: &str, value: &str) -> Bundle {
        let mut keys = BTreeMap::new();
        keys.insert(
            id.to_string(),
            BundleKey {
                value: value.to_string(),
                kind: API_KEY_KIND.to_string(),
                stored_at: 1_700_000_000,
            },
        );
        Bundle {
            version: VERSION,
            revision: 1,
            keys,
        }
    }

    #[test]
    fn bundle_roundtrip_preserves_revision_and_keys() {
        let bundle = bundle_with("openai", "sk-test");
        let payload = serde_json::to_string(&bundle).unwrap();
        assert_eq!(parse_payload(&payload), KeystoreState::Ok(bundle));
    }

    #[test]
    fn load_classifies_corrupt_and_unsupported_payloads() {
        assert_eq!(parse_payload("not json"), KeystoreState::Corrupt);
        assert_eq!(
            parse_payload(r#"{"revision":1,"keys":{}}"#),
            KeystoreState::Corrupt
        );
        assert_eq!(
            parse_payload(r#"{"version":0,"revision":1,"keys":{}}"#),
            KeystoreState::Corrupt
        );
        assert_eq!(
            parse_payload(r#"{"version":"1","revision":1,"keys":{}}"#),
            KeystoreState::Corrupt
        );
        assert_eq!(
            parse_payload(r#"{"version":1.5,"revision":1,"keys":{}}"#),
            KeystoreState::Corrupt
        );
        assert_eq!(
            parse_payload(r#"{"version":2,"revision":1,"keys":{}}"#),
            KeystoreState::VersionTooNew
        );
    }

    #[test]
    fn missing_item_is_a_distinct_load_state() {
        assert_eq!(
            load_password(Err(keyring::Error::NoEntry)),
            KeystoreState::ItemMissing
        );
    }

    #[test]
    fn mutations_block_corrupt_and_newer_bundles() {
        for state in [KeystoreState::Corrupt, KeystoreState::VersionTooNew] {
            let error = bundle_for_mutation(state).unwrap_err();
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn non_ok_states_have_distinct_secret_free_messages() {
        let states = [
            KeystoreState::ItemMissing,
            KeystoreState::AccessDenied,
            KeystoreState::Corrupt,
            KeystoreState::VersionTooNew,
        ];
        let messages: Vec<String> = states
            .into_iter()
            .map(|state| state_error(state).to_string())
            .collect();
        let unique: HashSet<&str> = messages.iter().map(String::as_str).collect();
        assert_eq!(unique.len(), messages.len());
        assert!(messages.iter().all(|message| !message.contains("secret")));
    }

    #[test]
    fn set_then_remove_preserves_unknown_ids() {
        let mut bundle = Bundle {
            version: VERSION,
            revision: 4,
            keys: BTreeMap::new(),
        };
        bundle.keys.insert(
            "future-provider".to_string(),
            BundleKey {
                value: "future-secret".to_string(),
                kind: API_KEY_KIND.to_string(),
                stored_at: 1,
            },
        );
        bundle = mutate_bundle(KeystoreState::Ok(bundle), |bundle| {
            bundle.keys.insert(
                "openai".to_string(),
                BundleKey {
                    value: "new-secret".to_string(),
                    kind: API_KEY_KIND.to_string(),
                    stored_at: 2,
                },
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(bundle.revision, 5);
        assert_eq!(bundle.keys["openai"].value, "new-secret");

        bundle = mutate_bundle(KeystoreState::Ok(bundle), |bundle| {
            bundle.keys.remove("openai");
            Ok(())
        })
        .unwrap();
        assert_eq!(bundle.revision, 6);
        assert!(bundle.keys.contains_key("future-provider"));
        assert!(!bundle.keys.contains_key("openai"));
    }

    #[test]
    fn remove_from_missing_item_does_not_initialize() {
        assert_eq!(
            remove_key_from_state(KeystoreState::ItemMissing, "openai").unwrap(),
            None
        );
    }

    #[test]
    fn remove_of_absent_id_does_not_write_or_bump_revision() {
        let bundle = bundle_with("openai", "sk-test");
        assert_eq!(
            remove_key_from_state(KeystoreState::Ok(bundle), "kimi-coding").unwrap(),
            None
        );
    }

    #[test]
    fn remove_of_present_id_bumps_revision() {
        let bundle = bundle_with("openai", "sk-test");
        let updated = remove_key_from_state(KeystoreState::Ok(bundle), "openai")
            .unwrap()
            .expect("key should be removed");
        assert_eq!(updated.revision, 2);
        assert!(updated.keys.is_empty());
    }

    #[test]
    fn remove_still_blocks_unusable_stores() {
        for state in [KeystoreState::Corrupt, KeystoreState::VersionTooNew] {
            assert!(remove_key_from_state(state, "openai").is_err());
        }
    }

    #[test]
    fn listing_a_missing_item_is_an_empty_store_not_an_error() {
        let ids: HashSet<String> = HashSet::new();
        assert_eq!(
            listed_keys(KeystoreState::ItemMissing, &ids).unwrap(),
            vec![]
        );
        assert!(listed_keys(KeystoreState::Corrupt, &ids).is_err());
    }

    #[test]
    fn list_marks_unknown_ids_without_values() {
        let mut bundle = bundle_with("future-provider", "must-not-render");
        bundle.keys.insert(
            "openai".to_string(),
            BundleKey {
                value: "also-must-not-render".to_string(),
                kind: API_KEY_KIND.to_string(),
                stored_at: 1_700_000_001,
            },
        );
        let ids: HashSet<String> = ["openai".to_string()].into_iter().collect();
        let listed: Vec<ListedKey> = bundle
            .keys
            .into_iter()
            .map(|(id, key)| ListedKey {
                unknown: !ids.contains(&id),
                id,
                kind: key.kind,
                stored_at: key.stored_at,
            })
            .collect();
        assert!(listed.iter().any(|key| key.unknown));
        assert!(listed.iter().all(|key| key.id != "must-not-render"));
        assert_eq!(stored_at_iso(0), "1970-01-01T00:00:00Z");
    }
}
