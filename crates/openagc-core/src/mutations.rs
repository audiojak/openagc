//! Mailbox mutations from the UI (spec §4.2, §7.4). Applied to the store at
//! once and queued for Gmail; Swift has already updated its rows, so these
//! are async and never block the main thread on the store's writer.

use mail_domain::{LabelId, ThreadId, system_labels};
use mail_sync::LocalChange;

use crate::sync::EventObserver;
use crate::{Core, CoreError, ErrorKind, runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct OutboxStatus {
    pub pending: u32,
    pub failed: u32,
}

fn threads(ids: Vec<String>) -> Result<Vec<ThreadId>, CoreError> {
    if ids.is_empty() {
        return Err(CoreError::new(ErrorKind::InvalidInput, "no threads given"));
    }
    Ok(ids.into_iter().map(ThreadId).collect())
}

impl Core {
    async fn mutate(&self, change: LocalChange) -> Result<(), CoreError> {
        let service = self.accounts.sync_service();
        let db = self.db()?;
        let events = self.events.clone();
        runtime::run(async move {
            match service {
                Some(service) => {
                    service.engine().apply_change(change, true).await?;
                    service.outbox_changed();
                }
                // No provider (the demo mailbox): local only.
                None => {
                    let changes = mail_sync::apply_local_change(&db, change, false).await?;
                    mail_sync::SyncObserver::threads_changed(&EventObserver { events }, &changes);
                }
            }
            Ok(())
        })
        .await
    }
}

#[uniffi::export]
impl Core {
    pub async fn archive(&self, thread_ids: Vec<String>) -> Result<(), CoreError> {
        self.mutate(LocalChange::archive(threads(thread_ids)?)).await
    }

    pub async fn move_to_inbox(&self, thread_ids: Vec<String>) -> Result<(), CoreError> {
        self.mutate(LocalChange::move_to_inbox(threads(thread_ids)?)).await
    }

    pub async fn set_read(&self, thread_ids: Vec<String>, read: bool) -> Result<(), CoreError> {
        self.mutate(LocalChange::set_read(threads(thread_ids)?, read)).await
    }

    pub async fn set_starred(&self, thread_ids: Vec<String>, starred: bool) -> Result<(), CoreError> {
        self.mutate(LocalChange::set_starred(threads(thread_ids)?, starred)).await
    }

    /// Add/remove user labels. System labels that change a message's
    /// nature (spam, trash, draft, sent) are refused here.
    pub async fn modify_labels(
        &self,
        thread_ids: Vec<String>,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Result<(), CoreError> {
        let protected = |l: &String| system_labels::PROTECTED.contains(&l.as_str());
        if add.iter().chain(&remove).any(protected) {
            return Err(CoreError::new(ErrorKind::InvalidInput, "spam, trash, draft and sent are not labels to set"));
        }
        let change = LocalChange::Labels {
            thread_ids: threads(thread_ids)?,
            add: add.into_iter().map(LabelId).collect(),
            remove: remove.into_iter().map(LabelId).collect(),
        };
        self.mutate(change).await
    }

    pub async fn trash(&self, thread_ids: Vec<String>) -> Result<(), CoreError> {
        self.mutate(LocalChange::Trash { thread_ids: threads(thread_ids)? }).await
    }

    pub async fn outbox_status(&self) -> Result<OutboxStatus, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let c = db.read(mail_store::outbox::counts).await?;
            Ok(OutboxStatus { pending: c.pending, failed: c.failed })
        })
        .await
    }

    /// Forget changes that could not be applied (they were already undone).
    pub async fn clear_failed_changes(&self) -> Result<(), CoreError> {
        let db = self.db()?;
        let events = self.events.clone();
        runtime::run(async move {
            db.write(|tx| mail_store::outbox::clear_failed(tx).map(|_| ())).await?;
            let c = db.read(mail_store::outbox::counts).await?;
            events.emit(crate::CoreEvent::OutboxStatus { pending: c.pending, failed: c.failed });
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::executor::block_on;
    use mail_domain::{EmailAddress, LabelId, MessageId, ThreadId};
    use provider_api::fake::FakeProvider;
    use provider_api::{FetchedBody, FetchedMessage};

    use crate::{Core, CoreConfig, CoreEvent, EventListener};

    struct Noop;
    impl EventListener for Noop {
        fn on_event(&self, _: CoreEvent) {}
    }

    fn core(name: &str) -> Arc<Core> {
        let dir = std::env::temp_dir().join(format!("openagc-core-mut-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap()
    }

    fn inbox_ids(core: &Core) -> Vec<String> {
        block_on(core.list_threads("INBOX".into(), None, 500)).unwrap().rows.into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn demo_mutations_apply_locally_without_an_outbox() {
        let core = core("demo");
        block_on(core.clone().open_account("demo".into())).unwrap();
        block_on(core.debug_seed_demo_mailbox(80)).unwrap();
        let first = inbox_ids(&core)[0].clone();
        block_on(core.archive(vec![first.clone()])).unwrap();
        assert!(!inbox_ids(&core).contains(&first));
        assert_eq!(block_on(core.outbox_status()).unwrap().pending, 0);
        block_on(core.move_to_inbox(vec![first.clone()])).unwrap();
        assert!(inbox_ids(&core).contains(&first));
        let err = block_on(core.modify_labels(vec![first], vec!["TRASH".into()], vec![])).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn synced_mutations_reach_the_server_through_the_outbox_loop() {
        let core = core("synced");
        block_on(core.clone().open_account("acct".into())).unwrap();
        let fake = Arc::new(FakeProvider::new("me@example.com", 1_790_000_000_000, 50));
        fake.seed(FetchedMessage {
            id: MessageId::new("m1"),
            thread_id: ThreadId::new("t1"),
            label_ids: vec![LabelId::new("INBOX"), LabelId::new("UNREAD")],
            internal_date: 1_790_000_000_000,
            from: Some(EmailAddress::new(None, "a@example.com")),
            body: Some(FetchedBody { text: Some("hi".into()), html: None, attachments: vec![] }),
            ..Default::default()
        });
        core.start_sync_with(fake.clone()).unwrap();
        for _ in 0..200 {
            if inbox_ids(&core).len() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        block_on(core.archive(vec!["t1".into()])).unwrap();
        assert!(inbox_ids(&core).is_empty(), "local at once");
        for _ in 0..200 {
            if !fake.message(&MessageId::new("m1")).unwrap().label_ids.contains(&LabelId::new("INBOX")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !fake.message(&MessageId::new("m1")).unwrap().label_ids.contains(&LabelId::new("INBOX")),
            "server archived"
        );
        assert_eq!(block_on(core.outbox_status()).unwrap().pending, 0);
        core.stop_sync();
    }
}
