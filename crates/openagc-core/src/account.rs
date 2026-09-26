//! Gmail sign-in, per-account credentials and starting sync (spec §7.3).
//!
//! Swift opens the authorization URL in the user's browser; Rust owns the
//! loopback listener, the code exchange and the Keychain writes (through
//! [`SecretStore`]). Tokens never cross back into Swift.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mail_store::Db;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mail_domain::Redacted;
use mail_sync::SyncEngine;
use provider_api::{MailProvider, ProviderError};
use provider_gmail::GmailProvider;
use provider_gmail::oauth::{self, GoogleTokenSource, OAuthClient, PendingAuthorization};
use serde::{Deserialize, Serialize};

use crate::secrets::{self, keys};
use crate::sync::{EventObserver, SyncService};
use crate::{Core, CoreError, ErrorKind, runtime};

/// How long to wait for the user to finish in the browser.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, uniffi::Record)]
pub struct OAuthClientConfig {
    pub client_id: String,
    /// Google requires it for desktop clients; it is not confidential.
    pub client_secret: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SignInStart {
    pub session_id: String,
    /// Open this in the user's default browser.
    pub authorization_url: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectedAccount {
    pub account_id: String,
    pub email: String,
}

#[derive(Serialize, Deserialize)]
struct StoredClient {
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Default)]
pub(crate) struct AccountState {
    pending: Mutex<HashMap<String, (PendingAuthorization, OAuthClientConfig)>>,
    sync: Mutex<Option<Arc<SyncService>>>,
}

impl AccountState {
    pub(crate) fn sync_service(&self) -> Option<Arc<SyncService>> {
        self.sync.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn client_key(account_id: &str) -> String {
    format!("oauth.client.{account_id}")
}

/// The account already holding `email`'s mail, if any: sign-in again must
/// not start a second store for the same mailbox. Each store records the
/// address it syncs (`sync_state.account_email`) once bootstrapped.
fn existing_account_id(data_dir: &Path, email: &str) -> Option<String> {
    let wanted = email.trim().to_lowercase();
    let entries = std::fs::read_dir(data_dir.join("accounts")).ok()?;
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if id == "demo" {
            continue;
        }
        let path = entry.path().join("mail.sqlite");
        if !path.is_file() {
            continue;
        }
        let stored = Db::open(&path)
            .ok()
            .and_then(|db| db.read_blocking(|c| mail_store::read::sync_state(c, "account_email")).ok().flatten());
        if stored.is_some_and(|e| e.trim().to_lowercase() == wanted) {
            return Some(id);
        }
    }
    None
}

fn random_id() -> Result<String, CoreError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// How far back mail is downloaded (spec §7.4); mirrors `mail_sync::SyncWindow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SyncWindow {
    Month,
    HalfYear,
    Year,
    Everything,
}

impl From<SyncWindow> for mail_sync::SyncWindow {
    fn from(w: SyncWindow) -> Self {
        match w {
            SyncWindow::Month => Self::Month,
            SyncWindow::HalfYear => Self::HalfYear,
            SyncWindow::Year => Self::Year,
            SyncWindow::Everything => Self::Everything,
        }
    }
}

impl From<mail_sync::SyncWindow> for SyncWindow {
    fn from(w: mail_sync::SyncWindow) -> Self {
        match w {
            mail_sync::SyncWindow::Month => Self::Month,
            mail_sync::SyncWindow::HalfYear => Self::HalfYear,
            mail_sync::SyncWindow::Year => Self::Year,
            mail_sync::SyncWindow::Everything => Self::Everything,
        }
    }
}

impl From<ProviderError> for CoreError {
    fn from(e: ProviderError) -> Self {
        let kind = match &e {
            ProviderError::Unauthorized => ErrorKind::Auth,
            ProviderError::Forbidden(_) => ErrorKind::PermissionDenied,
            ProviderError::NotFound(_) => ErrorKind::NotFound,
            ProviderError::RateLimited { .. } => ErrorKind::RateLimited,
            ProviderError::Network(_) | ProviderError::Server { .. } | ProviderError::Decode(_) => ErrorKind::Network,
            ProviderError::CursorExpired | ProviderError::Invalid(_) => ErrorKind::Internal,
        };
        CoreError::new(kind, e.to_string())
    }
}

impl From<mail_sync::SyncError> for CoreError {
    fn from(e: mail_sync::SyncError) -> Self {
        match e {
            mail_sync::SyncError::Provider(p) => p.into(),
            mail_sync::SyncError::Store(s) => s.into(),
            other => CoreError::new(ErrorKind::Internal, other.to_string()),
        }
    }
}

impl Core {
    /// Start sync for the open account with an explicit provider (tests use
    /// the in-memory fake; the app uses Gmail via `start_sync`).
    pub(crate) fn start_sync_with(self: &Arc<Self>, provider: Arc<dyn MailProvider>) -> Result<(), CoreError> {
        let db = self.db()?;
        let observer = Arc::new(EventObserver { events: self.events.clone() });
        let engine = Arc::new(SyncEngine::new(provider, db, observer));
        let weak = Arc::downgrade(self);
        let attribute: crate::sync::ExternalChanges = Arc::new(move |changes| {
            if let Some(core) = weak.upgrade() {
                runtime::runtime().spawn(async move { core.attribute_routine_changes(changes).await });
            }
        });
        let service = SyncService::start(engine, self.events.clone(), runtime::runtime().handle(), Some(attribute));
        if let Some(old) = self.accounts.sync.lock().unwrap_or_else(|e| e.into_inner()).replace(service) {
            old.stop();
        }
        Ok(())
    }

    fn gmail_provider(&self, account_id: &str) -> Result<Arc<dyn MailProvider>, CoreError> {
        let refresh = secrets::get_redacted(self.secrets.as_ref(), &keys::refresh_token(account_id))?
            .ok_or_else(|| CoreError::new(ErrorKind::Auth, "this account needs to sign in again"))?;
        let client_json = self
            .secrets
            .get(client_key(account_id))?
            .ok_or_else(|| CoreError::new(ErrorKind::Auth, "this account needs to sign in again"))?;
        let stored: StoredClient =
            serde_json::from_str(&client_json).map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?;
        let client =
            OAuthClient { client_id: stored.client_id, client_secret: stored.client_secret.map(Redacted::new) };
        let tokens = GoogleTokenSource::new(client, refresh);
        Ok(Arc::new(GmailProvider::new(tokens)?))
    }
}

#[uniffi::export]
impl Core {
    /// Begin Gmail sign-in: returns the URL Swift opens in the browser.
    pub async fn begin_gmail_sign_in(
        &self,
        client: OAuthClientConfig,
        login_hint: Option<String>,
    ) -> Result<SignInStart, CoreError> {
        if client.client_id.trim().is_empty() {
            return Err(CoreError::new(ErrorKind::InvalidInput, "an OAuth client ID is required"));
        }
        let oauth_client = OAuthClient {
            client_id: client.client_id.trim().to_owned(),
            client_secret: client.client_secret.clone().filter(|s| !s.is_empty()).map(Redacted::new),
        };
        let pending =
            runtime::run(async move { Ok(oauth::begin(&oauth_client, login_hint.as_deref()).await?) }).await?;
        let session_id = random_id()?;
        let url = pending.url.clone();
        self.accounts.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id.clone(), (pending, client));
        Ok(SignInStart { session_id, authorization_url: url })
    }

    /// Wait for the browser to finish, then create the account: exchange
    /// the code, read the address from Gmail, store credentials in the
    /// Keychain and open the account's store.
    pub async fn complete_gmail_sign_in(self: Arc<Self>, session_id: String) -> Result<ConnectedAccount, CoreError> {
        let (pending, config) = self
            .accounts
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id)
            .ok_or_else(|| CoreError::new(ErrorKind::NotFound, "no sign-in in progress"))?;
        let core = self.clone();
        runtime::run(async move {
            let code = pending.wait_for_code(SIGN_IN_TIMEOUT).await?;
            let client = OAuthClient {
                client_id: config.client_id.trim().to_owned(),
                client_secret: config.client_secret.clone().filter(|s| !s.is_empty()).map(Redacted::new),
            };
            let http = reqwest_client()?;
            let tokens = oauth::exchange_code(&http, oauth::TOKEN_URL, &client, &code).await?;
            let refresh = tokens.refresh_token.clone().ok_or_else(|| {
                CoreError::new(
                    ErrorKind::Auth,
                    "Google did not return a refresh token; remove OpenAGC's access in your Google account and try again",
                )
            })?;
            let source = GoogleTokenSource::new(client, Redacted::new(refresh.clone()));
            source.prime(tokens.access_token, tokens.expires_in).await;
            let gmail = GmailProvider::new(source)?;
            let profile = gmail.profile().await?;

            // Signing in again keeps the account (and its downloaded mail).
            let data_dir = PathBuf::from(&core.config.data_dir);
            let email = profile.email.clone();
            let existing = tokio::task::spawn_blocking(move || existing_account_id(&data_dir, &email))
                .await
                .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?;
            let account_id = match existing {
                Some(id) => id,
                None => random_id()?,
            };
            core.secrets.set(keys::refresh_token(&account_id), refresh)?;
            let stored = StoredClient { client_id: config.client_id.trim().to_owned(), client_secret: config.client_secret };
            core.secrets.set(
                client_key(&account_id),
                serde_json::to_string(&stored).map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?,
            )?;
            tracing::info!(account = %account_id, "gmail account connected");
            Ok(ConnectedAccount { account_id, email: profile.email })
        })
        .await
    }

    pub fn cancel_gmail_sign_in(&self, session_id: String) {
        self.accounts.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&session_id);
    }

    /// Start syncing the open account with Gmail.
    pub fn start_sync(self: Arc<Self>) -> Result<(), CoreError> {
        let account_id =
            self.current_account_id().ok_or_else(|| CoreError::new(ErrorKind::NotFound, "no account is open"))?;
        let provider = self.gmail_provider(&account_id)?;
        self.start_sync_with(provider)
    }

    pub fn stop_sync(&self) {
        if let Some(service) = self.accounts.sync.lock().unwrap_or_else(|e| e.into_inner()).take() {
            service.stop();
        }
    }

    /// The app became active or inactive; adjusts the poll interval and
    /// syncs immediately on activation.
    pub fn set_app_active(&self, active: bool) {
        if let Some(service) = self.accounts.sync.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            service.set_active(active);
        }
    }

    /// How far back the open account downloads mail (spec §7.4).
    pub async fn sync_window(&self) -> Result<SyncWindow, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let stored = db.read(|c| mail_store::read::sync_state(c, mail_sync::KEY_WINDOW)).await?;
            Ok(stored.as_deref().and_then(mail_sync::SyncWindow::parse).unwrap_or_default().into())
        })
        .await
    }

    /// Change how far back mail is downloaded. Widening queues the extra
    /// mail; narrowing stops fetching older mail but keeps what is stored.
    pub async fn set_sync_window(&self, window: SyncWindow) -> Result<(), CoreError> {
        let window: mail_sync::SyncWindow = window.into();
        let service = self.accounts.sync.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match service {
            Some(service) => {
                let engine = service.clone();
                runtime::run(async move { engine.engine().set_window(window).await.map_err(CoreError::from) }).await?;
                service.sync_now();
                Ok(())
            }
            None => {
                let db = self.db()?;
                runtime::run(async move {
                    db.write(move |tx| mail_store::read::set_sync_state(tx, mail_sync::KEY_WINDOW, window.as_str()))
                        .await?;
                    Ok(())
                })
                .await
            }
        }
    }

    /// Sync now (foreground, wake from sleep, network regained, ⌘R).
    pub fn sync_now(&self) {
        if let Some(service) = self.accounts.sync.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            service.sync_now();
        }
    }

    /// Whether an account has stored credentials (so sync can start). A
    /// Keychain that refuses to answer is an error, not "no credentials":
    /// the user must be told to sign in again rather than see nothing.
    pub fn account_has_credentials(&self, account_id: String) -> Result<bool, CoreError> {
        match self.secrets.get(keys::refresh_token(&account_id)) {
            Ok(token) => Ok(token.is_some()),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the stored sign-in from the Keychain");
                Err(e)
            }
        }
    }

    /// Remove an account: stop sync, forget its credentials and delete its
    /// local mail store. Gmail itself is not touched.
    pub async fn sign_out(&self, account_id: String) -> Result<(), CoreError> {
        if self.current_account_id().as_deref() == Some(account_id.as_str()) {
            self.stop_sync();
            if let Some(account) = self.account.write().unwrap_or_else(|e| e.into_inner()).take() {
                account.db.close();
            }
        }
        self.secrets.delete(keys::refresh_token(&account_id))?;
        self.secrets.delete(client_key(&account_id))?;
        let dir = self.account_db_path(&account_id).parent().map(std::path::Path::to_path_buf);
        runtime::run(async move {
            if let Some(dir) = dir {
                let _ = tokio::fs::remove_dir_all(dir).await;
            }
            Ok(())
        })
        .await
    }
}

fn reqwest_client() -> Result<reqwest::Client, CoreError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| CoreError::new(ErrorKind::Network, e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use futures::executor::block_on;
    use mail_domain::{EmailAddress, LabelId, MessageId, ThreadId};
    use provider_api::fake::FakeProvider;
    use provider_api::{FetchedBody, FetchedMessage};

    use crate::{Core, CoreConfig, CoreEvent, EventListener, SyncState};

    use super::*;

    #[derive(Default)]
    struct Recorder(StdMutex<Vec<CoreEvent>>);
    impl EventListener for Recorder {
        fn on_event(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn message(id: &str, labels: &[&str]) -> FetchedMessage {
        FetchedMessage {
            id: MessageId::new(id),
            thread_id: ThreadId::new(format!("t-{id}")),
            label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
            internal_date: 1_790_000_000_000,
            from: Some(EmailAddress::new(None, "a@example.com")),
            subject: format!("Subject {id}"),
            body: Some(FetchedBody { text: Some("hi".into()), html: None, attachments: vec![] }),
            ..Default::default()
        }
    }

    fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn the_sync_service_bootstraps_backfills_and_picks_up_new_mail() {
        let dir = std::env::temp_dir().join(format!("openagc-core-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let recorder = Arc::new(Recorder::default());
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            recorder.clone(),
        )
        .unwrap();
        block_on(core.clone().open_account("acct".into())).unwrap();

        let fake = Arc::new(FakeProvider::new("me@example.com", 1_790_000_000_000, 50));
        fake.seed(message("m1", &["INBOX", "UNREAD"]));
        fake.seed(message("m2", &["INBOX"]));
        core.start_sync_with(fake.clone()).unwrap();

        wait_for("bootstrap to fill the inbox", || {
            block_on(core.list_threads("INBOX".into(), None, 10)).map(|p| p.rows.len() == 2).unwrap_or(false)
        });
        fake.deliver(message("m3", &["INBOX", "UNREAD"]));
        core.sync_now();
        wait_for("new mail after sync_now", || {
            block_on(core.list_threads("INBOX".into(), None, 10)).map(|p| p.rows.len() == 3).unwrap_or(false)
        });

        // Change events are coalesced for up to 50 ms, so wait for them
        // rather than asserting the moment the data is visible.
        wait_for("an inbox change event", || {
            recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, CoreEvent::ThreadsChanged { mailbox_id, .. } if mailbox_id == "INBOX"))
        });
        assert!(
            recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, CoreEvent::SyncStatus { state: SyncState::Bootstrapping, .. }))
        );
        // Only the message that arrived after the first sync is announced.
        wait_for("a new-mail event", || {
            recorder.0.lock().unwrap().iter().any(|e| matches!(e, CoreEvent::NewMail { .. }))
        });
        let announced: Vec<String> = recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                CoreEvent::NewMail { messages } => {
                    Some(messages.iter().map(|m| m.message_id.clone()).collect::<Vec<_>>())
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(announced, vec!["m3"]);
        core.stop_sync();
    }

    struct TempRoot(std::path::PathBuf);
    impl TempRoot {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn signing_in_again_reuses_the_account_that_holds_that_mailbox() {
        let root = std::env::temp_dir().join(format!("openagc-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = TempRoot(root);
        for (id, email) in [("aaaa", "old@example.com"), ("bbbb", "Me@Example.com")] {
            let path = dir.path().join("accounts").join(id).join("mail.sqlite");
            let db = Db::open(&path).unwrap();
            db.write_blocking(move |tx| mail_store::read::set_sync_state(tx, "account_email", email)).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("accounts").join("cccc")).unwrap(); // no store yet
        assert_eq!(existing_account_id(dir.path(), " me@example.com ").as_deref(), Some("bbbb"));
        assert_eq!(existing_account_id(dir.path(), "old@example.com").as_deref(), Some("aaaa"));
        assert_eq!(existing_account_id(dir.path(), "new@example.com"), None);
        assert_eq!(existing_account_id(&dir.path().join("missing"), "me@example.com"), None);
    }

    #[test]
    fn sign_in_requires_a_client_id_and_unknown_sessions_fail() {
        let dir = std::env::temp_dir().join(format!("openagc-core-signin-{}", std::process::id()));
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Recorder::default()),
        )
        .unwrap();
        let err =
            block_on(core.begin_gmail_sign_in(OAuthClientConfig { client_id: " ".into(), client_secret: None }, None))
                .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        let start = block_on(core.begin_gmail_sign_in(
            OAuthClientConfig { client_id: "id.apps.googleusercontent.com".into(), client_secret: Some("s".into()) },
            None,
        ))
        .unwrap();
        assert!(start.authorization_url.starts_with("https://accounts.google.com/"));
        core.cancel_gmail_sign_in(start.session_id.clone());
        let err = block_on(core.clone().complete_gmail_sign_in(start.session_id)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(!core.account_has_credentials("nobody".into()).unwrap());
    }
}
