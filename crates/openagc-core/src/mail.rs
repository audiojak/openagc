//! Account lifecycle and mail reads exposed to Swift (spec §4.2). Reads hit
//! SQLite only, never the network.

use std::path::PathBuf;
use std::sync::Arc;

use mail_domain::{MessageId, ThreadId};
use mail_store::{Db, StoreError, read};

use crate::ffi::{LabelInfo, MailboxInfo, RenderedBody, ThreadDetail, ThreadPage};
use crate::{Core, CoreError, ErrorKind, runtime};

/// An open account: its id and store.
pub(crate) struct Account {
    pub id: String,
    pub db: Db,
}

impl From<StoreError> for CoreError {
    fn from(e: StoreError) -> Self {
        let kind = match e {
            StoreError::NotFound(_) => ErrorKind::NotFound,
            StoreError::Invalid(_) => ErrorKind::InvalidInput,
            _ => ErrorKind::Storage,
        };
        CoreError::new(kind, e.to_string())
    }
}

impl Core {
    pub(crate) fn account_db_path(&self, account_id: &str) -> PathBuf {
        PathBuf::from(&self.config.data_dir).join("accounts").join(account_id).join("mail.sqlite")
    }

    pub(crate) fn db(&self) -> Result<Db, CoreError> {
        let guard = self.account.read().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|a| a.db.clone()).ok_or_else(|| CoreError::new(ErrorKind::NotFound, "no account is open"))
    }
}

fn valid_account_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[uniffi::export]
impl Core {
    /// Open (creating if needed) the store for `account_id` and make it the
    /// current account. Idempotent.
    pub async fn open_account(self: Arc<Self>, account_id: String) -> Result<(), CoreError> {
        if !valid_account_id(&account_id) {
            return Err(CoreError::new(ErrorKind::InvalidInput, "account id must be 1-64 of [A-Za-z0-9-]"));
        }
        let core = self.clone();
        runtime::run(async move {
            if core.account.read().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|a| a.id == account_id) {
                return Ok(());
            }
            let path = core.account_db_path(&account_id);
            let db = tokio::task::spawn_blocking(move || Db::open(&path))
                .await
                .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))??;
            tracing::info!(account = %account_id, "account opened");
            *core.account.write().unwrap_or_else(|e| e.into_inner()) = Some(Account { id: account_id, db });
            core.start_routine_scheduler();
            Ok(())
        })
        .await
    }

    /// Diagnostics / development hook: fill the open account with a
    /// deterministic synthetic mailbox of `threads` threads. Returns the
    /// number of messages written.
    pub async fn debug_seed_demo_mailbox(&self, threads: u32) -> Result<u32, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let spec = mail_store::demo::DemoSpec { threads, ..Default::default() };
            let stats = tokio::task::spawn_blocking(move || mail_store::demo::generate(&db, &spec))
                .await
                .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))??;
            Ok(stats.messages)
        })
        .await
    }

    /// The open account's id, if any.
    pub fn current_account_id(&self) -> Option<String> {
        self.account.read().unwrap_or_else(|e| e.into_inner()).as_ref().map(|a| a.id.clone())
    }

    pub async fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let mailboxes = db.read(read::list_mailboxes).await?;
            Ok(mailboxes.into_iter().map(Into::into).collect())
        })
        .await
    }

    pub async fn list_labels(&self) -> Result<Vec<LabelInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move { Ok(db.read(read::list_labels).await?.into_iter().map(Into::into).collect()) }).await
    }

    /// Threads in `mailbox_id` (a label id or `@archive`), newest first.
    /// Pass the previous page's `next_cursor` to continue.
    pub async fn list_threads(
        &self,
        mailbox_id: String,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<ThreadPage, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let page = db.read(move |c| read::list_threads(c, &mailbox_id, cursor.as_deref(), limit)).await?;
            Ok(ThreadPage { rows: page.rows.into_iter().map(Into::into).collect(), next_cursor: page.next_cursor })
        })
        .await
    }

    /// Threads matching a Gmail-style query (spec §8), newest first.
    /// A malformed query is an `InvalidInput` error whose message says
    /// what is wrong.
    pub async fn search_threads(
        &self,
        query: String,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<ThreadPage, CoreError> {
        let expr =
            mail_store::search::parse(&query).map_err(|e| CoreError::new(ErrorKind::InvalidInput, e.to_string()))?;
        let db = self.db()?;
        runtime::run(async move {
            let now = mail_sync::now_millis();
            let page = db.read(move |c| mail_store::search::search(c, &expr, now, cursor.as_deref(), limit)).await?;
            Ok(ThreadPage { rows: page.rows.into_iter().map(Into::into).collect(), next_cursor: page.next_cursor })
        })
        .await
    }

    /// A thread and its messages, oldest first; `None` if unknown.
    pub async fn get_thread(&self, thread_id: String) -> Result<Option<ThreadDetail>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let found = db.read(move |c| read::get_thread(c, &ThreadId(thread_id))).await?;
            Ok(found.map(|(summary, messages)| ThreadDetail {
                thread: summary.into(),
                messages: messages.into_iter().map(Into::into).collect(),
            }))
        })
        .await
    }

    /// The display-ready body of a message; `None` until sync fetched it.
    pub async fn get_rendered_body(&self, message_id: String) -> Result<Option<RenderedBody>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let id = MessageId(message_id.clone());
            let body = db.read(move |c| read::get_body(c, &id)).await?;
            // Plain-text mail renders through the same reader as HTML.
            Ok(body.map(|b| RenderedBody {
                message_id,
                html: b.html_sanitized.or_else(|| b.text_plain.as_deref().map(mail_mime::text_to_html)),
                text: b.text_plain,
                has_remote_images: b.has_remote_images,
            }))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;

    use crate::{Core, CoreConfig, CoreEvent, ErrorKind, EventListener};

    struct Noop;
    impl EventListener for Noop {
        fn on_event(&self, _: CoreEvent) {}
    }

    fn core(name: &str) -> Arc<Core> {
        let dir = std::env::temp_dir().join(format!("openagc-core-mail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap()
    }

    #[test]
    fn reads_before_opening_an_account_fail_cleanly() {
        let core = core("noaccount");
        let err = block_on(core.list_mailboxes()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        let err = block_on(core.clone().open_account("../escape".into())).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn open_seed_and_read_through_the_ffi_surface() {
        let core = core("reads");
        block_on(core.clone().open_account("acct-1".into())).unwrap();
        assert_eq!(core.current_account_id().as_deref(), Some("acct-1"));
        let messages = block_on(core.debug_seed_demo_mailbox(150)).unwrap();
        assert!(messages >= 150);

        let mailboxes = block_on(core.list_mailboxes()).unwrap();
        let inbox = mailboxes.iter().find(|m| m.id == "INBOX").unwrap();
        assert!(inbox.total_count > 0);
        assert!(mailboxes.iter().any(|m| m.id == "@archive"));
        assert!(mailboxes.iter().any(|m| m.name == "Receipts"));

        let first = block_on(core.list_threads("INBOX".into(), None, 10)).unwrap();
        assert_eq!(first.rows.len(), 10.min(inbox.total_count as usize));
        if inbox.total_count > 10 {
            let second = block_on(core.list_threads("INBOX".into(), first.next_cursor.clone(), 10)).unwrap();
            assert!(second.rows.iter().all(|r| !first.rows.contains(r)));
        }

        let thread = block_on(core.get_thread(first.rows[0].id.clone())).unwrap().unwrap();
        assert_eq!(thread.thread.id, first.rows[0].id);
        let message = &thread.messages[0];
        let body = block_on(core.get_rendered_body(message.id.clone())).unwrap().unwrap();
        assert!(body.html.unwrap().starts_with("<p>"));
        assert!(block_on(core.get_thread("missing".into())).unwrap().is_none());
    }
}
