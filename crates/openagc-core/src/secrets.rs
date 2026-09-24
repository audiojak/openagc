//! Secrets live in the macOS Keychain, which Swift owns (spec §12). Rust
//! reaches it only through this foreign trait, and only for OpenAGC's own
//! keys; the agent CLIs' credentials are never touched.

use mail_domain::Redacted;

use crate::CoreError;

#[uniffi::export(with_foreign)]
pub trait SecretStore: Send + Sync {
    fn get(&self, key: String) -> Result<Option<String>, CoreError>;
    fn set(&self, key: String, value: String) -> Result<(), CoreError>;
    fn delete(&self, key: String) -> Result<(), CoreError>;
}

/// Key names, in one place so they can be audited.
pub mod keys {
    pub fn refresh_token(account_id: &str) -> String {
        format!("oauth.refresh_token.{account_id}")
    }
    pub const CUSTOM_CLIENT_SECRET: &str = "oauth.client_secret.custom";
    pub const ANTHROPIC_API_KEY: &str = "anthropic.api_key";
}

/// Read a secret as `Redacted` so it cannot end up in logs by accident.
pub(crate) fn get_redacted(store: &dyn SecretStore, key: &str) -> Result<Option<Redacted<String>>, CoreError> {
    Ok(store.get(key.to_owned())?.map(Redacted::new))
}

/// In-memory store for tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemorySecrets(pub std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemorySecrets {
    fn get(&self, key: String) -> Result<Option<String>, CoreError> {
        Ok(self.0.lock().unwrap().get(&key).cloned())
    }
    fn set(&self, key: String, value: String) -> Result<(), CoreError> {
        self.0.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn delete(&self, key: String) -> Result<(), CoreError> {
        self.0.lock().unwrap().remove(&key);
        Ok(())
    }
}
