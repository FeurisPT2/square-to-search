//! OS keyring secret storage and thread-safe in-memory secret store for BYOK API keys.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use sts_core::config::{KEYRING_SERVICE_NAME, SecretSlot};

use crate::PlatformError;

/// Abstraction over persistent or in-memory storage of sensitive BYOK provider API keys.
///
/// Implementations must never leak secret values into logs, errors, or [`fmt::Debug`] output.
pub trait SecretStore: Send + Sync {
    /// Reads the stored secret for `slot`, returning `Ok(None)` when no entry exists.
    fn get_secret(&self, slot: SecretSlot) -> Result<Option<String>, PlatformError>;

    /// Stores or overwrites the secret for `slot`, rejecting empty or whitespace-only values.
    fn set_secret(&self, slot: SecretSlot, value: &str) -> Result<(), PlatformError>;

    /// Removes any stored secret for `slot`. Deleting a missing slot is idempotent (`Ok(())`).
    fn delete_secret(&self, slot: SecretSlot) -> Result<(), PlatformError>;

    /// Returns `true` when `slot` currently holds a non-empty secret.
    fn has_secret(&self, slot: SecretSlot) -> Result<bool, PlatformError> {
        Ok(self.get_secret(slot)?.is_some())
    }
}

fn normalize_secret_value(slot: SecretSlot, value: &str) -> Result<&str, PlatformError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(PlatformError::SecretStoreFailed(format!(
            "secret for `{}` cannot be empty",
            slot.slot_id()
        )));
    }
    Ok(trimmed)
}

fn map_keyring_error(operation: &str, slot: SecretSlot, err: keyring::Error) -> PlatformError {
    let detail = match err {
        keyring::Error::BadEncoding(_) => String::from("stored credential is not valid UTF-8"),
        other => other.to_string(),
    };
    PlatformError::SecretStoreFailed(format!(
        "failed to {operation} for `{}`: {detail}",
        slot.slot_id()
    ))
}

fn map_keyring_get_result(
    slot: SecretSlot,
    res: Result<String, keyring::Error>,
) -> Result<Option<String>, PlatformError> {
    match res {
        Ok(secret) if secret.trim().is_empty() => Ok(None),
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(map_keyring_error("read secret", slot, err)),
    }
}

fn map_keyring_delete_result(
    slot: SecretSlot,
    res: Result<(), keyring::Error>,
) -> Result<(), PlatformError> {
    match res {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(map_keyring_error("delete secret", slot, err)),
    }
}

/// Thread-safe in-memory [`SecretStore`] used in automated tests and sandboxed previews.
#[derive(Default)]
pub struct MemorySecretStore {
    entries: Mutex<HashMap<SecretSlot, String>>,
}

impl MemorySecretStore {
    /// Creates an empty in-memory secret store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_entries(&self) -> Result<MutexGuard<'_, HashMap<SecretSlot, String>>, PlatformError> {
        self.entries.lock().map_err(|_| {
            PlatformError::SecretStoreFailed(String::from("in-memory secret store mutex poisoned"))
        })
    }
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("MemorySecretStore");
        match self.entries.lock() {
            Ok(guard) => {
                let redacted: Vec<(&'static str, &'static str)> = SecretSlot::all()
                    .into_iter()
                    .filter(|slot| guard.contains_key(slot))
                    .map(|slot| (slot.slot_id(), "[REDACTED]"))
                    .collect();
                debug.field("entries", &redacted).finish()
            }
            Err(_) => debug.field("entries", &"<poisoned>").finish(),
        }
    }
}

impl SecretStore for MemorySecretStore {
    fn get_secret(&self, slot: SecretSlot) -> Result<Option<String>, PlatformError> {
        let guard = self.lock_entries()?;
        Ok(guard.get(&slot).cloned())
    }

    fn set_secret(&self, slot: SecretSlot, value: &str) -> Result<(), PlatformError> {
        let normalized = normalize_secret_value(slot, value)?;
        let mut guard = self.lock_entries()?;
        guard.insert(slot, normalized.to_owned());
        Ok(())
    }

    fn delete_secret(&self, slot: SecretSlot) -> Result<(), PlatformError> {
        let mut guard = self.lock_entries()?;
        guard.remove(&slot);
        Ok(())
    }

    fn has_secret(&self, slot: SecretSlot) -> Result<bool, PlatformError> {
        let guard = self.lock_entries()?;
        Ok(guard.contains_key(&slot))
    }
}

/// Production [`SecretStore`] backed by the OS credential store via `keyring`
/// (Windows Credential Manager on Windows, Secret Service via D-Bus on Linux).
#[derive(Debug, Clone)]
pub struct KeyringSecretStore {
    service_name: String,
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringSecretStore {
    /// Creates a keyring store scoped to [`KEYRING_SERVICE_NAME`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            service_name: KEYRING_SERVICE_NAME.to_owned(),
        }
    }

    fn entry(&self, slot: SecretSlot) -> Result<keyring::Entry, PlatformError> {
        keyring::Entry::new(&self.service_name, slot.account_key())
            .map_err(|err| map_keyring_error("open keyring entry", slot, err))
    }
}

impl SecretStore for KeyringSecretStore {
    fn get_secret(&self, slot: SecretSlot) -> Result<Option<String>, PlatformError> {
        let entry = self.entry(slot)?;
        map_keyring_get_result(slot, entry.get_password())
    }

    fn set_secret(&self, slot: SecretSlot, value: &str) -> Result<(), PlatformError> {
        let normalized = normalize_secret_value(slot, value)?;
        let entry = self.entry(slot)?;
        entry
            .set_password(normalized)
            .map_err(|err| map_keyring_error("store secret", slot, err))
    }

    fn delete_secret(&self, slot: SecretSlot) -> Result<(), PlatformError> {
        let entry = self.entry(slot)?;
        map_keyring_delete_result(slot, entry.delete_credential())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_secret_store_crud_and_idempotent_delete() -> Result<(), PlatformError> {
        let store = MemorySecretStore::new();

        for slot in SecretSlot::all() {
            assert_eq!(store.get_secret(slot)?, None);
            assert!(!store.has_secret(slot)?);
            // Deleting an absent slot must succeed idempotently.
            store.delete_secret(slot)?;
        }

        store.set_secret(SecretSlot::GeminiApiKey, "AIzaSy-secret-gemini-123")?;
        assert!(store.has_secret(SecretSlot::GeminiApiKey)?);
        assert_eq!(
            store.get_secret(SecretSlot::GeminiApiKey)?,
            Some(String::from("AIzaSy-secret-gemini-123"))
        );
        assert!(!store.has_secret(SecretSlot::OpenAiApiKey)?);

        // Overwrite with padded value trims outer whitespace consistently.
        store.set_secret(SecretSlot::GeminiApiKey, "  AIzaSy-updated-456 \n")?;
        assert_eq!(
            store.get_secret(SecretSlot::GeminiApiKey)?,
            Some(String::from("AIzaSy-updated-456"))
        );

        store.delete_secret(SecretSlot::GeminiApiKey)?;
        assert!(!store.has_secret(SecretSlot::GeminiApiKey)?);
        assert_eq!(store.get_secret(SecretSlot::GeminiApiKey)?, None);

        // Second delete on already-deleted slot remains Ok(()).
        store.delete_secret(SecretSlot::GeminiApiKey)?;
        Ok(())
    }

    #[test]
    fn memory_secret_store_rejects_blank_secrets_without_leaking_value() {
        let store = MemorySecretStore::new();
        for blank in ["", "   ", "\t\n  "] {
            let err = store.set_secret(SecretSlot::OpenAiApiKey, blank);
            assert!(
                matches!(err, Err(PlatformError::SecretStoreFailed(_))),
                "expected blank secret {blank:?} to be rejected, got {err:?}"
            );
        }
        assert!(matches!(
            store.has_secret(SecretSlot::OpenAiApiKey),
            Ok(false)
        ));
    }

    #[test]
    fn memory_secret_store_debug_redacts_all_secret_values() -> Result<(), PlatformError> {
        let store = MemorySecretStore::new();
        let gemini_secret = "AIzaSySuperSensitiveToken999";
        let openai_secret = "sk-live-super-sensitive-openai-888";
        let deepl_secret = "deepl-fx-super-sensitive-777";

        store.set_secret(SecretSlot::GeminiApiKey, gemini_secret)?;
        store.set_secret(SecretSlot::OpenAiApiKey, openai_secret)?;
        store.set_secret(SecretSlot::DeepLApiKey, deepl_secret)?;

        let formatted = format!("{store:?}");
        assert!(
            !formatted.contains(gemini_secret),
            "Debug leaked Gemini secret: {formatted}"
        );
        assert!(
            !formatted.contains(openai_secret),
            "Debug leaked OpenAI secret: {formatted}"
        );
        assert!(
            !formatted.contains(deepl_secret),
            "Debug leaked DeepL secret: {formatted}"
        );
        assert!(
            formatted.contains("[REDACTED]"),
            "Debug output should indicate redacted entries: {formatted}"
        );
        Ok(())
    }

    #[test]
    fn keyring_result_mapping_handles_no_entry_and_storage_errors() -> Result<(), PlatformError> {
        assert_eq!(
            map_keyring_get_result(SecretSlot::GeminiApiKey, Err(keyring::Error::NoEntry))?,
            None
        );
        assert_eq!(
            map_keyring_get_result(SecretSlot::GeminiApiKey, Ok(String::from("  ")))?,
            None
        );
        assert_eq!(
            map_keyring_get_result(
                SecretSlot::GeminiApiKey,
                Ok(String::from("AIzaSy-valid-key"))
            )?,
            Some(String::from("AIzaSy-valid-key"))
        );

        let locked_err = map_keyring_get_result(
            SecretSlot::OpenAiApiKey,
            Err(keyring::Error::NoStorageAccess(Box::new(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "keyring locked"),
            ))),
        );
        assert!(matches!(
            locked_err,
            Err(PlatformError::SecretStoreFailed(_))
        ));

        let bad_encoding_err = map_keyring_get_result(
            SecretSlot::DeepLApiKey,
            Err(keyring::Error::BadEncoding(vec![0xff, 0xfe])),
        );
        assert!(matches!(
            bad_encoding_err,
            Err(PlatformError::SecretStoreFailed(_))
        ));

        map_keyring_delete_result(SecretSlot::DeepLApiKey, Err(keyring::Error::NoEntry))?;
        map_keyring_delete_result(SecretSlot::DeepLApiKey, Ok(()))?;

        let store = KeyringSecretStore::new();
        assert_eq!(store.service_name, KEYRING_SERVICE_NAME);
        assert!(format!("{store:?}").contains(KEYRING_SERVICE_NAME));
        assert!(matches!(
            store.set_secret(SecretSlot::GeminiApiKey, "   "),
            Err(PlatformError::SecretStoreFailed(_))
        ));
        Ok(())
    }

    #[test]
    fn secret_store_trait_is_dyn_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn SecretStore>();
        assert_send_sync::<MemorySecretStore>();
        assert_send_sync::<KeyringSecretStore>();
    }
}
