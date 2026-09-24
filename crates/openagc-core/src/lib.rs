//! The Rust core behind the OpenAGC app. This is the only crate that knows
//! about UniFFI; everything Swift can see is exported from here.

use std::sync::Arc;

uniffi::setup_scaffolding!();

mod error;

pub use error::{CoreError, ErrorKind};

/// Configuration the app passes when it creates the core.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreConfig {
    /// Directory for databases and runtime files,
    /// e.g. `~/Library/Application Support/OpenAGC`.
    pub data_dir: String,
}

/// The core. Swift holds exactly one for the app's lifetime.
#[derive(uniffi::Object)]
pub struct Core {
    config: CoreConfig,
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn new(config: CoreConfig) -> Result<Arc<Self>, CoreError> {
        if config.data_dir.is_empty() {
            return Err(CoreError::new(ErrorKind::InvalidInput, "data_dir must not be empty"));
        }
        Ok(Arc::new(Self { config }))
    }

    /// Round-trip check used by the app at launch and by tests.
    pub fn ping(&self, message: String) -> String {
        format!("pong: {message}")
    }

    /// The core's version, for About and diagnostics.
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    pub fn data_dir(&self) -> String {
        self.config.data_dir.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CoreConfig {
        CoreConfig { data_dir: "/tmp/openagc-test".into() }
    }

    #[test]
    fn ping_round_trips() {
        let core = Core::new(config()).unwrap();
        assert_eq!(core.ping("hi".into()), "pong: hi");
    }

    #[test]
    fn empty_data_dir_is_rejected() {
        let err = Core::new(CoreConfig { data_dir: String::new() }).err().unwrap();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }
}
