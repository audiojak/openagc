//! Mailbox mutations from the UI (spec §4.2, §7.4). Applied to the store at
//! once and queued for Gmail; Swift has already updated its rows, so these
//! are async and never block the main thread on the store's writer.

use mail_domain::{LabelId, ThreadId, system_labels};
use mail_sync::LocalChange;

use crate::ffi::LabelInfo;
use crate::sync::EventObserver;
use crate::{Core, CoreError, ErrorKind, runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct OutboxStatus {
    pub pending: u32,
    pub failed: u32,
}

/// White or near-black text, whichever reads better on `background`.
fn text_color_for(background: &str) -> &'static str {
    let hex = background.trim_start_matches('#');
    let channel = |i: usize| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0) as f64;
    let luminance = 0.299 * channel(0) + 0.587 * channel(2) + 0.114 * channel(4);
    if luminance > 150.0 { "#000000" } else { "#ffffff" }
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
        let events = self.account_events();
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

    /// Create a user label, or return the one with that name (spec §10.2,
    /// §11). A `/` path creates any missing parents first, so the label
    /// tree never has holes OpenAGC made (spec §14.3). `color` is a
    /// background like `#fb4c2f` for the label itself; parents get none.
    /// Needs Gmail to be reachable: label ids come from the server.
    pub async fn create_label(&self, name: String, color: Option<String>) -> Result<LabelInfo, CoreError> {
        let name = name.trim().trim_matches('/').to_owned();
        let segments: Vec<&str> = name.split('/').collect();
        for depth in 1..segments.len() {
            self.create_single_label(segments[..depth].join("/"), None).await?;
        }
        self.create_single_label(name, color).await
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
        let events = self.account_events();
        runtime::run(async move {
            db.write(|tx| mail_store::outbox::clear_failed(tx).map(|_| ())).await?;
            let c = db.read(mail_store::outbox::counts).await?;
            events.emit(crate::CoreEvent::OutboxStatus { pending: c.pending, failed: c.failed });
            Ok(())
        })
        .await
    }
}

/// Local label ids must differ even when created in the same millisecond
/// (a path creates its parents back to back).
fn next_local_label() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Core {
    /// One label, no parents: returns the existing label with that name
    /// (case-insensitive) or creates it. The text color is chosen for
    /// contrast with `color`.
    async fn create_single_label(&self, name: String, color: Option<String>) -> Result<LabelInfo, CoreError> {
        let name = name.trim().trim_matches('/').to_owned();
        if name.is_empty() || name.len() > 225 || name.split('/').any(|part| part.trim().is_empty()) {
            return Err(CoreError::new(ErrorKind::InvalidInput, "a label needs a name (nest with /)"));
        }
        if system_labels::PROTECTED
            .iter()
            .chain(&["INBOX", "UNREAD", "STARRED", "IMPORTANT"])
            .any(|s| s.eq_ignore_ascii_case(&name))
        {
            return Err(CoreError::new(ErrorKind::InvalidInput, format!("{name} is a system label")));
        }
        let db = self.db()?;
        let service = self.accounts.sync_service();
        let events = self.account_events();
        runtime::run(async move {
            let wanted = name.clone();
            let labels = db.read(mail_store::read::list_labels).await?;
            if let Some(existing) = labels.into_iter().find(|l| l.name.eq_ignore_ascii_case(&wanted)) {
                return Ok(existing.into());
            }
            let color = color.map(|bg| {
                let fg = text_color_for(&bg);
                (bg, fg.to_owned())
            });
            let label = match &service {
                Some(service) => {
                    let provider = service.engine().provider();
                    let with_color = color.as_ref().map(|(bg, fg)| (bg.as_str(), fg.as_str()));
                    match provider.create_label(&name, with_color).await {
                        // Gmail only accepts palette colors; try again plain.
                        Err(provider_api::ProviderError::Invalid(_)) if with_color.is_some() => {
                            provider.create_label(&name, None).await?
                        }
                        other => other?,
                    }
                }
                // The demo mailbox has no server: a local id.
                None => mail_domain::Label {
                    id: LabelId(format!("Label_local_{}_{}", mail_sync::now_millis(), next_local_label())),
                    name,
                    kind: mail_domain::LabelKind::User,
                    color: color.map(|(background, text)| mail_domain::LabelColor { background, text }),
                    visible: true,
                },
            };
            let stored = label.clone();
            db.write(move |tx| {
                let mut w = mail_store::MailWriter::new(tx);
                w.upsert_labels(std::slice::from_ref(&stored))?;
                w.finish().map(|_| ())
            })
            .await?;
            // The sidebar reloads its labels on any change event.
            events.emit(crate::CoreEvent::ThreadsChanged {
                mailbox_id: label.id.0.clone(),
                hint: crate::ChangeHint::default(),
            });
            Ok(label.into())
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
        fn on_event(&self, _: Option<String>, _: CoreEvent) {}
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
        // The op is removed just after the server call returns.
        for _ in 0..200 {
            if block_on(core.outbox_status()).unwrap().pending == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(block_on(core.outbox_status()).unwrap().pending, 0);

        // Labels are created on the server, then stored; a path creates its
        // missing parent first, without the child's color.
        let label = block_on(core.create_label("Sorted/Later".into(), Some("#ffad47".into()))).unwrap();
        assert_eq!(label.id, "Label_2");
        assert_eq!(label.text_color.as_deref(), Some("#000000"), "dark text on a light color");
        let labels = block_on(core.list_labels()).unwrap();
        let parent = labels.iter().find(|l| l.name == "Sorted").expect("parent created");
        assert_eq!(parent.id, "Label_1");
        assert_eq!(parent.background_color, None);
        assert!(labels.iter().any(|l| l.id == "Label_2" && l.name == "Sorted/Later"));
        let same = block_on(core.create_label("SORTED/LATER".into(), None)).unwrap();
        assert_eq!(same.id, "Label_2");
        let deeper = block_on(core.create_label("Sorted/Later/Soon".into(), None)).unwrap();
        assert_eq!(deeper.id, "Label_3", "existing parents are reused");
        core.stop_sync();
    }
}
