//! The Rust core behind the OpenAGC app. This is the only crate that knows
//! about UniFFI; everything Swift can see is exported from here.

use std::sync::{Arc, RwLock};

uniffi::setup_scaffolding!();

mod account;
mod agents;
mod attachments;
mod cloud_routines;
mod compose;
mod error;
mod events;
pub mod ffi;
mod logging;
mod mail;
mod mutations;
mod registry;
mod routines;
mod runtime;
pub mod secrets;
mod sync;

pub use account::{ConnectedAccount, OAuthClientConfig, SignInStart};
pub use agents::{
    AgentActionInfo, AgentEventInfo, AgentProviderInfo, AgentSessionInfo, AgentStatusInfo, AgentTranscriptItem,
    PromptContextInfo, TextExtractor,
};
pub use attachments::AttachmentFileInfo;
pub use cloud_routines::RoutineHandoff;
pub use compose::{DraftAttachmentInfo, DraftInfo, DraftStatus};
pub use error::{CoreError, ErrorKind};
pub use events::{ChangeHint, CoreEvent, EventBus, EventListener, LogLevel, NewMailInfo, SyncState};
pub use mutations::OutboxStatus;
pub use registry::{AccountKind, AccountSummary};
pub use routines::{RoutineInfo, RoutinePreviewRow, RoutineRunInfo};
pub use secrets::SecretStore;

/// Configuration the app passes when it creates the core.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreConfig {
    /// Directory for databases and runtime files,
    /// e.g. `~/Library/Application Support/OpenAGC`.
    pub data_dir: String,
    /// Directory for `core.log`, e.g. `~/Library/Logs/OpenAGC`. `None`
    /// disables the file log (tests); warnings still reach Swift.
    pub log_dir: Option<String>,
}

/// The core. Swift holds exactly one for the app's lifetime.
#[derive(uniffi::Object)]
pub struct Core {
    config: CoreConfig,
    events: EventBus,
    secrets: Arc<dyn SecretStore>,
    open_accounts: RwLock<registry::OpenAccounts>,
    /// Serializes changes to `accounts/index.json`.
    index_lock: tokio::sync::Mutex<()>,
    accounts: account::AccountState,
    agents: agents::AgentHub,
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn new(
        config: CoreConfig,
        secrets: Arc<dyn SecretStore>,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<Self>, CoreError> {
        if config.data_dir.is_empty() {
            return Err(CoreError::new(ErrorKind::InvalidInput, "data_dir must not be empty"));
        }
        let events = EventBus::start(listener, runtime::runtime().handle());
        logging::init(config.log_dir.as_deref().map(std::path::Path::new), events.clone());
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "core started");
        Ok(Arc::new(Self {
            config,
            events,
            secrets,
            open_accounts: RwLock::new(registry::OpenAccounts::default()),
            index_lock: tokio::sync::Mutex::new(()),
            accounts: Default::default(),
            agents: Default::default(),
        }))
    }

    /// Round-trip check used by the app at launch and by tests.
    pub fn ping(&self, message: String) -> String {
        format!("pong: {message}")
    }

    /// Async round-trip: proves exported futures run on the core runtime
    /// (the sleep needs tokio's timer) when awaited from Swift.
    pub async fn ping_async(&self, message: String) -> Result<String, CoreError> {
        runtime::run(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            let thread = std::thread::current().name().unwrap_or_default().to_owned();
            Ok(format!("pong: {message} (on {thread})"))
        })
        .await
    }

    /// The core's version, for About and diagnostics.
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    pub fn data_dir(&self) -> String {
        self.config.data_dir.clone()
    }

    /// Diagnostics hook: emit `ThreadsChanged` events as if a sync had
    /// inserted `thread_ids`, one event per id, to exercise coalescing.
    pub fn debug_emit_threads_changed(&self, mailbox_id: String, thread_ids: Vec<String>) {
        for id in thread_ids {
            self.account_events().emit(CoreEvent::ThreadsChanged {
                mailbox_id: mailbox_id.clone(),
                hint: ChangeHint { inserted: vec![id], ..ChangeHint::default() },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CoreConfig {
        CoreConfig { data_dir: "/tmp/openagc-test".into(), log_dir: None }
    }

    struct NoopListener;
    impl EventListener for NoopListener {
        fn on_event(&self, _: Option<String>, _: CoreEvent) {}
    }

    fn core() -> Arc<Core> {
        Core::new(config(), Arc::new(secrets::MemorySecrets::default()), Arc::new(NoopListener)).unwrap()
    }

    #[test]
    fn ping_round_trips() {
        let core = core();
        assert_eq!(core.ping("hi".into()), "pong: hi");
    }

    /// Awaited from a plain thread with a non-tokio executor, as Swift does.
    #[test]
    fn async_exports_run_on_the_core_runtime_from_any_executor() {
        let core = core();
        let reply = std::thread::spawn(move || futures::executor::block_on(core.ping_async("hi".into())))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(reply, "pong: hi (on openagc-core)");
    }

    #[test]
    fn empty_data_dir_is_rejected() {
        let err = Core::new(
            CoreConfig { data_dir: String::new(), log_dir: None },
            Arc::new(secrets::MemorySecrets::default()),
            Arc::new(NoopListener),
        )
        .err()
        .unwrap();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }
}
