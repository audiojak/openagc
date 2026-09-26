//! Archive accounts (spec §7.8): mailboxes imported from mbox files, with
//! no server behind them. They are listed with the user's Gmail accounts,
//! but never sync, never hold a sign-in, and cannot send.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::registry::{AccountKind, IndexEntry, accounts_dir};
use crate::{Core, CoreError, CoreEvent, ErrorKind, runtime};

const META_FILE: &str = "account.json";

/// `accounts/<id>/account.json` for an archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ArchiveMeta {
    pub kind: String,
    pub name: String,
    pub source_path: String,
    #[serde(default)]
    pub imported_at: i64,
    #[serde(default)]
    pub messages: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub my_addresses: Vec<String>,
}

/// What a quick look at a mailbox file or folder found, for the import
/// sheet.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct MailboxScan {
    /// The mbox files that will be imported, in order.
    pub files: Vec<String>,
    pub total_bytes: u64,
    /// The address that appears most in the first messages: probably the
    /// owner's.
    pub suggested_address: Option<String>,
    /// A name for the account, from the file or folder name.
    pub suggested_name: String,
}

/// Import progress, as an event (spec §7.8).
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ImportStatus {
    pub bytes: u64,
    pub total_bytes: u64,
    pub imported: u64,
    pub duplicates: u64,
    pub unreadable: u64,
    pub done: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// Running imports, so they can be cancelled.
#[derive(Default)]
pub(crate) struct Imports {
    running: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
}

/// The mbox files for a path: the file itself, or a folder's `.mbox` files
/// (Takeout splits large exports), sorted by name.
fn mbox_files(path: &Path) -> Result<Vec<PathBuf>, CoreError> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(CoreError::new(ErrorKind::NotFound, "that file or folder does not exist"));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mbox")))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(CoreError::new(ErrorKind::InvalidInput, "no .mbox files in that folder"));
    }
    Ok(files)
}

pub(crate) fn read_meta(dir: &Path) -> Option<ArchiveMeta> {
    serde_json::from_slice(&std::fs::read(dir.join(META_FILE)).ok()?).ok()
}

fn write_meta(dir: &Path, meta: &ArchiveMeta) -> Result<(), CoreError> {
    std::fs::create_dir_all(dir).map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))?;
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?;
    std::fs::write(dir.join(META_FILE), bytes).map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))
}

/// What an archive cannot do, in one sentence for errors and the agent.
pub(crate) const CANNOT_SEND: &str =
    "This account is an imported mailbox: it cannot draft, reply, forward or send mail.";

/// Appended to the agent's system prompt in an archive account.
const ARCHIVE_PROMPT: &str = "\n\n## This account is an archive\n\nThe mailbox you are working in \
was imported from a file. It has no mail server: you can search, read, label and sort it, but \
drafting, replying, forwarding and sending are impossible here and those tools will refuse. When \
a task would need an email written, write the text in your answer instead.\n";

impl Core {
    /// Whether an account is an archive (reads its directory).
    pub(crate) fn is_archive(&self, account_id: &str) -> bool {
        read_meta(&accounts_dir(&self.data_path()).join(account_id)).is_some()
    }

    /// Refuse sending-side work when the account this work acts on is an
    /// archive (spec §7.8). The boundary lives here, not in the UI.
    pub(crate) fn refuse_if_archive(&self) -> Result<(), CoreError> {
        match self.effective_account_id() {
            Some(id) if self.is_archive(&id) => Err(CoreError::new(ErrorKind::InvalidInput, CANNOT_SEND)),
            _ => Ok(()),
        }
    }

    /// The agent system prompt for this account: the shipped one, plus the
    /// archive note in an archive (written next to the session).
    pub(crate) fn system_prompt_for_session(&self, shipped: &Path, session_dir: &Path) -> PathBuf {
        let archive = self.effective_account_id().is_some_and(|id| self.is_archive(&id));
        if !archive {
            return shipped.to_path_buf();
        }
        let mut text = std::fs::read_to_string(shipped).unwrap_or_default();
        text.push_str(ARCHIVE_PROMPT);
        let path = session_dir.join("system-prompt.md");
        match std::fs::write(&path, text) {
            Ok(()) => path,
            Err(_) => shipped.to_path_buf(),
        }
    }
}

#[uniffi::export]
impl Core {
    /// Look at a mailbox file or folder before importing it.
    pub async fn scan_mailbox(&self, path: String) -> Result<MailboxScan, CoreError> {
        runtime::run(async move {
            tokio::task::spawn_blocking(move || {
                let path = PathBuf::from(&path);
                let files = mbox_files(&path)?;
                let total_bytes = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
                let first =
                    std::fs::File::open(&files[0]).map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))?;
                let suggested_address =
                    mail_sync::import::most_frequent_address(BufReader::new(first), 500).map(|a| a.email);
                let stem = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let suggested_name = if path.is_dir() { stem(&path) } else { stem(&files[0]) };
                Ok(MailboxScan {
                    files: files.iter().map(|f| f.to_string_lossy().into_owned()).collect(),
                    total_bytes,
                    suggested_address,
                    suggested_name: if suggested_name.is_empty() { "Imported mail".into() } else { suggested_name },
                })
            })
            .await
            .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
        })
        .await
    }

    /// Import a mailbox file or folder into an archive account: a new one
    /// (`account_id` `None`), or an existing archive again (re-import:
    /// idempotent, it adds what is missing). Returns the account id at
    /// once; progress arrives as `ImportProgress` events tagged with it.
    pub async fn start_import(
        self: Arc<Self>,
        path: String,
        name: String,
        my_addresses: Vec<String>,
        account_id: Option<String>,
    ) -> Result<String, CoreError> {
        let files = {
            let path = PathBuf::from(&path);
            runtime::run(async move {
                tokio::task::spawn_blocking(move || mbox_files(&path))
                    .await
                    .map_err(|e| CoreError::new(ErrorKind::Internal, e.to_string()))?
            })
            .await?
        };
        let account_id = match account_id {
            Some(id) if self.is_archive(&id) => id,
            Some(_) => {
                return Err(CoreError::new(ErrorKind::InvalidInput, "only an archive account can be re-imported"));
            }
            None => crate::account::new_account_id()?,
        };
        let name = if name.trim().is_empty() { "Imported mail".to_owned() } else { name.trim().to_owned() };
        let dir = accounts_dir(&self.data_path()).join(&account_id);
        let mut meta = read_meta(&dir).unwrap_or(ArchiveMeta {
            kind: "archive".into(),
            name: name.clone(),
            source_path: path.clone(),
            imported_at: 0,
            messages: 0,
            bytes: 0,
            my_addresses: my_addresses.clone(),
        });
        meta.name = name.clone();
        meta.source_path = path;
        if !my_addresses.is_empty() {
            meta.my_addresses = my_addresses;
        }
        write_meta(&dir, &meta)?;
        let db = self.store_for(&account_id).await?;
        self.register_account(IndexEntry {
            id: account_id.clone(),
            kind: AccountKind::Archive,
            email: name,
            display_name: None,
            avatar_file: None,
            added_at: mail_sync::now_millis(),
            imap: None,
        })
        .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.imports.running.lock().unwrap_or_else(|e| e.into_inner()).insert(account_id.clone(), cancel.clone());
        let events = self.events.for_account(Some(account_id.clone()));
        let core = self.clone();
        let id = account_id.clone();
        runtime::runtime().spawn(async move {
            let total_bytes: u64 = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
            let options = mail_sync::import::ImportOptions { my_addresses: meta.my_addresses.clone() };
            let progress_events = events.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<mail_sync::import::ImportStats, String> {
                let mut total = mail_sync::import::ImportStats::default();
                let mut done_bytes = 0u64;
                for file in &files {
                    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                    let reader =
                        BufReader::with_capacity(1 << 20, std::fs::File::open(file).map_err(|e| e.to_string())?);
                    let before = done_bytes;
                    let before_imported = total.imported;
                    let stats = mail_sync::import::import_mbox(&db, reader, size, &options, &cancel, |p| {
                        progress_events.emit(CoreEvent::ImportProgress {
                            status: ImportStatus {
                                bytes: before + p.bytes,
                                total_bytes,
                                imported: before_imported + p.imported,
                                duplicates: 0,
                                unreadable: 0,
                                done: false,
                                cancelled: false,
                                error: None,
                            },
                        });
                    })
                    .map_err(|e| e.to_string())?;
                    done_bytes += size;
                    total.imported += stats.imported;
                    total.duplicates += stats.duplicates;
                    total.unreadable += stats.unreadable;
                    if stats.cancelled {
                        total.cancelled = true;
                        break;
                    }
                }
                Ok(total)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            core.imports.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            let status = match result {
                Ok(stats) => {
                    meta.imported_at = mail_sync::now_millis();
                    meta.messages = stats.imported;
                    meta.bytes = total_bytes;
                    let _ = write_meta(&dir, &meta);
                    ImportStatus {
                        bytes: total_bytes,
                        total_bytes,
                        imported: stats.imported,
                        duplicates: stats.duplicates,
                        unreadable: stats.unreadable,
                        done: true,
                        cancelled: stats.cancelled,
                        error: None,
                    }
                }
                Err(message) => {
                    tracing::warn!(error = %message, "mailbox import failed");
                    ImportStatus {
                        bytes: 0,
                        total_bytes,
                        imported: 0,
                        duplicates: 0,
                        unreadable: 0,
                        done: true,
                        cancelled: false,
                        error: Some(message),
                    }
                }
            };
            events
                .emit(CoreEvent::ThreadsChanged { mailbox_id: "INBOX".into(), hint: crate::ChangeHint::invalidate() });
            events.emit(CoreEvent::ImportProgress { status });
        });
        Ok(account_id)
    }

    /// Import an archive again from where it came from (Settings'
    /// Re-import…). Adds anything missing; changes nothing already there.
    pub async fn reimport_archive(self: Arc<Self>, account_id: String) -> Result<String, CoreError> {
        let meta = read_meta(&accounts_dir(&self.data_path()).join(&account_id))
            .ok_or_else(|| CoreError::new(ErrorKind::InvalidInput, "not an archive account"))?;
        self.start_import(meta.source_path, meta.name, vec![], Some(account_id)).await
    }

    /// Stop an import at the next batch; what was imported stays.
    pub fn cancel_import(&self, account_id: String) {
        if let Some(flag) = self.imports.running.lock().unwrap_or_else(|e| e.into_inner()).get(&account_id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Whether an account is an imported mailbox that cannot send.
    pub fn account_is_archive(&self, account_id: String) -> bool {
        self.is_archive(&account_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::executor::block_on;
    use mail_mime::mbox::fixture::{FixtureMessage, build};

    use super::*;
    use crate::{CoreConfig, EventListener};

    #[derive(Default)]
    struct Imports(Mutex<Vec<(Option<String>, ImportStatus)>>);
    impl EventListener for Imports {
        fn on_event(&self, account: Option<String>, event: CoreEvent) {
            if let CoreEvent::ImportProgress { status } = event {
                self.0.lock().unwrap().push((account, status));
            }
        }
    }

    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wait_done(events: &Imports, account: &str, count: usize) -> ImportStatus {
        for _ in 0..400 {
            let done: Vec<ImportStatus> = events
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|(a, s)| a.as_deref() == Some(account) && s.done)
                .map(|(_, s)| s.clone())
                .collect();
            if done.len() >= count {
                return done[count - 1].clone();
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("import did not finish");
    }

    #[test]
    fn a_folder_of_mbox_files_becomes_a_listed_archive_account() {
        let t = Temp(std::env::temp_dir().join(format!("openagc-archive-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&t.0);
        let export = t.0.join("Takeout Mail");
        std::fs::create_dir_all(&export).unwrap();
        let mut first: Vec<FixtureMessage> = (1..=3).map(FixtureMessage::simple).collect();
        first[0].from = "Owner <owner@example.com>".into();
        for m in first.iter_mut().skip(1) {
            m.to = "owner@example.com".into();
        }
        std::fs::write(export.join("a.mbox"), build(&first)).unwrap();
        std::fs::write(export.join("b.mbox"), build(&[FixtureMessage::simple(4)])).unwrap();
        std::fs::write(export.join("notes.txt"), b"not mail").unwrap();

        let events = Arc::new(Imports::default());
        let core = Core::new(
            CoreConfig { data_dir: t.0.join("data").to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            events.clone(),
        )
        .unwrap();
        let scan = block_on(core.scan_mailbox(export.to_string_lossy().into_owned())).unwrap();
        assert_eq!(scan.files.len(), 2, "only .mbox files, sorted");
        assert!(scan.files[0].ends_with("a.mbox"));
        assert_eq!(scan.suggested_name, "Takeout Mail");
        assert_eq!(scan.suggested_address.as_deref(), Some("owner@example.com"));

        let id = block_on(core.clone().start_import(
            export.to_string_lossy().into_owned(),
            "Old mail".into(),
            vec!["owner@example.com".into()],
            None,
        ))
        .unwrap();
        let done = wait_done(&events, &id, 1);
        assert_eq!((done.imported, done.error.clone(), done.cancelled), (4, None, false));
        assert!(core.account_is_archive(id.clone()));

        let accounts = block_on(core.list_accounts()).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].kind, AccountKind::Archive);
        assert_eq!(accounts[0].email, "Old mail");
        let sent =
            block_on(crate::registry::scoped(Some(id.clone()), core.list_threads("SENT".into(), None, 10))).unwrap();
        assert_eq!(sent.rows.len(), 1, "the owner's mail is Sent");

        // Archives never sync: nothing to start, nothing needing a sign-in.
        assert!(block_on(core.clone().start_all_sync()).unwrap().is_empty());
        assert!(!core.is_syncing(&id));

        // Re-importing adds nothing new and keeps the account.
        let again = block_on(core.clone().start_import(
            export.to_string_lossy().into_owned(),
            "Old mail".into(),
            vec![],
            Some(id.clone()),
        ))
        .unwrap();
        assert_eq!(again, id);
        assert_eq!(wait_done(&events, &id, 2).imported, 4);
        let meta = read_meta(&accounts_dir(&t.0.join("data")).join(&id)).unwrap();
        assert_eq!(meta.my_addresses, vec!["owner@example.com".to_owned()], "kept when not given again");

        // A Gmail account cannot be "re-imported".
        assert!(
            block_on(core.clone().start_import(
                export.to_string_lossy().into_owned(),
                "x".into(),
                vec![],
                Some("gmailid".into())
            ))
            .is_err()
        );

        block_on(core.remove_account(id.clone())).unwrap();
        assert!(block_on(core.list_accounts()).unwrap().is_empty());
        assert!(!accounts_dir(&t.0.join("data")).join(&id).exists());
    }

    #[test]
    fn an_archive_reads_and_sorts_but_never_drafts_or_sends() {
        use agent_mcp::Outcome;
        use permissions::{Scope, Tool};
        use serde_json::json;

        let t = Temp(std::env::temp_dir().join(format!("openagc-archive-nosend-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&t.0);
        std::fs::create_dir_all(&t.0).unwrap();
        let file = t.0.join("old.mbox");
        std::fs::write(&file, build(&(1..=5).map(FixtureMessage::simple).collect::<Vec<_>>())).unwrap();
        let events = Arc::new(Imports::default());
        let core = Core::new(
            CoreConfig { data_dir: t.0.join("data").to_string_lossy().into_owned(), log_dir: None },
            Arc::new(crate::secrets::MemorySecrets::default()),
            events.clone(),
        )
        .unwrap();
        let id = block_on(core.clone().start_import(file.to_string_lossy().into_owned(), "Old".into(), vec![], None))
            .unwrap();
        wait_done(&events, &id, 1);
        block_on(core.clone().set_current_account(id.clone())).unwrap();

        // The core refuses every sending-side call.
        let refused = |r: Result<(), CoreError>| {
            let CoreError::Failed { message, .. } = r.unwrap_err();
            assert_eq!(message, CANNOT_SEND);
        };
        let thread = block_on(core.list_threads("INBOX".into(), None, 1)).unwrap().rows.remove(0);
        let detail = block_on(core.get_thread(thread.id.clone())).unwrap().unwrap();
        let message_id = detail.messages[0].id.clone();
        refused(block_on(core.reply_draft(message_id.clone(), false)).map(|_| ()));
        refused(block_on(core.forward_draft(message_id)).map(|_| ()));
        refused(block_on(core.send_draft(1)));

        // Agents: reading and local sorting work; drafting is refused with
        // a final, structured answer.
        core.agents.register("s", Scope::Mailbox, None);
        let call = |tool: Tool, args: serde_json::Value| {
            crate::runtime::runtime().block_on(crate::agents::tools::call(&core, "s", tool, args))
        };
        assert!(matches!(call(Tool::Search, json!({ "query": "", "limit": 5 })), Outcome::Ok { .. }));
        assert!(
            matches!(call(Tool::CreateLabel, json!({ "name": "Sorted/Later" })), Outcome::Ok { .. }),
            "local labels are fine"
        );
        match call(Tool::CreateDraft, json!({ "to": ["a@example.com"], "subject": "x", "body_markdown": "x" })) {
            Outcome::Error { code, message } => {
                assert_eq!(code, "cannot_send");
                assert_eq!(message, CANNOT_SEND);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // No cloud routine can reach it, and the agent is told what it is.
        let routine = block_on(core.create_routine_from_template("claude_cloud".into())).unwrap();
        assert!(block_on(core.publish_routine_to_cloud(routine.id)).is_err());
        let dir = t.0.join("session");
        std::fs::create_dir_all(&dir).unwrap();
        let shipped = t.0.join("prompt.md");
        std::fs::write(&shipped, "Base prompt.").unwrap();
        let prompt = std::fs::read_to_string(core.system_prompt_for_session(&shipped, &dir)).unwrap();
        assert!(prompt.starts_with("Base prompt.") && prompt.contains("This account is an archive"));

        // A Gmail account gets the shipped prompt untouched.
        block_on(core.clone().set_current_account("gmail".into())).unwrap();
        assert_eq!(core.system_prompt_for_session(&shipped, &dir), shipped);
        assert!(core.refuse_if_archive().is_ok());
    }
}
