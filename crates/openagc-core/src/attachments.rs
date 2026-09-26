//! Attachments as Swift sees them (spec §14.3, §15.3): downloaded on
//! demand into a per-account cache, then opened, previewed or dragged out
//! by the app, which marks each file quarantined.

use std::path::PathBuf;

use crate::{Core, CoreError, ErrorKind, runtime};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AttachmentFileInfo {
    pub path: String,
    pub filename: String,
    pub mime_type: String,
    pub content_id: Option<String>,
    /// True if this call fetched it; the app quarantines new files.
    pub downloaded: bool,
}

impl Core {
    fn attachment_cache_dir(&self) -> Result<PathBuf, CoreError> {
        let id =
            self.effective_account_id().ok_or_else(|| CoreError::new(ErrorKind::NotFound, "no account is open"))?;
        Ok(PathBuf::from(&self.config.data_dir).join("accounts").join(id).join("Attachments"))
    }
}

#[uniffi::export]
impl Core {
    /// The local file for an attachment (an `AttachmentInfo.id`), fetching
    /// it from Gmail first if it is not cached.
    pub async fn attachment_file(&self, attachment_id: String) -> Result<AttachmentFileInfo, CoreError> {
        let id: i64 = attachment_id
            .parse()
            .map_err(|_| CoreError::new(ErrorKind::InvalidInput, format!("bad attachment id {attachment_id:?}")))?;
        let db = self.db()?;
        let cache = self.attachment_cache_dir()?;
        let service = self.accounts.sync_service();
        runtime::run(async move {
            let provider = service.as_ref().map(|s| s.engine().provider());
            let file = mail_sync::attachment_file(&db, provider, &cache, id).await?;
            Ok(AttachmentFileInfo {
                path: file.path.to_string_lossy().into_owned(),
                filename: file.filename,
                mime_type: file.mime_type,
                content_id: file.content_id,
                downloaded: file.downloaded,
            })
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
    fn demo_attachments_open_from_the_cache() {
        let dir = std::env::temp_dir().join(format!("openagc-core-attachments-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            Arc::new(Noop),
        )
        .unwrap();
        block_on(core.clone().open_account("demo".into())).unwrap();
        block_on(core.debug_seed_demo_mailbox(200)).unwrap();
        let page = block_on(core.search_threads("has:attachment".into(), None, 5)).unwrap();
        let thread = page.rows.first().expect("the demo has attachments");
        let detail = block_on(core.get_thread(thread.id.clone())).unwrap().unwrap();
        let attachment = detail.messages.iter().flat_map(|m| m.attachments.clone()).next().unwrap();

        let first = block_on(core.attachment_file(attachment.id.clone())).unwrap();
        assert!(first.downloaded);
        assert!(first.path.starts_with(dir.to_str().unwrap()));
        assert_eq!(first.filename, attachment.filename);
        let bytes = std::fs::read(&first.path).unwrap();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        let again = block_on(core.attachment_file(attachment.id)).unwrap();
        assert!(!again.downloaded, "served from the cache");
        assert_eq!(again.path, first.path);

        let err = block_on(core.attachment_file("nope".into())).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidInput);
        let err = block_on(core.attachment_file("999999".into())).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::NotFound);
    }
}
