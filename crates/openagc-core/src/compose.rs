//! Drafts, reply/forward and send, as Swift sees them (spec §14.5).

use mail_domain::{EmailAddress, MessageId};
use mail_store::drafts::{self, DraftAttachment, DraftRecord, DraftState};
use mail_store::read;

use crate::ffi::AddressInfo;
use crate::{Core, CoreError, ErrorKind, runtime};

/// The demo mailbox's own address.
const DEMO_ADDRESS: &str = mail_store::demo::ME;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DraftStatus {
    Editing,
    Sending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DraftAttachmentInfo {
    pub path: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DraftInfo {
    /// 0 until saved.
    pub id: i64,
    pub thread_id: Option<String>,
    pub in_reply_to_message_id: Option<String>,
    pub to: Vec<AddressInfo>,
    pub cc: Vec<AddressInfo>,
    pub bcc: Vec<AddressInfo>,
    pub subject: String,
    /// The editable body.
    pub body_html: String,
    /// For a new reply or forward: the quoted original, shown below the
    /// editor and appended on save. Empty for saved drafts.
    pub quoted_html: String,
    pub attachments: Vec<DraftAttachmentInfo>,
    pub status: DraftStatus,
    pub error: Option<String>,
    pub updated_at: i64,
}

impl From<DraftRecord> for DraftInfo {
    fn from(d: DraftRecord) -> Self {
        let addrs = |v: Vec<EmailAddress>| v.into_iter().map(Into::into).collect();
        Self {
            id: d.id,
            thread_id: d.thread_id,
            in_reply_to_message_id: d.in_reply_to,
            to: addrs(d.to),
            cc: addrs(d.cc),
            bcc: addrs(d.bcc),
            subject: d.subject,
            body_html: d.body_html,
            quoted_html: d.quoted_html,
            attachments: d
                .attachments
                .into_iter()
                .map(|a| DraftAttachmentInfo {
                    path: a.path,
                    filename: a.filename,
                    mime_type: a.mime_type,
                    size: a.size,
                })
                .collect(),
            status: match d.state {
                DraftState::Editing => DraftStatus::Editing,
                DraftState::Sending => DraftStatus::Sending,
                DraftState::Failed => DraftStatus::Failed,
            },
            error: d.last_error,
            updated_at: d.updated_at,
        }
    }
}

impl From<DraftInfo> for DraftRecord {
    fn from(d: DraftInfo) -> Self {
        let addrs = |v: Vec<AddressInfo>| v.into_iter().map(Into::into).collect();
        Self {
            // Owned by the store; saving never changes it.
            gmail_draft_id: None,
            id: d.id,
            thread_id: d.thread_id,
            in_reply_to: d.in_reply_to_message_id,
            to: addrs(d.to),
            cc: addrs(d.cc),
            bcc: addrs(d.bcc),
            subject: d.subject,
            body_html: d.body_html,
            quoted_html: d.quoted_html,
            attachments: d
                .attachments
                .into_iter()
                .map(|a| DraftAttachment { path: a.path, filename: a.filename, mime_type: a.mime_type, size: a.size })
                .collect(),
            updated_at: d.updated_at,
            state: DraftState::Editing,
            last_error: None,
        }
    }
}

impl Core {
    /// The open account's address: from Gmail's profile, or the demo's.
    async fn own_address(&self) -> Result<String, CoreError> {
        let db = self.db()?;
        let email = runtime::run(async move { Ok(db.read(|c| read::sync_state(c, "account_email")).await?) }).await?;
        Ok(email.unwrap_or_else(|| DEMO_ADDRESS.to_owned()))
    }
}

#[uniffi::export]
impl Core {
    /// The address mail is sent from, for the composer's From line.
    pub async fn account_address(&self) -> Result<String, CoreError> {
        self.own_address().await
    }

    pub async fn reply_draft(&self, message_id: String, reply_all: bool) -> Result<DraftInfo, CoreError> {
        let me = vec![self.own_address().await?];
        let db = self.db()?;
        runtime::run(
            async move { Ok(mail_sync::reply_draft(&db, &MessageId(message_id), reply_all, &me).await?.into()) },
        )
        .await
    }

    pub async fn forward_draft(&self, message_id: String) -> Result<DraftInfo, CoreError> {
        let db = self.db()?;
        runtime::run(async move { Ok(mail_sync::forward_draft(&db, &MessageId(message_id)).await?.into()) }).await
    }

    /// Save (autosave) a draft; returns its id.
    pub async fn save_draft(&self, draft: DraftInfo) -> Result<i64, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let record: DraftRecord = draft.into();
            let now = mail_sync::now_millis();
            Ok(db.write(move |tx| drafts::save(tx, &record, now)).await?)
        })
        .await
    }

    pub async fn get_draft(&self, id: i64) -> Result<Option<DraftInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move { Ok(db.read(move |c| drafts::get(c, id)).await?.map(Into::into)) }).await
    }

    pub async fn list_drafts(&self) -> Result<Vec<DraftInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move { Ok(db.read(drafts::list).await?.into_iter().map(Into::into).collect()) }).await
    }

    /// Discard a draft, and its server copy if it was mirrored.
    pub async fn delete_draft(&self, id: i64) -> Result<(), CoreError> {
        let db = self.db()?;
        let now = mail_sync::now_millis();
        runtime::run(async move { Ok(db.write(move |tx| drafts::discard(tx, id, now)).await?) }).await?;
        if let Some(service) = self.accounts.sync_service() {
            service.outbox_changed();
        }
        Ok(())
    }

    /// A composer closed: mirror its edits to the server now rather than
    /// at the next 30 s tick. Does nothing without a connected account.
    pub fn flush_drafts(&self) {
        if let Some(service) = self.accounts.sync_service() {
            service.flush_drafts();
        }
    }

    /// Send a saved draft. With Gmail connected it goes through the outbox
    /// (retried if offline); the demo mailbox "sends" locally.
    pub async fn send_draft(&self, id: i64) -> Result<(), CoreError> {
        let from = EmailAddress::new(None, &self.own_address().await?);
        let db = self.db()?;
        let service = self.accounts.sync_service();
        let events = self.account_events();
        runtime::run(async move {
            let changes = mail_sync::send_draft(&db, id, from, service.is_some()).await.map_err(|e| match e {
                mail_sync::SyncError::Store(mail_store::StoreError::Invalid(m)) => {
                    CoreError::new(ErrorKind::InvalidInput, m)
                }
                other => other.into(),
            })?;
            mail_sync::SyncObserver::threads_changed(&crate::sync::EventObserver { events }, &changes);
            if let Some(service) = service {
                service.outbox_changed();
            }
            Ok(())
        })
        .await
    }

    /// Recipient suggestions for AppKit's token field, which asks
    /// synchronously on the main thread. A deliberate exception to "sync
    /// exports never block": it runs on a pooled read-only connection (no
    /// wait on the writer in WAL mode) and takes well under a millisecond.
    pub fn suggest_contacts_now(&self, text: String, limit: u32) -> Vec<AddressInfo> {
        let Ok(db) = self.db() else { return vec![] };
        db.read_blocking(|c| read::suggest_contacts(c, &text, limit))
            .map(|v| v.into_iter().map(Into::into).collect())
            .unwrap_or_default()
    }

    /// Recipient suggestions, most-written-to first.
    pub async fn suggest_contacts(&self, text: String, limit: u32) -> Result<Vec<AddressInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            Ok(db.read(move |c| read::suggest_contacts(c, &text, limit)).await?.into_iter().map(Into::into).collect())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;

    use crate::{Core, CoreConfig, CoreEvent, EventListener};

    struct Noop;
    impl EventListener for Noop {
        fn on_event(&self, _: Option<String>, _: CoreEvent) {}
    }

    #[test]
    fn demo_reply_save_send_and_suggestions() {
        let dir = std::env::temp_dir().join(format!("openagc-core-compose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        block_on(core.clone().open_account("demo".into())).unwrap();
        block_on(core.debug_seed_demo_mailbox(60)).unwrap();
        assert_eq!(block_on(core.account_address()).unwrap(), "me@example.com");

        let thread = block_on(core.list_threads("INBOX".into(), None, 1)).unwrap().rows.remove(0);
        let detail = block_on(core.get_thread(thread.id.clone())).unwrap().unwrap();
        let last = detail.messages.last().unwrap().id.clone();
        let mut draft = block_on(core.reply_draft(last, false)).unwrap();
        assert!(draft.subject.starts_with("Re: "));
        assert!(!draft.to.is_empty());
        draft.body_html = "<p>Sounds good.</p>".into();
        let id = block_on(core.save_draft(draft)).unwrap();
        assert_eq!(block_on(core.list_drafts()).unwrap().len(), 1);

        let sent_before = block_on(core.list_threads("SENT".into(), None, 500)).unwrap().rows.len();
        block_on(core.send_draft(id)).unwrap();
        assert!(block_on(core.get_draft(id)).unwrap().is_none());
        let sent = block_on(core.list_threads("SENT".into(), None, 500)).unwrap().rows;
        assert!(sent.iter().any(|t| t.id == thread.id), "the reply joined its thread in Sent");
        assert!(sent.len() >= sent_before);

        let suggestions = block_on(core.suggest_contacts("rive".into(), 5)).unwrap();
        assert!(
            suggestions
                .iter()
                .all(|a| a.email.contains("rive") || a.name.as_deref().unwrap_or("").to_lowercase().contains("rive"))
        );
        assert!(block_on(core.suggest_contacts("".into(), 5)).unwrap().is_empty());
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..250 {
            if done() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn closing_a_composer_mirrors_the_draft_and_discarding_removes_it() {
        let dir = std::env::temp_dir().join(format!("openagc-core-mirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        block_on(core.clone().open_account("acct".into())).unwrap();
        let fake = Arc::new(provider_api::fake::FakeProvider::new("me@example.com", 1_790_000_000_000, 50));
        core.start_sync_with(fake.clone()).unwrap();
        assert!(wait_until(|| block_on(core.account_address()).is_ok_and(|a| a == "me@example.com")));

        let draft = super::DraftInfo {
            id: 0,
            thread_id: None,
            in_reply_to_message_id: None,
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: "Plans".into(),
            body_html: "<p>Draft</p>".into(),
            quoted_html: String::new(),
            attachments: vec![],
            status: super::DraftStatus::Editing,
            error: None,
            updated_at: 0,
        };
        let id = block_on(core.save_draft(draft)).unwrap();
        core.flush_drafts();
        assert!(wait_until(|| fake.drafts().len() == 1), "mirrored without waiting for the 30 s tick");
        assert!(wait_until(|| block_on(core.get_draft(id)).unwrap().is_some()));
        block_on(core.delete_draft(id)).unwrap();
        assert!(wait_until(|| fake.drafts().is_empty()), "server copy deleted");
        core.stop_sync();
    }
}
